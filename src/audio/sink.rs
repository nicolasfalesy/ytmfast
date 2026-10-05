//! Where decoded audio goes: the `Sink` trait, and `NullSink`, which plays nothing and counts
//! what it is given (tests and benchmarks). The real one is `pw::PipeWireSink`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::Error;

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
        self.rate = rate;
        self.channels = channels;
        self.stats.rate.store(rate, Ordering::SeqCst);
        Ok(())
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), Error> {
        if self.rate == 0 {
            return Err(Error::Internal("the output is not open".into()));
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
                // Sleep for as long as the deficit takes to play: no busy wait.
                let deficit = self.queued + n - capacity.max(n);
                std::thread::sleep(Duration::from_secs_f64(
                    deficit as f64 / f64::from(self.rate),
                ));
            }
            self.queued += n;
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
