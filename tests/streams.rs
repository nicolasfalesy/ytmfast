//! Link resolution against a local wiremock server, with a fake solver and a fake yt-dlp.
//!
//! Every fixture is hand-made: fake ids, fake URLs, fake cookie values. Nothing here talks to
//! YouTube. The server stands in for the three things `Streams` fetches (the player version,
//! the player script and the InnerTube `player` answer); its http base URL is injected through
//! the constructors, the one place the https allowlist is bypassed (ruling R7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::auth::{Cookie, MemoryStore, Session};
use ytmfast::error::Error;
use ytmfast::innertube::{AudioFormat, Innertube};
use ytmfast::solver::{Answers, ChallengeKind, ChallengeSolver};
use ytmfast::streams::ytdlp::{YtDlp, YtDlpCommand};
use ytmfast::streams::{Resolver, Streams, pick_format};

const VIDEO: &str = "testvideo01";
const PLAYER: &str = "0000000a";
const PLAYER_JS: &str = "var cfg={signatureTimestamp:20725};";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn format(itag: u32, mime: &str, bitrate: u32) -> AudioFormat {
    AudioFormat {
        itag,
        mime: mime.into(),
        bitrate,
        content_length: None,
        url: Some(format!(
            "https://rr1---sn-test.googlevideo.com/v?itag={itag}"
        )),
        signature_cipher: None,
    }
}

#[test]
fn prefers_opus_then_aac() {
    let all = [
        format(251, "audio/webm; codecs=\"opus\"", 160_000),
        format(141, "audio/mp4; codecs=\"mp4a.40.2\"", 256_000),
        format(774, "audio/webm; codecs=\"opus\"", 256_000),
        format(140, "audio/mp4; codecs=\"mp4a.40.2\"", 128_000),
    ];
    assert_eq!(pick_format(&all).unwrap().itag, 774);
    assert_eq!(pick_format(&all[..2]).unwrap().itag, 141);
    // 141 wins over a higher-bitrate other format.
    let mut louder = all[..2].to_vec();
    louder[0].bitrate = 999_000;
    assert_eq!(pick_format(&louder).unwrap().itag, 141);
}

#[test]
fn falls_back_to_best_audio() {
    let all = [
        format(140, "audio/mp4; codecs=\"mp4a.40.2\"", 128_000),
        format(251, "audio/webm; codecs=\"opus\"", 160_000),
        format(250, "audio/webm; codecs=\"opus\"", 64_000),
    ];
    assert_eq!(pick_format(&all).unwrap().itag, 251);
    assert!(pick_format(&[]).is_none());
}

// ---- the rig ---------------------------------------------------------------------------

/// One recorded solver call: player id, player code, requests.
type SolveCall = (String, Option<String>, Vec<(ChallengeKind, Vec<String>)>);

/// Answers each challenge from a fixed map, or fails every call.
#[derive(Default)]
struct FakeSolver {
    answers: HashMap<String, String>,
    fail: bool,
    /// Player versions it fails on (as the frozen scripts would on a newer player).
    fail_players: Vec<String>,
    /// What `has_player` says: true means `Streams` need not send the player code.
    cached: bool,
    /// The error a failing call returns (a JS failure, `stream_failed`, if `None`).
    error: Option<Error>,
    calls: Mutex<Vec<SolveCall>>,
}

impl FakeSolver {
    fn mapping(pairs: &[(&str, &str)]) -> FakeSolver {
        FakeSolver {
            answers: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cached: true,
            ..Default::default()
        }
    }
    fn failing() -> FakeSolver {
        FakeSolver {
            fail: true,
            cached: true,
            ..Default::default()
        }
    }
    /// Fails every call with `error` (our own faults, not the scripts').
    fn failing_with(error: Error) -> FakeSolver {
        FakeSolver {
            error: Some(error),
            ..FakeSolver::failing()
        }
    }
}

#[async_trait]
impl ChallengeSolver for FakeSolver {
    fn has_player(&self, _player_id: &str) -> bool {
        self.cached
    }
    async fn solve_batch(
        &self,
        player_id: &str,
        player_code: Option<String>,
        requests: Vec<(ChallengeKind, Vec<String>)>,
    ) -> Result<Vec<Answers>, Error> {
        self.calls
            .lock()
            .unwrap()
            .push((player_id.into(), player_code, requests.clone()));
        if self.fail || self.fail_players.iter().any(|p| p == player_id) {
            return Err(self
                .error
                .clone()
                .unwrap_or_else(|| Error::StreamFailed("challenge solver: fake failure".into())));
        }
        Ok(requests
            .iter()
            .map(|(_, cs)| {
                cs.iter()
                    .map(|c| (c.clone(), self.answers[c].clone()))
                    .collect()
            })
            .collect())
    }
}

