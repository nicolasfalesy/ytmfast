//! The `Authorization` header signed-in YouTube API calls carry, built from the session.
//!
//! This follows yt-dlp's `_get_sid_authorization_header`: for each of three schemes whose
//! cookie is present, `"{scheme} {ts}_{sha1_hex("{ts} {sid} {origin}")}"`, joined by spaces.

use std::fmt::Write as _;

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
    // Every signed request runs this, so it writes into one output string and reuses one
    // input buffer and a stack hex buffer, instead of a String per hash input, hex and part
    // plus a Vec and a join (measured 2026-11-07, see the PR).
    // Sized up front so neither grows: each part is a scheme (13 bytes), a space, a 10-digit
    // time, "_" and 40 hex, plus the space between parts.
    let mut out = String::with_capacity(3 * 66);
    let mut input = String::with_capacity(origin.len() + 64);
    for (scheme, sid) in schemes {
        let Some(sid) = sid else { continue };
        input.clear();
        // Writing into a String cannot fail.
        let _ = write!(input, "{now_unix} {sid} {origin}");
        let mut hex = [0u8; 40];
        // A SHA-1 is 20 bytes, so its hex fills the 40 exactly: this cannot fail either.
        hex::encode_to_slice(Sha1::digest(input.as_bytes()), &mut hex).ok()?;
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(
            out,
            "{scheme} {now_unix}_{}",
            std::str::from_utf8(&hex).ok()?
        );
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
