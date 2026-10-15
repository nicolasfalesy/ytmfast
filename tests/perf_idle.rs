//! An idle always-on engine costs nothing: no CPU, (almost) no wakeups, no disk writes, and a
//! small memory footprint. Release builds only (a debug binary is far bigger and slower).
//!
//! The daemon runs as `daemon --null-sink --stay` in its own process with every folder in a
//! temp dir and the D-Bus address pointing at nothing, like `daemon_signal.rs`; no session is
//! loaded, so this is the fresh engine. The counters are the process's own, from /proc, so a
//! busy machine does not change them (the memory numbers do not depend on speed either).
//!
//! Limits are the audit's measured values (2026-10-14: 0 CPU ticks and 0 wakeups on every
//! thread over 146 s, 0 bytes written) plus 20%; memory is this test's own measure plus 20%.
//! Wakeups: a limit of 2 a minute, over this test's window.

#![cfg(not(debug_assertions))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the quiet engine is watched.
const WINDOW: Duration = Duration::from_secs(20);
/// Voluntary context switches (wakeups) allowed in `WINDOW`: 2 a minute, rounded up.
const MAX_WAKEUPS: u64 = 1;
/// Measured by this test on the release build after the audit's fixes: 7,152 KiB PSS and
/// 10,032 KiB RSS (the audit's first measure, with other tools, was 10.7 MB and 13.3 MB).
const MAX_PSS_KIB: u64 = 8_700;
const MAX_RSS_KIB: u64 = 12_100;

fn spawn_daemon(root: &Path) -> Child {
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ytmfast"))
        .args(["daemon", "--null-sink", "--stay"])
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

fn wait_until_serving(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut s) = UnixStream::connect(socket) {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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

/// A field of a /proc file ("Name:  value ..."): the first number after the key.
fn field(path: &str, key: &str) -> u64 {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// utime + stime of the whole process in clock ticks (threads that ended included).
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // After the ")" that ends the command name: state is field 3, utime 14, stime 15.
    let rest = stat.rsplit_once(')').unwrap().1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

/// Voluntary context switches summed over the threads that exist now.
fn wakeups(pid: u32) -> u64 {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
        .map(|t| {
            field(
                &format!("{}/status", t.path().display()),
                "voluntary_ctxt_switches:",
            )
        })
        .sum()
}

#[test]
fn a_fresh_always_on_engine_is_idle_and_small() {
    let root = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(root.path());
    let pid = child.id();
    wait_until_serving(&root.path().join("run/ytmfast/socket"));
    // Start-up work (the first accept, the runtime's threads settling) is not idle time.
    std::thread::sleep(Duration::from_secs(3));

    let (ticks0, wake0) = (cpu_ticks(pid), wakeups(pid));
    let wrote0 = field(&format!("/proc/{pid}/io"), "write_bytes:");
    std::thread::sleep(WINDOW);
    let ticks = cpu_ticks(pid) - ticks0;
    let woke = wakeups(pid).saturating_sub(wake0);
    let wrote = field(&format!("/proc/{pid}/io"), "write_bytes:") - wrote0;
    let pss = field(&format!("/proc/{pid}/smaps_rollup"), "Pss:");
    let rss = field(&format!("/proc/{pid}/smaps_rollup"), "Rss:");
    let threads = std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .count();

    // SAFETY: kill with a pid we spawned and have not reaped.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    child.wait().unwrap();

    println!(
        "idle {WINDOW:?}: {ticks} CPU ticks, {woke} wakeups, {wrote} bytes written; \
         {pss} KiB PSS, {rss} KiB RSS, {threads} threads"
    );
    assert_eq!(ticks, 0, "an idle engine used CPU");
    assert!(
        woke <= MAX_WAKEUPS,
        "{woke} wakeups in {WINDOW:?}, the limit is {MAX_WAKEUPS}"
    );
    assert_eq!(wrote, 0, "an idle engine wrote to the disk");
    assert!(
        pss <= MAX_PSS_KIB,
        "{pss} KiB PSS, the limit is {MAX_PSS_KIB}"
    );
    assert!(
        rss <= MAX_RSS_KIB,
        "{rss} KiB RSS, the limit is {MAX_RSS_KIB}"
    );
}
