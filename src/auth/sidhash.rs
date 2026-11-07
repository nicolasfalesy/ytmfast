//! The `Authorization` header signed-in YouTube API calls carry, built from the session.
//!
//! This follows yt-dlp's `_get_sid_authorization_header`: for each of three schemes whose
//! cookie is present, `"{scheme} {ts}_{sha1_hex("{ts} {sid} {origin}")}"`, joined by spaces.

use std::borrow::Cow;

use sha1::{Digest, Sha1};
use url::Url;

use super::Session;

/// The cookies the three schemes read, in the order `authorization` looks them up.
const SID_NAMES: [&str; 3] = ["SAPISID", "__Secure-3PAPISID", "__Secure-1PAPISID"];

/// The host, path and `https` flag `Url::parse(origin)` gives, or `None` when `origin` is not a
/// URL with a host. A client's own origin (every caller's) takes its host from the client
/// table, checked against `Url::parse` by a test there, so no request parses it; any other
/// origin still goes to `Url` (measured 2026-11-07: the parse was an allocation and most of
/// the time per call).
fn origin_parts(origin: &str) -> Option<(Cow<'_, str>, Cow<'_, str>, bool)> {
    if let Some(client) = crate::innertube::clients::ALL
        .iter()
        .find(|c| c.origin == origin)
    {
        return Some((client.origin_host.into(), "/".into(), true));
    }
    let url = Url::parse(origin).ok()?;
    let host = url.host_str()?.to_owned();
    Some((
        host.into(),
        url.path().to_owned().into(),
        url.scheme() == "https",
    ))
}

/// The values of the cookies in [`SID_NAMES`] that apply to a request to `origin` at
/// `now_unix`, the first of each name, or `None` when `origin` or the time is unusable. The
/// one lookup both `authorization` and `has_sid` use, so the two cannot drift apart.
fn sids<'a>(session: &'a Session, origin: &str, now_unix: u64) -> Option<[Option<&'a str>; 3]> {
    let (host, path, https) = origin_parts(origin)?;
    let now = i64::try_from(now_unix).ok()?;
    let mut sids = [None; 3];
    for c in session.matching_host(&host, &path, https, now) {
        if let Some(i) = SID_NAMES.iter().position(|n| c.name == *n) {
            sids[i].get_or_insert(c.value.as_str());
        }
    }
    Some(sids)
}

/// True when `authorization(session, origin, now_unix)` would be `Some`: the session holds a
/// SAPISID cookie for the origin. Same lookup, no hashing.
pub fn has_sid(session: &Session, origin: &str, now_unix: u64) -> bool {
    sids(session, origin, now_unix).is_some_and(|s| s.iter().any(Option::is_some))
}

