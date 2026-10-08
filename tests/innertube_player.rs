//! The InnerTube `player` request against a local wiremock server.
//!
//! Every fixture is hand-made: fake ids, fake URLs, fake cookie values. Nothing here talks to
//! YouTube. Wiremock speaks plain http, so the test session's cookies for it are `secure: false`
//! (a `Secure` cookie is never sent over http), and the server's base URL is injected through
//! `Innertube::new`, the one place the https allowlist is bypassed (ruling R7).

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::auth::{Cookie, MemoryStore, Session, SessionStore};
use ytmfast::error::Error;
use ytmfast::innertube::{Innertube, clients};

const PREMIUM: &str = include_str!("fixtures/player_tv_premium.json");
const LOGIN_REQUIRED: &str = include_str!("fixtures/player_login_required.json");
const UNPLAYABLE: &str = include_str!("fixtures/player_unplayable.json");

const STS: u32 = 20_300;

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

/// A signed-in test session. The `.youtube.com` SAPISID signs the request (the signature is
/// made for the `https://www.youtube.com` origin, whatever host the request goes to); the
/// `127.0.0.1` cookies are the ones the wiremock host gets in its `Cookie` header.
fn session() -> Session {
    Session {
        cookies: vec![
            cookie(".youtube.com", "SAPISID", "fake-sapisid", true),
            cookie("127.0.0.1", "SAPISID", "fake-sapisid", false),
            cookie("127.0.0.1", "ROTATE", "old", false),
        ],
        account: None,
    }
}

struct Rig {
    server: MockServer,
    store: Arc<MemoryStore>,
    session: Arc<Mutex<Session>>,
    api: Innertube,
}

async fn rig_with(s: Session) -> Rig {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryStore::new());
    let session = Arc::new(Mutex::new(s));
    let base = Url::parse(&server.uri()).unwrap();
    let api = Innertube::new(session.clone(), store.clone(), base);
    Rig {
        server,
        store,
        session,
        api,
    }
}

async fn rig() -> Rig {
    rig_with(session()).await
}

fn player_mock(body: ResponseTemplate) -> Mock {
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .and(query_param("prettyPrint", "false"))
        .respond_with(body)
}

fn json_answer(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn header<'a>(req: &'a wiremock::Request, name: &str) -> &'a str {
    req.headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn player_request_shape() {
    let rig = rig().await;
    player_mock(json_answer(PREMIUM))
        .expect(1)
        .mount(&rig.server)
        .await;

    rig.api.player("FAKEVID0001", STS).await.unwrap();

    let reqs = rig.server.received_requests().await.unwrap();
    let req = &reqs[0];
    assert_eq!(req.url.query(), Some("prettyPrint=false"));
    assert_eq!(header(req, "x-youtube-client-name"), "7");
    assert_eq!(header(req, "x-youtube-client-version"), "5.20260707");
    assert_eq!(header(req, "origin"), "https://www.youtube.com");
    assert_eq!(header(req, "x-origin"), "https://www.youtube.com");
    assert_eq!(
        header(req, "user-agent"),
        "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1"
    );
    assert_eq!(header(req, "content-type"), "application/json");
    // Step 1's TV request never sent it; the music client's does (innertube_next.rs).
    assert!(req.headers.get("x-goog-authuser").is_none());
    let auth = header(req, "authorization");
    assert!(auth.starts_with("SAPISIDHASH "), "authorization scheme");
    // Only the cookies for the request's own host, in the stored order.
    assert_eq!(header(req, "cookie"), "SAPISID=fake-sapisid; ROTATE=old");

    let body: Value = serde_json::from_slice(&req.body).unwrap();
    let client = &body["context"]["client"];
    assert_eq!(client["clientName"], "TVHTML5");
    assert_eq!(client["clientVersion"], "5.20260707");
    assert_eq!(
        client["userAgent"],
        "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1"
    );
    assert_eq!(client["hl"], "en");
    // The device, without which YouTube answers "The page needs to be reloaded." (clients.rs).
    assert_eq!(client["deviceMake"], "Samsung");
    assert_eq!(client["osName"], "Tizen");
    assert_eq!(client["timeZone"], "UTC");
    assert_eq!(client["utcOffsetMinutes"], 0);
    assert_eq!(body["videoId"], "FAKEVID0001");
    let playback = &body["playbackContext"]["contentPlaybackContext"];
    assert_eq!(playback["signatureTimestamp"], STS);
    assert_eq!(playback["html5Preference"], "HTML5_PREF_WANTS");
    assert_eq!(body["contentCheckOk"], true);
    assert_eq!(body["racyCheckOk"], true);
}