/// Hands back a fixed `-j` answer, or fails, and records what it was called with.
struct FakeYtDlp {
    out: Result<Vec<u8>, Error>,
    calls: Mutex<Vec<(String, Session)>>,
}

impl FakeYtDlp {
    fn answering(v: Value) -> FakeYtDlp {
        FakeYtDlp {
            out: Ok(serde_json::to_vec(&v).unwrap()),
            calls: Mutex::default(),
        }
    }
    fn failing() -> FakeYtDlp {
        FakeYtDlp {
            out: Err(Error::StreamFailed("yt-dlp failed".into())),
            calls: Mutex::default(),
        }
    }
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl YtDlp for FakeYtDlp {
    async fn info_json(&self, video_id: &str, session: &Session) -> Result<Vec<u8>, Error> {
        self.calls
            .lock()
            .unwrap()
            .push((video_id.into(), session.clone()));
        self.out.clone()
    }
}

fn cookie(domain: &str, name: &str, value: &str, secure: bool) -> Cookie {
    Cookie {
        domain: domain.into(),
        name: name.into(),
        value: value.into(),
        path: "/".into(),
        secure,
        expires_utc: None,
    }
}

fn session() -> Session {
    Session {
        cookies: vec![
            cookie(".youtube.com", "SAPISID", "fake-sapisid", true),
            cookie("127.0.0.1", "SAPISID", "fake-sapisid", false),
        ],
    }
}

/// A `player` answer with one audio format built from `format`.
fn answer(format: Value) -> Value {
    json!({
        "playabilityStatus": {"status": "OK"},
        "videoDetails": {
            "videoId": VIDEO,
            "title": "Test Song",
            "author": "Test Artist",
            "lengthSeconds": "201",
            "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/vi/x/hq.jpg", "width": 480}]}
        },
        "streamingData": {"adaptiveFormats": [format]},
        "playerConfig": {"audioConfig": {"loudnessDb": 3.5}},
        "playbackTracking": {
            "videostatsPlaybackUrl": {"baseUrl": "https://s.youtube.com/api/stats/playback"}
        }
    })
}

fn stream_url(expire: u64, extra: &str) -> String {
    format!("https://rr1---sn-test.googlevideo.com/videoplayback?expire={expire}&itag=774{extra}")
}

fn url_format(url: &str) -> Value {
    json!({"itag": 774, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 256000,
           "contentLength": "4000000", "url": url})
}

fn cipher_format(cipher: &str) -> Value {
    json!({"itag": 774, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 256000,
           "contentLength": "4000000", "signatureCipher": cipher})
}

struct Rig {
    server: MockServer,
    streams: Streams,
    solver: Arc<FakeSolver>,
    ytdlp: Arc<FakeYtDlp>,
    api: Arc<Innertube>,
    session: Arc<Mutex<Session>>,
    cache: tempfile::TempDir,
}

impl Rig {
    /// A new `Streams` on the same cache folder, server and fakes: a restarted engine.
    fn restart(&self) -> Streams {
        Streams::new(
            self.api.clone(),
            self.session.clone(),
            self.solver.clone(),
            self.ytdlp.clone(),
            self.cache.path().to_path_buf(),
        )
        .with_web_base(Url::parse(&self.server.uri()).unwrap())
    }
}

/// Serves `id` as the current player version, for the next `times` asks (all, if `None`).
async fn mount_player_version(server: &MockServer, id: &str, times: Option<u64>) {
    let mock = Mock::given(method("GET"))
        .and(path("/iframe_api"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r"var scriptUrl = 'https:\/\/www.youtube.com\/s\/player\/{id}\/www-widgetapi.vflset\/www-widgetapi.js';"
        )));
    match times {
        Some(n) => mock.up_to_n_times(n).mount(server).await,
        None => mock.mount(server).await,
    }
    Mock::given(method("GET"))
        .and(path(format!(
            "/s/player/{id}/player_ias.vflset/en_US/base.js"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_JS))
        .mount(server)
        .await;
}

