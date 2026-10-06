//! The audio thread: decodes the current track into a `Sink`, and takes commands from any
//! thread.
//!
//! Commands go in over a channel and events come out on another, so the engine never waits on
//! the audio thread. The thread never spins: while playing it blocks inside `Sink::write`
//! until the output has room (so it decodes at most the output's buffer ahead, which keeps
//! the CPU asleep most of the time); while paused or idle it blocks on the command channel;
//! at the end of a track it waits, in steps, for the output to play what it holds.
//!
//! Gapless: the engine hands over the next track early (`preload`). When the current track's
//! decoder runs out, the next one's frames go into the same output right behind it, with no
//! flush, so the output never runs dry between them. The position clock starts again at the
//! exact frame where the new track starts, once that frame is heard (`AudioEvent::Advanced`).
//! A track at another sample rate can't share the output: the old track plays out, the output
//! is opened again at the new rate, and the gap that costs is measured and logged.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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
    /// The track played to its end (all of it heard, not just decoded), with nothing
    /// preloaded behind it.
    Ended,
    /// The preloaded track with this id (`AudioPlayer::preload`'s) is now the current one:
    /// the old track's last frame and the new track's first frame were heard back to back.
    /// Sent when the new track's first frame is heard, which is also where the position starts
    /// again from 0. It takes the place of the old track's `Ended` and the new track's
    /// `Started` (the new track plays on in the old one's state).
    Advanced(u64),
    /// The track failed; the player is idle. The error itself, so its `code()` reaches the
    /// user (its `Display` is URL-free, ruling R6).
    Error(Error),
    /// The output's volume was changed outside the app (a mixer): the new slider value, 0.0
    /// to 1.0. Not about any track, and never sent for `set_volume`.
    VolumeChanged(f32),
}

enum Command {
    Load {
        reader: TrackReader,
        mime: String,
        gain: f32,
        start: f64,
        length_hint: Option<f64>,
    },
    Preload {
        id: u64,
        reader: TrackReader,
        mime: String,
        gain: f32,
        length_hint: Option<f64>,
    },
    CancelPreload,
    Play,
    Pause,
    Seek(f64),
    Volume(f32),
    Stop,
    /// The sink says its output was lost (`Sink::watch_lost`).
    OutputLost,
    /// The sink says a mixer set its volume to this (`Sink::watch_volume`).
    MixerVolume(f32),
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

/// The gap budget between two tracks of the same rate (Global Constraints). Only a log line
/// when it is broken: nothing can be done about it after the fact.
const GAP_BUDGET: Duration = Duration::from_millis(5);

/// `last_gap` before any handover.
const NO_GAP: u64 = u64::MAX;

/// The readers the engine can cancel: the current track's and the preloaded one's.
///
/// Shared with the audio thread, which moves the preload's into `current` at a handover
/// under this lock. So a `preload` or `cancel_preload` sent just as the handover happens can
/// only ever cancel a track that is still waiting, never the one that is playing.
#[derive(Default)]
struct Readers {
    current: Option<ReaderCancel>,
    next: Option<(u64, ReaderCancel)>,
    /// Every preload id up to this one was cancelled. Kept apart from `next`: in the
    /// handover window the preload is already `current`, so a cancel finds no `next` to drop,
    /// and a seek's roll-back must still know not to bring it back.
    cancelled_through: u64,
}

impl Readers {
    fn cancel_all(&mut self) {
        if let Some(c) = self.current.take() {
            c.cancel();
        }
        self.cancel_next();
    }

    fn cancel_next(&mut self) {
        if let Some((_, c)) = self.next.take() {
            c.cancel();
        }
    }

    fn next_id(&self) -> Option<u64> {
        self.next.as_ref().map(|(id, _)| *id)
    }
}

fn lock(readers: &Mutex<Readers>) -> MutexGuard<'_, Readers> {
    readers.lock().unwrap_or_else(|e| e.into_inner())
}

