//! `KeyringStore` with no Secret Service reachable: it must fail with the documented error and
//! never fall back to a file.
//!
//! This is its own test binary (its own process) because it points the D-Bus session address
//! at nothing, and changing the environment inside the shared unit-test process would race
//! the other tests. Nothing here can reach the real keyring: both ways zbus finds the session
//! bus (`DBUS_SESSION_BUS_ADDRESS`, then `$XDG_RUNTIME_DIR/bus`) point into an empty temp folder.

use ytmfast::auth::{KeyringStore, Session, SessionStore};
use ytmfast::error::Error;

#[tokio::test]
async fn keyring_unavailable_is_internal_and_writes_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let bogus = format!("unix:path={}/no-bus", dir.path().display());
    // SAFETY: this binary has exactly one test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &bogus);
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        std::env::set_var("HOME", dir.path());
        std::env::set_var("XDG_DATA_HOME", dir.path());
        std::env::set_var("XDG_STATE_HOME", dir.path());
    }
    let store = KeyringStore::new();
    let want = Err(Error::Internal("keyring locked or unavailable".into()));
    assert_eq!(store.load().await, want);
    assert_eq!(store.save(&Session::default()).await, want.map(|_| ()));
    // No fallback file anywhere under the (fake) home.
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(left.is_empty(), "files appeared: {left:?}");
}
