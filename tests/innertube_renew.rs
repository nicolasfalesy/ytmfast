//! A session YouTube dropped: its answers come back as a guest's (200, `logged_in` 0), which
//! is `SignedOut`, and a client given a `Renew` hook gets a new session from it at most once per
//! interval and sends the request again. Against a local wiremock server, as
//! `innertube_browse.rs`; nothing here talks to YouTube.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::auth::{Cookie, MemoryStore, Session, SessionStore};
use ytmfast::error::Error;
use ytmfast::innertube::{Innertube, Renew};

const HOME: &str = include_str!("fixtures/browse/browse_home.json");
/// A TV `player` answer that plays, and one with the "confirm you're not a bot" check.
const PREMIUM: &str = include_str!("fixtures/player_tv_premium.json");
const BOT_CHECK: &str = include_str!("fixtures/player_login_required.json");

/// What YouTube answered every request of a dropped session with (2026-10-07), cut down to
/// the part that says so.
const GUEST: &str = r#"{"responseContext":{"serviceTrackingParams":[{"service":"GFEEDBACK","params":[{"key":"logged_in","value":"0"}]}]},"contents":{}}"#;

const ACCOUNT: &str = "UCfakefakefakefakefake00";

fn session(sapisid: &str) -> Session {
    let cookie = |domain: &str, secure| Cookie {
        domain: domain.into(),
        name: "SAPISID".into(),
        value: sapisid.into(),
        path: "/".into(),
        secure,
        expires_utc: None,
    };
    Session {
        cookies: vec![cookie(".youtube.com", true), cookie("127.0.0.1", false)],
        account: Some(ACCOUNT.into()),
        browser_stamp: None,
    }
}

/// A hook that hands out the "new" session (or fails), after `delay`, and counts its calls.
struct Hook {
    calls: AtomicUsize,
    works: bool,
    delay: Duration,
    /// The session each call was given.
    seen: Mutex<Vec<Session>>,
    /// What `stamp` answers: the browser's sign-in stamp.
    stamp: Mutex<Option<i64>>,
    /// Whether `cached_stamp` answers (the browser's file is unchanged), and how often the
    /// blocking `stamp` was called.
    cached: std::sync::atomic::AtomicBool,
    stamp_calls: AtomicUsize,
}

