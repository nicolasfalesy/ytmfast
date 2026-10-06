//! The track download: the whole song in a few big HTTP range requests, kept in memory and
//! read by the audio thread through a blocking `Read + Seek`.
//!
//! Why bursts: the radio can sleep between requests. A song is fetched as 10 MiB ranges sent
//! back to back on one connection (most songs are one request), instead of trickling in at
//! the playback rate and keeping the radio awake for the whole song.
//!
//! Failures: a dropped connection or a 5xx resumes from the last byte received, after 1, 2,
//! 4, 8 and 16 s; the sixth failure in a row ends the download. A 403 means the link stopped
//! working: the `relink` callback is asked once per failure streak for a new one. Any bytes
//! received end a streak. A failed download wakes every reader with an io error; the bytes
//! that did arrive stay readable.
//!
//! The download task runs while the `TrackBuffer` or any `TrackReader` is alive, and is
//! aborted when the last of them is dropped.

use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use url::{Origin, Url};

use crate::error::Error;
use crate::innertube::clients;
use crate::net;
use crate::streams::Stream;

/// One range request. Big, so most songs are a single request and the radio sleeps after.
const CHUNK: u64 = 10 << 20;

/// The per-track cap from the spec: a bogus length can't make the engine allocate more.
pub const MAX_TRACK: u64 = 32 << 20;

/// Waits before each retry in a failure streak; once they are used up, the next failure
/// ends the download (the first try plus five retries).
const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];

/// 403s from a relinked link, in one streak, that end the download: a link fresh from the
/// resolver that is still refused twice won't start working on a third try.
const FORBIDDEN_AFTER_RELINK: u32 = 2;

/// A boxed, sendable future (the `futures` crate's alias, without the dependency).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Hands back a new link for the same song when the current one answers 403. The engine
/// builds it from `Resolver::resolve_fresh` (ruling R2).
pub type Relink = Box<dyn Fn() -> BoxFuture<'static, Result<String, Error>> + Send + Sync>;

/// A song being downloaded into memory. Cheap to read from many places: each `reader` is an
/// independent cursor over the same bytes.
pub struct TrackBuffer {
    shared: Arc<Shared>,
    owner: Arc<Owner>,
}

/// A blocking `Read + Seek` over a `TrackBuffer`, for the audio thread. A read waits for
/// bytes that haven't arrived yet; reading at or past the track's length is EOF; a read
/// after the download failed for good (with no bytes left to give) is an io error that
/// wraps the `Error` (get it with `get_ref()` and `downcast_ref::<Error>()`).
///
/// A reader can be cancelled from another thread (`canceller`): a read blocked waiting for
/// bytes then returns at once with an error, so a thread stuck on a stalled download can be
/// stopped.
pub struct TrackReader {
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
    pos: u64,
    cancelled: Arc<AtomicBool>,
}

/// Cancels one `TrackReader` (see `TrackReader::canceller`).
#[derive(Clone)]
pub struct ReaderCancel {
    shared: Arc<Shared>,
    cancelled: Arc<AtomicBool>,
}

impl ReaderCancel {
    /// Makes every read and length wait on the reader fail from now on, and wakes one that is
    /// blocked. Other readers of the same track are not affected.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Under the lock: a reader between its check and its wait can't miss the wake-up.
        let _st = self.shared.lock();
        self.shared.wake.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when bytes arrive, the length becomes known, or the download ends.
    wake: Condvar,
}

struct State {
    /// The bytes so far: always the track from its start, with no gaps (the download is
    /// sequential). Reserved to the full length once it is known, so it never regrows.
    data: Vec<u8>,
    total: Option<u64>,
    /// `None` while the download runs.
    end: Option<End>,
    /// Readers blocked on `wake`: with none, appending a network chunk skips the futex call.
    waiting: usize,
}

enum End {
    Done,
    Failed(Error),
    /// The task went away without finishing: aborted, or its runtime shut down.
    Stopped,
}

