//! The yt-dlp fallback: when the own-code path fails, ask `yt-dlp -j` for the link.
//!
//! yt-dlp gets the session from a cookie file in Netscape format, written from the in-memory
//! session only: the file is created 0600 (with that mode, never chmod-ed after) in a fresh
//! 0700 folder under the runtime folder (a tmpfs, so the cookies never touch a disk), and the
//! folder is removed when yt-dlp exits, is killed at the 30 s timeout, or the call is dropped;
//! the last two also kill everything yt-dlp started (its process group).
//! `--ignore-config` and `--no-cookies-from-browser` stop a yt-dlp config file from pointing it
//! at a browser profile instead.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::auth::Session;
use crate::error::Error;
use crate::net;

/// The spec's limit for one yt-dlp run. A normal run takes about 4 s.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Runs yt-dlp. `Streams` holds it as a trait object so tests can fake it.
#[async_trait]
pub trait YtDlp: Send + Sync {
    /// yt-dlp's `-j` answer (the chosen format's details as JSON) for `video_id`, signed in
    /// with `session`.
    async fn info_json(&self, video_id: &str, session: &Session) -> Result<Vec<u8>, Error>;
}

/// The real yt-dlp, run as a child process.
pub struct YtDlpCommand {
    runtime_dir: PathBuf,
    program: PathBuf,
    prefix_args: Vec<OsString>,
    timeout: Duration,
}

impl YtDlpCommand {
    /// Runs `yt-dlp` from `PATH`, with its cookie folders made under `runtime_dir`
    /// (`paths::runtime_dir()` in production).
    pub fn new(runtime_dir: PathBuf) -> YtDlpCommand {
        YtDlpCommand {
            runtime_dir,
            program: "yt-dlp".into(),
            prefix_args: Vec::new(),
            timeout: TIMEOUT,
        }
    }