impl Hook {
    fn new(works: bool, delay: Duration) -> Arc<Hook> {
        Arc::new(Hook {
            calls: AtomicUsize::new(0),
            works,
            delay,
            seen: Mutex::new(Vec::new()),
            stamp: Mutex::new(None),
            cached: std::sync::atomic::AtomicBool::new(false),
            stamp_calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Renew for Hook {
    async fn renew(&self, current: &Session) -> Result<Session, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(current.clone());
        tokio::time::sleep(self.delay).await;
        if self.works {
            Ok(session("new"))
        } else {
            Err("the browser is signed out".into())
        }
    }

    fn stamp(&self) -> Option<i64> {
        self.stamp_calls.fetch_add(1, Ordering::SeqCst);
        *self.stamp.lock().unwrap()
    }

    fn cached_stamp(&self) -> Option<Option<i64>> {
        self.cached
            .load(Ordering::SeqCst)
            .then(|| *self.stamp.lock().unwrap())
    }
}

struct Rig {
    server: MockServer,
    store: Arc<MemoryStore>,
    session: Arc<Mutex<Session>>,
    api: Arc<Innertube>,
}

/// The old session gets a guest's answer, the new one the home page.
async fn rig(hook: Option<(Arc<Hook>, Duration)>) -> Rig {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/browse"))
        .and(header("cookie", "SAPISID=old"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GUEST, "application/json"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/browse"))
        .and(header("cookie", "SAPISID=new"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HOME, "application/json"))
        .mount(&server)
        .await;
    let store = Arc::new(MemoryStore::new());
    let session = Arc::new(Mutex::new(session("old")));
    let mut api = Innertube::new(
        session.clone(),
        store.clone(),
        Url::parse(&server.uri()).unwrap(),
    );
    if let Some((hook, every)) = hook {
        api = api.with_renew(hook, every);
    }
    Rig {
        server,
        store,
        session,
        api: Arc::new(api),
    }
}

async fn home(rig: &Rig) -> Result<(), Error> {
    rig.api.browse("FEmusic_home", None).await.map(|_| ())
}

#[tokio::test]
async fn a_guest_answer_is_signed_out() {
    // Before 2026-10-07 this was a page with nothing in it, and the engine played as a guest.
    let rig = rig(None).await;
    assert_eq!(home(&rig).await, Err(Error::SignedOut));
}

#[tokio::test]
async fn a_dropped_session_is_renewed_and_the_request_sent_again() {
    let hook = Hook::new(true, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    assert_eq!(home(&rig).await, Ok(()));
    assert_eq!(hook.calls(), 1);
    // The hook was given the dropped session, so it can match its account.
    assert_eq!(hook.seen.lock().unwrap()[0], session("old"));
    // The new session is the one in use from now on (for every request sharing it)...
    assert_eq!(*rig.session.lock().unwrap(), session("new"));
    let reqs = rig.server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    // ...and saved, in the background.
    for _ in 0..100 {
        if rig.store.load().await.ok() == Some(session("new")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rig.store.load().await, Ok(session("new")));
    // The next request goes straight out with it.
    assert_eq!(home(&rig).await, Ok(()));
    assert_eq!(hook.calls(), 1);
}

#[tokio::test]
async fn a_failed_renewal_is_not_tried_again_until_the_interval_passed() {
    let hook = Hook::new(false, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_millis(300)))).await;
    assert_eq!(home(&rig).await, Err(Error::SignedOut));
    assert_eq!(hook.calls(), 1);
    // Within the interval: signed out at once, the browser is not read again.
    assert_eq!(home(&rig).await, Err(Error::SignedOut));
    assert_eq!(hook.calls(), 1);
    assert_eq!(*rig.session.lock().unwrap(), session("old"));
    // After it: one more try.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(home(&rig).await, Err(Error::SignedOut));
    assert_eq!(hook.calls(), 2);
}

#[tokio::test]
async fn requests_signed_out_together_renew_once() {
    // A slow renewal: the second request comes back signed out while the first one's try is
    // still running, waits for it, and is sent again with its session.
    let hook = Hook::new(true, Duration::from_millis(200));
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    let (a, b) = tokio::join!(home(&rig), home(&rig));
    assert_eq!((a, b), (Ok(()), Ok(())));
    assert_eq!(hook.calls(), 1);
}

/// The once-a-minute look at an unchanged browser file is answered from the hook's cache on the
/// runtime's own thread: the blocking `stamp` (a thread of the blocking pool, made and ended
/// each minute) is only called when the file changed. Same results either way.
#[tokio::test]
async fn an_unchanged_browser_file_is_answered_without_a_blocking_call() {
    let hook = Hook::new(true, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    *hook.stamp.lock().unwrap() = Some(100);
    // Changed (nothing cached yet): the blocking call reads it, and the copy is taken.
    assert!(rig.api.follow_browser().await);
    assert_eq!(hook.stamp_calls.load(Ordering::SeqCst), 1);
    // Unchanged from now on: a minute's look is answered without `stamp`, 60 times over.
    hook.cached.store(true, Ordering::SeqCst);
    for _ in 0..60 {
        assert!(!rig.api.follow_browser().await);
    }
    assert_eq!(hook.stamp_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hook.calls(), 1);
    // The cached answer still moves things when it is newer than the copy.
    *hook.stamp.lock().unwrap() = Some(200);
    assert!(rig.api.follow_browser().await);
    assert_eq!(hook.stamp_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hook.calls(), 2);
}

#[tokio::test]
async fn following_takes_the_browsers_new_sign_in_once_per_change() {
    let hook = Hook::new(true, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    // No stamp (no browser to follow): nothing is read or taken.
    assert!(!rig.api.follow_browser().await);
    assert_eq!(hook.calls(), 0);
    *hook.stamp.lock().unwrap() = Some(100);
    assert!(rig.api.follow_browser().await);
    assert_eq!(hook.calls(), 1);
    let now = rig.session.lock().unwrap().clone();
    assert_eq!(now.cookies, session("new").cookies);
    assert_eq!(now.browser_stamp, Some(100));
    // The same stamp again: the copy is already the newest.
    assert!(!rig.api.follow_browser().await);
    assert_eq!(hook.calls(), 1);
    // Brave rotated again: one more copy.
    *hook.stamp.lock().unwrap() = Some(200);
    assert!(rig.api.follow_browser().await);
    assert_eq!(hook.calls(), 2);
    // The request after it goes out with the new session, never signed out first.
    assert_eq!(home(&rig).await, Ok(()));
    assert_eq!(hook.calls(), 2);
}

#[tokio::test]
async fn a_failed_follow_is_tried_again_only_after_the_next_change() {
    let hook = Hook::new(false, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    *hook.stamp.lock().unwrap() = Some(100);
    assert!(!rig.api.follow_browser().await);
    assert!(!rig.api.follow_browser().await);
    assert_eq!(hook.calls(), 1);
    assert_eq!(*rig.session.lock().unwrap(), session("old"));
    *hook.stamp.lock().unwrap() = Some(200);
    assert!(!rig.api.follow_browser().await);
    assert_eq!(hook.calls(), 2);
}

#[tokio::test]
async fn a_failed_follow_does_not_hold_back_a_sign_out_renewal() {
    let hook = Hook::new(false, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    *hook.stamp.lock().unwrap() = Some(100);
    assert!(!rig.api.follow_browser().await);
    // A sign-out right after still gets its own try (it fails here too, as the hook does).
    assert_eq!(home(&rig).await, Err(Error::SignedOut));
    assert_eq!(hook.calls(), 2);
}

/// The old session's `player` answer is the bot check, the new one's plays.
async fn mount_player(rig: &Rig) {
    mount_player_with(rig, BOT_CHECK, PREMIUM).await;
}

async fn mount_player_with(rig: &Rig, old: &'static str, new: &'static str) {
    for (cookie, body) in [("SAPISID=old", old), ("SAPISID=new", new)] {
        Mock::given(method("POST"))
            .and(path("/youtubei/v1/player"))
            .and(header("cookie", cookie))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .mount(&rig.server)
            .await;
    }
}

#[tokio::test]
async fn a_bot_checked_session_is_renewed_and_the_player_asked_again() {
    // 2026-11-27: the check stayed on the engine's copy of the sign-in for hours while the
    // browser played; a fresh copy played at once.
    let hook = Hook::new(true, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    mount_player(&rig).await;
    assert!(rig.api.player("FAKEVID0001", 20000).await.is_ok());
    assert_eq!(hook.calls(), 1);
    assert_eq!(*rig.session.lock().unwrap(), session("new"));
}

#[tokio::test]
async fn a_bot_check_renews_at_most_once_per_interval() {
    // The browser can't help (signed out there too): the song fails as before, and the next
    // bot check within the interval doesn't read the browser again.
    let hook = Hook::new(false, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    mount_player(&rig).await;
    for _ in 0..2 {
        let err = rig.api.player("FAKEVID0001", 20000).await.unwrap_err();
        assert_eq!(err.code(), "stream_failed", "{err}");
    }
    assert_eq!(hook.calls(), 1);
}

#[tokio::test]
async fn a_private_video_does_not_renew() {
    // Only the bot check is about the session copy: a private video's LOGIN_REQUIRED fails the
    // song without reading the browser.
    const PRIVATE: &str =
        r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"This video is private"}}"#;
    let hook = Hook::new(true, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    mount_player_with(&rig, PRIVATE, PREMIUM).await;
    let err = rig.api.player("FAKEVID0001", 20000).await.unwrap_err();
    assert_eq!(err.code(), "stream_failed", "{err}");
    assert_eq!(hook.calls(), 0);
}

#[tokio::test]
async fn a_renewed_session_still_bot_checked_fails_after_one_more_try() {
    let hook = Hook::new(true, Duration::ZERO);
    let rig = rig(Some((hook.clone(), Duration::from_secs(600)))).await;
    mount_player_with(&rig, BOT_CHECK, BOT_CHECK).await;
    let err = rig.api.player("FAKEVID0001", 20000).await.unwrap_err();
    assert_eq!(err.code(), "stream_failed", "{err}");
    assert_eq!(hook.calls(), 1);
    let sent = rig.server.received_requests().await.unwrap();
    assert_eq!(
        sent.len(),
        2,
        "the first request and one more with the new session"
    );
}