/// Aborts the download task when the last handle (buffer or reader) is dropped. This is what
/// stops a skipped song's download: the task itself holds only `Shared`, never an `Owner`.
struct Owner {
    task: Option<tokio::task::AbortHandle>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Lives inside the download task. If the task is dropped before it records an end (an abort,
/// or the runtime shutting down), this records `Stopped`, so no reader can wait forever.
struct StopGuard(Arc<Shared>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.finish(End::Stopped);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A reader that panicked mid-copy leaves the bytes valid; keep going.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, mut st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        st.waiting += 1;
        let mut st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
        st.waiting -= 1;
        st
    }

    fn wake_readers(&self, st: &State) {
        if st.waiting > 0 {
            self.wake.notify_all();
        }
    }

    /// Records the end, unless one is already recorded (the first end wins), and wakes
    /// every reader.
    fn finish(&self, end: End) {
        let mut st = self.lock();
        if st.end.is_none() {
            if matches!(end, End::Done) && st.total.is_none() {
                // A 200 without a length: the track is what arrived.
                st.total = Some(st.data.len() as u64);
            }
            st.end = Some(end);
        }
        self.wake.notify_all();
    }

    /// Sets the track's length, or checks it against the one already known.
    fn set_total(&self, total: u64) -> Result<(), Error> {
        if total > MAX_TRACK {
            return Err(too_large());
        }
        let mut st = self.lock();
        let changed = || {
            Err(Error::StreamFailed(
                "the server changed the track's length".into(),
            ))
        };
        if total < st.data.len() as u64 {
            // More bytes already arrived than the track now has: they can't all be this track.
            return changed();
        }
        match st.total {
            Some(known) if known != total => changed(),
            Some(_) => Ok(()),
            None => {
                st.total = Some(total);
                let more = (total as usize).saturating_sub(st.data.len());
                st.data.reserve_exact(more);
                // A reader may be waiting in `seek(End)` for the length.
                self.wake_readers(&st);
                Ok(())
            }
        }
    }

    /// Appends bytes that continue the track, refusing to grow past the cap.
    fn append(&self, bytes: &[u8]) -> Result<(), Error> {
        let mut st = self.lock();
        if (st.data.len() + bytes.len()) as u64 > MAX_TRACK {
            return Err(too_large());
        }
        st.data.extend_from_slice(bytes);
        self.wake_readers(&st);
        Ok(())
    }

    fn progress(&self) -> (u64, Option<u64>) {
        let st = self.lock();
        (st.data.len() as u64, st.total)
    }
}

fn too_large() -> Error {
    Error::StreamFailed(format!(
        "the track is too large (over {} MiB)",
        MAX_TRACK >> 20
    ))
}

impl TrackBuffer {
    /// Starts downloading `stream` in the background, on the current tokio runtime. Without
    /// one, the buffer is born failed (its readers get an error) instead of panicking.
    pub fn start(stream: Stream, relink: Relink) -> TrackBuffer {
        Self::spawn(stream, relink, shared_client(), Allow { test_origin: None })
    }

    /// Like `start`, but links on `base`'s origin (a local http test server) are allowed as
    /// well as the allowlist, and the HTTP client is the caller's. For tests only: this is the
    /// one way past the https allowlist for track downloads (ruling R7). The client is
    /// injected because each test runs its own runtime (pooled connections die with theirs),
    /// and because a paused-clock test needs one without timeouts: tokio auto-advances a
    /// paused clock whenever the runtime waits on real I/O, which fires a 10 s read timeout
    /// at once.
    pub fn start_with_test_base(
        stream: Stream,
        relink: Relink,
        base: Url,
        client: reqwest::Client,
    ) -> TrackBuffer {
        let allow = Allow {
            test_origin: Some(base.origin()),
        };
        Self::spawn(stream, relink, client, allow)
    }

