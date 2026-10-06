//! The control socket: widgets connect, send JSON-line requests, and get replies and the
//! engine's events (see `protocol` and `docs/protocol.md`).
//!
//! Shape:
//! - One hub task accepts connections, watches the engine's state for the idle clock, and
//!   ends the daemon on `quit` or when idle (`idle`).
//! - Each client gets one task that reads its lines and forwards engine events, and one
//!   writer task fed through a bounded queue. A client whose queue fills (it stopped
//!   reading) is dropped: nothing here ever waits on a client, so a stuck widget can't hold
//!   up the engine or the other widgets.
//! - A browsing request (`browse`, `search`, `more`, `playPage`, `lyrics`), and a `like`, runs in a task
//!   of the client's own, so a request that waits on YouTube never holds up that client's
//!   other requests, and its answer goes back to that client alone. At most `MAX_BROWSING` at
//!   once per client.
//!
//! The listening socket comes from systemd (socket activation) when it passed one, else the
//! daemon binds `$XDG_RUNTIME_DIR/ytmfast/socket` itself.

pub mod idle;
pub mod lyrics;
pub mod protocol;
pub mod stop;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio::task::{self, JoinSet};
use tokio::time::{Instant, Sleep};

use crate::browse::{self, Browser};
use crate::engine::{Engine, EngineCmd, EngineEvent, QueueView, Status};
use crate::error::Error;
use crate::mpris;
use idle::IdlePolicy;
use lyrics::{EngineTabs, LyricsCache};
use protocol::{BAD_REQUEST, MAX_LINE, Request};

/// The socket's name in the runtime folder.
pub const SOCKET_NAME: &str = "socket";

/// Lines that may wait for one client's writer. Its socket buffer (a few hundred KB) fills
/// first, so a client only reaches this after it has stopped reading for a long while. Well
/// above the engine's 64-event buffer: a lagged client gets those 64 plus a fresh state in
/// one go, before its writer has had a turn, and that must not count as stuck.
const OUT_QUEUE: usize = 256;

/// Bytes of lines that may wait for one client (queued, or in the write under way), as well
/// as `OUT_QUEUE` lines. A queue event can be about 375 KB (1,000 songs), so 256 of them
/// would hold close to 100 MB for one client that stopped reading; 4 MiB is still about ten
/// full queue events of slack for a slow but live one.
const OUT_BYTES: usize = 4 << 20;

/// How long a closing client gets to take what is already queued for it (a reply sent just
/// before the client half-closed, or the `quit` reply).
const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// Read size for a client's lines. Fixed, so a line near the cap holds at most
/// `MAX_LINE + READ_CHUNK` bytes, never a doubled buffer.
const READ_CHUNK: usize = 64 * 1024;

/// After a failed accept (out of file descriptors, say), wait this long before the next, so
/// the hub doesn't spin on the same error.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Browsing requests one client may have waiting on YouTube at once; more are refused with
/// `bad_request` "busy". A widget opens a page, maybe its next page and a search: 4 covers
/// that, and bounds how many answers (each up to a few hundred KB while being built) one
/// client can make the daemon hold.
const MAX_BROWSING: usize = 4;

/// `playPage`'s answer for a page with nothing to play: `Page.js`'s words, which the widget
/// shows as they are.
const NOTHING_TO_PLAY: &str = "Nothing here can be played.";

/// systemd's first passed descriptor (`SD_LISTEN_FDS_START`).
const LISTEN_FDS_START: RawFd = 3;

/// Only the user's own processes may drive the engine: the socket's 0600 mode and 0700
/// folder already say so, and this check holds even if those were loosened.
pub fn peer_allowed(peer_uid: u32, my_uid: u32) -> bool {
    peer_uid == my_uid
}

/// How the daemon behaves.
#[derive(Clone)]
pub struct Options {
    pub idle: IdlePolicy,
    /// `/sys/class/power_supply` in production; tests point it at a folder they fill.
    pub power_supply_root: PathBuf,
    /// Where to serve MPRIS, if anywhere. Off by default, so a test (or any other caller)
    /// never registers a player on the user's real session bus by accident; the daemon
    /// asks for the session bus.
    pub mpris: Option<mpris::Bus>,
    /// Serves the browsing commands. `None` answers them `unavailable` (tests that don't
    /// browse); the daemon passes the lazily loaded session, so a browse shares the session
    /// (and its one keyring read) with the resolver and the queue source.
    pub browser: Option<Arc<dyn Browser>>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("idle", &self.idle)
            .field("power_supply_root", &self.power_supply_root)
            .field("mpris", &self.mpris)
            .field("browser", &self.browser.is_some())
            .finish()
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            idle: IdlePolicy::default(),
            power_supply_root: PathBuf::from(idle::POWER_SUPPLY_ROOT),
            mpris: None,
            browser: None,
        }
    }
}

/// Why the daemon stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Nothing played for the idle limit.
    Idle,
    /// A client sent `quit`.
    Quit,
    /// The engine task ended by itself, which only a panic does.
    EngineGone,
    /// SIGTERM (`systemctl stop`) or SIGINT.
    Signal,
}

