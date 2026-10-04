//! The `ytmfast` command line.
//!
//! Hand-rolled parsing: four subcommands don't justify a CLI-parser dependency.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use ytmfast::auth::{KeyringStore, SessionStore, chromium};
use ytmfast::error::Error;

const USAGE: &str = "\
usage: ytmfast <command>

commands:
  daemon            run the engine and its control socket
  import-session [--profile PATH]
                    store a YouTube Music session in the login keyring, read from the
                    pear-desktop profile (default: ~/.config/YouTube Music)
  play <videoId>    play one song to the default output and exit (debug helper)

options:
  -h, --help        show this help
  -V, --version     show the version";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Daemon,
    /// The profile folder, when `--profile` gave one.
    ImportSession(Option<PathBuf>),
    Play(String),
    Version,
    Help,
    /// Anything we don't understand: print usage and exit 2.
    Usage,
}

fn parse(args: impl IntoIterator<Item = String>) -> Command {
    let args: Vec<String> = args.into_iter().collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["daemon"] => Command::Daemon,
        ["import-session"] => Command::ImportSession(None),
        ["import-session", "--profile", path] => Command::ImportSession(Some(path.into())),
        ["play", id] => Command::Play((*id).to_string()),
        ["-V" | "--version"] => Command::Version,
        ["-h" | "--help"] => Command::Help,
        _ => Command::Usage,
    }
}

/// Subcommands later steps wire up: each task wires the part it owns.
fn not_built_yet(name: &str) -> ExitCode {
    eprintln!("ytmfast: {name} is not built yet");
    ExitCode::from(1)
}

/// The `pear-desktop` profile: Electron keeps an app's data in
/// `$XDG_CONFIG_HOME/<app name>`, else `~/.config/<app name>`, and the app is named
/// "YouTube Music".
fn default_profile_in(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let config = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("YouTube Music"))
}

/// Imports the session from the profile and saves it in the login keyring. Prints only the
/// cookie count: never a value.
fn import_session(profile: Option<PathBuf>) -> ExitCode {
    let Some(profile) = profile.or_else(|| default_profile_in(&|k| std::env::var_os(k))) else {
        eprintln!("ytmfast: no home folder; pass --profile PATH");
        return ExitCode::from(1);
    };
    let session = match chromium::import(&profile) {
        Ok(s) => s,
        Err(Error::SignedOut) => {
            eprintln!("ytmfast: no YouTube sign-in in that profile; sign in to the app first");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("ytmfast: {e}");
            return ExitCode::from(1);
        }
    };
    // One small runtime for the one keyring call; the daemon builds its own.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => {
            eprintln!("ytmfast: could not start the async runtime");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(KeyringStore::new().save(&session)) {
        Ok(()) => {
            println!("Imported {} cookies", session.cookies.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ytmfast: {e}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)) {
        Command::Daemon => not_built_yet("daemon"),
        Command::ImportSession(profile) => import_session(profile),
        Command::Play(_video_id) => not_built_yet("play"),
        Command::Version => {
            println!("ytmfast {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Usage => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Command {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn cli_parses_subcommands() {
        assert_eq!(p(&["daemon"]), Command::Daemon);
        assert_eq!(p(&["import-session"]), Command::ImportSession(None));
        assert_eq!(
            p(&["import-session", "--profile", "/x/YouTube Music"]),
            Command::ImportSession(Some("/x/YouTube Music".into()))
        );
        assert_eq!(
            p(&["play", "dQw4w9WgXcQ"]),
            Command::Play("dQw4w9WgXcQ".into())
        );
        assert_eq!(p(&["--version"]), Command::Version);
        assert_eq!(p(&["-V"]), Command::Version);
        assert_eq!(p(&["--help"]), Command::Help);
        assert_eq!(p(&["-h"]), Command::Help);
    }

    #[test]
    fn cli_rejects_bad_usage() {
        assert_eq!(p(&[]), Command::Usage);
        assert_eq!(p(&["frobnicate"]), Command::Usage);
        assert_eq!(p(&["play"]), Command::Usage);
        assert_eq!(p(&["play", "a", "b"]), Command::Usage);
        assert_eq!(p(&["daemon", "extra"]), Command::Usage);
        assert_eq!(p(&["import-session", "--profile"]), Command::Usage);
        assert_eq!(p(&["import-session", "/x"]), Command::Usage);
    }

    #[test]
    fn default_profile_follows_electron() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            default_profile_in(&env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/YouTube Music"))
        );
        assert_eq!(
            default_profile_in(&env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/c")])),
            Some(PathBuf::from("/c/YouTube Music"))
        );
        // A relative XDG value is invalid by the spec and ignored.
        assert_eq!(
            default_profile_in(&env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "rel")])),
            Some(PathBuf::from("/h/.config/YouTube Music"))
        );
        assert_eq!(default_profile_in(&env(&[])), None);
    }
}
