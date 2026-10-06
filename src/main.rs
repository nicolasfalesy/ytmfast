//! The `ytmfast` command line.
//!
//! Hand-rolled parsing: four subcommands don't justify a CLI-parser dependency.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use ytmfast::audio::decode::loudness_gain;
use ytmfast::audio::fetch::{Relink, TrackBuffer};
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::pw::PipeWireSink;
use ytmfast::audio::sink::{NullSink, Sink};
use ytmfast::auth::{KeyringStore, Session, SessionStore, chromium, sidhash};
use ytmfast::control::{self, Exit, stop};
use ytmfast::engine::Engine;
use ytmfast::error::Error;
use ytmfast::innertube::{Innertube, clients};
use ytmfast::paths;
use ytmfast::report::Reporter;
use ytmfast::solver::Solver;
use ytmfast::state;
use ytmfast::streams::lazy::{self, LazySession};
use ytmfast::streams::ytdlp::{self, YtDlpCommand};
use ytmfast::streams::{Resolver, Streams, TrackMeta};

const USAGE: &str = "\
usage: ytmfast <command>

commands:
  daemon [--null-sink]
                    run the engine and its control socket (systemd starts it through
                    ytmfast.socket); --null-sink plays into nothing (benchmarks)
  import-session [--profile PATH]
                    store a YouTube Music session in the login keyring, read from the
                    pear-desktop profile (default: ~/.config/YouTube Music)
  play <videoId> [--null-sink] [--seconds N]
                    play one song to the default output and exit (debug helper);
                    --null-sink plays in real time into nothing (benchmarks),
                    --seconds stops after N seconds

options:
  -h, --help        show this help
  -V, --version     show the version";

#[derive(Debug, PartialEq)]
enum Command {
    /// `null_sink`: play into a real-time `NullSink` instead of PipeWire.
    Daemon {
        null_sink: bool,
    },
    /// The profile folder, when `--profile` gave one.
    ImportSession(Option<PathBuf>),
    Play(PlayArgs),
    Version,
    Help,
    /// Anything we don't understand: print usage and exit 2.
    Usage,
}

#[derive(Debug, PartialEq)]
struct PlayArgs {
    video_id: String,
    /// Play into a `NullSink` paced in real time instead of PipeWire.
    null_sink: bool,
    /// Stop after this long.
    seconds: Option<f64>,
}

fn parse(args: impl IntoIterator<Item = String>) -> Command {
    let args: Vec<String> = args.into_iter().collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["daemon"] => Command::Daemon { null_sink: false },
        ["daemon", "--null-sink"] => Command::Daemon { null_sink: true },
        ["import-session"] => Command::ImportSession(None),
        ["import-session", "--profile", path] => Command::ImportSession(Some(path.into())),
        ["play", rest @ ..] => parse_play(rest).map_or(Command::Usage, Command::Play),
        ["-V" | "--version"] => Command::Version,
        ["-h" | "--help"] => Command::Help,
        _ => Command::Usage,
    }
}

/// `play`'s arguments, in any order: the id once, each option at most once.
fn parse_play(args: &[&str]) -> Option<PlayArgs> {
    let mut video_id = None;
    let mut null_sink = false;
    let mut seconds = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "--null-sink" if !null_sink => null_sink = true,
            "--seconds" if seconds.is_none() => {
                let n: f64 = it.next()?.parse().ok()?;
                if !(n.is_finite() && n > 0.0) {
                    return None;
                }
                seconds = Some(n);
            }
            a if !a.starts_with('-') && video_id.is_none() => video_id = Some(a.to_string()),
            _ => return None,
        }
    }
    Some(PlayArgs {
        video_id: video_id?,
        null_sink,
        seconds,
    })
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