    fn spawn(stream: Stream, relink: Relink, client: reqwest::Client, allow: Allow) -> TrackBuffer {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                data: Vec::new(),
                total: None,
                end: None,
                waiting: 0,
            }),
            wake: Condvar::new(),
        });
        let job = Download {
            shared: shared.clone(),
            client,
            url: stream.url,
            relink,
            allow,
        };
        let declared = stream.content_length;
        let task = match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let shared = shared.clone();
                let handle = rt.spawn(async move {
                    let _guard = StopGuard(shared.clone());
                    let end = match job.run(declared).await {
                        Ok(()) => End::Done,
                        Err(e) => {
                            // The code only (ruling R6): messages never carry the link, but a
                            // log needs no more than the code.
                            eprintln!("ytmfast: track download failed ({})", e.code());
                            End::Failed(e)
                        }
                    };
                    shared.finish(end);
                });
                Some(handle.abort_handle())
            }
            Err(_) => {
                shared.finish(End::Failed(Error::Internal(
                    "the track download needs an async runtime".into(),
                )));
                None
            }
        };
        TrackBuffer {
            shared,
            owner: Arc::new(Owner { task }),
        }
    }

    /// A track that is already whole in memory: no download, readers see `bytes` and then
    /// EOF. For tests and benchmarks (fixture files), and later for a cached track.
    pub fn from_bytes(bytes: Vec<u8>) -> TrackBuffer {
        let total = bytes.len() as u64;
        TrackBuffer {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    data: bytes,
                    total: Some(total),
                    end: Some(End::Done),
                    waiting: 0,
                }),
                wake: Condvar::new(),
            }),
            owner: Arc::new(Owner { task: None }),
        }
    }

    /// A download that has `bytes` of a `total`-byte track and never gets more: readers block
    /// past `bytes` until cancelled. For tests of a stalled server.
    #[cfg(test)]
    pub(crate) fn stalled(bytes: Vec<u8>, total: u64) -> TrackBuffer {
        TrackBuffer {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    data: bytes,
                    total: Some(total),
                    end: None,
                    waiting: 0,
                }),
                wake: Condvar::new(),
            }),
            owner: Arc::new(Owner { task: None }),
        }
    }

    /// A new cursor at the start of the track. It keeps the download going while it lives.
    pub fn reader(&self) -> TrackReader {
        TrackReader {
            shared: self.shared.clone(),
            _owner: self.owner.clone(),
            pos: 0,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Bytes downloaded so far, from the start of the track.
    pub fn downloaded(&self) -> u64 {
        self.shared.lock().data.len() as u64
    }
}

impl TrackReader {
    /// Another cursor over the same track, at its start, cancelled together with this one:
    /// for reading its headers again (`Decoder` does when a seek after the end fails).
    pub(crate) fn sibling(&self) -> TrackReader {
        TrackReader {
            shared: self.shared.clone(),
            _owner: self._owner.clone(),
            pos: 0,
            cancelled: self.cancelled.clone(),
        }
    }

    /// A handle that cancels this reader from any thread.
    pub fn canceller(&self) -> ReaderCancel {
        ReaderCancel {
            shared: self.shared.clone(),
            cancelled: self.cancelled.clone(),
        }
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::other(Error::Internal(
                "the track was stopped".into(),
            )));
        }
        Ok(())
    }

    /// The track's length once the first answer has said it (it has by the time the first
    /// byte can be read), else `None`. Never waits.
    pub fn total_len(&self) -> Option<u64> {
        self.shared.lock().total
    }

    /// The download's end as an io error, for a reader that can't make progress.
    fn failure(end: &End) -> Option<io::Error> {
        match end {
            End::Done => None,
            End::Failed(e) => Some(io::Error::other(e.clone())),
            End::Stopped => Some(io::Error::other(Error::Internal(
                "the track download stopped".into(),
            ))),
        }
    }
}

