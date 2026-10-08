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
    }
}

/// A hook that hands out the "new" session (or fails), after `delay`, and counts its calls.
struct Hook {
    calls: AtomicUsize,
    works: bool,
    delay: Duration,
    /// The session each call was given.
    seen: Mutex<Vec<Session>>,
}

impl Hook {
    fn new(works: bool, delay: Duration) -> Arc<Hook> {
        Arc::new(Hook {
            calls: AtomicUsize::new(0),
            works,
            delay,
            seen: Mutex::new(Vec::new()),
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