/// The `Authorization` value for a request sent with `Origin: origin` at `now_unix`, or
/// `None` when the session holds none of the SAPISID cookies (or `origin` is not a URL).
pub fn authorization(session: &Session, origin: &str, now_unix: u64) -> Option<String> {
    // The SID cookies are the ones the origin's own host would get (yt-dlp reads them for
    // `.youtube.com`), so a `.google.com` SAPISID never signs a youtube.com request.
    let [sapisid, three_p, one_p] = sids(session, origin, now_unix)?;
    let schemes = [
        ("SAPISIDHASH", sapisid.or(three_p)),
        ("SAPISID1PHASH", one_p),
        ("SAPISID3PHASH", three_p),
    ];
    // Every signed request runs this, so (measured 2026-11-07, ytmfast PR #5): the time is
    // formatted once, the hash takes its pieces directly, a cookie value seen before reuses
    // its hex (real jars often hold the same value in SAPISID and __Secure-3PAPISID), and the
    // parts go straight into one string instead of a String each plus a Vec and a join.
    let ts = now_unix.to_string();
    // Room for three parts of the longest scheme (13 bytes), a space, the time, "_", 40 hex
    // and a separating space. A guess for speed only: the string still grows if it must.
    let mut out = String::with_capacity(3 * (13 + ts.len() + 43));
    let mut seen = [("", [0u8; 40]); 3];
    let mut n = 0;
    for (scheme, sid) in schemes {
        let Some(sid) = sid else { continue };
        let hex = match seen[..n].iter().find(|(s, _)| *s == sid) {
            Some(&(_, hex)) => hex,
            None => {
                let digest = Sha1::new()
                    .chain_update(&ts)
                    .chain_update(b" ")
                    .chain_update(sid)
                    .chain_update(b" ")
                    .chain_update(origin)
                    .finalize();
                let mut hex = [0u8; 40];
                hex::encode_to_slice(digest, &mut hex)
                    .expect("a SHA-1's 20 bytes fill 40 hex digits exactly");
                seen[n] = (sid, hex);
                n += 1;
                hex
            }
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(scheme);
        out.push(' ');
        out.push_str(&ts);
        out.push('_');
        out.push_str(std::str::from_utf8(&hex).expect("hex digits are ASCII"));
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Cookie;

    fn yt(name: &str, value: &str) -> Cookie {
        Cookie {
            domain: ".youtube.com".into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            secure: true,
            expires_utc: None,
        }
    }

    fn sha1_hex(s: &str) -> String {
        hex::encode(Sha1::digest(s.as_bytes()))
    }

    const ORIGIN: &str = "https://www.youtube.com";

    #[test]
    fn sidhash_vector() {
        let session = Session {
            cookies: vec![yt("SAPISID", "abc")],
            account: None,
            browser_stamp: None,
        };
        // The hash from `sha1sum`, not from the sha1/hex crates the code itself uses.
        assert_eq!(
            authorization(&session, ORIGIN, 1_700_000_000).as_deref(),
            Some("SAPISIDHASH 1700000000_27b236f59d4ec583d7530f2c7055d2f9c6aecf92")
        );
    }

    #[test]
    fn all_three_schemes_in_order() {
        let session = Session {
            cookies: vec![
                yt("__Secure-3PAPISID", "three"),
                yt("SAPISID", "plain"),
                yt("__Secure-1PAPISID", "one"),
            ],
            account: None,
            browser_stamp: None,
        };
        let ts = 1_700_000_000;
        let part = |sid: &str| format!("{ts}_{}", sha1_hex(&format!("{ts} {sid} {ORIGIN}")));
        assert_eq!(
            authorization(&session, ORIGIN, ts).unwrap(),
            format!(
                "SAPISIDHASH {} SAPISID1PHASH {} SAPISID3PHASH {}",
                part("plain"),
                part("one"),
                part("three")
            )
        );
    }

    /// Only the 1P cookie: the first scheme is skipped, so the header must not start with a
    /// space (the parts are joined by hand now, not by `join`). Hash from `sha1sum`.
    #[test]
    fn only_1papisid_has_no_stray_space() {
        let session = Session {
            cookies: vec![yt("__Secure-1PAPISID", "one")],
            account: None,
            browser_stamp: None,
        };
        assert_eq!(
            authorization(&session, ORIGIN, 1_700_000_000).as_deref(),
            Some("SAPISID1PHASH 1700000000_01ca8712a198105a3f0999f890c24137388b7fff")
        );
    }

    #[test]
    fn sapisid_falls_back_to_3papisid() {
        let session = Session {
            cookies: vec![yt("__Secure-3PAPISID", "three")],
            account: None,
            browser_stamp: None,
        };
        let ts = 1_700_000_000;
        let part = format!("{ts}_{}", sha1_hex(&format!("{ts} three {ORIGIN}")));
        assert_eq!(
            authorization(&session, ORIGIN, ts).unwrap(),
            format!("SAPISIDHASH {part} SAPISID3PHASH {part}")
        );
    }

    #[test]
    fn uses_cookies_for_the_origin_host_only() {
        // A google.com SAPISID is not the one youtube.com requests are signed with.
        let mut g = yt("SAPISID", "google");
        g.domain = ".google.com".into();
        let session = Session {
            cookies: vec![g],
            account: None,
            browser_stamp: None,
        };
        assert_eq!(authorization(&session, ORIGIN, 1), None);
        assert_eq!(authorization(&Session::default(), ORIGIN, 1), None);
        assert_eq!(authorization(&session, "not a url", 1), None);
    }

    /// The client-table shortcut and the `Url` fallback must give exactly what `Url::parse`
    /// does, for the client origins and for anything else.
    #[test]
    fn origin_parts_matches_url_parse() {
        for origin in crate::innertube::clients::ALL
            .iter()
            .map(|c| c.origin)
            .chain([
                "https://WWW.YouTube.com",
                "https://www.youtube.com:443",
                "https://www.youtube.com:8443",
                "https://www.youtube.com/",
                "https://www.youtube.com/youtubei",
                "https://www.youtube.com.",
                "https://1.2.3.4",
                "https://a.123",
                "https://a.0x1f",
                "https://xn--nxasmq6b.com",
                "https://a..b",
                "https://-a-.b-c",
                "http://www.youtube.com",
                "https://",
                "not a url",
            ])
        {
            let want = Url::parse(origin).ok().and_then(|u| {
                Some((
                    u.host_str()?.to_owned(),
                    u.path().to_owned(),
                    u.scheme() == "https",
                ))
            });
            let got = origin_parts(origin).map(|(h, p, s)| (h.into_owned(), p.into_owned(), s));
            assert_eq!(got, want, "{origin}");
        }
        assert!(matches!(
            origin_parts(ORIGIN),
            Some((Cow::Borrowed(_), Cow::Borrowed(_), true))
        ));
    }

    /// `has_sid` answers exactly `authorization(..).is_some()`, without hashing.
    #[test]
    fn has_sid_agrees_with_authorization() {
        let mut google = yt("SAPISID", "google");
        google.domain = ".google.com".into();
        let mut expired = yt("SAPISID", "old");
        expired.expires_utc = Some(1_000);
        let sessions = [
            vec![],
            vec![yt("YSC", "visitor")],
            vec![yt("SAPISID", "a")],
            vec![yt("__Secure-3PAPISID", "a")],
            vec![yt("__Secure-1PAPISID", "a")],
            vec![google.clone()],
            vec![google, yt("__Secure-1PAPISID", "a")],
            vec![expired],
        ];
        let origins = [
            "https://www.youtube.com",
            "https://music.youtube.com",
            "not a url",
        ];
        for cookies in sessions {
            let session = Session {
                cookies,
                account: None,
                browser_stamp: None,
            };
            for origin in origins {
                for now in [1_000, 1_700_000_000, u64::MAX] {
                    assert_eq!(
                        has_sid(&session, origin, now),
                        authorization(&session, origin, now).is_some(),
                        "{:?} {origin} {now}",
                        session.cookies
                    );
                }
            }
        }
    }

    #[test]
    fn expired_sid_is_ignored() {
        let mut c = yt("SAPISID", "abc");
        c.expires_utc = Some(1_000);
        let session = Session {
            cookies: vec![c],
            account: None,
            browser_stamp: None,
        };
        assert_eq!(authorization(&session, ORIGIN, 1_000), None);
    }
}