/// Mounts the player version and script; the `player` answer is mounted by each test (so it
/// can count calls) unless `player` is given.
async fn rig(player: Option<Value>, solver: FakeSolver, ytdlp: FakeYtDlp) -> Rig {
    let server = MockServer::start().await;
    mount_player_version(&server, PLAYER, None).await;
    if let Some(p) = player {
        mount_player(&server, p, None).await;
    }
    let base = Url::parse(&server.uri()).unwrap();
    let session = Arc::new(Mutex::new(session()));
    let api = Arc::new(Innertube::new(
        session.clone(),
        Arc::new(MemoryStore::new()),
        base.clone(),
    ));
    let solver = Arc::new(solver);
    let ytdlp = Arc::new(ytdlp);
    let cache = tempfile::tempdir().unwrap();
    let streams = Streams::new(
        api.clone(),
        session.clone(),
        solver.clone(),
        ytdlp.clone(),
        cache.path().to_path_buf(),
    )
    .with_web_base(base);
    Rig {
        server,
        streams,
        solver,
        ytdlp,
        api,
        session,
        cache,
    }
}

async fn mount_player(server: &MockServer, body: Value, expect: Option<u64>) {
    let mock = Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body));
    match expect {
        Some(n) => mock.expect(n).mount(server).await,
        None => mock.mount(server).await,
    }
}

// ---- own-code path ---------------------------------------------------------------------

#[tokio::test]
async fn deciphers_signature_cipher() {
    let expire = now() + 6 * 3600;
    let u = stream_url(expire, "");
    let cipher = format!("s=AB&sp=sig&url={}", urlencode(&u));
    let r = rig(
        Some(answer(cipher_format(&cipher))),
        FakeSolver::mapping(&[("AB", "XY")]),
        FakeYtDlp::failing(),
    )
    .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.url, format!("{u}&sig=XY"));
    assert_eq!(s.itag, 774);
    // One solver call, for the signature only (this link has no `n`).
    let calls = r.solver.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PLAYER);
    assert_eq!(
        calls[0].2,
        vec![(ChallengeKind::Sig, vec!["AB".to_string()])]
    );
    assert_eq!(r.ytdlp.calls(), 0);
}

#[tokio::test]
async fn signature_param_defaults_to_signature() {
    // No `sp`: yt-dlp's default name is `signature`. The solved value is URL-encoded.
    let expire = now() + 6 * 3600;
    let u = stream_url(expire, "");
    let cipher = format!("s=AB&url={}", urlencode(&u));
    let r = rig(
        Some(answer(cipher_format(&cipher))),
        FakeSolver::mapping(&[("AB", "X=Y/Z")]),
        FakeYtDlp::failing(),
    )
    .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.url, format!("{u}&signature=X%3DY%2FZ"));
}

#[tokio::test]
async fn replaces_n() {
    let expire = now() + 6 * 3600;
    let u = stream_url(expire, "&n=abc&lmt=1");
    let r = rig(
        Some(answer(url_format(&u))),
        FakeSolver::mapping(&[("abc", "def")]),
        FakeYtDlp::failing(),
    )
    .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.url, stream_url(expire, "&n=def&lmt=1"));
    // Everything else comes from the `player` answer.
    assert_eq!(s.video_id, VIDEO);
    assert_eq!(s.mime, "audio/webm; codecs=\"opus\"");
    assert_eq!(s.content_length, Some(4_000_000));
    assert_eq!(s.loudness_db, Some(3.5));
    assert_eq!(s.meta.title, "Test Song");
    assert_eq!(s.meta.artist, "Test Artist");
    assert_eq!(s.meta.length_seconds, 201);
    assert_eq!(
        s.meta.thumbnail.as_deref(),
        Some("https://i.ytimg.com/vi/x/hq.jpg")
    );
    assert_eq!(
        s.tracking.playback_url.as_deref(),
        Some("https://s.youtube.com/api/stats/playback")
    );
}

#[tokio::test]
async fn cipher_and_n_are_solved_in_one_call() {
    let expire = now() + 6 * 3600;
    let u = stream_url(expire, "&n=abc");
    let cipher = format!("s=AB&sp=sig&url={}", urlencode(&u));
    let r = rig(
        Some(answer(cipher_format(&cipher))),
        FakeSolver::mapping(&[("AB", "XY"), ("abc", "def")]),
        FakeYtDlp::failing(),
    )
    .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.url, format!("{}&sig=XY", stream_url(expire, "&n=def")));
    let calls = r.solver.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "both challenges go in one solver call");
    assert_eq!(calls[0].2.len(), 2);
}

