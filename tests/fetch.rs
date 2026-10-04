//! The track download (`audio::fetch`) against a local HTTP server.
//!
//! The server is a raw tokio TcpListener responder rather than wiremock: the tests need to cut
//! a connection part way through a body, pace a body slowly, and see when the client hangs
//! up, none of which wiremock can do. Every connection answers one request and closes.

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;
use url::Url;
use ytmfast::audio::fetch::{Relink, TrackBuffer, TrackReader};
use ytmfast::error::Error;
use ytmfast::innertube::Tracking;
use ytmfast::streams::{Stream, TrackMeta};

const MIB: u64 = 1 << 20;

/// A fake access token in every test link: no error may ever show it (ruling R6).
const TOKEN: &str = "FAKETOKEN123";

/// Bytes that differ from offset to offset, so a resume from the wrong place shows up.
fn track(len: u64) -> Arc<Vec<u8>> {
    Arc::new(
        (0..len)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect(),
    )
}

#[derive(Clone, Debug)]
struct Req {
    path: String,
    range: Option<String>,
    at: Instant,
}

impl Req {
    /// The asked range, clamped to the track like a real server does.
    fn span(&self, total: u64) -> (u64, u64) {
        let Some(r) = self.range.as_deref().and_then(|r| r.strip_prefix("bytes=")) else {
            return (0, total - 1);
        };
        let (a, b) = r.split_once('-').unwrap();
        let a: u64 = a.parse().unwrap();
        let b = b.parse().map_or(total - 1, |b: u64| b.min(total - 1));
        (a, b)
    }
}

enum Reply {
    /// A 206 for `[start, end]` of the track. `cut`: hang up once the body reaches this
    /// offset. `slow`: 64 KiB every 50 ms.
    Part {
        start: u64,
        end: u64,
        cut: Option<u64>,
        slow: bool,
    },
    /// A 200 with the whole track, no Content-Range.
    Whole,
    /// A bodyless answer with this status.
    Status(u16),
    /// This exact response head, and no body.
    Raw(String),
}

type Handler = Arc<dyn Fn(usize, &Req) -> Reply + Send + Sync>;

struct Server {
    base: Url,
    reqs: Arc<Mutex<Vec<Req>>>,
    /// Set when a body write failed: the client hung up.
    hung_up: Arc<AtomicBool>,
}

impl Server {
    fn reqs(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}?sig={TOKEN}", self.base)
    }
}

async fn server(data: Arc<Vec<u8>>, handler: Handler) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let hung_up = Arc::new(AtomicBool::new(false));
    let (r, h) = (reqs.clone(), hung_up.clone());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (data, handler, reqs, hung_up) =
                (data.clone(), handler.clone(), r.clone(), h.clone());
            tokio::spawn(async move {
                answer(sock, data, handler, reqs, hung_up).await;
            });
        }
    });
    Server {
        base,
        reqs,
        hung_up,
    }
}

