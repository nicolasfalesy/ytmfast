//! Imports the session from a Chromium profile: the `pear-desktop` app's (Electron), or the
//! Brave Origin browser's.
//!
//! Chromium keeps cookies in an SQLite database. Each value is either plain text in `value`
//! (with `encrypted_value` empty) or encrypted in `encrypted_value`:
//! - `v10`: AES-128-CBC with a key every Linux Chromium shares when no keyring is in use
//!   (PBKDF2-HMAC-SHA1 of "peanuts", salt "saltysalt", 1 round, 16 bytes; IV of 16 spaces).
//! - `v11`: the same, with the browser's "Safe Storage" password from the user's keyring in
//!   place of "peanuts" (`KeySource`). Read for Brave Origin only; the `pear-desktop` import
//!   still refuses it with a clear error.
//!
//! From database version 24 the plaintext starts with SHA-256(host_key). It is checked and
//! dropped: Chromium checks it too, and it is what proves a keyring key right (a wrong key
//! gives valid padding 1 time in 256).

use std::fs;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::{Cookie, Session};
use crate::error::Error;

/// Where the `v11` key comes from: the browser's "Safe Storage" password in the user's
/// keyring. A trait so the tests hand in a fake and never reach the user's keyring.
pub trait KeySource {
    /// Every password that may be the key, best first (the keyring can hold more than one
    /// matching item). Asked at most once per import, and only when a kept cookie is `v11`,
    /// so a profile that needs no key never shows an unlock prompt. Empty when the keyring
    /// holds none; an `Err` is the keyring's own failure, passed on as it is.
    ///
    /// `oo7::Secret`, not bytes: it wipes its memory when dropped.
    fn passwords(&self) -> Result<Vec<oo7::Secret>, Error>;
}

/// The fixed texts of the Brave Origin import. The bar widget shows them as they are, so
/// they say what to do; none carries a path, a cookie or a key.
pub const NO_BRAVE_PROFILE: &str =
    "no Brave Origin profile; open Brave Origin once, or pass --profile <folder>";
pub const NO_BRAVE_KEY: &str =
    "Brave Origin's cookie key (\"Brave Safe Storage\") is not in the keyring";
pub const BRAVE_KEY_WRONG: &str =
    "the keyring's \"Brave Safe Storage\" key does not open Brave Origin's cookies";

/// The only cookie hosts kept: the YouTube and Google sign-in cookies the API needs.
/// Everything else in the profile (other Google country domains, any other site) stays out
/// of the keyring.
const KEPT_HOSTS: &[&str] = &[".youtube.com", ".google.com", "accounts.google.com"];

/// Where the database sits inside a profile: `Network/Cookies` since Chromium 96, the
/// profile root before that; `Default/` when the app uses Chromium's profile folders.
const DB_CANDIDATES: &[&str] = &[
    "Network/Cookies",
    "Cookies",
    "Default/Network/Cookies",
    "Default/Cookies",
];

/// A real cookie database is well under 1 MiB; this only stops a wrong path from reading a
/// huge file into memory.
const MAX_DB_BYTES: u64 = 64 * 1024 * 1024;

/// Seconds from 1601-01-01 (Chromium's epoch) to 1970-01-01 (Unix's).
const CHROMIUM_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

/// Reads the YouTube and Google cookies out of the Chromium profile folder `profile`.
///
/// Refuses while the `pear-desktop` app is running: it rewrites the database (and rotates
/// cookies) while it runs, so a copy taken then can be half-written or soon stale.
pub fn import(profile: &Path) -> Result<Session, Error> {
    import_with(profile, Path::new("/proc"))
}

/// Reads the YouTube and Google cookies out of the Brave Origin profile folder `profile`
/// (`Default`), with `keys` for the `v11` ones.
///
/// Brave Origin may be running: it holds the database open, so it is copied first (see
/// `open_copy`), as for `pear-desktop`. Unlike `pear-desktop` that is no reason to refuse:
/// the user browses with it, and Chromium writes its cookie changes in short transactions
/// (batched, about every 30 seconds), so a copy is whole; the sign-in cookies the import
/// needs change rarely, so it is not stale either.
pub fn import_brave_origin(profile: &Path, keys: &dyn KeySource) -> Result<Session, Error> {
    let db_path = find_database(profile).ok_or_else(|| Error::Internal(NO_BRAVE_PROFILE.into()))?;
    read_session(&db_path, Keys::new(Some(keys)))
}