/// Resolves on the first SIGTERM or SIGINT. Registered up front: until it is, either signal
/// still kills the process the default way, without the clean stop.
pub fn termination() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    })
}

/// The hub's two doors for front ends other than the socket (MPRIS): the same idle clock
/// and the same quit path the socket's clients use, so neither is duplicated.
#[derive(Debug, Clone, Default)]
pub struct Hub {
    inner: Arc<HubDoors>,
}

#[derive(Debug, Default)]
struct HubDoors {
    /// A command came in: the idle clock starts again.
    activity: Notify,
    /// Someone asked the daemon to quit (after its reply is out).
    quit: Notify,
}

impl Hub {
    /// A command came in (ruling R21: every command counts).
    pub fn touch(&self) {
        self.inner.activity.notify_one();
    }

    /// Resolves at the next `touch` (or at once for one since the last wait).
    pub async fn touched(&self) {
        self.inner.activity.notified().await;
    }

    /// Ends the daemon the way the socket's `quit` does.
    pub fn quit(&self) {
        self.inner.quit.notify_one();
    }

    /// Resolves once `quit` was called.
    pub async fn quit_requested(&self) {
        self.inner.quit.notified().await;
    }
}

/// Runs the engine and serves the socket until `quit`, idle, `shutdown` resolving (a
/// signal), or the engine dying; then stops the engine and waits for it to finish (which
/// joins the audio thread).
pub async fn run(
    listener: UnixListener,
    engine: Engine,
    cmds: mpsc::Sender<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    options: Options,
    shutdown: impl Future<Output = ()>,
) -> Exit {
    let engine_task = tokio::spawn(engine.run());
    let hub = Hub::default();
    // MPRIS beside the socket, with its own command sender and event receiver. In a task of
    // its own: a slow or missing session bus must not hold up the socket.
    let (stop_mpris, mpris_stopped) = oneshot::channel();
    let mpris_task = options.mpris.clone().map(|bus| {
        tokio::spawn(mpris::run(
            bus,
            cmds.clone(),
            events.subscribe(),
            hub.clone(),
            mpris_stopped,
        ))
    });
    // A signal takes the same way out as idle: dropping `serve` closes the listener and
    // every client, then the engine is told to quit below.
    let exit = tokio::select! {
        exit = serve_with(listener, cmds.clone(), events, options, hub) => exit,
        () = shutdown => Exit::Signal,
    };
    // The bus name goes first, while the process surely still runs: desktop widgets drop
    // the player at once instead of showing a dead one until the connection closes.
    let _ = stop_mpris.send(());
    if let Some(task) = mpris_task {
        let _ = task.await;
    }
    // Fails only when the engine is already gone.
    let _ = cmds.send(EngineCmd::Quit).await;
    match engine_task.await {
        Ok(()) => exit,
        Err(_) => Exit::EngineGone,
    }
}

/// What every client task shares with the hub.
struct Shared {
    cmds: mpsc::Sender<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    /// Activity and quit, shared with MPRIS.
    hub: Hub,
    browser: Option<Arc<dyn Browser>>,
    /// Lyrics answers kept for every client, and the engine's Lyrics tabs.
    lyrics: LyricsCache,
}

/// Serves the socket until `quit`, idle, or the engine stopping. Leaves the engine running.
pub async fn serve(
    listener: UnixListener,
    cmds: mpsc::Sender<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    options: Options,
) -> Exit {
    serve_with(listener, cmds, events, options, Hub::default()).await
}

