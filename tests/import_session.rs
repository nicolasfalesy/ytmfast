//! `ytmfast import-session` refuses a profile that isn't signed in, before it touches the
//! keyring.
//!
//! The command runs as its own process with every folder in a temp dir and the D-Bus address
//! pointing at nothing, so it can never reach the real keyring: a save it tried would fail
//! with "keyring locked or unavailable", which is how the signed-in case below shows it got
//! that far. The profiles are hand-made Chromium cookie databases with fake values.
//!
//! Before saving, the import asks YouTube Music for the account's name (ruling P19). Here that
//! request goes to a local wiremock server through `YTMFAST_TEST_API_BASE`, which only a debug
//! build reads; every run sets it (a dead port where the test never gets that far), so no test
//! can reach YouTube.

use std::path::Path;
use std::process::{Command, Output};

use rusqlite::{Connection, params};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Where the account check goes in a test that never gets that far: a port nothing listens
/// on (the discard port), so a mistake fails instead of reaching the network.
const DEAD_API: &str = "http://127.0.0.1:9";

/// A fake YouTube Music answering the account check with `answer`.
async fn fake_api(answer: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/account/account_menu"))
        .respond_with(answer)
        .mount(&server)
        .await;
    server
}

/// The account menu of a signed-in session, with a made-up name.
fn signed_in_menu(name: &str) -> ResponseTemplate {
    let body = json!({"actions": [{"openPopupAction": {"popup": {"multiPageMenuRenderer": {
        "header": {"activeAccountHeaderRenderer": {"accountName": {"runs": [{"text": name}]}}}
    }}}}]});
    ResponseTemplate::new(200).set_body_json(body)
}

/// The account menu of a session that no longer signs anyone in: no account header.
fn signed_out_menu() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"actions": [{"openPopupAction": {"popup":
        {"multiPageMenuRenderer": {"sections": []}}}}]}))
}

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

fn import(root: &Path, profile: &Path, api: &str) -> Output {
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ytmfast"))
        .args(["import-session", "--profile"])
        .arg(profile)
        .env_clear()
        .env("YTMFAST_TEST_API_BASE", api)
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
    let out = import(root.path(), &p, DEAD_API);
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

#[tokio::test]
async fn a_signed_in_profile_goes_on_to_the_keyring() {
    let api = fake_api(signed_in_menu("Fake Person")).await;
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("profile");
    profile(&p, &[(".youtube.com", "SAPISID"), (".youtube.com", "SID")]);
    let out = import(root.path(), &p, &api.uri());
    let stderr = String::from_utf8_lossy(&out.stderr);
    if app_running(&stderr) {
        return;
    }
    // The account's name, before the save (ruling P19), on a line of its own the bar widget
    // reads as it is.
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Signed in as Fake Person\n"
    );
    assert_eq!(api.received_requests().await.unwrap().len(), 1);
    // No keyring here, so the save fails; that it was tried is the point.
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("keyring locked or unavailable"), "{stderr}");
    assert!(!stderr.contains("fake-value"));
}

#[tokio::test]
async fn a_session_youtube_turns_down_is_refused_before_the_keyring() {
    // The cookies are there, but YouTube Music signs nobody in with them (signed out in the
    // browser since): nothing is saved over the working session in the keyring.
    let api = fake_api(signed_out_menu()).await;
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("profile");
    profile(&p, &[(".youtube.com", "SAPISID"), (".youtube.com", "SID")]);
    let out = import(root.path(), &p, &api.uri());
    let stderr = String::from_utf8_lossy(&out.stderr);
    if app_running(&stderr) {
        return;
    }
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(
        stderr,
        "ytmfast: no YouTube sign-in in that profile; sign in to the app first\n"
    );
    assert!(out.stdout.is_empty());
    // A 401 is the same answer.
    let api = fake_api(ResponseTemplate::new(401)).await;
    let out = import(root.path(), &p, &api.uri());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "ytmfast: no YouTube sign-in in that profile; sign in to the app first\n"
    );
}

#[tokio::test]
async fn a_check_that_fails_saves_nothing() {
    // YouTube unreachable (or answering an error): the session can't be checked, so it isn't
    // saved; one fixed line says so (the bar widget shows it as it is).
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("profile");
    profile(&p, &[(".youtube.com", "SAPISID"), (".youtube.com", "SID")]);
    let api = fake_api(ResponseTemplate::new(500)).await;
    for base in [DEAD_API.to_owned(), api.uri()] {
        let out = import(root.path(), &p, &base);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if app_running(&stderr) {
            return;
        }
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(
            stderr
                .ends_with("ytmfast: could not check the sign-in with YouTube Music; try again\n"),
            "{stderr}"
        );
        assert!(!stderr.contains("keyring"), "{stderr}");
        assert!(out.stdout.is_empty());
    }
}

