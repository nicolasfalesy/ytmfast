//! A mixer's volume change, read back from a real PipeWire stream, against a private daemon
//! (see `common/mod.rs`): the player reports it, never echoes its own changes, and a new
//! stream (here a rate change) starts at the mixer's volume, not the app's older one.
//!
//! Ignored: it needs the `pipewire`, `pw-cli` and `pw-link` programs. Run it with
//! `cargo test --test volume_pipewire -- --ignored`. It is the only test in this binary,
//! because it points the process's PipeWire clients at the private daemon.

mod common;

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ytmfast::audio::fetch::{TrackBuffer, TrackReader};
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::pw::PipeWireSink;

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

fn reader(name: &str) -> TrackReader {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    TrackBuffer::from_bytes(std::fs::read(path).unwrap()).reader()
}

/// The events of the next `wait`, in order.
fn events_for(events: &Receiver<AudioEvent>, wait: Duration) -> Vec<AudioEvent> {
    let until = Instant::now() + wait;
    let mut seen = Vec::new();
    while let Ok(e) = events.recv_deadline(until) {
        seen.push(e);
    }
    seen
}

fn volume_changes(seen: &[AudioEvent]) -> Vec<f32> {
    seen.iter()
        .filter_map(|e| match e {
            AudioEvent::VolumeChanged(v) => Some(*v),
            _ => None,
        })
        .collect()
}

/// Waits up to 5 s for an event that isn't `Loading`.
fn next_event(events: &Receiver<AudioEvent>) -> AudioEvent {
    loop {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(AudioEvent::Loading) => continue,
            Ok(e) => return e,
            Err(e) => panic!("no event within 5 s: {e}"),
        }
    }
}

#[test]
#[ignore = "needs the pipewire, pw-cli and pw-link programs; run with --ignored"]
fn a_mixer_volume_is_read_back_and_kept_on_a_new_stream() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = common::use_private_daemon(tmp.path());
    let daemon = common::Daemon::start(&dir);

    let player = AudioPlayer::spawn(Box::new(PipeWireSink::new()));
    let events = player.events();
    player.set_volume(0.5);
    player.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    let node = common::link(&dir);
    // Ours: 0.5 on the slider is 0.125 on the channels (cubic). It is not reported back.
    common::wait_for_volume(&dir, node, 0.125);
    let seen = events_for(&events, Duration::from_millis(300));
    assert_eq!(volume_changes(&seen), [] as [f32; 0], "{seen:?}");

    // The user turns it down in a mixer: 0.064 on the channels is 0.4 on the slider.
    common::set_volume(&dir, node, 0.064);
    let seen = events_for(&events, Duration::from_millis(500));
    let changes = volume_changes(&seen);
    assert_eq!(changes.len(), 1, "{seen:?}");
    assert!((changes[0] - 0.4).abs() < 1e-3, "{changes:?}");

    // A song at another rate makes a new stream: it starts at the mixer's volume.
    player.load(reader("tone_c_44k.m4a"), AAC_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    // The old stream was closed before the new one was made; give the daemon a moment to
    // drop it, so the link goes to the new one (node ids can be reused, so not by its id).
    std::thread::sleep(Duration::from_millis(300));
    let new_node = common::link(&dir);
    common::wait_for_volume(&dir, new_node, 0.064);

    // A change of ours still applies, and is not reported back either.
    player.set_volume(1.0);
    common::wait_for_volume(&dir, new_node, 1.0);
    let seen = events_for(&events, Duration::from_millis(300));
    assert_eq!(volume_changes(&seen), [] as [f32; 0], "{seen:?}");

    drop(player);
    daemon.stop();
}
