//! The InnerTube API client: signed-in POSTs to `/youtubei/v1/*`.
//!
//! Every request carries the session's cookies and its SAPISIDHASH `Authorization`, and every
//! answer's `Set-Cookie` rotations are written back to the session and saved, so the session
//! stays valid on its own. Answers are capped at `net::MAX_ANSWER` while they are read.

mod browse;
pub mod clients;
mod next;
mod player;

pub use browse::{Account, MAX_QUERY, MoreKind, check_params, check_query};
pub use next::{NextPage, NextRequest, SongItem, SongNext, clean_artist};
pub use player::{AudioFormat, PlayerResponse, Tracking};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use url::{Host, Url};

use crate::auth::{Session, SessionStore, sidhash};
use crate::error::Error;
use crate::net;
use clients::ClientInfo;

/// The domains whose `Set-Cookie` answers may change the session (ruling R10). Stream hosts
/// (`googlevideo.com`) and image hosts are on the request allowlist but never set the
/// session's cookies, so anything they send is ignored.
const COOKIE_DOMAINS: &[&str] = &["youtube.com", "google.com"];

/// The `Referer` of the music web app's own requests: its page.
const MUSIC_REFERER: &str = "https://music.youtube.com/";

/// Where requests go.
enum Target {
    /// Production: each client to `https://{api_host}` from its own `ClientInfo`, so `player`
    /// (TV) goes to www.youtube.com and `next` (WEB_REMIX) to music.youtube.com.
    ApiHost,
    /// Tests: every client to one injected base, the only way past the https allowlist
    /// (ruling R7).
    Fixed(Url),
}

/// The shortest time between two tries at renewing a signed-out session (his pick, 2026-10-07:
/// once per 10 minutes). One try reads the browser's cookies and asks YouTube who they sign
/// in; when that fails, every request until the next try is signed out at once, and the bar
/// shows its Sign in again screen, whose button imports by hand at any time.
pub const RENEW_EVERY: Duration = Duration::from_secs(10 * 60);

/// How long one renewal may take: the browser's cookie key comes from the keyring, which can
/// hold a lookup behind an unlock prompt with no timeout of its own.
const RENEW_TIMEOUT: Duration = Duration::from_secs(30);

/// A way to get a new session when YouTube no longer accepts the one the engine has.
///
/// YouTube can drop a copied browser session at any time, and it does it silently: the
/// answers stay 200, as a guest's (2026-10-07: home page generic, library and history empty,
/// the music looked like "another Google account"). `Innertube` notices that (`SignedOut`)
/// and asks this once per `RENEW_EVERY`.
#[async_trait]
pub trait Renew: Send + Sync {
    /// A session for the same account as `current` (`Session::account`), checked to sign it
    /// in; or why there is none, as fixed text for the log (never a cookie or a name).
    async fn renew(&self, current: &Session) -> Result<Session, String>;
}

struct Renewal {
    hook: Arc<dyn Renew>,
    every: Duration,
    /// Held across one try, so requests signed out at the same moment make one try between
    /// them, not one each.
    last_try: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    /// How many tries succeeded. A request reads it before it is sent: when it changed by the
    /// time the request came back signed out, another request already renewed the session,
    /// and this one is sent again with it instead of trying too.
    renewed: AtomicU64,
}

pub struct Innertube {
    http: reqwest::Client,
    session: Arc<Mutex<Session>>,
    store: Arc<dyn SessionStore>,
    target: Target,
    /// Held across "copy the session, save it" by each background save, so two answers that
    /// both rotate a cookie can't save out of order and leave the older copy in the store.
    save_lock: Arc<tokio::sync::Mutex<()>>,
    /// Renews a session YouTube signed out; `None` never renews (the import's own account
    /// check, and tests that don't ask for it).
    renewal: Option<Renewal>,
}

impl Innertube {
    /// The production client: every request goes to its client's `api_host` over https.
    pub fn production(session: Arc<Mutex<Session>>, store: Arc<dyn SessionStore>) -> Self {
        Self::with_target(session, store, Target::ApiHost)
    }

    /// A client that sends every request, whatever its client, to `base`. For tests only: `base`
    /// is not checked against the allowlist, so it can be a local http server (ruling R7).
    /// Production code uses `production`.
    pub fn new(session: Arc<Mutex<Session>>, store: Arc<dyn SessionStore>, base: Url) -> Self {
        Self::with_target(session, store, Target::Fixed(base))
    }

