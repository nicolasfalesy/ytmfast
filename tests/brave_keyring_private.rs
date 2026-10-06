//! `import-session --browser brave-origin` against a real Secret Service: a throwaway
//! `gnome-keyring-daemon` on a private D-Bus bus, with its own home folder and a fresh,
//! unlocked login keyring holding fake "Brave Safe Storage" items.
//!
//! The bus config lists no service folders, so nothing can be auto-started on it, and both
//! ways zbus finds the session bus point at the private one, so the user's own keyring is
//! never reached. Ignored by default because it needs `dbus-daemon` and
//! `gnome-keyring-daemon` installed (CI has neither); run it with
//! `cargo test --test brave_keyring_private -- --ignored`.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use ytmfast::auth::chromium::KeySource;
use ytmfast::auth::{KeyringStore, SafeStorageKeys, SessionStore};

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

const RIGHT: &str = "fake-right-password";
const SCHEMA: (&str, &str) = ("xdg:schema", "chrome_libsecret_os_crypt_password_v2");

/// Chromium's `v11` value of `value` for `host` (database version 24) under `password`.
fn v11(password: &str, host: &str, value: &str) -> Vec<u8> {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1, &mut key);
    let mut plaintext = Sha256::digest(host.as_bytes()).to_vec();
    plaintext.extend_from_slice(value.as_bytes());
    let ct = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &[b' '; 16].into())
        .encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
    [b"v11".as_slice(), &ct].concat()
}

/// A Brave Origin profile (`Default/Cookies`) whose sign-in cookie is `v11` under `RIGHT`.
fn brave_profile(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let db = Connection::open(dir.join("Cookies")).unwrap();
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
    db.execute(
        "INSERT INTO cookies VALUES (0, '.youtube.com', '', 'SAPISID', '', ?1, '/', ?2, 1, 1)",
        params![v11(RIGHT, ".youtube.com", "fake-sapisid"), expires],
    )
    .unwrap();
}

/// The passwords `SafeStorageKeys` finds, sorted (the keyring's order is its own).
fn found() -> Vec<String> {
    let mut got: Vec<String> = SafeStorageKeys::brave()
        .passwords()
        .unwrap()
        .iter()
        .map(|s| String::from_utf8(s.as_bytes().to_vec()).unwrap())
        .collect();
    got.sort();
    got
}

#[test]
#[ignore = "needs dbus-daemon and gnome-keyring-daemon; run with --ignored"]
fn brave_origin_import_reads_the_safe_storage_key() {
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

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Until the daemon owns its bus name and has its login keyring, the lookup fails.
    let start = Instant::now();
    let collection = loop {
        let ready = runtime.block_on(async {
            let service = oo7::dbus::Service::new().await.ok()?;
            service
                .with_alias(oo7::dbus::Service::DEFAULT_COLLECTION)
                .await
                .ok()?
        });
        match ready {
            Some(c) => break c,
            None => {
                assert!(start.elapsed() < Duration::from_secs(10), "no keyring");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let add = |label: &str, attributes: &[(&str, &str)], secret: &str| {
        runtime
            .block_on(collection.create_item(
                label,
                &attributes.to_vec(),
                oo7::Secret::text(secret),
                false,
                None,
            ))
            .unwrap()
    };

    // Nothing yet: an empty list, which the import turns into its "not in the keyring" text.
    assert!(found().is_empty());

    // Found by its label alone when nothing has Chromium's attributes (an item an older
    // Chromium saved), and other apps' items are left out.
    let legacy = add("Brave Safe Storage", &[("legacy", "1")], "fake-legacy");
    add(
        "Chromium Safe Storage",
        &[SCHEMA, ("application", "chromium")],
        "fake-chromium",
    );
    assert_eq!(found(), ["fake-legacy"]);

    // Two items with Chromium's attributes: both found (and the label-only one no longer).
    add(
        "Brave Safe Storage",
        &[SCHEMA, ("application", "brave")],
        "fake-wrong",
    );
    add(
        "Brave Safe Storage",
        &[SCHEMA, ("application", "brave")],
        RIGHT,
    );
    assert_eq!(found(), [RIGHT, "fake-wrong"]);
    runtime.block_on(legacy.delete(None)).unwrap();

    // The whole command: the wrong key is tried and passed over, the session saved.
    let profile = r.join("home/config/BraveSoftware/Brave-Origin/Default");
    brave_profile(&profile);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ytmfast"));
    cmd.args(["import-session", "--browser", "brave-origin"]);
    private_env(&mut cmd, r, &bus);
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // What follows the save depends on whether an engine runs on this machine; only the
    // import itself is this test's business.
    assert!(
        stdout.starts_with("Imported 1 cookies\n"),
        "{stdout} {stderr}"
    );
    assert!(!format!("{stdout}{stderr}").contains("fake-sapisid"));
    let saved = runtime.block_on(KeyringStore::new().load()).unwrap();
    assert_eq!(saved.cookies.len(), 1);
    assert_eq!(saved.cookies[0].name, "SAPISID");
    assert_eq!(saved.cookies[0].value, "fake-sapisid");
}
