//! The control socket end to end, on a socket in a temp folder: the real engine with a
//! resolver that never answers (so a play stays buffering) and a `NullSink`, or a fake
//! engine side where a test needs to drive the events itself. Never the real account.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use ytmfast::audio::player::AudioPlayer;
use ytmfast::audio::sink::NullSink;
use ytmfast::control::{self, Exit, Options};
use ytmfast::engine::{Engine, EngineCmd, EngineEvent, PlayState, QueueSource, QueueView, Status};
use ytmfast::error::Error;
use ytmfast::innertube::{NextPage, NextRequest, SongItem};
use ytmfast::queue::{AddAt, QueueItem, Repeat};
use ytmfast::streams::{Resolver, Stream};

const SONG: &str = "dQw4w9WgXcQ";
const MIN: Duration = Duration::from_secs(60);
/// Real-time cap on any one wait, so a broken server fails the test instead of hanging it.
const WAIT: Duration = Duration::from_secs(10);

/// Never answers: a play stays `buffering`, which counts as playing for the idle clock.
struct Hang;

#[async_trait]
impl Resolver for Hang {
    async fn resolve(&self, _: &str) -> Result<Stream, Error> {
        std::future::pending().await
    }
    async fn resolve_fresh(&self, id: &str) -> Result<Stream, Error> {
        self.resolve(id).await
    }
}

#[async_trait]
impl QueueSource for Hang {
    async fn next(&self, _: NextRequest) -> Result<NextPage, Error> {
        std::future::pending().await
    }
}

/// A power supply folder: mains online or not.
fn power(on_ac: bool) -> TempDir {
    let root = tempfile::tempdir().unwrap();
    let ac = root.path().join("AC");
    std::fs::create_dir(&ac).unwrap();
    std::fs::write(ac.join("type"), "Mains\n").unwrap();
    std::fs::write(ac.join("online"), if on_ac { "1\n" } else { "0\n" }).unwrap();
    root
}

fn listener(dir: &Path) -> (UnixListener, PathBuf) {
    let path = dir.join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    (UnixListener::from_std(std).unwrap(), path)
}

/// The real engine behind the socket.
struct Daemon {
    path: PathBuf,
    task: JoinHandle<Exit>,
    _dir: TempDir,
    _power: TempDir,
}

fn daemon(on_ac: bool) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let power = power(on_ac);
    let (listener, path) = listener(dir.path());
    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let (engine, cmds, events) = Engine::new(Arc::new(Hang), Arc::new(Hang), player);
    let options = Options {
        power_supply_root: power.path().to_path_buf(),
        ..Options::default()
    };
    let task = tokio::spawn(control::run(
        listener,
        engine,
        cmds,
        events,
        options,
        std::future::pending(),
    ));
    Daemon {
        path,
        task,
        _dir: dir,
        _power: power,
    }
}

/// A fake engine side: answers `Status` with `status` and `QueueGet` with `fake_queue()`,
/// passes every other command to the test, and stops on `Quit`.
struct FakeEngine {
    path: PathBuf,
    events: broadcast::Sender<EngineEvent>,
    commands: mpsc::UnboundedReceiver<EngineCmd>,
    serve: JoinHandle<Exit>,
    _dir: TempDir,
    _power: TempDir,
}

fn fake_engine(status: Status) -> FakeEngine {
    let dir = tempfile::tempdir().unwrap();
    let power = power(true);
    let (listener, path) = listener(dir.path());
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
    // The engine's own event capacity.
    let (events, _) = broadcast::channel(64);
    let (seen_tx, commands) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                EngineCmd::Status(reply) => {
                    let _ = reply.send(status.clone());
                }
                EngineCmd::QueueGet(reply) => {
                    let _ = reply.send(fake_queue());
                }
                EngineCmd::Quit => return,
                other => {
                    let _ = seen_tx.send(other);
                }
            }
        }
    });
    let options = Options {
        power_supply_root: power.path().to_path_buf(),
        ..Options::default()
    };
    let serve = tokio::spawn(control::serve(listener, cmd_tx, events.clone(), options));
    FakeEngine {
        path,
        events,
        commands,
        serve,
        _dir: dir,
        _power: power,
    }
}

fn full_song(id: &str) -> SongItem {
    SongItem {
        video_id: id.into(),
        title: "Song".into(),
        artists: vec!["A".into(), "B".into()],
        album: Some("Album".into()),
        thumbnail: Some("https://i.ytimg.com/vi/x/hqdefault.jpg".into()),
        length_seconds: 213,
        playlist_id: None,
    }
}

