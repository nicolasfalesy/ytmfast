//! The engine: one task that owns what is playing, takes commands, and reports state.
//!
//! Commands come in on an mpsc channel; state, position and errors go out on a broadcast
//! channel, so the socket and MPRIS can each listen. The engine never waits on the network
//! or on the audio thread: a play's link is resolved in its own task, and the audio thread is
//! driven through `AudioPlayer`'s command channel. (The audio thread can be blocked for a
//! while opening a track whose first bytes haven't arrived; nothing here waits for it.)
//!
//! The engine also drives the queue (`crate::queue`): a play fills it from YouTube Music's
//! `next` (a `QueueSource`), a song's end plays the next item, an unplayable song is skipped,
//! the next song's link is prefetched halfway through the current one, and radio songs are
//! fetched when the queue is about to run out.
//!
//! Gapless: 10 s before the current song's end (or at once, for a song with less left), the
//! next item's link is resolved (already cached by the prefetch), its download started and
//! handed to the audio thread (`AudioPlayer::preload`). The audio thread plays it right after
//! the current one and says `Advanced` when its first frame is heard; the engine then makes
//! it current without a load. Any queue change that changes the next item drops the preload
//! (and preloads the new next one when it is time).
//!
//! Four counters keep late news out of the state:
//! - every play gets a generation number, and a resolve that comes back for an older one is
//!   dropped (its task is aborted too, but an answer already in the channel isn't);
//! - every new queue gets a queue generation, and a queue page that comes back for an older
//!   queue is dropped the same way;
//! - every load handed to the audio thread is counted, and so is every `AudioEvent::Loading`
//!   it sends back; until they match, its events are about an earlier track and are dropped;
//! - every preload's resolve gets a preload generation, and every preload handed to the audio
//!   thread its id: an `Advanced` for any other id is about a preload the engine dropped.
//!
//! The state follows the commands (pause is paused at once), except that a song only turns
//! `Playing` when the audio thread says it started: until then it is `Buffering`.
//!
//! Play reports (`crate::report`): every play of a song that is heard (its `Started`, or its
//! `Advanced` for a gapless handover) gets its own report, told about the position ticks,
//! pauses, resumes and seeks, and ended wherever the song stops being the one playing.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::{Instant, Interval, MissedTickBehavior};

use async_trait::async_trait;

use crate::audio::decode::loudness_gain;
use crate::audio::fetch::{Relink, TrackBuffer};
use crate::audio::player::{AudioEvent, AudioPlayer};
use crate::error::Error;
use crate::innertube::{Innertube, NextPage, NextRequest, SongItem};
use crate::queue::{AddAt, Previous, Queue, QueueItem, Repeat};
use crate::report::{self, PlayReport, Reporter};
use crate::state::{self, Saved, Writer};
use crate::streams::{Resolver, Stream, TrackMeta};
use url::Url;

/// Where the queue's songs come from: YouTube Music's `next` (`Innertube::next`) in
/// production; a trait so the engine's tests can answer with their own pages. Used as
/// `Arc<dyn QueueSource>`, hence async-trait (ruling R1).
#[async_trait]
pub trait QueueSource: Send + Sync {
    async fn next(&self, req: NextRequest) -> Result<NextPage, Error>;
}

#[async_trait]
impl QueueSource for Innertube {
    async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
        // The inherent method, not this one.
        Innertube::next(self, req).await
    }
}

/// What the socket and MPRIS ask of the engine.
#[derive(Debug)]
pub enum EngineCmd {
    /// A new queue, or the one there is.
    ///
    /// - `playlist_id` (an album or a playlist): its songs become the queue, starting at
    ///   `video_id` if given (it plays at once, before the list arrives), else at `index`.
    /// - `video_id` alone: that song plays at once, and its radio fills the queue behind it.
    /// - Neither: resume what is loaded, or play the current song again once it has ended
    ///   (from where it stopped, after a mid-song error); songs queued with none current yet
    ///   play from the first; with an empty queue, Liked songs.
    ///
    /// A list with no `video_id` or `index` starts at a random song while shuffle is on.
    ///
    /// `start_seconds` is where the first song starts.
    Play {
        video_id: Option<String>,
        playlist_id: Option<String>,
        index: Option<usize>,
        start_seconds: f64,
    },
    Pause,
    Toggle,
    /// A seek at or past the song's end acts like `Next`.
    Seek(f64),
    /// 0.0 to 1.0 (clamped).
    Volume(f32),
    Status(oneshot::Sender<Status>),
    Next,
    /// Restarts the song when more than 3 s in, else plays the item before.
    Previous,
    QueueGet(oneshot::Sender<QueueView>),
    /// `added` answers false, with nothing added, when the songs would take the queue past
    /// `queue::MAX_ITEMS` (the socket refuses the request).
    QueueAdd {
        songs: Vec<SongItem>,
        at: AddAt,
        added: oneshot::Sender<bool>,
    },
    /// By queue id.
    QueueRemove(u64),
    /// Plays the item with this queue id.
    QueueJump(u64),
    /// Moves the item with this queue id to `index` in the play order.
    QueueMove {
        id: u64,
        index: usize,
    },
    Shuffle(bool),
    Repeat(Repeat),
    Quit,
}

