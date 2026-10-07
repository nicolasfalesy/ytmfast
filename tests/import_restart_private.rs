//! `ytmfast import-session` with an engine running: the import is saved, the engine is asked
//! to quit and exits, so its old in-memory session can't be written back over the new one.
//!
//! Everything runs against a throwaway `gnome-keyring-daemon` on a private D-Bus bus (no
//! service folders, so nothing can be auto-started on it), with every folder in a temp dir;
//! the engine uses `--null-sink`, so nothing reaches the speakers. Ignored by default because
//! it needs `dbus-daemon` and `gnome-keyring-daemon` (CI has neither); run it with
//! `cargo test --test import_restart_private -- --ignored`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use ytmfast::auth::{KeyringStore, SessionStore};
use ytmfast::error::Error;

const WAIT: Duration = Duration::from_secs(10);

/// A fake YouTube Music for the import's account check (ruling P19), so the import never
/// reaches YouTube: the command sends the check to it through `YTMFAST_TEST_API_BASE` (read
/// by debug builds only). It answers with a made-up name.
async fn fake_api() -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let body = serde_json::json!({"actions": [{"openPopupAction": {"popup": {
        "multiPageMenuRenderer": {"header": {"activeAccountHeaderRenderer": {
            "accountName": {"runs": [{"text": "Fake Person"}]}}}}}}}]});
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/account/account_menu"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

/// Kills a child process when the test ends, pass or panic.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_env(cmd: &mut Command, root: &Path, bus: &str) {
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("home/data"))
        .env("XDG_CONFIG_HOME", root.join("home/config"))
        .env("XDG_STATE_HOME", root.join("home/state"))
        .env("XDG_CACHE_HOME", root.join("home/cache"))
        .env("DBUS_SESSION_BUS_ADDRESS", bus);
}

/// A signed-in profile: a `Network/Cookies` database with fake plain values.
fn profile(dir: &Path) {
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
    let expires: i64 = (1_900_000_000 + 11_644_473_600) * 1_000_000;
    for name in ["SAPISID", "SID"] {
        db.execute(
            "INSERT INTO cookies VALUES (0, '.youtube.com', '', ?1, 'fake-new', x'', '/', ?2, 1, 1)",
            params![name, expires],
        )
        .unwrap();
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < WAIT, "{what} never happened");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
#[ignore = "needs dbus-daemon and gnome-keyring-daemon; run with --ignored"]
async fn import_stops_the_running_engine() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    for d in ["home", "run"] {
        std::fs::create_dir(r.join(d)).unwrap();
    }
    let bus_socket = r.join("run/bus");
    let bus = format!("unix:path={}", bus_socket.display());
    let config = r.join("bus.conf");
    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
            bus_socket.display()
        ),
    )
    .unwrap();
    // SAFETY: this binary has exactly one test, so no other thread reads the environment.
    // Set before any D-Bus use, so zbus can only ever find the private bus.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &bus);
        std::env::set_var("XDG_RUNTIME_DIR", r.join("run"));
    }

    let mut cmd = Command::new("dbus-daemon");
    cmd.arg(format!("--config-file={}", config.display()))
        .arg("--nofork")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    private_env(&mut cmd, r, &bus);
    let _bus = Reap(cmd.spawn().expect("dbus-daemon"));
    wait_for("the bus", || bus_socket.exists());

    let mut cmd = Command::new("gnome-keyring-daemon");
    cmd.args(["--foreground", "--components=secrets", "--unlock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    private_env(&mut cmd, r, &bus);
    let mut keyring = cmd.spawn().expect("gnome-keyring-daemon");
    keyring
        .stdin
        .take()
        .unwrap()
        .write_all(b"test-password")
        .unwrap();
    let _keyring = Reap(keyring);
    let store = KeyringStore::new();
    let start = Instant::now();
    loop {
        match store.load().await {
            Err(Error::SignedOut) => break,
            Err(Error::Internal(_)) if start.elapsed() < WAIT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            other => panic!("expected an empty keyring, got {other:?}"),
        }
    }

    // The engine, serving its socket.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ytmfast"));
    cmd.args(["daemon", "--null-sink"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    private_env(&mut cmd, r, &bus);
    let mut engine = Reap(cmd.spawn().unwrap());
    let socket = r.join("run/ytmfast/socket");
    wait_for("the engine serving", || {
        let Ok(mut s) = UnixStream::connect(&socket) else {
            return false;
        };
        s.set_read_timeout(Some(WAIT)).unwrap();
        s.write_all(b"{\"id\":1,\"cmd\":\"status\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        line.contains("\"ok\":true")
    });

    let p = r.join("profile");
    profile(&p);
    let api = fake_api().await;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ytmfast"));
    cmd.args(["import-session", "--profile"]).arg(&p);
    private_env(&mut cmd, r, &bus);
    cmd.env("YTMFAST_TEST_API_BASE", api.uri());
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("pear-desktop is running") {
        eprintln!("skipped: the YouTube Music desktop app is running");
        return;
    }
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("Signed in as Fake Person"), "{stdout}");
    assert!(stdout.contains("Imported 2 cookies"), "{stdout}");
    assert!(stdout.contains("Restarted the running engine"), "{stdout}");
    assert!(!format!("{stdout}{stderr}").contains("fake-new"));

    // The engine quit cleanly.
    let status = engine.0.wait().unwrap();
    assert!(status.success(), "{status:?}");
    // And the keyring holds the import.
    let saved = store.load().await.unwrap();
    let names: Vec<&str> = saved.cookies.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["SAPISID", "SID"]);
    assert!(saved.cookies.iter().all(|c| c.value == "fake-new"));

    // With no engine running, the import says so and starts none. Only checkable when the
    // user has no engine of their own running (the check reads the real `/proc`).
    // SAFETY: getuid has no preconditions and can't fail.
    let uid = unsafe { libc::getuid() };
    if !ytmfast::control::stop::daemon_pids(Path::new("/proc"), uid, 0).is_empty() {
        eprintln!("skipped the no-engine half: an engine of the user's own is running");
        return;
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ytmfast"));
    cmd.args(["import-session", "--profile"]).arg(&p);
    private_env(&mut cmd, r, &bus);
    cmd.env("YTMFAST_TEST_API_BASE", api.uri());
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("No engine was running"), "{stdout}");
    assert!(!socket.exists(), "the stopped engine removed its socket");
}
