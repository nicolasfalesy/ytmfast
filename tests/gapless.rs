//! Gapless handover: a preloaded track follows the current one in the same output, with no
//! flush and no silence between them (gap budget: 5 ms between two tracks of the same rate).
//!
//! The NullSink tests run everywhere. `volume_not_reapplied_on_pause` needs a private
//! PipeWire daemon (see `common/mod.rs`) and is ignored; run it with
//! `cargo test --test gapless -- --ignored`. It is the only test here that changes the
//! environment, so `--ignored` runs it alone.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ytmfast::audio::fetch::{TrackBuffer, TrackReader};
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::sink::{NullSink, Sink};
use ytmfast::error::Error;

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

fn reader(name: &str) -> TrackReader {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    TrackBuffer::from_bytes(std::fs::read(path).unwrap()).reader()
}

fn mime(name: &str) -> &'static str {
    if name.ends_with(".m4a") {
        AAC_MIME
    } else {
        OPUS_MIME
    }
}

fn raw_event(events: &Receiver<AudioEvent>) -> AudioEvent {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("an event within 10 s")
}

/// The next event other than `Loading`.
fn next_event(events: &Receiver<AudioEvent>) -> AudioEvent {
    loop {
        match raw_event(events) {
            AudioEvent::Loading => continue,
            e => return e,
        }
    }
}

/// What a `Tape` saw. Shared, so the test reads it after the sink moved to the audio thread.
#[derive(Default)]
struct TapeLog {
    samples: Mutex<Vec<f32>>,
    opens: Mutex<Vec<u32>>,
    flushes: AtomicU64,
}

/// A sink that takes audio at once, with no delay, and keeps every sample it is given.
struct Tape(Arc<TapeLog>);

impl Sink for Tape {
    fn open(&mut self, rate: u32, _: u16) -> Result<(), Error> {
        let mut opens = self.0.opens.lock().unwrap();
        // Like the real sinks: opening again at the same rate keeps the output.
        if opens.last() != Some(&rate) {
            opens.push(rate);
        }
        Ok(())
    }
    fn write(&mut self, frames: &[f32]) -> Result<(), Error> {
        self.0.samples.lock().unwrap().extend_from_slice(frames);
        Ok(())
    }
    fn pause(&mut self, _: bool) {}
    fn flush(&mut self) {
        self.0.flushes.fetch_add(1, Ordering::SeqCst);
    }
    fn set_volume(&mut self, _: f32) {}
    fn delay_frames(&self) -> u64 {
        0
    }
}

fn tape() -> (AudioPlayer, Receiver<AudioEvent>, Arc<TapeLog>) {
    let log = Arc::new(TapeLog::default());
    let p = AudioPlayer::spawn(Box::new(Tape(log.clone())));
    let events = p.events();
    (p, events, log)
}

