//! A sound server restart under a playing or a paused song, against a private PipeWire daemon.
//!
//! Found live: the sound server was restarted while a song played, the song went quietly to
//! `paused`, and the next play failed with "the audio output stopped". The output must say
//! that it restarted (an `internal` error the engine retries on), at once even when paused,
//! and the next song must get a new stream. Our own closes (a new song, quit) must not count.
//!
//! The daemon runs from a config written here: a null sink and the modules a client stream
//! needs, nothing that opens a sound card, no session manager and no D-Bus. Its socket is in
//! a temp folder, and this test binary's `PIPEWIRE_RUNTIME_DIR` points there, so the user's
//! own PipeWire is never reached. Without a session manager nothing links the stream, so the
//! test links it to the null sink itself (`pw-cli`, `pw-link`).
//!
//! Ignored: it needs the `pipewire`, `pw-cli` and `pw-link` programs. Run it with
//! `cargo test --test pipewire_restart -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use tempfile::TempDir;
use ytmfast::audio::fetch::{TrackBuffer, TrackReader};
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::pw::PipeWireSink;
use ytmfast::error::Error;

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";

/// The private daemon: a 48 kHz null sink (`test-sink`) driving the graph, and the protocol,
/// client-node and adapter modules a client stream needs. No ALSA, no D-Bus, no RT module.
const CONFIG: &str = r#"
context.properties = {
    core.daemon = true
    core.name   = pipewire-0
    default.clock.rate = 48000
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    audio.adapt     = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-access args = { } }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
context.objects = [
    { factory = metadata args = { metadata.name = default } }
    { factory = spa-node-factory
        args = { factory.name = support.node.driver node.name = Dummy-Driver
                 node.group = pipewire.dummy priority.driver = 20000 } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink node.name = test-sink
                 media.class = Audio/Sink audio.position = [ FL FR ] audio.rate = 48000 } }
]
"#;

/// One private daemon run. Dropping it kills the daemon.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(dir: &Path) -> Daemon {
        let socket = dir.join("pipewire-0");
        let _ = std::fs::remove_file(&socket);
        let config = dir.join("test.conf");
        std::fs::write(&config, CONFIG).unwrap();
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PIPEWIRE_RUNTIME_DIR", dir)
            .env("XDG_RUNTIME_DIR", dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            // A bus address that leads nowhere: the daemon must not reach the user's bus.
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", dir.join("nobus").display()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the pipewire program (this test is run by hand: see the file's top)");
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                Instant::now() < until,
                "the private daemon never made its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The null sink has no ports until it is told its layout (a session manager's job).
        let configured = pw_tool(
            dir,
            "pw-cli",
            &["s", "test-sink", "PortConfig", &port_config("Input")],
        );
        assert!(configured.is_some(), "could not set up the null sink");
        Daemon { child }
    }

    /// Stops it the way `systemctl restart` does: SIGTERM, then wait.
    fn stop(mut self) {
        // SAFETY: kill(2) on our own child's pid; it can't touch any other process while we
        // haven't reaped it yet.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `pw-cli` or `pw-link` against the private daemon only.
fn pw_tool(dir: &Path, program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PIPEWIRE_RUNTIME_DIR", dir)
        .env("XDG_RUNTIME_DIR", dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Stereo F32 ports at 48 kHz, one per channel, on a node's `direction` side.
fn port_config(direction: &str) -> String {
    format!(
        r#"{{ "direction": "{direction}", "mode": "dsp", "format": {{ "mediaType": "audio",
             "mediaSubtype": "raw", "format": "F32P", "rate": 48000, "channels": 2,
             "position": [ "FL", "FR" ] }} }}"#
    )
}

/// The id of the newest node called `ytmfast`, from `pw-cli ls Node`.
fn ytmfast_node(dir: &Path) -> Option<u32> {
    let listing = pw_tool(dir, "pw-cli", &["ls", "Node"])?;
    let mut id = None;
    let mut found = None;
    for line in listing.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id ") {
            id = rest.split(',').next().and_then(|n| n.trim().parse().ok());
        } else if line == "node.name = \"ytmfast\"" {
            found = id;
        }
    }
    found
}