/// Imports the session from the profile and saves it in the login keyring, then stops a
/// running engine so it can't write its old session back over the new one. Prints only the
/// cookie count: never a value.
fn import_session(profile: Option<PathBuf>) -> ExitCode {
    let Some(profile) = profile.or_else(|| default_profile_in(&|k| std::env::var_os(k))) else {
        eprintln!("ytmfast: no home folder; pass --profile PATH");
        return ExitCode::from(1);
    };
    let session = match chromium::import(&profile) {
        Ok(s) => s,
        Err(Error::SignedOut) => {
            eprintln!("ytmfast: {NOT_SIGNED_IN}");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("ytmfast: {e}");
            return ExitCode::from(1);
        }
    };
    // Checked before saving: a profile with YouTube cookies but no sign-in (a visitor's
    // cookies only) would replace a working session with one that can't sign a request.
    if !has_sign_in(&session) {
        eprintln!("ytmfast: {NOT_SIGNED_IN}");
        return ExitCode::from(1);
    }
    // One small runtime for the keyring calls; the daemon builds its own.
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
    let store = KeyringStore::new();
    if let Err(e) = runtime.block_on(store.save(&session)) {
        eprintln!("ytmfast: {e}");
        return ExitCode::from(1);
    }
    println!("Imported {} cookies", session.cookies.len());
    // Stopped after the save, so a failed save never stops the music for nothing.
    match stop_running_engine() {
        EngineStop::NoneRunning => {
            println!("No engine was running");
            ExitCode::SUCCESS
        }
        EngineStop::Stopped => {
            // Saved again: a cookie rotation the old engine saved while it stopped could
            // otherwise be the last word in the keyring.
            if let Err(e) = runtime.block_on(store.save(&session)) {
                eprintln!("ytmfast: {e}");
                return ExitCode::from(1);
            }
            println!("Restarted the running engine: the next play starts it with the new session");
            ExitCode::SUCCESS
        }
        EngineStop::Unreachable => {
            eprintln!(
                "ytmfast: an engine is running but did not stop; stop it (systemctl --user stop ytmfast.service) and import again"
            );
            ExitCode::from(1)
        }
    }
}

const NOT_SIGNED_IN: &str = "no YouTube sign-in in that profile; sign in to the app first";

/// True when the session can sign API requests: it holds a SAPISID cookie for the origin the
/// `player` request is signed for. Without one every request would be `signed_out`.
fn has_sign_in(session: &Session) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    sidhash::authorization(session, clients::TV.origin, now).is_some()
}

/// What `import-session` did about a running engine.
#[derive(Debug, PartialEq)]
enum EngineStop {
    NoneRunning,
    Stopped,
    /// One is running but didn't answer `quit` or didn't exit in time.
    Unreachable,
}

/// Asks the user's running engine, if any, to quit, and waits for it to exit (`STOP_WAIT`
/// each for the reply and the exit). Never starts
/// one: the socket is only tried when an engine process exists (see `control::stop`).
fn stop_running_engine() -> EngineStop {
    let proc_root = std::path::Path::new("/proc");
    // SAFETY: getuid has no preconditions and can't fail.
    let uid = unsafe { libc::getuid() };
    let pids = stop::daemon_pids(proc_root, uid, std::process::id());
    if pids.is_empty() {
        return EngineStop::NoneRunning;
    }
    let Ok(dir) = paths::runtime_dir() else {
        return EngineStop::Unreachable;
    };
    let socket = dir.join(control::SOCKET_NAME);
    if stop::ask_to_quit(&socket, stop::STOP_WAIT)
        && stop::wait_gone(proc_root, &pids, stop::STOP_WAIT)
    {
        EngineStop::Stopped
    } else {
        EngineStop::Unreachable
    }
}

/// Plays one song to the end (or for `--seconds`). Prints the song's title and artist, and
/// errors; never a link (they carry access tokens).
fn play(args: PlayArgs) -> ExitCode {
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
    match runtime.block_on(play_track(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ytmfast: {message}");
            ExitCode::from(1)
        }
    }
}

/// An `Error` for the terminal: its text and its code.
fn describe(e: Error) -> String {
    format!("{e} [{}]", e.code())
}

/// The real resolver and queue source over the session in the login keyring. The keyring is
/// read on the first resolve or queue request, not here (`LazySession`): the daemon answers
/// its socket and signals at once, a missing session is reported per play (usually
/// `signed_out`, as an error event the widget shows) instead of the daemon refusing to start,
/// and a session imported later is picked up. A message when a folder is missing.
fn backend() -> Result<Arc<LazySession>, String> {
    let store: Arc<dyn SessionStore> = Arc::new(KeyringStore::new());
    let cache = paths::cache_dir().map_err(|_| "no cache folder".to_string())?;
    let runtime_dir = paths::runtime_dir().map_err(|_| "no runtime folder".to_string())?;
    let api_store = store.clone();
    let build: lazy::Build = Box::new(move |session| {
        let session = Arc::new(Mutex::new(session));
        let api = Arc::new(Innertube::production(session.clone(), api_store.clone()));
        let resolver = Arc::new(Streams::new(
            api.clone(),
            session,
            Arc::new(Solver::new(cache.clone())),
            Arc::new(YtDlpCommand::new(runtime_dir.clone())),
            cache.clone(),
        ));
        // One `Innertube` (so one session) for links, queues and play reports.
        lazy::Loaded {
            resolver: resolver.clone(),
            queue: api,
            reports: resolver,
        }
    });
    Ok(Arc::new(LazySession::new(store, build)))
}