/// The handle the engine holds. Dropping it stops the thread.
pub struct AudioPlayer {
    commands: Sender<Command>,
    events: Receiver<AudioEvent>,
    /// Seconds into the track, as f64 bits, kept fresh by the audio thread.
    position: Arc<AtomicU64>,
    /// Cancels the loaded and the preloaded tracks' readers. The audio thread can be blocked
    /// in a read waiting for a stalled download; `stop`, a new `load`, a new `preload` and
    /// drop cancel first, so they never wait behind the network.
    readers: Arc<Mutex<Readers>>,
    /// The last handover's gap in µs (`NO_GAP` before the first).
    gap_us: Arc<AtomicU64>,
    /// The id of the last preload the audio thread moved on to (0 before the first).
    advanced: Arc<AtomicU64>,
    /// Ids handed out by `preload`.
    preloads: AtomicU64,
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
        // A mixer's change comes in the same way and goes out as an event. Not straight
        // into the event channel: a sender held by the sink's watcher (or a PipeWire thread
        // left behind) would keep that channel open after the audio thread ends, and the
        // engine's event forwarder waits for it to close when it quits.
        let mixer = commands.clone();
        sink.watch_volume(Arc::new(move |v| {
            let _ = mixer.send(Command::MixerVolume(v));
        }));
        let (events_tx, events) = crossbeam_channel::unbounded();
        let position = Arc::new(AtomicU64::new(0f64.to_bits()));
        let readers = Arc::new(Mutex::new(Readers::default()));
        let gap_us = Arc::new(AtomicU64::new(NO_GAP));
        let advanced = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            sink,
            inbox,
            events: events_tx,
            position: position.clone(),
            readers: readers.clone(),
            gap_us: gap_us.clone(),
            advanced: advanced.clone(),
            track: None,
            next: None,
            outgoing: None,
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
            readers,
            gap_us,
            advanced,
            preloads: AtomicU64::new(0),
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
    /// starts it. Drops any preloaded track too.
    pub fn load(
        &self,
        reader: TrackReader,
        mime: &str,
        gain: f32,
        start_seconds: f64,
        length_hint: Option<f64>,
    ) {
        {
            let mut readers = lock(&self.readers);
            readers.cancel_all();
            readers.current = Some(reader.canceller());
        }
        self.send(Command::Load {
            reader,
            mime: mime.to_string(),
            gain,
            start: start_seconds,
            length_hint,
        });
    }

    /// Queues `reader` to play right after the current track, with no gap: `Advanced` with the
    /// returned id says when it did. It replaces an earlier preload. Its download is already
    /// running (the engine starts it 10 s before the current song's end, or at once for a shorter
    /// song), so it can overlap the current track's if that one is still downloading. The audio
    /// thread opens it once both downloads are finished (so opening it never waits on the network),
    /// or at the latest when the current track runs out. A preload that can't be opened is dropped
    /// (logged by code), and the current track then ends with `Ended`.
    pub fn preload(
        &self,
        reader: TrackReader,
        mime: &str,
        gain: f32,
        length_hint: Option<f64>,
    ) -> u64 {
        let id;
        {
            let mut readers = lock(&self.readers);
            // Under the lock, so a cancel never sees an id that isn't registered yet.
            id = self.preloads.fetch_add(1, Ordering::Relaxed) + 1;
            readers.cancel_next();
            readers.next = Some((id, reader.canceller()));
        }
        self.send(Command::Preload {
            id,
            reader,
            mime: mime.to_string(),
            gain,
            length_hint,
        });
        id
    }

