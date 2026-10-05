//! The engine: one task that owns what is playing, takes commands, and reports state.
//!
//! Commands come in on an mpsc channel; state, position and errors go out on a broadcast
//! channel, so the socket and MPRIS can each listen. The engine never waits on the network
//! or on the audio thread: a play's link is resolved in its own task, and the audio thread is
//! driven through `AudioPlayer`'s command channel. (The audio thread can be blocked for a
//! while opening a track whose first bytes haven't arrived; nothing here waits for it.)
//!
//! Two counters keep late news from an older song out of the state:
//! - every play gets a generation number, and a resolve that comes back for an older one is
//!   dropped (its task is aborted too, but an answer already in the channel isn't);
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

use crate::audio::decode::loudness_gain;
use crate::audio::fetch::{Relink, TrackBuffer};
use crate::audio::player::{AudioEvent, AudioPlayer};
use crate::error::Error;
use crate::streams::{Resolver, Stream, TrackMeta};

/// What the socket and MPRIS ask of the engine.
#[derive(Debug)]
pub enum EngineCmd {
    /// Play `video_id` from `start_seconds`. Without an id: resume what is loaded, or play the
    /// last song again once it has ended; with nothing at all, an `internal` error.
    Play {
        video_id: Option<String>,
        start_seconds: f64,
    },
    Pause,
    Toggle,
    Seek(f64),
    /// 0.0 to 1.0 (clamped).
    Volume(f32),
    Status(oneshot::Sender<Status>),
    Quit,
}

/// What the engine reports.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    State(Status),
    /// Once a second while playing, and right after every seek.
    Position {
        seconds: f64,
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
    /// Known once the song's link is resolved.
    pub meta: Option<TrackMeta>,
    pub position: f64,
    /// 0.0 to 1.0 (the socket turns it into a percent).
    pub volume: f32,
}

/// Starts a track's download. `TrackBuffer::start` in production; tests swap in one that
/// allows their local server (ruling R7).
type Starter = Box<dyn Fn(Stream, Relink) -> TrackBuffer + Send + Sync>;

/// How many events a slow listener may fall behind before it misses some.
const EVENTS_CAPACITY: usize = 64;

/// How many commands may wait for the engine.
const COMMANDS_CAPACITY: usize = 32;

const TICK: Duration = Duration::from_secs(1);

/// A finished resolve, tagged with the play it was for.
struct Resolved {
    generation: u64,
    result: Result<Stream, Error>,
}

