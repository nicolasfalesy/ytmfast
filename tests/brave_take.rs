//! A take from the Brave Origin profile reads its Cookies file once, and the sign-in stamp it
//! records is the one of the copy the cookies came from.
//!
//! This file holds one test on purpose: it counts the bytes the whole process has read
//! (`rchar` in /proc/self/io), which other tests running beside it would disturb.
//! Fake cookies and a fake profile only.

use rusqlite::Connection;
use ytmfast::auth::chromium::{self, KeySource};
use ytmfast::error::Error;

struct NoKeys;

impl KeySource for NoKeys {
    fn passwords(&self) -> Result<Vec<oo7::Secret>, Error> {
        Ok(Vec::new())
    }
}

fn rchar() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("rchar:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap()
}

/// A fake profile: the sign-in cookies on .youtube.com (the newest written at 7_000), and
/// thousands of other sites' rows to make the file a few hundred KB, like a real browser's.
fn profile(dir: &std::path::Path) -> u64 {
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
    for (i, name) in ["SAPISID", "__Secure-1PSID", "__Secure-3PSIDTS"]
        .into_iter()
        .enumerate()
    {
        db.execute(
            "INSERT INTO cookies VALUES (0, '.youtube.com', '', ?1, 'fake-value', x'', '/', 0, 1, 1, ?2)",
            rusqlite::params![name, 5_000 + 1_000 * i as i64],
        )
        .unwrap();
    }
    for i in 0..4_000 {
        db.execute(
            "INSERT INTO cookies VALUES (0, ?1, '', 'tracker', ?2, x'', '/', 0, 1, 1, 1)",
            rusqlite::params![format!("site{i}.example"), "x".repeat(100)],
        )
        .unwrap();
    }
    drop(db);
    std::fs::metadata(net.join("Cookies")).unwrap().len()
}

#[test]
fn a_take_reads_the_cookies_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let size = profile(dir.path());
    assert!(size > 300_000, "the fake file is {size} bytes");
    let before = rchar();
    let (session, stamp) = chromium::import_brave_origin_with_stamp(dir.path(), &NoKeys).unwrap();
    let read = rchar() - before;
    assert_eq!(session.cookies.len(), 3);
    assert_eq!(stamp, Some(7_000));
    // Once, with room for the few KB the process reads besides.
    assert!(
        read < size + size / 4,
        "the take read {read} bytes of a {size}-byte file ({} times)",
        read / size
    );
}
