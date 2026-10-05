//! Stopping a running engine from outside, for `ytmfast import-session`.
//!
//! A running engine holds the old session in memory and writes it back to the keyring at its
//! next cookie rotation, over the one just imported. So the import stops it: the next play
//! starts a new engine, which reads the new session.
//!
//! Connecting is only tried when an engine process is actually running. With the systemd
//! socket unit, any connection to the socket starts an engine, and the import must never
//! start one just to stop it again.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long the engine gets to answer `quit`, and then to exit.
pub const STOP_WAIT: Duration = Duration::from_secs(5);

/// The request line: the protocol's `quit` with id 1.
const QUIT: &[u8] = b"{\"id\":1,\"cmd\":\"quit\"}\n";

/// The user's running engines (`ytmfast daemon`), from `proc_root` (`/proc` in production):
/// processes owned by `uid` whose program is named `ytmfast` and whose first argument is
/// `daemon`, other than `own_pid`.
pub fn daemon_pids(proc_root: &Path, uid: u32, own_pid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            if pid == own_pid || e.metadata().ok()?.uid() != uid {
                return None;
            }
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            is_daemon(&cmdline).then_some(pid)
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// `ytmfast daemon …`, by the program's file name (it may be run by its full path).
fn is_daemon(cmdline: &[u8]) -> bool {
    let mut args = cmdline.split(|&b| b == 0);
    let program = args.next().unwrap_or_default();
    let name = Path::new(std::ffi::OsStr::from_bytes(program)).file_name();
    name.is_some_and(|n| n.as_bytes() == b"ytmfast") && args.next() == Some(b"daemon")
}

/// Sends `quit` to the engine at `socket` and waits up to `timeout` for its reply. True when
/// it answered (events may come first; the reply is the line with id 1).
pub fn ask_to_quit(socket: &Path, timeout: Duration) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    if stream.set_write_timeout(Some(timeout)).is_err() || stream.write_all(QUIT).is_err() {
        return false;
    }
    let mut lines = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {
                let reply: Option<serde_json::Value> = serde_json::from_str(&line).ok();
                if reply.is_some_and(|r| r["id"] == 1) {
                    return true;
                }
            }
        }
    }
}

/// Waits up to `timeout` for one of `pids` to be gone from `proc_root`. True when one is (or
/// there were none). One, not all: the engine that answered on our socket is one of them,
/// but which one isn't known, and another (on another runtime folder, as in tests) may stay.
pub fn wait_gone(proc_root: &Path, pids: &[u32], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let one_gone = pids.is_empty() || pids.iter().any(|pid| exited(proc_root, *pid));
        if one_gone {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// True when `pid` is gone, or has exited and only waits for its parent to collect it (a
/// zombie, state `Z`, or `X` while it is being removed): it no longer holds the session.
fn exited(proc_root: &Path, pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")) else {
        return !proc_root.join(pid.to_string()).exists();
    };
    // The state is the field after the `(comm)`, which may itself hold spaces or `)`.
    stat.rsplit_once(") ")
        .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    fn uid() -> u32 {
        // SAFETY: getuid has no preconditions and can't fail.
        unsafe { libc::getuid() }
    }

    fn process(root: &Path, pid: u32, args: &[&str]) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir(&dir).unwrap();
        let mut cmdline = Vec::new();
        for a in args {
            cmdline.extend_from_slice(a.as_bytes());
            cmdline.push(0);
        }
        std::fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn finds_only_running_engines() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        process(r, 100, &["ytmfast", "daemon"]);
        process(r, 101, &["/usr/bin/ytmfast", "daemon", "--null-sink"]);
        process(r, 102, &["ytmfast", "play", "dQw4w9WgXcQ"]);
        process(r, 103, &["ytmfast", "import-session"]);
        process(r, 104, &["not-ytmfast", "daemon"]);
        process(r, 105, &["sh", "-c", "ytmfast daemon"]);
        // The import itself is never counted, whatever it looks like.
        process(r, 106, &["ytmfast", "daemon"]);
        std::fs::create_dir(r.join("self")).unwrap();
        assert_eq!(daemon_pids(r, uid(), 106), vec![100, 101]);
        // Another user's engine is left alone.
        assert!(daemon_pids(r, uid().wrapping_add(1), 0).is_empty());
        // No /proc: nothing is running, as far as we can tell.
        assert!(daemon_pids(&r.join("missing"), uid(), 0).is_empty());
    }

    #[test]
    fn quit_is_sent_and_its_reply_awaited() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let engine = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = vec![0; QUIT.len()];
            s.read_exact(&mut got).unwrap();
            // An event can come before the reply.
            s.write_all(b"{\"event\":\"position\",\"seconds\":1.0}\n")
                .unwrap();
            s.write_all(b"{\"id\":1,\"ok\":true,\"data\":{}}\n")
                .unwrap();
            got
        });
        assert!(ask_to_quit(&socket, Duration::from_secs(5)));
        assert_eq!(engine.join().unwrap(), QUIT);
    }

    #[test]
    fn nothing_answering_is_not_a_quit() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        // No socket at all.
        assert!(!ask_to_quit(&socket, Duration::from_millis(200)));
        // A socket that takes the line but never answers: given up on at the timeout.
        let listener = UnixListener::bind(&socket).unwrap();
        let started = Instant::now();
        assert!(!ask_to_quit(&socket, Duration::from_millis(200)));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(listener);
    }

    #[test]
    fn waits_for_the_engine_to_exit() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path().to_path_buf();
        process(&r, 100, &["ytmfast", "daemon"]);
        process(&r, 101, &["ytmfast", "daemon"]);
        assert!(!wait_gone(&r, &[100, 101], Duration::from_millis(100)));
        let gone = r.join("100");
        let remover = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::remove_dir_all(gone).unwrap();
        });
        // The one that was asked exits; another engine (another runtime folder) stays.
        assert!(wait_gone(&r, &[100, 101], Duration::from_secs(5)));
        remover.join().unwrap();
        // An engine that exited but whose parent hasn't collected it yet counts as gone.
        std::fs::write(r.join("101/stat"), "101 (ytmfast) S 1 101").unwrap();
        assert!(!wait_gone(&r, &[101], Duration::from_millis(50)));
        std::fs::write(r.join("101/stat"), "101 (ytm fast) x) Z 1 101").unwrap();
        assert!(wait_gone(&r, &[101], Duration::from_millis(50)));
        assert!(wait_gone(&r, &[], Duration::ZERO));
    }
}
