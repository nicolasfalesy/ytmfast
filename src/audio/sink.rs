//! Where decoded audio goes: the `Sink` trait, and `NullSink`, which plays nothing and counts
//! what it is given (tests and benchmarks). The real one is `pw::PipeWireSink`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::Error;

/// Called, from whatever thread notices, when an output's connection goes away. The audio
/// thread passes one that wakes it, so a paused song (which never writes) learns at once.
pub type LostNotify = Arc<dyn Fn() + Send + Sync>;

/// An audio output. Owned by the audio thread, so `&mut self` everywhere.
pub trait Sink: Send {
    /// Prepares the output for interleaved `f32` audio at `rate` with `channels` channels.
    /// Opening again at the same rate keeps the output as it is.
    fn open(&mut self, rate: u32, channels: u16) -> Result<(), Error>;
    /// Queues interleaved frames. Blocks while the output's buffer is full, so the caller
    /// decodes only as far ahead as the output holds. Never called while paused.
    fn write(&mut self, frames: &[f32]) -> Result<(), Error>;
    /// Stops or restarts taking audio. Queued audio stays queued.
    fn pause(&mut self, paused: bool);
    /// Drops all queued audio (a seek, a stop, a new track).
    fn flush(&mut self);
    /// The output's own volume, 0.0 to 1.0. Not a change to the samples.
    fn set_volume(&mut self, v: f32);
    /// Frames written but not yet heard: queued in the sink plus the output's latency.
    fn delay_frames(&self) -> u64;
    /// The output's connection is gone (the sound server restarted or crashed): writes fail
    /// with `Error::OutputRestarted` until `open` makes a new one. The audio thread asks before
    /// resuming, because a paused song never writes and so would never find out.
    fn lost(&self) -> bool {
        false
    }
    /// Calls `notify` once each time an open output is lost (not when it is closed on
    /// purpose). An output that can't be lost ignores it.
    fn watch_lost(&mut self, notify: LostNotify) {
        let _ = notify;
    }
}

/// What a `NullSink` saw. Shared, so a test keeps reading it after the sink moves to the
/// audio thread.
#[derive(Debug, Default)]
pub struct NullStats {
    frames: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    paused: AtomicBool,
    /// f32 bits.
    volume: AtomicU32,
    rate: AtomicU32,
    /// Realtime: nanoseconds the pretend buffer sat empty while playing, between two pieces
    /// of audio (a gap a listener would hear).
    silence_ns: AtomicU64,
    /// Set by `lose_output`, cleared by the next `open`.
    lost: AtomicBool,
    /// Told when `lose_output` loses the output.
    watcher: Watcher,
}

/// `NullStats`' watcher, in a type of its own so the stats stay `Debug`.
#[derive(Default)]
struct Watcher(Mutex<Option<LostNotify>>);

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Watcher")
    }
}

impl NullStats {
    /// Frames written since the sink was made (flushes don't take any back).
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::SeqCst)
    }
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::SeqCst)
    }
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::SeqCst)
    }
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::SeqCst))
    }
    /// The rate it was last opened at (0 = never opened).
    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::SeqCst)
    }
    /// Realtime: how long the output ran dry while playing, after audio and before more
    /// came (the gaps a listener would hear). Not counted: before the first audio, after a
    /// flush, after the last audio, and while paused.
    pub fn silence(&self) -> Duration {
        Duration::from_nanos(self.silence_ns.load(Ordering::SeqCst))
    }

    /// Plays a sound server restart: the sink acts like a `PipeWireSink` whose stream died
    /// (writes fail, `lost` is true) until it is opened again.
    pub fn lose_output(&self) {
        if !self.lost.swap(true, Ordering::SeqCst) {
            let watcher = self
                .watcher
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(notify) = watcher {
                notify();
            }
        }
    }
}

/// How a `NullSink` takes audio.
#[derive(Debug, Clone, Copy)]
enum Pace {
    /// At once, reporting a fixed delay: for counting and for decode benchmarks.
    Instant { delay: u64 },
    /// Like a real output: a buffer of `capacity` frames that drains at the rate in real
    /// time, so `write` blocks when it is full and nothing drains while paused.
    Realtime { capacity_secs: f64 },
}

