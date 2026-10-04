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