/// One fixture on its own, as the player writes it: its samples, interleaved stereo.
fn alone(name: &str) -> Vec<f32> {
    let (p, events, log) = tape();
    p.load(reader(name), mime(name), 1.0, 0.0, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    assert_eq!(next_event(&events), AudioEvent::Ended);
    log.samples.lock().unwrap().clone()
}

/// The longest run of silent frames (both channels exactly 0) in `samples[from..to]`.
fn longest_silence(samples: &[f32], from: usize, to: usize) -> usize {
    let (mut best, mut run) = (0, 0);
    for frame in samples[from * 2..to * 2].chunks(2) {
        if frame.iter().all(|s| *s == 0.0) {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

#[test]
fn same_rate_handover_is_gapless() {
    let a = alone("tone_a_48k.webm");
    let b = alone("tone_b_48k.webm");
    let a_frames = a.len() / 2;

    // 1. Frame accounting: every frame of A, then every frame of B, nothing between and no
    // flush at the boundary.
    let (p, events, log) = tape();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    let id = p.preload(reader("tone_b_48k.webm"), OPUS_MIME, 1.0, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    let flushes = log.flushes.load(Ordering::SeqCst);
    assert_eq!(next_event(&events), AudioEvent::Advanced(id));
    assert_eq!(next_event(&events), AudioEvent::Ended);
    assert_eq!(
        log.flushes.load(Ordering::SeqCst),
        flushes,
        "no flush at the handover"
    );
    let samples = log.samples.lock().unwrap().clone();
    assert_eq!(samples.len(), a.len() + b.len(), "A then B, every frame");
    assert!(samples[..a.len()] == a[..], "A's frames, as alone");
    assert!(
        samples[a.len()..] == b[..],
        "B's frames start right after A's last"
    );
    // Nothing silent was inserted: the boundary is as silent as the tones themselves.
    let around = longest_silence(&samples, a_frames - 480, a_frames + 480);
    assert!(around <= 1, "{around} silent frames at the boundary");
    // Both decoders were open at once only briefly: the player's own measure agrees.
    let gap = p.last_gap().expect("a handover reports its gap");
    eprintln!("same rate, instant sink: last_gap {gap:?}");
    assert!(gap <= Duration::from_millis(5), "{gap:?}");

    // 2. In real time: the output never runs dry between A and B (silence counts the time
    // the 200 ms buffer was empty while playing), and the clock restarts at B's first frame.
    let sink = NullSink::realtime();
    let stats = sink.stats();
    let p = AudioPlayer::spawn(Box::new(sink));
    let events = p.events();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    let id = p.preload(reader("tone_b_48k.webm"), OPUS_MIME, 1.0, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    let start = Instant::now();
    // Just before the handover is heard, the position is near A's end.
    std::thread::sleep(Duration::from_millis(2800));
    let late = p.position();
    assert!(late > 2.6 && late < 3.01, "A's position {late}");
    assert_eq!(next_event(&events), AudioEvent::Advanced(id));
    let heard = start.elapsed().as_secs_f64();
    let at = p.position();
    assert!(at < 0.05, "B's clock starts at its first frame: {at}");
    // Advanced comes when B's first frame is heard: A's whole length after the start.
    assert!(
        (heard - a_frames as f64 / 48_000.0).abs() < 0.06,
        "Advanced after {heard} s"
    );
    assert_eq!(next_event(&events), AudioEvent::Ended);
    let silence = stats.silence();
    let gap = p.last_gap().unwrap();
    eprintln!("same rate, real-time sink: output dry for {silence:?}, last_gap {gap:?}");
    assert!(
        silence <= Duration::from_millis(5),
        "{silence:?} of silence"
    );
    assert!(gap <= Duration::from_millis(5), "{gap:?}");
    assert_eq!(stats.frames() as usize, a_frames + b.len() / 2);
}

#[test]
fn rate_change_renegotiates() {
    // Review Focus 5: Opus (48 kHz) then AAC (44.1 kHz). No crash, the second song starts,
    // and the gap is counted and reported.
    let a = alone("tone_a_48k.webm");
    let c = alone("tone_c_44k.m4a");

    let (p, events, log) = tape();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    let id = p.preload(reader("tone_c_44k.m4a"), AAC_MIME, 1.0, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    assert_eq!(next_event(&events), AudioEvent::Advanced(id));
    assert_eq!(next_event(&events), AudioEvent::Ended);
    assert_eq!(
        *log.opens.lock().unwrap(),
        [48_000, 44_100],
        "reopened at C's rate"
    );
    let samples = log.samples.lock().unwrap().clone();
    assert_eq!(samples.len(), a.len() + c.len());
    assert!(
        samples[a.len()..] == c[..],
        "all of C, from its first frame"
    );
    let gap = p.last_gap().expect("the gap is counted");
    eprintln!("rate change, instant sink: last_gap {gap:?}");

    // In real time: the old stream drains, then the new one opens; the gap is what that
    // costs (no budget: a new stream at a new rate can't be seamless).
    let sink = NullSink::realtime();
    let stats = sink.stats();
    let p = AudioPlayer::spawn(Box::new(sink));
    let events = p.events();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 2.0, None);
    let id = p.preload(reader("tone_c_44k.m4a"), AAC_MIME, 1.0, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    assert_eq!(next_event(&events), AudioEvent::Advanced(id));
    assert!(p.position() < 0.05, "C's clock: {}", p.position());
    assert_eq!(stats.rate(), 44_100);
    let gap = p.last_gap().unwrap();
    eprintln!(
        "rate change, real-time sink: last_gap {gap:?}, output dry {:?}",
        stats.silence()
    );
    assert!(gap < Duration::from_millis(100), "{gap:?}");
    p.stop();
}

#[test]
fn stop_drops_preload() {
    let sink = NullSink::realtime();
    let p = AudioPlayer::spawn(Box::new(sink));
    let events = p.events();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    let next = reader("tone_b_48k.webm");
    let cancel = next.canceller();
    p.preload(next, OPUS_MIME, 1.0, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    std::thread::sleep(Duration::from_millis(200));
    p.stop();
    // Its download is let go at once, like the current track's.
    assert!(cancel.is_cancelled(), "the preload's reader is cancelled");
    std::thread::sleep(Duration::from_millis(200));
    assert!(events.try_recv().is_err(), "no events after stop");

    // A track loaded after the stop ends on its own: the old preload doesn't follow it.
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 2.8, None);
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    assert_eq!(next_event(&events), AudioEvent::Ended);
}

#[test]
fn preload_replaced_by_newer_preload() {
    let a = alone("tone_a_48k.webm");
    let b = alone("tone_b_48k.webm");

    let (p, events, log) = tape();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    let first = reader("tone_b_48k.webm");
    let first_cancel = first.canceller();
    let old = p.preload(first, OPUS_MIME, 1.0, None);
    // The queue changed: A again follows instead.
    let new = p.preload(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, None);
    assert_ne!(old, new);
    assert!(
        first_cancel.is_cancelled(),
        "the replaced preload is let go"
    );
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    assert_eq!(next_event(&events), AudioEvent::Advanced(new));
    assert_eq!(next_event(&events), AudioEvent::Ended);
    let samples = log.samples.lock().unwrap().clone();
    assert_eq!(samples.len(), 2 * a.len());
    assert!(samples[a.len()..] == a[..], "the newer preload followed");
    assert_ne!(a.len(), 0);
    assert_ne!(b[..480], a[..480], "the fixtures differ");

    // A cancelled preload is not played: the track just ends.
    let (p, events, log) = tape();
    p.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    p.preload(reader("tone_b_48k.webm"), OPUS_MIME, 1.0, None);
    p.cancel_preload();
    p.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    assert_eq!(next_event(&events), AudioEvent::Ended);
    assert_eq!(log.samples.lock().unwrap().len(), a.len());
}

#[test]
#[ignore = "needs the pipewire, pw-cli and pw-link programs; run with --ignored"]
fn volume_not_reapplied_on_pause() {
    // The stream's volume is set when it changes, and never again on a state change: a
    // pause and a resume must not undo a change the user made in a mixer.
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = common::use_private_daemon(tmp.path());
    let daemon = common::Daemon::start(&dir);

    let player = AudioPlayer::spawn(Box::new(ytmfast::audio::pw::PipeWireSink::new()));
    let events = player.events();
    player.set_volume(0.5);
    // Long enough to pause in: A, with B queued behind it.
    player.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
    player.play();
    assert_eq!(next_event(&events), AudioEvent::Started);
    let node = common::link(&dir);
    // Ours: 0.5 on the slider is 0.125 on the channels (cubic).
    common::wait_for_volume(&dir, node, 0.125);
    // The user turns it down in a mixer.
    common::set_volume(&dir, node, 0.3);
    common::wait_for_volume(&dir, node, 0.3);
    player.pause();
    assert_eq!(next_event(&events), AudioEvent::Paused);
    std::thread::sleep(Duration::from_millis(200));
    player.play();
    assert_eq!(next_event(&events), AudioEvent::Resumed);
    std::thread::sleep(Duration::from_millis(300));
    let now = common::volume(&dir, node).expect("the stream's volume");
    assert!((now - 0.3).abs() < 1e-4, "the mixer's 0.3 stayed: {now}");
    // A change of ours still applies, once.
    player.set_volume(1.0);
    common::wait_for_volume(&dir, node, 1.0);

    drop(player);
    daemon.stop();
}