// ---- --browser brave-origin ---------------------------------------------------------------

/// `import-session --browser brave-origin [args]` with the same throwaway folders and dead bus.
fn import_brave(root: &Path, args: &[&Path]) -> Output {
    import_brave_with(root, args, DEAD_API)
}

/// `import_brave` with the account check sent to `api`.
fn import_brave_with(root: &Path, args: &[&Path], api: &str) -> Output {
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ytmfast"));
    cmd.args(["import-session", "--browser", "brave-origin"]);
    for p in args {
        cmd.arg("--profile").arg(p);
    }
    cmd.env_clear()
        .env("YTMFAST_TEST_API_BASE", api)
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

/// A Brave Origin profile as Brave Origin lays it out: `Default/Cookies`, no `Network/`. One
/// SAPISID encrypted with a keyring key (`v11`), so the import must ask the keyring.
fn brave_profile(dir: &Path) {
    profile(dir, &[(".youtube.com", "SID")]);
    std::fs::rename(dir.join("Network/Cookies"), dir.join("Cookies")).unwrap();
    let db = Connection::open(dir.join("Cookies")).unwrap();
    db.execute(
        "INSERT INTO cookies VALUES (0, '.youtube.com', '', 'SAPISID', '', ?1, '/', ?2, 1, 1)",
        params![b"v11-not-really-encrypted".to_vec(), EXPIRES],
    )
    .unwrap();
}

#[test]
fn brave_origin_without_a_profile_says_so() {
    let root = tempfile::tempdir().unwrap();
    let out = import_brave(root.path(), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(
        stderr,
        "ytmfast: internal error: no Brave Origin profile; open Brave Origin once, or pass --profile <folder>\n"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn brave_origin_signed_out_is_refused_before_the_keyring() {
    let root = tempfile::tempdir().unwrap();
    // The default place: ~/.config/BraveSoftware/Brave-Origin/Default.
    let p = root
        .path()
        .join(".config/BraveSoftware/Brave-Origin/Default");
    profile(&p, &[(".youtube.com", "VISITOR_INFO1_LIVE")]);
    let out = import_brave(root.path(), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(
        stderr,
        "ytmfast: Brave Origin isn't signed in to YouTube Music\n"
    );
    // Nothing YouTube at all is the same answer.
    let other = root.path().join("other");
    profile(&other, &[(".example.com", "SAPISID")]);
    let out = import_brave(root.path(), &[&other]);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "ytmfast: Brave Origin isn't signed in to YouTube Music\n"
    );
}

#[test]
fn brave_origin_v11_cookies_need_the_keyring() {
    // No Secret Service here, so the key can't be read: the keyring's own fixed message, and
    // no cookie value anywhere in the output.
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("brave");
    brave_profile(&p);
    let out = import_brave(root.path(), &[&p]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(
        stderr,
        "ytmfast: internal error: keyring locked or unavailable\n"
    );
    assert!(!stderr.contains("fake-value") && !stderr.contains("v11-not"));
    assert!(out.stdout.is_empty());
}

#[tokio::test]
async fn brave_origin_plain_sign_in_goes_on_to_the_keyring() {
    // A profile Brave wrote without a keyring (plain values): no key needed, so it reaches
    // the check and then the save, which fails here for want of a keyring. `pear-desktop`
    // running doesn't matter.
    let api = fake_api(signed_in_menu("Fake Person")).await;
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("brave");
    profile(&p, &[(".youtube.com", "SAPISID"), (".youtube.com", "SID")]);
    let out = import_brave_with(root.path(), &[&p], &api.uri());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Signed in as Fake Person\n"
    );
    assert!(stderr.contains("keyring locked or unavailable"), "{stderr}");
    assert!(!stderr.contains("pear-desktop"), "{stderr}");
    assert!(!stderr.contains("fake-value"));
}

#[tokio::test]
async fn brave_origin_signed_out_at_youtube_is_refused_before_the_keyring() {
    // Brave still holds the cookies, but they sign nobody in: the widget's own fixed text.
    let api = fake_api(signed_out_menu()).await;
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("brave");
    profile(&p, &[(".youtube.com", "SAPISID"), (".youtube.com", "SID")]);
    let out = import_brave_with(root.path(), &[&p], &api.uri());
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "ytmfast: Brave Origin isn't signed in to YouTube Music\n"
    );
    assert!(out.stdout.is_empty());
}
