//! Resume across a restart, end to end: every way a daemon quits (a signal, the socket's
//! `quit`, idle) writes `state.json`, and the next daemon comes back paused on the saved song
//! at the saved second.
//!
//! The binary tests run `ytmfast daemon --null-sink` as its own process with every folder in
//! a temp dir and the D-Bus address pointing at nothing (as in `daemon_signal.rs`): a restored
//! song is never loaded until a play, so nothing here reads a keyring or the network. The
//! idle test runs the library's engine and socket on a paused clock instead (5 minutes of
//! idle in no real time). Never the real account or speakers.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use ytmfast::audio::player::AudioPlayer;
use ytmfast::audio::sink::NullSink;
use ytmfast::control::{self, Exit, Options};
use ytmfast::engine::{Engine, QueueSource};
use ytmfast::error::Error;
use ytmfast::innertube::{NextPage, NextRequest, SongItem};
use ytmfast::queue::Repeat;
use ytmfast::state::{self, Saved, Writer};
use ytmfast::streams::{Resolver, Stream};

const WAIT: Duration = Duration::from_secs(10);
const SONG: &str = "dQw4w9WgXcQ";

fn song(id: &str, title: &str) -> SongItem {
    SongItem {
        video_id: id.into(),
        title: title.into(),
        artists: vec!["Artist".into()],
        album: Some("Album".into()),
        album_id: "MPREb_example".into(),
        thumbnail: Some(format!("https://i.ytimg.com/vi/{id}/hq.jpg")),
        length_seconds: 240,
        playlist_id: Some("OLAK5uy_example".into()),
    }
}

/// A two-song queue, the second current, 42.5 s in.
fn saved() -> Saved {
    Saved {
        queue: vec![song("AAAAAAAAAAA", "First"), song(SONG, "Second")],
        current_index: 1,
        position: 42.5,
        volume: 0.3,
        shuffle: true,
        original_order: Some(vec![1, 0]),
        repeat: Repeat::All,
        source_playlist: Some("OLAK5uy_example".into()),
        continuation: None,
        saved_unix: 1,
        ..Saved::default()
    }
}

fn state_dir(root: &Path) -> PathBuf {
    root.join("state/ytmfast")
}

/// Writes a saved state where the daemon will look, in a 0700 folder as it makes them.
fn seed(root: &Path, s: &Saved) {
    use std::os::unix::fs::DirBuilderExt;
    let dir = state_dir(root);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    state::save(&dir, s).unwrap();
}

