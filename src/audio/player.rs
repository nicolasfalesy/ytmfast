//! The audio thread: decodes the current track into a `Sink`, and takes commands from any
//! thread.
//!
//! Commands go in over a channel and events come out on another, so the engine never waits on
//! the audio thread. The thread never spins: while playing it blocks inside `Sink::write`
//! until the output has room (so it decodes at most the output's buffer ahead, which keeps
//! the CPU asleep most of the time); while paused or idle it blocks on the command channel;
//! at the end of a track it waits, in steps, for the output to play what it holds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

use crate::audio::decode::Decoder;
use crate::audio::fetch::{ReaderCancel, TrackReader};
use crate::audio::sink::Sink;
use crate::error::Error;

/// What the audio thread reports.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioEvent {
    /// The audio thread took the next `load` and is opening it: every event after this one is
    /// about that track. One per `load`, in order, so a listener that counts its loads can
    /// tell a late event from an earlier track (an `Ended` already on its way when a new song
    /// was picked) from one about the newest track.
    Loading,
    /// The loaded track's first `play`.
    Started,
    Paused,
    Resumed,
    /// The track played to its end (all of it heard, not just decoded).
    Ended,
    /// The track failed; the player is idle. The error itself, so its `code()` reaches the
    /// user (its `Display` is URL-free, ruling R6).
    Error(Error),
}

enum Command {
    Load {
        reader: TrackReader,
        mime: String,
        gain: f32,
        start: f64,
        length_hint: Option<f64>,
    },
    Play,
    Pause,
    Seek(f64),
    Volume(f32),
    Stop,
    /// The sink says its output was lost (`Sink::watch_lost`).
    OutputLost,
    Quit,
}

/// How long past the output's reported delay the end of a track waits before calling it
/// ended anyway: an output that stops draining (a stalled device) must not hold `Ended` back
/// for ever.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// The longest single wait while draining, so the position stays fresh and a stalled output
/// is noticed.
const DRAIN_STEP_MAX: Duration = Duration::from_millis(50);
const DRAIN_STEP_MIN: Duration = Duration::from_millis(5);

