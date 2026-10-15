//! The session keeps only the `.youtube.com` cookies, and that changes nothing the engine
//! sends: the `.google.com` and `accounts.google.com` cookies are the whole Google account's
//! sign-in, and no request the engine makes carries them.
//!
//! Fake cookies and a fake Brave Origin profile only.

use rusqlite::Connection;
use url::Url;
use ytmfast::auth::chromium::{self, KeySource};
use ytmfast::auth::{Cookie, Session, sidhash};
use ytmfast::error::Error;

struct NoKeys;

impl KeySource for NoKeys {
    fn passwords(&self) -> Result<Vec<oo7::Secret>, Error> {
        Ok(Vec::new())
    }
}

const HOSTS: [&str; 3] = [".youtube.com", ".google.com", "accounts.google.com"];
const NAMES: [&str; 12] = [
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PSID",
    "__Secure-3PSID",
    "__Secure-1PAPISID",
    "__Secure-3PAPISID",
    "__Secure-1PSIDTS",
    "__Secure-3PSIDTS",
    "LOGIN_INFO",
];

fn cookie(domain: &str, name: &str) -> Cookie {
    Cookie {
        domain: domain.into(),
        name: name.into(),
        value: format!("v-{domain}-{name}"),
        path: "/".into(),
        secure: true,
        expires_utc: None,
    }
}

/// A fake Brave Origin profile whose Cookies file holds every name on all three hosts, plus
/// another site's, with the values in plain text.
fn profile(dir: &std::path::Path) -> std::path::PathBuf {
    let net = dir.join("Network");
    std::fs::create_dir_all(&net).unwrap();
    let db = Connection::open(net.join("Cookies")).unwrap();
    db.execute_batch(
        "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
         INSERT INTO meta VALUES ('version', '24');
         CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL,
           top_frame_site_key TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
           encrypted_value BLOB NOT NULL, path TEXT NOT NULL, expires_utc INTEGER NOT NULL,
           is_secure INTEGER NOT NULL, is_httponly INTEGER NOT NULL,
           last_update_utc INTEGER NOT NULL);",
    )
    .unwrap();
    for host in HOSTS.into_iter().chain(["www.example.com"]) {
        for name in NAMES {
            db.execute(
                "INSERT INTO cookies VALUES (0, ?1, '', ?2, ?3, x'', '/', 0, 1, 1, 1)",
                rusqlite::params![host, name, format!("v-{host}-{name}")],
            )
            .unwrap();
        }
    }
    dir.to_path_buf()
}

#[test]
fn an_import_from_a_profile_with_all_three_hosts_keeps_only_youtube_dot_com() {
    let dir = tempfile::tempdir().unwrap();
    let p = profile(dir.path());
    let session = chromium::import_brave_origin(&p, &NoKeys).unwrap();
    assert_eq!(session.cookies.len(), NAMES.len(), "one of each name");
    assert!(
        session.cookies.iter().all(|c| c.domain == ".youtube.com"),
        "{:?}",
        session
            .cookies
            .iter()
            .map(|c| &c.domain)
            .collect::<Vec<_>>()
    );
    // And what signs a request is there: a SAPISID on .youtube.com.
    assert!(sidhash::authorization(&session, "https://music.youtube.com", 1_800_000_000).is_some());
}

#[test]
fn keeping_only_the_youtube_cookies_changes_no_request() {
    let mut all = Vec::new();
    for domain in HOSTS {
        for name in NAMES {
            all.push(cookie(domain, name));
        }
    }
    let full = Session {
        cookies: all.clone(),
        account: None,
        browser_stamp: None,
    };
    let youtube_only = Session {
        cookies: all
            .into_iter()
            .filter(|c| c.domain == ".youtube.com")
            .collect(),
        account: None,
        browser_stamp: None,
    };
    // Every host the engine can send the session to: its two API hosts, the play-history
    // links' hosts, and every other allowlisted suffix (a link from an answer can name them).
    let urls = [
        "https://music.youtube.com/youtubei/v1/next?prettyPrint=false",
        "https://www.youtube.com/youtubei/v1/player?prettyPrint=false",
        "https://s.youtube.com/api/stats/playback?cl=1",
        "https://www.youtube.com/ptracking?video_id=x",
        "https://rr1---sn-x.googlevideo.com/videoplayback?x=1",
        "https://i.ytimg.com/vi/x/hq.jpg",
        "https://yt3.ggpht.com/a",
        "https://lh3.googleusercontent.com/a",
    ];
    for u in urls {
        let url = Url::parse(u).unwrap();
        assert_eq!(
            full.cookie_header(&url),
            youtube_only.cookie_header(&url),
            "the Cookie header for {} would change",
            url.host_str().unwrap()
        );
    }
    // The signed Authorization is built from the youtube.com origin's cookies only.
    for origin in ["https://www.youtube.com", "https://music.youtube.com"] {
        assert_eq!(
            sidhash::authorization(&full, origin, 1_800_000_000),
            sidhash::authorization(&youtube_only, origin, 1_800_000_000),
            "Authorization for {origin} would change"
        );
    }
}