impl Read for TrackReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut st = self.shared.lock();
        loop {
            self.check_cancelled()?;
            let have = st.data.len() as u64;
            if self.pos < have {
                let n = (have - self.pos).min(buf.len() as u64) as usize;
                let at = self.pos as usize;
                buf[..n].copy_from_slice(&st.data[at..at + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if st.total.is_some_and(|t| self.pos >= t) {
                return Ok(0);
            }
            if let Some(end) = &st.end {
                // `Done` always sets `total`, so only a failure gets here.
                return Err(
                    Self::failure(end).unwrap_or_else(|| io::ErrorKind::UnexpectedEof.into())
                );
            }
            st = self.shared.wait(st);
        }
    }
}

impl Seek for TrackReader {
    /// Moving the cursor never waits, except `SeekFrom::End`, which waits for the length.
    /// A position past what has arrived is fine: the next read waits for it.
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
            SeekFrom::End(d) => {
                let mut st = self.shared.lock();
                let total = loop {
                    self.check_cancelled()?;
                    if let Some(t) = st.total {
                        break t;
                    }
                    if let Some(e) = st.end.as_ref().and_then(Self::failure) {
                        return Err(e);
                    }
                    st = self.shared.wait(st);
                };
                i128::from(total) + i128::from(d)
            }
        };
        self.pos = u64::try_from(target).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the track",
            )
        })?;
        Ok(self.pos)
    }
}

/// The user agent of the client the links come from. googlevideo serves a link to the
/// client it was made for; the own-code resolver asks as the TV client.
fn new_client() -> reqwest::Client {
    net::stream_client(clients::TV.user_agent)
}

/// One client for every track, so a song after the first can reuse the open connection
/// (no new TLS handshake) and the TLS setup is built once.
fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(new_client).clone()
}

/// Which links may be fetched (ruling R7).
struct Allow {
    test_origin: Option<Origin>,
}

impl Allow {
    fn check(&self, link: &str) -> Result<(), Error> {
        let url = Url::parse(link)
            .map_err(|_| Error::StreamFailed("the stream link is not a URL".into()))?;
        if net::allowed_host(&url) || self.test_origin.as_ref() == Some(&url.origin()) {
            Ok(())
        } else {
            Err(Error::StreamFailed(
                "the stream link is not on an allowed host".into(),
            ))
        }
    }
}

/// How one request went.
enum Failure {
    /// Worth another try from the last byte (a drop, a stall, a 5xx).
    Retry(Error),
    /// The link answered 403.
    Forbidden,
    /// Retrying can't help (a malformed answer, the cap, a refused redirect).
    Fatal(Error),
}

/// The failure streak's state. Reset by any bytes received.
#[derive(Default)]
struct Streak {
    retries: usize,
    relinked: bool,
    forbidden_after_relink: u32,
}

impl Streak {
    /// Sleeps before the next retry, or hands back `e` when the retries are used up.
    async fn back_off(&mut self, e: Error) -> Result<(), Error> {
        let Some(secs) = BACKOFF_SECS.get(self.retries) else {
            return Err(e);
        };
        self.retries += 1;
        eprintln!(
            "ytmfast: track download interrupted ({}); retrying in {secs} s",
            e.code()
        );
        tokio::time::sleep(Duration::from_secs(*secs)).await;
        Ok(())
    }
}

struct Download {
    shared: Arc<Shared>,
    client: reqwest::Client,
    /// The signed link: never logged or put in an error (ruling R6).
    url: String,
    relink: Relink,
    allow: Allow,
}

