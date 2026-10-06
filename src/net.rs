//! The host allowlist and the one HTTP client.
//!
//! Every request ytmfast makes goes to a YouTube or Google host over https. `allowed_host`
//! is the single place that rule lives: callers check the first URL with it, and the
//! client's redirect policy checks every hop, so a redirect can't walk a request (and the
//! session cookie on it) off to some other host.

use std::time::Duration;
use url::{Host, Url};

use crate::error::Error;

/// Domain suffixes we talk to. A host matches when it IS one of these or ends with
/// `.` + one of these. The leading dot matters: it is what stops `evilyoutube.com`.
const ALLOWED_SUFFIXES: &[&str] = &[
    "youtube.com",
    "googlevideo.com",
    "google.com",
    "ytimg.com",
    "ggpht.com",
    "googleusercontent.com",
];

/// API timeout from the spec. Applies to the whole request, so a stalled server can't hang
/// a command.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Size cap from the spec for one API answer: a server (or something in between) that sends
/// an endless or huge body can't make the engine buffer it.
pub const MAX_ANSWER: usize = 32 << 20;

/// Same hop limit reqwest uses by default; a custom policy replaces the default one, so the
/// limit has to be restated here.
const MAX_REDIRECTS: usize = 10;

/// True when `url` is https and its host is on the allowlist.
pub fn allowed_host(url: &Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    // Only a domain name can match: an IP literal is never one of ours. The url crate
    // lowercases domains of special schemes, so the comparison below is case-insensitive.
    let Some(Host::Domain(host)) = url.host() else {
        return false;
    };
    ALLOWED_SUFFIXES.iter().any(|suffix| {
        host == *suffix
            || host
                .strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('.'))
    })
}

/// A link found in data (a thumbnail) that another program will load: its parsed form when that is
/// https on an allowed host, else `None`.
///
/// Always the PARSED form, never the raw string, so the link sent is the link checked. The url crate
/// (WHATWG) and other parsers read odd links differently: `https://i.ytimg.com\@evil.example/a.jpg`
/// has the host `i.ytimg.com` here, but `evil.example` in Qt's QUrl (the bar widget's loader), and
/// WHATWG drops tabs and newlines that the raw string would keep. The serialized form has `\` turned
/// into `/` and those characters gone, so every reader sees the same host.
pub fn allowed_link(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    allowed_host(&url).then(|| url.into())
}

/// Decides one redirect hop. Split out of the reqwest closure so it can be unit tested:
/// reqwest's `Attempt` can't be built outside reqwest.
fn redirect_allowed(next: &Url, hops_so_far: usize) -> bool {
    hops_so_far < MAX_REDIRECTS && allowed_host(next)
}

/// The one HTTP client: rustls, a 10 s timeout and a redirect policy that re-checks the
/// allowlist on every hop.
///
/// Panics only if the TLS backend can't start, which is a broken build, not a runtime
/// condition worth an error path in every caller.
pub fn client(user_agent: &str) -> reqwest::Client {
    builder(user_agent)
        .timeout(TIMEOUT)
        .build()
        .expect("the rustls HTTP client should always build")
}

/// The client for track downloads: the same allowlist and redirect policy, but no total
/// deadline. A 10 MiB burst on a slow link can take longer than 10 s and still be healthy;
/// what must fail fast is a stall, so the 10 s applies to connecting and to each read.
pub fn stream_client(user_agent: &str) -> reqwest::Client {
    builder(user_agent)
        .connect_timeout(TIMEOUT)
        .read_timeout(TIMEOUT)
        .build()
        .expect("the rustls HTTP client should always build")
}

fn builder(user_agent: &str) -> reqwest::ClientBuilder {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        if redirect_allowed(attempt.url(), attempt.previous().len()) {
            attempt.follow()
        } else {
            // Static text only: the refused URL may be a signed stream link, and it must
            // not end up in an error message or log.
            attempt.error("redirect refused: not an allowed https host, or too many hops")
        }
    });
    reqwest::Client::builder()
        .user_agent(user_agent)
        .redirect(policy)
}