    fn with_target(
        session: Arc<Mutex<Session>>,
        store: Arc<dyn SessionStore>,
        target: Target,
    ) -> Self {
        Innertube {
            // The per-request `User-Agent` header overrides this default, so one client
            // serves every entry in the client table.
            http: net::client(clients::TV.user_agent),
            session,
            store,
            target,
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
            renewal: None,
        }
    }

    /// Renews a session YouTube signed out with `hook`, at most once per `every`
    /// (`RENEW_EVERY` in production; tests pass their own).
    pub fn with_renew(mut self, hook: Arc<dyn Renew>, every: Duration) -> Self {
        self.renewal = Some(Renewal {
            hook,
            every,
            last_try: tokio::sync::Mutex::new(None),
            renewed: AtomicU64::new(0),
        });
        self
    }

    /// Where `client`'s `endpoint` request goes:
    /// `https://{client.api_host}/youtubei/v1/{endpoint}?prettyPrint=false` in production,
    /// the same path and query on the injected base in tests.
    pub fn endpoint_url(&self, client: &ClientInfo, endpoint: &str) -> Url {
        let mut url = match &self.target {
            Target::Fixed(base) => base.clone(),
            // `api_host` is a constant from the client table, and a unit test parses every
            // entry, so this can only fail on a broken edit to that table.
            Target::ApiHost => Url::parse(&format!("https://{}", client.api_host))
                .expect("every client table api_host is a valid host"),
        };
        url.set_path(&format!("/youtubei/v1/{endpoint}"));
        // prettyPrint=false: the answer comes without indentation, which is a good part of
        // its size.
        url.set_query(Some("prettyPrint=false"));
        url
    }

    /// Whether a link that came in an answer may be requested: on the allowlist in
    /// production; in tests, only on the injected test server, so a test can never reach a
    /// real YouTube host (ruling R7).
    pub(crate) fn target_allows(&self, url: &Url) -> bool {
        match &self.target {
            Target::ApiHost => net::allowed_host(url),
            Target::Fixed(base) => url.origin() == base.origin(),
        }
    }

