//! The InnerTube API client: signed-in POSTs to `/youtubei/v1/*`.
//!
//! Every request carries the session's cookies and its SAPISIDHASH `Authorization`, and every
//! answer's `Set-Cookie` rotations are written back to the session and saved, so the session
//! stays valid on its own. Answers are capped at `net::MAX_ANSWER` while they are read.

pub mod clients;
mod player;

pub use player::{AudioFormat, PlayerResponse, Tracking};

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use url::{Host, Url};

use crate::auth::{Session, SessionStore, sidhash};
use crate::error::Error;
use crate::net;
use clients::ClientInfo;

/// Where the `player` request goes in production. Tests pass a local base to `Innertube::new`
/// instead; that constructor argument is the only way past the https allowlist (ruling R7).
pub const API_BASE: &str = "https://www.youtube.com";

/// The domains whose `Set-Cookie` answers may change the session (ruling R10). Stream hosts
/// (`googlevideo.com`) and image hosts are on the request allowlist but never set the
/// session's cookies, so anything they send is ignored.
const COOKIE_DOMAINS: &[&str] = &["youtube.com", "google.com"];

pub struct Innertube {
    http: reqwest::Client,
    session: Arc<Mutex<Session>>,
    store: Arc<dyn SessionStore>,
    base: Url,
    /// Held across "copy the session, save it" by each background save, so two answers that
    /// both rotate a cookie can't save out of order and leave the older copy in the store.
    save_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Innertube {
    /// `base` is `API_BASE` in production. It is not checked against the allowlist here, so
    /// tests can point it at a local http server; production callers pass the constant.
    pub fn new(session: Arc<Mutex<Session>>, store: Arc<dyn SessionStore>, base: Url) -> Self {
        Innertube {
            // The per-request `User-Agent` header overrides this default, so one client
            // serves every entry in the client table.
            http: net::client(clients::TV.user_agent),
            session,
            store,
            base,
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// `{base}/youtubei/v1/{endpoint}?prettyPrint=false`.
    fn endpoint_url(&self, endpoint: &str) -> Url {
        let mut url = self.base.clone();
        url.set_path(&format!("/youtubei/v1/{endpoint}"));
        // prettyPrint=false: the answer comes without indentation, which is a good part of
        // its size.
        url.set_query(Some("prettyPrint=false"));
        url
    }

    /// POSTs `body` as `client` to `endpoint` and returns the answer's bytes (at most
    /// `net::MAX_ANSWER`). A session with no SAPISID cookie, or a 401, is `SignedOut`.
    async fn post(
        &self,
        client: &ClientInfo,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<Vec<u8>, Error> {
        let url = self.endpoint_url(endpoint);
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
        if !cookie_source_allowed(from, &self.base) {
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
/// google.com or a subdomain (ruling R10), or the injected test base (ruling R7; in
/// production `base` is `API_BASE`, which the first rule already covers).
fn cookie_source_allowed(url: &Url, base: &Url) -> bool {
    let google = url.scheme() == "https"
        && match url.host() {
            Some(Host::Domain(host)) => COOKIE_DOMAINS.iter().any(|d| {
                host == *d || host.strip_suffix(d).is_some_and(|rest| rest.ends_with('.'))
            }),
            _ => false,
        };
    google || url.origin() == base.origin()
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
        assert!(net::allowed_host(&url(API_BASE)));
        for c in clients::ALL {
            assert!(net::allowed_host(&url(c.origin)), "{} origin", c.name);
            let api = url(&format!("https://{}/youtubei/v1/player", c.api_host));
            assert!(net::allowed_host(&api), "{} api host", c.name);
        }
    }

    #[test]
    fn set_cookie_only_from_youtube_or_google() {
        let base = url(API_BASE);
        for ok in [
            "https://www.youtube.com/youtubei/v1/player",
            "https://music.youtube.com/",
            "https://youtube.com/",
            "https://accounts.google.com/",
        ] {
            assert!(cookie_source_allowed(&url(ok), &base), "{ok}");
        }
        for bad in [
            // On the request allowlist, but not a cookie source (ruling R10).
            "https://rr1---sn-test.googlevideo.com/videoplayback",
            "https://i.ytimg.com/vi/x/hq.jpg",
            "https://evilyoutube.com/",
            "https://youtube.com.evil.example/",
            "http://www.youtube.com/",
        ] {
            assert!(!cookie_source_allowed(&url(bad), &base), "{bad}");
        }
        // The injected test base is the one exception.
        let test_base = url("http://127.0.0.1:4000");
        assert!(cookie_source_allowed(
            &url("http://127.0.0.1:4000/youtubei/v1/player"),
            &test_base
        ));
        assert!(!cookie_source_allowed(
            &url("http://127.0.0.1:4001/"),
            &test_base
        ));
    }

    #[test]
    fn endpoint_url_shape() {
        let store: Arc<dyn SessionStore> = Arc::new(crate::auth::MemoryStore::new());
        let api = Innertube::new(Arc::default(), store, url(API_BASE));
        assert_eq!(
            api.endpoint_url("player").as_str(),
            "https://www.youtube.com/youtubei/v1/player?prettyPrint=false"
        );
    }
}
