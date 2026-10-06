//! A sound server restart under a playing or a paused song, against a private PipeWire daemon.
//!
//! Found live: the sound server was restarted while a song played, the song went quietly to
//! `paused`, and the next play failed with "the audio output stopped". The output must say
//! that it restarted (an `internal` error the engine retries on), at once even when paused,
//! and the next song must get a new stream. Our own closes (a new song, quit) must not count.
//!
//! The daemon is the private one in `common/mod.rs`: the user's own PipeWire is never reached.
//!
//! Ignored: it needs the `pipewire`, `pw-cli` and `pw-link` programs. Run it with
//! `cargo test --test pipewire_restart -- --ignored`.

mod common;

use std::time::Duration;

use crossbeam_channel::Receiver;
use tempfile::TempDir;
use ytmfast::audio::fetch::{TrackBuffer, TrackReader};
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::pw::PipeWireSink;
use ytmfast::error::Error;

use common::{Daemon, link, use_private_daemon};

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";

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