/// Links the stream to the null sink, as a session manager would.
fn link(dir: &Path) {
    let until = Instant::now() + Duration::from_secs(5);
    let node = loop {
        if let Some(n) = ytmfast_node(dir) {
            break n;
        }
        assert!(
            Instant::now() < until,
            "the stream never showed up in the daemon"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let node = node.to_string();
    pw_tool(
        dir,
        "pw-cli",
        &["s", &node, "PortConfig", &port_config("Output")],
    );
    for channel in ["FL", "FR"] {
        let from = format!("ytmfast:output_{channel}");
        let to = format!("test-sink:playback_{channel}");
        let until = Instant::now() + Duration::from_secs(5);
        while pw_tool(dir, "pw-link", &[&from, &to]).is_none() {
            assert!(Instant::now() < until, "could not link {from}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn fixture() -> TrackReader {
    let path = format!(
        "{}/tests/fixtures/sine440_48k.webm",
        env!("CARGO_MANIFEST_DIR")
    );
    TrackBuffer::from_bytes(std::fs::read(path).unwrap()).reader()
}

/// The next event other than `Loading`, within `wait`.
fn next_event(events: &Receiver<AudioEvent>, wait: Duration) -> AudioEvent {
    loop {
        match events.recv_timeout(wait).expect("an event in time") {
            AudioEvent::Loading => continue,
            e => return e,
        }
    }
}

fn assert_restarted(e: AudioEvent) {
    match e {
        AudioEvent::Error(e) => {
            assert_eq!(e, Error::OutputRestarted);
            assert_eq!(e.code(), "internal");
            assert_eq!(e.to_string(), "internal error: the audio output restarted");
        }
        e => panic!("wanted the output-restarted error, got {e:?}"),
    }
}

/// Points this process's PipeWire clients at the private daemon.
fn use_private_daemon(dir: &Path) -> PathBuf {
    // SAFETY: this binary holds this one test, and it sets the variables before it starts any
    // thread that could read the environment (the player and PipeWire threads come after).
    unsafe {
        std::env::remove_var("PIPEWIRE_REMOTE");
        std::env::set_var("PIPEWIRE_RUNTIME_DIR", dir);
        std::env::set_var("XDG_RUNTIME_DIR", dir);
    }
    dir.to_path_buf()
}

#[test]
#[ignore]
fn output_survives_a_sound_server_restart() {
    let tmp = TempDir::new().unwrap();
    let dir = use_private_daemon(tmp.path());
    let daemon = Daemon::start(&dir);

    let player = AudioPlayer::spawn(Box::new(PipeWireSink::new()));
    let events = player.events();
    player.set_volume(0.0);

    // 1. Playing when the daemon goes away: an error that says so, at once.
    player.load(fixture(), OPUS_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Started
    );
    link(&dir);
    std::thread::sleep(Duration::from_millis(300));
    daemon.stop();
    assert_restarted(next_event(&events, Duration::from_secs(2)));

    // 2. The next song gets a new stream on the new daemon, and plays to its end.
    let daemon = Daemon::start(&dir);
    player.load(fixture(), OPUS_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Started
    );
    link(&dir);
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Ended
    );

    // 3. Paused when the daemon goes away: reported at once, without waiting for a play
    // (the PipeWire thread's news wakes the paused audio thread).
    player.load(fixture(), OPUS_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Started
    );
    std::thread::sleep(Duration::from_millis(200));
    player.pause();
    assert_eq!(
        next_event(&events, Duration::from_secs(2)),
        AudioEvent::Paused
    );
    daemon.stop();
    assert_restarted(next_event(&events, Duration::from_secs(2)));
    let daemon = Daemon::start(&dir);

    // 4. And the song after that plays, on a new stream.
    player.load(fixture(), OPUS_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Started
    );
    link(&dir);
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Ended
    );

    // 5. Our own close (the AAC fixture's 44.1 kHz needs a new stream) is not a loss: the
    // song plays to its end with no error.
    let aac = format!(
        "{}/tests/fixtures/sine440_44k.m4a",
        env!("CARGO_MANIFEST_DIR")
    );
    player.load(
        TrackBuffer::from_bytes(std::fs::read(aac).unwrap()).reader(),
        "audio/mp4; codecs=\"mp4a.40.2\"",
        1.0,
        0.0,
        None,
    );
    player.play();
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Started
    );
    link(&dir);
    assert_eq!(
        next_event(&events, Duration::from_secs(5)),
        AudioEvent::Ended
    );

    drop(player);
    daemon.stop();
}