async fn answer(
    mut sock: tokio::net::TcpStream,
    data: Arc<Vec<u8>>,
    handler: Handler,
    reqs: Arc<Mutex<Vec<Req>>>,
    hung_up: Arc<AtomicBool>,
) {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let head = loop {
        let Ok(n) = sock.read(&mut buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        got.extend_from_slice(&buf[..n]);
        if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break String::from_utf8_lossy(&got[..end]).into_owned();
        }
    };
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_string();
    let range = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("range")
            .then(|| v.trim().to_string())
    });
    let req = Req {
        path,
        range,
        at: Instant::now(),
    };
    let index = {
        let mut reqs = reqs.lock().unwrap();
        reqs.push(req.clone());
        reqs.len() - 1
    };
    let total = data.len() as u64;
    // The head, and which bytes of the track follow it (and whether slowly).
    let (head, body): (String, Option<(Range<u64>, bool)>) = match handler(index, &req) {
        Reply::Part {
            start,
            end,
            cut,
            slow,
        } => (
            format!(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {start}-{end}/{total}\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                end - start + 1
            ),
            Some((start..cut.map_or(end + 1, |c| c.min(end + 1)), slow)),
        ),
        Reply::Whole => (
            format!("HTTP/1.1 200 OK\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"),
            Some((0..total, false)),
        ),
        Reply::Status(code) => (
            format!("HTTP/1.1 {code} Nope\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
            None,
        ),
        Reply::Raw(head) => (head, None),
    };
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let Some((Range { start, end: stop }, slow)) = body else {
        return;
    };
    let step = if slow { 64 << 10 } else { 1 << 20 };
    let mut at = start;
    while at < stop {
        let to = (at + step).min(stop);
        if sock
            .write_all(&data[at as usize..to as usize])
            .await
            .is_err()
        {
            hung_up.store(true, Ordering::SeqCst);
            return;
        }
        at = to;
        if slow {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // A cut ends here: dropping the socket closes it with the body short of its length.
}

/// Serves every asked range of a `total`-byte track in full.
fn plain_for(total: u64) -> Handler {
    Arc::new(move |_, req: &Req| {
        let (start, end) = req.span(total);
        Reply::Part {
            start,
            end,
            cut: None,
            slow: false,
        }
    })
}

fn stream(url: String, content_length: Option<u64>) -> Stream {
    Stream {
        video_id: "FAKEVID0001".into(),
        url,
        itag: 774,
        mime: "audio/webm; codecs=\"opus\"".into(),
        content_length,
        expires_unix: u64::MAX,
        loudness_db: None,
        meta: TrackMeta::default(),
        tracking: Tracking::default(),
    }
}

/// A relink that must not be called.
fn no_relink() -> Relink {
    Box::new(|| Box::pin(async { panic!("relink was not expected") }))
}

/// A relink that counts its calls and hands back `url`.
fn relink_to(url: String, calls: Arc<AtomicUsize>) -> Relink {
    Box::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        let url = url.clone();
        Box::pin(async move { Ok(url) })
    })
}

/// With the production download client (its timeouts and redirect policy).
fn start(srv: &Server, s: Stream, relink: Relink) -> TrackBuffer {
    let client = ytmfast::net::stream_client("ytmfast-test/0");
    TrackBuffer::start_with_test_base(s, relink, srv.base.clone(), client)
}

/// For paused-clock tests: a client with no timeouts. Tokio auto-advances a paused clock
/// whenever the runtime waits on real I/O (here, loopback), so a pending 10 s read timeout
/// would fire at once and every request would "time out".
fn start_paused(srv: &Server, s: Stream, relink: Relink) -> TrackBuffer {
    let client = reqwest::Client::new();
    TrackBuffer::start_with_test_base(s, relink, srv.base.clone(), client)
}

/// Runs `f` on a plain thread, as the audio thread would. Not `spawn_blocking`: tokio stops
/// the paused clock from auto-advancing while a blocking-pool task runs, so a reader waiting
/// there would stall the backoff tests forever.
async fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.expect("the reader thread panicked")
}

/// Reads the whole track on a plain thread.
async fn read_all(mut r: TrackReader) -> std::io::Result<Vec<u8>> {
    on_thread(move || {
        let mut out = Vec::new();
        r.read_to_end(&mut out).map(|_| out)
    })
    .await
}

/// Our error inside the reader's io error.
fn ours(e: &std::io::Error) -> Error {
    e.get_ref()
        .and_then(|e| e.downcast_ref::<Error>())
        .cloned()
        .unwrap_or_else(|| panic!("not one of ours: {e}"))
}

fn no_token(e: &std::io::Error) {
    let text = format!("{e} {e:?} {}", ours(e));
    assert!(!text.contains(TOKEN), "an error shows the link: {text}");
    assert!(
        !text.contains("127.0.0.1"),
        "an error shows the link: {text}"
    );
}