#[tokio::test]
async fn expiry_from_url() {
    let expire = now() + 6 * 3600;
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "")))),
        FakeSolver::mapping(&[]),
        FakeYtDlp::failing(),
    )
    .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.expires_unix, expire - 30 * 60);
    // No challenge on this link: the solver isn't called at all.
    assert!(r.solver.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn passes_player_code_when_not_cached() {
    let expire = now() + 6 * 3600;
    let mut solver = FakeSolver::mapping(&[("abc", "def")]);
    solver.cached = false;
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "&n=abc")))),
        solver,
        FakeYtDlp::failing(),
    )
    .await;
    r.streams.resolve(VIDEO).await.unwrap();
    let calls = r.solver.calls.lock().unwrap();
    assert_eq!(calls[0].1.as_deref(), Some(PLAYER_JS));
}

#[tokio::test]
async fn sts_reaches_the_player_request() {
    let expire = now() + 6 * 3600;
    let r = rig(None, FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    mount_player(&r.server, answer(url_format(&stream_url(expire, ""))), None).await;
    r.streams.resolve(VIDEO).await.unwrap();
    let requests = r.server.received_requests().await.unwrap();
    let player = requests
        .iter()
        .find(|q| q.url.path() == "/youtubei/v1/player")
        .unwrap();
    let body: Value = serde_json::from_slice(&player.body).unwrap();
    assert_eq!(
        body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"],
        20725
    );
}

#[tokio::test]
async fn off_allowlist_cipher_url_is_refused() {
    let cipher = format!(
        "s=AB&sp=sig&url={}",
        urlencode("https://evil.example/videoplayback?expire=1&tok=SECRET")
    );
    let r = rig(
        Some(answer(cipher_format(&cipher))),
        FakeSolver::mapping(&[("AB", "XY")]),
        FakeYtDlp::failing(),
    )
    .await;
    let e = r.streams.resolve(VIDEO).await.unwrap_err();
    assert_eq!(e.code(), "stream_failed");
    assert!(!format!("{e} {e:?}").contains("SECRET"));
    assert!(!format!("{e} {e:?}").contains("://"));
}

#[tokio::test]
async fn odd_signature_param_is_refused() {
    let u = stream_url(now() + 3600, "");
    let cipher = format!("s=AB&sp={}&url={}", urlencode("sig&x=1"), urlencode(&u));
    let r = rig(
        Some(answer(cipher_format(&cipher))),
        FakeSolver::mapping(&[("AB", "XY")]),
        FakeYtDlp::failing(),
    )
    .await;
    assert_eq!(
        r.streams.resolve(VIDEO).await.unwrap_err().code(),
        "stream_failed"
    );
    assert!(r.solver.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_audio_format_is_unavailable() {
    let mut a = answer(json!({}));
    a["streamingData"]["adaptiveFormats"] = json!([]);
    let r = rig(Some(a), FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    assert_eq!(
        r.streams.resolve(VIDEO).await,
        Err(Error::Unavailable("no audio format".into()))
    );
    // The song itself is the problem: yt-dlp isn't asked.
    assert_eq!(r.ytdlp.calls(), 0);
}

#[tokio::test]
async fn unplayable_is_unavailable() {
    let a = json!({"playabilityStatus": {"status": "UNPLAYABLE", "reason": "Not in your country"}});
    let r = rig(Some(a), FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    assert_eq!(
        r.streams.resolve(VIDEO).await,
        Err(Error::Unavailable("Not in your country".into()))
    );
}

#[tokio::test]
async fn not_a_video_id_is_refused() {
    let r = rig(None, FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    for bad in [
        "",
        "short",
        "../../etc/x",
        "abcdefghijk&list=x",
        "abcdefghij!",
    ] {
        assert_eq!(
            r.streams.resolve(bad).await.unwrap_err().code(),
            "unavailable",
            "{bad}"
        );
    }
    assert_eq!(r.ytdlp.calls(), 0, "a bad id never reaches yt-dlp");
}

// ---- link cache ------------------------------------------------------------------------

#[tokio::test]
async fn cache_reuses_link_until_expiry() {
    // A link good for hours is reused; `resolve_fresh` skips the cache.
    let expire = now() + 6 * 3600;
    let r = rig(None, FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    mount_player(
        &r.server,
        answer(url_format(&stream_url(expire, ""))),
        Some(2),
    )
    .await;
    let a = r.streams.resolve(VIDEO).await.unwrap();
    let b = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(a, b);
    let c = r.streams.resolve_fresh(VIDEO).await.unwrap();
    assert_eq!(a.url, c.url);
    r.server.verify().await;

    // A link within 30 minutes of its expiry is not reused.
    let soon = now() + 10 * 60;
    let r = rig(None, FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    mount_player(
        &r.server,
        answer(url_format(&stream_url(soon, ""))),
        Some(2),
    )
    .await;
    r.streams.resolve(VIDEO).await.unwrap();
    r.streams.resolve(VIDEO).await.unwrap();
    r.server.verify().await;
}

// ---- yt-dlp fallback -------------------------------------------------------------------

#[tokio::test]
async fn uses_ytdlp_when_solver_fails() {
    let expire = now() + 6 * 3600;
    let fallback_url =
        format!("https://rr2---sn-test.googlevideo.com/videoplayback?expire={expire}");
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "&n=abc")))),
        FakeSolver::failing(),
        FakeYtDlp::answering(json!({
            "id": VIDEO,
            "url": fallback_url,
            "format_id": "774",
            "filesize": 4000000,
            "ext": "webm",
            "acodec": "opus",
            "title": "Test Song",
            "artist": "Test Artist",
            "uploader": "Test Uploader",
            "duration": 201.0,
            "thumbnail": "https://i.ytimg.com/vi/x/hq.jpg"
        })),
    )
    .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.url, fallback_url);
    assert_eq!(s.itag, 774);
    assert_eq!(s.loudness_db, None);
    assert_eq!(s.mime, "audio/webm; codecs=\"opus\"");
    assert_eq!(s.content_length, Some(4_000_000));
    assert_eq!(s.expires_unix, expire - 30 * 60);
    assert_eq!(s.meta.title, "Test Song");
    assert_eq!(s.meta.artist, "Test Artist");
    assert_eq!(s.meta.length_seconds, 201);
    // yt-dlp got the in-memory session, nothing else.
    let calls = r.ytdlp.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, VIDEO);
    assert_eq!(calls[0].1, session());
}

#[tokio::test]
async fn ytdlp_answer_is_checked() {
    // Off the allowlist, or no URL at all: refused, and the own-code error is what's reported.
    for bad in [
        json!({"id": VIDEO, "url": "https://evil.example/v?expire=1", "format_id": "251"}),
        json!({"id": VIDEO, "format_id": "251"}),
        json!({"id": "otherid0000", "url": "https://rr2---sn-test.googlevideo.com/v", "format_id": "251"}),
    ] {
        let r = rig(
            Some(answer(url_format(&stream_url(now() + 3600, "&n=abc")))),
            FakeSolver::failing(),
            FakeYtDlp::answering(bad.clone()),
        )
        .await;
        let e = r.streams.resolve(VIDEO).await.unwrap_err();
        assert_eq!(e.code(), "stream_failed", "{bad}");
        assert!(!format!("{e:?}").contains("://"));
    }
}

// ---- what never falls back --------------------------------------------------------------

#[tokio::test]
async fn signed_out_never_falls_back_to_ytdlp() {
    // YouTube refused the session. yt-dlp would usually still get an anonymous, lower-quality
    // link a few seconds later, and the widget would never learn the user must sign in again.
    let expire = now() + 6 * 3600;
    let login_required: Value =
        serde_json::from_str(include_str!("fixtures/player_login_required.json")).unwrap();
    let r = rig(
        Some(login_required),
        FakeSolver::mapping(&[]),
        FakeYtDlp::answering(fallback_answer(expire)),
    )
    .await;
    assert_eq!(r.streams.resolve(VIDEO).await, Err(Error::SignedOut));
    assert_eq!(r.ytdlp.calls(), 0);

    // The same when the session has no sign-in cookie at all.
    *r.session.lock().unwrap() = Session::default();
    assert_eq!(r.streams.resolve_fresh(VIDEO).await, Err(Error::SignedOut));
    assert_eq!(r.ytdlp.calls(), 0);
}

#[tokio::test]
async fn unavailable_never_falls_back_to_ytdlp() {
    let expire = now() + 6 * 3600;
    let a = json!({"playabilityStatus": {"status": "UNPLAYABLE", "reason": "Not in your country"}});
    let r = rig(
        Some(a),
        FakeSolver::mapping(&[]),
        FakeYtDlp::answering(fallback_answer(expire)),
    )
    .await;
    assert_eq!(
        r.streams.resolve(VIDEO).await,
        Err(Error::Unavailable("Not in your country".into()))
    );
    assert_eq!(r.ytdlp.calls(), 0);
}

#[tokio::test]
async fn network_failure_still_falls_back_to_ytdlp() {
    let expire = now() + 6 * 3600;
    let r = rig(
        None,
        FakeSolver::mapping(&[]),
        FakeYtDlp::answering(fallback_answer(expire)),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&r.server)
        .await;
    assert_eq!(r.streams.resolve(VIDEO).await.unwrap().itag, 251);
    assert_eq!(r.ytdlp.calls(), 1);
}

// ---- player versions the solver can't handle -------------------------------------------

fn fallback_answer(expire: u64) -> Value {
    json!({
        "id": VIDEO,
        "url": format!("https://rr2---sn-test.googlevideo.com/videoplayback?expire={expire}"),
        "format_id": "251",
    })
}

fn solver_failing_on(players: &[&str]) -> FakeSolver {
    FakeSolver {
        fail_players: players.iter().map(|p| p.to_string()).collect(),
        ..FakeSolver::mapping(&[("abc", "def")])
    }
}

#[tokio::test]
async fn failed_player_goes_straight_to_ytdlp() {
    let expire = now() + 6 * 3600;
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "&n=abc")))),
        solver_failing_on(&[PLAYER]),
        FakeYtDlp::answering(fallback_answer(expire)),
    )
    .await;
    assert_eq!(r.streams.resolve_fresh(VIDEO).await.unwrap().itag, 251);
    assert_eq!(r.solver.calls.lock().unwrap().len(), 1);
    // Same player version: the solver is not tried again, yt-dlp answers.
    assert_eq!(r.streams.resolve_fresh(VIDEO).await.unwrap().itag, 251);
    assert_eq!(r.solver.calls.lock().unwrap().len(), 1);
    assert_eq!(r.ytdlp.calls(), 2);
    // Nor after a restart: the failure is remembered next to the player cache.
    let restarted = r.restart();
    assert_eq!(restarted.resolve_fresh(VIDEO).await.unwrap().itag, 251);
    assert_eq!(r.solver.calls.lock().unwrap().len(), 1);
    assert!(
        r.cache
            .path()
            .join("players")
            .join(format!("{PLAYER}.failed"))
            .exists()
    );
}