    /// Runs `program prefix_args… <yt-dlp arguments>` instead, with its own timeout. For
    /// tests: a stand-in script run by `/bin/sh`.
    pub fn with_program(
        mut self,
        program: PathBuf,
        prefix_args: Vec<OsString>,
        timeout: Duration,
    ) -> YtDlpCommand {
        self.program = program;
        self.prefix_args = prefix_args;
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl YtDlp for YtDlpCommand {
    async fn info_json(&self, video_id: &str, session: &Session) -> Result<Vec<u8>, Error> {
        let folder = CookieFolder::create(&self.runtime_dir)
            .map_err(|_| Error::StreamFailed("could not write yt-dlp's cookie file".into()))?;
        let cookies = folder.path.join("cookies.txt");
        write_cookie_file(&cookies, session)
            .map_err(|_| Error::StreamFailed("could not write yt-dlp's cookie file".into()))?;

        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.prefix_args)
            .args(["--ignore-config", "--no-cookies-from-browser"])
            .args(["--no-warnings", "-q", "--cookies"])
            .arg(&cookies)
            .args(["-f", "774/141/bestaudio", "-j"])
            .arg(format!("https://music.youtube.com/watch?v={video_id}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Not read: yt-dlp's messages can quote stream links (ruling R6), and the exit
            // status says enough.
            .stderr(Stdio::null())
            // Its own process group, so stopping it (timeout or cancel) kills yt-dlp's
            // children too: it runs a JS runtime for the same challenges.
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|_| Error::StreamFailed("could not run yt-dlp".into()))?;
        // From here until yt-dlp is reaped, dropping this (timeout, error, or the caller
        // dropping the call when the user skips) kills yt-dlp's whole process group.
        // `kill_on_drop` alone would kill only yt-dlp and orphan the JS runtime it started.
        let mut group = GroupKill(child.id().and_then(|p| i32::try_from(p).ok()));

        let result = tokio::time::timeout(self.timeout, child.wait_with_output()).await;
        let output = match result {
            Ok(Ok(output)) => {
                // Reaped: the group id may be reused from now on, so it must not be killed.
                group.0 = None;
                output
            }
            Ok(Err(_)) => return Err(Error::StreamFailed("yt-dlp failed".into())),
            Err(_) => {
                return Err(Error::StreamFailed(format!(
                    "yt-dlp took over {} s",
                    self.timeout.as_secs()
                )));
            }
        };
        drop(folder);
        if !output.status.success() {
            return Err(Error::StreamFailed(match output.status.code() {
                Some(code) => format!("yt-dlp exited with status {code}"),
                None => "yt-dlp was killed".into(),
            }));
        }
        if output.stdout.len() > net::MAX_ANSWER {
            return Err(Error::StreamFailed("yt-dlp's answer is too large".into()));
        }
        Ok(output.stdout)
    }
}

/// Kills a process group when dropped, unless its id was taken out (`None`).
struct GroupKill(Option<i32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            // SAFETY: kill has no memory-safety preconditions. The group's leader is not
            // reaped yet (the id is cleared once it is), so the id can't name another group.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

/// A fresh 0700 folder for one cookie file, removed with everything in it when dropped (also
/// on an error or a cancelled call).
struct CookieFolder {
    path: PathBuf,
}

impl CookieFolder {
    fn create(runtime_dir: &Path) -> io::Result<CookieFolder> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = runtime_dir.join(format!("yt-dlp-{}-{n}", std::process::id()));
            // Not recursive: an existing folder (a leftover, or someone else's) is never
            // reused, so the cookie file always lands in a folder this call made 0700.
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(CookieFolder { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for CookieFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Removes cookie folders (`yt-dlp-{pid}-{n}`) left in `runtime_dir` by an engine that died
/// without cleaning up (killed, or crashed mid-run): they hold a copy of the session. Run at
/// daemon start. A folder whose process still runs (this one, or another user of the folder)
/// is left alone, and so is anything that isn't a plain folder with that name.
pub fn sweep_stale(runtime_dir: &Path) {
    let Ok(entries) = fs::read_dir(runtime_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(folder_pid) else {
            continue;
        };
        // `file_type` doesn't follow symlinks: a link named like a folder is never followed.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && pid != std::process::id() && !alive(pid) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// The pid in a cookie folder's name, `yt-dlp-{pid}-{n}`.
fn folder_pid(name: &str) -> Option<u32> {
    let (pid, n) = name.strip_prefix("yt-dlp-")?.split_once('-')?;
    n.parse::<u64>().ok()?;
    pid.parse().ok().filter(|p| *p > 0)
}

/// True while process `pid` exists (`EPERM` means it does, as someone else's).
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Writes `session` as a Netscape cookie file, the format `--cookies` reads. Created with
/// mode 0600 and `create_new`, so it is never readable by others, not even for a moment.
fn write_cookie_file(path: &Path, session: &Session) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(netscape_cookies(session).as_bytes())?;
    f.flush()
}

/// One line per cookie: domain, include-subdomains, path, secure, expiry (0 = session
/// cookie), name, value, tab-separated. A cookie with a tab or line break anywhere is skipped:
/// it could otherwise add lines of its own to the file.
fn netscape_cookies(session: &Session) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in &session.cookies {
        let breaks_line = [&c.domain, &c.path, &c.name, &c.value]
            .iter()
            .any(|f| f.contains(['\t', '\n', '\r']));
        // An empty domain, path or name would make a line the loader rejects.
        let incomplete = c.domain.is_empty() || c.path.is_empty() || c.name.is_empty();
        if breaks_line || incomplete {
            continue;
        }
        let flag = |b: bool| if b { "TRUE" } else { "FALSE" };
        let expires = c.expires_utc.unwrap_or(0).max(0);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            c.domain,
            flag(c.domain.starts_with('.')),
            c.path,
            flag(c.secure),
            expires,
            c.name,
            c.value
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Cookie;

    fn cookie(domain: &str, name: &str, value: &str) -> Cookie {
        Cookie {
            domain: domain.into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            secure: true,
            expires_utc: Some(1_900_000_000),
        }
    }

    #[test]
    fn stale_cookie_folders_are_swept() {
        let run = tempfile::tempdir().unwrap();
        let r = run.path();
        // A process that has exited and been collected: its pid is free.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let own = std::process::id();
        let dir = |name: String| {
            let p = r.join(name);
            fs::create_dir(&p).unwrap();
            fs::write(p.join("cookies.txt"), "fake").unwrap();
            p
        };
        let stale = dir(format!("yt-dlp-{dead}-0"));
        let stale2 = dir(format!("yt-dlp-{dead}-17"));
        let mine = dir(format!("yt-dlp-{own}-3"));
        // pid 1 always runs (as another user here: EPERM still means alive).
        let other = dir("yt-dlp-1-0".into());
        let unrelated = dir("ytmfast-something".into());
        let odd = dir(format!("yt-dlp-{dead}-x"));
        let file = r.join(format!("yt-dlp-{dead}-99"));
        fs::write(&file, "not a folder").unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join("keep"), "x").unwrap();
        let link = r.join(format!("yt-dlp-{dead}-98"));
        std::os::unix::fs::symlink(target.path(), &link).unwrap();

        sweep_stale(r);

        assert!(
            !stale.exists() && !stale2.exists(),
            "a dead engine's folders go"
        );
        for kept in [&mine, &other, &unrelated, &odd, &file, &link] {
            assert!(fs::symlink_metadata(kept).is_ok(), "{kept:?} was kept");
        }
        assert!(
            target.path().join("keep").exists(),
            "a link is never followed"
        );
        // A missing runtime folder is no error.
        sweep_stale(&r.join("missing"));
    }

    #[test]
    fn netscape_lines() {
        let s = Session {
            cookies: vec![
                cookie(".youtube.com", "SID", "v1"),
                cookie("music.youtube.com", "PREF", "f6=40000000"),
                cookie(".youtube.com", "EVIL", "x\n.evil.example\tTRUE"),
                cookie(".youtube.com", "", "v"),
            ],
        };
        assert_eq!(
            netscape_cookies(&s),
            "# Netscape HTTP Cookie File\n\
             .youtube.com\tTRUE\t/\tTRUE\t1900000000\tSID\tv1\n\
             music.youtube.com\tFALSE\t/\tTRUE\t1900000000\tPREF\tf6=40000000\n"
        );
    }
}