/// `import` with the `/proc` root injectable, so tests don't depend on what runs on the box.
fn import_with(profile: &Path, proc_root: &Path) -> Result<Session, Error> {
    if pear_desktop_running(proc_root, profile) {
        return Err(Error::Internal(
            "pear-desktop is running; quit it, then import again".into(),
        ));
    }
    let db_path = find_database(profile).ok_or_else(|| {
        Error::Internal("no Cookies database in the profile folder; pass --profile <folder>".into())
    })?;
    read_session(&db_path, Keys::new(None))
}

/// The profile's cookie database, wherever this Chromium keeps it.
fn find_database(profile: &Path) -> Option<std::path::PathBuf> {
    DB_CANDIDATES
        .iter()
        .map(|c| profile.join(c))
        .find(|p| p.is_file())
}

/// The kept cookies of the database at `db_path`.
fn read_session(db_path: &Path, mut keys: Keys) -> Result<Session, Error> {
    let db = open_copy(db_path)?;
    let version = meta_version(&db)?;

    let mut stmt = db
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure \
             FROM cookies ORDER BY rowid",
        )
        .map_err(unreadable)?;
    let mut rows = stmt.query([]).map_err(unreadable)?;
    let mut cookies = Vec::new();
    while let Some(row) = rows.next().map_err(unreadable)? {
        let host: String = row.get(0).map_err(unreadable)?;
        if !KEPT_HOSTS.contains(&host.as_str()) {
            // Dropped before decrypting, so a cookie we don't keep can't fail the import.
            continue;
        }
        let encrypted: Vec<u8> = row.get(3).map_err(unreadable)?;
        let value = if encrypted.is_empty() {
            row.get(2).map_err(unreadable)?
        } else {
            keys.decrypt(&encrypted, &host, version)?
        };
        let expires: i64 = row.get(5).map_err(unreadable)?;
        cookies.push(Cookie {
            domain: host,
            name: row.get(1).map_err(unreadable)?,
            value,
            path: row.get(4).map_err(unreadable)?,
            secure: row.get(6).map_err(unreadable)?,
            // Stored as Unix seconds: Chromium's `expires_utc` is microseconds since
            // 1601-01-01, and 0 means "no expiry" (a session cookie), kept as None.
            expires_utc: (expires != 0).then(|| expires / 1_000_000 - CHROMIUM_EPOCH_OFFSET_SECS),
        });
    }
    if cookies.is_empty() {
        // No YouTube or Google cookies at all: that profile was never signed in.
        return Err(Error::SignedOut);
    }
    Ok(Session {
        cookies,
        account: None,
    })
}

/// One fixed message for any database failure: rusqlite's text adds nothing the user can act on.
fn unreadable<E>(_: E) -> Error {
    Error::Internal("could not read the Cookies database".into())
}

/// Opens an in-memory, read-only copy of the database.
///
/// A copy, because Chromium may hold a lock on the file; in memory, not a temp file, so the
/// cookie values are never written anywhere else on disk.
fn open_copy(path: &Path) -> Result<Connection, Error> {
    let size = fs::metadata(path).map_err(unreadable)?.len();
    if size > MAX_DB_BYTES {
        return Err(Error::Internal("the Cookies database is too large".into()));
    }
    let mut bytes = fs::read(path).map_err(unreadable)?;
    // Header bytes 18 and 19 are the file-format write and read versions: 2 means WAL. The
    // in-memory database can't do WAL and refuses to read such a file, so mark it as a
    // rollback-journal database (1). The content is the same either way: a cleanly closed
    // database has its WAL checkpointed into the main file.
    if bytes.len() >= 100 && bytes.starts_with(b"SQLite format 3\0") {
        for b in &mut bytes[18..20] {
            if *b == 2 {
                *b = 1;
            }
        }
    }
    let mut db = Connection::open_in_memory().map_err(unreadable)?;
    db.deserialize_read_exact("main", Cursor::new(&bytes), bytes.len(), true)
        .map_err(unreadable)?;
    Ok(db)
}