impl Download {
    async fn run(mut self, declared: Option<u64>) -> Result<(), Error> {
        self.allow.check(&self.url)?;
        // A 0 from the API is no length at all; the first answer will say.
        if let Some(n) = declared.filter(|&n| n > 0) {
            self.shared.set_total(n)?;
        }
        let mut streak = Streak::default();
        loop {
            let (pos, total) = self.shared.progress();
            if total.is_some_and(|t| pos >= t) {
                return Ok(());
            }
            let last = (pos + CHUNK - 1).min(total.map_or(u64::MAX, |t| t - 1));
            let result = self.fetch(pos, last).await;
            if self.shared.progress().0 > pos {
                streak = Streak::default();
            }
            match result {
                Ok(()) => {}
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e)) => streak.back_off(e).await?,
                Err(Failure::Forbidden) if !streak.relinked => {
                    // The link expired or was revoked: get a new one and go on at once.
                    streak.relinked = true;
                    eprintln!("ytmfast: stream link refused; asking for a new one");
                    let url = (self.relink)().await?;
                    self.allow.check(&url)?;
                    self.url = url;
                }
                Err(Failure::Forbidden) => {
                    streak.forbidden_after_relink += 1;
                    let refused =
                        Error::StreamFailed("the server refused the new stream link".into());
                    if streak.forbidden_after_relink >= FORBIDDEN_AFTER_RELINK {
                        return Err(refused);
                    }
                    streak.back_off(refused).await?;
                }
            }
        }
    }

    /// Asks for bytes `pos..=last` and appends what arrives. `Ok` means the answer arrived
    /// whole, which may be fewer bytes than asked (a server may send a shorter range).
    async fn fetch(&self, pos: u64, last: u64) -> Result<(), Failure> {
        let mut resp = self
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={pos}-{last}"))
            .send()
            .await
            .map_err(request_failure)?;
        // The start of the track only: later chunks and retries are not the start's cost.
        let mut first = pos == 0;
        if first {
            crate::trace::mark("download answered");
        }
        let status = resp.status();
        // How many leading body bytes to drop, and how many to keep.
        let (mut skip, mut want) = match status {
            StatusCode::PARTIAL_CONTENT => {
                let (first, end, total) = resp
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range)
                    .ok_or_else(|| fatal("the server's answer had no usable Content-Range"))?;
                self.shared.set_total(total).map_err(Failure::Fatal)?;
                if first != pos || end < first || end >= total {
                    return Err(fatal("the server answered a different range"));
                }
                (0, Some(end - first + 1))
            }
            // The server ignored the range: this is the whole track from byte 0.
            StatusCode::OK => {
                if let Some(total) = resp.content_length() {
                    self.shared.set_total(total).map_err(Failure::Fatal)?;
                }
                let (_, total) = self.shared.progress();
                // `set_total` refused any length shorter than what arrived, so `t >= pos`.
                (pos, total.map(|t| t.saturating_sub(pos)))
            }
            StatusCode::FORBIDDEN => return Err(Failure::Forbidden),
            s if s.is_server_error()
                || s == StatusCode::REQUEST_TIMEOUT
                || s == StatusCode::TOO_MANY_REQUESTS =>
            {
                return Err(Failure::Retry(Error::Network(format!(
                    "the stream server answered {}",
                    s.as_u16()
                ))));
            }
            s => {
                return Err(Failure::Fatal(Error::StreamFailed(format!(
                    "the stream server answered {}",
                    s.as_u16()
                ))));
            }
        };
        while want != Some(0) {
            let Some(mut chunk) = resp.chunk().await.map_err(request_failure)? else {
                break;
            };
            if skip > 0 {
                let drop = (skip.min(chunk.len() as u64)) as usize;
                let _ = chunk.split_to(drop);
                skip -= drop as u64;
            }
            if let Some(w) = &mut want {
                // Never past the asked range, even if a server sends more than it said.
                chunk.truncate((*w).min(chunk.len() as u64) as usize);
                *w -= chunk.len() as u64;
            }
            if !chunk.is_empty() {
                self.shared.append(&chunk).map_err(Failure::Fatal)?;
                if first {
                    first = false;
                    crate::trace::mark("first bytes of the download");
                }
            }
        }
        if want.is_some_and(|w| w > 0) || skip > 0 {
            return Err(Failure::Retry(Error::Network(
                "the connection dropped while reading the answer".into(),
            )));
        }
        if want.is_none() {
            // A 200 that never said its length: the track is what arrived.
            let (len, _) = self.shared.progress();
            self.shared.set_total(len).map_err(Failure::Fatal)?;
        }
        Ok(())
    }
}

fn fatal(what: &str) -> Failure {
    Failure::Fatal(Error::StreamFailed(what.into()))
}