/// Runs the engine behind the control socket until `quit` or idle.
fn daemon(null_sink: bool) -> ExitCode {
    // First, while this is the only thread: taking systemd's socket clears the LISTEN_*
    // variables, and changing the environment is only sound before other threads exist.
    // SAFETY: `main` calls this before anything has started a thread.
    let passed = match unsafe { control::take_systemd_listener() } {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ytmfast: {e}");
            return ExitCode::from(1);
        }
    };
    // Not started by systemd: bind the socket ourselves (refused if a daemon answers there).
    let (listener, bound) = match passed {
        Some(l) => (l, None),
        None => {
            let bound = paths::runtime_dir()
                .and_then(|dir| control::bind_socket(&dir.join(control::SOCKET_NAME)));
            match bound {
                Ok((l, b)) => (l, Some(b)),
                Err(e) => {
                    eprintln!("ytmfast: {e}");
                    return ExitCode::from(1);
                }
            }
        }
    };
    let code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => match runtime.block_on(serve(listener, null_sink)) {
            Ok(Exit::Idle) => {
                eprintln!("ytmfast: nothing played for a while; quitting");
                ExitCode::SUCCESS
            }
            Ok(Exit::Quit) => ExitCode::SUCCESS,
            Ok(Exit::Signal) => {
                eprintln!("ytmfast: stopping on a signal");
                ExitCode::SUCCESS
            }
            Ok(Exit::EngineGone) => {
                eprintln!("ytmfast: the engine stopped unexpectedly");
                ExitCode::from(1)
            }
            Err(message) => {
                eprintln!("ytmfast: {message}");
                ExitCode::from(1)
            }
        },
        Err(_) => {
            eprintln!("ytmfast: could not start the async runtime");
            ExitCode::from(1)
        }
    };
    if let Some(b) = bound {
        b.remove();
    }
    code
}

async fn serve(
    listener: std::os::unix::net::UnixListener,
    null_sink: bool,
) -> Result<Exit, String> {
    // First, so a stop that comes while anything below starts still ends cleanly.
    let shutdown =
        control::termination().map_err(|e| format!("could not watch for signals: {e}"))?;
    let listener = tokio::net::UnixListener::from_std(listener)
        .map_err(|e| format!("could not use the socket: {e}"))?;
    // Cookie folders an earlier engine left behind when it was killed mid-run: each holds a
    // copy of the session.
    if let Ok(dir) = paths::runtime_dir() {
        ytdlp::sweep_stale(&dir);
    }
    let backend = backend()?;
    let sink: Box<dyn Sink> = if null_sink {
        Box::new(NullSink::realtime())
    } else {
        Box::new(PipeWireSink::new())
    };
    let player = AudioPlayer::spawn(sink);
    let (mut engine, cmds, events) = Engine::new(backend.clone(), backend.clone(), player);
    // Every song heard counts in the account's YouTube Music history, as with the official
    // player (its pings go through the same lazily loaded session).
    engine.report_with(Reporter::new(backend));
    // The queue, song and second from before the restart (paused: resume never plays by
    // itself), and saving from now on. Without a state folder the engine still plays; it
    // just starts fresh each time.
    match paths::state_dir() {
        Ok(dir) => {
            if let Some(saved) = state::load(&dir) {
                engine.restore(saved);
            }
            engine.save_with(state::Writer::spawn(dir));
        }
        Err(e) => eprintln!(
            "ytmfast: no state folder ({:?}); the queue won't survive a restart",
            e.kind()
        ),
    }
    // MPRIS on the session bus, started inside `control::run` next to the socket. Without a
    // session bus it logs one line and the socket carries on alone.
    let options = control::Options {
        mpris: Some(ytmfast::mpris::Bus::Session),
        ..control::Options::default()
    };
    Ok(control::run(listener, engine, cmds, events, options, shutdown).await)
}