#[tokio::test]
async fn our_own_faults_do_not_mark_the_player() {
    // A stopped solver thread, or a player script pruned between `has_player` and the call,
    // says nothing about whether the scripts can solve this player: the next song tries the
    // own-code path again, and nothing is written for a restarted engine to skip.
    for fault in [
        Error::Internal("the challenge solver stopped".into()),
        Error::Internal("the player script is needed but was not given".into()),
    ] {
        let expire = now() + 6 * 3600;
        let r = rig(
            Some(answer(url_format(&stream_url(expire, "&n=abc")))),
            FakeSolver::failing_with(fault.clone()),
            FakeYtDlp::answering(fallback_answer(expire)),
        )
        .await;
        assert_eq!(r.streams.resolve_fresh(VIDEO).await.unwrap().itag, 251);
        assert_eq!(r.streams.resolve_fresh(VIDEO).await.unwrap().itag, 251);
        assert_eq!(r.solver.calls.lock().unwrap().len(), 2, "{fault:?}");
        assert!(
            !r.cache
                .path()
                .join("players")
                .join(format!("{PLAYER}.failed"))
                .exists(),
            "{fault:?}"
        );
    }
}

#[tokio::test]
async fn stale_failure_is_retried() {
    // Six hours on, a failed version is tried again (the failure may have been a deadline
    // hit on a throttled CPU), and a success clears the mark.
    let expire = now() + 6 * 3600;
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "&n=abc")))),
        FakeSolver::mapping(&[("abc", "def")]),
        FakeYtDlp::failing(),
    )
    .await;
    let players = r.cache.path().join("players");
    std::fs::create_dir_all(&players).unwrap();
    let marker = players.join(format!("{PLAYER}.failed"));
    std::fs::write(&marker, "").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7 * 3600))
        .unwrap();
    let s = r.streams.resolve_fresh(VIDEO).await.unwrap();
    assert_eq!(s.url, stream_url(expire, "&n=def"));
    assert_eq!(r.solver.calls.lock().unwrap().len(), 1);
    assert!(!marker.exists(), "a success clears the mark");

    // A fresh mark is honoured.
    std::fs::write(&marker, "").unwrap();
    let restarted = r.restart();
    assert!(restarted.resolve_fresh(VIDEO).await.is_err());
    assert_eq!(r.solver.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn new_player_id_retries_own_path() {
    let expire = now() + 6 * 3600;
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "&n=abc")))),
        solver_failing_on(&[PLAYER]),
        FakeYtDlp::answering(fallback_answer(expire)),
    )
    .await;
    // The old version for the first ask, then a new one.
    r.server.reset().await;
    mount_player_version(&r.server, PLAYER, Some(1)).await;
    mount_player_version(&r.server, "0000000b", None).await;
    mount_player(
        &r.server,
        answer(url_format(&stream_url(expire, "&n=abc"))),
        None,
    )
    .await;

    assert_eq!(r.streams.resolve(VIDEO).await.unwrap().itag, 251);
    // The failure also dropped the remembered player version, so the next resolve asks for
    // the current one at once instead of reusing the failed one for an hour.
    let s = r.streams.resolve_fresh(VIDEO).await.unwrap();
    assert_eq!(s.itag, 774);
    assert_eq!(s.url, stream_url(expire, "&n=def"));
    let calls = r.solver.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].0, "0000000b");
}