/// A request error, through the one URL-free mapping (ruling R6). A refused redirect or a
/// request we built badly won't get better by retrying; the rest (drops, stalls, connection
/// errors) might.
fn request_failure(e: reqwest::Error) -> Failure {
    let permanent = e.is_redirect() || e.is_builder();
    let e = Error::from(e);
    if permanent {
        Failure::Fatal(e)
    } else {
        Failure::Retry(e)
    }
}

/// `bytes first-last/total` into its numbers. A `*` total (unknown) is refused: without the
/// length, the cap can't be checked up front and EOF can't be told from a drop.
fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    Some((
        first.trim().parse().ok()?,
        last.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cancel_wakes_a_blocked_read() {
        let buf = TrackBuffer::stalled(vec![1, 2, 3], 100);
        let mut reader = buf.reader();
        let cancel = reader.canceller();
        let other = buf.reader();
        let t = std::thread::spawn(move || {
            let mut b = [0u8; 8];
            assert_eq!(
                reader.read(&mut b).unwrap(),
                3,
                "what has arrived is readable"
            );
            reader.read(&mut b)
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished(), "the second read waits for bytes");
        cancel.cancel();
        let err = t.join().unwrap().unwrap_err();
        let ours = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<Error>())
            .cloned();
        assert_eq!(ours, Some(Error::Internal("the track was stopped".into())));
        assert!(cancel.is_cancelled());
        assert!(!other.canceller().is_cancelled(), "only that reader");
    }

    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(
            parse_content_range("bytes 0-1023/4096"),
            Some((0, 1023, 4096))
        );
        assert_eq!(parse_content_range("bytes 0-1023/*"), None);
        assert_eq!(parse_content_range("bytes */4096"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes 5-x/9"), None);
    }

    #[test]
    fn allow_checks_the_allowlist_and_only_the_test_origin() {
        let none = Allow { test_origin: None };
        assert!(
            none.check("https://rr1---sn-x.googlevideo.com/videoplayback?x=1")
                .is_ok()
        );
        assert!(none.check("http://127.0.0.1:8080/a").is_err());
        assert!(none.check("not a url").is_err());
        let test = Allow {
            test_origin: Some(Url::parse("http://127.0.0.1:8080/").unwrap().origin()),
        };
        assert!(test.check("http://127.0.0.1:8080/b?sig=1").is_ok());
        assert!(test.check("http://127.0.0.1:8081/b").is_err());
        assert!(test.check("https://evil.example/").is_err());
        // The messages never echo the link (ruling R6).
        let e = none
            .check("http://127.0.0.1:8080/a?sig=SECRET")
            .unwrap_err();
        assert!(!format!("{e} {e:?}").contains("SECRET"));
    }

    #[test]
    fn a_length_change_is_refused() {
        let shared = Shared {
            state: Mutex::new(State {
                data: Vec::new(),
                total: None,
                end: None,
                waiting: 0,
            }),
            wake: Condvar::new(),
        };
        shared.append(&[1, 2, 3, 4]).unwrap();
        assert!(shared.set_total(3).is_err(), "shorter than what arrived");
        shared.set_total(10).unwrap();
        shared.set_total(10).unwrap();
        assert!(shared.set_total(11).is_err(), "a different length");
        assert_eq!(shared.set_total(MAX_TRACK + 1), Err(too_large()));
    }

    #[test]
    fn no_runtime_fails_the_buffer_instead_of_panicking() {
        let s = Stream {
            video_id: "FAKEVID0001".into(),
            url: "https://rr1---sn-x.googlevideo.com/videoplayback".into(),
            itag: 774,
            mime: String::new(),
            content_length: Some(10),
            expires_unix: 0,
            loudness_db: None,
            meta: Default::default(),
            tracking: Default::default(),
        };
        let buf = TrackBuffer::start(s, Box::new(|| Box::pin(async { unreachable!() })));
        let e = buf.reader().read(&mut [0u8; 4]).unwrap_err();
        let ours = e.get_ref().and_then(|e| e.downcast_ref::<Error>()).unwrap();
        assert_eq!(ours.code(), "internal");
    }
}