/// `serve`, with a hub other front ends share.
async fn serve_with(
    listener: UnixListener,
    cmds: mpsc::Sender<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    options: Options,
    hub: Hub,
) -> Exit {
    let my_uid = current_uid();
    let mut monitor = events.subscribe();
    let shared = Arc::new(Shared {
        lyrics: LyricsCache::new(Arc::new(EngineTabs(cmds.clone()))),
        cmds,
        events,
        hub,
        browser: options.browser,
    });
    // Dropping the set when this returns aborts every client task.
    let mut clients = JoinSet::new();
    let policy = options.idle;
    let root = options.power_supply_root;
    let mut playing = false;
    let mut last_active = Instant::now();
    // The idle timer exists only while nothing plays, so a playing engine has no timer
    // waking the hub. It first fires at the shorter (battery) limit, then at the limit for
    // the power source of that moment: unplugging mid-wait is noticed by then, without
    // polling the power supply.
    let mut idle_timer = Some(idle_sleep(last_active + policy.earliest()));

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let allowed = stream
                        .peer_cred()
                        .is_ok_and(|c| peer_allowed(c.uid(), my_uid));
                    if allowed {
                        clients.spawn(client(stream, shared.clone()));
                    } else {
                        eprintln!("ytmfast: refused a connection from another user");
                    }
                }
                Err(e) => {
                    eprintln!("ytmfast: accept failed: {e}");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
            event = monitor.recv() => {
                let now_playing = match event {
                    Ok(EngineEvent::State(s)) => idle::is_playing(s.state),
                    Ok(_) => continue,
                    // Missed some states: ask for the current one.
                    Err(RecvError::Lagged(_)) => match query_status(&shared.cmds).await {
                        Some(s) => idle::is_playing(s.state),
                        None => return Exit::EngineGone,
                    },
                    // Can't happen while this runs: `shared.events` is a sender too. An engine
                    // that dies is caught by the `closed()` arm below instead.
                    Err(RecvError::Closed) => return Exit::EngineGone,
                };
                if now_playing != playing {
                    playing = now_playing;
                    last_active = Instant::now();
                    idle_timer = (!playing).then(|| idle_sleep(last_active + policy.earliest()));
                }
            }
            () = shared.hub.touched() => {
                last_active = Instant::now();
                if !playing {
                    idle_timer = Some(idle_sleep(last_active + policy.earliest()));
                }
            }
            () = shared.hub.quit_requested() => return Exit::Quit,
            // The engine task ended (a panic drops its command receiver). The events channel
            // can't say so, since `shared` holds a sender, and while the last state was
            // playing there is no idle timer: without this the daemon would never exit.
            () = shared.cmds.closed() => return Exit::EngineGone,
            () = wait(&mut idle_timer) => {
                let on_battery = idle::on_battery_in(&root);
                if idle::should_quit(last_active, Instant::now(), playing, on_battery, policy) {
                    return Exit::Idle;
                }
                idle_timer = Some(idle_sleep(last_active + policy.limit(on_battery)));
            }
            Some(_) = clients.join_next() => {}
        }
    }
}

fn idle_sleep(deadline: Instant) -> Pin<Box<Sleep>> {
    Box::pin(tokio::time::sleep_until(deadline))
}

/// Waits for the timer; never, while there is none.
async fn wait(timer: &mut Option<Pin<Box<Sleep>>>) {
    match timer {
        Some(t) => t.as_mut().await,
        None => std::future::pending().await,
    }
}

async fn query_status(cmds: &mpsc::Sender<EngineCmd>) -> Option<Status> {
    let (tx, rx) = oneshot::channel();
    cmds.send(EngineCmd::Status(tx)).await.ok()?;
    rx.await.ok()
}

async fn query_queue(cmds: &mpsc::Sender<EngineCmd>) -> Option<QueueView> {
    let (tx, rx) = oneshot::channel();
    cmds.send(EngineCmd::QueueGet(tx)).await.ok()?;
    rx.await.ok()
}

/// What a client that fell behind may have missed: the state, and the queue (a missed
/// queue event would leave a widget's list wrong until the next change). One line each.
async fn catch_up(cmds: &mpsc::Sender<EngineCmd>) -> Option<[String; 2]> {
    let status = query_status(cmds).await?;
    let queue = query_queue(cmds).await?;
    Some([
        protocol::event_line(&EngineEvent::State(status)),
        protocol::event_line(&EngineEvent::Queue {
            items: queue.items,
            current_id: queue.current_id,
            shuffle: queue.shuffle,
            repeat: queue.repeat,
        }),
    ])
}

/// How a client's connection ends.
enum Close {
    /// Let the writer send what is queued first (up to `FLUSH_WAIT`).
    Flush,
    /// Drop it now: it stopped reading, or its socket failed.
    Now,
}