async fn play_track(args: PlayArgs) -> Result<(), String> {
    let resolver: Arc<dyn Resolver> = backend()?;

    ytmfast::trace::play(&args.video_id);
    let stream = resolver.resolve(&args.video_id).await.map_err(describe)?;
    ytmfast::trace::mark("link resolved");
    println!("{}", song_line(&stream.meta, &args.video_id));
    let gain = loudness_gain(stream.loudness_db);
    let mime = stream.mime.clone();
    let length_hint = Some(f64::from(stream.meta.length_seconds)).filter(|s| *s > 0.0);

    // A link that stops working mid-song is replaced by a fresh one, never a cached one
    // (ruling R2).
    let relink: Relink = {
        let resolver = resolver.clone();
        let id = args.video_id.clone();
        Box::new(move || {
            let resolver = resolver.clone();
            let id = id.clone();
            Box::pin(async move { resolver.resolve_fresh(&id).await.map(|s| s.url) })
        })
    };
    let buffer = TrackBuffer::start(stream, relink);

    let sink: Box<dyn Sink> = if args.null_sink {
        Box::new(NullSink::realtime())
    } else {
        Box::new(PipeWireSink::new())
    };
    let player = AudioPlayer::spawn(sink);
    let events = player.events();
    player.load(buffer.reader(), &mime, gain, 0.0, length_hint);
    player.play();
    // The wait, and dropping the player (which joins the audio thread), happen off this
    // runtime's one thread: the download task runs on it, and the audio thread may be blocked
    // reading bytes that only that task can deliver. (Dropping the player also cancels its
    // reader, so the join can't wait on the network either way.)
    let seconds = args.seconds;
    let outcome = tokio::task::spawn_blocking(move || {
        let outcome = wait_for_end(&events, seconds);
        drop(player);
        outcome
    })
    .await
    .map_err(|_| "the wait for the song failed".to_string())?;
    drop(buffer);
    outcome
}

/// "Title - Artist", leaving out what is missing; the video id when there is no title.
fn song_line(meta: &TrackMeta, video_id: &str) -> String {
    match (meta.title.is_empty(), meta.artist.is_empty()) {
        (false, false) => format!("{} - {}", meta.title, meta.artist),
        (false, true) => meta.title.clone(),
        (true, _) => video_id.to_string(),
    }
}

/// Waits for the song to end: `Ok` at its end or once `seconds` have passed, `Err` with the
/// message of an audio error.
fn wait_for_end(events: &Receiver<AudioEvent>, seconds: Option<f64>) -> Result<(), String> {
    let gone = || "the audio thread stopped".to_string();
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    loop {
        let event = match deadline {
            Some(d) => match events.recv_deadline(d) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => return Err(gone()),
            },
            None => events.recv().map_err(|_| gone())?,
        };
        match event {
            AudioEvent::Ended => return Ok(()),
            AudioEvent::Error(e) => return Err(e.to_string()),
            AudioEvent::Loading
            | AudioEvent::Advanced(_)
            | AudioEvent::Started
            | AudioEvent::Paused
            | AudioEvent::Resumed
            | AudioEvent::VolumeChanged(_) => {}
        }
    }
}