#[tokio::test]
async fn parses_premium_formats() {
    let rig = rig().await;
    player_mock(json_answer(PREMIUM)).mount(&rig.server).await;

    let p = rig.api.player("FAKEVID0001", STS).await.unwrap();

    assert_eq!(p.video_id, "FAKEVID0001");
    assert_eq!(p.title, "Test Song");
    assert_eq!(p.author, "Test Artist");
    assert_eq!(p.length_seconds, 215);
    // The widest thumbnail, wherever it sits in the list.
    assert_eq!(
        p.thumbnail.as_deref(),
        Some("https://i.ytimg.com/vi/FAKEVID0001/maxresdefault.jpg")
    );
    assert_eq!(p.loudness_db, Some(5.1));

    let itags: Vec<u32> = p.formats.iter().map(|f| f.itag).collect();
    // Audio only (no 18, no 137), and itag 140's off-allowlist URL is dropped (ruling R7).
    assert_eq!(itags, vec![774, 141, 251]);

    let opus = &p.formats[0];
    assert_eq!(opus.mime, "audio/webm; codecs=\"opus\"");
    assert_eq!(opus.bitrate, 270_000);
    assert_eq!(opus.content_length, Some(7_340_032));
    assert!(
        opus.url
            .as_deref()
            .unwrap()
            .starts_with("https://rr1---sn-test.googlevideo.com/videoplayback?")
    );
    assert_eq!(opus.signature_cipher, None);

    let aac = &p.formats[1];
    assert_eq!(aac.url, None);
    assert!(
        aac.signature_cipher
            .as_deref()
            .unwrap()
            .starts_with("s=FAKESIG")
    );

    // contentLength is optional.
    assert_eq!(p.formats[2].content_length, None);

    assert!(
        p.tracking
            .playback_url
            .as_deref()
            .unwrap()
            .starts_with("https://s.youtube.com/api/stats/playback?")
    );
    assert!(
        p.tracking
            .watchtime_url
            .as_deref()
            .unwrap()
            .starts_with("https://s.youtube.com/api/stats/watchtime?")
    );
}

#[tokio::test]
async fn debug_output_hides_signed_urls() {
    let rig = rig().await;
    player_mock(json_answer(PREMIUM)).mount(&rig.server).await;
    let p = rig.api.player("FAKEVID0001", STS).await.unwrap();
    let shown = format!("{p:?}");
    assert!(!shown.contains("videoplayback"), "{shown}");
    assert!(!shown.contains("FAKESIG"), "{shown}");
    assert!(!shown.contains("api/stats"), "{shown}");
    assert!(shown.contains("774"));
}

#[tokio::test]
async fn login_required_is_signed_out() {
    let rig = rig().await;
    player_mock(json_answer(
        r#"{"playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Please sign in"}}"#,
    ))
    .mount(&rig.server)
    .await;
    assert_eq!(
        rig.api.player("FAKEVID0001", STS).await,
        Err(Error::SignedOut)
    );
}

#[tokio::test]
async fn bot_check_is_not_signed_out() {
    // The fixture's LOGIN_REQUIRED says "Sign in to confirm you're not a bot": a check on the
    // client, not a rejected session, so it is stream_failed (yt-dlp gets a try).
    let rig = rig().await;
    player_mock(json_answer(LOGIN_REQUIRED))
        .mount(&rig.server)
        .await;
    let err = rig.api.player("FAKEVID0001", STS).await.unwrap_err();
    assert_eq!(err.code(), "stream_failed", "{err}");
}

#[tokio::test]
async fn unplayable_is_a_tv_refusal() {
    // The TV client's refusal: stream_failed with YouTube's reason, so yt-dlp gets a try.
    let rig = rig().await;
    player_mock(json_answer(UNPLAYABLE))
        .mount(&rig.server)
        .await;
    assert_eq!(
        rig.api.player("FAKEVID0002", STS).await,
        Err(Error::StreamFailed(
            "This video is not available in your country".into()
        ))
    );
}

#[tokio::test]
async fn answer_for_another_video_is_refused() {
    // yt-dlp checks this too: YouTube sometimes answers with a different video. A transient,
    // so stream_failed (yt-dlp asks again).
    let rig = rig().await;
    player_mock(json_answer(PREMIUM)).mount(&rig.server).await;
    let err = rig.api.player("OTHERVID001", STS).await.unwrap_err();
    assert_eq!(err.code(), "stream_failed");
}