#[tokio::test]
async fn failure_forgets_the_player_version() {
    // Any own-code failure drops the remembered version: the next resolve asks iframe_api
    // again rather than reusing it for up to an hour.
    let r = rig(None, FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    let a = json!({"playabilityStatus": {"status": "ERROR"}});
    mount_player(&r.server, a, None).await;
    assert!(r.streams.resolve(VIDEO).await.is_err());
    assert!(r.streams.resolve(VIDEO).await.is_err());
    let asks = r
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|q| q.url.path() == "/iframe_api")
        .count();
    assert_eq!(asks, 2);
}

/// A stand-in yt-dlp: a shell script that records how it was called and what the cookie file
/// looked like while it ran, then prints a `-j` answer (or sleeps, for the timeout test).
fn fake_ytdlp_script(dir: &std::path::Path, sleep: bool) -> PathBuf {
    let out = dir.display();
    let tail = if sleep {
        // A child of its own, as yt-dlp starts a JS runtime: it must die with the run.
        format!("sleep 30 &\nprintf '%s' \"$!\" > \"{out}/grandchild\"\nsleep 30")
    } else {
        r#"printf '{"id":"testvideo01","url":"https://rr2---sn-test.googlevideo.com/v","format_id":"141"}'"#.to_string()
    };
    let script = format!(
        r#"prev=""
cookie=""
for a in "$@"; do
  if [ "$prev" = "--cookies" ]; then cookie="$a"; fi
  prev="$a"
done
stat -c %a "$cookie" > "{out}/mode"
stat -c %a "$(dirname "$cookie")" > "{out}/dirmode"
cat "$cookie" > "{out}/content"
printf '%s' "$cookie" > "{out}/path"
printf '%s\n' "$@" > "{out}/args"
{tail}
"#
    );
    let path = dir.join("fake-yt-dlp.sh");
    std::fs::write(&path, script).unwrap();
    path
}

