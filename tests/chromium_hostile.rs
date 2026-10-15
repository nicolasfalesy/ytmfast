//! A damaged or hostile Chromium `Cookies` file must not cost the engine unbounded time or
//! memory. The engine reads this file once a minute (the follow-Brave check), and the file
//! belongs to another program running as the same user.
//!
//! Fake data only: every database here is built in a temp folder.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use ytmfast::auth::chromium;

/// The memory tests read the process's peak, so they run one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn sql_profile(dir: &Path, sql: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let db = Connection::open(dir.join("Cookies")).unwrap();
    db.execute_batch(sql).unwrap();
    dir.to_path_buf()
}

fn peak_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap()
}

/// How much the stamp read grows the process's peak memory, in MB, and how long it takes.
fn stamp_cost(profile: &Path) -> (u64, Duration, bool) {
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // Resets the peak: building the file may have touched memory.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = peak_kb();
    let t = Instant::now();
    let r = chromium::brave_sign_in_stamp(profile);
    (
        peak_kb().saturating_sub(before) / 1024,
        t.elapsed(),
        r.is_err(),
    )
}

/// A `cookies` view whose definition does unbounded work.
#[test]
fn a_cookie_view_cannot_pin_the_stamp_read() {
    const ROWS: u64 = 30_000_000;
    let dir = tempfile::tempdir().unwrap();
    let p = sql_profile(
        dir.path(),
        &format!(
            "CREATE TABLE meta(key,value); INSERT INTO meta VALUES ('version','24');
             CREATE VIEW cookies AS
               WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{ROWS})
               SELECT '.youtube.com' AS host_key, 'SAPISID' AS name, x AS last_update_utc FROM c;"
        ),
    );
    let (_, took, refused) = stamp_cost(&p);
    assert!(refused, "a view named cookies was read");
    assert!(took < Duration::from_secs(1), "took {took:?}");
}

/// A view that builds one 100 MB string per query.
#[test]
fn a_cookie_view_cannot_make_the_stamp_read_allocate_hundreds_of_megabytes() {
    let dir = tempfile::tempdir().unwrap();
    let p = sql_profile(
        dir.path(),
        "CREATE TABLE meta(key,value); INSERT INTO meta VALUES ('version','24');
         CREATE VIEW cookies AS
           SELECT '.youtube.com' AS host_key, 'SAPISID' AS name,
                  length(hex(zeroblob(100000000))) AS last_update_utc;",
    );
    let (grew_mb, _, refused) = stamp_cost(&p);
    assert!(refused, "a view named cookies was read");
    assert!(grew_mb < 100, "peak memory grew {grew_mb} MB");
}

/// A real table (no view check can catch it) whose generated column builds a 100 MB string
/// per row, from a file of a few KB.
#[test]
fn a_generated_column_cannot_make_the_stamp_read_allocate_hundreds_of_megabytes() {
    let dir = tempfile::tempdir().unwrap();
    let p = sql_profile(
        dir.path(),
        "CREATE TABLE meta(key,value);
         CREATE TABLE cookies(host_key TEXT, name TEXT,
           last_update_utc INTEGER AS (length(hex(zeroblob(100000000)))));
         INSERT INTO cookies(host_key, name) VALUES ('.youtube.com','SAPISID');",
    );
    let (grew_mb, _, _) = stamp_cost(&p);
    assert!(grew_mb < 100, "peak memory grew {grew_mb} MB");
}

/// The defences must not touch a normal file.
#[test]
fn a_normal_cookies_table_still_gives_its_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let p = sql_profile(
        dir.path(),
        "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
         INSERT INTO meta VALUES ('version','24');
         CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL,
           top_frame_site_key TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
           encrypted_value BLOB NOT NULL, path TEXT NOT NULL, expires_utc INTEGER NOT NULL,
           is_secure INTEGER NOT NULL, is_httponly INTEGER NOT NULL, last_update_utc INTEGER NOT NULL);
         INSERT INTO cookies VALUES (0,'.youtube.com','','SAPISID','',x'','/',0,1,1,777);",
    );
    assert_eq!(chromium::brave_sign_in_stamp(&p).unwrap(), Some(777));
}
