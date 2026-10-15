//! Start-up stays fast: `ytmfast -V` (process start and exit) and a daemon's time from being
//! started to its socket file existing. Release builds only, and only on a machine in the state
//! the limits were measured in (see `perf_common`): these are wall-clock times of a few
//! milliseconds, which a busy or slowed machine changes by far more than 20%.
//!
//! The daemon runs as its own process with every folder in a temp dir and the D-Bus address
//! pointing at nothing (as in `daemon_signal.rs`), on `--null-sink`.

mod perf_common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Milliseconds: the measured value times 1.2.
/// `ytmfast -V`: 0.896 ms measured with hyperfine by the audit (0.83 to 0.92 here) times 1.2.
const VERSION_MEAN_MS: f64 = 1.08;
/// A daemon's start to its socket file: medians of 100 runs were 0.64 to 0.71 ms in five runs
/// here (the audit's first measure, with another way of timing it, was 1.016 ms); the worst
/// times 1.2.
const SOCKET_MEDIAN_MS: f64 = 0.86;

fn ytmfast() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ytmfast"))
}

fn daemon(root: &Path) -> Command {
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let mut c = ytmfast();
    c.args(["daemon", "--null-sink"])
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
        .stderr(Stdio::null());
    c
}

#[test]
fn the_version_flag_runs_within_its_limit() {
    if !perf_common::speed_limits_apply("perf_start version") {
        return;
    }
    let run = || {
        ytmfast()
            .arg("-V")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
    };
    for _ in 0..30 {
        assert!(run().success());
    }
    // The mean of 300 runs, three times over; the lowest of the three counts (a one-off
    // hiccup of the machine should not fail the test, a real slowdown shows in all three).
    let mut means = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        for _ in 0..300 {
            run();
        }
        means.push(t.elapsed().as_secs_f64() * 1000.0 / 300.0);
    }
    let best = means.iter().cloned().fold(f64::MAX, f64::min);
    println!("ytmfast -V: {means:.3?} ms per run");
    assert!(
        best <= VERSION_MEAN_MS,
        "ytmfast -V took {best:.3} ms per run, the limit is {VERSION_MEAN_MS}"
    );
}

#[test]
fn the_daemon_creates_its_socket_within_its_limit() {
    if !perf_common::speed_limits_apply("perf_start socket") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("run/ytmfast/socket");
    let mut times = Vec::new();
    for round in 0..110 {
        // A daemon killed before its signal handlers were in place leaves its socket file.
        let _ = std::fs::remove_file(&socket);
        let t = Instant::now();
        let mut child = daemon(root.path()).spawn().unwrap();
        let ready = loop {
            if socket.exists() {
                break t.elapsed();
            }
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "no socket after 10 s"
            );
            std::hint::spin_loop();
        };
        // SAFETY: kill with a pid we spawned and have not reaped.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
        // The first rounds warm the page cache.
        if round >= 10 {
            times.push(ready.as_secs_f64() * 1000.0);
        }
    }
    let median = perf_common::median(&mut times);
    println!(
        "daemon start to socket file: median {median:.3} ms of {} runs",
        times.len()
    );
    assert!(
        median <= SOCKET_MEDIAN_MS,
        "the daemon took {median:.3} ms to its socket, the limit is {SOCKET_MEDIAN_MS}"
    );
}