#[tokio::test]
async fn ytdlp_cookie_file_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().join("run");
    std::fs::create_dir(&runtime).unwrap();
    let script = fake_ytdlp_script(tmp.path(), false);
    // Run through /bin/sh rather than executing the fresh file: exec of a file just written
    // can fail with ETXTBSY while another test thread is forking.
    let ytdlp = YtDlpCommand::new(runtime.clone()).with_program(
        "/bin/sh".into(),
        vec![script.into()],
        Duration::from_secs(30),
    );
    let out = ytdlp.info_json(VIDEO, &session()).await.unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("\"format_id\":\"141\"")
    );

    let read = |name: &str| std::fs::read_to_string(tmp.path().join(name)).unwrap();
    assert_eq!(read("mode").trim(), "600");
    assert_eq!(read("dirmode").trim(), "700");
    let content = read("content");
    assert!(
        content.starts_with("# Netscape HTTP Cookie File\n"),
        "{content}"
    );
    assert!(content.contains(".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfake-sapisid\n"));
    assert!(content.contains("127.0.0.1\tFALSE\t/\tFALSE\t0\tSAPISID\tfake-sapisid\n"));
    let cookie_path = PathBuf::from(read("path"));
    assert!(cookie_path.starts_with(&runtime));
    assert!(!cookie_path.exists(), "the cookie file is removed");
    assert!(!cookie_path.parent().unwrap().exists(), "and its folder");
    let args: Vec<String> = read("args").lines().map(String::from).collect();
    for want in [
        "--ignore-config",
        "--no-cookies-from-browser",
        "--no-warnings",
        "-q",
        "774/141/bestaudio",
        "-j",
        "https://music.youtube.com/watch?v=testvideo01",
    ] {
        assert!(args.iter().any(|a| a == want), "{want} in {args:?}");
    }
}