fn main() -> ExitCode {
    // Read once, before anything can play.
    ytmfast::trace::init_from_env();
    match parse(std::env::args().skip(1)) {
        Command::Daemon { null_sink } => daemon(null_sink),
        Command::ImportSession(profile) => import_session(profile),
        Command::Play(args) => play(args),
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
        assert_eq!(p(&["daemon"]), Command::Daemon { null_sink: false });
        assert_eq!(
            p(&["daemon", "--null-sink"]),
            Command::Daemon { null_sink: true }
        );
        assert_eq!(p(&["import-session"]), Command::ImportSession(None));
        assert_eq!(
            p(&["import-session", "--profile", "/x/YouTube Music"]),
            Command::ImportSession(Some("/x/YouTube Music".into()))
        );
        assert_eq!(
            p(&["play", "dQw4w9WgXcQ"]),
            Command::Play(play("dQw4w9WgXcQ"))
        );
        assert_eq!(p(&["--version"]), Command::Version);
        assert_eq!(p(&["-V"]), Command::Version);
        assert_eq!(p(&["--help"]), Command::Help);
        assert_eq!(p(&["-h"]), Command::Help);
    }

    fn play(id: &str) -> PlayArgs {
        PlayArgs {
            video_id: id.into(),
            null_sink: false,
            seconds: None,
        }
    }

    #[test]
    fn cli_parses_play_options() {
        assert_eq!(
            p(&["play", "dQw4w9WgXcQ", "--null-sink"]),
            Command::Play(PlayArgs {
                null_sink: true,
                ..play("dQw4w9WgXcQ")
            })
        );
        assert_eq!(
            p(&["play", "--seconds", "2.5", "--null-sink", "dQw4w9WgXcQ"]),
            Command::Play(PlayArgs {
                null_sink: true,
                seconds: Some(2.5),
                ..play("dQw4w9WgXcQ")
            })
        );
        for bad in [
            &["play", "id", "--seconds"][..],
            &["play", "id", "--seconds", "x"],
            &["play", "id", "--seconds", "0"],
            &["play", "id", "--seconds", "-1"],
            &["play", "id", "--seconds", "inf"],
            &["play", "id", "--null-sink", "--null-sink"],
            &["play", "--null-sink"],
            &["play", "id", "--loud"],
        ] {
            assert_eq!(p(bad), Command::Usage, "{bad:?}");
        }
    }

    #[test]
    fn song_line_leaves_out_what_is_missing() {
        let meta = |t: &str, a: &str| TrackMeta {
            title: t.into(),
            artist: a.into(),
            ..Default::default()
        };
        assert_eq!(song_line(&meta("Song", "Artist"), "id"), "Song - Artist");
        assert_eq!(song_line(&meta("Song", ""), "id"), "Song");
        assert_eq!(song_line(&meta("", ""), "dQw4w9WgXcQ"), "dQw4w9WgXcQ");
    }

    #[test]
    fn waiting_ends_on_ended_error_or_time() {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(AudioEvent::Started).unwrap();
        tx.send(AudioEvent::Ended).unwrap();
        assert_eq!(wait_for_end(&rx, None), Ok(()));

        tx.send(AudioEvent::Error(Error::StreamFailed("x".into())))
            .unwrap();
        assert_eq!(wait_for_end(&rx, None), Err("stream failed: x".into()));

        // Nothing comes: --seconds stops it.
        let t = std::time::Instant::now();
        assert_eq!(wait_for_end(&rx, Some(0.1)), Ok(()));
        assert!(t.elapsed() >= std::time::Duration::from_millis(100));

        drop(tx);
        assert!(
            wait_for_end(&rx, None).is_err(),
            "the audio thread went away"
        );
    }

    #[test]
    fn cli_rejects_bad_usage() {
        assert_eq!(p(&[]), Command::Usage);
        assert_eq!(p(&["frobnicate"]), Command::Usage);
        assert_eq!(p(&["play"]), Command::Usage);
        assert_eq!(p(&["play", "a", "b"]), Command::Usage);
        assert_eq!(p(&["daemon", "extra"]), Command::Usage);
        assert_eq!(p(&["daemon", "--null-sink", "--null-sink"]), Command::Usage);
        assert_eq!(p(&["import-session", "--profile"]), Command::Usage);
        assert_eq!(p(&["import-session", "/x"]), Command::Usage);
    }

    #[test]
    fn import_needs_a_sign_in_cookie() {
        use ytmfast::auth::Cookie;
        let cookie = |domain: &str, name: &str| Cookie {
            domain: domain.into(),
            name: name.into(),
            value: "fake-value".into(),
            path: "/".into(),
            secure: true,
            expires_utc: None,
        };
        // A visitor's cookies only: YouTube cookies, but nothing to sign a request with.
        let visitor = Session {
            cookies: vec![
                cookie(".youtube.com", "VISITOR_INFO1_LIVE"),
                cookie(".youtube.com", "YSC"),
            ],
        };
        assert!(!has_sign_in(&visitor));
        // A SAPISID for another domain doesn't sign youtube.com requests.
        let google_only = Session {
            cookies: vec![cookie(".google.com", "SAPISID")],
        };
        assert!(!has_sign_in(&google_only));
        let signed_in = Session {
            cookies: vec![cookie(".youtube.com", "SAPISID")],
        };
        assert!(has_sign_in(&signed_in));
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
