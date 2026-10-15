//! The once-a-minute look at the browser's Cookies file stays cheap: heap allocations per read
//! (the same on every machine, checked everywhere) and its time (release builds on a machine in
//! the state the limit was measured in, see `perf_common`).
//!
//! A fake profile with a Cookies file of a real browser's size (about 540 KB). A counting
//! allocator makes this its own test binary.
//!
//! Limits are the measured value plus 20%. The audit measured 0.347 ms and 37 allocations for a
//! 537 KB file; here, 2026-10-14: see the constants.

mod perf_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use rusqlite::Connection;
use ytmfast::auth::chromium;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call to the system allocator unchanged, after counting it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// 45 measured (37 before the table-and-limits checks of the Cookies fixes) times 1.2.
const MAX_ALLOCS: u64 = 54;
/// 0.149 to 0.154 ms in three runs (the audit's 0.347 ms was another file) times 1.2.
const MAX_MS: f64 = 0.19;

fn profile(dir: &std::path::Path) {
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
           last_update_utc INTEGER NOT NULL);
         BEGIN;",
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
    for i in 0..3_600 {
        db.execute(
            "INSERT INTO cookies VALUES (0, ?1, '', 'tracker', ?2, x'', '/', 0, 1, 1, 1)",
            rusqlite::params![format!("site{i}.example"), "x".repeat(100)],
        )
        .unwrap();
    }
    db.execute_batch("COMMIT;").unwrap();
}

#[test]
fn the_cookie_stamp_read_stays_within_its_limits() {
    let dir = tempfile::tempdir().unwrap();
    profile(dir.path());
    let size = std::fs::metadata(dir.path().join("Network/Cookies"))
        .unwrap()
        .len();
    assert!(size > 450_000, "the fake file is {size} bytes");
    // Warm, and the answer is right.
    assert_eq!(
        chromium::brave_sign_in_stamp(dir.path()).unwrap(),
        Some(7_000)
    );

    let a0 = ALLOCS.load(Relaxed);
    for _ in 0..20 {
        chromium::brave_sign_in_stamp(dir.path()).unwrap();
    }
    let allocs = (ALLOCS.load(Relaxed) - a0) / 20;
    println!("{size}-byte file: {allocs} allocations per read");
    assert!(
        allocs <= MAX_ALLOCS,
        "{allocs} allocations per read, the limit is {MAX_ALLOCS}"
    );

    if perf_common::speed_limits_apply("perf_stamp") {
        let mut ms: Vec<f64> = (0..200)
            .map(|_| {
                let t = Instant::now();
                chromium::brave_sign_in_stamp(dir.path()).unwrap();
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        let median = perf_common::median(&mut ms);
        println!("{size}-byte file: median {median:.3} ms per read");
        assert!(
            median <= MAX_MS,
            "{median:.3} ms per read, the limit is {MAX_MS}"
        );
    }
}