/// The handle the engine holds. Dropping it stops the thread.
pub struct AudioPlayer {
    commands: Sender<Command>,
    events: Receiver<AudioEvent>,
    /// Seconds into the track, as f64 bits, kept fresh by the audio thread.
    position: Arc<AtomicU64>,
    /// Cancels the newest loaded track's reader. The audio thread can be blocked in a read
    /// waiting for a stalled download; `stop`, a new `load` and drop cancel it first, so they
    /// never wait behind the network.
    current: Mutex<Option<ReaderCancel>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Starts the audio thread with `sink` as its output.
    pub fn spawn(mut sink: Box<dyn Sink>) -> AudioPlayer {
        let (commands, inbox) = crossbeam_channel::unbounded();
        // The sink's news of a lost output comes in as a command, so it wakes an audio thread
        // that is waiting for one (paused): a dead output is reported at once, not at the next
        // play, and nothing polls for it.
        let wake = commands.clone();
        sink.watch_lost(Arc::new(move || {
            let _ = wake.send(Command::OutputLost);
        }));
        let (events_tx, events) = crossbeam_channel::unbounded();
        let position = Arc::new(AtomicU64::new(0f64.to_bits()));
        let worker = Worker {
            sink,
            inbox,
            events: events_tx,
            position: position.clone(),
            track: None,
            scratch: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("ytmfast-audio".into())
            .spawn(move || worker.run())
            .expect("the audio thread should start");
        AudioPlayer {
            commands,
            events,
            position,
            current: Mutex::new(None),
            thread: Some(thread),
        }
    }

    /// The event stream. Every clone sees every event only once between them (a channel, not
    /// a broadcast): the engine should hold one.
    pub fn events(&self) -> Receiver<AudioEvent> {
        self.events.clone()
    }

    /// Replaces the current track with `reader`, paused at `start_seconds`, with `gain`
    /// applied to its samples (loudness normalisation). `length_hint` is the resolver's length
    /// in seconds, used when the file states none (see `Decoder::with_length_hint`). `play`
    /// starts it.
    pub fn load(
        &self,
        reader: TrackReader,
        mime: &str,
        gain: f32,
        start_seconds: f64,
        length_hint: Option<f64>,
    ) {
        self.cancel_current(Some(reader.canceller()));
        self.send(Command::Load {
            reader,
            mime: mime.to_string(),
            gain,
            start: start_seconds,
            length_hint,
        });
    }

    pub fn play(&self) {
        self.send(Command::Play);
    }

    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    pub fn seek(&self, seconds: f64) {
        self.send(Command::Seek(seconds));
    }

    /// The output's volume, 0.0 to 1.0 (clamped). The samples are not scaled: mixers see it
    /// as the stream's own volume.
    pub fn set_volume(&self, volume: f32) {
        self.send(Command::Volume(volume));
    }

    /// Unloads the track and empties the output.
    pub fn stop(&self) {
        self.cancel_current(None);
        self.send(Command::Stop);
    }

    /// Cancels the current track's reader and makes `next` the current one.
    fn cancel_current(&self, next: Option<ReaderCancel>) {
        let old = std::mem::replace(
            &mut *self.current.lock().unwrap_or_else(|e| e.into_inner()),
            next,
        );
        if let Some(old) = old {
            old.cancel();
        }
    }

    /// Seconds into the track that the listener is hearing now: (frames written − the
    /// output's delay) / rate, from the start point of the last load or seek.
    pub fn position(&self) -> f64 {
        f64::from_bits(self.position.load(Ordering::Acquire))
    }

    fn send(&self, command: Command) {
        // Fails only once the thread has gone (it never exits on its own before `Quit`).
        let _ = self.commands.send(command);
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.cancel_current(None);
        self.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The loaded track.
struct Track {
    decoder: Decoder,
    /// Set when the engine cancelled this track's reader: its read errors are then expected
    /// and not reported.
    cancel: ReaderCancel,
    gain: f32,
    rate: f64,
    /// The position (seconds) where `written` counts from: the load or seek point.
    base: f64,
    /// Frames written to the sink since `base`.
    written: u64,
    started: bool,
    playing: bool,
    /// Every frame is decoded and written; waiting for the output to play them.
    drain: Option<Instant>,
    ended: bool,
}

struct Worker {
    sink: Box<dyn Sink>,
    inbox: Receiver<Command>,
    events: Sender<AudioEvent>,
    position: Arc<AtomicU64>,
    track: Option<Track>,
    /// The gain-scaled copy of a packet's frames, reused.
    scratch: Vec<f32>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let busy = self.track.as_ref().filter(|t| t.playing && !t.ended);
            let command = match busy {
                // Playing: take any waiting command, else decode the next packet.
                Some(t) if t.drain.is_none() => match self.inbox.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                },
                // Draining: wait for a command or the next check.
                Some(t) => {
                    let wait = self.drain_step(t.rate);
                    match self.inbox.recv_timeout(wait) {
                        Ok(c) => Some(c),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                // Paused, ended or idle: nothing to do until told.
                None => match self.inbox.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            };
            match command {
                Some(Command::Quit) => return,
                Some(c) => self.handle(c),
                None => self.step(),
            }
        }
    }

    /// How long to wait before the next drain check: the output's delay, within bounds.
    fn drain_step(&self, rate: f64) -> Duration {
        let delay = Duration::from_secs_f64(self.sink.delay_frames() as f64 / rate);
        delay.clamp(DRAIN_STEP_MIN, DRAIN_STEP_MAX)
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Load {
                reader,
                mime,
                gain,
                start,
                length_hint,
            } => self.load(reader, &mime, gain, start, length_hint),
            Command::Play => {
                if self.track.as_ref().is_none_or(|t| t.playing || t.ended) {
                    return;
                }
                // The sound server went away while paused: resuming would write into a dead
                // stream and fail with a vaguer error (the user's live finding). Say what
                // happened instead; the engine plays the song again from here on a new one.
                if self.sink.lost() {
                    return self.fail(Error::OutputRestarted);
                }
                let Some(t) = self.track.as_mut() else {
                    return;
                };
                t.playing = true;
                if t.drain.is_some() {
                    // Paused while draining: the deadline restarts with the output.
                    t.drain = Some(Instant::now());
                }
                let event = if t.started {
                    AudioEvent::Resumed
                } else {
                    AudioEvent::Started
                };
                t.started = true;
                self.sink.pause(false);
                self.emit(event);
            }
            Command::Pause => {
                let Some(t) = self.track.as_mut().filter(|t| t.playing) else {
                    return;
                };
                t.playing = false;
                self.sink.pause(true);
                self.publish();
                self.emit(AudioEvent::Paused);
            }
            Command::Seek(seconds) => {
                let Some(t) = self.track.as_mut() else {
                    return;
                };
                match t.decoder.seek(seconds) {
                    Ok(at) => {
                        t.base = at;
                        t.written = 0;
                        t.drain = None;
                        t.ended = false;
                        self.sink.flush();
                        self.publish();
                    }
                    Err(e) => {
                        let cancel = t.cancel.clone();
                        self.fail_unless_cancelled(e, &cancel);
                    }
                }
            }
            Command::Volume(v) => {
                if !v.is_nan() {
                    self.sink.set_volume(v.clamp(0.0, 1.0));
                }
            }
            Command::Stop => self.unload(),
            Command::OutputLost => {
                // Only about the current output (a replaced one doesn't matter), and only when
                // a song is loaded: a playing song's write may have reported it already.
                if self.sink.lost() && self.track.as_ref().is_some_and(|t| !t.ended) {
                    self.fail(Error::OutputRestarted);
                }
            }
            Command::Quit => {}
        }
    }

    fn load(
        &mut self,
        reader: TrackReader,
        mime: &str,
        gain: f32,
        start: f64,
        length_hint: Option<f64>,
    ) {
        self.emit(AudioEvent::Loading);
        self.unload();
        let cancel = reader.canceller();
        // Opening reads the track's headers, so it waits for the first bytes to arrive.
        let mut decoder = match Decoder::open(reader, mime) {
            Ok(d) => d.with_length_hint(length_hint),
            Err(e) => return self.fail_unless_cancelled(e, &cancel),
        };
        crate::trace::mark("decoder open");
        let base = if start > 0.0 {
            match decoder.seek(start) {
                Ok(at) => at,
                Err(e) => return self.fail_unless_cancelled(e, &cancel),
            }
        } else {
            0.0
        };
        if let Err(e) = self.sink.open(decoder.rate(), 2) {
            return self.fail(e);
        }
        crate::trace::mark("output open");
        self.track = Some(Track {
            rate: f64::from(decoder.rate()),
            decoder,
            cancel,
            gain: if gain.is_finite() {
                gain.clamp(0.0, 1.0)
            } else {
                1.0
            },
            base,
            written: 0,
            started: false,
            playing: false,
            drain: None,
            ended: false,
        });
        self.publish();
    }

    /// Drops the track; the output is emptied and paused (an idle stream would keep the
    /// audio graph, and the CPU, awake).
    fn unload(&mut self) {
        self.drop_track();
        self.position.store(0f64.to_bits(), Ordering::Release);
    }

    /// `unload`, keeping the last position: after a failure it says where the song stopped.
    fn drop_track(&mut self) {
        self.track = None;
        self.sink.flush();
        self.sink.pause(true);
    }

    /// One unit of work while playing: decode and write a packet, or check the drain.
    fn step(&mut self) {
        let Some(t) = self.track.as_mut() else {
            return;
        };
        if let Some(since) = t.drain {
            let delay = self.sink.delay_frames();
            let deadline = since + Duration::from_secs_f64(delay as f64 / t.rate) + DRAIN_GRACE;
            if delay == 0 || Instant::now() >= deadline {
                t.ended = true;
                t.playing = false;
                self.sink.pause(true);
                self.publish();
                self.emit(AudioEvent::Ended);
            } else {
                self.publish();
            }
            return;
        }
        let frames = match t.decoder.next_frames() {
            Ok(Some(f)) => f,
            Ok(None) => {
                t.drain = Some(Instant::now());
                self.publish();
                return;
            }
            Err(e) => {
                let cancel = t.cancel.clone();
                return self.fail_unless_cancelled(e, &cancel);
            }
        };
        let n = (frames.len() / 2) as u64;
        let out: &[f32] = if t.gain == 1.0 {
            frames
        } else {
            self.scratch.clear();
            self.scratch.extend(frames.iter().map(|s| s * t.gain));
            &self.scratch
        };
        if let Err(e) = self.sink.write(out) {
            return self.fail(e);
        }
        if t.written == 0 {
            // Also after a seek, which starts the count again: that is a start too.
            crate::trace::mark("first frames written");
        }
        t.written += n;
        self.publish();
    }

    /// Recomputes the position from what was written and the output's delay.
    fn publish(&self) {
        let Some(t) = &self.track else {
            return;
        };
        let heard = t.written.saturating_sub(self.sink.delay_frames());
        let seconds = t.base + heard as f64 / t.rate;
        self.position.store(seconds.to_bits(), Ordering::Release);
    }

    /// A failed track: report it and go idle. The position stays where the song stopped, for
    /// the engine's status and its replay after an output restart.
    fn fail(&mut self, e: Error) {
        self.drop_track();
        self.emit(AudioEvent::Error(e));
    }

    /// Like `fail`, but quiet when the engine cancelled the track's reader on purpose (stop,
    /// a new load, drop): that error is the cancel itself, not a fault to report.
    fn fail_unless_cancelled(&mut self, e: Error, cancel: &ReaderCancel) {
        if cancel.is_cancelled() {
            self.unload();
        } else {
            self.fail(e);
        }
    }

    fn emit(&self, event: AudioEvent) {
        // Nobody listening is fine: the engine may not care about events.
        let _ = self.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fetch::TrackBuffer;
    use crate::audio::sink::{NullSink, NullStats};
    use std::sync::Arc;

    const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
    const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

    fn fixture(name: &str) -> TrackReader {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        TrackBuffer::from_bytes(std::fs::read(path).unwrap()).reader()
    }

    fn player(sink: NullSink) -> (AudioPlayer, Receiver<AudioEvent>, Arc<NullStats>) {
        let stats = sink.stats();
        let p = AudioPlayer::spawn(Box::new(sink));
        let events = p.events();
        (p, events, stats)
    }

    /// The next event, skipping `Loading` (most tests are about what follows it).
    fn next_event(events: &Receiver<AudioEvent>) -> AudioEvent {
        loop {
            match raw_event(events) {
                AudioEvent::Loading => continue,
                e => return e,
            }
        }
    }

    fn raw_event(events: &Receiver<AudioEvent>) -> AudioEvent {
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("an event within 5 s")
    }

    #[test]
    fn player_position_counts_delay() {
        let (p, events, stats) = player(NullSink::with_delay(4800));
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        // The sink never drains (its delay is fixed), so the end comes from the drain
        // deadline; by then every frame is written.
        assert_eq!(next_event(&events), AudioEvent::Ended);
        let written = stats.frames();
        assert!(written >= 96_000, "{written} frames written");
        let want = (written - 4800) as f64 / 48_000.0;
        assert!(
            (p.position() - want).abs() < 1e-9,
            "{} vs {want}",
            p.position()
        );
        assert!((p.position() - 1.9).abs() < 0.002, "{}", p.position());
    }

    #[test]
    fn pause_stops_writes() {
        let (p, events, stats) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        let t = std::time::Instant::now();
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(300));
        p.pause();
        assert_eq!(next_event(&events), AudioEvent::Paused);
        let elapsed = t.elapsed().as_secs_f64();
        assert!(stats.paused());
        let frames = stats.frames();
        let at = p.position();
        // Decoded only as far ahead as the 200 ms buffer: what has played, plus the buffer,
        // plus the packet that didn't fit.
        let most = ((elapsed + 0.2) * 48_000.0) as u64 + 960;
        assert!(frames <= most, "{frames} frames written in {elapsed} s");
        assert!(
            at >= 0.25 && at <= elapsed,
            "position {at} after {elapsed} s"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(stats.frames(), frames, "no writes while paused");
        assert_eq!(p.position(), at, "the position holds while paused");

        p.play();
        assert_eq!(next_event(&events), AudioEvent::Resumed);
        assert!(!stats.paused());
        std::thread::sleep(Duration::from_millis(200));
        assert!(stats.frames() > frames, "writes again after resume");
        assert!(p.position() > at);
    }

    #[test]
    fn ended_event_at_eof() {
        let (p, events, stats) = player(NullSink::new());
        p.load(fixture("sine440_44k.m4a"), AAC_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        assert_eq!(next_event(&events), AudioEvent::Ended);
        assert_eq!(stats.rate(), 44_100);
        assert_eq!(stats.frames(), 88_200, "the whole track, trimmed");
        assert!((p.position() - 2.0).abs() < 1e-9);
        // Ended leaves the output paused: an idle stream keeps the audio graph awake.
        assert!(stats.paused());
    }

    #[test]
    fn load_at_a_start_point_and_seek() {
        let (p, events, stats) = player(NullSink::new());
        p.load(fixture("sine440_44k.m4a"), AAC_MIME, 1.0, 0.5, None);
        p.seek(0.75);
        p.set_volume(0.3);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        assert_eq!(next_event(&events), AudioEvent::Ended);
        // From 0.75 s to the end.
        assert_eq!(stats.frames(), 88_200 - 33_075);
        assert!((stats.volume() - 0.3).abs() < 1e-6);
        assert!((p.position() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn gain_scales_samples() {
        // A sink that keeps the peak it was given.
        struct Peak(Arc<std::sync::Mutex<f32>>);
        impl Sink for Peak {
            fn open(&mut self, _: u32, _: u16) -> Result<(), Error> {
                Ok(())
            }
            fn write(&mut self, f: &[f32]) -> Result<(), Error> {
                let mut p = self.0.lock().unwrap();
                *p = f.iter().fold(*p, |m, s| m.max(s.abs()));
                Ok(())
            }
            fn pause(&mut self, _: bool) {}
            fn flush(&mut self) {}
            fn set_volume(&mut self, _: f32) {}
            fn delay_frames(&self) -> u64 {
                0
            }
        }
        let peak = |gain: f32| {
            let seen = Arc::new(std::sync::Mutex::new(0.0f32));
            let p = AudioPlayer::spawn(Box::new(Peak(seen.clone())));
            let events = p.events();
            p.load(fixture("sine440_44k.m4a"), AAC_MIME, gain, 0.0, None);
            p.play();
            assert_eq!(next_event(&events), AudioEvent::Started);
            assert_eq!(next_event(&events), AudioEvent::Ended);
            *seen.lock().unwrap()
        };
        let full = peak(1.0);
        let half = peak(0.5);
        assert!((half / full - 0.5).abs() < 1e-4, "{half} vs {full}");
    }

    #[test]
    fn bad_track_is_an_error_event() {
        let (p, events, _) = player(NullSink::new());
        let garbage = TrackBuffer::from_bytes(vec![0x5a; 4096]).reader();
        p.load(garbage, OPUS_MIME, 1.0, 0.0, None);
        p.play();
        match next_event(&events) {
            AudioEvent::Error(e) => {
                // The error itself, so the engine can report its code.
                assert_eq!(e.code(), "stream_failed");
                assert!(e.to_string().contains("not a recognised audio file"), "{e}");
            }
            e => panic!("wanted an error, got {e:?}"),
        }
    }

    #[test]
    fn stop_flushes_and_resets() {
        let (p, events, stats) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(100));
        p.stop();
        // Commands are handled in order: a play after stop finds no track and does nothing.
        p.play();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(p.position(), 0.0);
        assert!(stats.flushes() >= 1);
        assert!(stats.paused());
        assert!(events.try_recv().is_err(), "no events after stop");
    }

    /// The first `n` bytes of a fixture, from a download that then stalls for ever.
    fn stalled(name: &str, n: usize) -> TrackReader {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let bytes = std::fs::read(path).unwrap();
        let total = bytes.len() as u64;
        TrackBuffer::stalled(bytes[..n].to_vec(), total).reader()
    }

    /// Fails the test, rather than hanging the suite, if `f` takes over 3 s.
    fn within_3s(what: &str, f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(3)).is_ok(),
            "{what} hung behind the stalled download"
        );
    }

    #[test]
    fn drop_returns_while_opening_a_stalled_download() {
        let (p, _events, _) = player(NullSink::new());
        // Too few bytes to open: the audio thread blocks in the decoder's first reads.
        p.load(stalled("sine440_48k.webm", 100), OPUS_MIME, 1.0, 0.0, None);
        std::thread::sleep(Duration::from_millis(100));
        within_3s("dropping the player", move || drop(p));
    }

    #[test]
    fn drop_returns_while_a_track_stalls_mid_way() {
        let (p, events, stats) = player(NullSink::new());
        // MP4 with its index at the front: it opens and plays, then runs out of bytes.
        p.load(stalled("sine440_44k.m4a", 20_000), AAC_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(100));
        assert!(stats.frames() > 0, "played the part that arrived");
        within_3s("dropping the player", move || drop(p));
    }

    #[test]
    fn stop_and_a_new_load_work_while_a_download_stalls() {
        let (p, events, stats) = player(NullSink::new());
        p.load(stalled("sine440_48k.webm", 100), OPUS_MIME, 1.0, 0.0, None);
        std::thread::sleep(Duration::from_millis(100));
        p.stop();
        p.load(fixture("sine440_44k.m4a"), AAC_MIME, 1.0, 0.0, None);
        p.play();
        // The stopped track fails quietly: no Error event for it.
        assert_eq!(next_event(&events), AudioEvent::Started);
        assert_eq!(next_event(&events), AudioEvent::Ended);
        assert_eq!(stats.frames(), 88_200);
    }

    #[test]
    fn each_load_is_announced_before_its_events() {
        let (p, events, _) = player(NullSink::new());
        p.load(stalled("sine440_48k.webm", 100), OPUS_MIME, 1.0, 0.0, None);
        p.load(fixture("sine440_44k.m4a"), AAC_MIME, 1.0, 0.0, None);
        p.play();
        // One `Loading` per load, in order, and the second track's events after its own:
        // the engine counts them to drop events that belong to an earlier track.
        assert_eq!(raw_event(&events), AudioEvent::Loading);
        assert_eq!(raw_event(&events), AudioEvent::Loading);
        assert_eq!(raw_event(&events), AudioEvent::Started);
        assert_eq!(raw_event(&events), AudioEvent::Ended);
    }

    #[test]
    fn output_lost_while_playing_is_reported_where_it_was() {
        let (p, events, stats) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(300));
        stats.lose_output();
        assert_eq!(
            next_event(&events),
            AudioEvent::Error(Error::OutputRestarted)
        );
        // The position stays where the song was, so the engine can play it again from there.
        let at = p.position();
        assert!(at > 0.05 && at < 0.4, "position {at}");
    }

    #[test]
    fn output_lost_while_paused_is_reported_at_once() {
        let (p, events, stats) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(200));
        p.pause();
        assert_eq!(next_event(&events), AudioEvent::Paused);
        let at = p.position();
        // No play needed: the sink's own news wakes the paused audio thread.
        stats.lose_output();
        assert_eq!(
            next_event(&events),
            AudioEvent::Error(Error::OutputRestarted)
        );
        assert_eq!(p.position(), at, "the position stays where it was paused");
    }

    #[test]
    fn play_after_the_output_was_lost_while_paused_reports_it() {
        let (p, events, stats) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(200));
        p.pause();
        assert_eq!(next_event(&events), AudioEvent::Paused);
        stats.lose_output();
        // Not `Resumed` into a dead stream (the user's live finding): the restart, at once.
        p.play();
        assert_eq!(
            next_event(&events),
            AudioEvent::Error(Error::OutputRestarted)
        );
        // The next song opens a new output and plays.
        p.load(fixture("sine440_44k.m4a"), AAC_MIME, 1.0, 0.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        assert_eq!(next_event(&events), AudioEvent::Ended);
    }

    #[test]
    fn a_seek_in_the_last_200_ms_plays_on() {
        // The song's end is decoded up to 200 ms before it is heard: a seek then lands in a
        // decoder that has read to the end (the demuxer is read again, see `Decoder::seek`),
        // not in a "damaged audio" error.
        let (p, events, _) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 1.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(900));
        p.seek(0.5);
        assert_eq!(next_event(&events), AudioEvent::Ended);
    }

    #[test]
    fn length_hint_reaches_the_decoder() {
        let (p, _events, _) = player(NullSink::new());
        p.load(
            fixture("sine440_44k_frag.m4a"),
            AAC_MIME,
            1.0,
            0.0,
            Some(2.0),
        );
        p.seek(99.0);
        // Commands run in order; the position is published after the seek.
        let t = std::time::Instant::now();
        while p.position() == 0.0 && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!((p.position() - 1.0).abs() < 1e-9, "{}", p.position());
    }
}
