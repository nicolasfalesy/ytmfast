//! Gapless handover through a real PipeWire stream, against a private daemon (see
//! `common/mod.rs`): the stream must never run dry between two songs of the same rate.
//!
//! Ignored: it needs the `pipewire`, `pw-cli` and `pw-link` programs. Run it with
//! `cargo test --test gapless_pipewire -- --ignored --nocapture` (it prints what it measured).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ytmfast::audio::fetch::{TrackBuffer, TrackReader};
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::pw::PipeWireSink;
use ytmfast::audio::sink::{LostNotify, Sink};
use ytmfast::error::Error;

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";

fn reader(name: &str) -> TrackReader {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    TrackBuffer::from_bytes(std::fs::read(path).unwrap()).reader()
}

/// A `PipeWireSink` whose dropout count the test can read after it moved to the audio thread.
struct Probe {
    inner: PipeWireSink,
    underruns: Arc<AtomicU64>,
}

impl Probe {
    fn note(&self) {
        self.underruns
            .store(self.inner.underruns(), Ordering::SeqCst);
    }
}

impl Sink for Probe {
    fn open(&mut self, rate: u32, channels: u16) -> Result<(), Error> {
        self.inner.open(rate, channels)
    }
    fn write(&mut self, frames: &[f32]) -> Result<(), Error> {
        let r = self.inner.write(frames);
        self.note();
        r
    }
    fn pause(&mut self, paused: bool) {
        self.note();
        self.inner.pause(paused);
    }
    fn flush(&mut self) {
        self.inner.flush();
    }
    fn set_volume(&mut self, v: f32) {
        self.inner.set_volume(v);
    }
    fn delay_frames(&self) -> u64 {
        // Read while draining too, so the count includes the song's own end.
        self.note();
        self.inner.delay_frames()
    }
    fn lost(&self) -> bool {
        self.inner.lost()
    }
    fn watch_lost(&mut self, notify: LostNotify) {
        self.inner.watch_lost(notify);
    }
}

#[test]
#[ignore = "needs the pipewire, pw-cli and pw-link programs; run with --ignored"]
fn same_rate_handover_never_runs_dry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = common::use_private_daemon(tmp.path());
    let daemon = common::Daemon::start(&dir);

    let underruns = Arc::new(AtomicU64::new(0));
    let player = AudioPlayer::spawn(Box::new(Probe {
        inner: PipeWireSink::new(),
        underruns: underruns.clone(),
    }));
    let events = player.events();
    player.set_volume(0.0);
    // The last second of A, then all of B.
    player.load(reader("tone_a_48k.webm"), OPUS_MIME, 1.0, 2.0, None);
    let id = player.preload(reader("tone_b_48k.webm"), OPUS_MIME, 1.0, None);
    player.play();
    let wait = Duration::from_secs(10);
    let next = || loop {
        match events.recv_timeout(wait).expect("an event in time") {
            AudioEvent::Loading => continue,
            e => return e,
        }
    };
    assert_eq!(next(), AudioEvent::Started);
    common::link(&dir);
    let linked = Instant::now();
    assert_eq!(next(), AudioEvent::Advanced(id));
    let advanced = linked.elapsed();
    let before_end = underruns.load(Ordering::SeqCst);
    let at = player.position();
    assert_eq!(next(), AudioEvent::Ended);
    let total = underruns.load(Ordering::SeqCst);
    let gap = player.last_gap().unwrap();
    eprintln!(
        "PipeWire, same rate: dropouts before B's end {before_end}, in all {total} \
         (B's own end is one); Advanced {advanced:?} after the link, B at {at:.4} s then; \
         last_gap {gap:?}"
    );
    assert_eq!(before_end, 0, "the stream ran dry at the handover");
    assert!(total <= 1, "{total} dropouts");
    assert!(at < 0.1, "B's clock at Advanced: {at}");
    assert!(gap <= Duration::from_millis(5), "{gap:?}");

    drop(player);
    daemon.stop();
}