/// A sink that plays nothing. See `Pace` for its two modes.
pub struct NullSink {
    stats: Arc<NullStats>,
    pace: Pace,
    rate: u32,
    channels: u16,
    /// Realtime: frames in the pretend buffer as of `since`.
    queued: u64,
    since: Instant,
    paused: bool,
    /// Realtime: audio was written since the last flush, so running dry is a gap...
    primed: bool,
    /// ...once more audio follows: dry time not yet counted, in seconds. A song's end (no
    /// more audio, then a pause) is not a gap.
    dry: f64,
}

impl NullSink {
    /// Takes audio at once, with no delay.
    pub fn new() -> NullSink {
        Self::with_delay(0)
    }

    /// Takes audio at once and always reports `frames` of delay.
    pub fn with_delay(frames: u64) -> NullSink {
        Self::make(Pace::Instant { delay: frames })
    }

    /// Plays in real time into nothing, with a 200 ms buffer like `PipeWireSink`'s: for a
    /// play run that costs what a real one does, without sound.
    pub fn realtime() -> NullSink {
        Self::make(Pace::Realtime { capacity_secs: 0.2 })
    }

    fn make(pace: Pace) -> NullSink {
        let stats = Arc::new(NullStats::default());
        stats.volume.store(1.0f32.to_bits(), Ordering::SeqCst);
        NullSink {
            stats,
            pace,
            rate: 0,
            channels: 2,
            queued: 0,
            since: Instant::now(),
            paused: false,
            primed: false,
            dry: 0.0,
        }
    }

    pub fn stats(&self) -> Arc<NullStats> {
        self.stats.clone()
    }

    /// Realtime: the pretend buffer's fill now.
    fn fill_now(&self) -> u64 {
        if self.paused || self.rate == 0 {
            return self.queued;
        }
        let played = self.since.elapsed().as_secs_f64() * f64::from(self.rate);
        self.queued.saturating_sub(played as u64)
    }

    fn settle(&mut self) {
        if self.primed && !self.paused && self.rate > 0 {
            // Played past what was queued: the buffer ran dry for the difference.
            let dry =
                self.since.elapsed().as_secs_f64() - self.queued as f64 / f64::from(self.rate);
            if dry > 0.0 {
                self.dry += dry;
            }
        }
        self.queued = self.fill_now();
        self.since = Instant::now();
    }
}