/// The `meta` table's schema version; 0 when it is missing (very old databases).
fn meta_version(db: &Connection) -> Result<i64, Error> {
    let value: Option<String> =
        match db.query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| {
            r.get(0)
        }) {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(unreadable(e)),
        };
    Ok(value.and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// The fixed `v10` key (see the module docs).
fn v10_key() -> [u8; 16] {
    derive_key(b"peanuts")
}

/// A key from a password: PBKDF2-HMAC-SHA1, salt "saltysalt", 1 round, 16 bytes (Chromium's
/// `os_crypt` on Linux, for both `v10` and `v11`).
fn derive_key(password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key);
    key
}

/// The keys this import may need, each found the first time a cookie needs it.
struct Keys<'a> {
    v10: Option<[u8; 16]>,
    /// `None` for an import that doesn't read `v11` (`pear-desktop`).
    source: Option<&'a dyn KeySource>,
    /// The keys from `source`'s passwords once asked; the last one that worked comes first.
    v11: Option<Vec<[u8; 16]>>,
}

impl<'a> Keys<'a> {
    fn new(source: Option<&'a dyn KeySource>) -> Self {
        Keys {
            v10: None,
            source,
            v11: None,
        }
    }

    /// The value of one encrypted cookie of `host`.
    fn decrypt(
        &mut self,
        encrypted: &[u8],
        host: &str,
        meta_version: i64,
    ) -> Result<String, Error> {
        match encrypted.split_at_checked(3) {
            Some((b"v10", rest)) => {
                let key = self.v10.get_or_insert_with(v10_key);
                let plain = open(rest, key, host, meta_version)
                    .ok_or_else(|| Error::Internal("could not decrypt a v10 cookie".into()))?;
                String::from_utf8(plain)
                    .map_err(|_| Error::Internal("a decrypted cookie is not text".into()))
            }
            Some((b"v11", rest)) => self.decrypt_v11(rest, host, meta_version),
            _ => Err(Error::Internal(
                "a cookie uses an unknown encryption format".into(),
            )),
        }
    }

    fn decrypt_v11(
        &mut self,
        ciphertext: &[u8],
        host: &str,
        meta_version: i64,
    ) -> Result<String, Error> {
        let Some(source) = self.source else {
            return Err(Error::Internal(
                "the cookies are encrypted with a keyring key (v11), which ytmfast can't read yet"
                    .into(),
            ));
        };
        let keys = match &mut self.v11 {
            Some(keys) => keys,
            None => {
                let keys = source
                    .passwords()?
                    .iter()
                    .map(|p| derive_key(p.as_bytes()))
                    .collect();
                self.v11.insert(keys)
            }
        };
        if keys.is_empty() {
            return Err(Error::Internal(NO_BRAVE_KEY.into()));
        }
        // Each key in turn (two keyring items can match). The one that works moves to the
        // front, so the other cookies try it first.
        for i in 0..keys.len() {
            if let Some(value) = open(ciphertext, &keys[i], host, meta_version)
                .and_then(|p| String::from_utf8(p).ok())
            {
                keys.swap(0, i);
                return Ok(value);
            }
        }
        Err(Error::Internal(BRAVE_KEY_WRONG.into()))
    }
}

/// Decrypts one value (without its version tag) with `key`, checking and dropping the host
/// hash of databases from version 24. `None` for a wrong key, a damaged value, or a value
/// that belongs to another host.
fn open(ciphertext: &[u8], key: &[u8; 16], host: &str, meta_version: i64) -> Option<Vec<u8>> {
    let mut plain = cbc::Decryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .ok()?;
    if meta_version >= 24 {
        // The SHA-256 of the host the cookie belongs to, which Chromium prepends so a value
        // can't be moved to another host's row. Checked, as Chromium does: it is also what
        // tells a wrong keyring key from the right one.
        if plain.get(..32)? != Sha256::digest(host.as_bytes()).as_slice() {
            return None;
        }
        plain.drain(..32);
    }
    Some(plain)
}

/// True when a process under `proc_root` looks like the `pear-desktop` main process.
///
/// If `/proc` can't be read at all the answer is false: the check is a guard against a
/// half-written copy, not a security boundary.
fn pear_desktop_running(proc_root: &Path, profile: &Path) -> bool {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().as_bytes().iter().all(u8::is_ascii_digit))
        .filter_map(|e| fs::read(e.path().join("cmdline")).ok())
        .any(|cmdline| is_pear_main(&cmdline, profile.as_os_str().as_bytes()))
}