fn spawn_daemon(root: &Path) -> Child {
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ytmfast"))
        .args(["daemon", "--null-sink"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("XDG_RUNTIME_DIR", &run)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/no-bus", root.display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// One request on a fresh connection, retried until the daemon serves; its reply.
fn request(root: &Path, line: &str) -> Value {
    let socket = root.join("run/ytmfast/socket");
    let deadline = Instant::now() + WAIT;
    let mut s = loop {
        match UnixStream::connect(&socket) {
            Ok(s) => break s,
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "the daemon never started serving"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    s.set_read_timeout(Some(WAIT)).unwrap();
    s.write_all(line.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    // Replies and events share the connection: skip events until the reply.
    let mut lines = BufReader::new(s).lines();
    loop {
        let v: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        if v.get("id").is_some() {
            return v;
        }
    }
}

fn status(root: &Path) -> Value {
    let v = request(root, r#"{"id":1,"cmd":"status"}"#);
    assert_eq!(v["ok"], true, "{v}");
    v["data"].clone()
}

fn wait_exit(child: &mut Child) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "exit: {status:?}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon did not stop within {WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sigterm(child: &Child) {
    // SAFETY: kill with a pid we spawned and still own (not yet reaped).
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn restart_resumes_paused_at_the_saved_second() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path(), &saved());
    for round in 0..2 {
        let mut child = spawn_daemon(root.path());
        let s = status(root.path());
        assert_eq!(s["state"], "paused", "round {round}: {s}");
        assert_eq!(s["videoId"], SONG);
        assert_eq!(s["title"], "Second");
        assert_eq!(s["position"], 42.5);
        assert_eq!(s["volume"], 30);
        // Ruling P15: the album link comes back with the song.
        assert_eq!(s["albumId"], "MPREb_example", "round {round}: {s}");
        // What `systemctl --user restart` sends: the last save, then a clean exit.
        sigterm(&child);
        wait_exit(&mut child);
        let file = state_dir(root.path()).join(state::FILE_NAME);
        assert_eq!(mode(&file), 0o600);
        let back = state::load(&state_dir(root.path())).unwrap();
        assert!(back.saved_unix > 1, "written again on the way out");
        assert_eq!(back.position, 42.5);
        assert_eq!(back.queue, saved().queue, "the shuffled order");
        assert_eq!(back.current_index, 1);
        assert_eq!(back.original_order, Some(vec![1, 0]));
        assert!(back.shuffle);
        assert_eq!(back.repeat, Repeat::All);
        assert_eq!(back.volume, 0.3);
        assert_eq!(back.source_playlist.as_deref(), Some("OLAK5uy_example"));
    }
}

#[test]
fn socket_quit_saves() {
    let root = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(root.path());
    // Nothing saved yet: nothing to show.
    assert_eq!(status(root.path())["state"], "stopped");
    let v = request(
        root.path(),
        r#"{"id":2,"cmd":"volume","args":{"percent":25}}"#,
    );
    assert_eq!(v["ok"], true, "{v}");
    let v = request(root.path(), r#"{"id":3,"cmd":"quit"}"#);
    assert_eq!(v["ok"], true, "{v}");
    wait_exit(&mut child);
    let back = state::load(&state_dir(root.path())).unwrap();
    assert_eq!(back.volume, 0.25);
    assert!(back.queue.is_empty(), "so a play starts Liked songs");
}

#[test]
fn a_corrupt_state_file_is_kept_and_the_daemon_starts_fresh() {
    let root = tempfile::tempdir().unwrap();
    seed(root.path(), &saved());
    let dir = state_dir(root.path());
    std::fs::write(dir.join(state::FILE_NAME), "{ half a file").unwrap();
    let mut child = spawn_daemon(root.path());
    assert_eq!(status(root.path())["state"], "stopped");
    sigterm(&child);
    wait_exit(&mut child);
    assert_eq!(
        std::fs::read_to_string(dir.join("state.json.bad")).unwrap(),
        "{ half a file"
    );
}

/// Never answers; counts nothing because nothing may be asked before a play.
struct Hang;

#[async_trait]
impl Resolver for Hang {
    async fn resolve(&self, _: &str) -> Result<Stream, Error> {
        panic!("a restored song was resolved before any play")
    }
    async fn resolve_fresh(&self, id: &str) -> Result<Stream, Error> {
        self.resolve(id).await
    }
}

#[async_trait]
impl QueueSource for Hang {
    async fn next(&self, _: NextRequest) -> Result<NextPage, Error> {
        panic!("a queue was fetched before any play")
    }
    async fn song_next(&self, _: &str) -> Result<ytmfast::innertube::SongNext, Error> {
        std::future::pending().await
    }
    async fn like(&self, _: &str, _: ytmfast::browse::LikeStatus) -> Result<(), Error> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn idle_quit_saves() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let power = dir.path().join("power");
    std::fs::create_dir_all(power.join("AC")).unwrap();
    std::fs::write(power.join("AC/type"), "Mains\n").unwrap();
    std::fs::write(power.join("AC/online"), "1\n").unwrap();
    let (std_listener, _bound) =
        control::bind_socket(&dir.path().join(control::SOCKET_NAME)).unwrap();
    let listener = tokio::net::UnixListener::from_std(std_listener).unwrap();

    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let (mut engine, cmds, events) = Engine::new(Arc::new(Hang), Arc::new(Hang), player);
    engine.restore(saved());
    engine.save_with(Writer::spawn(state.clone()));
    let options = Options {
        power_supply_root: power,
        ..Options::default()
    };
    let exit = control::run(
        listener,
        engine,
        cmds,
        events,
        options,
        std::future::pending(),
    )
    .await;
    // A restored song is paused: paused counts as idle, so the engine quits after 5 minutes
    // on mains rather than holding the daemon up for ever.
    assert_eq!(exit, Exit::Idle);
    // The paused clock skips the last save's 2 s wait at once, so the write (on its own
    // thread, in real time) may land just after: wait for it in real time.
    let deadline = Instant::now() + WAIT;
    let back = loop {
        if let Some(s) = state::load(&state).filter(|s| s.saved_unix > 1) {
            break s;
        }
        assert!(Instant::now() < deadline, "no save on idle quit");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(back.position, 42.5);
    assert_eq!(back.queue[back.current_index].video_id, SONG);
}
