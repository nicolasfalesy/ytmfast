//! The lyrics services' own HTTP client: three hosts, https only, no redirects, no cookies,
//! 8 s and 2 MiB per answer, the limits the widget's curl had.
//!
//! Kept apart from `net::client` on purpose: the YouTube client carries the session, and its
//! allowlist (`net::allowed_host`) stays YouTube's and Google's alone. These hosts are allowed
//! here, for these requests, and nowhere else.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::{Host, Url};

use super::js::truthy;

/// The lyrics hosts. Exact names, not suffixes: the widget only ever asked these.
const HOSTS: [&str; 3] = ["lrclib.net", "krcs.kugou.com", "lyrics.kugou.com"];

/// Per answer, as the widget's `curl --max-time 8`: a lookup that hangs (a dead connection
/// after a network change) is given up and the next source is used.
pub const TIMEOUT: Duration = Duration::from_secs(8);

/// Per answer, as the widget's `curl --max-filesize`: a huge or endless answer is dropped once
/// past it, never buffered whole. Real answers are 5 KiB (KuGou) to about 120 KiB (an LRCLIB
/// search).
pub const MAX_ANSWER: usize = 2 * 1024 * 1024;

/// LRCLIB asks clients to name themselves.
const LRCLIB_CLIENT: &str = concat!("ytmfast/", env!("CARGO_PKG_VERSION"));

/// True when `url` is https on one of the three lyrics hosts.
pub fn allowed_host(url: &Url) -> bool {
    url.scheme() == "https"
        && matches!(url.host(), Some(Host::Domain(h)) if HOSTS.contains(&h))
        // No port other than https's own, and no user info: the links are built here, so
        // anything else means something went wrong.
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// The extra header one request carries: LRCLIB's client name, for LRCLIB only.
pub fn extra_header(url: &Url) -> Option<(&'static str, &'static str)> {
    (url.host_str() == Some("lrclib.net")).then_some(("Lrclib-Client", LRCLIB_CLIENT))
}

/// One answer, as the widget sorted them.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    /// A 2xx with a JSON body that is something (not `null`, `false`, `0` or `""`).
    Json(Value),
    /// The service said the song is not there: HTTP 4xx, except 408 and 429, which only mean
    /// "later". Kept like any answer.
    NotFound,
    /// Anything else: no network, a timeout, an answer too big, a 5xx, 408, 429, a redirect,
    /// a body that is not JSON. What came of the lookup is shown but not kept.
    Failed,
}

impl Fetched {
    /// A 2xx's parsed body: a falsy one (`null`, `false`, `0`, `""`) is a failure, as the
    /// widget's `code === 0 && !d` had it.
    pub fn from_json(v: Value) -> Fetched {
        if truthy(Some(&v)) {
            Fetched::Json(v)
        } else {
            Fetched::Failed
        }
    }

    /// A non-2xx status.
    pub fn from_status(status: u16) -> Fetched {
        if (400..500).contains(&status) && status != 408 && status != 429 {
            Fetched::NotFound
        } else {
            Fetched::Failed
        }
    }
}

/// Fetches lyrics answers: `HttpWeb` in the daemon, saved answers in tests.
#[async_trait]
pub trait LyricsWeb: Send + Sync {
    /// GETs `url` (built by `lookup`, always on a lyrics host) and sorts its answer.
    async fn get_json(&self, url: &str) -> Fetched;
}

/// The real one: reqwest with rustls, no redirects (curl followed none either), no cookie
/// store, the timeout and size cap above.
pub struct HttpWeb {
    client: reqwest::Client,
    allow: fn(&Url) -> bool,
}

impl HttpWeb {
    /// Panics only if the TLS backend can't start, a broken build (as `net::client`).
    pub fn new() -> HttpWeb {
        HttpWeb::with(allowed_host, TIMEOUT)
    }

    /// With another host rule and timeout: tests point it at a local plain-http server.
    #[doc(hidden)]
    pub fn with(allow: fn(&Url) -> bool, timeout: Duration) -> HttpWeb {
        let client = reqwest::Client::builder()
            .user_agent(LRCLIB_CLIENT)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .expect("the rustls HTTP client should always build");
        HttpWeb { client, allow }
    }
}

impl Default for HttpWeb {
    fn default() -> Self {
        HttpWeb::new()
    }
}

#[async_trait]
impl LyricsWeb for HttpWeb {
    async fn get_json(&self, url: &str) -> Fetched {
        let Ok(url) = Url::parse(url) else {
            return Fetched::Failed;
        };
        if !(self.allow)(&url) {
            return Fetched::Failed;
        }
        let mut req = self.client.get(url.clone());
        if let Some((name, value)) = extra_header(&url) {
            req = req.header(name, value);
        }
        // Nothing of a failure is logged or passed on: the link holds the song's title and
        // artist, and the chain only needs to know it failed.
        let Ok(resp) = req.send().await else {
            return Fetched::Failed;
        };
        let status = resp.status();
        if !status.is_success() {
            return Fetched::from_status(status.as_u16());
        }
        match crate::net::read_capped(resp, MAX_ANSWER).await {
            Ok(body) => match serde_json::from_slice::<Value>(&body) {
                Ok(v) => Fetched::from_json(v),
                Err(_) => Fetched::Failed,
            },
            Err(_) => Fetched::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> bool {
        allowed_host(&Url::parse(s).unwrap())
    }

    #[test]
    fn only_the_three_hosts_over_https() {
        for good in [
            "https://lrclib.net/api/get?x=1",
            "https://krcs.kugou.com/search?ver=1",
            "https://lyrics.kugou.com/download?ver=1",
            "https://LRCLIB.net/api/search",
        ] {
            assert!(ok(good), "{good}");
        }
        for bad in [
            "http://lrclib.net/api/get",
            "https://www.lrclib.net/",
            "https://kugou.com/",
            "https://lrclib.net.evil.example/",
            "https://evil-lrclib.net/",
            "https://lrclib.net:8443/",
            "https://user@lrclib.net/",
            "https://music.youtube.com/",
            "https://127.0.0.1/",
        ] {
            assert!(!ok(bad), "{bad}");
        }
        // And the YouTube allowlist did not grow.
        for host in HOSTS {
            assert!(!crate::net::allowed_host(
                &Url::parse(&format!("https://{host}/")).unwrap()
            ));
        }
    }

    #[test]
    fn lrclib_alone_gets_the_client_header() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            extra_header(&u("https://lrclib.net/api/get")),
            Some(("Lrclib-Client", LRCLIB_CLIENT))
        );
        assert!(LRCLIB_CLIENT.starts_with("ytmfast/"));
        assert_eq!(extra_header(&u("https://krcs.kugou.com/search")), None);
    }

    #[test]
    fn answers_sort_as_the_widget_did() {
        assert_eq!(Fetched::from_status(404), Fetched::NotFound);
        assert_eq!(Fetched::from_status(400), Fetched::NotFound);
        for s in [408, 429, 500, 503, 301, 302, 204, 100] {
            assert_eq!(Fetched::from_status(s), Fetched::Failed, "{s}");
        }
        for v in [Value::Null, false.into(), 0.into(), "".into()] {
            assert_eq!(Fetched::from_json(v.clone()), Fetched::Failed, "{v}");
        }
        assert_eq!(
            Fetched::from_json(serde_json::json!([])),
            Fetched::Json(serde_json::json!([]))
        );
    }
}