#[tokio::test]
async fn ytdlp_timeout_kills_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().join("run");
    std::fs::create_dir(&runtime).unwrap();
    let script = fake_ytdlp_script(tmp.path(), true);
    let ytdlp = YtDlpCommand::new(runtime.clone()).with_program(
        "/bin/sh".into(),
        vec![script.into()],
        Duration::from_millis(500),
    );
    let start = std::time::Instant::now();
    let e = ytdlp.info_json(VIDEO, &session()).await.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(e.code(), "stream_failed");
    // The script wrote down the cookie path before it slept; the kill must still clean up.
    let cookie_path = PathBuf::from(std::fs::read_to_string(tmp.path().join("path")).unwrap());
    assert!(!cookie_path.exists());
    assert_eq!(std::fs::read_dir(&runtime).unwrap().count(), 0);
    assert_gone(&grandchild(tmp.path())).await;
}

#[tokio::test]
async fn ytdlp_cancel_kills_its_children() {
    // The caller drops the call mid-run (the user skipped): yt-dlp and everything it started
    // must go, and the cookie folder with them.
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().join("run");
    std::fs::create_dir(&runtime).unwrap();
    let script = fake_ytdlp_script(tmp.path(), true);
    let ytdlp = YtDlpCommand::new(runtime.clone()).with_program(
        "/bin/sh".into(),
        vec![script.into()],
        Duration::from_secs(30),
    );
    let session = session();
    let call = ytdlp.info_json(VIDEO, &session);
    // Long enough for the script to start its child and note it down.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), call)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&runtime).unwrap().count(), 0);
    assert_gone(&grandchild(tmp.path())).await;
}

fn grandchild(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("grandchild")).expect("the script noted its child")
}

/// Waits up to 3 s for process `pid` to be gone (or a zombie waiting for its new parent).
async fn assert_gone(pid: &str) {
    let stat = format!("/proc/{pid}/stat");
    for _ in 0..60 {
        match std::fs::read_to_string(&stat) {
            Err(_) => return,
            // The state is the field after the `(comm)`.
            Ok(s)
                if s.rsplit_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z')) =>
            {
                return;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("process {pid} is still running");
}

#[tokio::test]
async fn ytdlp_missing_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let ytdlp = YtDlpCommand::new(tmp.path().to_path_buf()).with_program(
        tmp.path().join("no-such-program"),
        vec![],
        Duration::from_secs(5),
    );
    assert_eq!(
        ytdlp.info_json(VIDEO, &session()).await.unwrap_err().code(),
        "stream_failed"
    );
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// `application/x-www-form-urlencoded` value encoding, as YouTube's `signatureCipher` uses.
fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ---- song details when the TV answer has none --------------------------------------------

/// The TV `player` answer on the real account came back with no title or author (a live
/// play printed an empty title line). The link still resolves; the details then come from
/// YouTube's oEmbed answer, which needs no session.
#[tokio::test]
async fn details_come_from_oembed_when_the_answer_has_none() {
    let expire = now() + 6 * 3600;
    let mut a = answer(url_format(&stream_url(expire, "")));
    a.as_object_mut().unwrap().remove("videoDetails");
    let r = rig(Some(a), FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .and(query_param("format", "json"))
        .and(query_param(
            "url",
            format!("https://www.youtube.com/watch?v={VIDEO}"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "title": "Oembed Song",
            "author_name": "Oembed Artist",
            "type": "video"
        })))
        .expect(1)
        .mount(&r.server)
        .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.meta.title, "Oembed Song");
    assert_eq!(s.meta.artist, "Oembed Artist");
    // No session goes to oEmbed.
    let reqs = r.server.received_requests().await.unwrap();
    let oembed = reqs.iter().find(|q| q.url.path() == "/oembed").unwrap();
    assert!(oembed.headers.get("cookie").is_none());
    assert!(oembed.headers.get("authorization").is_none());
}

#[tokio::test]
async fn details_missing_everywhere_still_plays() {
    let expire = now() + 6 * 3600;
    let mut a = answer(url_format(&stream_url(expire, "")));
    a.as_object_mut().unwrap().remove("videoDetails");
    let r = rig(Some(a), FakeSolver::mapping(&[]), FakeYtDlp::failing()).await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&r.server)
        .await;
    let s = r.streams.resolve(VIDEO).await.unwrap();
    assert_eq!(s.meta.title, "");
    assert_eq!(s.itag, 774);
}

#[tokio::test]
async fn details_in_the_answer_skip_oembed() {
    let expire = now() + 6 * 3600;
    let r = rig(
        Some(answer(url_format(&stream_url(expire, "")))),
        FakeSolver::mapping(&[]),
        FakeYtDlp::failing(),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&r.server)
        .await;
    assert_eq!(
        r.streams.resolve(VIDEO).await.unwrap().meta.title,
        "Test Song"
    );
}