#[tokio::test]
async fn oversize_answer_rejected() {
    let rig = rig().await;
    let big = vec![b' '; 33 << 20];
    player_mock(ResponseTemplate::new(200).set_body_raw(big, "application/json"))
        .mount(&rig.server)
        .await;
    let err = rig.api.player("FAKEVID0001", STS).await.unwrap_err();
    assert_eq!(err.code(), "network", "{err}");
    assert!(err.to_string().contains("too large"), "{err}");
}

/// A server that answers with a chunked body (no Content-Length) that never ends. The cap must
/// cut it off while reading; a client that buffered first would only stop at the 10 s timeout.
#[tokio::test]
async fn endless_chunked_answer_is_cut_while_streaming() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Read the request head and body so the client isn't writing into a closed socket.
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            got.extend_from_slice(&buf[..n]);
            if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                if got.len() >= end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                return;
            }
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n";
        if sock.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let chunk = vec![b' '; 1 << 20];
        let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
        framed.extend_from_slice(&chunk);
        framed.extend_from_slice(b"\r\n");
        // Until the client hangs up.
        while sock.write_all(&framed).await.is_ok() {}
    });

    let store = Arc::new(MemoryStore::new());
    let api = Innertube::new(
        Arc::new(Mutex::new(session())),
        store,
        Url::parse(&format!("http://{addr}")).unwrap(),
    );
    let started = std::time::Instant::now();
    let err = api.player("FAKEVID0001", STS).await.unwrap_err();
    assert_eq!(err.code(), "network", "{err}");
    assert!(err.to_string().contains("too large"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
}

fn rotated(s: &Session) -> Option<String> {
    s.cookies
        .iter()
        .find(|c| c.name == "ROTATE")
        .map(|c| c.value.clone())
}

/// The session `store` holds once the background save has landed (polls for up to 5 s).
async fn saved_eventually(store: &dyn SessionStore) -> Session {
    for _ in 0..500 {
        if let Ok(s) = store.load().await {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the rotation was never saved");
}

#[tokio::test]
async fn set_cookie_saved() {
    let rig = rig().await;
    player_mock(json_answer(PREMIUM).append_header("set-cookie", "ROTATE=new; Path=/"))
        .mount(&rig.server)
        .await;

    rig.api.player("FAKEVID0001", STS).await.unwrap();

    // The live session the next request uses has it at once.
    assert_eq!(
        rotated(&rig.session.lock().unwrap()).as_deref(),
        Some("new")
    );
    // The store gets it from a background task, after the answer is back.
    let saved = saved_eventually(rig.store.as_ref()).await;
    assert_eq!(rotated(&saved).as_deref(), Some("new"));
}

#[tokio::test]
async fn cookie_deletion_on_401_is_saved() {
    // A rejected session's cookie deletions are applied and saved even though the request
    // fails.
    let rig = rig().await;
    player_mock(
        ResponseTemplate::new(401).append_header("set-cookie", "ROTATE=; Path=/; Max-Age=0"),
    )
    .mount(&rig.server)
    .await;
    assert_eq!(
        rig.api.player("FAKEVID0001", STS).await,
        Err(Error::SignedOut)
    );
    assert_eq!(rotated(&rig.session.lock().unwrap()), None);
    let saved = saved_eventually(rig.store.as_ref()).await;
    assert_eq!(rotated(&saved), None);
    assert_eq!(saved.cookies.len(), 2);
}

/// A store whose saves take a minute, like a keyring sitting on an unlock prompt.
struct SlowStore;

#[async_trait::async_trait]
impl SessionStore for SlowStore {
    async fn load(&self) -> Result<Session, Error> {
        Err(Error::SignedOut)
    }
    async fn save(&self, _: &Session) -> Result<(), Error> {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        Ok(())
    }
}

#[tokio::test]
async fn slow_save_does_not_block_player() {
    let server = MockServer::start().await;
    player_mock(json_answer(PREMIUM).append_header("set-cookie", "ROTATE=new; Path=/"))
        .mount(&server)
        .await;
    let api = Innertube::new(
        Arc::new(Mutex::new(session())),
        Arc::new(SlowStore),
        Url::parse(&server.uri()).unwrap(),
    );
    // Well under the request's 10 s deadline: a save awaited on the request path would
    // hold the answer for the whole minute (and then fail it as timed out).
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        api.player("FAKEVID0001", STS),
    )
    .await
    .expect("player waited for the save");
    assert_eq!(answer.unwrap().video_id, "FAKEVID0001");
}

/// Records which save method was called.
#[derive(Default)]
struct RecordingStore {
    calls: Mutex<Vec<&'static str>>,
}

#[async_trait::async_trait]
impl SessionStore for RecordingStore {
    async fn load(&self) -> Result<Session, Error> {
        Err(Error::SignedOut)
    }
    async fn save(&self, _: &Session) -> Result<(), Error> {
        self.calls.lock().unwrap().push("save");
        Ok(())
    }
    async fn save_without_prompt(&self, _: &Session) -> Result<(), Error> {
        self.calls.lock().unwrap().push("save_without_prompt");
        Ok(())
    }
}

#[tokio::test]
async fn background_save_never_prompts() {
    // Nobody is there to answer a keyring unlock prompt for a cookie rotation, so the
    // background save must use the no-prompt path.
    let server = MockServer::start().await;
    player_mock(json_answer(PREMIUM).append_header("set-cookie", "ROTATE=new; Path=/"))
        .mount(&server)
        .await;
    let store = Arc::new(RecordingStore::default());
    let api = Innertube::new(
        Arc::new(Mutex::new(session())),
        store.clone(),
        Url::parse(&server.uri()).unwrap(),
    );
    api.player("FAKEVID0001", STS).await.unwrap();
    for _ in 0..500 {
        if !store.calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(*store.calls.lock().unwrap(), vec!["save_without_prompt"]);
}

#[tokio::test]
async fn unchanged_cookie_is_not_saved() {
    let rig = rig().await;
    player_mock(json_answer(PREMIUM).append_header("set-cookie", "ROTATE=old; Path=/"))
        .mount(&rig.server)
        .await;
    rig.api.player("FAKEVID0001", STS).await.unwrap();
    // Time for a background save to land, if one had been started.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Nothing changed, so nothing was written to the store.
    assert_eq!(rig.store.load().await, Err(Error::SignedOut));
}

#[tokio::test]
async fn unauthorized_is_signed_out() {
    let rig = rig().await;
    player_mock(ResponseTemplate::new(401))
        .mount(&rig.server)
        .await;
    assert_eq!(
        rig.api.player("FAKEVID0001", STS).await,
        Err(Error::SignedOut)
    );
}

#[tokio::test]
async fn server_error_is_network() {
    let rig = rig().await;
    player_mock(ResponseTemplate::new(503))
        .mount(&rig.server)
        .await;
    let err = rig.api.player("FAKEVID0001", STS).await.unwrap_err();
    assert_eq!(err.code(), "network");
}

#[tokio::test]
async fn garbage_answer_is_an_error_without_its_text() {
    let rig = rig().await;
    player_mock(json_answer("{\"playabilityStatus\": SECRETGARBAGE"))
        .mount(&rig.server)
        .await;
    let err = rig.api.player("FAKEVID0001", STS).await.unwrap_err();
    assert!(!err.to_string().contains("SECRETGARBAGE"), "{err}");
}

#[tokio::test]
async fn no_sapisid_is_signed_out_without_a_request() {
    let rig = rig_with(Session::default()).await;
    player_mock(json_answer(PREMIUM))
        .expect(0)
        .mount(&rig.server)
        .await;
    assert_eq!(
        rig.api.player("FAKEVID0001", STS).await,
        Err(Error::SignedOut)
    );
}

#[test]
fn client_table() {
    let tv = clients::TV;
    assert_eq!(tv.name, "TVHTML5");
    assert_eq!(tv.version, "5.20260707");
    assert_eq!(tv.name_id, 7);
    assert_eq!(
        tv.user_agent,
        "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1"
    );
    assert_eq!(tv.origin, "https://www.youtube.com");
    assert_eq!(tv.api_host, "www.youtube.com");
    let music = clients::WEB_REMIX;
    assert_eq!(music.name, "WEB_REMIX");
    assert_eq!(music.name_id, 67);
    assert_eq!(music.origin, "https://music.youtube.com");
    assert_eq!(music.api_host, "music.youtube.com");
}

#[tokio::test]
async fn player_future_is_send() {
    // The daemon runs requests on spawned tasks; a lock held across an await would make the
    // future !Send and break that at compile time, so check it here.
    fn assert_send<T: Send>(_: &T) {}
    let rig = rig().await;
    let fut = rig.api.player("FAKEVID0001", STS);
    assert_send(&fut);
}
