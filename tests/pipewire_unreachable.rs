//! With no PipeWire to reach, a play reports an error at once, and so does the next one.
//!
//! The controller's live finding: with XDG_RUNTIME_DIR pointed at a folder with no PipeWire
//! socket (and no PIPEWIRE_RUNTIME_DIR), the first play waited for ever after "decoder open"
//! and only the second reported the error. This sets up exactly that environment for this test
//! binary only: an empty temp folder, so the user's own PipeWire is never reached. It needs no
//! daemon, so it is not ignored and runs in CI.

use std::time::{Duration, Instant};

use ytmfast::audio::fetch::TrackBuffer;
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::pw::PipeWireSink;
use ytmfast::audio::sink::Sink;

#[test]
fn open_without_pipewire_fails_promptly() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary, and it changes the environment before starting
    // any thread that could read it.
    unsafe {
        std::env::remove_var("PIPEWIRE_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        std::env::remove_var("PIPEWIRE_REMOTE");
    }
    // Through the audio thread first, as a play does.
    let player = AudioPlayer::spawn(Box::new(PipeWireSink::new()));
    let events = player.events();
    let path = format!(
        "{}/tests/fixtures/sine440_48k.webm",
        env!("CARGO_MANIFEST_DIR")
    );
    let t = Instant::now();
    player.load(
        TrackBuffer::from_bytes(std::fs::read(path).unwrap()).reader(),
        "audio/webm; codecs=\"opus\"",
        1.0,
        0.0,
        None,
    );
    player.play();
    loop {
        match events.recv_timeout(Duration::from_secs(4)) {
            Ok(AudioEvent::Loading) => continue,
            Ok(AudioEvent::Error(e)) => {
                assert_eq!(e.code(), "internal");
                break;
            }
            other => panic!(
                "wanted an error event, got {other:?} after {:?}",
                t.elapsed()
            ),
        }
    }
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    drop(player);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = PipeWireSink::new();
        let t = Instant::now();
        let first = sink.open(48_000, 2).map(|_| ());
        let first_took = t.elapsed();
        let second = sink.open(48_000, 2).map(|_| ());
        let _ = tx.send((first, first_took, second));
    });
    let (first, took, second) = rx
        .recv_timeout(Duration::from_secs(4))
        .expect("open returned (it hung with no PipeWire to reach)");
    assert!(first.is_err(), "{first:?}");
    assert!(second.is_err(), "{second:?}");
    assert!(
        took < Duration::from_secs(1),
        "the first open took {took:?}"
    );
}