    /// One play-history ping: a GET of `url` (a `playbackTracking` link with the report's
    /// parameters added) as the music web client, with the session.
    ///
    /// The headers are the ones that made a play reach the YouTube Music history in the
    /// step-2 spike (ledger, variant 2): the music web user agent, `Origin` and `X-Origin`
    /// music.youtube.com, `Referer` music.youtube.com/, the SAPISIDHASH for that origin,
    /// `X-Goog-AuthUser: 0`, `X-Goog-Visitor-Id` from the same song's answer, and the
    /// cookies. The answer's body (empty, a 204) is not read; its `Set-Cookie`s are kept.
    ///
    /// Errors (fixed text, never the link, ruling R6): `Internal` for a link off the
    /// allowlist (nothing is sent), `SignedOut` for no session or a 401, `Network` otherwise.
    pub async fn ping(&self, url: &Url, visitor_data: Option<&str>) -> Result<(), Error> {
        if !self.target_allows(url) {
            return Err(Error::Internal(
                "a play-history link is not on an allowed host".into(),
            ));
        }
        let client = &clients::WEB_REMIX;
        let (cookie, auth) = {
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            (
                session.cookie_header(url),
                sidhash::authorization(&session, client.origin, now_unix()),
            )
        };
        let Some(auth) = auth else {
            return Err(Error::SignedOut);
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(client.user_agent),
        );
        headers.insert(header::ORIGIN, HeaderValue::from_static(client.origin));
        headers.insert(
            HeaderName::from_static("x-origin"),
            HeaderValue::from_static(client.origin),
        );
        headers.insert(header::REFERER, HeaderValue::from_static(MUSIC_REFERER));
        headers.insert(
            HeaderName::from_static("x-goog-authuser"),
            HeaderValue::from_static("0"),
        );
        if let Some(v) = visitor_data {
            headers.insert(
                HeaderName::from_static("x-goog-visitor-id"),
                secret_header(v)?,
            );
        }
        headers.insert(header::AUTHORIZATION, secret_header(&auth)?);
        if !cookie.is_empty() {
            headers.insert(header::COOKIE, secret_header(&cookie)?);
        }
        let resp = self.http.get(url.clone()).headers(headers).send().await?;
        self.absorb_set_cookies(&resp);
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::SignedOut);
        }
        if !status.is_success() {
            return Err(Error::Network(format!(
                "YouTube answered HTTP {}",
                status.as_u16()
            )));
        }
        Ok(())
    }

    /// POSTs `body` as `client` to `endpoint` and returns the answer's bytes (at most
    /// `net::MAX_ANSWER`). A session with no SAPISID cookie, or a 401, is `SignedOut`.
    async fn post(
        &self,
        client: &ClientInfo,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<Vec<u8>, Error> {
        self.post_with(client, endpoint, body, false).await
    }

    /// `post`, where `forbidden_is_signed_out` makes a 403 `SignedOut` too. For account
    /// actions (a like): YouTube refuses those with a 403 when the session no longer counts
    /// as signed in, and the fix is the same as for a 401, signing in again. A 403 on a read
    /// stays a network error, as before.
    ///
    /// A request signed out (the session was refused, or answered as a guest's) renews the
    /// session when it may (`Renew`) and is sent once more with the new one.
    async fn post_with(
        &self,
        client: &ClientInfo,
        endpoint: &str,
        body: &serde_json::Value,
        forbidden_is_signed_out: bool,
    ) -> Result<Vec<u8>, Error> {
        let seen = self.renewals();
        match self
            .post_once(client, endpoint, body, forbidden_is_signed_out)
            .await
        {
            Err(Error::SignedOut) if self.renew(seen).await => {
                self.post_once(client, endpoint, body, forbidden_is_signed_out)
                    .await
            }
            other => other,
        }
    }

    fn renewals(&self) -> u64 {
        self.renewal
            .as_ref()
            .map_or(0, |r| r.renewed.load(Ordering::Acquire))
    }

    /// Renews the session after a request that was sent when `seen` renewals had been made
    /// came back signed out. True when the request should be sent again: this try renewed
    /// the session, or another one did while the request was out.
    async fn renew(&self, seen: u64) -> bool {
        let Some(r) = &self.renewal else {
            return false;
        };
        let mut last_try = r.last_try.lock().await;
        if r.renewed.load(Ordering::Acquire) != seen {
            return true;
        }
        if last_try.is_some_and(|t| t.elapsed() < r.every) {
            return false;
        }
        *last_try = Some(tokio::time::Instant::now());
        let current = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match tokio::time::timeout(RENEW_TIMEOUT, r.hook.renew(&current)).await {
            Ok(Ok(session)) => {
                *self.session.lock().unwrap_or_else(|e| e.into_inner()) = session;
                r.renewed.fetch_add(1, Ordering::Release);
                // In the background, as a cookie rotation is: the keyring keeps the new
                // session for the next start, and this request doesn't wait on the save.
                self.save_in_background();
                eprintln!(
                    "ytmfast: YouTube signed the session out; signed in again from the browser"
                );
                true
            }
            Ok(Err(why)) => {
                eprintln!(
                    "ytmfast: YouTube signed the session out; could not sign in again: {why}"
                );
                false
            }
            Err(_) => {
                eprintln!(
                    "ytmfast: YouTube signed the session out; signing in again took too long"
                );
                false
            }
        }
    }

    /// One `post_with` request, without renewing.
    async fn post_once(
        &self,
        client: &ClientInfo,
        endpoint: &str,
        body: &serde_json::Value,
        forbidden_is_signed_out: bool,
    ) -> Result<Vec<u8>, Error> {
        let url = self.endpoint_url(client, endpoint);
        // Ruling R7: production checks every URL it builds. The test base is the one bypass.
        if matches!(self.target, Target::ApiHost) && !net::allowed_host(&url) {
            return Err(Error::Internal(
                "the API host is not an allowed host".into(),
            ));
        }
        let (cookie, auth) = {
            // A poisoned lock still holds a whole session (every write is one assignment
            // under the lock), so it is safe to use.
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            (
                session.cookie_header(&url),
                sidhash::authorization(&session, client.origin, now_unix()),
            )
        };
        // Without a SAPISID the request would go out anonymous and come back without the
        // account's (Premium) formats, or as LOGIN_REQUIRED; say so up front instead.
        let Some(auth) = auth else {
            return Err(Error::SignedOut);
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(client.user_agent),
        );
        headers.insert(header::ORIGIN, HeaderValue::from_static(client.origin));
        headers.insert(
            HeaderName::from_static("x-origin"),
            HeaderValue::from_static(client.origin),
        );
        headers.insert(
            HeaderName::from_static("x-youtube-client-name"),
            HeaderValue::from(client.name_id),
        );
        headers.insert(
            HeaderName::from_static("x-youtube-client-version"),
            HeaderValue::from_static(client.version),
        );
        if client.sends_auth_user {
            // The account index among the browser's signed-in Google accounts; the session
            // holds one account's cookies, so it is always the first.
            headers.insert(
                HeaderName::from_static("x-goog-authuser"),
                HeaderValue::from_static("0"),
            );
        }
        headers.insert(header::AUTHORIZATION, secret_header(&auth)?);
        if !cookie.is_empty() {
            headers.insert(header::COOKIE, secret_header(&cookie)?);
        }

        let body = serde_json::to_vec(body)
            .map_err(|_| Error::Internal("could not encode the request".into()))?;
        let resp = self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        // Before the status check: a 401 can carry cookie deletions worth keeping.
        self.absorb_set_cookies(&resp);

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || (forbidden_is_signed_out && status == reqwest::StatusCode::FORBIDDEN)
        {
            return Err(Error::SignedOut);
        }
        if !status.is_success() {
            return Err(Error::Network(format!(
                "YouTube answered HTTP {}",
                status.as_u16()
            )));
        }
        let bytes = net::read_capped(resp, net::MAX_ANSWER).await?;
        // A session YouTube no longer accepts gets a 200 and a guest's answer, not a 401: the
        // music client's answers say which in their tracking data, so a guest's answer is
        // `SignedOut` here (and renewed) instead of a library that is silently empty.
        if client.sends_auth_user && answered_as_guest(&bytes) {
            return Err(Error::SignedOut);
        }
        Ok(bytes)
    }

    /// Applies the answer's `Set-Cookie` headers to the in-memory session at once (when the
    /// answer came from a cookie domain, ruling R10), and starts a background save when one
    /// of them changed it.
    fn absorb_set_cookies(&self, resp: &reqwest::Response) {
        let from = resp.url();
        let base = match &self.target {
            Target::Fixed(base) => Some(base),
            Target::ApiHost => None,
        };
        if !cookie_source_allowed(from, base) {
            return;
        }
        let changed = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let mut changed = false;
            for value in resp.headers().get_all(header::SET_COOKIE) {
                if let Ok(value) = value.to_str() {
                    // `|` not `||`: every header is applied, not just up to the first change.
                    changed |= session.apply_set_cookie(from, value);
                }
            }
            changed
        };
        if changed {
            self.save_in_background();
        }
    }

    /// Saves the session from a spawned task. Never on the request path: the request's 10 s
    /// deadline covers reading the body, and a keyring save opens a D-Bus connection and can
    /// be slow, so an awaited save could turn a good answer into a timeout. The save uses the
    /// no-prompt path: nobody is waiting to unlock a keyring for a cookie rotation, so a
    /// locked keyring skips the save (logged) and the session stays current in memory.
    fn save_in_background(&self) {
        let session = self.session.clone();
        let store = self.store.clone();
        let order = self.save_lock.clone();
        tokio::spawn(async move {
            let _order = order.lock().await;
            // Copied under the save lock, so the last save always writes the newest session.
            let snapshot = session.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Err(e) = store.save_without_prompt(&snapshot).await {
                // The error text is fixed (no secrets). The next rotation saves again.
                eprintln!("ytmfast: could not save the refreshed session: {e}");
            }
        });
    }
}