/// One client: reads its requests and forwards engine events into its writer's queue.
async fn client(stream: UnixStream, shared: Arc<Shared>) {
    let (read_half, write_half) = stream.into_split();
    let (tx, out_rx) = mpsc::channel::<String>(OUT_QUEUE);
    let out = Outbox {
        tx,
        bytes: Arc::new(AtomicUsize::new(0)),
    };
    let mut writer = tokio::spawn(write_lines(write_half, out_rx, out.bytes.clone()));
    let mut events = shared.events.subscribe();
    let mut lines = LineReader::new(read_half);
    let mut quit = false;
    // The client's browsing requests on their way, and which request id each task answers
    // (for a task that panics). Dropping the set when the client goes aborts them.
    let mut browsing: JoinSet<String> = JoinSet::new();
    let mut asking: HashMap<task::Id, u64> = HashMap::new();
    // False once the client closed its sending side: its browsing answers still go out.
    let mut reading = true;

    let close = loop {
        tokio::select! {
            line = lines.next(), if reading => match line {
                Ok(Line::Text(text)) => {
                    if text.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let reply = match handle(&shared, &text).await {
                        Handled::Reply(reply) => reply,
                        Handled::Quit(reply) => {
                            quit = true;
                            reply
                        }
                        Handled::Browse(id, request, epoch) => {
                            if browsing.len() >= MAX_BROWSING {
                                protocol::error_reply(Some(id), BAD_REQUEST, "busy")
                            } else {
                                let task =
                                    browsing.spawn(answer(shared.clone(), id, request, epoch));
                                asking.insert(task.id(), id);
                                continue;
                            }
                        }
                    };
                    if !push(&out, reply) {
                        break Close::Now;
                    }
                    if quit {
                        break Close::Flush;
                    }
                }
                Ok(Line::TooLong) => {
                    let _ = push(&out, protocol::error_reply(None, BAD_REQUEST, "line too long"));
                    break Close::Flush;
                }
                // The client is done sending; what it asked for still goes out, browsing
                // answers included.
                Ok(Line::Eof) => {
                    if browsing.is_empty() {
                        break Close::Flush;
                    }
                    reading = false;
                }
                Err(_) => break Close::Now,
            },
            Some(done) = browsing.join_next_with_id(), if !browsing.is_empty() => {
                let reply = match done {
                    Ok((task, reply)) => {
                        asking.remove(&task);
                        reply
                    }
                    // A panic in a browsing task is a bug; the client still gets an answer.
                    Err(e) => protocol::error_reply(
                        asking.remove(&e.id()),
                        "internal",
                        "internal error: the request failed",
                    ),
                };
                if !push(&out, reply) {
                    break Close::Now;
                }
                if !reading && browsing.is_empty() {
                    break Close::Flush;
                }
            },
            event = events.recv() => {
                // A big queue goes through the same bounded queue as everything else: a
                // client that stopped reading is still dropped, never waited on.
                let pushed = match event {
                    Ok(e) => push(&out, protocol::event_line(&e)),
                    // It missed some events: a fresh state and queue cover them.
                    Err(RecvError::Lagged(_)) => match catch_up(&shared.cmds).await {
                        Some(lines) => lines.into_iter().all(|l| push(&out, l)),
                        None => break Close::Flush,
                    },
                    // Can't happen while the hub's `shared.events` sender lives; kept so a
                    // closed channel ends the client rather than spinning.
                    Err(RecvError::Closed) => break Close::Flush,
                };
                if !pushed {
                    break Close::Now;
                }
            }
            _ = &mut writer => break Close::Now,
        }
    };

    drop(out);
    drop(events);
    match close {
        Close::Flush => {
            if tokio::time::timeout(FLUSH_WAIT, &mut writer).await.is_err() {
                writer.abort();
            }
        }
        Close::Now => writer.abort(),
    }
    if quit {
        shared.hub.quit();
    }
}

/// One client's outgoing lines, capped by count (the channel) and by bytes.
struct Outbox {
    tx: mpsc::Sender<String>,
    /// Bytes pushed and not yet written; the writer takes them off once written.
    bytes: Arc<AtomicUsize>,
}

/// Queues a line for the writer; false when the queue is full (`OUT_QUEUE` lines or
/// `OUT_BYTES` bytes) or the writer is gone, and the client should be dropped.
fn push(out: &Outbox, line: String) -> bool {
    let n = line.len();
    // Reserve the bytes, or refuse. A plain compare-exchange loop rather than `fetch_update`,
    // which newer Rust deprecates (renamed `try_update`, missing from older ones): this builds
    // warning-free on both. An empty outbox always takes the line, so one line over the cap
    // (none should be) can't drop every client; the next line then waits for it.
    let mut b = out.bytes.load(Ordering::Acquire);
    loop {
        if b != 0 && b + n > OUT_BYTES {
            return false;
        }
        match out
            .bytes
            .compare_exchange_weak(b, b + n, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => break,
            Err(now) => b = now,
        }
    }
    if out.tx.try_send(line).is_err() {
        out.bytes.fetch_sub(n, Ordering::AcqRel);
        return false;
    }
    true
}

/// What a request comes to.
enum Handled {
    Reply(String),
    /// `quit`'s reply; the client closes once it is out, then the daemon quits.
    Quit(String),
    /// A browsing request (or a `like`), answered from a task of its own (`answer`). For
    /// `playPage`, the engine's play epoch when it came in (ruling P7).
    Browse(u64, Request, Option<u64>),
}