/// The queue as the widgets see it: the `queue` event's fields, and `QueueGet`'s reply.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueView {
    /// In play order (the shuffled order while shuffle is on). Shared, not copied: the
    /// broadcast channel clones every event once per listener, and a radio queue runs to
    /// hundreds of items.
    pub items: Arc<[QueueItem]>,
    pub current_id: Option<u64>,
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// What the engine reports.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    State(Status),
    /// Once a second while playing, and right after every seek. `seeked` is true only for a
    /// seek's own event (MPRIS sends `Seeked` for those, and never for ticks).
    Position {
        seconds: f64,
        seeked: bool,
    },
    /// The queue, on every change: songs added, removed or moved, a new current item, shuffle
    /// or repeat.
    Queue {
        items: Arc<[QueueItem]>,
        current_id: Option<u64>,
        shuffle: bool,
        repeat: Repeat,
    },
    /// `code` is `Error::code()`; `message` its `Display`, which never holds a link (R6).
    Error {
        code: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    Playing,
    Paused,
    Buffering,
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: PlayState,
    /// The current song, or the last one once it has ended (so the bar can still show it).
    pub video_id: Option<String>,
    /// From the queue item when it has details (at once); else known once the song's link
    /// is resolved.
    pub meta: Option<TrackMeta>,
    /// The current song's album, from its queue item.
    pub album: Option<String>,
    /// The current song's queue id.
    pub queue_id: Option<u64>,
    pub position: f64,
    /// 0.0 to 1.0 (the socket turns it into a percent).
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// Starts a track's download. `TrackBuffer::start` in production; tests swap in one that
/// allows their local server (ruling R7).
type Starter = Box<dyn Fn(Stream, Relink) -> TrackBuffer + Send + Sync>;

/// How many events a slow listener may fall behind before it misses some.
const EVENTS_CAPACITY: usize = 64;

/// How many commands may wait for the engine.
const COMMANDS_CAPACITY: usize = 32;

const TICK: Duration = Duration::from_secs(1);

/// The playlist a play with nothing at all starts: the user's Liked songs.
const LIKED_SONGS: &str = "LM";

/// A song's radio is the playlist `RDAMVM` + its id, asked for together with the id.
const RADIO_PREFIX: &str = "RDAMVM";

/// The next song is preloaded this long before the current one ends (Global Constraints).
const PRELOAD_LEAD_SECS: f64 = 10.0;

/// While playing, the state is saved every this many position ticks (30 s; Global
/// Constraints), so a crash or a power loss loses at most that much of the song.
const SAVE_EVERY_TICKS: u32 = 30;

/// How long a quit waits for its last save: long enough for any working disk, short enough
/// that a hung one can't hold up `systemctl stop` (whose own limit is far longer).
const LAST_SAVE_WAIT: Duration = Duration::from_secs(2);

/// A finished resolve, tagged with the play it was for.
struct Resolved {
    generation: u64,
    result: Result<Stream, Error>,
}

/// A finished preload resolve, tagged with the preload it was for.
struct Preresolved {
    generation: u64,
    result: Result<Stream, Error>,
}

/// A song's download and how to decode it: all the audio thread needs to play it.
#[derive(Clone)]
struct Source {
    buffer: TrackBuffer,
    video_id: String,
    mime: String,
    gain: f32,
    length_hint: Option<f64>,
    /// The details from its link.
    meta: TrackMeta,
}

/// The next item, made ready before the current one ends.
struct Preload {
    /// The queue item it is for: it is dropped once that is no longer the next.
    queue_id: u64,
    state: PreloadState,
}

enum PreloadState {
    Resolving {
        generation: u64,
        task: AbortHandle,
    },
    /// Handed to the audio thread under this id (`AudioEvent::Advanced`).
    Ready {
        ticket: u64,
        source: Source,
    },
    /// Its link failed: not asked again for this item, whose own turn reports it.
    Failed,
}

impl Preload {
    #[cfg(test)]
    fn resolving(&self) -> bool {
        matches!(self.state, PreloadState::Resolving { .. })
    }

    #[cfg(test)]
    fn ticket(&self) -> Option<u64> {
        match self.state {
            PreloadState::Ready { ticket, .. } => Some(ticket),
            _ => None,
        }
    }
}

/// A finished queue request, tagged with the queue it was for (`Engine::queue_generation`).
struct Paged {
    queue_generation: u64,
    /// More radio songs for the end of the queue, rather than a play's whole queue.
    refill: bool,
    result: Result<NextPage, Error>,
}

/// A play's queue that is still being fetched.
struct PendingLoad {
    /// The song the play named. It plays at once, alone in the queue until the list arrives.
    seed: Option<String>,
    /// Where to start when there is no seed (or the seed is found at that index).
    index: Option<usize>,
    /// Where the first song starts when there is no seed.
    start: f64,
    /// The play named a playlist, so failing to fetch it is news for the user. A lone song's
    /// radio is a background extra: it failing just leaves the song alone in the queue.
    report_errors: bool,
    /// Songs the user added while the list was on its way, by queue id, and where. The list
    /// replaces the queue when it lands; these are put back (`Engine::keep_added`), so they
    /// aren't lost and the one playing keeps its queue id.
    added: HashMap<u64, AddAt>,
}

/// The songs added while a play's list was on its way that are still in the queue, in play
/// order, sorted by where they go once it lands.
#[derive(Default)]
struct Kept {
    /// Up to and including the current item, when that is one of them: heard, or playing.
    played: Vec<QueueItem>,
    /// Still to come, added with `next` (right after the current song).
    next: Vec<QueueItem>,
    /// Still to come, added with `end`.
    end: Vec<QueueItem>,
}

pub struct Engine {
    resolver: Arc<dyn Resolver>,
    source: Arc<dyn QueueSource>,
    queue: Queue,
    player: AudioPlayer,
    start_buffer: Starter,
    commands: mpsc::Receiver<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    resolved_tx: mpsc::UnboundedSender<Resolved>,
    resolved_rx: mpsc::UnboundedReceiver<Resolved>,
    pages_tx: mpsc::UnboundedSender<Paged>,
    pages_rx: mpsc::UnboundedReceiver<Paged>,
    status: Status,
    /// Bumped by every play that starts a song, and by every stop.
    generation: u64,
    /// Bumped by every play that makes a new queue: a queue page that comes back for an older
    /// one is dropped (the same rule as `generation`, for queues).
    queue_generation: u64,
    /// The running resolve, aborted when a newer play replaces it.
    resolving: Option<AbortHandle>,
    /// The play's queue request, and what it is for.
    loading: Option<AbortHandle>,
    pending: Option<PendingLoad>,
    /// The radio request for the end of the queue. One at a time.
    refilling: Option<AbortHandle>,
    /// The queue's next page, when it came from a radio (or another endless list).
    continuation: Option<String>,
    /// YouTube has no more songs for this queue: no more radio requests until a new queue.
    exhausted: bool,
    /// The queue ran out while more songs were on the way: the next one plays when they come.
    waiting: bool,
    /// Stopped because the queue ran out (not by an error).
    at_end: bool,
    /// Songs that failed in a row; a whole queue's worth stops the skipping.
    skip_streak: usize,
    /// How long that queue was when the run of failures began (once its list was all there):
    /// the pass the run stops after. Fixed then, because radio refills grow the queue while
    /// it skips, and an endless radio of unplayable songs would never reach its length.
    skip_cap: Option<usize>,
    /// Where the current song stopped after a failure: a play goes on from there.
    resume_from: Option<f64>,
    /// The current song's details from its link (fills gaps in the queue item's).
    resolved_meta: Option<TrackMeta>,
    /// The latest link prefetch, by queue id.
    prefetch: Option<(u64, AbortHandle)>,
    /// The current song's download: repeat one (or the same song twice in a row) preloads a
    /// second reader over its bytes instead of downloading it again.
    current: Option<Source>,
    preload: Option<Preload>,
    /// Bumped by every preload resolve: an answer for an older one is dropped.
    preload_generation: u64,
    preloads_tx: mpsc::UnboundedSender<Preresolved>,
    preloads_rx: mpsc::UnboundedReceiver<Preresolved>,
    /// Where the current song starts: the play's start, moved by a seek while resolving.
    start_seconds: f64,
    /// The current song was handed to the audio thread and has not ended or failed.
    loaded: bool,
    /// The audio thread has said `Started` for the current song.
    started: bool,
    /// The current play already started its song again after an output restart. Once per
    /// play: an output that keeps dying under the song must not loop for ever.
    replayed: bool,
    /// Loads handed to the audio thread, and `Loading` events back from it.
    loads_sent: u64,
    loads_seen: u64,
    /// The position clock: only exists while playing, so a paused or stopped engine has no
    /// timer waking it.
    ticker: Option<Interval>,
    /// Saves the state (`crate::state`); `None` when nothing is saved (most tests).
    writer: Option<Writer>,
    /// Something worth saving changed (a song, the queue, a pause, a seek, the volume). Saved
    /// once the command or event at hand is handled, so a burst of changes is one snapshot.
    dirty: bool,
    /// Position ticks since the last save; `SAVE_EVERY_TICKS` of them save again. The ticker
    /// only runs while playing, so this 30 s timer only exists then too.
    ticks_since_save: u32,
    /// The current song comes from a saved state and was never loaded: a play loads it at
    /// its saved second (`status.position`).
    restored: bool,
    /// A seek's target, for the save it causes: the audio thread takes the seek a moment
    /// later, so until then its position is still the old one (a paused song would be saved
    /// at its old second, and stay so until the next save).
    seeked_to: Option<f64>,
    /// The playlist the queue came from (saved with the queue).
    source_playlist: Option<String>,
    /// Starts play reports; `None` when nothing is reported (most tests).
    reporter: Option<Reporter>,
    /// The report of the song playing now: from when it was first heard until it stops.
    report: Option<PlayReport>,
}

impl Engine {
    pub fn new(
        resolver: Arc<dyn Resolver>,
        source: Arc<dyn QueueSource>,
        player: AudioPlayer,
    ) -> (
        Engine,
        mpsc::Sender<EngineCmd>,
        broadcast::Sender<EngineEvent>,
    ) {
        Self::with_starter(resolver, source, player, Box::new(TrackBuffer::start))
    }

    fn with_starter(
        resolver: Arc<dyn Resolver>,
        source: Arc<dyn QueueSource>,
        player: AudioPlayer,
        start_buffer: Starter,
    ) -> (
        Engine,
        mpsc::Sender<EngineCmd>,
        broadcast::Sender<EngineEvent>,
    ) {
        let (cmd_tx, commands) = mpsc::channel(COMMANDS_CAPACITY);
        let (events, _) = broadcast::channel(EVENTS_CAPACITY);
        let (resolved_tx, resolved_rx) = mpsc::unbounded_channel();
        let (pages_tx, pages_rx) = mpsc::unbounded_channel();
        let (preloads_tx, preloads_rx) = mpsc::unbounded_channel();
        let engine = Engine {
            resolver,
            source,
            queue: Queue::new(),
            player,
            start_buffer,
            commands,
            events: events.clone(),
            resolved_tx,
            resolved_rx,
            pages_tx,
            pages_rx,
            status: Status {
                state: PlayState::Stopped,
                video_id: None,
                meta: None,
                album: None,
                queue_id: None,
                position: 0.0,
                volume: 1.0,
                shuffle: false,
                repeat: Repeat::Off,
            },
            generation: 0,
            queue_generation: 0,
            resolving: None,
            loading: None,
            pending: None,
            refilling: None,
            continuation: None,
            exhausted: false,
            waiting: false,
            at_end: false,
            skip_streak: 0,
            skip_cap: None,
            resume_from: None,
            resolved_meta: None,
            prefetch: None,
            current: None,
            preload: None,
            preload_generation: 0,
            preloads_tx,
            preloads_rx,
            start_seconds: 0.0,
            loaded: false,
            started: false,
            replayed: false,
            loads_sent: 0,
            loads_seen: 0,
            ticker: None,
            writer: None,
            dirty: false,
            ticks_since_save: 0,
            restored: false,
            seeked_to: None,
            source_playlist: None,
            reporter: None,
            report: None,
        };
        (engine, cmd_tx, events)
    }

    /// Takes up a saved state (before `run`): the queue, the current song paused at its
    /// second, volume, shuffle, repeat and the queue's source. Nothing is fetched or
    /// downloaded until a play.
    pub fn restore(&mut self, saved: Saved) {
        let current = (!saved.queue.is_empty()).then_some(saved.current_index);
        // Shuffle on with no order saved: `Queue::restore` falls back to the play order.
        let original = saved
            .shuffle
            .then(|| saved.original_order.unwrap_or_default());
        self.queue = Queue::restore(saved.queue, current, original, saved.repeat);
        self.source_playlist = saved.source_playlist;
        self.continuation = saved.continuation;
        self.exhausted = saved.exhausted;
        let volume = if saved.volume.is_finite() {
            saved.volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.player.set_volume(volume);
        self.status.volume = volume;
        self.status.shuffle = self.queue.shuffle();
        self.status.repeat = self.queue.repeat();
        let Some(item) = self.queue.current().cloned() else {
            return;
        };
        let at = if saved.position.is_finite() {
            saved.position.max(0.0)
        } else {
            0.0
        };
        self.status.video_id = Some(item.song.video_id.clone());
        self.status.queue_id = Some(item.id);
        self.status.album = item.song.album.clone();
        self.status.meta = song_meta(&item.song, None);
        self.status.position = at;
        self.start_seconds = at;
        // Paused, never Playing: a restart (an update, an idle quit) must not start music by
        // itself. Nothing is loaded until a play (`resume`).
        self.status.state = PlayState::Paused;
        self.restored = true;
    }

    /// Saves the state through `writer` from now on, and once more on the way out.
    pub fn save_with(&mut self, writer: Writer) {
        self.writer = Some(writer);
    }

    /// Reports every song heard from now on to the account's history.
    pub fn report_with(&mut self, reporter: Reporter) {
        self.reporter = Some(reporter);
    }

    /// Downloads tracks with links on `base`'s origin (a local http test server) allowed as
    /// well as the allowlist. For tests only, like `TrackBuffer::start_with_test_base`: it is
    /// the one way past the https allowlist for an engine's downloads (ruling R7).
    pub fn download_from_test_base(&mut self, base: Url) {
        // No timeouts, as in the engine's own tests: a paused test clock would fire them.
        let client = reqwest::Client::new();
        self.start_buffer = Box::new(move |stream, relink| {
            TrackBuffer::start_with_test_base(stream, relink, base.clone(), client.clone())
        });
    }

    /// Runs until `Quit`, or until every command sender is gone. Stops the audio thread on
    /// the way out.
    pub async fn run(mut self) {
        let (mut audio, forwarder) = forward_audio_events(&self.player);
        loop {
            tokio::select! {
                cmd = self.commands.recv() => match cmd {
                    None | Some(EngineCmd::Quit) => break,
                    Some(cmd) => self.handle(cmd),
                },
                Some(r) = self.resolved_rx.recv() => self.on_resolved(r),
                Some(p) = self.pages_rx.recv() => self.on_page(p),
                Some(p) = self.preloads_rx.recv() => self.on_preloaded(p),
                Some(e) = audio.recv() => self.on_audio(e),
                () = next_tick(&mut self.ticker) => self.on_tick(),
            }
            if self.dirty {
                self.write_state();
            }
        }
        // Every way out (the socket's quit, idle, a signal) ends here: one last save, while
        // the player still knows the position. Bounded, so a hung disk can't hold up a stop.
        // The song playing now gets its last report (best-effort: the runtime may stop
        // before it is sent).
        let at = if self.loaded && self.started {
            self.played_to()
        } else {
            self.snapshot().position
        };
        self.end_report(at);
        if let Some(writer) = self.writer.take() {
            let last = self.saved();
            if !writer.finish(last, LAST_SAVE_WAIT).await {
                eprintln!("ytmfast: the play state was not saved in time; quitting anyway");
            }
        }
        let prefetch = self.prefetch.take().map(|(_, task)| task);
        self.drop_preload();
        for task in [
            self.resolving.take(),
            self.loading.take(),
            self.refilling.take(),
            prefetch,
        ]
        .into_iter()
        .flatten()
        {
            task.abort();
        }
        // Dropping the player cancels its reader and joins the audio thread; the forwarder
        // ends when that thread's event sender goes with it. Both are joins, so off this
        // runtime's threads: the audio thread may be finishing a write into the sink.
        let player = self.player;
        let _ = tokio::task::spawn_blocking(move || {
            drop(player);
            if let Some(f) = forwarder {
                let _ = f.join();
            }
        })
        .await;
    }

    fn handle(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::Play {
                video_id,
                playlist_id,
                index,
                start_seconds,
            } => self.play(video_id, playlist_id, index, start_seconds),
            EngineCmd::Pause => self.pause(),
            EngineCmd::Toggle => match self.status.state {
                PlayState::Playing | PlayState::Buffering => self.pause(),
                PlayState::Paused => self.resume(),
                PlayState::Stopped => self.play(None, None, None, 0.0),
            },
            EngineCmd::Seek(seconds) => self.seek(seconds),
            EngineCmd::Volume(v) => self.volume(v),
            EngineCmd::Status(reply) => {
                let _ = reply.send(self.snapshot());
            }
            EngineCmd::Next => self.advance(false, false),
            EngineCmd::Previous => self.previous(),
            EngineCmd::QueueGet(reply) => {
                let _ = reply.send(self.queue_view());
            }
            EngineCmd::QueueAdd { songs, at, added } => {
                let ok = self.queue_add(songs, at);
                let _ = added.send(ok);
            }
            EngineCmd::QueueRemove(id) => self.queue_remove(id),
            EngineCmd::QueueJump(id) => self.queue_jump(id),
            EngineCmd::QueueMove { id, index } => {
                if self.queue.move_to(id, index) {
                    self.emit_queue();
                } else {
                    self.not_in_queue();
                }
            }
            EngineCmd::Shuffle(on) => {
                self.queue.set_shuffle(on);
                self.emit_queue();
                self.emit_state();
            }
            EngineCmd::Repeat(repeat) => {
                self.queue.set_repeat(repeat);
                self.emit_queue();
                self.emit_state();
                // Repeat off can leave the queue short of songs.
                self.maybe_refill();
            }
            // Handled by `run`.
            EngineCmd::Quit => {}
        }
        // Whatever changed the next item (a skip, a jump, a removal, a move, shuffle, repeat,
        // a new play) changed what follows the current song.
        self.check_preload();
    }

    fn play(
        &mut self,
        video_id: Option<String>,
        playlist_id: Option<String>,
        index: Option<usize>,
        start_seconds: f64,
    ) {
        let start = if start_seconds.is_finite() {
            start_seconds.max(0.0)
        } else {
            0.0
        };
        let request = match (&video_id, &playlist_id) {
            (None, None) => return self.play_current(start),
            // A song's radio is asked for with its seed song.
            (Some(v), Some(p)) if p.starts_with(RADIO_PREFIX) => NextRequest {
                video_id: Some(v.clone()),
                playlist_id: Some(p.clone()),
                ..NextRequest::default()
            },
            // An album or playlist by its id alone: with a video id too, YouTube answers
            // with just that song (tests/fixtures/NEXT_FIXTURES.md).
            (_, Some(p)) => NextRequest {
                playlist_id: Some(p.clone()),
                ..NextRequest::default()
            },
            // A lone song: its radio fills the queue behind it, so "radio when the queue
            // runs out" holds from the first song.
            (Some(v), None) => NextRequest {
                video_id: Some(v.clone()),
                playlist_id: Some(format!("{RADIO_PREFIX}{v}")),
                ..NextRequest::default()
            },
        };
        self.new_queue();
        self.source_playlist = request.playlist_id.clone();
        match &video_id {
            Some(id) => {
                // It plays at once, without waiting for the list.
                self.queue.replace(vec![bare_song(id)], 0);
                self.emit_queue();
                self.start_current(start);
            }
            None => {
                self.queue.replace(Vec::new(), 0);
                self.emit_queue();
                self.halt();
                self.show_nothing();
                self.status.position = start;
                self.status.state = PlayState::Buffering;
                self.emit_state();
            }
        }
        self.pending = Some(PendingLoad {
            seed: video_id,
            index,
            start,
            report_errors: playlist_id.is_some(),
            added: HashMap::new(),
        });
        self.loading = Some(self.request(request, false));
    }

    /// A play without an id or a playlist.
    fn play_current(&mut self, start: f64) {
        match self.status.state {
            PlayState::Paused => {
                if start > 0.0 {
                    self.seek(start);
                }
                self.resume();
            }
            PlayState::Playing | PlayState::Buffering => {
                if start > 0.0 {
                    self.seek(start);
                }
            }
            PlayState::Stopped => {
                // The queue ran out and songs were added since, or songs were added to a queue
                // with nothing current yet: go on with them. Liked songs (below) only when the
                // queue is truly empty, so a play never throws away songs the user queued.
                if (self.at_end || self.queue.current().is_none())
                    && self.queue.peek_next(false).is_some()
                {
                    return self.advance(false, false);
                }
                if self.queue.current().is_some() {
                    let from = if start > 0.0 {
                        start
                    } else {
                        self.resume_from.unwrap_or(0.0)
                    };
                    self.start_current(from);
                } else {
                    self.play(None, Some(LIKED_SONGS.into()), None, start);
                }
            }
        }
    }

    /// Forgets everything about the old queue's fetching: its answers are dropped when they
    /// come.
    fn new_queue(&mut self) {
        self.queue_generation += 1;
        self.drop_preload();
        let prefetch = self.prefetch.take().map(|(_, task)| task);
        for task in [self.loading.take(), self.refilling.take(), prefetch]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        self.pending = None;
        self.continuation = None;
        self.exhausted = false;
        self.waiting = false;
        self.at_end = false;
        self.skip_streak = 0;
        self.skip_cap = None;
        self.resume_from = None;
    }

    /// Asks the queue source for a page, tagged with the current queue.
    fn request(&self, req: NextRequest, refill: bool) -> AbortHandle {
        let source = self.source.clone();
        let tx = self.pages_tx.clone();
        let queue_generation = self.queue_generation;
        let task = tokio::spawn(async move {
            let result = source.next(req).await;
            let _ = tx.send(Paged {
                queue_generation,
                refill,
                result,
            });
        });
        task.abort_handle()
    }

    fn on_page(&mut self, p: Paged) {
        if p.queue_generation != self.queue_generation {
            return;
        }
        if p.refill {
            self.refilling = None;
            self.on_refill(p.result);
        } else {
            self.loading = None;
            self.on_load(p.result);
        }
        self.check_preload();
    }

    /// A play's queue arrived (or failed).
    fn on_load(&mut self, result: Result<NextPage, Error>) {
        let Some(plan) = self.pending.take() else {
            return;
        };
        let result = result.and_then(|page| {
            if page.items.is_empty() {
                Err(Error::Unavailable("YouTube sent no queue".into()))
            } else {
                Ok(page)
            }
        });
        let page = match result {
            Ok(page) => page,
            Err(e) => {
                let unavailable = matches!(e, Error::Unavailable(_));
                if plan.report_errors || plan.seed.is_none() {
                    self.emit(EngineEvent::Error {
                        code: e.code(),
                        message: e.to_string(),
                    });
                } else if !unavailable {
                    // The code only: the radio is a background extra, and the song plays on.
                    eprintln!("ytmfast: could not fetch the song's radio ({})", e.code());
                }
                // "No queue" means no songs to come; anything else may work on a later try.
                if unavailable {
                    self.exhausted = true;
                }
                if plan.seed.is_none() {
                    self.status.state = PlayState::Stopped;
                    self.emit_state();
                } else if self.waiting {
                    // Not another request at once: a source that keeps failing would loop.
                    self.stop_at_end();
                }
                return;
            }
        };
        self.continuation = page.continuation;
        let kept = self.kept(&plan.added);
        // The user is already on a song they added (skipped or jumped to it) while the list
        // was coming: it stays current, with its queue id, and the list fits around it.
        let playing_kept = kept.played.last().map(|i| i.id);
        match plan.seed {
            None => {
                match plan.index {
                    Some(i) => self.queue.replace(page.items, i),
                    // Shuffled, a random song starts (`Queue::replace_unpicked`).
                    None => self.queue.replace_unpicked(page.items),
                };
                let start_id = self.queue.current().map(|i| i.id);
                if let (Some(now), Some(start)) = (playing_kept, start_id) {
                    // What the user heard goes before the list's start song, which comes
                    // next: it was never heard. (While shuffled this moves it in the play
                    // order only, like any move.)
                    self.queue.insert_items(kept.played, AddAt::Next);
                    self.queue.jump(now);
                    // `move_to` takes the index after the start song is taken out: it sits
                    // before the current song (the played ones went in right after it), so
                    // the current song's index is the place right after it.
                    let c = self.queue.current_index().unwrap_or(0);
                    let s = self.queue.items().iter().position(|i| i.id == start);
                    let to = if s.is_some_and(|s| s < c) { c } else { c + 1 };
                    self.queue.move_to(start, to);
                }
                self.queue.insert_items(kept.next, AddAt::Next);
                self.queue.insert_items(kept.end, AddAt::End);
                self.emit_queue();
                if playing_kept.is_some() {
                    self.emit_state();
                    if self.waiting {
                        // The added song already ended or failed while the list was coming.
                        self.waiting = false;
                        return self.advance(false, true);
                    }
                } else {
                    let paused = self.status.state == PlayState::Paused;
                    self.start_current(plan.start);
                    if paused {
                        self.pause();
                    }
                }
            }
            Some(seed) => {
                // The seed already plays: find it in the list (at the play's index if it is
                // there), so its item there becomes current; a list without it gets it first.
                let mut songs = page.items;
                let at = plan
                    .index
                    .filter(|&i| songs.get(i).is_some_and(|s| s.video_id == seed))
                    .or_else(|| songs.iter().position(|s| s.video_id == seed));
                let at = at.unwrap_or_else(|| {
                    songs.insert(0, bare_song(&seed));
                    0
                });
                self.queue.replace(songs, at);
                if let Some(now) = playing_kept {
                    // The seed was heard, then what the user added: they follow it.
                    self.queue.insert_items(kept.played, AddAt::Next);
                    self.queue.jump(now);
                }
                self.queue.insert_items(kept.next, AddAt::Next);
                self.queue.insert_items(kept.end, AddAt::End);
                // With an added song current, the status already shows it (its id is kept).
                if playing_kept.is_none()
                    && let Some(item) = self.queue.current()
                {
                    self.status.queue_id = Some(item.id);
                    self.status.album = item.song.album.clone();
                }
                self.refresh_meta();
                self.emit_queue();
                self.emit_state();
                if self.waiting {
                    // The seed already ended or failed while the list was coming.
                    self.waiting = false;
                    return self.advance(false, true);
                }
            }
        }
        self.maybe_refill();
    }

    /// The songs in `added` still in the queue, sorted for putting back once the list lands:
    /// as if they were added after it. When the current item is one of them, it and the ones
    /// before it are `played`; the rest go by how they were added.
    fn kept(&self, added: &HashMap<u64, AddAt>) -> Kept {
        let mut kept = Kept::default();
        if added.is_empty() {
            return kept;
        }
        let current = self
            .queue
            .current()
            .filter(|i| added.contains_key(&i.id))
            .and(self.queue.current_index());
        for (p, item) in self.queue.items().iter().enumerate() {
            let Some(at) = added.get(&item.id) else {
                continue;
            };
            let to = match (current, at) {
                (Some(c), _) if p <= c => &mut kept.played,
                (_, AddAt::Next) => &mut kept.next,
                (_, AddAt::End) => &mut kept.end,
            };
            to.push(item.clone());
        }
        kept
    }

    /// More radio songs for the end of the queue arrived (or failed).
    fn on_refill(&mut self, result: Result<NextPage, Error>) {
        match result {
            Ok(page) => {
                self.continuation = page.continuation;
                let added = self.queue.append_radio(page.items);
                if added == 0 {
                    // Nothing new (radio pages overlap): asking again could loop.
                    self.exhausted = true;
                } else {
                    self.emit_queue();
                }
                if self.waiting {
                    self.waiting = false;
                    if added > 0 {
                        self.advance(false, true);
                    } else {
                        self.stop_at_end();
                    }
                } else if added > 0 {
                    self.maybe_refill();
                }
            }
            Err(e) => {
                let unavailable = matches!(e, Error::Unavailable(_));
                if unavailable {
                    // "YouTube sent no queue": no more songs (ruling S7).
                    self.exhausted = true;
                } else {
                    eprintln!("ytmfast: could not fetch more radio songs ({})", e.code());
                }
                if self.waiting {
                    if !unavailable {
                        self.emit(EngineEvent::Error {
                            code: e.code(),
                            message: e.to_string(),
                        });
                    }
                    self.stop_at_end();
                }
            }
        }
    }

    /// Fetches more radio songs when the queue is about to run out: the radio's next page if
    /// the queue came from one, else the radio of the queue's last song. Never while a request
    /// is out, and never again once YouTube had no more.
    fn maybe_refill(&mut self) {
        if self.loading.is_some()
            || self.refilling.is_some()
            || self.exhausted
            || !self.queue.needs_more()
        {
            return;
        }
        let Some(last) = self.queue.items().last() else {
            return;
        };
        let req = match &self.continuation {
            Some(c) => NextRequest {
                continuation: Some(c.clone()),
                ..NextRequest::default()
            },
            None => {
                let id = last.song.video_id.clone();
                NextRequest {
                    playlist_id: Some(format!("{RADIO_PREFIX}{id}")),
                    video_id: Some(id),
                    ..NextRequest::default()
                }
            }
        };
        self.refilling = Some(self.request(req, true));
    }

    /// Moves on to the next item: `auto` when the song ended by itself (only then does repeat
    /// one play it again). At the end of the queue it waits for songs on the way, or stops.
    fn advance(&mut self, auto: bool, keep_pause: bool) {
        let paused = keep_pause && self.status.state == PlayState::Paused;
        if self.queue.next(auto).is_some() {
            self.start_current(0.0);
            if paused {
                self.pause();
            }
            self.emit_queue();
            self.maybe_refill();
            return;
        }
        self.maybe_refill();
        if self.loading.is_some() || self.refilling.is_some() {
            self.halt();
            self.waiting = true;
            if !paused {
                self.status.state = PlayState::Buffering;
            }
            self.emit_state();
        } else {
            self.stop_at_end();
        }
    }

    fn stop_at_end(&mut self) {
        self.halt();
        self.waiting = false;
        self.at_end = true;
        self.status.state = PlayState::Stopped;
        self.emit_state();
    }

    /// Stops what plays (keeping its position in the status) and drops its resolve.
    fn halt(&mut self) {
        let mut ended_at = self.status.position;
        if self.loaded && self.started {
            self.status.position = self.player.position();
            ended_at = self.played_to();
        }
        self.end_report(ended_at);
        self.generation += 1;
        if let Some(task) = self.resolving.take() {
            task.abort();
        }
        self.drop_preload();
        self.current = None;
        self.player.stop();
        self.loaded = false;
        self.started = false;
        self.ticker = None;
        self.restored = false;
    }

    fn previous(&mut self) {
        let position = self.snapshot().position;
        let moved = matches!(self.queue.previous(position), Previous::Item(_));
        if moved {
            self.start_current(0.0);
            self.emit_queue();
            self.maybe_refill();
        } else if self.loaded || self.restored {
            // A paused song goes back to its start and stays paused.
            self.seek(0.0);
        } else if self.queue.current().is_some() {
            self.start_current(0.0);
        }
    }

    /// False when the queue is full (nothing changed).
    fn queue_add(&mut self, songs: Vec<SongItem>, at: AddAt) -> bool {
        let first = self.queue.next_id();
        if !self.queue.add(songs, at) {
            return false;
        }
        // A play's list still on its way replaces the queue when it lands: these are put
        // back then.
        if let Some(plan) = &mut self.pending {
            plan.added
                .extend((first..self.queue.next_id()).map(|id| (id, at)));
        }
        self.emit_queue();
        if self.waiting {
            self.waiting = false;
            self.advance(false, true);
        } else {
            self.maybe_refill();
        }
        true
    }

    fn queue_remove(&mut self, id: u64) {
        let was_current = self.queue.current().map(|i| i.id) == Some(id);
        if !self.queue.remove(id) {
            return self.not_in_queue();
        }
        if was_current {
            if self.queue.current().is_some() {
                // The queue's new current item takes the removed song's place, in its state.
                match self.status.state {
                    PlayState::Stopped => self.show_current(),
                    PlayState::Paused => {
                        self.start_current(0.0);
                        self.pause();
                    }
                    PlayState::Playing | PlayState::Buffering => self.start_current(0.0),
                }
            } else {
                self.halt();
                self.waiting = false;
                self.at_end = false;
                self.show_nothing();
                self.status.position = 0.0;
                self.status.state = PlayState::Stopped;
                self.emit_state();
            }
        }
        self.emit_queue();
        self.maybe_refill();
    }

    fn queue_jump(&mut self, id: u64) {
        if self.queue.jump(id).is_none() {
            return self.not_in_queue();
        }
        self.start_current(0.0);
        self.emit_queue();
        self.maybe_refill();
    }

    /// A queue id the queue doesn't have (a widget acting on an old copy of the queue).
    fn not_in_queue(&self) {
        let e = Error::Unavailable("not in the queue".into());
        self.emit(EngineEvent::Error {
            code: e.code(),
            message: e.to_string(),
        });
    }

    /// The status shows the queue's current item without playing it.
    fn show_current(&mut self) {
        let Some(item) = self.queue.current().cloned() else {
            return;
        };
        self.resolved_meta = None;
        self.status.video_id = Some(item.song.video_id.clone());
        self.status.queue_id = Some(item.id);
        self.status.album = item.song.album.clone();
        self.status.meta = song_meta(&item.song, None);
        self.status.position = 0.0;
        self.resume_from = None;
        self.emit_state();
    }

    fn show_nothing(&mut self) {
        self.resolved_meta = None;
        self.status.video_id = None;
        self.status.queue_id = None;
        self.status.album = None;
        self.status.meta = None;
    }

    /// Plays the queue's current item from `start`.
    fn start_current(&mut self, start: f64) {
        let Some(item) = self.queue.current().cloned() else {
            return;
        };
        self.waiting = false;
        self.at_end = false;
        self.resume_from = None;
        self.resolved_meta = None;
        self.status.queue_id = Some(item.id);
        self.status.album = item.song.album.clone();
        // The queue item's details show at once; the link's only fill its gaps.
        self.status.meta = song_meta(&item.song, None);
        // The old song's report ends now, while the preload still tells whether the audio
        // thread already moved on to it (`played_to`); `begin` would see it taken.
        if self.loaded && self.started {
            let at = self.played_to();
            self.end_report(at);
        }
        // Skipped (or jumped) to the preloaded item: its link and download are here already.
        let ready = match self.preload.take() {
            Some(Preload {
                queue_id,
                state: PreloadState::Ready { source, .. },
            }) if queue_id == item.id => Some(source),
            other => {
                self.preload = other;
                None
            }
        };
        match ready {
            Some(source) => self.start_from(source, start),
            None => self.start(item.song.video_id, start),
        }
    }

    /// Drops the old song at once, and shows the new one buffering.
    fn begin(&mut self, video_id: &str, start: f64) {
        // The old song's report ends where it stopped playing.
        let was_at = if self.loaded && self.started {
            self.played_to()
        } else {
            self.status.position
        };
        self.end_report(was_at);
        self.generation += 1;
        if let Some(task) = self.resolving.take() {
            task.abort();
        }
        self.drop_preload();
        self.current = None;
        // Cancels the old track's reader too, so an audio thread stuck opening it is freed.
        self.player.stop();
        self.loaded = false;
        self.started = false;
        self.replayed = false;
        self.restored = false;
        self.ticker = None;
        self.start_seconds = start;
        self.status.state = PlayState::Buffering;
        self.status.video_id = Some(video_id.to_string());
        self.status.position = start;
        self.emit_state();
        crate::trace::play(video_id);
    }

    /// A song whose download is already running (its preload): no resolve, no new download.
    fn start_from(&mut self, source: Source, start: f64) {
        self.begin(&source.video_id, start);
        self.resolved_meta = Some(source.meta.clone());
        self.refresh_meta();
        self.load_source(source);
    }

    /// A new song: drop the old one at once and resolve the new one in the background.
    fn start(&mut self, video_id: String, start: f64) {
        self.begin(&video_id, start);
        let resolver = self.resolver.clone();
        let tx = self.resolved_tx.clone();
        let generation = self.generation;
        let task = tokio::spawn(async move {
            let result = resolver.resolve(&video_id).await;
            let _ = tx.send(Resolved { generation, result });
        });
        self.resolving = Some(task.abort_handle());
    }

    fn on_resolved(&mut self, r: Resolved) {
        if r.generation != self.generation {
            return;
        }
        self.resolving = None;
        let stream = match r.result {
            Ok(s) => s,
            Err(e) => {
                crate::trace::mark("resolve failed");
                // This song can't be played: the next one may. Anything else (signed out,
                // the network, a bug) would fail every song the same way, so it stops.
                if matches!(e, Error::Unavailable(_) | Error::StreamFailed(_)) {
                    return self.skip_unplayable(&e);
                }
                return self.fail(&e);
            }
        };
        crate::trace::mark("link resolved");
        self.resolved_meta = Some(stream.meta.clone());
        self.refresh_meta();
        let known = self.status.meta.as_ref().map_or(0, |m| m.length_seconds);
        let source = self.source(stream, known);
        self.load_source(source);
    }

    /// Starts `stream`'s download. `known` is the song's length from elsewhere (its queue
    /// item), for a link that states none.
    fn source(&self, stream: Stream, known: u32) -> Source {
        let gain = loudness_gain(stream.loudness_db);
        let mime = stream.mime.clone();
        let video_id = stream.video_id.clone();
        let meta = stream.meta.clone();
        // The link's own length first: it describes the file the audio thread decodes.
        let length_hint = [stream.meta.length_seconds, known]
            .into_iter()
            .find(|s| *s > 0)
            .map(f64::from);
        // A link that stops working mid-song is replaced by a fresh one, never a cached one
        // (ruling R2).
        let relink: Relink = {
            let resolver = self.resolver.clone();
            let id = stream.video_id.clone();
            Box::new(move || {
                let resolver = resolver.clone();
                let id = id.clone();
                Box::pin(async move { resolver.resolve_fresh(&id).await.map(|s| s.url) })
            })
        };
        Source {
            buffer: (self.start_buffer)(stream, relink),
            video_id,
            mime,
            gain,
            length_hint,
            meta,
        }
    }

    /// Hands the current song's download to the audio thread, and plays it unless paused.
    fn load_source(&mut self, source: Source) {
        self.player.load(
            source.buffer.reader(),
            &source.mime,
            source.gain,
            self.start_seconds,
            source.length_hint,
        );
        self.current = Some(source);
        self.loads_sent += 1;
        self.loaded = true;
        // Paused while it resolved: it loads paused at its start point.
        if self.status.state == PlayState::Buffering {
            self.player.play();
        }
        self.emit_state();
    }

    /// An unplayable song: report it and move on, unless a queue's worth of songs failed in a
    /// row (a full pass, measured when the run began), which stops rather than skipping round
    /// for ever.
    fn skip_unplayable(&mut self, e: &Error) {
        self.emit(EngineEvent::Error {
            code: e.code(),
            message: e.to_string(),
        });
        self.loaded = false;
        self.started = false;
        self.ticker = None;
        self.skip_streak += 1;
        // While a play's list is still coming, the queue isn't all there yet: the pass is
        // measured at the first failure after it came.
        if self.loading.is_none() && self.skip_cap.is_none() {
            self.skip_cap = Some(self.queue.len());
        }
        if self
            .skip_cap
            .is_some_and(|cap| self.loading.is_none() && self.skip_streak >= cap)
        {
            self.resume_from = None;
            self.status.state = PlayState::Stopped;
            self.emit_state();
            return;
        }
        self.advance(false, true);
    }

    /// The current song's details: its queue item's, with gaps filled from its link.
    fn refresh_meta(&mut self) {
        if let Some(item) = self.queue.current()
            && self.status.video_id.as_deref() == Some(item.song.video_id.as_str())
        {
            self.status.meta = song_meta(&item.song, self.resolved_meta.as_ref());
        }
    }

    fn pause(&mut self) {
        match self.status.state {
            PlayState::Playing => {
                self.player.pause();
                self.status.position = self.player.position();
                if let Some(r) = &self.report {
                    r.pause(self.status.position);
                }
            }
            PlayState::Buffering => {
                // Still resolving: `on_resolved` sees the state and loads without playing.
                if self.loaded {
                    self.player.pause();
                }
            }
            PlayState::Paused | PlayState::Stopped => return,
        }
        self.ticker = None;
        self.status.state = PlayState::Paused;
        self.emit_state();
    }

    fn resume(&mut self) {
        if self.status.state != PlayState::Paused {
            return;
        }
        // The saved song after a restart: only now is its link fetched and its download
        // started, at the saved second (or where a seek since moved it).
        if self.restored {
            let at = self.status.position;
            self.start_current(at);
            self.maybe_refill();
            return;
        }
        if self.loaded {
            self.player.play();
        }
        // A song that never started waits for the audio thread's `Started`.
        if self.loaded && self.started {
            self.set_playing();
            if let Some(r) = &self.report {
                r.resume(self.player.position());
            }
        } else {
            self.status.state = PlayState::Buffering;
        }
        self.emit_state();
    }

    fn seek(&mut self, seconds: f64) {
        if !seconds.is_finite() || self.status.state == PlayState::Stopped {
            return;
        }
        let mut at = seconds.max(0.0);
        if let Some(len) = self.status.meta.as_ref().map(|m| m.length_seconds)
            && len > 0
        {
            // At or past the end: the song is over, as if it had played out.
            if at >= f64::from(len) {
                return self.advance(false, false);
            }
            // The decoder lands at most 1 s before the end (so a seek never lands on
            // silence); the reported position must match where the audio really goes.
            at = at.min((f64::from(len) - 1.0).max(0.0));
        }
        if self.loaded
            && self.started
            && let Some(r) = &self.report
        {
            // Read before the seek goes to the audio thread: where the played range ended.
            r.seek(self.player.position(), at);
        }
        if self.loaded {
            self.player.seek(at);
        }
        // Still resolving: the load starts there. Loaded but not yet open: the audio thread
        // takes the seek after the load, so this is only used for the status until it starts.
        self.start_seconds = at;
        self.status.position = at;
        if let Some(t) = self.ticker.as_mut() {
            // The next tick a whole second after the seek's own position event.
            t.reset();
        }
        self.emit(EngineEvent::Position {
            seconds: at,
            seeked: true,
        });
        self.seeked_to = Some(at);
        self.dirty = true;
    }

    /// The output's volume was changed in a mixer: the status shows it and it is saved, but
    /// it is not sent back to the output, which already has it (and would echo it back).
    fn mixer_volume(&mut self, v: f32) {
        if !v.is_finite() {
            return;
        }
        let v = v.clamp(0.0, 1.0);
        if v == self.status.volume {
            return;
        }
        self.status.volume = v;
        self.dirty = true;
        self.emit_state();
    }

    fn volume(&mut self, v: f32) {
        if v.is_nan() {
            return;
        }
        let v = v.clamp(0.0, 1.0);
        self.player.set_volume(v);
        self.status.volume = v;
        self.dirty = true;
        self.emit_state();
    }

    fn on_audio(&mut self, event: AudioEvent) {
        if event == AudioEvent::Loading {
            self.loads_seen += 1;
            return;
        }
        // About the output, not a track: taken whatever is loaded.
        if let AudioEvent::VolumeChanged(v) = event {
            return self.mixer_volume(v);
        }
        // Not about the current song: an earlier track's news, sent before the audio thread
        // took the newest load (or while the new song is still resolving).
        if !self.loaded || self.loads_seen < self.loads_sent {
            return;
        }
        match event {
            AudioEvent::Started => {
                self.started = true;
                // Heard: the play is reported from here (unless it is the same play going
                // on after an output restart, which kept its report).
                let at = self.player.position();
                match &self.report {
                    // The kept report: it may have been told of a pause before the output
                    // went, and the song plays again from `at`.
                    Some(r) => r.resume(at),
                    None => self.start_report(at),
                }
                // A song played: the skipping run (if any) is over.
                self.skip_streak = 0;
                self.skip_cap = None;
                if self.status.state == PlayState::Buffering {
                    self.set_playing();
                    self.emit_state();
                }
                // A song started with under 10 s left preloads the next at once.
                self.maybe_preload(at);
            }
            AudioEvent::Advanced(ticket) => self.on_advanced(ticket),
            // The engine set these states when it sent the command.
            // Loading and VolumeChanged were taken above.
            AudioEvent::Paused
            | AudioEvent::Resumed
            | AudioEvent::Loading
            | AudioEvent::VolumeChanged(_) => {}
            AudioEvent::Ended => {
                self.status.position = self.player.position();
                self.end_report(self.status.position);
                self.loaded = false;
                self.started = false;
                self.ticker = None;
                // Nothing was handed over: no preload yet, it reached the audio thread after
                // the end (a song shorter than the time its link took), or it couldn't be
                // opened. The next item loads the usual way, from the preload's download if
                // there is one (no second download), unless that download failed.
                let failed = self.preload.as_ref().is_some_and(|p| match &p.state {
                    PreloadState::Ready { source, .. } => source.buffer.failed(),
                    _ => false,
                });
                if failed {
                    self.drop_preload();
                }
                self.advance(true, true);
            }
            AudioEvent::Error(e) => {
                self.status.position = self.player.position();
                let was_paused = self.status.state == PlayState::Paused;
                let replay =
                    e == Error::OutputRestarted && !self.replayed && self.queue.current().is_some();
                // The replay below is the same play going on, not a second one: it keeps its
                // report (and its cpn), so the song isn't counted twice in the history.
                let kept = if replay { self.report.take() } else { None };
                // Its open watch range ends where the output went (as a pause there), so the
                // play up to the failure counts; the replay's `Started` resumes it from where
                // the song plays again. Without this the resume would restart the range and
                // drop the seconds since the last ping.
                if let Some(r) = &kept {
                    r.pause(self.status.position);
                }
                self.fail(&e);
                // The sound server restarted under the song (often a `systemctl restart` or
                // an update): after reporting it, play the song again from where it was, on
                // a new stream. The link is usually still cached, so this is quick.
                if replay {
                    let at = self.status.position;
                    self.start_current(at);
                    self.report = kept;
                    self.replayed = true;
                    // A paused song comes back paused: it loads at its place and waits for a
                    // play, rather than starting by itself after a restart.
                    if was_paused {
                        self.pause();
                    }
                }
            }
        }
    }

    /// The current song failed: report it, and stop, keeping the song and where it stopped
    /// in the status, so a play goes on from there.
    fn fail(&mut self, e: &Error) {
        self.end_report(self.status.position);
        self.drop_preload();
        self.current = None;
        self.loaded = false;
        self.started = false;
        self.ticker = None;
        self.resume_from = Some(self.status.position);
        self.status.state = PlayState::Stopped;
        self.emit(EngineEvent::Error {
            code: e.code(),
            message: e.to_string(),
        });
        self.emit_state();
    }

    fn set_playing(&mut self) {
        self.status.state = PlayState::Playing;
        if self.ticker.is_none() {
            let mut t = tokio::time::interval_at(Instant::now() + TICK, TICK);
            // After a stall, carry on a second from now rather than firing a burst.
            t.set_missed_tick_behavior(MissedTickBehavior::Delay);
            self.ticker = Some(t);
        }
    }

    fn on_tick(&mut self) {
        self.ticks_since_save += 1;
        if self.ticks_since_save >= SAVE_EVERY_TICKS {
            self.dirty = true;
        }
        let seconds = self.player.position();
        self.status.position = seconds;
        if let Some(r) = &self.report {
            r.tick(seconds);
        }
        self.emit(EngineEvent::Position {
            seconds,
            seeked: false,
        });
        self.maybe_prefetch(seconds);
        self.maybe_preload(seconds);
    }

    /// The audio thread moved on to the preload with this id: its song is current now, as if
    /// it had been loaded (but with no load, and no `Buffering`: it is already playing).
    fn on_advanced(&mut self, ticket: u64) {
        // The old song played to its end, whichever item comes next: its report ends there
        // (the player's position is already the new track's, so its length stands in).
        let old_end = self.song_end();
        self.end_report(old_end);
        let next = self.queue.peek_next(true).map(|i| i.id);
        let source = match self.preload.take() {
            Some(Preload {
                queue_id,
                state: PreloadState::Ready { ticket: t, source },
            }) if t == ticket && Some(queue_id) == next => source,
            other => {
                // A preload the engine had dropped (the queue changed just as the audio thread
                // moved on to it): play the item that really is next instead.
                self.preload = other;
                self.status.position = self.player.position();
                self.loaded = false;
                self.started = false;
                self.ticker = None;
                return self.advance(true, true);
            }
        };
        let Some(item) = self.queue.next(true).cloned() else {
            return;
        };
        // What `start_current` and `on_resolved` would set, without the load.
        self.generation += 1;
        if let Some(task) = self.resolving.take() {
            task.abort();
        }
        self.waiting = false;
        self.at_end = false;
        self.resume_from = None;
        self.replayed = false;
        self.skip_streak = 0;
        self.skip_cap = None;
        self.start_seconds = 0.0;
        self.status.video_id = Some(item.song.video_id.clone());
        self.status.queue_id = Some(item.id);
        self.status.album = item.song.album.clone();
        self.resolved_meta = Some(source.meta.clone());
        self.status.meta = song_meta(&item.song, self.resolved_meta.as_ref());
        self.current = Some(source);
        let at = self.player.position();
        self.status.position = at;
        if let Some(t) = self.ticker.as_mut() {
            // Ticks a whole second into the new song, not on the old one's beat.
            t.reset();
        }
        // Heard from its first frame: a new play, with its own report (and cpn).
        self.start_report(at);
        self.emit_queue();
        self.emit_state();
        self.maybe_refill();
        // A short song: the one after it is due at once.
        self.maybe_preload(at);
    }

    /// The current song's end: its length, else the last position seen.
    fn song_end(&self) -> f64 {
        self.status
            .meta
            .as_ref()
            .map(|m| f64::from(m.length_seconds))
            .filter(|l| *l > 0.0)
            .unwrap_or(self.status.position)
    }

    /// Where the current song has got to, for its report. Between the audio thread moving on
    /// to the preload and the engine taking its `Advanced`, the player's position is already
    /// the next song's: the current one then played to its end. The position is read before
    /// the id (`AudioPlayer::advanced_to`), so a position that is the next song's always comes
    /// with that id.
    fn played_to(&self) -> f64 {
        let at = self.player.position();
        match &self.preload {
            Some(Preload {
                state: PreloadState::Ready { ticket, .. },
                ..
            }) if self.player.advanced_to() == *ticket => self.song_end(),
            _ => at,
        }
    }

    /// Starts the report of the song now heard, at `at`.
    fn start_report(&mut self, at: f64) {
        self.end_report(at);
        let (Some(reporter), Some(video_id)) = (&self.reporter, &self.status.video_id) else {
            return;
        };
        let length = self
            .status
            .meta
            .as_ref()
            .map_or(0.0, |m| f64::from(m.length_seconds));
        self.report = Some(reporter.start(video_id, report::cpn(), at, length));
    }

    /// Ends the current song's report (if it has one) at `at`.
    fn end_report(&mut self, at: f64) {
        if let Some(r) = self.report.take() {
            r.end(at);
        }
    }

    /// From 10 s before the current song's end (`PRELOAD_LEAD_SECS`), makes the next item
    /// ready and hands it to the audio thread. Once per next item.
    fn maybe_preload(&mut self, position: f64) {
        if !(self.loaded && self.started) || self.preload.is_some() {
            return;
        }
        let Some(len) = self
            .status
            .meta
            .as_ref()
            .map(|m| m.length_seconds)
            .filter(|l| *l > 0)
        else {
            return;
        };
        if f64::from(len) - position > PRELOAD_LEAD_SECS {
            return;
        }
        let Some(next) = self.queue.peek_next(true).cloned() else {
            return;
        };
        // Repeat one, or the same song twice in a row: a second reader over the bytes that
        // are already here, not a second download.
        if let Some(current) = self
            .current
            .as_ref()
            .filter(|c| c.video_id == next.song.video_id)
        {
            let source = current.clone();
            return self.preload_source(next.id, source);
        }
        self.preload_generation += 1;
        let generation = self.preload_generation;
        let resolver = self.resolver.clone();
        let tx = self.preloads_tx.clone();
        let video_id = next.song.video_id.clone();
        let task = tokio::spawn(async move {
            // Usually from the link cache: the prefetch at half the song resolved it.
            let result = resolver.resolve(&video_id).await;
            let _ = tx.send(Preresolved { generation, result });
        });
        self.preload = Some(Preload {
            queue_id: next.id,
            state: PreloadState::Resolving {
                generation,
                task: task.abort_handle(),
            },
        });
    }

    /// The next item's link: start its download and hand it to the audio thread.
    fn on_preloaded(&mut self, p: Preresolved) {
        let queue_id = match &self.preload {
            Some(Preload {
                queue_id,
                state: PreloadState::Resolving { generation, .. },
            }) if *generation == p.generation => *queue_id,
            _ => return,
        };
        match p.result {
            Ok(stream) => {
                let known = self
                    .queue
                    .items()
                    .iter()
                    .find(|i| i.id == queue_id)
                    .map_or(0, |i| i.song.length_seconds);
                let source = self.source(stream, known);
                self.preload_source(queue_id, source);
            }
            Err(e) => {
                // The code only (R6); the song's own turn tries again and reports it.
                eprintln!("ytmfast: could not preload the next song ({})", e.code());
                self.preload = Some(Preload {
                    queue_id,
                    state: PreloadState::Failed,
                });
            }
        }
    }

    fn preload_source(&mut self, queue_id: u64, source: Source) {
        let ticket = self.player.preload(
            source.buffer.reader(),
            &source.mime,
            source.gain,
            source.length_hint,
        );
        self.preload = Some(Preload {
            queue_id,
            state: PreloadState::Ready { ticket, source },
        });
    }

    /// Drops the preload, wherever it got to.
    fn drop_preload(&mut self) {
        match self.preload.take().map(|p| p.state) {
            Some(PreloadState::Resolving { task, .. }) => task.abort(),
            Some(PreloadState::Ready { .. }) => self.player.cancel_preload(),
            Some(PreloadState::Failed) | None => {}
        }
    }

    /// Keeps the preload only while it is still for the next item, and preloads the next one
    /// if it is time (a queue change near the end of a song).
    fn check_preload(&mut self) {
        let next = self.queue.peek_next(true).map(|i| i.id);
        if self
            .preload
            .as_ref()
            .is_some_and(|p| Some(p.queue_id) != next)
        {
            self.drop_preload();
        }
        if self.loaded && self.started {
            let at = self.player.position();
            self.maybe_preload(at);
        }
    }

    /// Past half the song, fetches the next song's link, so it starts without waiting for
    /// one (the resolver keeps it in its link cache). Once per next item; a newer prefetch
    /// replaces an older one.
    fn maybe_prefetch(&mut self, position: f64) {
        let Some(len) = self
            .status
            .meta
            .as_ref()
            .map(|m| m.length_seconds)
            .filter(|l| *l > 0)
        else {
            return;
        };
        if position < f64::from(len) / 2.0 {
            return;
        }
        let Some(next) = self.queue.peek_next(true) else {
            return;
        };
        // Repeat one (or the same song queued twice): its link is the one playing.
        if self.prefetch.as_ref().is_some_and(|(id, _)| *id == next.id)
            || self.status.video_id.as_deref() == Some(next.song.video_id.as_str())
        {
            return;
        }
        let (id, video_id) = (next.id, next.song.video_id.clone());
        if let Some((_, old)) = self.prefetch.take() {
            old.abort();
        }
        let resolver = self.resolver.clone();
        let task = tokio::spawn(async move {
            // Only the cache matters here; a failure shows when the song's turn comes.
            let _ = resolver.resolve(&video_id).await;
        });
        self.prefetch = Some((id, task.abort_handle()));
    }

    /// The status, with the position fresh from the audio thread once the song has started
    /// (before that, the audio thread's position is still the old song's or zero).
    fn snapshot(&mut self) -> Status {
        if self.loaded && self.started {
            self.status.position = self.player.position();
        }
        self.status.shuffle = self.queue.shuffle();
        self.status.repeat = self.queue.repeat();
        self.status.clone()
    }

    fn queue_view(&self) -> QueueView {
        QueueView {
            items: self.queue.items().into(),
            current_id: self.queue.current().map(|i| i.id),
            shuffle: self.queue.shuffle(),
            repeat: self.queue.repeat(),
        }
    }

    /// Also marks the state for saving: every queue change, and every song change (which
    /// moves the current item), comes through here.
    fn emit_queue(&mut self) {
        self.dirty = true;
        let QueueView {
            items,
            current_id,
            shuffle,
            repeat,
        } = self.queue_view();
        self.emit(EngineEvent::Queue {
            items,
            current_id,
            shuffle,
            repeat,
        });
    }

    fn emit_state(&mut self) {
        let status = self.snapshot();
        // A pause or a stop is saved; Buffering and Playing are not (a song change already
        // was, through `emit_queue`, and playing is saved every 30 s).
        if matches!(status.state, PlayState::Paused | PlayState::Stopped) {
            self.dirty = true;
        }
        self.emit(EngineEvent::State(status));
    }

    /// Hands a snapshot to the writer (which writes it on its own thread).
    fn write_state(&mut self) {
        self.dirty = false;
        self.ticks_since_save = 0;
        if self.writer.is_some() {
            let saved = self.saved();
            if let Some(w) = &self.writer {
                w.submit(saved);
            }
        }
        self.seeked_to = None;
    }

    /// What `state.json` gets: at most `state::MAX_ITEMS` songs around the current one, and
    /// the second to resume at.
    fn saved(&mut self) -> Saved {
        let status = self.snapshot();
        let items = self.queue.items();
        let current = self.queue.current_index();
        let range = state::window(items.len(), current.unwrap_or(0), state::MAX_ITEMS);
        let queue = items[range.clone()]
            .iter()
            .map(|i| i.song.clone())
            .collect();
        let original_order = self.queue.original_positions().map(|order| {
            order
                .into_iter()
                .filter(|p| range.contains(p))
                .map(|p| p - range.start)
                .collect()
        });
        let position = match status.state {
            // Stopped mid-song by an error: a play goes on from there. Stopped at the end of
            // the queue (or before anything played): from the start.
            PlayState::Stopped => self.resume_from.unwrap_or(0.0),
            _ => self.seeked_to.unwrap_or(status.position),
        };
        Saved {
            version: state::VERSION,
            queue,
            current_index: current.map_or(0, |c| c - range.start),
            position,
            volume: status.volume,
            shuffle: status.shuffle,
            original_order,
            repeat: status.repeat,
            source_playlist: self.source_playlist.clone(),
            continuation: self.continuation.clone(),
            exhausted: self.exhausted,
            saved_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        }
    }

    fn emit(&self, event: EngineEvent) {
        // No listener is fine: nobody is connected.
        let _ = self.events.send(event);
    }
}

/// A queue item for a song known only by its id (its details come with its link, or with
/// the list it is found in).
fn bare_song(video_id: &str) -> SongItem {
    SongItem {
        video_id: video_id.into(),
        ..SongItem::default()
    }
}

/// What the bar shows for a queue song: its own details, with gaps filled from its link's
/// (`resolved`). A song with no title (a bare id) shows its link's details alone.
fn song_meta(song: &SongItem, resolved: Option<&TrackMeta>) -> Option<TrackMeta> {
    if song.title.is_empty() {
        return resolved.cloned();
    }
    let artist = if song.artists.is_empty() {
        resolved.map(|m| m.artist.clone()).unwrap_or_default()
    } else {
        song.artists.join(", ")
    };
    Some(TrackMeta {
        title: song.title.clone(),
        artist,
        length_seconds: if song.length_seconds > 0 {
            song.length_seconds
        } else {
            resolved.map_or(0, |m| m.length_seconds)
        },
        thumbnail: song
            .thumbnail
            .clone()
            .or_else(|| resolved.and_then(|m| m.thumbnail.clone())),
    })
}

/// Waits for the position clock's next tick; never, while there is no clock.
async fn next_tick(ticker: &mut Option<Interval>) {
    match ticker {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Moves the audio thread's events (a blocking crossbeam channel) onto a tokio channel, on a
/// small thread of its own: the engine then waits on them like on anything else, without a
/// polling timer. The thread ends when the audio thread does.
fn forward_audio_events(
    player: &AudioPlayer,
) -> (mpsc::UnboundedReceiver<AudioEvent>, Option<JoinHandle<()>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let source = player.events();
    let thread = std::thread::Builder::new()
        .name("ytmfast-events".into())
        .spawn(move || {
            while let Ok(event) = source.recv() {
                if tx.send(event).is_err() {
                    return;
                }
            }
        });
    // Without the thread the engine still takes commands; it just never hears that a song
    // started or ended, which the error makes visible.
    let thread = match thread {
        Ok(t) => Some(t),
        Err(_) => {
            eprintln!("ytmfast: could not start the audio event thread");
            None
        }
    };
    (rx, thread)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sink::{NullSink, NullStats};
    use crate::error::Error;
    use crate::innertube::Tracking;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tokio::time::Instant;
    use url::Url;

    const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
    /// The id whose link points at a server path that never answers.
    const STALL: &str = "STALLSTALL1";

    fn fixture() -> Arc<Vec<u8>> {
        let path = format!(
            "{}/tests/fixtures/sine440_48k.webm",
            env!("CARGO_MANIFEST_DIR")
        );
        Arc::new(std::fs::read(path).unwrap())
    }

    /// A local HTTP server: `/stall` takes the request and never answers; any other path
    /// gets the fixture in one 200. Records the paths asked for.
    struct Server {
        base: Url,
        paths: Arc<Mutex<Vec<String>>>,
    }

    async fn server() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = paths.clone();
        let data = fixture();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (data, seen) = (data.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut got = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&got).into_owned();
                    let path = head.split(' ').nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path.clone());
                    if path.starts_with("/stall") {
                        // Hold the connection open until the client hangs up.
                        let _ = sock.read(&mut buf).await;
                        return;
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&data).await;
                });
            }
        });
        Server { base, paths }
    }

    /// A resolver with a per-id delay and failure; links point at the test server.
    struct Fake {
        base: Url,
        delays: HashMap<String, Duration>,
        failures: HashMap<String, Error>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn stream(&self, id: &str) -> Stream {
            let path = if id == STALL { "stall" } else { id };
            Stream {
                video_id: id.into(),
                url: format!("{}{path}", self.base),
                itag: 251,
                mime: OPUS_MIME.into(),
                content_length: None,
                expires_unix: u64::MAX,
                loudness_db: None,
                meta: meta(id),
                tracking: Tracking::default(),
            }
        }
    }

    fn meta(id: &str) -> TrackMeta {
        TrackMeta {
            title: format!("Song {id}"),
            artist: "Artist".into(),
            length_seconds: 2,
            thumbnail: None,
        }
    }

    #[async_trait]
    impl Resolver for Fake {
        async fn resolve(&self, id: &str) -> Result<Stream, Error> {
            self.calls.lock().unwrap().push(id.to_string());
            if let Some(d) = self.delays.get(id) {
                tokio::time::sleep(*d).await;
            }
            match self.failures.get(id) {
                Some(e) => Err(e.clone()),
                None => Ok(self.stream(id)),
            }
        }

        async fn resolve_fresh(&self, id: &str) -> Result<Stream, Error> {
            self.resolve(id).await
        }
    }

    /// A queue source: answers by playlist id (or the continuation token), after an optional
    /// delay; anything else is "no queue". Records every request.
    #[derive(Default)]
    struct FakeSource {
        pages: HashMap<String, (Duration, Result<NextPage, Error>)>,
        requests: Mutex<Vec<NextRequest>>,
    }

    #[async_trait]
    impl QueueSource for FakeSource {
        async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
            self.requests.lock().unwrap().push(req.clone());
            let key = req.continuation.or(req.playlist_id).unwrap_or_default();
            match self.pages.get(&key) {
                Some((delay, answer)) => {
                    tokio::time::sleep(*delay).await;
                    answer.clone()
                }
                None => Err(Error::Unavailable("YouTube sent no queue".into())),
            }
        }
    }

    impl FakeSource {
        fn requests(&self) -> Vec<NextRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[derive(Default)]
    struct Setup {
        /// Queue pages: (playlist id or continuation, delay in ms, answer).
        pages: Vec<(String, u64, Result<NextPage, Error>)>,
        delays: Vec<(&'static str, u64)>,
        failures: Vec<(&'static str, Error)>,
        /// A sink that plays as fast as it can, instead of in real time.
        fast: bool,
        /// An output that can't be opened (no sound server to reach).
        no_output: bool,
        /// A saved state the engine takes up before it runs.
        saved: Option<Saved>,
        /// Where it saves.
        writer: Option<Writer>,
        /// Reports plays to a recording fake.
        reports: bool,
    }

    struct Rig {
        cmds: mpsc::Sender<EngineCmd>,
        events: broadcast::Receiver<EngineEvent>,
        /// The ids whose download was started, in order.
        started: Arc<Mutex<Vec<String>>>,
        stats: Arc<NullStats>,
        resolver: Arc<Fake>,
        source: Arc<FakeSource>,
        server: Server,
        task: JoinHandle<()>,
        reports: Arc<FakeReports>,
    }

    /// Hands out made-up history links and records what the reports ask and send. Nothing
    /// leaves the process.
    #[derive(Default)]
    struct FakeReports {
        tracked: Mutex<Vec<String>>,
        pings: Mutex<Vec<Url>>,
    }

    #[async_trait]
    impl crate::report::ReportApi for FakeReports {
        async fn tracking(&self, video_id: &str) -> Result<Tracking, Error> {
            self.tracked.lock().unwrap().push(video_id.into());
            let link = |kind: &str| {
                Some(format!(
                    "https://s.youtube.com/api/stats/{kind}?docid={video_id}"
                ))
            };
            Ok(Tracking {
                playback_url: link("playback"),
                watchtime_url: link("watchtime"),
                visitor_data: None,
            })
        }
        async fn ping(&self, url: Url, _: Option<String>) -> Result<(), Error> {
            self.pings.lock().unwrap().push(url);
            Ok(())
        }
    }

    impl FakeReports {
        /// The `cpn` of every ping of `kind`, in order.
        fn cpns(&self, kind: &str) -> Vec<String> {
            self.pings
                .lock()
                .unwrap()
                .iter()
                .filter(|u| u.path().ends_with(kind))
                .filter_map(|u| u.query_pairs().find(|(k, _)| k == "cpn"))
                .map(|(_, v)| v.into_owned())
                .collect()
        }
        /// Every watch-time ping's `st`, `et` and `state`, in order.
        fn watch_ranges(&self) -> Vec<(f64, f64, String)> {
            self.pings
                .lock()
                .unwrap()
                .iter()
                .filter(|u| u.path().ends_with("watchtime"))
                .map(|u| {
                    let get = |key: &str| {
                        u.query_pairs()
                            .find(|(k, _)| k == key)
                            .map(|(_, v)| v.into_owned())
                            .unwrap_or_default()
                    };
                    (
                        get("st").parse().unwrap(),
                        get("et").parse().unwrap(),
                        get("state"),
                    )
                })
                .collect()
        }
        fn finals(&self) -> usize {
            self.pings
                .lock()
                .unwrap()
                .iter()
                .filter(|u| u.query_pairs().any(|(k, v)| k == "final" && v == "1"))
                .count()
        }
    }

    fn sink(fast: bool) -> (Box<NullSink>, Arc<NullStats>) {
        let sink = if fast {
            NullSink::new()
        } else {
            NullSink::realtime()
        };
        let stats = sink.stats();
        (Box::new(sink), stats)
    }

    /// An engine that hasn't been run, and its handles.
    struct Built {
        engine: Engine,
        cmds: mpsc::Sender<EngineCmd>,
        events: broadcast::Sender<EngineEvent>,
        started: Arc<Mutex<Vec<String>>>,
        stats: Arc<NullStats>,
    }

    /// The engine with its download starter pointed at the test server.
    /// An output with no sound server behind it: every open fails, like `PipeWireSink`'s
    /// when PipeWire can't be reached.
    struct NoOutput;

    impl crate::audio::sink::Sink for NoOutput {
        fn open(&mut self, _: u32, _: u16) -> Result<(), Error> {
            Err(Error::Internal("could not connect to PipeWire".into()))
        }
        fn write(&mut self, _: &[f32]) -> Result<(), Error> {
            Err(Error::Internal("the output is not open".into()))
        }
        fn pause(&mut self, _: bool) {}
        fn flush(&mut self) {}
        fn set_volume(&mut self, _: f32) {}
        fn delay_frames(&self) -> u64 {
            0
        }
    }

    fn engine_for(
        server: &Server,
        resolver: Arc<Fake>,
        source: Arc<FakeSource>,
        fast: bool,
        no_output: bool,
    ) -> Built {
        let (null, stats) = sink(fast);
        let sink: Box<dyn crate::audio::sink::Sink> =
            if no_output { Box::new(NoOutput) } else { null };
        let started = Arc::new(Mutex::new(Vec::new()));
        let base = server.base.clone();
        let log = started.clone();
        let starter: Starter = Box::new(move |stream, relink| {
            log.lock().unwrap().push(stream.video_id.clone());
            // No timeouts: a paused test clock would fire them at once (see fetch's tests).
            TrackBuffer::start_with_test_base(stream, relink, base.clone(), reqwest::Client::new())
        });
        let (engine, cmds, events) =
            Engine::with_starter(resolver, source, AudioPlayer::spawn(sink), starter);
        Built {
            engine,
            cmds,
            events,
            started,
            stats,
        }
    }

    async fn rig(setup: Setup) -> Rig {
        let server = server().await;
        let resolver = Arc::new(Fake {
            base: server.base.clone(),
            delays: setup
                .delays
                .into_iter()
                .map(|(id, ms)| (id.to_string(), Duration::from_millis(ms)))
                .collect(),
            failures: setup
                .failures
                .into_iter()
                .map(|(id, e)| (id.to_string(), e))
                .collect(),
            calls: Mutex::new(Vec::new()),
        });
        let source = Arc::new(FakeSource {
            pages: setup
                .pages
                .into_iter()
                .map(|(key, ms, answer)| (key, (Duration::from_millis(ms), answer)))
                .collect(),
            ..FakeSource::default()
        });
        let Built {
            mut engine,
            cmds,
            events,
            started,
            stats,
        } = engine_for(
            &server,
            resolver.clone(),
            source.clone(),
            setup.fast,
            setup.no_output,
        );
        if let Some(saved) = setup.saved {
            engine.restore(saved);
        }
        if let Some(writer) = setup.writer {
            engine.save_with(writer);
        }
        let reports = Arc::new(FakeReports::default());
        if setup.reports {
            engine.report_with(Reporter::new(reports.clone()));
        }
        let events = events.subscribe();
        let task = tokio::spawn(engine.run());
        Rig {
            cmds,
            events,
            started,
            stats,
            resolver,
            source,
            server,
            task,
            reports,
        }
    }

    impl Rig {
        async fn send(&self, cmd: EngineCmd) {
            self.cmds.send(cmd).await.unwrap();
        }

        async fn play(&self, id: &str) {
            self.send(EngineCmd::Play {
                video_id: Some(id.into()),
                playlist_id: None,
                index: None,
                start_seconds: 0.0,
            })
            .await;
        }

        /// The next event, failing the test after 5 s (not for paused-clock tests).
        async fn next(&mut self) -> EngineEvent {
            tokio::time::timeout(Duration::from_secs(5), self.events.recv())
                .await
                .expect("an event within 5 s")
                .expect("the event channel is open")
        }

        /// Events up to and including the first state of `want`.
        async fn until(&mut self, want: PlayState) -> Vec<EngineEvent> {
            let mut seen = Vec::new();
            loop {
                let e = self.next().await;
                let done = matches!(&e, EngineEvent::State(s) if s.state == want);
                seen.push(e);
                if done {
                    return seen;
                }
            }
        }

        async fn status(&self) -> Status {
            let (tx, rx) = oneshot::channel();
            self.send(EngineCmd::Status(tx)).await;
            rx.await.unwrap()
        }

        fn started(&self) -> Vec<String> {
            self.started.lock().unwrap().clone()
        }

        fn calls(&self) -> Vec<String> {
            self.resolver.calls.lock().unwrap().clone()
        }

        async fn play_list(&self, playlist: &str, index: Option<usize>) {
            self.send(EngineCmd::Play {
                video_id: None,
                playlist_id: Some(playlist.into()),
                index,
                start_seconds: 0.0,
            })
            .await;
        }

        /// Events up to and including the first state of `want` for song `id`.
        async fn until_song(&mut self, id: &str, want: PlayState) -> Vec<EngineEvent> {
            let mut seen = Vec::new();
            loop {
                let e = self.next().await;
                let done = matches!(&e, EngineEvent::State(s)
                    if s.state == want && s.video_id.as_deref() == Some(id));
                seen.push(e);
                if done {
                    return seen;
                }
            }
        }

        /// The next `Queue` event.
        async fn until_queue(&mut self) -> QueueView {
            loop {
                if let EngineEvent::Queue {
                    items,
                    current_id,
                    shuffle,
                    repeat,
                } = self.next().await
                {
                    return QueueView {
                        items,
                        current_id,
                        shuffle,
                        repeat,
                    };
                }
            }
        }

        async fn queue(&self) -> QueueView {
            let (tx, rx) = oneshot::channel();
            self.send(EngineCmd::QueueGet(tx)).await;
            rx.await.unwrap()
        }
    }

    /// An 11-character id of one repeated letter.
    fn vid(c: char) -> String {
        c.to_string().repeat(11)
    }

    /// A queue song with every detail filled, unlike what the resolver says (`meta`).
    fn song(c: char) -> SongItem {
        SongItem {
            video_id: vid(c),
            title: format!("Title {c}"),
            artists: vec!["One".into(), "Two".into()],
            album: Some("Album".into()),
            thumbnail: Some(format!("https://i.ytimg.com/{c}.jpg")),
            length_seconds: 2,
            playlist_id: None,
        }
    }

    fn page(songs: &str, continuation: Option<&str>) -> NextPage {
        NextPage {
            items: songs.chars().map(song).collect(),
            continuation: continuation.map(String::from),
            playlist_id: None,
        }
    }

    fn ok(
        key: &str,
        ms: u64,
        songs: &str,
        continuation: Option<&str>,
    ) -> (String, u64, Result<NextPage, Error>) {
        (key.into(), ms, Ok(page(songs, continuation)))
    }

    fn radio_of(c: char) -> String {
        format!("RDAMVM{}", vid(c))
    }

    fn errors(events: &[EngineEvent]) -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Error { code, .. } => Some(*code),
                _ => None,
            })
            .collect()
    }

    fn id_of(q: &QueueView, c: char) -> u64 {
        q.items
            .iter()
            .find(|i| i.song.video_id == vid(c))
            .unwrap()
            .id
    }

    fn states(events: &[EngineEvent]) -> Vec<Status> {
        events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::State(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn no_errors(events: &[EngineEvent]) {
        for e in events {
            assert!(!matches!(e, EngineEvent::Error { .. }), "unexpected {e:?}");
        }
    }

    /// Waits up to 3 s of real time for `f` to hold.
    async fn eventually(what: &str, f: impl Fn() -> bool) {
        let t = std::time::Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(3), "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_restarted_output_plays_the_song_again_once_where_it_was() {
        let mut r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Where the song was just before the restart, and how long it could go on after it.
        let before = r.status().await.position;
        let lost_at = std::time::Instant::now();
        r.stats.lose_output();
        let seen = r.until(PlayState::Playing).await;
        let window = lost_at.elapsed().as_secs_f64();
        // The restart is reported, then the same song plays again from where it was.
        let errors: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Error { code, message } => Some((*code, message.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            errors,
            [(
                "internal",
                "internal error: the audio output restarted".into()
            )]
        );
        let states = states(&seen);
        let mut kinds: Vec<_> = states.iter().map(|s| s.state).collect();
        // A play reports Buffering twice: when asked, and again with the song's details.
        kinds.dedup();
        assert_eq!(
            kinds,
            [PlayState::Stopped, PlayState::Buffering, PlayState::Playing]
        );
        // The replay starts where the song was when the output went: not before the last
        // position seen, and no further than the real time that passed since (no fixed window,
        // so a slow test machine can't fail it).
        let at = states[1].position;
        assert!(before > 0.0, "the song played before the restart");
        assert!(
            at >= before && at <= before + window,
            "restarted at {at}, was at {before}, {window} s later"
        );
        assert_eq!(r.started(), ["AAAAAAAAAAA", "AAAAAAAAAAA"]);

        // Once per play: a second restart in the same song is only reported.
        r.stats.lose_output();
        let seen = r.until(PlayState::Stopped).await;
        assert!(seen.iter().any(|e| matches!(
            e,
            EngineEvent::Error {
                code: "internal",
                ..
            }
        )));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(r.started().len(), 2, "no second retry");
        assert_eq!(r.status().await.state, PlayState::Stopped);

        // A new play is a new song: its own restart gets its own retry.
        r.play("BBBBBBBBBBB").await;
        r.until(PlayState::Playing).await;
        r.stats.lose_output();
        r.until(PlayState::Stopped).await;
        r.until(PlayState::Playing).await;
        assert_eq!(r.started().len(), 4);
    }

    #[tokio::test]
    async fn an_output_restart_keeps_the_plays_report_and_a_replay_gets_a_new_one() {
        let mut r = rig(Setup {
            reports: true,
            ..Setup::default()
        })
        .await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        r.stats.lose_output();
        r.until(PlayState::Stopped).await;
        r.until(PlayState::Playing).await;
        // Played to its end after the restart: one play, one report.
        r.until(PlayState::Stopped).await;
        eventually("the end ping", || r.reports.finals() == 1).await;
        assert_eq!(*r.reports.tracked.lock().unwrap(), ["AAAAAAAAAAA"]);
        let first = r.reports.cpns("playback");
        assert_eq!(first.len(), 1);
        assert!(r.reports.cpns("watchtime").iter().all(|c| *c == first[0]));
        // The watch ranges join up across the restart: the range open when the output went
        // was closed there (as a pause), and the replay's range starts where it left off, so
        // no play time is lost.
        let ranges = r.reports.watch_ranges();
        assert!(
            ranges
                .iter()
                .any(|(st, et, state)| state == "paused" && *et > *st),
            "{ranges:?}"
        );
        for pair in ranges.windows(2) {
            assert!((pair[1].0 - pair[0].1).abs() < 0.1, "{ranges:?}");
        }
        let played: f64 = ranges.iter().map(|(st, et, _)| et - st).sum();
        assert!(played > 1.8, "{played} s of a 2 s song: {ranges:?}");

        // Played again after its end: a new play, with a new cpn.
        r.send(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        })
        .await;
        r.until(PlayState::Playing).await;
        eventually("the second playback ping", || {
            r.reports.cpns("playback").len() == 2
        })
        .await;
        let both = r.reports.cpns("playback");
        assert_ne!(both[0], both[1]);
    }

    #[tokio::test]
    async fn a_song_skipped_while_buffering_is_never_reported() {
        let mut r = rig(Setup {
            reports: true,
            delays: vec![("AAAAAAAAAAA", 300)],
            ..Setup::default()
        })
        .await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Buffering).await;
        r.play("BBBBBBBBBBB").await;
        r.until_song("BBBBBBBBBBB", PlayState::Playing).await;
        eventually("B's playback ping", || {
            r.reports.cpns("playback").len() == 1
        })
        .await;
        assert_eq!(*r.reports.tracked.lock().unwrap(), ["BBBBBBBBBBB"]);
    }

    #[tokio::test]
    async fn a_restart_while_paused_reloads_the_song_paused_where_it_was() {
        let mut r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        r.send(EngineCmd::Pause).await;
        r.until(PlayState::Paused).await;
        let before = r.status().await.position;
        r.stats.lose_output();
        // Reported at once, without a play, then the song is loaded again, still paused.
        let seen = r.until(PlayState::Paused).await;
        assert!(
            seen.iter().any(|e| matches!(
                e,
                EngineEvent::Error {
                    code: "internal",
                    ..
                }
            )),
            "{seen:?}"
        );
        let status = r.status().await;
        assert_eq!(status.state, PlayState::Paused);
        // The audio thread's own paused position: the engine's figure from the moment it sent
        // the pause can be a packet (20 ms) behind it.
        assert!(
            (status.position - before).abs() < 0.1,
            "reloaded at {}, paused at {before}",
            status.position
        );
        eventually("the song is downloaded again", || r.started().len() == 2).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(e) = r.events.try_recv() {
            assert!(
                !matches!(&e, EngineEvent::State(s) if s.state == PlayState::Playing),
                "a paused song must not start by itself"
            );
        }
        assert!(r.stats.paused(), "the output stays paused");
        // A play resumes it from there.
        r.send(EngineCmd::Toggle).await;
        r.until(PlayState::Playing).await;
    }

    #[tokio::test]
    async fn an_output_that_cannot_open_is_an_error_on_the_first_play() {
        // Found live with PipeWire unreachable: the first play stayed `buffering` for ever,
        // and only the second reported the error.
        let mut r = rig(Setup {
            no_output: true,
            ..Setup::default()
        })
        .await;
        for _ in 0..2 {
            r.play("AAAAAAAAAAA").await;
            let seen = r.until(PlayState::Stopped).await;
            assert!(
                seen.iter().any(|e| matches!(
                    e,
                    EngineEvent::Error {
                        code: "internal",
                        ..
                    }
                )),
                "{seen:?}"
            );
        }
    }

    #[tokio::test]
    async fn play_emits_buffering_then_playing() {
        let mut r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        let seen = r.until(PlayState::Playing).await;
        no_errors(&seen);
        let states = states(&seen);
        let first = &states[0];
        assert_eq!(first.state, PlayState::Buffering);
        assert_eq!(first.video_id.as_deref(), Some("AAAAAAAAAAA"));
        assert_eq!(first.position, 0.0);
        let last = states.last().unwrap();
        assert_eq!(last.meta, Some(meta("AAAAAAAAAAA")));
        assert_eq!(r.started(), ["AAAAAAAAAAA"]);
        assert_eq!(r.status().await.state, PlayState::Playing);
        eventually("the sink is written to", || r.stats.frames() > 0).await;
    }

    #[tokio::test]
    async fn latest_play_wins() {
        let mut r = rig(Setup {
            delays: vec![("AAAAAAAAAAA", 500)],
            ..Setup::default()
        })
        .await;
        r.play("AAAAAAAAAAA").await;
        // A's resolve is under way (and will take 500 ms) when B is picked.
        let resolver = r.resolver.clone();
        eventually("A is being resolved", || {
            resolver
                .calls
                .lock()
                .unwrap()
                .contains(&"AAAAAAAAAAA".into())
        })
        .await;
        r.play("BBBBBBBBBBB").await;
        let mut seen = r.until(PlayState::Playing).await;
        assert_eq!(r.status().await.video_id.as_deref(), Some("BBBBBBBBBBB"));
        // Past the moment A's resolve would have come back.
        tokio::time::sleep(Duration::from_millis(700)).await;
        while let Ok(e) = r.events.try_recv() {
            seen.push(e);
        }
        no_errors(&seen);
        assert_eq!(r.started(), ["BBBBBBBBBBB"], "only B was loaded");
        let status = r.status().await;
        assert_eq!(status.video_id.as_deref(), Some("BBBBBBBBBBB"));
        assert_eq!(status.state, PlayState::Playing);
        // After B's first state, nothing about A.
        let states = states(&seen);
        let b = states
            .iter()
            .position(|s| s.video_id.as_deref() == Some("BBBBBBBBBBB"))
            .unwrap();
        assert!(
            states[b..]
                .iter()
                .all(|s| s.video_id.as_deref() == Some("BBBBBBBBBBB")),
            "{states:?}"
        );
    }

    #[tokio::test]
    async fn no_session_reports_signed_out() {
        let mut r = rig(Setup {
            failures: vec![("AAAAAAAAAAA", Error::SignedOut)],
            ..Setup::default()
        })
        .await;
        r.play("AAAAAAAAAAA").await;
        let seen = r.until(PlayState::Stopped).await;
        assert!(
            seen.contains(&EngineEvent::Error {
                code: "signed_out",
                message: "signed out".into()
            }),
            "{seen:?}"
        );
        assert_eq!(r.status().await.state, PlayState::Stopped);
        assert!(r.started().is_empty());
    }

    #[tokio::test]
    async fn toggle_pauses_and_resumes() {
        let mut r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        r.send(EngineCmd::Toggle).await;
        let s = states(&r.until(PlayState::Paused).await);
        assert_eq!(s.last().unwrap().video_id.as_deref(), Some("AAAAAAAAAAA"));
        eventually("the output pauses", || r.stats.paused()).await;
        assert_eq!(r.status().await.state, PlayState::Paused);
        r.send(EngineCmd::Toggle).await;
        r.until(PlayState::Playing).await;
        eventually("the output resumes", || !r.stats.paused()).await;
        assert_eq!(r.status().await.state, PlayState::Playing);
    }

    #[tokio::test]
    async fn seek_emits_position() {
        let mut r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        r.send(EngineCmd::Seek(1.0)).await;
        loop {
            if let EngineEvent::Position { seconds, .. } = r.next().await {
                assert_eq!(seconds, 1.0);
                break;
            }
        }
        // The audio thread lands on the seek point (to within a packet).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let at = r.status().await.position;
        assert!((0.95..1.5).contains(&at), "{at}");
    }

    /// Fires the receiver after `after` of real time, unless the returned guard is dropped
    /// first (which also ends its thread). Uses no tokio timer, so a paused clock can't skip
    /// it.
    fn watchdog(after: Duration) -> (std::sync::mpsc::Sender<()>, oneshot::Receiver<()>) {
        let (guard, stop) = std::sync::mpsc::channel::<()>();
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(after) {
                let _ = tx.send(());
            }
        });
        (guard, rx)
    }

    #[tokio::test]
    async fn seek_near_the_end_reports_length_minus_one() {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let Built {
            mut engine, events, ..
        } = engine_for(&server, fake.clone(), Arc::default(), true, false);
        let mut rx = events.subscribe();
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        // A 318 s song, loaded (not yet started, so the status shows the engine's own
        // position rather than the audio thread's on the 2 s fixture).
        let mut stream = fake.stream("AAAAAAAAAAA");
        stream.meta.length_seconds = 318;
        engine.on_resolved(Resolved {
            generation: engine.generation,
            result: Ok(stream),
        });
        while rx.try_recv().is_ok() {}
        // The decoder lands at most 1 s before the end; the report must say the same. (At or
        // past the end, a seek is a `Next`: `seek_past_end_acts_like_next`.)
        engine.handle(EngineCmd::Seek(317.5));
        assert_eq!(
            rx.try_recv().unwrap(),
            EngineEvent::Position {
                seconds: 317.0,
                seeked: true
            }
        );
        assert!((engine.snapshot().position - 317.0).abs() < 1e-9);
        // Below zero still clamps to the start.
        engine.handle(EngineCmd::Seek(-5.0));
        assert_eq!(
            rx.try_recv().unwrap(),
            EngineEvent::Position {
                seconds: 0.0,
                seeked: true
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_ticks_each_second_only_while_playing() {
        // No tokio timeouts in here: tokio would skip a paused clock straight to them while
        // the audio thread (real time) does its work. A watchdog on real time stands in.
        let mut r = rig(Setup::default()).await;
        let (_dog, mut bark) = watchdog(Duration::from_secs(30));
        let mut next = async |r: &mut Rig| {
            tokio::select! {
                e = r.events.recv() => e.unwrap(),
                _ = &mut bark => panic!("no event within 30 s of real time"),
            }
        };
        r.play("AAAAAAAAAAA").await;
        loop {
            if let EngineEvent::State(s) = next(&mut r).await
                && s.state == PlayState::Playing
            {
                break;
            }
        }
        let t0 = Instant::now();
        for n in 1..=3 {
            let e = next(&mut r).await;
            assert!(matches!(e, EngineEvent::Position { .. }), "{e:?}");
            assert_eq!(t0.elapsed(), Duration::from_secs(n), "tick {n}");
        }
        r.send(EngineCmd::Pause).await;
        loop {
            match next(&mut r).await {
                EngineEvent::State(s) if s.state == PlayState::Paused => break,
                EngineEvent::Position { .. } => {}
                e => panic!("{e:?}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(
            matches!(
                r.events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "no events while paused"
        );
        r.send(EngineCmd::Toggle).await;
        loop {
            if let EngineEvent::State(s) = next(&mut r).await
                && s.state == PlayState::Playing
            {
                break;
            }
        }
        let t1 = Instant::now();
        let e = next(&mut r).await;
        assert!(matches!(e, EngineEvent::Position { .. }), "{e:?}");
        assert_eq!(t1.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_new_play_is_not_held_by_a_stalled_open() {
        let mut r = rig(Setup::default()).await;
        r.play(STALL).await;
        let paths = r.server.paths.clone();
        eventually("the stalled request arrives", || {
            paths.lock().unwrap().iter().any(|p| p == "/stall")
        })
        .await;
        // The audio thread is now blocked opening a track whose bytes never come.
        tokio::time::sleep(Duration::from_millis(50)).await;
        r.play("BBBBBBBBBBB").await;
        let seen = r.until(PlayState::Playing).await;
        no_errors(&seen);
        assert_eq!(r.status().await.video_id.as_deref(), Some("BBBBBBBBBBB"));
    }

    #[tokio::test]
    async fn pause_while_buffering_loads_paused() {
        let mut r = rig(Setup {
            delays: vec![("AAAAAAAAAAA", 200)],
            ..Setup::default()
        })
        .await;
        r.play("AAAAAAAAAAA").await;
        r.send(EngineCmd::Pause).await;
        r.until(PlayState::Paused).await;
        // The resolve finishes: the song is loaded, with its details, but stays paused.
        let s = loop {
            if let EngineEvent::State(s) = r.next().await
                && s.meta.is_some()
            {
                break s;
            }
        };
        assert_eq!(s.state, PlayState::Paused);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(r.stats.frames(), 0, "nothing played");
        assert_eq!(r.started(), ["AAAAAAAAAAA"]);
        r.send(EngineCmd::Toggle).await;
        r.until(PlayState::Playing).await;
        eventually("it plays", || r.stats.frames() > 0).await;
    }

    #[tokio::test]
    async fn the_end_stops_but_keeps_the_song_and_play_replays_it() {
        let mut r = rig(Setup {
            fast: true,
            ..Setup::default()
        })
        .await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        let s = states(&r.until(PlayState::Stopped).await);
        let end = s.last().unwrap();
        assert_eq!(end.video_id.as_deref(), Some("AAAAAAAAAAA"));
        assert_eq!(end.meta, Some(meta("AAAAAAAAAAA")));
        assert!(end.position > 1.5, "{}", end.position);
        r.send(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        })
        .await;
        let s = states(&r.until(PlayState::Playing).await);
        assert_eq!(s[0].state, PlayState::Buffering);
        assert_eq!(s[0].video_id.as_deref(), Some("AAAAAAAAAAA"));
        assert_eq!(r.started(), ["AAAAAAAAAAA", "AAAAAAAAAAA"]);
    }

    #[tokio::test]
    async fn a_mixer_volume_change_reaches_the_status() {
        let mut r = rig(Setup::default()).await;
        r.send(EngineCmd::Volume(0.5)).await;
        eventually("the output volume", || r.stats.volume() == 0.5).await;
        // The user turns the stream down in a mixer: the status (and so MPRIS and the
        // widgets) follow, and it is saved like a change of ours.
        r.stats.mixer_volume(0.3);
        loop {
            if let EngineEvent::State(s) = r.next().await
                && s.volume == 0.3
            {
                break;
            }
        }
        assert_eq!(r.status().await.volume, 0.3);
        // Not sent back to the output: the output already has it (no loop).
        assert_eq!(r.stats.volume_sets(), 1);
    }

    #[tokio::test]
    async fn volume_is_clamped_and_reported() {
        let mut r = rig(Setup::default()).await;
        r.send(EngineCmd::Volume(0.5)).await;
        let EngineEvent::State(s) = r.next().await else {
            panic!("wanted a state")
        };
        assert_eq!(s.volume, 0.5);
        eventually("the output volume", || r.stats.volume() == 0.5).await;
        r.send(EngineCmd::Volume(7.0)).await;
        let EngineEvent::State(s) = r.next().await else {
            panic!("wanted a state")
        };
        assert_eq!(s.volume, 1.0);
        assert_eq!(r.status().await.volume, 1.0);
    }

    #[tokio::test]
    async fn quit_ends_the_engine() {
        let r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        r.send(EngineCmd::Quit).await;
        tokio::time::timeout(Duration::from_secs(5), r.task)
            .await
            .expect("the engine stops within 5 s")
            .unwrap();
    }

    #[tokio::test]
    async fn stale_resolve_result_is_dropped() {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let Built {
            mut engine,
            events,
            started,
            ..
        } = engine_for(&server, fake.clone(), Arc::default(), true, false);
        let mut rx = events.subscribe();
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        engine.handle(EngineCmd::Play {
            video_id: Some("BBBBBBBBBBB".into()),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        while rx.try_recv().is_ok() {}
        // A's answer, arriving late, after B was asked for.
        engine.on_resolved(Resolved {
            generation: engine.generation - 1,
            result: Ok(fake.stream("AAAAAAAAAAA")),
        });
        assert!(started.lock().unwrap().is_empty());
        assert_eq!(engine.status.video_id.as_deref(), Some("BBBBBBBBBBB"));
        assert_eq!(engine.status.state, PlayState::Buffering);
        assert!(rx.try_recv().is_err(), "nothing reported");
    }

    #[tokio::test]
    async fn an_earlier_tracks_events_are_dropped() {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let Built { mut engine, .. } =
            engine_for(&server, fake.clone(), Arc::default(), true, false);
        // A was loaded and started; B is picked and loaded.
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        engine.on_resolved(Resolved {
            generation: engine.generation,
            result: Ok(fake.stream("AAAAAAAAAAA")),
        });
        engine.on_audio(AudioEvent::Loading);
        engine.on_audio(AudioEvent::Started);
        assert_eq!(engine.status.state, PlayState::Playing);
        engine.handle(EngineCmd::Play {
            video_id: Some("BBBBBBBBBBB".into()),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        // A's end, sent before the audio thread took B's load: B is still resolving.
        engine.on_audio(AudioEvent::Ended);
        assert_eq!(engine.status.state, PlayState::Buffering);
        engine.on_resolved(Resolved {
            generation: engine.generation,
            result: Ok(fake.stream("BBBBBBBBBBB")),
        });
        // A's error, sent before the audio thread took B's load: B is loaded now.
        engine.on_audio(AudioEvent::Error(Error::StreamFailed("x".into())));
        assert_eq!(engine.status.state, PlayState::Buffering);
        engine.on_audio(AudioEvent::Loading);
        engine.on_audio(AudioEvent::Started);
        assert_eq!(engine.status.state, PlayState::Playing);
        assert_eq!(engine.status.video_id.as_deref(), Some("BBBBBBBBBBB"));
        // B's radio answer ("no queue") arrives: the queue has nothing after B.
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(p) = engine.pages_rx.try_recv() {
            engine.on_page(p);
        }
        // B's own end counts.
        engine.on_audio(AudioEvent::Ended);
        assert_eq!(engine.status.state, PlayState::Stopped);
    }

    #[tokio::test]
    async fn play_playlist_fills_queue_and_plays_index() {
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", Some(1)).await;
        let seen = r.until_song(&vid('B'), PlayState::Playing).await;
        no_errors(&seen);
        // A playlist is asked for by its id alone (with a video id, YouTube sends one song).
        assert_eq!(
            r.source.requests()[0],
            NextRequest {
                playlist_id: Some("PLlist".into()),
                ..NextRequest::default()
            }
        );
        let q = r.queue().await;
        let ids: Vec<_> = q.items.iter().map(|i| i.song.video_id.clone()).collect();
        assert_eq!(ids, [vid('A'), vid('B'), vid('C')]);
        assert_eq!(q.current_id, Some(id_of(&q, 'B')));
        let status = r.status().await;
        assert_eq!(status.queue_id, Some(id_of(&q, 'B')));
        assert_eq!(status.album.as_deref(), Some("Album"));
        // B plays first; C (the 2 s song's next) may be preloaded already.
        assert_eq!(r.started()[0], vid('B'));
        assert!(!r.started().contains(&vid('A')));
    }

    #[tokio::test]
    async fn ended_plays_next() {
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "AB", None)],
            fast: true,
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        r.until_song(&vid('B'), PlayState::Playing).await;
        // B is the last song and the radio has nothing more: the end stops, keeping B.
        let seen = r.until(PlayState::Stopped).await;
        no_errors(&seen);
        assert_eq!(r.status().await.video_id, Some(vid('B')));
        assert_eq!(r.started(), [vid('A'), vid('B')]);
    }

    #[tokio::test]
    async fn end_of_queue_waits_for_radio() {
        // Review Focus 2: the last song ends while the radio request is still out.
        let mut r = rig(Setup {
            pages: vec![
                ok("PLone", 0, "A", None),
                // A radio starts with its seed song, already in the queue.
                ok(&radio_of('A'), 600, "ABC", None),
            ],
            fast: true,
            ..Setup::default()
        })
        .await;
        r.play_list("PLone", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        let seen = r.until_song(&vid('B'), PlayState::Playing).await;
        no_errors(&seen);
        // It waited (buffering) rather than stopping, and nothing played twice (C may be
        // preloaded behind B already).
        assert!(
            !states(&seen).iter().any(|s| s.state == PlayState::Stopped),
            "{seen:?}"
        );
        let started = r.started();
        assert_eq!(started[..2], [vid('A'), vid('B')]);
        assert!(started[2..].iter().all(|id| *id == vid('C')) && started.len() <= 3);
        let radio: Vec<_> = r
            .source
            .requests()
            .into_iter()
            .filter(|q| q.playlist_id.as_deref() == Some(radio_of('A').as_str()))
            .collect();
        assert_eq!(radio.len(), 1, "one radio request");
        let q = r.queue().await;
        assert_eq!(q.items.len(), 3, "the seed isn't queued twice");
    }

    #[tokio::test]
    async fn needs_more_fetches_radio_once() {
        let mut r = rig(Setup {
            pages: vec![
                ok("PLlist", 0, "ABC", None),
                ok(&radio_of('C'), 300, "CD", Some("CONT1")),
            ],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        // Changes while the radio request is out ask for nothing more.
        r.send(EngineCmd::Repeat(Repeat::Off)).await;
        r.send(EngineCmd::Shuffle(false)).await;
        r.send(EngineCmd::QueueAdd {
            added: oneshot::channel().0,
            songs: vec![],
            at: AddAt::End,
        })
        .await;
        let q = loop {
            let q = r.until_queue().await;
            if q.items.len() == 4 {
                break q;
            }
        };
        assert_eq!(q.items[3].song.video_id, vid('D'));
        // Two after B: the queue needs more, and continues the radio it now plays from.
        r.send(EngineCmd::Next).await;
        r.until_song(&vid('B'), PlayState::Playing).await;
        // That continuation has no queue: no more songs. A, B, C, D: nothing asked at C.
        r.send(EngineCmd::Next).await;
        r.until_song(&vid('C'), PlayState::Playing).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let asked = r.source.requests();
        assert_eq!(
            asked,
            [
                NextRequest {
                    playlist_id: Some("PLlist".into()),
                    ..NextRequest::default()
                },
                // The radio of the queue's last song.
                NextRequest {
                    video_id: Some(vid('C')),
                    playlist_id: Some(radio_of('C')),
                    ..NextRequest::default()
                },
                NextRequest {
                    continuation: Some("CONT1".into()),
                    ..NextRequest::default()
                },
            ]
        );
    }

    #[tokio::test]
    async fn unavailable_song_is_skipped() {
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            failures: vec![("BBBBBBBBBBB", Error::Unavailable("not here".into()))],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", Some(1)).await;
        let seen = r.until_song(&vid('C'), PlayState::Playing).await;
        assert_eq!(errors(&seen), ["unavailable"]);
        assert_eq!(r.started(), [vid('C')]);
        assert_eq!(r.queue().await.current_id, Some(3));
    }

    #[tokio::test]
    async fn a_private_or_age_checked_song_is_skipped_not_signed_out() {
        // What the TV client's LOGIN_REQUIRED for a private video or an age check becomes
        // (`innertube::player`): that song's failure, so the queue goes on.
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            failures: vec![
                (
                    "AAAAAAAAAAA",
                    Error::StreamFailed("This is a private video.".into()),
                ),
                (
                    "BBBBBBBBBBB",
                    Error::StreamFailed("Sign in to confirm your age".into()),
                ),
            ],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        let seen = r.until_song(&vid('C'), PlayState::Playing).await;
        assert_eq!(errors(&seen), ["stream_failed", "stream_failed"]);
        assert_eq!(r.started(), [vid('C')]);
    }

    #[tokio::test]
    async fn all_unplayable_stops_after_one_pass() {
        // Review Focus 3: with repeat on, a queue of nothing but unplayable songs must not
        // skip round for ever.
        let gone = || Error::Unavailable("not here".into());
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            failures: vec![
                ("AAAAAAAAAAA", gone()),
                ("BBBBBBBBBBB", gone()),
                ("CCCCCCCCCCC", gone()),
            ],
            ..Setup::default()
        })
        .await;
        r.send(EngineCmd::Repeat(Repeat::All)).await;
        r.play_list("PLlist", Some(1)).await;
        // B, C, then A (repeat all wraps), the last one tried.
        let seen = r.until_song(&vid('A'), PlayState::Stopped).await;
        assert_eq!(errors(&seen), ["unavailable"; 3]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(r.calls(), [vid('B'), vid('C'), vid('A')], "each tried once");
        assert_eq!(r.status().await.state, PlayState::Stopped);
        assert!(r.started().is_empty());
    }

    #[tokio::test]
    async fn an_endless_radio_of_unplayable_songs_stops_after_one_pass() {
        // A radio that keeps sending new songs, none of them playable: the queue grows with
        // every refill, so "a pass" is the queue's length when the skipping began (4: A and
        // its radio's first page), not its length now.
        let gone = || Error::Unavailable("not here".into());
        let letters = "ABCDEFGHIJKLMNOPQRS";
        let mut r = rig(Setup {
            pages: vec![
                (radio_of('A'), 0, Ok(page("BCD", Some("CONT1")))),
                ok("CONT1", 0, "EFG", Some("CONT2")),
                ok("CONT2", 0, "HIJ", Some("CONT3")),
                ok("CONT3", 0, "KLM", Some("CONT4")),
                ok("CONT4", 0, "NOP", Some("CONT5")),
                ok("CONT5", 0, "QRS", None),
            ],
            failures: letters
                .chars()
                .map(|c| (&*Box::leak(vid(c).into_boxed_str()), gone()))
                .collect(),
            // Each failure takes a moment, so every refill lands before the next skip: the
            // queue grows the way a real radio's would.
            delays: letters
                .chars()
                .map(|c| (&*Box::leak(vid(c).into_boxed_str()), 30))
                .collect(),
            ..Setup::default()
        })
        .await;
        r.play(&vid('A')).await;
        r.until_song(&vid('D'), PlayState::Stopped).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(r.calls(), [vid('A'), vid('B'), vid('C'), vid('D')]);
        assert_eq!(r.status().await.state, PlayState::Stopped);
    }

    /// An engine (not run) playing A, from a queue of `songs`, with A loaded and started.
    async fn playing(songs: Vec<SongItem>) -> (Built, Arc<Fake>, Server) {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let mut built = engine_for(&server, fake.clone(), Arc::default(), true, false);
        let engine = &mut built.engine;
        engine.queue.replace(songs, 0);
        engine.start_current(0.0);
        engine.on_resolved(Resolved {
            generation: engine.generation,
            result: Ok(fake.stream(&vid('A'))),
        });
        engine.on_audio(AudioEvent::Loading);
        engine.on_audio(AudioEvent::Started);
        (built, fake, server)
    }

    /// Hands the engine its preload resolves as they finish (its `run` loop would).
    async fn take_preloads(engine: &mut Engine) {
        let t = std::time::Instant::now();
        while engine.preload.as_ref().is_some_and(Preload::resolving) {
            assert!(
                t.elapsed() < Duration::from_secs(3),
                "the preload resolve hung"
            );
            match tokio::time::timeout(Duration::from_millis(50), engine.preloads_rx.recv()).await {
                Ok(Some(p)) => engine.on_preloaded(p),
                _ => continue,
            }
        }
    }

    fn long(c: char) -> SongItem {
        SongItem {
            length_seconds: 100,
            ..song(c)
        }
    }

    #[tokio::test]
    async fn next_link_prefetched_at_half() {
        let (built, fake, _server) = playing(vec![long('A'), long('B')]).await;
        let mut engine = built.engine;
        let has_b = |fake: &Fake| fake.calls.lock().unwrap().contains(&vid('B'));
        engine.maybe_prefetch(49.0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!has_b(&fake), "nothing before half");
        engine.maybe_prefetch(50.0);
        let calls = fake.clone();
        eventually("B's link is fetched", || has_b(&calls)).await;
        // Only its link: B isn't downloaded at half.
        assert_eq!(*built.started.lock().unwrap(), [vid('A')]);
        assert_eq!(engine.status.video_id, Some(vid('A')));
    }

    #[tokio::test]
    async fn preload_starts_10_s_before_the_end() {
        let (built, _fake, _server) = playing(vec![long('A'), long('B')]).await;
        let mut engine = built.engine;
        engine.maybe_preload(89.0);
        assert!(engine.preload.is_none(), "nothing 11 s before the end");
        engine.maybe_preload(90.0);
        take_preloads(&mut engine).await;
        // B's download started and was handed to the audio thread; A is still current.
        assert_eq!(*built.started.lock().unwrap(), [vid('A'), vid('B')]);
        assert!(
            engine
                .preload
                .as_ref()
                .is_some_and(|p| p.ticket().is_some())
        );
        assert_eq!(engine.status.video_id, Some(vid('A')));
        // Once per next item.
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        assert_eq!(*built.started.lock().unwrap(), [vid('A'), vid('B')]);
    }

    #[tokio::test]
    async fn advanced_makes_the_next_current() {
        let (built, _fake, _server) = playing(vec![long('A'), long('B')]).await;
        let mut engine = built.engine;
        let mut rx = built.events.subscribe();
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        let ticket = engine.preload.as_ref().and_then(Preload::ticket).unwrap();
        let b = engine.queue.items()[1].id;
        engine.on_audio(AudioEvent::Advanced(ticket));
        // B is current and playing, with no load (and no Buffering) of its own.
        assert_eq!(engine.status.video_id, Some(vid('B')));
        assert_eq!(engine.status.queue_id, Some(b));
        assert_eq!(engine.status.state, PlayState::Playing);
        assert_eq!(engine.queue.current().map(|i| i.id), Some(b));
        assert!(engine.loaded && engine.started);
        assert!(engine.preload.is_none());
        assert_eq!(*built.started.lock().unwrap(), [vid('A'), vid('B')]);
        let mut saw_queue = false;
        while let Ok(e) = rx.try_recv() {
            match e {
                EngineEvent::State(s) => {
                    assert_ne!(s.state, PlayState::Buffering, "{s:?}");
                }
                EngineEvent::Queue { current_id, .. } => saw_queue = current_id == Some(b),
                _ => {}
            }
        }
        assert!(saw_queue, "a queue event with B current");
        assert_eq!(engine.status.meta.as_ref().unwrap().title, "Title B");
    }

    #[tokio::test]
    async fn a_stale_advance_plays_the_real_next() {
        // The preload was replaced (the queue changed) just as the audio thread moved on to
        // it: the engine plays the queue's real next item instead.
        let (built, _fake, _server) = playing(vec![long('A'), long('B'), long('C')]).await;
        let mut engine = built.engine;
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        let stale = engine.preload.as_ref().and_then(Preload::ticket).unwrap();
        let c = engine.queue.items()[2].id;
        engine.handle(EngineCmd::QueueMove { id: c, index: 1 });
        assert!(
            engine
                .preload
                .as_ref()
                .is_none_or(|p| p.queue_id != engine.queue.items()[2].id),
            "B's preload is gone"
        );
        engine.on_audio(AudioEvent::Advanced(stale));
        assert_eq!(engine.status.video_id, Some(vid('C')));
        assert_eq!(engine.queue.current().map(|i| i.id), Some(c));
    }

    #[tokio::test]
    async fn queue_changes_replace_the_preload() {
        let (built, _fake, _server) = playing(vec![long('A'), long('B'), long('C')]).await;
        let mut engine = built.engine;
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        let [_, b, c] = [0, 1, 2].map(|i| engine.queue.items()[i].id);
        assert_eq!(engine.preload.as_ref().map(|p| p.queue_id), Some(b));
        // Seek and pause keep it.
        engine.handle(EngineCmd::Pause);
        engine.handle(EngineCmd::Seek(50.0));
        assert_eq!(engine.preload.as_ref().map(|p| p.queue_id), Some(b));
        engine.handle(EngineCmd::Toggle);
        // Repeat one: the next is A itself.
        engine.handle(EngineCmd::Repeat(Repeat::One));
        assert!(engine.preload.is_none(), "dropped when the next changed");
        engine.handle(EngineCmd::Repeat(Repeat::Off));
        // Removing the next item.
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        assert_eq!(engine.preload.as_ref().map(|p| p.queue_id), Some(b));
        engine.handle(EngineCmd::QueueRemove(b));
        assert!(engine.preload.as_ref().is_none_or(|p| p.queue_id == c));
        // A new play drops it too.
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        assert_eq!(engine.preload.as_ref().map(|p| p.queue_id), Some(c));
        engine.handle(EngineCmd::Play {
            video_id: Some(vid('D')),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        assert!(engine.preload.is_none());
    }

    #[tokio::test]
    async fn a_next_in_the_handover_window_ends_the_old_report_at_its_end() {
        // The audio thread has moved on to the preload (B), but the engine hasn't taken the
        // `Advanced` yet when a Next comes: the player's position is already B's. A's report
        // must end where A really stopped (its end), not at B's few tenths of a second.
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        // In real time, so B's position grows at a known pace.
        let built = engine_for(&server, fake.clone(), Arc::default(), false, false);
        let mut engine = built.engine;
        let reports = Arc::new(FakeReports::default());
        engine.report_with(Reporter::new(reports.clone()));
        let audio = engine.player.events();
        engine
            .queue
            .replace(vec![song('A'), song('B'), song('C')], 0);
        engine.start_current(0.0);
        engine.on_resolved(Resolved {
            generation: engine.generation,
            result: Ok(fake.stream(&vid('A'))),
        });
        engine.on_audio(AudioEvent::Loading);
        engine.on_audio(AudioEvent::Started);
        // A 2 s song: under 10 s left, so B is preloaded at once.
        engine.maybe_preload(0.0);
        take_preloads(&mut engine).await;
        let ticket = engine.preload.as_ref().and_then(Preload::ticket).unwrap();
        // Wait (off the engine) for the audio thread's handover to B.
        let waited = tokio::task::spawn_blocking(move || {
            loop {
                match audio.recv_timeout(Duration::from_secs(5)) {
                    Ok(AudioEvent::Advanced(t)) if t == ticket => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap();
        assert!(waited, "the audio thread moved on to B");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(engine.player.position() < 1.5, "the position is B's now");
        engine.handle(EngineCmd::Next);
        eventually("A's last ping", || reports.finals() == 1).await;
        let ranges = reports.watch_ranges();
        let end = ranges.iter().map(|r| r.1).fold(0.0, f64::max);
        assert!(end > 1.9, "A's report ended at {end}: {ranges:?}");
    }

    #[tokio::test]
    async fn skip_uses_the_preloaded_track() {
        let (built, fake, _server) = playing(vec![long('A'), long('B')]).await;
        let mut engine = built.engine;
        engine.maybe_preload(95.0);
        take_preloads(&mut engine).await;
        let calls = fake.calls.lock().unwrap().len();
        engine.handle(EngineCmd::Next);
        // B loads at once from its preloaded download: no new resolve, no second download.
        assert_eq!(engine.status.video_id, Some(vid('B')));
        assert!(engine.loaded);
        assert_eq!(fake.calls.lock().unwrap().len(), calls);
        assert_eq!(*built.started.lock().unwrap(), [vid('A'), vid('B')]);
    }

    #[tokio::test]
    async fn ended_hands_over_to_the_preloaded_next() {
        // The whole path, in real time: B follows A with no load of its own.
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "AB", None)],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        let seen = r.until_song(&vid('B'), PlayState::Playing).await;
        no_errors(&seen);
        assert!(
            !states(&seen)
                .iter()
                .any(|s| s.video_id == Some(vid('B')) && s.state != PlayState::Playing),
            "B went straight to playing: {seen:?}"
        );
        assert_eq!(r.started(), [vid('A'), vid('B')]);
        let seen = r.until(PlayState::Stopped).await;
        no_errors(&seen);
    }

    #[tokio::test]
    async fn seek_past_end_acts_like_next() {
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "AB", None)],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        r.send(EngineCmd::Seek(5.0)).await;
        let seen = r.until_song(&vid('B'), PlayState::Playing).await;
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, EngineEvent::Position { seeked: true, .. })),
            "no seek is reported: {seen:?}"
        );
        assert_eq!(r.started(), [vid('A'), vid('B')]);
    }

    #[tokio::test]
    async fn meta_comes_from_queue_item_not_oembed() {
        // The queue item has no length: that one gap is filled from the resolver.
        let mut r = rig(Setup {
            pages: vec![(
                "PLlist".into(),
                0,
                Ok(NextPage {
                    items: vec![SongItem {
                        length_seconds: 0,
                        ..song('A')
                    }],
                    ..NextPage::default()
                }),
            )],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        let seen = r.until_song(&vid('A'), PlayState::Playing).await;
        let from_queue = TrackMeta {
            title: "Title A".into(),
            artist: "One, Two".into(),
            length_seconds: 0,
            thumbnail: Some("https://i.ytimg.com/A.jpg".into()),
        };
        // Known at once, before the link is resolved.
        let first = states(&seen)
            .into_iter()
            .find(|s| s.video_id == Some(vid('A')))
            .unwrap();
        assert_eq!(first.meta, Some(from_queue.clone()));
        let status = r.status().await;
        assert_eq!(
            status.meta,
            Some(TrackMeta {
                length_seconds: 2,
                ..from_queue
            })
        );
        assert_eq!(status.album.as_deref(), Some("Album"));

        // A raw song id with no queue details: the resolver's details.
        r.play("XXXXXXXXXXX").await;
        r.until_song("XXXXXXXXXXX", PlayState::Playing).await;
        assert_eq!(r.status().await.meta, Some(meta("XXXXXXXXXXX")));
    }

    #[tokio::test]
    async fn play_without_anything_starts_liked_songs() {
        let mut r = rig(Setup {
            pages: vec![ok("LM", 0, "AB", None)],
            ..Setup::default()
        })
        .await;
        r.send(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        })
        .await;
        let seen = r.until_song(&vid('A'), PlayState::Playing).await;
        no_errors(&seen);
        assert_eq!(
            r.source.requests()[0],
            NextRequest {
                playlist_id: Some("LM".into()),
                ..NextRequest::default()
            }
        );
    }

    #[tokio::test]
    async fn play_with_songs_queued_but_none_current_plays_them_not_liked_songs() {
        let mut r = rig(Setup {
            pages: vec![ok("LM", 0, "XY", None)],
            ..Setup::default()
        })
        .await;
        // Songs added to an empty queue: none is current yet (ruling S8).
        let (added, ok_rx) = oneshot::channel();
        r.send(EngineCmd::QueueAdd {
            songs: vec![song('B'), song('C')],
            at: AddAt::End,
            added,
        })
        .await;
        assert!(ok_rx.await.unwrap());
        let before = r.queue().await;
        assert_eq!(before.current_id, None);
        r.send(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        })
        .await;
        let seen = r.until_song(&vid('B'), PlayState::Playing).await;
        no_errors(&seen);
        let q = r.queue().await;
        let ids: Vec<_> = q.items.iter().map(|i| i.song.video_id.clone()).collect();
        assert_eq!(ids, [vid('B'), vid('C')], "the queue is kept");
        assert_eq!(q.current_id, Some(id_of(&before, 'B')));
        assert!(
            !r.source
                .requests()
                .iter()
                .any(|q| q.playlist_id.as_deref() == Some(LIKED_SONGS)),
            "Liked songs are not asked for"
        );
    }

    #[tokio::test]
    async fn toggle_with_songs_queued_but_none_current_plays_them() {
        let mut r = rig(Setup {
            pages: vec![ok("LM", 0, "XY", None)],
            ..Setup::default()
        })
        .await;
        r.send(EngineCmd::QueueAdd {
            songs: vec![song('B'), song('C')],
            at: AddAt::End,
            added: oneshot::channel().0,
        })
        .await;
        // MPRIS Play and PlayPause from Stopped come here too.
        r.send(EngineCmd::Toggle).await;
        r.until_song(&vid('B'), PlayState::Playing).await;
        // (The radio of the queue's last song may be asked for now that B plays.)
        assert!(
            !r.source
                .requests()
                .iter()
                .any(|q| q.playlist_id.as_deref() == Some(LIKED_SONGS)),
            "{:?}",
            r.source.requests()
        );
    }

    #[tokio::test]
    async fn adding_to_a_queue_with_nothing_current_fetches_no_radio() {
        let r = rig(Setup {
            pages: vec![ok(&radio_of('B'), 0, "BXY", None)],
            ..Setup::default()
        })
        .await;
        let (added, ok_rx) = oneshot::channel();
        r.send(EngineCmd::QueueAdd {
            songs: vec![song('B')],
            at: AddAt::End,
            added,
        })
        .await;
        assert!(ok_rx.await.unwrap());
        // Let a request (if one went out) land.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            r.source.requests().is_empty(),
            "nothing plays yet, so no radio: {:?}",
            r.source.requests()
        );
        assert_eq!(r.queue().await.items.len(), 1);
    }

    /// An engine (not run) whose queue requests never answer by themselves: the tests hand it
    /// the list with `on_load`, when they choose.
    async fn idle_engine() -> (Built, Server) {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let built = engine_for(&server, fake, Arc::default(), true, false);
        (built, server)
    }

    fn add(engine: &mut Engine, songs: &str, at: AddAt) {
        engine.handle(EngineCmd::QueueAdd {
            songs: songs.chars().map(song).collect(),
            at,
            added: oneshot::channel().0,
        });
    }

    fn queue_ids(engine: &Engine) -> Vec<(String, u64)> {
        engine
            .queue
            .items()
            .iter()
            .map(|i| (i.song.video_id.clone(), i.id))
            .collect()
    }

    fn order(engine: &Engine) -> Vec<String> {
        engine
            .queue
            .items()
            .iter()
            .map(|i| i.song.video_id.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_song_added_and_playing_before_the_radio_lands_stays_current() {
        // The review's repro: A alone, its radio slow; A ends; the user adds B, which plays;
        // then the radio lands. B must stay current under the same queue id, and no song is
        // lost.
        let (mut built, _server) = idle_engine().await;
        let engine = &mut built.engine;
        engine.handle(EngineCmd::Play {
            video_id: Some(vid('A')),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        // A ends while its radio is still on the way: the engine waits.
        engine.advance(true, true);
        assert!(engine.waiting);
        add(engine, "B", AddAt::End);
        assert_eq!(engine.status.video_id, Some(vid('B')));
        let b = engine.queue.current().unwrap().id;
        assert_eq!(engine.status.queue_id, Some(b));

        engine.on_load(Ok(page("ACD", None)));
        // A (heard), B (playing), then the radio's songs to come.
        assert_eq!(order(engine), [vid('A'), vid('B'), vid('C'), vid('D')]);
        assert_eq!(engine.queue.current().map(|i| i.id), Some(b));
        assert_eq!(engine.status.queue_id, Some(b));
        assert_eq!(engine.status.video_id, Some(vid('B')));
        assert_eq!(
            engine
                .queue
                .peek_next(false)
                .map(|i| i.song.video_id.clone()),
            Some(vid('C'))
        );
    }

    #[tokio::test]
    async fn songs_added_while_the_list_loads_are_added_again_after_it() {
        let (mut built, _server) = idle_engine().await;
        let engine = &mut built.engine;
        engine.handle(EngineCmd::Play {
            video_id: Some(vid('A')),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        add(engine, "B", AddAt::End);
        add(engine, "X", AddAt::Next);
        add(engine, "Y", AddAt::Next);
        let before: HashMap<String, u64> = queue_ids(engine).into_iter().collect();
        let a_before = before[&vid('A')];
        engine.on_load(Ok(page("ACD", None)));
        // As if added after it landed: next ones right after A, the end one at the end, each
        // with the queue id it already had.
        assert_eq!(
            order(engine),
            [vid('A'), vid('Y'), vid('X'), vid('C'), vid('D'), vid('B')]
        );
        for c in ['B', 'X', 'Y'] {
            let id = queue_ids(engine)
                .into_iter()
                .find(|(v, _)| *v == vid(c))
                .unwrap()
                .1;
            assert_eq!(id, before[&vid(c)], "{c} keeps its id");
        }
        // A (the seed) is current, as the list's own item.
        let a = engine.queue.current().unwrap();
        assert_eq!(a.song.video_id, vid('A'));
        assert_ne!(a.id, a_before);
        assert_eq!(engine.status.queue_id, Some(a.id));
    }

    #[tokio::test]
    async fn a_song_played_before_a_list_with_no_seed_lands_stays_current() {
        // A playlist without a song: while it loads, the user adds X and skips to it.
        let (mut built, _server) = idle_engine().await;
        let engine = &mut built.engine;
        engine.handle(EngineCmd::Play {
            video_id: None,
            playlist_id: Some("PLlist".into()),
            index: Some(1),
            start_seconds: 0.0,
        });
        add(engine, "X", AddAt::End);
        add(engine, "Z", AddAt::End);
        engine.handle(EngineCmd::Next);
        assert_eq!(engine.status.video_id, Some(vid('X')));
        let x = engine.queue.current().unwrap().id;
        engine.on_load(Ok(page("ABC", None)));
        // X stays current; the list's start song (B) comes next; Z, added to the end, last.
        assert_eq!(engine.queue.current().map(|i| i.id), Some(x));
        assert_eq!(engine.status.queue_id, Some(x));
        assert_eq!(engine.status.video_id, Some(vid('X')));
        assert_eq!(
            order(engine),
            [vid('A'), vid('X'), vid('B'), vid('C'), vid('Z')]
        );
    }

    #[tokio::test]
    async fn a_shuffled_list_play_with_no_song_picked_starts_at_random() {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let mut built = engine_for(&server, fake, Arc::default(), true, false);
        let engine = &mut built.engine;
        engine.queue = Queue::with_seed(3);
        engine.handle(EngineCmd::Shuffle(true));
        let mut play = |index: Option<usize>| {
            engine.handle(EngineCmd::Play {
                video_id: None,
                playlist_id: Some("PLlist".into()),
                index,
                start_seconds: 0.0,
            });
            engine.on_load(Ok(page("ABCDEFGH", None)));
            engine.queue.current().unwrap().song.video_id.clone()
        };
        let mut starts = std::collections::HashSet::new();
        for _ in 0..30 {
            starts.insert(play(None));
        }
        assert!(starts.len() >= 4, "random starts: {starts:?}");
        // A song the play picks still starts there.
        for _ in 0..5 {
            assert_eq!(play(Some(2)), vid('C'));
        }
    }

    #[tokio::test]
    async fn queue_event_on_every_change() {
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        let q = loop {
            let q = r.until_queue().await;
            if q.items.len() == 3 {
                break q;
            }
        };
        assert_eq!(q.current_id, Some(id_of(&q, 'A')));
        r.until_song(&vid('A'), PlayState::Playing).await;

        r.send(EngineCmd::QueueAdd {
            added: oneshot::channel().0,
            songs: vec![song('D')],
            at: AddAt::End,
        })
        .await;
        let q = r.until_queue().await;
        assert_eq!(q.items.len(), 4);
        let d = id_of(&q, 'D');
        r.send(EngineCmd::QueueMove { id: d, index: 1 }).await;
        assert_eq!(r.until_queue().await.items[1].id, d);
        r.send(EngineCmd::QueueRemove(d)).await;
        assert_eq!(r.until_queue().await.items.len(), 3);
        r.send(EngineCmd::Shuffle(true)).await;
        assert!(r.until_queue().await.shuffle);
        r.send(EngineCmd::Repeat(Repeat::All)).await;
        assert_eq!(r.until_queue().await.repeat, Repeat::All);
        let c = id_of(&q, 'C');
        r.send(EngineCmd::QueueJump(c)).await;
        assert_eq!(r.until_queue().await.current_id, Some(c));
        r.send(EngineCmd::Next).await;
        assert_ne!(r.until_queue().await.current_id, Some(c));
        r.send(EngineCmd::Previous).await;
        assert_eq!(r.until_queue().await.current_id, Some(c));
        let status = r.status().await;
        assert!(status.shuffle);
        assert_eq!(status.repeat, Repeat::All);
    }

    #[tokio::test]
    async fn seek_sets_seeked_flag() {
        let mut r = rig(Setup::default()).await;
        r.play("AAAAAAAAAAA").await;
        r.until(PlayState::Playing).await;
        // A tick is not a seek.
        loop {
            if let EngineEvent::Position { seeked, .. } = r.next().await {
                assert!(!seeked);
                break;
            }
        }
        r.send(EngineCmd::Seek(0.5)).await;
        loop {
            if let EngineEvent::Position { seconds, seeked } = r.next().await {
                assert_eq!(seconds, 0.5);
                assert!(seeked);
                break;
            }
        }
    }

    #[tokio::test]
    async fn position_kept_after_mid_song_error() {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let Built { mut engine, .. } =
            engine_for(&server, fake.clone(), Arc::default(), false, false);
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        engine.on_resolved(Resolved {
            generation: engine.generation,
            result: Ok(fake.stream("AAAAAAAAAAA")),
        });
        engine.on_audio(AudioEvent::Loading);
        engine.on_audio(AudioEvent::Started);
        // The song plays in real time for a while, then the download fails for good.
        tokio::time::sleep(Duration::from_millis(600)).await;
        engine.on_audio(AudioEvent::Error(Error::StreamFailed("x".into())));
        let at = engine.snapshot().position;
        assert_eq!(engine.status.state, PlayState::Stopped);
        assert!(at > 0.3, "the position is kept: {at}");
        // A play goes on from there, not from the start.
        engine.handle(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        });
        assert_eq!(engine.status.state, PlayState::Buffering);
        assert_eq!(engine.status.video_id.as_deref(), Some("AAAAAAAAAAA"));
        assert!(
            (engine.status.position - at).abs() < 1e-9,
            "{}",
            engine.status.position
        );
        assert!((engine.start_seconds - at).abs() < 1e-9);
    }

    /// A writer that keeps every snapshot it is asked to save, in memory.
    fn recorder() -> (Writer, Arc<Mutex<Vec<Saved>>>) {
        let saves = Arc::new(Mutex::new(Vec::new()));
        let log = saves.clone();
        let writer = Writer::with(Box::new(move |s| {
            log.lock().unwrap().push(s.clone());
            Ok(())
        }));
        (writer, saves)
    }

    /// Waits (real time, blocking: the writer is a thread of its own) until `saves` holds
    /// `n` snapshots, then a little longer to be sure no more come.
    fn saves_settle(saves: &Mutex<Vec<Saved>>, n: usize) -> usize {
        let t = std::time::Instant::now();
        while saves.lock().unwrap().len() < n && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(30));
        saves.lock().unwrap().len()
    }

    /// The next event, failing after 30 s of real time; for paused-clock tests.
    async fn next_real(r: &mut Rig, bark: &mut oneshot::Receiver<()>) -> EngineEvent {
        tokio::select! {
            e = r.events.recv() => e.unwrap(),
            _ = bark => panic!("no event within 30 s of real time"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn writes_every_30_s_only_while_playing() {
        let (writer, saves) = recorder();
        let mut r = rig(Setup {
            writer: Some(writer),
            ..Setup::default()
        })
        .await;
        let (_dog, mut bark) = watchdog(Duration::from_secs(30));
        r.play("AAAAAAAAAAA").await;
        loop {
            if let EngineEvent::State(s) = next_real(&mut r, &mut bark).await
                && s.state == PlayState::Playing
            {
                break;
            }
        }
        // The play itself (a new queue) was saved.
        let base = saves_settle(&saves, 1);
        assert!(base >= 1, "a play's new queue is saved");
        // 29 seconds of play: no save. (The 2 s fixture plays in real time; these ticks are
        // on the paused clock, so they take no real time.)
        let mut ticks = 0;
        while ticks < 29 {
            if let EngineEvent::Position { .. } = next_real(&mut r, &mut bark).await {
                ticks += 1;
            }
        }
        assert_eq!(saves_settle(&saves, base), base, "nothing before 30 s");
        while ticks < 30 {
            if let EngineEvent::Position { .. } = next_real(&mut r, &mut bark).await {
                ticks += 1;
            }
        }
        assert_eq!(saves_settle(&saves, base + 1), base + 1, "one at 30 s");
        // Pause saves once; then no timer and no more saves, however long it stays paused.
        r.send(EngineCmd::Pause).await;
        loop {
            if let EngineEvent::State(s) = next_real(&mut r, &mut bark).await
                && s.state == PlayState::Paused
            {
                break;
            }
        }
        assert_eq!(saves_settle(&saves, base + 2), base + 2, "pause saves");
        let paused_at = saves.lock().unwrap().last().unwrap().position;
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(
            saves_settle(&saves, base + 2),
            base + 2,
            "none while paused"
        );
        assert_eq!(saves.lock().unwrap().last().unwrap().position, paused_at);
        // A seek, a volume change and a queue change are each saved. (Each followed by a
        // status round trip: `saves_settle` blocks this thread, which the engine shares.)
        r.send(EngineCmd::Seek(0.5)).await;
        r.status().await;
        assert_eq!(saves_settle(&saves, base + 3), base + 3, "seek saves");
        assert_eq!(saves.lock().unwrap().last().unwrap().position, 0.5);
        r.send(EngineCmd::Volume(0.25)).await;
        r.status().await;
        assert_eq!(saves_settle(&saves, base + 4), base + 4, "volume saves");
        r.send(EngineCmd::QueueAdd {
            added: oneshot::channel().0,
            songs: vec![song('B')],
            at: AddAt::End,
        })
        .await;
        r.status().await;
        assert_eq!(
            saves_settle(&saves, base + 5),
            base + 5,
            "queue change saves"
        );
        let last = saves.lock().unwrap().last().unwrap().clone();
        assert_eq!(last.volume, 0.25);
        assert_eq!(last.queue.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_resumes_paused_near_position() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            writer: Some(Writer::spawn(dir.path().to_path_buf())),
            ..Setup::default()
        })
        .await;
        let (_dog, mut bark) = watchdog(Duration::from_secs(30));
        a.play_list("PLlist", Some(1)).await;
        loop {
            if let EngineEvent::State(s) = next_real(&mut a, &mut bark).await
                && s.state == PlayState::Playing
                && s.video_id == Some(vid('B'))
            {
                break;
            }
        }
        // Let the song really play a little (real time: the audio thread's clock).
        std::thread::sleep(Duration::from_millis(300));
        // 30 s of play on the paused clock: the periodic save is the one a crash relies on.
        let mut ticks = 0;
        while ticks < 30 {
            if let EngineEvent::Position { .. } = next_real(&mut a, &mut bark).await {
                ticks += 1;
            }
        }
        let t = std::time::Instant::now();
        loop {
            if let Some(s) = crate::state::load(dir.path())
                && s.position > 0.0
            {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(3), "the 30 s save");
            std::thread::sleep(Duration::from_millis(5));
        }
        let playing_at = a.status().await.position;
        // A crash, not a quit: no last save.
        a.task.abort();
        let _ = a.task.await;
        let saved = crate::state::load(dir.path()).unwrap();
        assert!(saved.position > 0.0);
        assert!(
            playing_at - saved.position < 30.0,
            "within 30 s of where it was"
        );
        assert_eq!(saved.queue[saved.current_index].video_id, vid('B'));

        // The new engine: B, paused at the saved second, nothing fetched or downloading.
        let loaded = saved.clone();
        let mut b = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            saved: Some(loaded),
            ..Setup::default()
        })
        .await;
        let s = b.status().await;
        assert_eq!(s.state, PlayState::Paused);
        assert_eq!(s.video_id, Some(vid('B')));
        assert_eq!(s.position, saved.position);
        assert_eq!(s.meta.as_ref().map(|m| m.title.as_str()), Some("Title B"));
        let q = b.queue().await;
        let ids: Vec<_> = q.items.iter().map(|i| i.song.video_id.clone()).collect();
        assert_eq!(ids, [vid('A'), vid('B'), vid('C')]);
        assert_eq!(q.current_id, Some(id_of(&q, 'B')));
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(b.calls().is_empty(), "no link fetched before play");
        assert!(b.started().is_empty(), "no download before play");
        assert!(
            b.source.requests().is_empty(),
            "no queue request before play"
        );

        // Play with no id picks up there.
        let (_dog, mut bark) = watchdog(Duration::from_secs(30));
        b.send(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        })
        .await;
        let mut first = None;
        loop {
            if let EngineEvent::State(s) = next_real(&mut b, &mut bark).await {
                first.get_or_insert(s.clone());
                if s.state == PlayState::Playing {
                    break;
                }
            }
        }
        let first = first.unwrap();
        assert_eq!(first.state, PlayState::Buffering);
        assert_eq!(first.video_id, Some(vid('B')));
        assert_eq!(first.position, saved.position, "starts at the saved second");
        assert_eq!(b.started()[0], vid('B'));
    }

    #[tokio::test]
    async fn a_seek_before_the_first_play_moves_the_start() {
        let saved = Saved {
            queue: vec![song('A')],
            position: 0.2,
            ..Saved::default()
        };
        let mut r = rig(Setup {
            saved: Some(saved),
            ..Setup::default()
        })
        .await;
        r.send(EngineCmd::Seek(1.0)).await;
        let s = r.status().await;
        assert_eq!((s.state, s.position), (PlayState::Paused, 1.0));
        assert!(r.calls().is_empty());
        // Toggle plays it too, from the new second.
        r.send(EngineCmd::Toggle).await;
        let s = states(&r.until(PlayState::Playing).await);
        assert_eq!(s[0].state, PlayState::Buffering);
        assert_eq!(s[0].position, 1.0);
    }

    #[tokio::test]
    async fn a_resumed_radio_keeps_refilling() {
        let saved = Saved {
            queue: vec![song('A'), song('B')],
            source_playlist: Some(radio_of('A')),
            continuation: Some("CONT9".into()),
            volume: 0.3,
            repeat: Repeat::Off,
            ..Saved::default()
        };
        let mut r = rig(Setup {
            pages: vec![ok("CONT9", 0, "CD", Some("CONT10"))],
            saved: Some(saved),
            ..Setup::default()
        })
        .await;
        assert_eq!(r.status().await.volume, 0.3);
        eventually("the restored volume reaches the output", || {
            r.stats.volume() == 0.3
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(r.source.requests().is_empty(), "nothing asked before play");
        r.send(EngineCmd::Toggle).await;
        r.until(PlayState::Playing).await;
        let t = std::time::Instant::now();
        let q = loop {
            let q = r.queue().await;
            if q.items.len() == 4 || t.elapsed() > Duration::from_secs(3) {
                break q;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let ids: Vec<_> = q.items.iter().map(|i| i.song.video_id.clone()).collect();
        assert_eq!(ids, [vid('A'), vid('B'), vid('C'), vid('D')]);
        // The radio's own next page, not a new radio of the last song.
        assert_eq!(
            r.source.requests()[0],
            NextRequest {
                continuation: Some("CONT9".into()),
                ..NextRequest::default()
            }
        );
    }

    #[tokio::test]
    async fn previous_on_a_restored_song() {
        let long_b = SongItem {
            length_seconds: 300,
            ..song('B')
        };
        let saved = Saved {
            queue: vec![song('A'), long_b],
            current_index: 1,
            position: 100.0,
            ..Saved::default()
        };
        let r = rig(Setup {
            saved: Some(saved.clone()),
            ..Setup::default()
        })
        .await;
        // Over 3 s in: back to its start, still paused, still not loaded.
        r.send(EngineCmd::Previous).await;
        let s = r.status().await;
        assert_eq!(
            (s.state, s.video_id.clone(), s.position),
            (PlayState::Paused, Some(vid('B')), 0.0)
        );
        assert!(r.calls().is_empty());
        // Under 3 s in: the song before, which (like Next) plays.
        r.send(EngineCmd::Previous).await;
        let s = r.status().await;
        assert_eq!(s.video_id, Some(vid('A')));
        assert_ne!(s.state, PlayState::Paused);
    }

    #[tokio::test]
    async fn quit_writes_a_last_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "ABC", None)],
            writer: Some(Writer::spawn(dir.path().to_path_buf())),
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", Some(2)).await;
        r.until_song(&vid('C'), PlayState::Playing).await;
        r.send(EngineCmd::Shuffle(true)).await;
        r.send(EngineCmd::Repeat(Repeat::All)).await;
        r.send(EngineCmd::Pause).await;
        r.until(PlayState::Paused).await;
        let at = r.status().await.position;
        r.send(EngineCmd::Quit).await;
        tokio::time::timeout(Duration::from_secs(5), r.task)
            .await
            .unwrap()
            .unwrap();
        // Written before `run` returned: no waiting for the writer here.
        let s = crate::state::load(dir.path()).unwrap();
        assert_eq!(s.version, crate::state::VERSION);
        assert_eq!(s.queue[s.current_index].video_id, vid('C'));
        assert!((s.position - at).abs() < 0.05, "{} vs {at}", s.position);
        assert!(s.shuffle);
        assert_eq!(s.repeat, Repeat::All);
        assert_eq!(s.source_playlist.as_deref(), Some("PLlist"));
        // The current song first in the shuffled order; the original order kept.
        assert_eq!(s.current_index, 0);
        let original: Vec<String> = s
            .original_order
            .unwrap()
            .iter()
            .map(|&p| s.queue[p].video_id.clone())
            .collect();
        assert_eq!(original, [vid('A'), vid('B'), vid('C')]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_disk_holds_quit_for_2_s_at_most() {
        let gate = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
        let held = gate.clone();
        let writer = Writer::with(Box::new(move |_| {
            let mut shut = held.0.lock().unwrap();
            while *shut {
                shut = held.1.wait(shut).unwrap();
            }
            Ok(())
        }));
        let r = rig(Setup {
            writer: Some(writer),
            ..Setup::default()
        })
        .await;
        r.send(EngineCmd::Volume(0.5)).await;
        let t = Instant::now();
        r.send(EngineCmd::Quit).await;
        r.task.await.unwrap();
        assert_eq!(t.elapsed(), Duration::from_secs(2));
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();
    }

    /// Every string in a JSON value, with the key it sits under.
    fn strings<'a>(v: &'a serde_json::Value, key: &'a str, out: &mut Vec<(&'a str, &'a str)>) {
        match v {
            serde_json::Value::String(s) => out.push((key, s)),
            serde_json::Value::Array(a) => a.iter().for_each(|x| strings(x, key, out)),
            serde_json::Value::Object(o) => o.iter().for_each(|(k, x)| strings(x, k, out)),
            _ => {}
        }
    }

    #[tokio::test]
    async fn no_url_or_cookie_in_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = rig(Setup {
            pages: vec![ok("PLlist", 0, "AB", Some("CONTX"))],
            writer: Some(Writer::spawn(dir.path().to_path_buf())),
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        let link = format!(
            "{}:{}",
            r.server.base.host_str().unwrap(),
            r.server.base.port().unwrap()
        );
        r.send(EngineCmd::Quit).await;
        r.task.await.unwrap();
        let text = std::fs::read_to_string(dir.path().join(crate::state::FILE_NAME)).unwrap();
        // The song's link (on the test server) went nowhere near the file.
        assert!(
            !text.contains(&link) && !text.contains("127.0.0.1"),
            "{text}"
        );
        let lower = text.to_lowercase();
        for word in [
            "googlevideo",
            "cookie",
            "sapisid",
            "__secure",
            "authorization",
            "signature",
            "expire",
        ] {
            assert!(!lower.contains(word), "{word} in {text}");
        }
        // The only links are thumbnails, on an allowed https host.
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut all = Vec::new();
        strings(&v, "", &mut all);
        for (key, s) in all {
            if s.contains("://") || s.starts_with("//") {
                assert_eq!(key, "thumbnail", "{s}");
                let u = Url::parse(s).unwrap();
                assert!(crate::net::allowed_host(&u), "{s}");
            }
        }
    }
}