#[tokio::test]
async fn downloads_whole_track_in_bursts() {
    let data = track(3 * MIB);
    let srv = server(data.clone(), plain_for(3 * MIB)).await;
    let buf = start(&srv, stream(srv.url("a"), Some(3 * MIB)), no_relink());
    let got = read_all(buf.reader()).await.unwrap();
    assert!(got == *data, "the bytes differ");
    assert_eq!(buf.downloaded(), 3 * MIB);
    let reqs = srv.reqs();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].range.as_deref(), Some("bytes=0-3145727"));

    // Past the end is EOF, and seeking from the end works.
    let mut r = buf.reader();
    let tail = on_thread(move || {
        assert_eq!(r.seek(SeekFrom::End(0)).unwrap(), 3 * MIB);
        let mut b = [0u8; 16];
        assert_eq!(r.read(&mut b).unwrap(), 0, "read past the end is EOF");
        r.seek(SeekFrom::Start(10 * MIB)).unwrap();
        assert_eq!(r.read(&mut b).unwrap(), 0, "read far past the end is EOF");
        r.seek(SeekFrom::End(-10)).unwrap();
        let mut t = Vec::new();
        r.read_to_end(&mut t).unwrap();
        t
    })
    .await;
    assert_eq!(tail, data[data.len() - 10..]);
}

#[tokio::test]
async fn chunks_are_10_mib_back_to_back() {
    let total = 25 * MIB;
    let data = track(total);
    let srv = server(data.clone(), plain_for(total)).await;
    let buf = start(&srv, stream(srv.url("a"), Some(total)), no_relink());
    let got = read_all(buf.reader()).await.unwrap();
    assert!(got == *data, "the bytes differ");
    let ranges: Vec<_> = srv.reqs().into_iter().map(|r| r.range.unwrap()).collect();
    assert_eq!(
        ranges,
        [
            "bytes=0-10485759",
            "bytes=10485760-20971519",
            "bytes=20971520-26214399"
        ]
    );
}

#[tokio::test]
async fn unknown_length_is_read_from_content_range() {
    let data = track(3 * MIB);
    let srv = server(data.clone(), plain_for(3 * MIB)).await;
    let buf = start(&srv, stream(srv.url("a"), None), no_relink());
    let got = read_all(buf.reader()).await.unwrap();
    assert!(got == *data, "the bytes differ");
    let reqs = srv.reqs();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].range.as_deref(), Some("bytes=0-10485759"));
    assert_eq!(buf.reader().total_len(), Some(3 * MIB));
}

#[tokio::test]
async fn a_200_with_the_whole_body_is_taken() {
    let data = track(3 * MIB);
    let srv = server(data.clone(), Arc::new(|_, _: &Req| Reply::Whole)).await;
    for len in [Some(3 * MIB), None] {
        let buf = start(&srv, stream(srv.url("a"), len), no_relink());
        let got = read_all(buf.reader()).await.unwrap();
        assert!(got == *data, "the bytes differ ({len:?})");
    }
}

#[tokio::test]
async fn resumes_after_drop() {
    let total = 3 * MIB;
    let data = track(total);
    let srv = server(
        data.clone(),
        Arc::new(move |i, req: &Req| {
            let (start, end) = req.span(total);
            Reply::Part {
                start,
                end,
                cut: (i == 0).then_some(MIB),
                slow: false,
            }
        }),
    )
    .await;
    let buf = start(&srv, stream(srv.url("a"), Some(total)), no_relink());
    let got = read_all(buf.reader()).await.unwrap();
    assert!(got == *data, "the bytes differ");
    let reqs = srv.reqs();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert!(
        reqs[1]
            .range
            .as_deref()
            .unwrap()
            .starts_with("bytes=1048576-"),
        "{reqs:?}"
    );
}

