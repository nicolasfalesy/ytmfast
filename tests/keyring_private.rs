//! `KeyringStore` against a real Secret Service: a throwaway `gnome-keyring-daemon` on a
//! private D-Bus bus, with its own home folder and a fresh, unlocked login keyring.
//!
//! The bus config lists no service folders, so nothing can be auto-started on it, and both
//! ways zbus finds the session bus point at the private one, so the user's own keyring is
//! never reached. Ignored by default because it needs `dbus-daemon` and
//! `gnome-keyring-daemon` installed (CI has neither); run it with
//! `cargo test --test keyring_private -- --ignored`.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ytmfast::auth::{Cookie, KeyringStore, Session, SessionStore};
use ytmfast::error::Error;

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
        .env("DBUS_SESSION_BUS_ADDRESS", bus);
}

fn session(value: &str) -> Session {
    Session {
        cookies: vec![Cookie {
            domain: ".youtube.com".into(),
            name: "SAPISID".into(),
            value: value.into(),
            path: "/".into(),
            secure: true,
            expires_utc: Some(1_900_000_000),
        }],
    }
}

#[tokio::test]
#[ignore = "needs dbus-daemon and gnome-keyring-daemon; run with --ignored"]
async fn keyring_store_roundtrip_on_a_private_secret_service() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    for d in ["home", "run"] {
        std::fs::create_dir(r.join(d)).unwrap();
    }
    let socket = r.join("run/bus");
    let bus = format!("unix:path={}", socket.display());
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
            socket.display()
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
    let start = Instant::now();
    while !socket.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bus never came up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // `--unlock` reads a password on stdin and creates (and unlocks) the login keyring.
    let mut cmd = Command::new("gnome-keyring-daemon");
    cmd.args(["--foreground", "--components=secrets", "--unlock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    private_env(&mut cmd, r, &bus);
    let mut keyring = cmd.spawn().expect("gnome-keyring-daemon");
    {
        use std::io::Write;
        let mut stdin = keyring.stdin.take().unwrap();
        stdin.write_all(b"test-password").unwrap();
    }
    let _keyring = Reap(keyring);

    let store = KeyringStore::new();
    // Until the daemon owns its bus name, loads fail as "unavailable"; then, with no item
    // yet, they are SignedOut.
    let start = Instant::now();
    loop {
        match store.load().await {
            Err(Error::SignedOut) => break,
            Err(Error::Internal(_)) if start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            other => panic!("expected SignedOut from an empty keyring, got {other:?}"),
        }
    }

    store.save(&session("first")).await.unwrap();
    assert_eq!(store.load().await.unwrap(), session("first"));
    // A second save replaces the first: there is only ever one ytmfast item.
    store.save(&session("second")).await.unwrap();
    assert_eq!(store.load().await.unwrap(), session("second"));

    let service = oo7::dbus::Service::new().await.unwrap();
    let items = service
        .default_collection()
        .await
        .unwrap()
        .search_items(&[("application", "ytmfast")])
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label().await.unwrap(), "ytmfast session");

    // The background (cookie rotation) path saves to an unlocked keyring like `save` does...
    store.save_without_prompt(&session("third")).await.unwrap();
    assert_eq!(store.load().await.unwrap(), session("third"));
    // ...but on a locked one it refuses at once instead of raising an unlock prompt. Nothing
    // here can answer a prompt (no prompter can start on this bus), so a prompt would hang.
    let collection = service.default_collection().await.unwrap();
    collection.lock(None).await.unwrap();
    assert!(collection.is_locked().await.unwrap());
    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        store.save_without_prompt(&session("fourth")),
    )
    .await
    .expect("the no-prompt save waited on a prompt");
    assert_eq!(
        refused,
        Err(Error::Internal(
            "keyring locked; the refreshed session was not saved".into()
        ))
    );
}