    /// Drops the preloaded track, if it hasn't become the current one yet. Once it is heard
    /// it plays on; until then (the handover window) a seek of the old track no longer
    /// brings it back as the preload.
    pub fn cancel_preload(&self) {
        let mut readers = lock(&self.readers);
        readers.cancel_next();
        readers.cancelled_through = self.preloads.load(Ordering::Relaxed);
        drop(readers);
        self.send(Command::CancelPreload);
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

    /// Unloads the track (and any preload) and empties the output.
    pub fn stop(&self) {
        lock(&self.readers).cancel_all();
        self.send(Command::Stop);
    }

    /// Seconds into the track that the listener is hearing now: (frames written − the
    /// output's delay) / rate, from the start point of the last load or seek.
    pub fn position(&self) -> f64 {
        f64::from_bits(self.position.load(Ordering::Acquire))
    }

    /// The id of the last preload the audio thread moved on to (0 before the first): set
    /// before the new track's position is, so a `position()` that is already the preload's
    /// is always read with its id here (read the position first). It tells the engine, in
    /// the moment before it takes `Advanced`, that the position it reads is the next song's.
    pub fn advanced_to(&self) -> u64 {
        self.advanced.load(Ordering::Acquire)
    }

    /// The silence the last handover put between two tracks. Same rate: how much longer
    /// than the audio still queued at the old track's end the switch took (0 when the output
    /// never ran dry). A new rate: from the old track's last frame heard to the new one's
    /// first. `None` before the first handover.
    pub fn last_gap(&self) -> Option<Duration> {
        match self.gap_us.load(Ordering::Acquire) {
            NO_GAP => None,
            us => Some(Duration::from_micros(us)),
        }
    }

    fn send(&self, command: Command) {
        // Fails only once the thread has gone (it never exits on its own before `Quit`).
        let _ = self.commands.send(command);
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        lock(&self.readers).cancel_all();
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
    /// Draining before a handover to a track at another rate (the output reopens after).
    reopen: bool,
    ended: bool,
}

/// The preloaded track.
struct Next {
    id: u64,
    gain: f32,
    cancel: ReaderCancel,
    open: NextOpen,
}

enum NextOpen {
    /// Not opened yet: opening reads the headers, which waits for them to download.
    Waiting {
        reader: TrackReader,
        mime: String,
        length_hint: Option<f64>,
    },
    // Boxed: a decoder is large, and a preload waits most of a song unopened.
    Ready(Box<Decoder>),
}

/// The track just handed over from, until the new one's first frame is heard: until then
/// the listener still hears the old one, so the position is still the old one's (and a seek
/// still means the old one).
struct Outgoing {
    track: Track,
    /// The new track's preload id, for `Advanced`.
    id: u64,
    /// The output was opened again at the new track's rate: nothing of the old track is left
    /// in it.
    reopened: bool,
    gap: Gap,
}

/// Measuring the handover's gap.
enum Gap {
    /// Same rate: when the old track ran out, and how much of it the output still held then.
    /// The gap is whatever the switch took beyond that, measured at the new track's first
    /// write.
    Same {
        eof: Instant,
        buffered: Duration,
    },
    /// New rate: when the old track's last frame was heard. The gap ends when the new track's
    /// first frame is heard.
    Reopen {
        drained: Instant,
    },
    Counted(Duration),
}

struct Worker {
    sink: Box<dyn Sink>,
    inbox: Receiver<Command>,
    events: Sender<AudioEvent>,
    position: Arc<AtomicU64>,
    readers: Arc<Mutex<Readers>>,
    gap_us: Arc<AtomicU64>,
    advanced: Arc<AtomicU64>,
    track: Option<Track>,
    next: Option<Next>,
    outgoing: Option<Outgoing>,
    /// The gain-scaled copy of a packet's frames, reused.
    scratch: Vec<f32>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let busy = self.track.as_ref().filter(|t| t.playing && !t.ended);
            let command = match busy {
                // Playing: take any waiting command, else decode the next packet. Also while
                // draining with a track of the same rate to hand over to: at once, before the
                // output runs dry.
                Some(t) if t.drain.is_none() || (self.next.is_some() && !t.reopen) => {
                    match self.inbox.try_recv() {
                        Ok(c) => Some(c),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
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
            Command::Preload {
                id,
                reader,
                mime,
                gain,
                length_hint,
            } => self.preload(id, reader, mime, gain, length_hint),
            Command::CancelPreload => {
                if let Some(next) = self.next.take() {
                    // Only its own entry: a newer preload may be on its way behind this.
                    let mut readers = lock(&self.readers);
                    if readers.next_id() == Some(next.id) {
                        readers.cancel_next();
                    }
                }
            }
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
                // Mid-handover the listener still hears the old track, and the engine still
                // calls it current: the seek is about it.
                self.roll_back();
                let Some(t) = self.track.as_mut() else {
                    return;
                };
                match t.decoder.seek(seconds) {
                    Ok(at) => {
                        t.base = at;
                        t.written = 0;
                        t.drain = None;
                        t.reopen = false;
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
            Command::MixerVolume(v) => self.emit(AudioEvent::VolumeChanged(v)),
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
            gain: clean_gain(gain),
            base,
            written: 0,
            started: false,
            playing: false,
            drain: None,
            reopen: false,
            ended: false,
        });
        self.publish();
    }

    fn preload(
        &mut self,
        id: u64,
        reader: TrackReader,
        mime: String,
        gain: f32,
        length_hint: Option<f64>,
    ) {
        // A newer preload or a cancel came after this one: its reader is already cancelled.
        if lock(&self.readers).next_id() != Some(id) {
            return;
        }
        self.next = Some(Next {
            id,
            gain: clean_gain(gain),
            cancel: reader.canceller(),
            open: NextOpen::Waiting {
                reader,
                mime,
                length_hint,
            },
        });
        self.open_next_when_ready();
    }

    /// Opens the preload once both downloads are finished: its headers are then in memory, so
    /// opening never blocks a write. (Waiting for the current song's download too keeps the
    /// open off a busy connection; it doesn't stop the preload's download, which the engine
    /// started 10 s before the end, from running alongside it.) If that hasn't happened by
    /// the current track's end, `ready_next` opens it there.
    fn open_next_when_ready(&mut self) {
        let waiting = self
            .next
            .as_ref()
            .is_some_and(|n| matches!(n.open, NextOpen::Waiting { .. }));
        if waiting
            && self
                .next
                .as_ref()
                .is_some_and(|n| n.cancel.download_finished())
            && self
                .track
                .as_ref()
                .is_none_or(|t| t.cancel.download_finished())
        {
            self.ready_next();
        }
    }

    /// Opens the preload if it is still waiting (blocking on its download). False when there
    /// is none, or it could not be opened (then it is dropped).
    fn ready_next(&mut self) -> bool {
        let Some(mut next) = self.next.take() else {
            return false;
        };
        next.open = match next.open {
            NextOpen::Ready(d) => NextOpen::Ready(d),
            NextOpen::Waiting {
                reader,
                mime,
                length_hint,
            } => match Decoder::open(reader, &mime) {
                Ok(d) => {
                    crate::trace::mark("next decoder open");
                    NextOpen::Ready(Box::new(d.with_length_hint(length_hint)))
                }
                Err(e) => {
                    if !next.cancel.is_cancelled() {
                        // The code only (ruling R6). The engine loads it the usual way when
                        // the current track ends, and reports the error then if it is real.
                        eprintln!("ytmfast: could not open the next track ({})", e.code());
                    }
                    let mut readers = lock(&self.readers);
                    if readers.next_id() == Some(next.id) {
                        readers.cancel_next();
                    }
                    return false;
                }
            },
        };
        self.next = Some(next);
        true
    }

    /// Drops the preload, and cancels its reader if it is still the registered one.
    fn drop_next(&mut self) {
        if let Some(next) = self.next.take() {
            let mut readers = lock(&self.readers);
            if readers.next_id() == Some(next.id) {
                readers.cancel_next();
            }
        }
    }

    /// Drops the track and any preload; the output is emptied and paused (an idle stream
    /// would keep the audio graph, and the CPU, awake).
    fn unload(&mut self) {
        self.drop_track();
        self.position.store(0f64.to_bits(), Ordering::Release);
    }

    /// `unload`, keeping the last position: after a failure it says where the song stopped.
    fn drop_track(&mut self) {
        self.track = None;
        self.outgoing = None;
        self.drop_next();
        self.sink.flush();
        self.sink.pause(true);
    }

    /// One unit of work while playing: decode and write a packet, check the drain, or hand
    /// over to the preloaded track.
    fn step(&mut self) {
        self.open_next_when_ready();
        let Some(t) = self.track.as_mut() else {
            return;
        };
        if let Some(since) = t.drain {
            return self.drain(since);
        }
        let frames = match t.decoder.next_frames() {
            Ok(Some(f)) => f,
            Ok(None) => return self.at_end(),
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
        if t.written == 0
            && let Some(o) = self.outgoing.as_mut()
            && let Gap::Same { eof, buffered } = o.gap
        {
            // The new track's first frames: the output still plays the old track's last ones
            // unless the switch took longer than they last.
            let gap = eof.elapsed().saturating_sub(buffered);
            o.gap = Gap::Counted(gap);
            self.gap_us.store(gap.as_micros() as u64, Ordering::Release);
        }
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

    /// The current track's decoder ran out: hand over to the preload at once (same rate), or
    /// drain first (a new rate, or nothing preloaded).
    fn at_end(&mut self) {
        let eof = Instant::now();
        let buffered = self.sink.delay_frames();
        let mut reopen = false;
        if self.ready_next() {
            if self.next_rate() == self.track.as_ref().map(|t| t.rate) {
                let buffered = Duration::from_secs_f64(buffered as f64 / self.rate());
                return self.hand_over(Gap::Same { eof, buffered });
            }
            reopen = true;
        }
        if let Some(t) = self.track.as_mut() {
            t.drain = Some(eof);
            t.reopen = reopen;
        }
        self.publish();
    }

    /// Every frame is written: wait for the output to play them, then end, or hand over.
    fn drain(&mut self, since: Instant) {
        let rate = self.rate();
        let reopen = self.track.as_ref().is_some_and(|t| t.reopen);
        // A preload that came in while draining: at the same rate, it follows at once.
        if !reopen && self.ready_next() {
            if self.next_rate() == Some(rate) {
                let buffered = self.sink.delay_frames() as f64 / rate;
                return self.hand_over(Gap::Same {
                    eof: Instant::now(),
                    buffered: Duration::from_secs_f64(buffered),
                });
            }
            if let Some(t) = self.track.as_mut() {
                t.reopen = true;
            }
        }
        let delay = self.sink.delay_frames();
        let deadline = since + Duration::from_secs_f64(delay as f64 / rate) + DRAIN_GRACE;
        if delay != 0 && Instant::now() < deadline {
            return self.publish();
        }
        // All of the old track was heard: a preload at another rate (or one that replaced it
        // while draining) gets its own output, and the gap starts now.
        if self.ready_next() {
            return self.hand_over(Gap::Reopen {
                drained: Instant::now(),
            });
        }
        let Some(t) = self.track.as_mut() else {
            return;
        };
        t.ended = true;
        t.playing = false;
        self.sink.pause(true);
        self.publish();
        // A preloaded track shorter than the output's delay: heard in full by now.
        self.finish_advance();
        self.emit(AudioEvent::Ended);
    }

    /// The preload (opened) becomes the current track. Its frames follow the old track's in
    /// the same output (same rate), or in a reopened one (`Gap::Reopen`).
    fn hand_over(&mut self, gap: Gap) {
        let Some(next) = self.next.take() else {
            return;
        };
        let NextOpen::Ready(decoder) = next.open else {
            return;
        };
        {
            let mut readers = lock(&self.readers);
            // A newer preload or a cancel is on its way: this one is no longer wanted (its
            // reader is already cancelled), and the old track just ends.
            if readers.next_id() != Some(next.id) {
                return;
            }
            readers.current = readers.next.take().map(|(_, c)| c);
        }
        // A track shorter than the output's delay, still waiting to be heard: it is, as of
        // the frames that follow it now.
        self.finish_advance();
        let Some(old) = self.track.take() else {
            return;
        };
        let reopened = matches!(gap, Gap::Reopen { .. });
        let rate = decoder.rate();
        let old_rate = old.rate;
        self.track = Some(Track {
            rate: f64::from(rate),
            decoder: *decoder,
            cancel: next.cancel,
            gain: next.gain,
            base: 0.0,
            written: 0,
            // It plays on in the old track's state (a pause while draining included).
            started: true,
            playing: old.playing,
            drain: None,
            reopen: false,
            ended: false,
        });
        self.outgoing = Some(Outgoing {
            track: old,
            id: next.id,
            reopened,
            gap,
        });
        if reopened {
            crate::trace::mark("output reopen for a new rate");
            if let Err(e) = self.sink.open(rate, 2) {
                // The old track was heard to its end; the new one is current, and failed
                // before a gap could be measured.
                if let Some(o) = self.outgoing.as_mut() {
                    o.gap = Gap::Counted(Duration::ZERO);
                }
                return self.fail(e);
            }
            eprintln!(
                "ytmfast: the next song is at {rate} Hz, not {old_rate} Hz: reopening the output"
            );
        }
        self.publish();
    }

    /// Mid-handover, a seek of the old track: it becomes current again, and the new track goes
    /// back to waiting, from its start (none of it was heard).
    fn roll_back(&mut self) {
        let Some(old) = self.outgoing.take() else {
            return;
        };
        let Some(mut new) = self.track.take() else {
            return;
        };
        // Before the lock: a seek may read, and the engine's `preload` and `cancel_preload`
        // wait on that lock.
        let rewound = new.decoder.seek(0.0).is_ok();
        let keep = {
            let mut readers = lock(&self.readers);
            readers.current = Some(old.track.cancel.clone());
            // A newer preload, or a cancel (even one sent in the window, when this track was
            // already current): drop this one.
            let wanted = rewound && readers.next.is_none() && old.id > readers.cancelled_through;
            if wanted {
                readers.next = Some((old.id, new.cancel.clone()));
            }
            wanted
        };
        if keep {
            self.next = Some(Next {
                id: old.id,
                gain: new.gain,
                cancel: new.cancel,
                open: NextOpen::Ready(Box::new(new.decoder)),
            });
        } else {
            new.cancel.cancel();
        }
        let rate = old.track.rate;
        self.track = Some(old.track);
        if old.reopened
            && let Err(e) = self.sink.open(rate as u32, 2)
        {
            self.fail(e);
        }
    }

    /// The handover is heard (or has to count as heard): the position is the new track's from
    /// now, and then the engine is told (so a position read on `Advanced` is the new one's).
    fn finish_advance(&mut self) {
        let Some(o) = self.outgoing.take() else {
            return;
        };
        // Before the position: whoever reads the new track's position sees this id too.
        self.advanced.store(o.id, Ordering::Release);
        self.publish();
        let gap = match o.gap {
            Gap::Reopen { drained } => {
                let gap = drained.elapsed();
                self.gap_us.store(gap.as_micros() as u64, Ordering::Release);
                eprintln!(
                    "ytmfast: {:.1} ms of silence between songs (sample rate change)",
                    gap.as_secs_f64() * 1000.0
                );
                gap
            }
            Gap::Counted(gap) => gap,
            Gap::Same { .. } => Duration::ZERO,
        };
        if !o.reopened && gap > GAP_BUDGET {
            eprintln!(
                "ytmfast: {:.1} ms of silence between songs (over the {} ms budget)",
                gap.as_secs_f64() * 1000.0,
                GAP_BUDGET.as_millis()
            );
        }
        self.emit(AudioEvent::Advanced(o.id));
    }

    /// The opened preload's rate.
    fn next_rate(&self) -> Option<f64> {
        match &self.next.as_ref()?.open {
            NextOpen::Ready(d) => Some(f64::from(d.rate())),
            NextOpen::Waiting { .. } => None,
        }
    }

    /// The current track's rate (48 kHz, never used without a track).
    fn rate(&self) -> f64 {
        self.track.as_ref().map_or(48_000.0, |t| t.rate)
    }

    /// Recomputes the position from what was written and the output's delay. Mid-handover it
    /// is still the old track's, until the new track's first frame is heard.
    fn publish(&mut self) {
        let delay = self.sink.delay_frames();
        if self.outgoing.is_some() && self.track.as_ref().is_some_and(|t| t.written > delay) {
            // Publishes the new track's position itself, before it says so.
            return self.finish_advance();
        }
        let Some(t) = &self.track else {
            return;
        };
        let seconds = match &self.outgoing {
            Some(o) => {
                // What of the old track is still queued ahead of the new one's frames.
                let unheard = if o.reopened { 0 } else { delay - t.written };
                let old = &o.track;
                old.base + old.written.saturating_sub(unheard) as f64 / old.rate
            }
            None => t.base + t.written.saturating_sub(delay) as f64 / t.rate,
        };
        self.position.store(seconds.to_bits(), Ordering::Release);
    }

    /// A failed track: report it and go idle. The position stays where the song stopped, for
    /// the engine's status and its replay after an output restart.
    fn fail(&mut self, e: Error) {
        // Mid-handover, the failure is the new track's (or the output's, under it): the
        // engine must know it is current before it hears what went wrong.
        self.finish_advance();
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

/// A loudness gain the samples can take: within 0..=1, and 1 when it isn't a number.
fn clean_gain(gain: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(0.0, 1.0)
    } else {
        1.0
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
    fn a_preload_still_downloading_is_not_opened_mid_song() {
        // Opening reads the headers: a preload whose bytes haven't come must not block the
        // song that plays (it would stop writing, and the output would run dry).
        let (p, events, _) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 0.0, None);
        p.preload(stalled("sine440_48k.webm", 100), OPUS_MIME, 1.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(400));
        let at = p.position();
        assert!(at > 0.3, "the song plays on: {at}");
        // At the song's end it has to be opened, and waits for its bytes; a cancel frees it,
        // and the song just ends.
        std::thread::sleep(Duration::from_millis(1700));
        p.cancel_preload();
        assert_eq!(next_event(&events), AudioEvent::Ended);
    }

    #[test]
    fn a_seek_mid_handover_is_about_the_old_track() {
        // The old track's end is decoded (and the handover made) up to 200 ms before it is
        // heard. Until the new track's first frame is heard, the listener hears the old one,
        // so a seek then goes back into the old one, and the new one waits again.
        let (p, events, _) = player(NullSink::realtime());
        // 1 s from the end (as close as a seek goes).
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 1.0, None);
        let id = p.preload(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        // Inside the last 200 ms: handed over, not heard yet. (Past 1.8 s, the writer, 200 ms
        // ahead, has reached the end, and the handover is made there and then.)
        std::thread::sleep(Duration::from_millis(850));
        assert!(events.try_recv().is_err(), "not heard yet");
        // Not moved on yet: the position is still the old track's.
        assert_eq!(p.advanced_to(), 0);
        let before = p.position();
        assert!(before > 1.8, "the old track's clock: {before}");
        let t = std::time::Instant::now();
        p.seek(0.5);
        assert_eq!(next_event(&events), AudioEvent::Advanced(id));
        assert_eq!(p.advanced_to(), id, "the position is the preload's now");
        let after = t.elapsed().as_secs_f64();
        // 1.5 s more of the old track first, not the 0.1 s that was left before the seek.
        assert!(
            after > 1.4 && after < 1.6,
            "Advanced {after} s after the seek"
        );
        assert!(p.position() < 0.05, "{}", p.position());
        p.stop();
    }

    #[test]
    fn a_preload_cancelled_mid_handover_stays_cancelled_after_a_seek() {
        // In the handover window the new track is already current to the audio thread, so a
        // cancel finds no preload to drop. A seek then rolls the handover back: the new track
        // must not come back as the preload the engine dropped (it would play after the old
        // one, out of the queue's order).
        let (p, events, _) = player(NullSink::realtime());
        p.load(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, 1.0, None);
        p.preload(fixture("sine440_48k.webm"), OPUS_MIME, 1.0, None);
        p.play();
        assert_eq!(next_event(&events), AudioEvent::Started);
        std::thread::sleep(Duration::from_millis(850));
        assert!(events.try_recv().is_err(), "not heard yet");
        p.cancel_preload();
        p.seek(0.5);
        // The old track plays to its end, and nothing follows it.
        assert_eq!(next_event(&events), AudioEvent::Ended);
        p.stop();
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