/// One request.
async fn handle(shared: &Shared, text: &[u8]) -> Handled {
    let (id, request) = match protocol::parse_request(text) {
        Ok(r) => r,
        Err(bad) => {
            return Handled::Reply(protocol::error_reply(bad.id, BAD_REQUEST, &bad.message));
        }
    };
    // Browsing counts as activity too (ruling R21): a user looking through pages with
    // nothing playing is not idle.
    shared.hub.touch();
    let gone = || protocol::error_reply(Some(id), "internal", "the engine stopped");
    let cmd = match request {
        Request::Status => {
            return Handled::Reply(match query_status(&shared.cmds).await {
                Some(s) => protocol::ok_reply(id, Value::Object(protocol::status_data(&s))),
                None => gone(),
            });
        }
        Request::QueueGet => {
            return Handled::Reply(match query_queue(&shared.cmds).await {
                Some(q) => protocol::ok_reply(id, protocol::queue_data(&q)),
                None => gone(),
            });
        }
        Request::Quit => return Handled::Quit(protocol::ok_reply(id, json!({}))),
        // Read now, in this client's command order (through the engine's channel): any play
        // the user makes after this, from anywhere, wins over the page's (ruling P7).
        request @ Request::PlayPage { .. } => {
            let (tx, rx) = oneshot::channel();
            if shared.cmds.send(EngineCmd::PlayEpoch(tx)).await.is_err() {
                return Handled::Reply(gone());
            }
            return match rx.await {
                Ok(epoch) => Handled::Browse(id, request, Some(epoch)),
                Err(_) => Handled::Reply(gone()),
            };
        }
        // A like waits on YouTube (the engine sends it from a task of its own): answered like
        // a browse, so this client's other requests go on meanwhile.
        request @ (Request::Browse { .. }
        | Request::Search { .. }
        | Request::More { .. }
        | Request::Lyrics { .. }
        | Request::Like { .. }) => {
            return Handled::Browse(id, request, None);
        }
        Request::Play {
            video_id,
            playlist_id,
            index,
            start_seconds,
        } => EngineCmd::Play {
            video_id,
            playlist_id,
            index,
            params: None,
            start_seconds,
        },
        Request::PlayEndpoint(endpoint) => match EngineCmd::play_endpoint(endpoint) {
            Some(cmd) => cmd,
            // `parse_endpoint` never lets one through that plays nothing.
            None => {
                return Handled::Reply(protocol::error_reply(
                    Some(id),
                    BAD_REQUEST,
                    "the endpoint plays nothing",
                ));
            }
        },
        Request::Pause => EngineCmd::Pause,
        Request::Toggle => EngineCmd::Toggle,
        Request::Seek { seconds } => EngineCmd::Seek(seconds),
        Request::Volume { percent } => EngineCmd::Volume(protocol::percent_to_volume(percent)),
        Request::Next => EngineCmd::Next,
        Request::Previous => EngineCmd::Previous,
        // Answered by the engine, which alone knows whether the songs fit (ruling S15).
        Request::QueueAdd { songs, at } => {
            let (added, ok) = oneshot::channel();
            if shared
                .cmds
                .send(EngineCmd::QueueAdd { songs, at, added })
                .await
                .is_err()
            {
                return Handled::Reply(gone());
            }
            return Handled::Reply(match ok.await {
                Ok(true) => protocol::ok_reply(id, json!({})),
                Ok(false) => protocol::error_reply(Some(id), BAD_REQUEST, "the queue is full"),
                Err(_) => gone(),
            });
        }
        Request::QueueRemove { id } => EngineCmd::QueueRemove(id),
        Request::QueueJump { id } => EngineCmd::QueueJump(id),
        Request::QueueMove { id, index } => EngineCmd::QueueMove { id, index },
        Request::Shuffle { on } => EngineCmd::Shuffle(on),
        Request::Repeat { mode } => EngineCmd::Repeat(mode),
        Request::Mute { on } => EngineCmd::Mute(on),
    };
    // "ok" means the engine took the command; what came of it arrives as events.
    Handled::Reply(match shared.cmds.send(cmd).await {
        Ok(()) => protocol::ok_reply(id, json!({})),
        Err(_) => gone(),
    })
}

