//! `ytmfast import-session` refuses a profile that isn't signed in, before it touches the
//! keyring.
//!
//! The command runs as its own process with every folder in a temp dir and the D-Bus address
//! pointing at nothing, so it can never reach the real keyring: a save it tried would fail
//! with "keyring locked or unavailable", which is how the signed-in case below shows it got
//! that far. The profiles are hand-made Chromium cookie databases with fake values.

use std::path::Path;
use std::process::{Command, Output};

use rusqlite::{Connection, params};

/// Chromium's `expires_utc` for a far-future Unix time: microseconds since 1601-01-01.
const EXPIRES: i64 = (1_900_000_000 + 11_644_473_600) * 1_000_000;

/// A profile folder with a `Network/Cookies` database holding `cookies` (host, name) as
/// plain values.
fn profile(dir: &Path, cookies: &[(&str, &str)]) {
    let net = dir.join("Network");
    std::fs::create_dir_all(&net).unwrap();
    let db = Connection::open(net.join("Cookies")).unwrap();
    db.execute_batch(
        "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
         INSERT INTO meta VALUES ('version', '24');
         CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL,
           top_frame_site_key TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
           encrypted_value BLOB NOT NULL, path TEXT NOT NULL, expires_utc INTEGER NOT NULL,
           is_secure INTEGER NOT NULL, is_httponly INTEGER NOT NULL);",
    )
    .unwrap();
    for (host, name) in cookies {
        db.execute(
            "INSERT INTO cookies VALUES (0, ?1, '', ?2, 'fake-value', x'', '/', ?3, 1, 1)",
            params![host, name, EXPIRES],
        )
        .unwrap();
    }
}

fn import(root: &Path, profile: &Path) -> Output {
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ytmfast"))
        .args(["import-session", "--profile"])
        .arg(profile)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("XDG_RUNTIME_DIR", &run)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/no-bus", root.display()),
        )
        .output()
        .unwrap()
}

/// The import also refuses while the desktop app runs (it checks the real `/proc`); then
/// these tests can't tell anything and say so instead of failing.
fn app_running(stderr: &str) -> bool {
    let running = stderr.contains("pear-desktop is running");
    if running {
        eprintln!("skipped: the YouTube Music desktop app is running");
    }
    running
}

#[test]
fn visitor_cookies_are_refused_before_the_keyring() {
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("profile");
    // YouTube cookies a signed-out visitor gets: no SAPISID.
    profile(
        &p,
        &[
            (".youtube.com", "VISITOR_INFO1_LIVE"),
            (".youtube.com", "YSC"),
        ],
    );
    let out = import(root.path(), &p);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if app_running(&stderr) {
        return;
    }
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("no YouTube sign-in"), "{stderr}");
    assert!(
        !stderr.contains("keyring"),
        "the keyring was tried: {stderr}"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn a_signed_in_profile_goes_on_to_the_keyring() {
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("profile");
    profile(&p, &[(".youtube.com", "SAPISID"), (".youtube.com", "SID")]);
    let out = import(root.path(), &p);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if app_running(&stderr) {
        return;
    }
    // No keyring here, so the save fails; that it was tried is the point.
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("keyring locked or unavailable"), "{stderr}");
    assert!(!stderr.contains("fake-value"));
}