/// A header value marked sensitive, so reqwest/hyper never print it (in `Debug` output or
/// HTTP/2 header tables). The error is fixed text: the value is the session.
fn secret_header(value: &str) -> Result<HeaderValue, Error> {
    let mut v = HeaderValue::from_str(value)
        .map_err(|_| Error::Internal("a session cookie is not a valid header value".into()))?;
    v.set_sensitive(true);
    Ok(v)
}

/// True when an answer from `url` may set session cookies: https from youtube.com or
/// google.com or a subdomain (ruling R10), or the injected test base (ruling R7; production
/// has none).
fn cookie_source_allowed(url: &Url, base: Option<&Url>) -> bool {
    let google = url.scheme() == "https"
        && match url.host() {
            Some(Host::Domain(host)) => COOKIE_DOMAINS.iter().any(|d| {
                host == *d || host.strip_suffix(d).is_some_and(|rest| rest.ends_with('.'))
            }),
            _ => false,
        };
    google || base.is_some_and(|b| url.origin() == b.origin())
}

/// Whether a music client's answer was made for a guest: its `responseContext` tracking says
/// `{"key":"logged_in","value":"0"}` (GFEEDBACK), as every answer to a session YouTube dropped
/// did on 2026-10-07; a signed-in one says `"1"`. Matched on the bytes, as YouTube writes
/// them (`prettyPrint=false`, no spaces): the answer is parsed later by its own reader, and
/// a reorder that stops this matching only brings back the old silent guest answers.
fn answered_as_guest(answer: &[u8]) -> bool {
    const GUEST: &[u8] = br#""key":"logged_in","value":"0""#;
    answer.windows(GUEST.len()).any(|w| w == GUEST)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn production_urls_pass_the_allowlist() {
        // Ruling R7: everything production code sends to must pass `allowed_host`.
        let store: Arc<dyn SessionStore> = Arc::new(crate::auth::MemoryStore::new());
        let api = Innertube::production(Arc::default(), store);
        for c in clients::ALL {
            assert!(
                net::allowed_host(&api.endpoint_url(c, "next")),
                "{}",
                c.name
            );
            assert!(net::allowed_host(&url(c.origin)), "{} origin", c.name);
            let api = url(&format!("https://{}/youtubei/v1/player", c.api_host));
            assert!(net::allowed_host(&api), "{} api host", c.name);
        }
    }

    #[test]
    fn a_guest_answer_is_told_apart() {
        let tracking = |v: &str| {
            format!(
                r#"{{"responseContext":{{"serviceTrackingParams":[{{"service":"GFEEDBACK","params":[{{"key":"logged_in","value":"{v}"}}]}}]}}}}"#
            )
        };
        assert!(answered_as_guest(tracking("0").as_bytes()));
        assert!(!answered_as_guest(tracking("1").as_bytes()));
        assert!(!answered_as_guest(b"{}"));
        assert!(!answered_as_guest(b""));
    }

    #[test]
    fn set_cookie_only_from_youtube_or_google() {
        let base = None;
        for ok in [
            "https://www.youtube.com/youtubei/v1/player",
            "https://music.youtube.com/",
            "https://youtube.com/",
            "https://accounts.google.com/",
        ] {
            assert!(cookie_source_allowed(&url(ok), base), "{ok}");
        }
        for bad in [
            // On the request allowlist, but not a cookie source (ruling R10).
            "https://rr1---sn-test.googlevideo.com/videoplayback",
            "https://i.ytimg.com/vi/x/hq.jpg",
            "https://evilyoutube.com/",
            "https://youtube.com.evil.example/",
            "http://www.youtube.com/",
        ] {
            assert!(!cookie_source_allowed(&url(bad), base), "{bad}");
        }
        // The injected test base is the one exception.
        let test_base = url("http://127.0.0.1:4000");
        assert!(cookie_source_allowed(
            &url("http://127.0.0.1:4000/youtubei/v1/player"),
            Some(&test_base)
        ));
        assert!(!cookie_source_allowed(
            &url("http://127.0.0.1:4001/"),
            Some(&test_base)
        ));
        // Production has no test base: a local answer never sets a cookie.
        assert!(!cookie_source_allowed(&url("http://127.0.0.1:4000/"), None));
    }

    #[test]
    fn endpoint_url_shape() {
        let store: Arc<dyn SessionStore> = Arc::new(crate::auth::MemoryStore::new());
        let api = Innertube::production(Arc::default(), store);
        assert_eq!(
            api.endpoint_url(&clients::TV, "player").as_str(),
            "https://www.youtube.com/youtubei/v1/player?prettyPrint=false"
        );
    }
}