/// What the fake engine answers `QueueGet` with.
fn fake_queue() -> QueueView {
    QueueView {
        items: vec![
            QueueItem {
                id: 3,
                song: full_song(SONG),
            },
            QueueItem {
                id: 9,
                song: SongItem {
                    video_id: "AAAAAAAAAAA".into(),
                    ..SongItem::default()
                },
            },
        ]
        .into(),
        current_id: Some(3),
        shuffle: true,
        repeat: Repeat::One,
    }
}

fn paused_status(id: &str) -> Status {
    Status {
        state: PlayState::Paused,
        video_id: Some(id.into()),
        meta: None,
        album: None,
        queue_id: None,
        position: 1.5,
        volume: 0.5,
        shuffle: false,
        repeat: Repeat::Off,
    }
}

struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

async fn connect(path: &Path) -> Client {
    let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
    Client {
        lines: BufReader::new(read).lines(),
        write,
    }
}

/// Keeps a paused test clock still until dropped. Tokio advances a paused clock whenever
/// the runtime parks, even when that park is what delivers a socket's readiness, so a
/// request in flight could see the idle timer (or a read timeout) fire first. It never
/// auto-advances while a blocking task runs, so one waits here for the drop. Harmless with a
/// real clock.
struct ClockHold(#[allow(dead_code)] std::sync::mpsc::Sender<()>);

fn hold_clock() -> ClockHold {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let _ = rx.recv();
    });
    ClockHold(tx)
}

impl Client {
    async fn send(&mut self, line: &str) {
        let _hold = hold_clock();
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.write_all(b"\n").await.unwrap();
    }

    /// The next line, or None at the end of the connection.
    async fn next(&mut self) -> Option<Value> {
        let _hold = hold_clock();
        let line = tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .expect("no line in time")
            .ok()??;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// The reply to `id`, skipping events.
    async fn reply(&mut self, id: u64) -> Value {
        loop {
            let v = self.next().await.expect("connection closed");
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
        }
    }

    /// The next event named `name`, skipping everything else.
    async fn event(&mut self, name: &str) -> Value {
        loop {
            let v = self.next().await.expect("connection closed");
            if v["event"] == name {
                return v;
            }
        }
    }

    /// Reads until the server closes the connection.
    async fn until_closed(&mut self) {
        while self.next().await.is_some() {}
    }
}

#[tokio::test]
async fn bad_json_gets_bad_request_reply_and_connection_stays_open() {
    let d = daemon(true);
    let mut c = connect(&d.path).await;
    c.send("{nope").await;
    let v = c.next().await.unwrap();
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "bad_request");

    c.send(r#"{"id":2,"cmd":"frobnicate"}"#).await;
    assert_eq!(
        c.reply(2).await,
        json!({"id": 2, "ok": false,
               "error": {"code": "bad_request", "message": "unknown command"}})
    );

    // Still open, and answering.
    c.send(r#"{"id":3,"cmd":"status"}"#).await;
    let v = c.reply(3).await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["state"], "stopped");
    assert_eq!(v["data"]["volume"], 100);
}

#[tokio::test]
async fn line_over_1_mib_closes_connection() {
    let d = daemon(true);
    let mut c = connect(&d.path).await;
    let mut big = vec![b'x'; control::protocol::MAX_LINE + 2];
    big.push(b'\n');
    let Client {
        mut lines,
        mut write,
    } = c;
    // The server stops reading partway, so the write may fail: that's fine.
    tokio::spawn(async move {
        let _ = write.write_all(&big).await;
        // Keep our half open so the close seen below is the server's.
        std::future::pending::<()>().await;
    });
    let line = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["error"]["code"], "bad_request");
    assert_eq!(v["error"]["message"], "line too long");
    let end = tokio::time::timeout(WAIT, lines.next_line()).await.unwrap();
    assert!(matches!(end, Ok(None) | Err(_)), "closed after: {end:?}");

