//! Playing stays cheap: the process's CPU time and wakeups per second of audio, playing a local
//! file through the real audio thread into a null output that keeps real time (no PipeWire, no
//! sound). Release builds, on a machine in the state the limits were measured in
//! (`perf_common`).
//!
//! The 2-second fixtures are played back to back to make a 6-second run; their loads are
//! part of the numbers, as they are in real play. Limits are the measured value plus 20%.

mod perf_common;

use std::time::{Duration, Instant};

use ytmfast::audio::fetch::TrackBuffer;
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::sink::NullSink;

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

/// (fixture, mime, CPU ms per second of audio, wakeups per second): measured times 1.2.
/// Opus: best of three 2.9 to 3.5 ms (the audit: 3.7), 49 wakeups (52); AAC: 2.1 to 2.3 ms
/// (2.6 to 3.4), 43 wakeups (45). The CPU limit for Opus is the audit's, the larger of the two.
const LIMITS: [(&str, &str, f64, f64); 2] = [
    ("sine440_48k.webm", OPUS_MIME, 4.4, 59.0),
    ("sine440_44k.m4a", AAC_MIME, 2.7, 52.0),
];

/// Plays `name` three times in a row; (CPU ms per second, voluntary context switches per
/// second) of the whole process.
fn play(name: &str, mime: &str) -> (f64, f64) {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the fixture is in the repo");
    let player = AudioPlayer::spawn(Box::new(NullSink::realtime()));
    let events = player.events();
    // Let the audio thread start before counting.
    std::thread::sleep(Duration::from_millis(300));
    let (cpu0, sw0) = (
        perf_common::cpu_seconds(),
        perf_common::voluntary_switches(),
    );
    let start = Instant::now();
    for _ in 0..3 {
        player.load(
            TrackBuffer::from_bytes(bytes.clone()).reader(),
            mime,
            0.8,
            0.0,
            None,
        );
        player.play();
        loop {
            match events.recv_timeout(Duration::from_secs(10)) {
                Ok(AudioEvent::Ended) => break,
                Ok(_) => {}
                Err(_) => panic!("{name}: the song never ended"),
            }
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = perf_common::cpu_seconds() - cpu0;
    let switches = perf_common::voluntary_switches() - sw0;
    drop(player);
    (cpu * 1000.0 / wall, switches as f64 / wall)
}

#[test]
fn playing_stays_within_its_cpu_and_wakeup_limits() {
    if !perf_common::speed_limits_apply("perf_play") {
        return;
    }
    for (name, mime, max_cpu, max_wakeups) in LIMITS {
        // The lowest of three runs: a one-off hiccup should not fail the test.
        let runs: Vec<(f64, f64)> = (0..3).map(|_| play(name, mime)).collect();
        let cpu = runs.iter().map(|r| r.0).fold(f64::MAX, f64::min);
        let wakeups = runs.iter().map(|r| r.1).fold(f64::MAX, f64::min);
        println!("{name}: {cpu:.2} ms of CPU and {wakeups:.1} wakeups per second ({runs:.2?})");
        assert!(
            cpu <= max_cpu,
            "{name}: {cpu:.2} ms CPU per second, the limit is {max_cpu}"
        );
        assert!(
            wakeups <= max_wakeups,
            "{name}: {wakeups:.1} wakeups per second, the limit is {max_wakeups}"
        );
    }
}