/// Reads `resp`'s body, refusing it once it passes `cap` bytes. The cap is checked on every
/// chunk as it arrives, so an oversize body is dropped after at most `cap` bytes plus one
/// chunk, never buffered whole first. A `Content-Length` over the cap is refused before
/// reading at all.
pub async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, Error> {
    let too_large = || Error::Network(format!("answer too large (over {} MiB)", cap >> 20));
    let declared = resp.content_length();
    if declared.is_some_and(|n| n > cap as u64) {
        return Err(too_large());
    }
    // `declared` is at most `cap` here; reserving it saves the regrowth copies.
    let mut body = Vec::with_capacity(declared.unwrap_or(0) as usize);
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > cap {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> bool {
        allowed_host(&Url::parse(s).unwrap())
    }

    #[test]
    fn allowed_link_sends_what_it_checked() {
        // A backslash is a path separator to WHATWG but not to QUrl, which would read the host as
        // evil.example; the parsed form has a plain slash, so both read i.ytimg.com.
        assert_eq!(
            allowed_link("https://i.ytimg.com\\@evil.example/a.jpg").as_deref(),
            Some("https://i.ytimg.com/@evil.example/a.jpg")
        );
        // Tabs and newlines are dropped before the check; the link sent has none either.
        assert_eq!(
            allowed_link("https://i.ytimg.com/vi/a\n/b\tc.jpg").as_deref(),
            Some("https://i.ytimg.com/vi/a/bc.jpg")
        );
        assert_eq!(
            allowed_link("https://lh3.googleusercontent.com/x=w226").as_deref(),
            Some("https://lh3.googleusercontent.com/x=w226")
        );
        for bad in [
            "http://i.ytimg.com/a.jpg",
            "https://evil.example\\@i.ytimg.com/a.jpg",
            "https://i.ytimg.com.evil.example/a.jpg",
            "not a url",
        ] {
            assert_eq!(allowed_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn host_allowlist() {
        assert!(ok("https://rr3---sn-x.googlevideo.com/v"));
        assert!(ok("https://music.youtube.com/"));
        assert!(!ok("http://music.youtube.com/"));
        assert!(!ok("https://youtube.com.evil.example/"));
        assert!(!ok("https://evilyoutube.com/"));
    }

    #[test]
    fn host_allowlist_rest() {
        for good in [
            "https://youtube.com/",
            "https://www.google.com/",
            "https://i.ytimg.com/vi/x/hq.jpg",
            "https://yt3.ggpht.com/a",
            "https://lh3.googleusercontent.com/a",
            "https://MUSIC.YouTube.COM/",
        ] {
            assert!(ok(good), "{good} should be allowed");
        }
        for bad in [
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://notgoogle.com/",
            "https://google.com.evil.example/",
            "ftp://music.youtube.com/",
            "wss://music.youtube.com/",
            "file:///etc/passwd",
            "data:text/plain,hi",
        ] {
            assert!(!ok(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn redirects_recheck_allowlist() {
        let good = Url::parse("https://music.youtube.com/next").unwrap();
        let bad = Url::parse("https://evil.example/").unwrap();
        let plain = Url::parse("http://music.youtube.com/").unwrap();
        assert!(redirect_allowed(&good, 0));
        assert!(!redirect_allowed(&bad, 0));
        assert!(!redirect_allowed(&plain, 0));
        assert!(redirect_allowed(&good, MAX_REDIRECTS - 1));
        assert!(!redirect_allowed(&good, MAX_REDIRECTS));
    }

    #[test]
    fn client_builds() {
        // Building must not panic outside a tokio runtime (the CLI builds it before
        // starting one in later tasks).
        let _ = client("ytmfast-test/0");
        let _ = stream_client("ytmfast-test/0");
    }
}
