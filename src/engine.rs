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
//! Three counters keep late news out of the state:
//! - every play gets a generation number, and a resolve that comes back for an older one is
//!   dropped (its task is aborted too, but an answer already in the channel isn't);
//! - every new queue gets a queue generation, and a queue page that comes back for an older
//!   queue is dropped the same way;
//! - every load handed to the audio thread is counted, and so is every `AudioEvent::Loading`
//!   it sends back; until they match, its events are about an earlier track and are dropped.
//!
//! The state follows the commands (pause is paused at once), except that a song only turns
//! `Playing` when the audio thread says it started: until then it is `Buffering`.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

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
use crate::streams::{Resolver, Stream, TrackMeta};

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
    ///   (from where it stopped, after a mid-song error); with nothing at all, Liked songs.
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
    QueueAdd {
        songs: Vec<SongItem>,
        at: AddAt,
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

/// A finished resolve, tagged with the play it was for.
struct Resolved {
    generation: u64,
    result: Result<Stream, Error>,
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
    /// Where the current song stopped after a failure: a play goes on from there.
    resume_from: Option<f64>,
    /// The current song's details from its link (fills gaps in the queue item's).
    resolved_meta: Option<TrackMeta>,
    /// The latest link prefetch, by queue id.
    prefetch: Option<(u64, AbortHandle)>,
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
            resume_from: None,
            resolved_meta: None,
            prefetch: None,
            start_seconds: 0.0,
            loaded: false,
            started: false,
            replayed: false,
            loads_sent: 0,
            loads_seen: 0,
            ticker: None,
        };
        (engine, cmd_tx, events)
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
                Some(e) = audio.recv() => self.on_audio(e),
                () = next_tick(&mut self.ticker) => self.on_tick(),
            }
        }
        let prefetch = self.prefetch.take().map(|(_, task)| task);
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
            EngineCmd::QueueAdd { songs, at } => self.queue_add(songs, at),
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
                // The queue ran out and songs were added since: go on with them.
                if self.at_end && self.queue.peek_next(false).is_some() {
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
        match plan.seed {
            None => {
                self.queue.replace(page.items, plan.index.unwrap_or(0));
                self.emit_queue();
                let paused = self.status.state == PlayState::Paused;
                self.start_current(plan.start);
                if paused {
                    self.pause();
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
                if let Some(item) = self.queue.current() {
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
        if self.loaded && self.started {
            self.status.position = self.player.position();
        }
        self.generation += 1;
        if let Some(task) = self.resolving.take() {
            task.abort();
        }
        self.player.stop();
        self.loaded = false;
        self.started = false;
        self.ticker = None;
    }

    fn previous(&mut self) {
        let position = self.snapshot().position;
        let moved = matches!(self.queue.previous(position), Previous::Item(_));
        if moved {
            self.start_current(0.0);
            self.emit_queue();
            self.maybe_refill();
        } else if self.loaded {
            self.seek(0.0);
        } else if self.queue.current().is_some() {
            self.start_current(0.0);
        }
    }

    fn queue_add(&mut self, songs: Vec<SongItem>, at: AddAt) {
        self.queue.add(songs, at);
        self.emit_queue();
        if self.waiting {
            self.waiting = false;
            return self.advance(false, true);
        }
        self.maybe_refill();
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
        self.start(item.song.video_id, start);
    }

    /// A new song: drop the old one at once and resolve the new one in the background.
    fn start(&mut self, video_id: String, start: f64) {
        self.generation += 1;
        if let Some(task) = self.resolving.take() {
            task.abort();
        }
        // Cancels the old track's reader too, so an audio thread stuck opening it is freed.
        self.player.stop();
        self.loaded = false;
        self.started = false;
        self.replayed = false;
        self.ticker = None;
        self.start_seconds = start;
        self.status.state = PlayState::Buffering;
        self.status.video_id = Some(video_id.clone());
        self.status.position = start;
        self.emit_state();
        crate::trace::play(&video_id);

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
        let gain = loudness_gain(stream.loudness_db);
        let mime = stream.mime.clone();
        // The link's own length first: it describes the file the audio thread decodes.
        let length_hint = [
            stream.meta.length_seconds,
            self.status.meta.as_ref().map_or(0, |m| m.length_seconds),
        ]
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
        // The reader keeps the download alive; the buffer itself isn't needed after this.
        let buffer = (self.start_buffer)(stream, relink);
        self.player.load(
            buffer.reader(),
            &mime,
            gain,
            self.start_seconds,
            length_hint,
        );
        self.loads_sent += 1;
        self.loaded = true;
        // Paused while it resolved: it loads paused at its start point.
        if self.status.state == PlayState::Buffering {
            self.player.play();
        }
        self.emit_state();
    }

    /// An unplayable song: report it and move on, unless every song in the queue failed in
    /// a row (a full pass), which stops rather than skipping round for ever.
    fn skip_unplayable(&mut self, e: &Error) {
        self.emit(EngineEvent::Error {
            code: e.code(),
            message: e.to_string(),
        });
        self.loaded = false;
        self.started = false;
        self.ticker = None;
        self.skip_streak += 1;
        // While a play's list is still coming, the queue isn't all there yet.
        if self.loading.is_none() && self.skip_streak >= self.queue.len() {
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
        if self.loaded {
            self.player.play();
        }
        // A song that never started waits for the audio thread's `Started`.
        if self.loaded && self.started {
            self.set_playing();
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
    }

    fn volume(&mut self, v: f32) {
        if v.is_nan() {
            return;
        }
        let v = v.clamp(0.0, 1.0);
        self.player.set_volume(v);
        self.status.volume = v;
        self.emit_state();
    }

    fn on_audio(&mut self, event: AudioEvent) {
        if event == AudioEvent::Loading {
            self.loads_seen += 1;
            return;
        }
        // Not about the current song: an earlier track's news, sent before the audio thread
        // took the newest load (or while the new song is still resolving).
        if !self.loaded || self.loads_seen < self.loads_sent {
            return;
        }
        match event {
            AudioEvent::Started => {
                self.started = true;
                // A song played: the skipping run (if any) is over.
                self.skip_streak = 0;
                if self.status.state == PlayState::Buffering {
                    self.set_playing();
                    self.emit_state();
                }
            }
            // The engine set these states when it sent the command.
            AudioEvent::Paused | AudioEvent::Resumed | AudioEvent::Loading => {}
            AudioEvent::Ended => {
                self.status.position = self.player.position();
                self.loaded = false;
                self.started = false;
                self.ticker = None;
                // A normal load of the next item (ruling S2; gapless handover is Task 5).
                self.advance(true, true);
            }
            AudioEvent::Error(e) => {
                self.status.position = self.player.position();
                let was_paused = self.status.state == PlayState::Paused;
                self.fail(&e);
                // The sound server restarted under the song (often a `systemctl restart` or
                // an update): after reporting it, play the song again from where it was, on
                // a new stream. The link is usually still cached, so this is quick.
                if e == Error::OutputRestarted && !self.replayed && self.queue.current().is_some() {
                    let at = self.status.position;
                    self.start_current(at);
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
        let seconds = self.player.position();
        self.status.position = seconds;
        self.emit(EngineEvent::Position {
            seconds,
            seeked: false,
        });
        self.maybe_prefetch(seconds);
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

    fn emit_queue(&self) {
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
        self.emit(EngineEvent::State(status));
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
            engine,
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
        assert_eq!(r.started(), [vid('B')]);
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
        // It waited (buffering) rather than stopping, and nothing played twice.
        assert!(
            !states(&seen).iter().any(|s| s.state == PlayState::Stopped),
            "{seen:?}"
        );
        assert_eq!(r.started(), [vid('A'), vid('B')]);
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
    async fn next_link_prefetched_at_half() {
        // The queue says each song is 1 s long (the fixture plays 2 s): half is 0.5 s, so the
        // first tick (1 s) prefetches.
        let short = |c| SongItem {
            length_seconds: 1,
            ..song(c)
        };
        let mut r = rig(Setup {
            pages: vec![(
                "PLlist".into(),
                0,
                Ok(NextPage {
                    items: vec![short('A'), short('B')],
                    ..NextPage::default()
                }),
            )],
            ..Setup::default()
        })
        .await;
        r.play_list("PLlist", None).await;
        r.until_song(&vid('A'), PlayState::Playing).await;
        assert_eq!(r.calls(), [vid('A')], "nothing before half");
        let calls = r.resolver.clone();
        eventually("B's link is fetched", || {
            calls.calls.lock().unwrap().contains(&vid('B'))
        })
        .await;
        // Only its link: B isn't loaded while A plays.
        assert_eq!(r.started(), [vid('A')]);
        assert_eq!(r.status().await.video_id, Some(vid('A')));
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
}