impl Default for NullSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for NullSink {
    fn open(&mut self, rate: u32, channels: u16) -> Result<(), Error> {
        if rate == 0 || channels == 0 {
            return Err(Error::Internal("bad output format".into()));
        }
        // Account for the old rate's buffer (and any dry time) before the rate changes.
        self.settle();
        self.rate = rate;
        self.channels = channels;
        self.stats.rate.store(rate, Ordering::SeqCst);
        // A new output: whatever the old one lost, this one has.
        self.stats.lost.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), Error> {
        if self.rate == 0 {
            return Err(Error::Internal("the output is not open".into()));
        }
        if self.lost() {
            return Err(Error::OutputRestarted);
        }
        let n = (frames.len() / usize::from(self.channels)) as u64;
        if let Pace::Realtime { capacity_secs } = self.pace {
            let capacity = (capacity_secs * f64::from(self.rate)) as u64;
            loop {
                self.settle();
                if self.queued + n <= capacity.max(n) {
                    break;
                }
                if self.paused {
                    return Err(Error::Internal("write while paused".into()));
                }
                if self.lost() {
                    return Err(Error::OutputRestarted);
                }
                // Sleep for as long as the deficit takes to play: no busy wait.
                let deficit = self.queued + n - capacity.max(n);
                std::thread::sleep(Duration::from_secs_f64(
                    deficit as f64 / f64::from(self.rate),
                ));
            }
            self.queued += n;
            self.primed = true;
            if self.dry > 0.0 {
                self.stats
                    .silence_ns
                    .fetch_add((self.dry * 1e9) as u64, Ordering::SeqCst);
                self.dry = 0.0;
            }
        }
        self.stats.frames.fetch_add(n, Ordering::SeqCst);
        self.stats.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn pause(&mut self, paused: bool) {
        self.settle();
        self.paused = paused;
        self.stats.paused.store(paused, Ordering::SeqCst);
    }

    fn flush(&mut self) {
        self.primed = false;
        self.dry = 0.0;
        self.queued = 0;
        self.since = Instant::now();
        self.stats.flushes.fetch_add(1, Ordering::SeqCst);
    }

    fn set_volume(&mut self, v: f32) {
        self.stats.volume.store(v.to_bits(), Ordering::SeqCst);
    }

    fn delay_frames(&self) -> u64 {
        match self.pace {
            Pace::Instant { delay } => delay,
            Pace::Realtime { .. } => self.fill_now(),
        }
    }

    fn lost(&self) -> bool {
        self.stats.lost.load(Ordering::SeqCst)
    }

    fn watch_lost(&mut self, notify: LostNotify) {
        *self
            .stats
            .watcher
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(notify);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_counts_frames() {
        let mut s = NullSink::with_delay(4800);
        let stats = s.stats();
        assert!(s.write(&[0.0; 4]).is_err(), "write before open");
        s.open(48_000, 2).unwrap();
        s.write(&[0.0; 1920]).unwrap();
        assert_eq!(stats.frames(), 960);
        assert_eq!(s.delay_frames(), 4800);
        s.set_volume(0.5);
        assert_eq!(stats.volume(), 0.5);
    }

    #[test]
    fn a_lost_output_fails_writes_until_reopened() {
        let mut s = NullSink::new();
        let stats = s.stats();
        s.open(48_000, 2).unwrap();
        assert!(!s.lost());
        stats.lose_output();
        assert!(s.lost());
        assert_eq!(s.write(&[0.0; 4]), Err(Error::OutputRestarted));
        s.open(48_000, 2).unwrap();
        assert!(!s.lost(), "open reconnects");
        s.write(&[0.0; 4]).unwrap();
    }

    #[test]
    fn a_lost_output_tells_its_watcher_once() {
        let mut s = NullSink::new();
        let stats = s.stats();
        let calls = Arc::new(AtomicU64::new(0));
        let seen = calls.clone();
        s.watch_lost(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        s.open(48_000, 2).unwrap();
        stats.lose_output();
        stats.lose_output();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "once per loss");
        s.open(48_000, 2).unwrap();
        stats.lose_output();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a new output can be lost again"
        );
    }

    #[test]
    fn realtime_counts_silence_while_dry() {
        let mut s = NullSink::realtime();
        let stats = s.stats();
        s.open(48_000, 2).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        // Before the first audio: not a gap.
        s.write(&[0.0; 960 * 2]).unwrap();
        assert_eq!(stats.silence(), Duration::ZERO);
        // 20 ms queued, then 60 ms without a write: dry for about 40 ms.
        std::thread::sleep(Duration::from_millis(60));
        s.write(&[0.0; 960 * 2]).unwrap();
        let dry = stats.silence();
        assert!(
            dry >= Duration::from_millis(35) && dry <= Duration::from_millis(60),
            "{dry:?}"
        );
        // Paused: nothing plays, so nothing runs dry.
        s.pause(true);
        std::thread::sleep(Duration::from_millis(60));
        s.pause(false);
        s.write(&[0.0; 960 * 2]).unwrap();
        assert!(stats.silence() - dry < Duration::from_millis(5));
        // A flush (a seek, a stop) ends the audio on purpose: the wait after it isn't a gap.
        s.flush();
        std::thread::sleep(Duration::from_millis(40));
        s.write(&[0.0; 960 * 2]).unwrap();
        assert!(stats.silence() - dry < Duration::from_millis(5));
    }

    #[test]
    fn realtime_blocks_when_full_and_drains() {
        let mut s = NullSink::realtime();
        s.open(48_000, 2).unwrap();
        let t = Instant::now();
        // 300 ms into a 200 ms buffer: the last 100 ms has to wait for room.
        for _ in 0..15 {
            s.write(&[0.0; 960 * 2]).unwrap();
        }
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(90), "{waited:?}");
        assert!(s.delay_frames() <= 9600);
        // Paused: nothing drains.
        s.pause(true);
        let held = s.delay_frames();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(s.delay_frames(), held);
        s.flush();
        assert_eq!(s.delay_frames(), 0);
    }
}