/// A browsing request's reply line, for the client that asked. A failure is answered with its
/// code and fixed text (`Error`'s `Display`, which never holds a value the client sent, a link
/// or a token; ruling R6) and is never broadcast as an `error` event: it is this client's
/// news alone, and a refused id (`bad_request`) is the client's own mistake. Nothing is logged
/// here either: the request already logged its failure by endpoint and code.
///
/// `playPage`'s play is sent as `EngineCmd::PlayIfLatest` with the epoch read when it came in
/// (`epoch`): when the user picked something else to play while the page loaded, the page's
/// play is dropped and the reply says `{"superseded": true}` (ruling P7).
async fn answer(shared: Arc<Shared>, id: u64, request: Request, epoch: Option<u64>) -> String {
    // Through the engine, which knows the song playing and keeps its like status.
    if let Request::Like { status, video_id } = request {
        return like(&shared.cmds, id, status, video_id).await;
    }
    let Some(browser) = shared.browser.clone() else {
        return protocol::error_reply(Some(id), "unavailable", "browsing is not available");
    };
    let result =
        match request {
            Request::Browse { browse_id, params } => browser
                .browse(&browse_id, params.as_deref())
                .await
                .map(|page| protocol::data_reply(id, &page)),
            Request::Search { query, params } => browser
                .search(&query, params.as_deref())
                .await
                .map(|page| protocol::data_reply(id, &page)),
            Request::More { kind, token } => browser
                .more(kind, &token)
                .await
                .map(|page| protocol::data_reply(id, &page)),
            Request::Lyrics { video_id } => shared
                .lyrics
                .get(browser.as_ref(), &video_id)
                .await
                .map(|lyrics| match lyrics {
                    Some(l) => protocol::data_reply(id, &l),
                    None => protocol::ok_reply(id, json!({ "none": true })),
                }),
            Request::PlayPage { browse_id, params } => {
                match browser.browse(&browse_id, params.as_deref()).await {
                    Ok(page) => {
                        let play = browse::page_play(&page)
                            .cloned()
                            .and_then(EngineCmd::play_endpoint);
                        Ok(match play {
                            Some(cmd) => play_if_latest(&shared.cmds, id, cmd, epoch).await,
                            None => protocol::error_reply(Some(id), BAD_REQUEST, NOTHING_TO_PLAY),
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            // `handle` sends only the browsing requests here.
            _ => Err(Error::Internal("not a browsing request".into())),
        };
    match result {
        // Widgets read lines of at most `MAX_LINE`, as the socket does. The parsing caps
        // (300 rows a section, 1,000 a continuation) keep real answers under it (a full
        // continuation with long fields is about 550 KB; tests/control_browse.rs), so this
        // only guards against a freak page.
        Ok(line) if line.len() > MAX_LINE => protocol::error_reply(
            Some(id),
            "unavailable",
            "unavailable: the page is too big to send",
        ),
        Ok(line) => line,
        Err(e) => protocol::error_reply(Some(id), e.code(), &e.to_string()),
    }
}

/// A like's reply line: ok once YouTube took it. A failure is the asker's alone, with its code
/// and fixed text, as for browsing; the engine never broadcasts it either.
async fn like(
    cmds: &mpsc::Sender<EngineCmd>,
    id: u64,
    status: browse::LikeStatus,
    video_id: Option<String>,
) -> String {
    let gone = || protocol::error_reply(Some(id), "internal", "the engine stopped");
    let (reply, answer) = oneshot::channel();
    let cmd = EngineCmd::Like {
        video_id,
        status,
        reply,
    };
    if cmds.send(cmd).await.is_err() {
        return gone();
    }
    match answer.await {
        Ok(Ok(())) => protocol::ok_reply(id, json!({})),
        Ok(Err(e)) => protocol::error_reply(Some(id), e.code(), &e.to_string()),
        Err(_) => gone(),
    }
}

/// Sends a late play (`playPage`'s) unless a newer one came in since `epoch`; its reply line.
async fn play_if_latest(
    cmds: &mpsc::Sender<EngineCmd>,
    id: u64,
    play: EngineCmd,
    epoch: Option<u64>,
) -> String {
    let gone = || protocol::error_reply(Some(id), "internal", "the engine stopped");
    // `handle` always reads one for a `playPage`.
    let Some(epoch) = epoch else {
        return gone();
    };
    let (played, rx) = oneshot::channel();
    let cmd = EngineCmd::PlayIfLatest {
        epoch,
        play: Box::new(play),
        played,
    };
    if cmds.send(cmd).await.is_err() {
        return gone();
    }
    match rx.await {
        Ok(true) => protocol::ok_reply(id, json!({})),
        Ok(false) => protocol::ok_reply(id, json!({ "superseded": true })),
        Err(_) => gone(),
    }
}

/// Writes queued lines until the queue closes or the socket fails. Takes whatever else is
/// already queued along with each line, so a burst goes out in one write. `bytes` drops by
/// what was written only once it is written: a batch stuck in a write still counts against
/// the client's `OUT_BYTES`.
async fn write_lines<W: AsyncWrite + Unpin>(
    mut socket: W,
    mut queue: mpsc::Receiver<String>,
    bytes: Arc<AtomicUsize>,
) {
    let mut buf = Vec::new();
    while let Some(line) = queue.recv().await {
        buf.clear();
        buf.extend_from_slice(line.as_bytes());
        while let Ok(more) = queue.try_recv() {
            buf.extend_from_slice(more.as_bytes());
        }
        if socket.write_all(&buf).await.is_err() {
            return;
        }
        bytes.fetch_sub(buf.len(), Ordering::AcqRel);
    }
    let _ = socket.shutdown().await;
}

enum Line {
    Text(Vec<u8>),
    TooLong,
    Eof,
}

/// Splits a stream into lines of at most `MAX_LINE` bytes. Cancel safe (it sits in a
/// `select!`): everything read is kept in `self` until a whole line is there.
struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    /// How much of `buf` is known to hold no newline.
    scanned: usize,
    chunk: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    fn new(inner: R) -> Self {
        LineReader {
            inner,
            buf: Vec::new(),
            scanned: 0,
            chunk: vec![0; READ_CHUNK].into_boxed_slice(),
        }
    }

    async fn next(&mut self) -> io::Result<Line> {
        loop {
            if let Some(i) = self.buf[self.scanned..].iter().position(|b| *b == b'\n') {
                let end = self.scanned + i;
                // The newline may come in the same read as the bytes past the cap.
                if end > MAX_LINE {
                    return Ok(Line::TooLong);
                }
                let mut line: Vec<u8> = self.buf.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.scanned = 0;
                return Ok(Line::Text(line));
            }
            self.scanned = self.buf.len();
            if self.buf.len() > MAX_LINE {
                return Ok(Line::TooLong);
            }
            // `read` is cancel safe: either it read into `chunk` and returns, or nothing.
            let n = self.inner.read(&mut self.chunk).await?;
            if n == 0 {
                // A last line without a newline is still a request.
                if self.buf.is_empty() {
                    return Ok(Line::Eof);
                }
                self.scanned = 0;
                return Ok(Line::Text(std::mem::take(&mut self.buf)));
            }
            self.buf.extend_from_slice(&self.chunk[..n]);
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and can't fail.
    unsafe { libc::getuid() }
}

/// The descriptor systemd passed, if it passed one to this process: `LISTEN_PID` must be
/// our pid (the variables may have been inherited from a parent) and `LISTEN_FDS` at least
/// 1. The socket is the first one, fd 3.
pub fn listen_fd_in(env: &dyn Fn(&str) -> Option<OsString>, pid: u32) -> Option<RawFd> {
    let number = |k: &str| env(k)?.to_str()?.parse::<u64>().ok();
    (number("LISTEN_PID")? == u64::from(pid) && number("LISTEN_FDS")? >= 1)
        .then_some(LISTEN_FDS_START)
}

/// Takes the listening socket systemd passed, if any, and clears the `LISTEN_*` variables
/// so nothing this process starts (yt-dlp) thinks they are for it.
///
/// # Safety
///
/// Must run before the process starts any other thread: it changes the environment.
pub unsafe fn take_systemd_listener() -> io::Result<Option<std::os::unix::net::UnixListener>> {
    use std::os::fd::FromRawFd;

    let fd = listen_fd_in(&|k| std::env::var_os(k), std::process::id());
    for k in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: the caller guarantees no other thread exists yet.
        unsafe { std::env::remove_var(k) };
    }
    let Some(fd) = fd else {
        return Ok(None);
    };
    // SAFETY: plain fstat/fcntl calls on an int; they fail cleanly on a bad descriptor.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the descriptor systemd passed is not a socket",
            ));
        }
        // systemd passes it without close-on-exec; the yt-dlp fallback must not inherit
        // the listening socket.
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: systemd handed fd 3 to this process (LISTEN_PID matched), it is a socket, and
    // with the variables cleared nothing else takes ownership of it.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    listener.set_nonblocking(true)?;
    Ok(Some(listener))
}

/// A socket this process bound itself, removed again on exit.
#[derive(Debug)]
pub struct BoundSocket {
    pub path: PathBuf,
    /// Which file we made: on exit, a socket another daemon put there since is left alone.
    id: SocketId,
}

/// A socket file's identity. The inode number alone isn't one: a filesystem hands an
/// unlinked file's number to the next new file, so the change time (set by our chmod right
/// after bind) is compared too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketId {
    dev: u64,
    ino: u64,
    ctime: i64,
    ctime_nsec: i64,
}

impl SocketId {
    fn of(m: &std::fs::Metadata) -> SocketId {
        use std::os::unix::fs::MetadataExt;
        SocketId {
            dev: m.dev(),
            ino: m.ino(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        }
    }
}

impl BoundSocket {
    /// Removes the socket on exit, once our listener is closed. Left alone when it isn't
    /// ours any more, and when anything answers on it: file times come from a coarse clock
    /// (about a millisecond), so a socket re-bound right after ours can share both its inode
    /// number and its change time, and only a live listener tells them apart.
    pub fn remove(self) {
        let ours = std::fs::symlink_metadata(&self.path).is_ok_and(|m| SocketId::of(&m) == self.id);
        if ours && std::os::unix::net::UnixStream::connect(&self.path).is_err() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Binds the socket at `path` (in a 0700 folder) with mode 0600. A socket already there
/// that answers means another daemon is running: refused. One that doesn't answer is left
/// over from a crash and is replaced. Anything else at the path is refused.
pub fn bind_socket(path: &Path) -> io::Result<(std::os::unix::net::UnixListener, BoundSocket)> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "ytmfast is already running",
                ));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Between bind and chmod the socket has the umask's mode; the 0700 folder keeps other
    // users out of it meanwhile.
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let bound = BoundSocket {
        path: path.to_path_buf(),
        id: SocketId::of(&std::fs::symlink_metadata(path)?),
    };
    Ok((listener, bound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn peer_uid_checked() {
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(1001, 1000));
        // Not even root gets in: the engine is the user's alone.
        assert!(!peer_allowed(0, 1000));
    }

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn systemd_fd_only_for_our_pid() {
        let ours = env_of(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "1")]);
        assert_eq!(listen_fd_in(&ours, 42), Some(3));
        // Inherited from a parent: not ours.
        assert_eq!(listen_fd_in(&ours, 43), None);
        assert_eq!(
            listen_fd_in(&env_of(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "2")]), 42),
            Some(3)
        );
        for bad in [
            &[("LISTEN_PID", "42"), ("LISTEN_FDS", "0")][..],
            &[("LISTEN_PID", "42")],
            &[("LISTEN_FDS", "1")],
            &[("LISTEN_PID", "x"), ("LISTEN_FDS", "1")],
            &[("LISTEN_PID", "42"), ("LISTEN_FDS", "-1")],
            &[],
        ] {
            let vars: &'static [(&str, &str)] = bad;
            assert_eq!(listen_fd_in(&env_of(vars), 42), None, "{bad:?}");
        }
    }

    #[test]
    fn binds_0600_and_refuses_a_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let (listener, bound) = bind_socket(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Something answers there: refused, and the socket is left alone.
        let err = bind_socket(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());

        // The daemon died without cleaning up: nothing answers, so it is replaced.
        drop(listener);
        let (listener2, bound2) = bind_socket(&path).unwrap();

        // The first daemon's late cleanup doesn't remove the second one's socket.
        bound.remove();
        assert!(path.exists());
        // The second one's own cleanup does, once its listener is closed (as `daemon` does).
        drop(listener2);
        bound2.remove();
        assert!(!path.exists());
    }

    #[test]
    fn socket_identity_sees_inode_reuse() {
        let a = SocketId {
            dev: 1,
            ino: 42,
            ctime: 1_000,
            ctime_nsec: 5,
        };
        assert_eq!(a, a);
        // The same inode number handed to a newer socket: its change time differs.
        assert_ne!(a, SocketId { ctime_nsec: 6, ..a });
        assert_ne!(a, SocketId { ctime: 1_001, ..a });
        assert_ne!(a, SocketId { ino: 43, ..a });
        assert_ne!(a, SocketId { dev: 2, ..a });
    }

    /// The worst case of inode reuse: a newer socket with the same inode number AND the same
    /// change time (file times come from a coarse clock, so a socket re-bound within a
    /// millisecond or so can share it). A live socket is still never removed.
    #[test]
    fn late_cleanup_never_removes_a_live_socket_even_with_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let (old_listener, _old) = bind_socket(&path).unwrap();
        drop(old_listener);
        let (_new_listener, new) = bind_socket(&path).unwrap();
        // The old daemon's record, forged to match the new socket exactly.
        let forged = BoundSocket {
            path: path.clone(),
            id: new.id,
        };
        forged.remove();
        assert!(path.exists(), "a late cleanup removed a live socket");
    }

    #[test]
    fn refuses_a_file_that_is_not_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        std::fs::write(&path, "keep me").unwrap();
        assert!(bind_socket(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep me");
    }

    #[test]
    fn an_empty_outbox_takes_one_oversized_line() {
        // One line over the byte cap (none should be, but a cap must not make a line that
        // big drop every client) goes out when nothing else waits; the next one waits for it.
        let (tx, _rx) = mpsc::channel::<String>(OUT_QUEUE);
        let out = Outbox {
            tx,
            bytes: Arc::new(AtomicUsize::new(0)),
        };
        assert!(push(&out, "x".repeat(OUT_BYTES + 1)));
        assert!(!push(&out, "y".into()));
        assert_eq!(out.bytes.load(Ordering::Acquire), OUT_BYTES + 1);
        // Under the cap, lines go in until the next would pass it.
        let (tx, _rx) = mpsc::channel::<String>(OUT_QUEUE);
        let out = Outbox {
            tx,
            bytes: Arc::new(AtomicUsize::new(0)),
        };
        assert!(push(&out, "a".repeat(OUT_BYTES - 1)));
        assert!(push(&out, "b".into()));
        assert!(!push(&out, "c".into()));
        assert_eq!(out.bytes.load(Ordering::Acquire), OUT_BYTES);
    }

    #[tokio::test]
    async fn lines_are_split_and_capped() {
        let (mut a, b) = tokio::io::duplex(1 << 16);
        let mut lines = LineReader::new(b);
        tokio::spawn(async move {
            a.write_all(b"one\r\ntwo\nlast").await.unwrap();
        });
        let text = |l: Line| match l {
            Line::Text(t) => String::from_utf8(t).unwrap(),
            Line::TooLong => "<too long>".into(),
            Line::Eof => "<eof>".into(),
        };
        assert_eq!(text(lines.next().await.unwrap()), "one");
        assert_eq!(text(lines.next().await.unwrap()), "two");
        assert_eq!(text(lines.next().await.unwrap()), "last");
        assert_eq!(text(lines.next().await.unwrap()), "<eof>");

        // Exactly the cap is fine; one byte more is not.
        let (mut a, b) = tokio::io::duplex(1 << 16);
        let mut lines = LineReader::new(b);
        tokio::spawn(async move {
            let mut big = vec![b'x'; MAX_LINE];
            big.push(b'\n');
            a.write_all(&big).await.unwrap();
            a.write_all(&vec![b'y'; MAX_LINE + 1]).await.unwrap();
        });
        match lines.next().await.unwrap() {
            Line::Text(t) => assert_eq!(t.len(), MAX_LINE),
            _ => panic!("a line of exactly the cap is taken"),
        }
        assert!(matches!(lines.next().await.unwrap(), Line::TooLong));

        // Over the cap with its newline arriving in the same read: still too long.
        let (mut a, b) = tokio::io::duplex(1 << 16);
        let mut lines = LineReader::new(b);
        tokio::spawn(async move {
            let mut big = vec![b'z'; MAX_LINE + 1];
            big.push(b'\n');
            a.write_all(&big).await.unwrap();
        });
        assert!(matches!(lines.next().await.unwrap(), Line::TooLong));
    }
}
