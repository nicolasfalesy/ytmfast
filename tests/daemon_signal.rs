//! `ytmfast daemon` stops cleanly on SIGTERM (what `systemctl stop` sends) and SIGINT: it
//! quits the engine, removes the socket it bound itself, and exits 0.
//!
//! The daemon runs as its own process with every folder in a temp dir and the D-Bus address
//! pointing at nothing, so it never reaches the real keyring (it reads none until a song is
//! asked for, and these tests ask for none) or the real socket; `--null-sink` keeps it off
//! the speakers.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

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

/// Waits for the daemon to answer `status`: by then its signal handlers are in place.
fn wait_until_serving(socket: &Path) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(mut s) = UnixStream::connect(socket) {
            s.set_read_timeout(Some(WAIT)).unwrap();
            s.write_all(b"{\"id\":1,\"cmd\":\"status\"}\n").unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            assert!(line.contains("\"ok\":true"), "{line}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never started serving"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stops_cleanly_on(signal: libc::c_int) {
    let root = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(root.path());
    let socket = root.path().join("run/ytmfast/socket");
    wait_until_serving(&socket);

    // SAFETY: kill with a pid we spawned and still own (not yet reaped).
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon did not stop within {WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "exit: {status:?}");
    assert!(!socket.exists(), "the socket was left behind");
}

#[test]
fn sigterm_stops_the_daemon_cleanly() {
    stops_cleanly_on(libc::SIGTERM);
}

#[test]
fn sigint_stops_the_daemon_cleanly() {
    stops_cleanly_on(libc::SIGINT);
}

#[test]
fn start_sweeps_a_dead_engines_cookie_folders() {
    use std::os::unix::fs::DirBuilderExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("run/ytmfast");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    // A process that has exited and been collected: its pid is free.
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let stale = dir.join(format!("yt-dlp-{dead}-0"));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("cookies.txt"), "fake").unwrap();

    let mut child = spawn_daemon(root.path());
    wait_until_serving(&dir.join("socket"));
    let swept = !stale.exists();
    // SAFETY: kill with a pid we spawned and still own (not yet reaped).
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let _ = child.wait();
    assert!(swept, "the stale cookie folder is still there");
}