pub struct Engine {
    resolver: Arc<dyn Resolver>,
    player: AudioPlayer,
    start_buffer: Starter,
    commands: mpsc::Receiver<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    resolved_tx: mpsc::UnboundedSender<Resolved>,
    resolved_rx: mpsc::UnboundedReceiver<Resolved>,
    status: Status,
    /// Bumped by every play that starts a song.
    generation: u64,
    /// The running resolve, aborted when a newer play replaces it.
    resolving: Option<AbortHandle>,
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
        player: AudioPlayer,
    ) -> (
        Engine,
        mpsc::Sender<EngineCmd>,
        broadcast::Sender<EngineEvent>,
    ) {
        Self::with_starter(resolver, player, Box::new(TrackBuffer::start))
    }

    fn with_starter(
        resolver: Arc<dyn Resolver>,
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
        let engine = Engine {
            resolver,
            player,
            start_buffer,
            commands,
            events: events.clone(),
            resolved_tx,
            resolved_rx,
            status: Status {
                state: PlayState::Stopped,
                video_id: None,
                meta: None,
                position: 0.0,
                volume: 1.0,
            },
            generation: 0,
            resolving: None,
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
                Some(e) = audio.recv() => self.on_audio(e),
                () = next_tick(&mut self.ticker) => self.on_tick(),
            }
        }
        if let Some(task) = self.resolving.take() {
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
                start_seconds,
            } => self.play(video_id, start_seconds),
            EngineCmd::Pause => self.pause(),
            EngineCmd::Toggle => match self.status.state {
                PlayState::Playing | PlayState::Buffering => self.pause(),
                PlayState::Paused => self.resume(),
                PlayState::Stopped => self.play(None, 0.0),
            },
            EngineCmd::Seek(seconds) => self.seek(seconds),
            EngineCmd::Volume(v) => self.volume(v),
            EngineCmd::Status(reply) => {
                let _ = reply.send(self.snapshot());
            }
            // Handled by `run`.
            EngineCmd::Quit => {}
        }
    }

    fn play(&mut self, video_id: Option<String>, start_seconds: f64) {
        let start = if start_seconds.is_finite() {
            start_seconds.max(0.0)
        } else {
            0.0
        };
        if let Some(id) = video_id {
            return self.start(id, start);
        }
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
            PlayState::Stopped => match self.status.video_id.clone() {
                Some(id) => self.start(id, start),
                // Step 2 starts Liked songs here.
                None => self.emit(EngineEvent::Error {
                    code: "internal",
                    message: "nothing to play".into(),
                }),
            },
        }
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
        self.status.meta = None;
        self.status.position = start;
        self.emit_state();

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
            Err(e) => return self.fail(&e),
        };
        self.status.meta = Some(stream.meta.clone());
        let gain = loudness_gain(stream.loudness_db);
        let mime = stream.mime.clone();
        let length_hint = Some(f64::from(stream.meta.length_seconds)).filter(|s| *s > 0.0);
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
        self.emit(EngineEvent::Position { seconds: at });
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
                // Step 2 moves to the next song in the queue here.
                self.status.state = PlayState::Stopped;
                self.emit_state();
            }
            AudioEvent::Error(e) => {
                self.status.position = self.player.position();
                self.fail(&e);
                // The sound server restarted under the song (often a `systemctl restart` or
                // an update): after reporting it, play the song again from where it was, on
                // a new stream. The link is usually still cached, so this is quick.
                if e == Error::OutputRestarted
                    && !self.replayed
                    && let Some(id) = self.status.video_id.clone()
                {
                    let at = self.status.position;
                    self.start(id, at);
                    self.replayed = true;
                }
            }
        }
    }

    /// The current song failed: report it, and stop (keeping the song in the status).
    fn fail(&mut self, e: &Error) {
        self.loaded = false;
        self.started = false;
        self.ticker = None;
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
        self.emit(EngineEvent::Position { seconds });
    }

    /// The status, with the position fresh from the audio thread once the song has started
    /// (before that, the audio thread's position is still the old song's or zero).
    fn snapshot(&mut self) -> Status {
        if self.loaded && self.started {
            self.status.position = self.player.position();
        }
        self.status.clone()
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
    use async_trait::async_trait;
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

    #[derive(Default)]
    struct Setup {
        delays: Vec<(&'static str, u64)>,
        failures: Vec<(&'static str, Error)>,
        /// A sink that plays as fast as it can, instead of in real time.
        fast: bool,
    }

    struct Rig {
        cmds: mpsc::Sender<EngineCmd>,
        events: broadcast::Receiver<EngineEvent>,
        /// The ids whose download was started, in order.
        started: Arc<Mutex<Vec<String>>>,
        stats: Arc<NullStats>,
        resolver: Arc<Fake>,
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
    fn engine_for(server: &Server, resolver: Arc<Fake>, fast: bool) -> Built {
        let (sink, stats) = sink(fast);
        let started = Arc::new(Mutex::new(Vec::new()));
        let base = server.base.clone();
        let log = started.clone();
        let starter: Starter = Box::new(move |stream, relink| {
            log.lock().unwrap().push(stream.video_id.clone());
            // No timeouts: a paused test clock would fire them at once (see fetch's tests).
            TrackBuffer::start_with_test_base(stream, relink, base.clone(), reqwest::Client::new())
        });
        let (engine, cmds, events) =
            Engine::with_starter(resolver, AudioPlayer::spawn(sink), starter);
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
        let Built {
            engine,
            cmds,
            events,
            started,
            stats,
        } = engine_for(&server, resolver.clone(), setup.fast);
        let events = events.subscribe();
        let task = tokio::spawn(engine.run());
        Rig {
            cmds,
            events,
            started,
            stats,
            resolver,
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
        r.stats.lose_output();
        let seen = r.until(PlayState::Playing).await;
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
        let at = states[1].position;
        assert!(at > 0.3 && at < 0.7, "restarted at {at}");
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
            if let EngineEvent::Position { seconds } = r.next().await {
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
    async fn seek_past_end_reports_length_minus_one() {
        let server = server().await;
        let fake = Arc::new(Fake {
            base: server.base.clone(),
            delays: HashMap::new(),
            failures: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        let Built {
            mut engine, events, ..
        } = engine_for(&server, fake.clone(), true);
        let mut rx = events.subscribe();
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
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
        // The decoder lands at most 1 s before the end; the report must say the same.
        engine.handle(EngineCmd::Seek(9999.0));
        assert_eq!(
            rx.try_recv().unwrap(),
            EngineEvent::Position { seconds: 317.0 }
        );
        assert!((engine.snapshot().position - 317.0).abs() < 1e-9);
        // Below zero still clamps to the start.
        engine.handle(EngineCmd::Seek(-5.0));
        assert_eq!(
            rx.try_recv().unwrap(),
            EngineEvent::Position { seconds: 0.0 }
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
    async fn play_without_id_and_nothing_loaded_is_noop_error() {
        let mut r = rig(Setup::default()).await;
        r.send(EngineCmd::Play {
            video_id: None,
            start_seconds: 0.0,
        })
        .await;
        assert_eq!(
            r.next().await,
            EngineEvent::Error {
                code: "internal",
                message: "nothing to play".into()
            }
        );
        let s = r.status().await;
        assert_eq!(s.state, PlayState::Stopped);
        assert_eq!(s.video_id, None);
        assert!(r.started().is_empty());
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
        } = engine_for(&server, fake.clone(), true);
        let mut rx = events.subscribe();
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
            start_seconds: 0.0,
        });
        engine.handle(EngineCmd::Play {
            video_id: Some("BBBBBBBBBBB".into()),
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
        let Built { mut engine, .. } = engine_for(&server, fake.clone(), true);
        // A was loaded and started; B is picked and loaded.
        engine.handle(EngineCmd::Play {
            video_id: Some("AAAAAAAAAAA".into()),
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
        // B's own end counts.
        engine.on_audio(AudioEvent::Ended);
        assert_eq!(engine.status.state, PlayState::Stopped);
    }
}
