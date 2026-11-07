//! The `Authorization` header signed-in YouTube API calls carry, built from the session.
//!
//! This follows yt-dlp's `_get_sid_authorization_header`: for each of three schemes whose
//! cookie is present, `"{scheme} {ts}_{sha1_hex("{ts} {sid} {origin}")}"`, joined by spaces.

use sha1::{Digest, Sha1};
use url::Url;

use super::Session;

/// The `Authorization` value for a request sent with `Origin: origin` at `now_unix`, or
/// `None` when the session holds none of the SAPISID cookies (or `origin` is not a URL).
pub fn authorization(session: &Session, origin: &str, now_unix: u64) -> Option<String> {
    // The SID cookies are the ones the origin's own host would get (yt-dlp reads them for
    // `.youtube.com`), so a `.google.com` SAPISID never signs a youtube.com request.
    let origin_url = Url::parse(origin).ok()?;
    let cookies = session.matching(&origin_url, i64::try_from(now_unix).ok()?);
    let get = |name: &str| {
        cookies
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
    };
    let schemes = [
        (
            "SAPISIDHASH",
            get("SAPISID").or_else(|| get("__Secure-3PAPISID")),
        ),
        ("SAPISID1PHASH", get("__Secure-1PAPISID")),
        ("SAPISID3PHASH", get("__Secure-3PAPISID")),
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