#[tokio::test]
async fn relinks_on_403() {
    let total = 3 * MIB;
    let data = track(total);
    let srv = server(
        data.clone(),
        Arc::new(move |_, req: &Req| {
            let (start, end) = req.span(total);
            match req.path.as_str() {
                // The first link stops working at 2 MiB: it serves up to there (a shorter
                // range than asked, which HTTP allows), then answers 403.
                "/a" if start >= 2 * MIB => Reply::Status(403),
                "/a" => Reply::Part {
                    start,
                    end: end.min(2 * MIB - 1),
                    cut: None,
                    slow: false,
                },
                _ => Reply::Part {
                    start,
                    end,
                    cut: None,
                    slow: false,
                },
            }
        }),
    )
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let buf = start(
        &srv,
        stream(srv.url("a"), Some(total)),
        relink_to(srv.url("b"), calls.clone()),
    );
    let got = read_all(buf.reader()).await.unwrap();
    assert!(got == *data, "the bytes differ");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let seen: Vec<_> = srv
        .reqs()
        .into_iter()
        .map(|r| (r.path, r.range.unwrap()))
        .collect();
    assert_eq!(
        seen,
        [
            ("/a".to_string(), "bytes=0-3145727".to_string()),
            ("/a".to_string(), "bytes=2097152-3145727".to_string()),
            ("/b".to_string(), "bytes=2097152-3145727".to_string()),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn gives_up_when_the_new_link_403s_twice() {
    let data = track(MIB);
    let srv = server(data, Arc::new(|_, _: &Req| Reply::Status(403))).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let buf = start_paused(
        &srv,
        stream(srv.url("a"), Some(MIB)),
        relink_to(srv.url("b"), calls.clone()),
    );
    let e = read_all(buf.reader()).await.unwrap_err();
    no_token(&e);
    assert_eq!(ours(&e).code(), "stream_failed", "{e}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let paths: Vec<_> = srv.reqs().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, ["/a", "/b", "/b"]);
}

#[tokio::test]
async fn gives_up_when_relink_fails() {
    let data = track(MIB);
    let srv = server(data, Arc::new(|_, _: &Req| Reply::Status(403))).await;
    let relink: Relink =
        Box::new(|| Box::pin(async { Err(Error::Unavailable("the song was taken down".into())) }));
    let buf = start(&srv, stream(srv.url("a"), Some(MIB)), relink);
    let e = read_all(buf.reader()).await.unwrap_err();
    no_token(&e);
    assert_eq!(
        ours(&e),
        Error::Unavailable("the song was taken down".into())
    );
    assert_eq!(srv.reqs().len(), 1);
}

#[tokio::test]
async fn cap_32_mib() {
    // A declared length over the cap: refused before any request.
    let srv = server(track(1), plain_for(1)).await;
    let buf = start(&srv, stream(srv.url("a"), Some(33 * MIB)), no_relink());
    let e = read_all(buf.reader()).await.unwrap_err();
    assert_eq!(ours(&e).code(), "stream_failed", "{e}");
    assert!(e.to_string().contains("32 MiB"), "{e}");
    assert!(srv.reqs().is_empty());

    // No declared length, and the server's Content-Range says 33 MiB.
    let big = 33 * MIB;
    let srv = server(
        track(1),
        Arc::new(move |_, _: &Req| {
            Reply::Raw(format!(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-10485759/{big}\r\n\
                 content-length: 10485760\r\nconnection: close\r\n\r\n"
            ))
        }),
    )
    .await;
    let buf = start(&srv, stream(srv.url("a"), None), no_relink());
    let e = read_all(buf.reader()).await.unwrap_err();
    assert_eq!(ours(&e).code(), "stream_failed", "{e}");
    assert!(e.to_string().contains("32 MiB"), "{e}");
    assert_eq!(buf.downloaded(), 0);
}

#[tokio::test]
async fn seek_beyond_downloaded_waits() {
    let total = 3 * MIB;
    let data = track(total);
    let srv = server(
        data.clone(),
        Arc::new(move |_, req: &Req| {
            let (start, end) = req.span(total);
            Reply::Part {
                start,
                end,
                cut: None,
                slow: true,
            }
        }),
    )
    .await;
    let buf = start(&srv, stream(srv.url("a"), Some(total)), no_relink());
    let mut r = buf.reader();
    assert!(buf.downloaded() < 2 * MIB);
    let read = on_thread(move || {
        r.seek(SeekFrom::Start(2 * MIB)).unwrap();
        let mut b = vec![0u8; 64 << 10];
        r.read_exact(&mut b).unwrap();
        b
    });
    let got = tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("the reader hung");
    let at = 2 * MIB as usize;
    assert!(got == data[at..at + (64 << 10)], "the bytes differ");
    assert!(buf.downloaded() >= 2 * MIB + (64 << 10));
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_retries() {
    let data = track(MIB);
    // Every answer drops before its first body byte.
    let srv = server(
        data,
        Arc::new(|_, req: &Req| {
            let (start, end) = req.span(MIB);
            Reply::Part {
                start,
                end,
                cut: Some(start),
                slow: false,
            }
        }),
    )
    .await;
    let t0 = Instant::now();
    let buf = start_paused(&srv, stream(srv.url("a"), Some(MIB)), no_relink());
    let e = read_all(buf.reader()).await.unwrap_err();
    let took = t0.elapsed();
    no_token(&e);
    assert_eq!(ours(&e).code(), "network", "{e}");
    // The first try and five retries, 1, 2, 4, 8 and 16 s apart.
    let reqs = srv.reqs();
    assert_eq!(reqs.len(), 6, "{reqs:?}");
    let gaps: Vec<u64> = reqs
        .windows(2)
        .map(|w| (w[1].at - w[0].at).as_millis() as u64)
        .collect();
    for (gap, want) in gaps.iter().zip([1000, 2000, 4000, 8000, 16000]) {
        assert!(
            (want..want + 100).contains(gap),
            "gaps {gaps:?} should be 1, 2, 4, 8, 16 s"
        );
    }
    assert!(took < Duration::from_secs(32), "{took:?}");
}

#[tokio::test]
async fn dropping_the_buffer_stops_the_download() {
    let total = 3 * MIB;
    let srv = server(
        track(total),
        Arc::new(move |_, req: &Req| {
            let (start, end) = req.span(total);
            Reply::Part {
                start,
                end,
                cut: None,
                slow: true,
            }
        }),
    )
    .await;
    let buf = start(&srv, stream(srv.url("a"), Some(total)), no_relink());
    // A reader keeps the download going after the buffer is dropped.
    let r = buf.reader();
    drop(buf);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !srv.hung_up.load(Ordering::SeqCst),
        "a reader still needs it"
    );
    // Once the last reader goes, the connection closes long before the 2.4 s body ends.
    drop(r);
    let gone = Instant::now();
    while !srv.hung_up.load(Ordering::SeqCst) {
        assert!(
            gone.elapsed() < Duration::from_secs(2),
            "the download kept going"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn refuses_links_off_the_allowlist() {
    // The production constructor: the local http server is not an allowed host, so not even
    // one request is sent.
    let srv = server(track(MIB), plain_for(MIB)).await;
    let buf = TrackBuffer::start(stream(srv.url("a"), Some(MIB)), no_relink());
    let e = read_all(buf.reader()).await.unwrap_err();
    no_token(&e);
    assert_eq!(ours(&e).code(), "stream_failed", "{e}");
    assert!(srv.reqs().is_empty());

    // A relink to a host off the allowlist is refused too, test base or not.
    let srv = server(track(MIB), Arc::new(|_, _: &Req| Reply::Status(403))).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let buf = start(
        &srv,
        stream(srv.url("a"), Some(MIB)),
        relink_to(format!("https://evil.example/v?sig={TOKEN}"), calls.clone()),
    );
    let e = read_all(buf.reader()).await.unwrap_err();
    no_token(&e);
    assert!(!e.to_string().contains("evil.example"), "{e}");
    assert_eq!(ours(&e).code(), "stream_failed", "{e}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(srv.reqs().len(), 1);
}
