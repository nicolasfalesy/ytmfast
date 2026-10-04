//! The `ytmfast` command line.
//!
//! Hand-rolled parsing: four subcommands don't justify a CLI-parser dependency.

use std::process::ExitCode;

const USAGE: &str = "\
usage: ytmfast <command>

commands:
  daemon            run the engine and its control socket
  import-session    store a YouTube Music session in the login keyring
  play <videoId>    play one song to the default output and exit (debug helper)

options:
  -h, --help        show this help
  -V, --version     show the version";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Daemon,
    ImportSession,
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
        ["import-session"] => Command::ImportSession,
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

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)) {
        Command::Daemon => not_built_yet("daemon"),
        Command::ImportSession => not_built_yet("import-session"),
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
        assert_eq!(p(&["import-session"]), Command::ImportSession);
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
    }
}