/// The main process of an Electron app is `electron [flags] <path>/app.asar`; its renderers
/// and helpers carry `--type=…` and no `app.asar`. Matching the app's install folder (the
/// current `pear-desktop` name and the older `youtube-music` one), or the profile folder
/// passed as `--user-data-dir`, keeps other Electron apps from blocking the import.
fn is_pear_main(cmdline: &[u8], profile: &[u8]) -> bool {
    let args: Vec<String> = cmdline
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).to_ascii_lowercase())
        .collect();
    let mut asars = args.iter().filter(|a| a.ends_with("app.asar")).peekable();
    if asars.peek().is_none() {
        return false;
    }
    let profile = String::from_utf8_lossy(profile).to_ascii_lowercase();
    asars.any(|a| {
        a.contains("pear-desktop") || a.contains("youtube-music") || a.contains("youtube music")
    }) || (!profile.is_empty() && args.iter().any(|a| a.contains(&profile)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    use rusqlite::{Connection, params};
    use std::path::PathBuf;

    /// Chromium time for a Unix time: microseconds since 1601-01-01.
    fn chromium_time(unix: i64) -> i64 {
        (unix + 11_644_473_600) * 1_000_000
    }

    struct Row<'a> {
        host: &'a str,
        name: &'a str,
        value: &'a str,
        encrypted: Vec<u8>,
        expires: i64,
        secure: bool,
    }

    fn plain<'a>(host: &'a str, name: &'a str, value: &'a str) -> Row<'a> {
        Row {
            host,
            name,
            value,
            encrypted: Vec::new(),
            expires: chromium_time(1_900_000_000),
            secure: true,
        }
    }

    /// A profile folder with a Chromium-shaped `Network/Cookies` database (the columns that
    /// matter of the real schema) at meta `version`.
    fn profile(dir: &Path, version: i64, rows: &[Row]) -> PathBuf {
        let net = dir.join("Network");
        std::fs::create_dir_all(&net).unwrap();
        let db = Connection::open(net.join("Cookies")).unwrap();
        db.execute_batch(
            "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
             CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL,
               top_frame_site_key TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
               encrypted_value BLOB NOT NULL, path TEXT NOT NULL, expires_utc INTEGER NOT NULL,
               is_secure INTEGER NOT NULL, is_httponly INTEGER NOT NULL);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO meta VALUES ('version', ?1)",
            params![version.to_string()],
        )
        .unwrap();
        for r in rows {
            db.execute(
                "INSERT INTO cookies VALUES (0, ?1, '', ?2, ?3, ?4, '/', ?5, ?6, 1)",
                params![r.host, r.name, r.value, r.encrypted, r.expires, r.secure],
            )
            .unwrap();
        }
        dir.to_path_buf()
    }

    /// An empty fake `/proc`.
    fn no_procs(dir: &Path) -> PathBuf {
        let p = dir.join("proc");
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fake_proc(proc_root: &Path, pid: u32, args: &[&str]) {
        let d = proc_root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        let mut cmdline = args.join("\0");
        cmdline.push('\0');
        std::fs::write(d.join("cmdline"), cmdline).unwrap();
    }

    fn v10_encrypt(host: &str, value: &str, version: i64) -> Vec<u8> {
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"peanuts", b"saltysalt", 1, &mut key);
        let mut plaintext = Vec::new();
        if version >= 24 {
            plaintext.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        plaintext.extend_from_slice(value.as_bytes());
        let ct = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &[b' '; 16].into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
        let mut out = b"v10".to_vec();
        out.extend_from_slice(&ct);
        out
    }

    /// Chromium's `v11` encryption with the keyring password `password` (the same AES and
    /// PBKDF2 as v10, with the password in place of "peanuts").
    fn v11_encrypt(password: &[u8], host: &str, value: &str, version: i64) -> Vec<u8> {
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key);
        let mut plaintext = Vec::new();
        if version >= 24 {
            plaintext.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        plaintext.extend_from_slice(value.as_bytes());
        let ct = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &[b' '; 16].into())
            .encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
        let mut out = b"v11".to_vec();
        out.extend_from_slice(&ct);
        out
    }

    /// A fake keyring: hands out fixed passwords (or an error) and counts the asks.
    struct FakeKeys {
        answer: Result<Vec<&'static [u8]>, Error>,
        asked: std::cell::Cell<u32>,
    }

    impl FakeKeys {
        fn new(passwords: &[&'static [u8]]) -> Self {
            FakeKeys {
                answer: Ok(passwords.to_vec()),
                asked: std::cell::Cell::new(0),
            }
        }
    }

    impl KeySource for FakeKeys {
        fn passwords(&self) -> Result<Vec<oo7::Secret>, Error> {
            self.asked.set(self.asked.get() + 1);
            self.answer
                .clone()
                .map(|list| list.into_iter().map(oo7::Secret::from).collect())
        }
    }

    const PASSWORD: &[u8] = b"fake-safe-storage-password";

    fn v11_row<'a>(host: &'a str, name: &'a str, value: &str, version: i64) -> Row<'a> {
        let mut r = plain(host, name, "");
        r.encrypted = v11_encrypt(PASSWORD, host, value, version);
        r
    }

    #[test]
    fn brave_v11_cookies_open_with_the_keyring_key() {
        let dir = tempfile::tempdir().unwrap();
        for version in [23, 24] {
            let d = dir.path().join(version.to_string());
            let mut v10 = plain(".google.com", "NID", "");
            v10.encrypted = v10_encrypt(".google.com", "v10-value", version);
            let p = profile(
                &d,
                version,
                &[
                    v11_row(".youtube.com", "SAPISID", "v11-value", version),
                    v10,
                    plain("accounts.google.com", "LSID", "plain-value"),
                    // Dropped hosts are never decrypted, whatever their key.
                    v11_row(".google.ca", "SID", "x", version),
                ],
            );
            let keys = FakeKeys::new(&[PASSWORD]);
            let s = import_brave_origin(&p, &keys).unwrap();
            let got: Vec<(&str, &str)> = s
                .cookies
                .iter()
                .map(|c| (c.name.as_str(), c.value.as_str()))
                .collect();
            assert_eq!(
                got,
                [
                    ("SAPISID", "v11-value"),
                    ("NID", "v10-value"),
                    ("LSID", "plain-value")
                ],
                "meta version {version}"
            );
            assert_eq!(keys.asked.get(), 1, "the keyring is asked once");
        }
    }

    #[test]
    fn brave_keyring_is_not_asked_without_v11_cookies() {
        // No unlock prompt for a profile that doesn't need the key.
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".youtube.com", "SAPISID", "abc")]);
        let keys = FakeKeys::new(&[]);
        assert_eq!(import_brave_origin(&p, &keys).unwrap().cookies.len(), 1);
        assert_eq!(keys.asked.get(), 0);
    }

    #[test]
    fn brave_tries_each_matching_key() {
        // Two keyring items can match: the one that opens the cookies is used.
        let dir = tempfile::tempdir().unwrap();
        let p = profile(
            dir.path(),
            24,
            &[
                v11_row(".youtube.com", "SAPISID", "one", 24),
                v11_row(".youtube.com", "SID", "two", 24),
            ],
        );
        let keys = FakeKeys::new(&[b"wrong-password", PASSWORD]);
        let s = import_brave_origin(&p, &keys).unwrap();
        let values: Vec<&str> = s.cookies.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["one", "two"]);
    }

    #[test]
    fn brave_wrong_or_missing_key_is_a_clear_error_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(
            dir.path(),
            24,
            &[v11_row(
                ".youtube.com",
                "SAPISID",
                "secret-cookie-value",
                24,
            )],
        );
        let err = import_brave_origin(&p, &FakeKeys::new(&[b"wrong-password"])).unwrap_err();
        assert_eq!(err, Error::Internal(BRAVE_KEY_WRONG.into()));
        assert!(!format!("{err} {err:?}").contains("secret-cookie-value"));
        let err = import_brave_origin(&p, &FakeKeys::new(&[])).unwrap_err();
        assert_eq!(err, Error::Internal(NO_BRAVE_KEY.into()));
        // The keyring's own failure (locked, a dismissed prompt, no Secret Service) is passed
        // on as it is.
        let locked = FakeKeys {
            answer: Err(Error::Internal("keyring locked or unavailable".into())),
            asked: std::cell::Cell::new(0),
        };
        assert_eq!(
            import_brave_origin(&p, &locked).unwrap_err(),
            Error::Internal("keyring locked or unavailable".into())
        );
    }

    #[test]
    fn a_cookie_moved_to_another_host_is_refused() {
        // From version 24 the value starts with the SHA-256 of its own host. A wrong key that
        // happens to give valid padding (1 in 256) fails this too, which is what tells two
        // keyring keys apart.
        let dir = tempfile::tempdir().unwrap();
        let mut r = plain(".youtube.com", "SAPISID", "");
        r.encrypted = v11_encrypt(PASSWORD, ".google.com", "moved", 24);
        let mut v10 = plain(".youtube.com", "SID", "");
        v10.encrypted = v10_encrypt(".google.com", "moved", 24);
        let p = profile(dir.path(), 24, &[r]);
        assert_eq!(
            import_brave_origin(&p, &FakeKeys::new(&[PASSWORD])).unwrap_err(),
            Error::Internal(BRAVE_KEY_WRONG.into())
        );
        let d = dir.path().join("v10");
        let p = profile(&d, 24, &[v10]);
        assert!(matches!(
            import_brave_origin(&p, &FakeKeys::new(&[])),
            Err(Error::Internal(_))
        ));
    }

    #[test]
    fn brave_reads_the_database_at_the_profile_root() {
        // Brave Origin keeps it at `Default/Cookies`, not under `Network/`.
        let dir = tempfile::tempdir().unwrap();
        let p = profile(
            dir.path(),
            24,
            &[v11_row(".youtube.com", "SAPISID", "v", 24)],
        );
        std::fs::rename(p.join("Network/Cookies"), p.join("Cookies")).unwrap();
        let s = import_brave_origin(&p, &FakeKeys::new(&[PASSWORD])).unwrap();
        assert_eq!(s.cookies[0].value, "v");
    }

    #[test]
    fn brave_without_a_profile_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        for p in [dir.path().to_path_buf(), dir.path().join("missing")] {
            assert_eq!(
                import_brave_origin(&p, &FakeKeys::new(&[])),
                Err(Error::Internal(NO_BRAVE_PROFILE.into()))
            );
        }
    }

    #[test]
    fn brave_with_no_youtube_cookies_is_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".google.ca", "D", "4")]);
        assert_eq!(
            import_brave_origin(&p, &FakeKeys::new(&[])),
            Err(Error::SignedOut)
        );
    }

    #[test]
    fn import_plain_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut session_cookie = plain(".google.com", "NID", "n");
        session_cookie.expires = 0;
        session_cookie.secure = false;
        let p = profile(
            dir.path(),
            24,
            &[plain(".youtube.com", "SAPISID", "abc"), session_cookie],
        );
        let s = import_with(&p, &no_procs(dir.path())).unwrap();
        assert_eq!(
            s.cookies,
            vec![
                Cookie {
                    domain: ".youtube.com".into(),
                    name: "SAPISID".into(),
                    value: "abc".into(),
                    path: "/".into(),
                    secure: true,
                    expires_utc: Some(1_900_000_000),
                },
                Cookie {
                    domain: ".google.com".into(),
                    name: "NID".into(),
                    value: "n".into(),
                    path: "/".into(),
                    secure: false,
                    expires_utc: None,
                },
            ]
        );
    }

    #[test]
    fn import_v10() {
        let dir = tempfile::tempdir().unwrap();
        for version in [23, 24] {
            let d = dir.path().join(version.to_string());
            let mut r = plain(".youtube.com", "SAPISID", "");
            r.encrypted = v10_encrypt(".youtube.com", "decrypted-value", version);
            let p = profile(&d, version, &[r]);
            let s = import_with(&p, &no_procs(&d)).unwrap();
            assert_eq!(s.cookies.len(), 1);
            assert_eq!(
                s.cookies[0].value, "decrypted-value",
                "meta version {version}"
            );
        }
    }

    #[test]
    fn import_v10_bad_ciphertext_is_an_error_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = plain(".youtube.com", "SAPISID", "");
        r.encrypted = b"v10not-a-block".to_vec();
        let p = profile(dir.path(), 24, &[r]);
        let err = import_with(&p, &no_procs(dir.path())).unwrap_err();
        assert!(matches!(err, Error::Internal(_)), "{err:?}");
    }

    #[test]
    fn import_v11_is_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = plain(".youtube.com", "SAPISID", "");
        r.encrypted = b"v11whatever-bytes".to_vec();
        let p = profile(dir.path(), 24, &[r]);
        match import_with(&p, &no_procs(dir.path())) {
            Err(Error::Internal(msg)) => assert!(msg.contains("v11"), "{msg}"),
            other => panic!("expected an Internal v11 error, got {other:?}"),
        }
    }

    #[test]
    fn import_v11_on_a_dropped_host_is_ignored() {
        // Only kept cookies are decrypted, so an unsupported one elsewhere doesn't block.
        let dir = tempfile::tempdir().unwrap();
        let mut other = plain(".google.ca", "SID", "");
        other.encrypted = b"v11whatever-bytes".to_vec();
        let p = profile(
            dir.path(),
            24,
            &[other, plain(".youtube.com", "SAPISID", "abc")],
        );
        let s = import_with(&p, &no_procs(dir.path())).unwrap();
        assert_eq!(s.cookies.len(), 1);
    }

    #[test]
    fn import_filters_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(
            dir.path(),
            24,
            &[
                plain(".youtube.com", "A", "1"),
                plain(".google.com", "B", "2"),
                plain("accounts.google.com", "C", "3"),
                plain(".google.ca", "D", "4"),
                plain("www.example.com", "E", "5"),
                plain(".evilyoutube.com", "F", "6"),
                plain("music.youtube.com", "G", "7"),
            ],
        );
        let s = import_with(&p, &no_procs(dir.path())).unwrap();
        let names: Vec<&str> = s.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "C"]);
    }

    #[test]
    fn import_with_no_kept_cookies_is_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".google.ca", "D", "4")]);
        assert_eq!(
            import_with(&p, &no_procs(dir.path())),
            Err(Error::SignedOut)
        );
    }

    #[test]
    fn import_finds_the_older_profile_layout() {
        // Older Electron builds keep the database at the profile root, not under Network/.
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".youtube.com", "A", "1")]);
        std::fs::rename(p.join("Network/Cookies"), p.join("Cookies")).unwrap();
        assert_eq!(
            import_with(&p, &no_procs(dir.path()))
                .unwrap()
                .cookies
                .len(),
            1
        );
    }

    #[test]
    fn import_reads_a_wal_mode_database() {
        // A database left in WAL mode can't be opened from memory as is; the import
        // rewrites the header to the rollback-journal mode first.
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".youtube.com", "A", "1")]);
        let db = Connection::open(p.join("Network/Cookies")).unwrap();
        let mode: String = db
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(db);
        assert_eq!(
            import_with(&p, &no_procs(dir.path()))
                .unwrap()
                .cookies
                .len(),
            1
        );
    }

    #[test]
    fn import_without_a_database_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = import_with(dir.path(), &no_procs(dir.path())).unwrap_err();
        match err {
            Error::Internal(msg) => assert!(msg.contains("Cookies"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn import_refuses_while_pear_desktop_runs() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".youtube.com", "A", "1")]);
        let procs = no_procs(dir.path());
        fake_proc(
            &procs,
            4242,
            &[
                "/usr/lib/electron42/electron",
                "--remote-debugging-pipe",
                "/usr/lib/pear-desktop/app.asar",
            ],
        );
        match import_with(&p, &procs) {
            Err(Error::Internal(msg)) => assert!(msg.contains("pear-desktop"), "{msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn other_electron_apps_do_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), 24, &[plain(".youtube.com", "A", "1")]);
        let procs = no_procs(dir.path());
        fake_proc(
            &procs,
            10,
            &["/usr/lib/electron/electron", "/usr/lib/obsidian/app.asar"],
        );
        // A renderer child of pear-desktop has no app.asar argument: it isn't the main process.
        fake_proc(
            &procs,
            11,
            &["/usr/lib/electron42/electron", "--type=renderer"],
        );
        // Non-process entries in /proc are skipped.
        std::fs::write(procs.join("uptime"), "1 2").unwrap();
        assert!(import_with(&p, &procs).is_ok());
    }
}