    // The daemon itself carries on.
    c = connect(&d.path).await;
    c.send(r#"{"id":1,"cmd":"status"}"#).await;
    assert_eq!(c.reply(1).await["ok"], true);
}

#[tokio::test]
async fn two_clients_both_get_events() {
    let d = daemon(true);
    let mut a = connect(&d.path).await;
    let mut b = connect(&d.path).await;
    // Both are connected and served before the play.
    for c in [&mut a, &mut b] {
        c.send(r#"{"id":1,"cmd":"status"}"#).await;
        c.reply(1).await;
    }
    a.send(&format!(
        r#"{{"id":2,"cmd":"play","args":{{"videoId":"{SONG}"}}}}"#
    ))
    .await;
    assert_eq!(a.reply(2).await, json!({"id": 2, "ok": true, "data": {}}));
    for c in [&mut a, &mut b] {
        let v = c.event("state").await;
        assert_eq!(v["state"], "buffering");
        assert_eq!(v["videoId"], SONG);
        assert_eq!(v["title"], Value::Null);
    }
    // Commands from the second one work too.
    b.send(r#"{"id":3,"cmd":"volume","args":{"percent":40}}"#)
        .await;
    assert_eq!(b.reply(3).await["ok"], true);
    for c in [&mut a, &mut b] {
        assert_eq!(c.event("state").await["volume"], 40);
    }
}

#[tokio::test]
async fn quit_command_exits_cleanly() {
    let d = daemon(true);
    let mut c = connect(&d.path).await;
    c.send(r#"{"id":9,"cmd":"quit"}"#).await;
    assert_eq!(c.reply(9).await, json!({"id": 9, "ok": true, "data": {}}));
    let exit = tokio::time::timeout(WAIT, d.task).await.unwrap().unwrap();
    assert_eq!(exit, Exit::Quit);
    c.until_closed().await;
}

/// A request without a newline before the client half-closes still gets its reply: what
/// `printf '{"id":1,"cmd":"status"}' | socat - UNIX-CONNECT:...` does.
#[tokio::test]
async fn reply_goes_out_after_the_client_half_closes() {
    let d = daemon(true);
    let mut c = connect(&d.path).await;
    c.write
        .write_all(br#"{"id":1,"cmd":"status"}"#)
        .await
        .unwrap();
    c.write.shutdown().await.unwrap();
    assert_eq!(c.reply(1).await["ok"], true);
    c.until_closed().await;
}

/// The paused clock jumps straight to each timer, so these take no real time. The client
/// holds the clock while it talks (`hold_clock`); the window between sending a command and
/// seeing its reply is checked anyway, as the command's moment lies somewhere in it. The
/// quit must come one idle limit after a moment in that window.
struct Window {
    sent: Instant,
    answered: Instant,
}

impl Window {
    async fn around(c: &mut Client, id: u64, line: &str) -> Window {
        let sent = Instant::now();
        c.send(line).await;
        c.reply(id).await;
        Window {
            sent,
            answered: Instant::now(),
        }
    }

    fn assert_quit_after(&self, quit_at: Instant, limit: Duration) {
        assert!(
            quit_at >= self.sent + limit && quit_at <= self.answered + limit,
            "quit {:?} after the command was sent, {:?} after its reply; limit {limit:?}",
            quit_at - self.sent,
            quit_at - self.answered,
        );
    }
}

async fn idle_exit(on_ac: bool, limit: Duration) {
    let d = daemon(on_ac);
    let mut c = connect(&d.path).await;
    let w = Window::around(&mut c, 1, r#"{"id":1,"cmd":"status"}"#).await;
    assert_eq!(d.task.await.unwrap(), Exit::Idle);
    w.assert_quit_after(Instant::now(), limit);
}

#[tokio::test(start_paused = true)]
async fn daemon_quits_after_5_min_idle_on_ac() {
    idle_exit(true, 5 * MIN).await;
}

#[tokio::test(start_paused = true)]
async fn daemon_quits_after_2_min_idle_on_battery() {
    idle_exit(false, 2 * MIN).await;
}

#[tokio::test(start_paused = true)]
async fn daemon_never_quits_while_playing() {
    let d = daemon(true);
    let mut c = connect(&d.path).await;
    c.send(&format!(
        r#"{{"id":1,"cmd":"play","args":{{"videoId":"{SONG}"}}}}"#
    ))
    .await;
    assert_eq!(c.event("state").await["state"], "buffering");
    tokio::time::sleep(60 * MIN).await;
    assert!(!d.task.is_finished(), "quit while a song was buffering");

    // Paused: the idle clock starts from the pause.
    let w = Window::around(&mut c, 2, r#"{"id":2,"cmd":"pause"}"#).await;
    assert_eq!(d.task.await.unwrap(), Exit::Idle);
    w.assert_quit_after(Instant::now(), 5 * MIN);
}

#[tokio::test(start_paused = true)]
async fn commands_restart_the_idle_clock() {
    let d = daemon(true);
    let mut c = connect(&d.path).await;
    let first = Window::around(&mut c, 1, r#"{"id":1,"cmd":"status"}"#).await;
    tokio::time::sleep_until(first.answered + 4 * MIN).await;
    assert!(!d.task.is_finished(), "quit 4 minutes after a command");
    let w = Window::around(
        &mut c,
        2,
        r#"{"id":2,"cmd":"volume","args":{"percent":10}}"#,
    )
    .await;
    tokio::time::sleep_until(w.answered + 4 * MIN).await;
    assert!(
        !d.task.is_finished(),
        "quit 4 minutes after the second command"
    );
    assert_eq!(d.task.await.unwrap(), Exit::Idle);
    w.assert_quit_after(Instant::now(), 5 * MIN);
}

#[tokio::test]
async fn commands_reach_the_engine_with_volume_as_a_fraction() {
    let mut f = fake_engine(paused_status(SONG));
    let mut c = connect(&f.path).await;
    c.send(r#"{"id":1,"cmd":"volume","args":{"percent":25}}"#)
        .await;
    c.send(r#"{"id":2,"cmd":"seek","args":{"seconds":42.5}}"#)
        .await;
    c.send(r#"{"id":3,"cmd":"toggle"}"#).await;
    c.send(r#"{"id":4,"cmd":"status"}"#).await;
    for id in 1..=3 {
        assert_eq!(c.reply(id).await["ok"], true);
    }
    let status = c.reply(4).await;
    assert_eq!(status["data"]["volume"], 50);
    assert_eq!(status["data"]["position"], 1.5);
    let got: Vec<String> = (0..3)
        .map(|_| format!("{:?}", f.commands.try_recv().unwrap()))
        .collect();
    assert_eq!(got, ["Volume(0.25)", "Seek(42.5)", "Toggle"]);
    f.serve.abort();
}

/// Task 8 carry: a client that fell behind the engine's events gets a fresh `state`
/// instead of being dropped.
#[tokio::test]
async fn lagged_client_gets_a_fresh_state() {
    let f = fake_engine(paused_status("LAGLAGLAG_1"));
    let mut c = connect(&f.path).await;
    c.send(r#"{"id":1,"cmd":"status"}"#).await;
    c.reply(1).await;
    // No await in between: the client's task can't run, so it falls far behind.
    for i in 0..200 {
        let _ = f.events.send(EngineEvent::Position {
            seconds: f64::from(i),
            seeked: false,
        });
    }
    let v = c.event("state").await;
    assert_eq!(v["videoId"], "LAGLAGLAG_1");
    assert_eq!(v["state"], "paused");
    // And it carries on with the events still buffered.
    let p = c.event("position").await;
    assert!(p["seconds"].as_f64().unwrap() >= 136.0, "{p}");
    f.serve.abort();
}

/// A client that stops reading is dropped once its queue fills; the others carry on.
#[tokio::test]
async fn client_that_stops_reading_is_dropped() {
    let f = fake_engine(paused_status(SONG));
    let mut stuck = connect(&f.path).await;
    let mut live = connect(&f.path).await;
    for c in [&mut stuck, &mut live] {
        c.send(r#"{"id":1,"cmd":"status"}"#).await;
        c.reply(1).await;
    }
    // The live client is read all along; it keeps the last position it saw.
    let reader = tokio::spawn(async move {
        let mut last = 0.0;
        while let Some(v) = live.next().await {
            if let Some(s) = v["seconds"].as_f64() {
                last = s;
                if s >= 49_999.0 {
                    break;
                }
            }
        }
        last
    });
    for i in 0..50_000 {
        let _ = f.events.send(EngineEvent::Position {
            seconds: f64::from(i),
            seeked: false,
        });
        if i % 16 == 0 {
            tokio::task::yield_now().await;
        }
    }
    assert_eq!(reader.await.unwrap(), 49_999.0);
    // The stuck one: its kernel buffer and queue filled, so the server hung up. Reading now
    // drains what was sent and then ends.
    tokio::time::timeout(Duration::from_secs(30), stuck.until_closed())
        .await
        .expect("the stuck client was never dropped");
    f.serve.abort();
}

/// The engine task died (a panic drops its command receiver) while the last state was
/// playing, so no idle timer runs: the hub must still notice and end, or the daemon would
/// stay up for good answering "the engine stopped".
#[tokio::test]
async fn engine_gone_while_playing_ends_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let power = power(true);
    let (listener, _path) = listener(dir.path());
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (events, _) = broadcast::channel(64);
    let options = Options {
        power_supply_root: power.path().to_path_buf(),
        ..Options::default()
    };
    let serve = tokio::spawn(control::serve(listener, cmd_tx, events.clone(), options));
    // Wait until the hub listens, so it really sees the playing state.
    while events.receiver_count() == 0 {
        tokio::task::yield_now().await;
    }
    events
        .send(EngineEvent::State(Status {
            state: PlayState::Playing,
            ..paused_status(SONG)
        }))
        .unwrap();
    tokio::task::yield_now().await;
    drop(cmd_rx);
    let exit = tokio::time::timeout(WAIT, serve)
        .await
        .expect("the hub never noticed the engine was gone")
        .unwrap();
    assert_eq!(exit, Exit::EngineGone);
}

/// Every queue command, in its wire form, reaches the engine as the matching command.
#[tokio::test]
async fn queue_commands_reach_the_engine() {
    let mut f = fake_engine(paused_status(SONG));
    let mut c = connect(&f.path).await;
    let lines = [
        r#"{"id":1,"cmd":"next"}"#.to_string(),
        r#"{"id":2,"cmd":"previous"}"#.into(),
        format!(
            r#"{{"id":3,"cmd":"queue.add","args":{{"at":"next","songs":[{{"videoId":"{SONG}",
            "title":"Song","artists":["A","B"],"album":"Album",
            "thumbnail":"https://i.ytimg.com/vi/x/hqdefault.jpg","lengthSeconds":213}}]}}}}"#
        )
        .replace('\n', ""),
        r#"{"id":4,"cmd":"queue.add","args":{"videoIds":["AAAAAAAAAAA","BBBBBBBBBBB"],"at":"end"}}"#.into(),
        r#"{"id":5,"cmd":"queue.remove","args":{"queueId":4}}"#.into(),
        r#"{"id":6,"cmd":"queue.jump","args":{"queueId":18446744073709551615}}"#.into(),
        r#"{"id":7,"cmd":"queue.move","args":{"queueId":2,"index":0}}"#.into(),
        r#"{"id":8,"cmd":"shuffle","args":{"on":true}}"#.into(),
        r#"{"id":9,"cmd":"repeat","args":{"mode":"all"}}"#.into(),
        r#"{"id":10,"cmd":"play","args":{"playlistId":"OLAK5uy_x-Y","index":2}}"#.into(),
        format!(
            r#"{{"id":11,"cmd":"play","args":{{"videoId":"{SONG}","playlistId":"RDAMVM{SONG}"}}}}"#
        ),
    ];
    for line in &lines {
        c.send(line).await;
    }
    for id in 1..=lines.len() as u64 {
        assert_eq!(
            c.reply(id).await,
            json!({"id": id, "ok": true, "data": {}}),
            "{id}"
        );
    }
    let mut got = Vec::new();
    for _ in 0..lines.len() {
        got.push(f.commands.try_recv().unwrap());
    }
    assert!(matches!(got[0], EngineCmd::Next));
    assert!(matches!(got[1], EngineCmd::Previous));
    match &got[2] {
        EngineCmd::QueueAdd { songs, at } => {
            assert_eq!(songs, &vec![full_song(SONG)]);
            assert_eq!(*at, AddAt::Next);
        }
        other => panic!("{other:?}"),
    }
    match &got[3] {
        EngineCmd::QueueAdd { songs, at } => {
            let ids: Vec<&str> = songs.iter().map(|s| s.video_id.as_str()).collect();
            assert_eq!(ids, ["AAAAAAAAAAA", "BBBBBBBBBBB"]);
            assert!(
                songs
                    .iter()
                    .all(|s| s.title.is_empty() && s.album.is_none())
            );
            assert_eq!(*at, AddAt::End);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(got[4], EngineCmd::QueueRemove(4)));
    assert!(matches!(got[5], EngineCmd::QueueJump(u64::MAX)));
    assert!(matches!(got[6], EngineCmd::QueueMove { id: 2, index: 0 }));
    assert!(matches!(got[7], EngineCmd::Shuffle(true)));
    assert!(matches!(got[8], EngineCmd::Repeat(Repeat::All)));
    match &got[9] {
        EngineCmd::Play {
            video_id: None,
            playlist_id: Some(p),
            index: Some(2),
            start_seconds,
        } => {
            assert_eq!(p, "OLAK5uy_x-Y");
            assert_eq!(*start_seconds, 0.0);
        }
        other => panic!("{other:?}"),
    }
    match &got[10] {
        EngineCmd::Play {
            video_id: Some(v),
            playlist_id: Some(p),
            index: None,
            ..
        } => assert_eq!((v.as_str(), p.as_str()), (SONG, &*format!("RDAMVM{SONG}"))),
        other => panic!("{other:?}"),
    }
    // Bad input never reaches the engine.
    c.send(r#"{"id":20,"cmd":"queue.add","args":{"videoIds":["../etc/pass"]}}"#)
        .await;
    c.send(r#"{"id":21,"cmd":"repeat","args":{"mode":"Playlist"}}"#)
        .await;
    for id in [20, 21] {
        assert_eq!(c.reply(id).await["error"]["code"], "bad_request");
    }
    assert!(f.commands.try_recv().is_err());
    f.serve.abort();
}

#[tokio::test]
async fn queue_get_replies_with_the_queue() {
    let f = fake_engine(paused_status(SONG));
    let mut c = connect(&f.path).await;
    c.send(r#"{"id":1,"cmd":"queue.get"}"#).await;
    assert_eq!(
        c.reply(1).await,
        json!({"id": 1, "ok": true, "data": {
            "currentId": 3, "shuffle": true, "repeat": "one",
            "items": [
                {"queueId": 3, "videoId": SONG, "title": "Song", "artists": ["A", "B"],
                 "album": "Album", "thumbnail": "https://i.ytimg.com/vi/x/hqdefault.jpg",
                 "lengthSeconds": 213},
                {"queueId": 9, "videoId": "AAAAAAAAAAA", "title": null, "artists": [],
                 "album": null, "thumbnail": null, "lengthSeconds": null}]}})
    );
    f.serve.abort();
}

/// A client that fell behind may have missed a queue change too: it gets a fresh queue
/// along with the fresh state.
#[tokio::test]
async fn lagged_client_gets_a_fresh_queue() {
    let f = fake_engine(paused_status(SONG));
    let mut c = connect(&f.path).await;
    c.send(r#"{"id":1,"cmd":"status"}"#).await;
    c.reply(1).await;
    for i in 0..200 {
        let _ = f.events.send(EngineEvent::Position {
            seconds: f64::from(i),
            seeked: false,
        });
    }
    let q = c.event("queue").await;
    assert_eq!(q["currentId"], 3);
    assert_eq!(q["items"].as_array().unwrap().len(), 2);
    f.serve.abort();
}

/// The queue end to end on the real engine (its songs never load: the resolver hangs, so a
/// jumped-to song stays buffering): every queue command round trips, and both clients see
/// every `queue` event.
#[tokio::test]
async fn queue_round_trips_and_events_reach_both_clients() {
    let d = daemon(true);
    let mut a = connect(&d.path).await;
    let mut b = connect(&d.path).await;
    for c in [&mut a, &mut b] {
        c.send(r#"{"id":1,"cmd":"status"}"#).await;
        c.reply(1).await;
    }
    let full = serde_json::to_string(&json!({"id": 2, "cmd": "queue.add", "args": {"songs": [
        {"videoId": SONG, "title": "Song", "artists": ["A", "B"], "album": "Album",
         "thumbnail": "https://i.ytimg.com/vi/x/hqdefault.jpg", "lengthSeconds": 213}]}}))
    .unwrap();
    a.send(&full).await;
    a.send(r#"{"id":3,"cmd":"queue.add","args":{"videoIds":["AAAAAAAAAAA","BBBBBBBBBBB"]}}"#)
        .await;
    // The sender's replies and events interleave: keep the queue events while waiting.
    let mut a_queues = Vec::new();
    let mut replies = 0;
    while replies < 2 || a_queues.len() < 2 {
        let v = a.next().await.unwrap();
        if v["event"] == "queue" {
            a_queues.push(v);
        } else if v.get("id").is_some() {
            assert_eq!(v["ok"], true, "{v}");
            replies += 1;
        }
    }

    // Both clients get both queue events; the second has all three songs, in order.
    let mut b_queues = Vec::new();
    for _ in 0..2 {
        b_queues.push(b.event("queue").await);
    }
    assert_eq!(a_queues, b_queues);
    assert_eq!(a_queues[0]["items"].as_array().unwrap().len(), 1);
    let mut last = Value::Null;
    for q in [&a_queues[1], &b_queues[1]] {
        last = q.clone();
        let ids: Vec<&str> = last["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["videoId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [SONG, "AAAAAAAAAAA", "BBBBBBBBBBB"]);
        assert_eq!(last["items"][0]["album"], "Album");
        assert_eq!(last["items"][1]["title"], Value::Null);
    }
    let qid = |v: &Value, i: usize| v["items"][i]["queueId"].as_u64().unwrap();
    let (first, second, third) = (qid(&last, 0), qid(&last, 1), qid(&last, 2));
    assert!(first >= 1 && first < second && second < third, "{last}");

    // queue.get answers the same queue.
    a.send(r#"{"id":4,"cmd":"queue.get"}"#).await;
    let got = a.reply(4).await;
    assert_eq!(got["data"]["items"], last["items"]);

    // Jump: the song shows at once from its queue item, album and queue id included.
    a.send(&format!(
        r#"{{"id":5,"cmd":"queue.jump","args":{{"queueId":{first}}}}}"#
    ))
    .await;
    assert_eq!(a.reply(5).await["ok"], true);
    let s = loop {
        let s = a.event("state").await;
        if s["queueId"] == first {
            break s;
        }
    };
    assert_eq!(s["videoId"], SONG);
    assert_eq!(s["title"], "Song");
    assert_eq!(s["album"], "Album");
    assert_eq!(s["artist"], "A, B");

    // Move the last song to the front.
    a.send(&format!(
        r#"{{"id":6,"cmd":"queue.move","args":{{"queueId":{third},"index":0}}}}"#
    ))
    .await;
    assert_eq!(a.reply(6).await["ok"], true);
    let q = b.event("queue").await;
    let q = if q["items"][0]["queueId"] == third {
        q
    } else {
        // b may still hold the jump's queue event.
        b.event("queue").await
    };
    assert_eq!(qid(&q, 0), third, "{q}");

    // Shuffle and repeat show in both the queue event and the state.
    a.send(r#"{"id":7,"cmd":"shuffle","args":{"on":true}}"#)
        .await;
    assert_eq!(a.reply(7).await["ok"], true);
    a.send(r#"{"id":8,"cmd":"repeat","args":{"mode":"all"}}"#)
        .await;
    assert_eq!(a.reply(8).await["ok"], true);
    a.send(r#"{"id":9,"cmd":"status"}"#).await;
    let s = a.reply(9).await;
    assert_eq!(s["data"]["shuffle"], true);
    assert_eq!(s["data"]["repeat"], "all");
    a.send(r#"{"id":10,"cmd":"queue.get"}"#).await;
    let q = a.reply(10).await;
    assert_eq!(q["data"]["shuffle"], true);
    assert_eq!(q["data"]["repeat"], "all");
    assert_eq!(q["data"]["currentId"], first);

    // Next moves off the current song; Previous (under 3 s in) comes back to it.
    a.send(r#"{"id":11,"cmd":"next"}"#).await;
    assert_eq!(a.reply(11).await["ok"], true);
    let s = loop {
        let s = a.event("state").await;
        if s["queueId"] != first {
            break s;
        }
    };
    assert_ne!(s["queueId"], Value::Null);
    a.send(r#"{"id":12,"cmd":"previous"}"#).await;
    assert_eq!(a.reply(12).await["ok"], true);
    loop {
        if a.event("state").await["queueId"] == first {
            break;
        }
    }

    // Remove: gone from the queue.
    a.send(&format!(
        r#"{{"id":13,"cmd":"queue.remove","args":{{"queueId":{second}}}}}"#
    ))
    .await;
    assert_eq!(a.reply(13).await["ok"], true);
    a.send(r#"{"id":14,"cmd":"queue.get"}"#).await;
    let q = a.reply(14).await;
    let ids: Vec<u64> = q["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["queueId"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&second));

    // An id that isn't in the queue: taken, and the engine says why it did nothing.
    a.send(r#"{"id":15,"cmd":"queue.jump","args":{"queueId":999999}}"#)
        .await;
    assert_eq!(a.reply(15).await["ok"], true);
    let e = a.event("error").await;
    assert!(!e["message"].as_str().unwrap().is_empty());
}
