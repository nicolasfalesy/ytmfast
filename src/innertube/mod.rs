//! The InnerTube API client: signed-in POSTs to `/youtubei/v1/*`.
//!
//! Every request carries the session's cookies and its SAPISIDHASH `Authorization`, and every
//! answer's `Set-Cookie` rotations are written back to the session and saved, so the session
//! stays valid on its own. Answers are capped at `net::MAX_ANSWER` while they are read.

pub mod clients;
mod next;
mod player;

pub use next::{NextPage, NextRequest, SongItem, clean_artist};
pub use player::{AudioFormat, PlayerResponse, Tracking};

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

pub struct Innertube {
    http: reqwest::Client,
    session: Arc<Mutex<Session>>,
    store: Arc<dyn SessionStore>,
    target: Target,
    /// Held across "copy the session, save it" by each background save, so two answers that
    /// both rotate a cookie can't save out of order and leave the older copy in the store.
    save_lock: Arc<tokio::sync::Mutex<()>>,
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
        }
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
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::SignedOut);
        }
        if !status.is_success() {
            return Err(Error::Network(format!(
                "YouTube answered HTTP {}",
                status.as_u16()
            )));
        }
        net::read_capped(resp, net::MAX_ANSWER).await
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
