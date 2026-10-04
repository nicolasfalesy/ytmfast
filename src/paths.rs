//! Private folders for state, cache and the socket.
//!
//! Each folder is created 0700 and tightened to 0700 if something made it wider: the state
//! folder holds the database and the runtime folder holds the control socket, and neither
//! should be readable by other users.
//!
//! The public functions read the real environment. The `*_in` variants take the
//! environment as a function so tests never have to mutate process env (which would race
//! between test threads).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

const APP: &str = "ytmfast";

type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn real_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// `$XDG_STATE_HOME/ytmfast`, else `~/.local/state/ytmfast`.
pub fn state_dir() -> io::Result<PathBuf> {
    state_dir_in(&real_env)
}

/// `$XDG_CACHE_HOME/ytmfast`, else `~/.cache/ytmfast`.
pub fn cache_dir() -> io::Result<PathBuf> {
    cache_dir_in(&real_env)
}

/// `$XDG_RUNTIME_DIR/ytmfast`, else `/run/user/$UID/ytmfast`.
pub fn runtime_dir() -> io::Result<PathBuf> {
    runtime_dir_in(&real_env)
}

fn state_dir_in(env: Env) -> io::Result<PathBuf> {
    let base = match xdg(env, "XDG_STATE_HOME") {
        Some(p) => p,
        None => home(env)?.join(".local/state"),
    };
    private_dir(&base.join(APP))
}

fn cache_dir_in(env: Env) -> io::Result<PathBuf> {
    let base = match xdg(env, "XDG_CACHE_HOME") {
        Some(p) => p,
        None => home(env)?.join(".cache"),
    };
    private_dir(&base.join(APP))
}

fn runtime_dir_in(env: Env) -> io::Result<PathBuf> {
    private_dir(&runtime_base(env).join(APP))
}

fn runtime_base(env: Env) -> PathBuf {
    xdg(env, "XDG_RUNTIME_DIR").unwrap_or_else(|| {
        // SAFETY: getuid has no preconditions and can't fail.
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/run/user/{uid}"))
    })
}

/// An XDG base variable, if set to an absolute path. The XDG spec says an empty or
/// relative value is invalid and must be ignored.
fn xdg(env: Env, key: &str) -> Option<PathBuf> {
    env(key).map(PathBuf::from).filter(|p| p.is_absolute())
}

fn home(env: Env) -> io::Result<PathBuf> {
    xdg(env, "HOME").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

/// Creates `dir` (and any missing parents) with mode 0700, or checks an existing one and
/// tightens it to 0700. Refuses a symlink or a non-folder at `dir`, and a folder owned by
/// another user, rather than trusting or chmod-ing something it didn't make.
fn private_dir(dir: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // symlink_metadata, not metadata: `create` succeeds on a symlink to a folder, and we
    // must see the link itself to refuse it.
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a plain folder", dir.display()),
        ));
    }
    // SAFETY: getuid has no preconditions and can't fail.
    if meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by another user", dir.display()),
        ));
    }
    if meta.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// A fake environment holding only the given variables.
    fn env_of(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
            .collect();
        move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn paths_created_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[("XDG_STATE_HOME", tmp.path())]);
        let dir = state_dir_in(&env).unwrap();
        assert_eq!(dir, tmp.path().join("ytmfast"));
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn paths_tighten_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ytmfast");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = env_of(&[("XDG_STATE_HOME", tmp.path())]);
        assert_eq!(state_dir_in(&env).unwrap(), dir);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn paths_home_fallbacks() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[("HOME", tmp.path())]);
        let state = state_dir_in(&env).unwrap();
        let cache = cache_dir_in(&env).unwrap();
        assert_eq!(state, tmp.path().join(".local/state/ytmfast"));
        assert_eq!(cache, tmp.path().join(".cache/ytmfast"));
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&cache), 0o700);
    }

    #[test]
    fn paths_cache_and_runtime_honour_xdg() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path().join("c");
        let r = tmp.path().join("r");
        let env = env_of(&[("XDG_CACHE_HOME", &c), ("XDG_RUNTIME_DIR", &r)]);
        assert_eq!(cache_dir_in(&env).unwrap(), c.join("ytmfast"));
        assert_eq!(runtime_dir_in(&env).unwrap(), r.join("ytmfast"));
        assert_eq!(mode(&r.join("ytmfast")), 0o700);
    }

    #[test]
    fn paths_ignore_relative_xdg() {
        // The XDG spec says a relative path in these variables is invalid and must be
        // ignored; otherwise the folder would land wherever the process was started.
        let tmp = tempfile::tempdir().unwrap();
        let env = {
            let home = tmp.path().as_os_str().to_owned();
            move |k: &str| match k {
                "XDG_STATE_HOME" => Some(OsString::from("relative/state")),
                "XDG_CACHE_HOME" => Some(OsString::new()),
                "HOME" => Some(home.clone()),
                _ => None,
            }
        };
        assert_eq!(
            state_dir_in(&env).unwrap(),
            tmp.path().join(".local/state/ytmfast")
        );
        assert_eq!(
            cache_dir_in(&env).unwrap(),
            tmp.path().join(".cache/ytmfast")
        );
    }

    #[test]
    fn paths_runtime_fallback_is_run_user_uid() {
        // SAFETY: getuid has no preconditions and can't fail.
        let uid = unsafe { libc::getuid() };
        assert_eq!(
            runtime_base(&|_| None),
            PathBuf::from(format!("/run/user/{uid}"))
        );
    }

    #[test]
    fn paths_no_home_is_an_error() {
        assert!(state_dir_in(&|_| None).is_err());
    }

    #[test]
    fn paths_refuse_symlink() {
        // A symlink planted at the app folder could point the socket or database at a
        // folder someone else controls; refuse it rather than chmod its target.
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, tmp.path().join("ytmfast")).unwrap();
        let env = env_of(&[("XDG_STATE_HOME", tmp.path())]);
        assert!(state_dir_in(&env).is_err());
        assert_eq!(
            mode(&elsewhere),
            0o755,
            "the symlink target must be left alone"
        );
    }

    #[test]
    fn paths_refuse_plain_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ytmfast"), b"").unwrap();
        let env = env_of(&[("XDG_STATE_HOME", tmp.path())]);
        assert!(state_dir_in(&env).is_err());
    }
}
