//! The real output: a PipeWire playback stream.
//!
//! PipeWire runs on its own thread (a `pw` main loop). The stream's `process` callback runs on
//! PipeWire's real-time data thread and only copies from a lock-free ring (`rtrb`, 200 ms)
//! into PipeWire's buffer: no locks, no allocation, no syscalls. Everything else (pause,
//! volume, flush) goes to the main loop as a message, because those calls belong on it.
//!
//! The audio thread writes into the ring and, when it is full, sleeps for as long as the
//! missing room takes to play: no busy wait, and decoding stays at most 200 ms ahead.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamRc, StreamState};

use crate::audio::sink::{LostNotify, Sink};
use crate::error::Error;

/// The ring between the audio thread and PipeWire, in seconds of audio.
const RING_SECS: f64 = 0.2;

/// Interleaved stereo f32: bytes per frame.
const STRIDE: usize = 2 * std::mem::size_of::<f32>();

/// A writer that waited this long without PipeWire taking a single buffer gives up: the
/// stream isn't linked to anything, or the daemon went away.
const STALL: Duration = Duration::from_secs(5);

/// How long `open` waits for the PipeWire thread to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `close` waits for the PipeWire thread to end before leaving it behind.
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// The stream's identity, as mixers and per-app rules see it (Global Constraints).
const APP_NAME: &str = "YouTube Music";
const NODE_NAME: &str = "ytmfast";

/// State shared with the real-time callback: atomics only.
#[derive(Default)]
struct Shared {
    /// Bumped by the writer to ask for the ring to be emptied; `flush_done` echoes it once the
    /// callback has (only the consumer side may pop).
    flush_request: AtomicU64,
    flush_done: AtomicU64,
    /// Frames of music between the stream and the speaker as of the last cycle: the device and
    /// graph delay, buffers queued in PipeWire, and the resampler's backlog, less any silence
    /// sent after the music.
    pipeline_delay: AtomicU64,
    /// Cycles where the ring ran dry mid-play (one per dropout; the end of a track counts one).
    underruns: AtomicU64,
    /// Bumped every cycle, so a waiting writer can tell a stalled output from a full one.
    cycles: AtomicU64,
    /// When the last cycle ran (CLOCK_MONOTONIC ns) and how many frames it took: a writer
    /// waiting for room sleeps until the cycle that frees it, so it wakes once per write
    /// instead of polling while PipeWire drains the ring in quantum-sized bursts.
    cycle_ns: AtomicU64,
    cycle_frames: AtomicU64,
    /// Set when the writer queues audio, cleared when the ring runs dry.
    primed: AtomicBool,
    /// The stream is gone for good: it errored, or the sound server went away (see
    /// `stream_gone` and `core_gone`).
    failed: AtomicBool,
    /// Set by `close` before it stops the stream: the Unconnected that follows is ours, not
    /// a loss to report.
    closing: AtomicBool,
}

/// Messages to the PipeWire main loop.
enum Control {
    Active(bool),
    /// PipeWire channel volume (already mapped from the slider value).
    Volume(f32),
    /// Drop what PipeWire has queued (the ring is emptied by the callback).
    Flush,
    Quit,
}

/// One connected stream at one rate.
struct Output {
    rate: u32,
    producer: rtrb::Producer<f32>,
    capacity: usize,
    shared: Arc<Shared>,
    control: pw::channel::Sender<Control>,
    thread: Option<JoinHandle<()>>,
    /// Disconnected once the PipeWire thread has ended (its sender drops with the thread),
    /// so `close` can wait for that with a timeout, which a join can't.
    done: Option<std::sync::mpsc::Receiver<()>>,
}

/// How an `Output` is made: `Output::connect`, or a stand-in in tests (which must never
/// reach the user's PipeWire).
type Connect =
    fn(rate: u32, volume: f32, paused: bool, notify: Option<LostNotify>) -> Result<Output, Error>;

/// PipeWire playback. Connects on the first `open`; a new rate reconnects (the rare switch
/// between 48 kHz Opus and 44.1 kHz AAC), and so does a stream that failed.
pub struct PipeWireSink {
    out: Option<Output>,
    connect: Connect,
    /// The slider value (0..=1), re-applied to each new stream.
    volume: f32,
    paused: bool,
    /// Passed to each new stream: told when it is lost (`Sink::watch_lost`).
    notify: Option<LostNotify>,
}

impl PipeWireSink {
    pub fn new() -> PipeWireSink {
        PipeWireSink {
            out: None,
            connect: Output::connect,
            volume: 1.0,
            paused: false,
            notify: None,
        }
    }

    /// Dropouts so far on the current stream.
    pub fn underruns(&self) -> u64 {
        self.out
            .as_ref()
            .map_or(0, |o| o.shared.underruns.load(Ordering::Relaxed))
    }

    fn send(&self, c: Control) {
        if let Some(o) = &self.out {
            let _ = o.control.send(c);
        }
    }

    /// Waits until the callback has emptied the ring for the last flush, so what is written
    /// next isn't thrown away with the old audio.
    fn wait_flushed(out: &Output) -> Result<(), Error> {
        let mut watch = StallWatch::new(&out.shared);
        while out.shared.flush_done.load(Ordering::Acquire)
            != out.shared.flush_request.load(Ordering::Acquire)
        {
            // A dead stream never runs the flush: say why now, not after the stall timeout.
            out.check_alive()?;
            watch.check()?;
            // The next cycle does the flush.
            std::thread::sleep(out.wait_for_room(1));
        }
        Ok(())
    }
}

impl Default for PipeWireSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PipeWireSink {
    fn drop(&mut self) {
        if let Some(out) = self.out.take() {
            out.close();
        }
    }
}

/// The slider value to PipeWire's channel volume. Channel volumes are linear gain, and mixers
/// (wpctl, pavucontrol, desktop volume popups) show their cube root; cubing makes the slider
/// and the mixer show the same number, and makes the slider feel even across its range.
fn channel_volume(v: f32) -> f32 {
    v * v * v
}

impl Sink for PipeWireSink {
    fn open(&mut self, rate: u32, channels: u16) -> Result<(), Error> {
        if channels != 2 {
            return Err(Error::Internal("the output is stereo only".into()));
        }
        // A failed stream stays failed (PipeWire restarted, or the stream was unlinked for
        // good): reusing it would fail every later song too, so it is replaced.
        if self
            .out
            .as_ref()
            .is_some_and(|o| o.rate == rate && !o.shared.failed.load(Ordering::Acquire))
        {
            return Ok(());
        }
        if let Some(old) = self.out.take() {
            old.close();
        }
        self.out = Some((self.connect)(
            rate,
            channel_volume(self.volume),
            self.paused,
            self.notify.clone(),
        )?);
        Ok(())
    }

    fn write(&mut self, mut frames: &[f32]) -> Result<(), Error> {
        let out = self
            .out
            .as_mut()
            .ok_or_else(|| Error::Internal("the output is not open".into()))?;
        Self::wait_flushed(out)?;
        let mut watch = StallWatch::new(&out.shared);
        // Whole frames only, so the ring always holds whole frames.
        frames = &frames[..frames.len() & !1];
        while !frames.is_empty() {
            out.check_alive()?;
            let room = (out.producer.slots() & !1).min(frames.len());
            if room > 0 {
                let (taken, _) = out.producer.push_partial_slice(&frames[..room]);
                let rest = &frames[taken.len()..];
                if !taken.is_empty() {
                    out.shared.primed.store(true, Ordering::Release);
                    watch = StallWatch::new(&out.shared);
                }
                frames = rest;
                continue;
            }
            watch.check()?;
            // Full: sleep until the cycle that frees room for the rest.
            let missing = frames.len().min(out.capacity) / 2;
            std::thread::sleep(out.wait_for_room(missing));
        }
        Ok(())
    }

    fn pause(&mut self, paused: bool) {
        self.paused = paused;
        self.send(Control::Active(!paused));
    }

    fn flush(&mut self) {
        if let Some(out) = &self.out {
            out.shared.flush_request.fetch_add(1, Ordering::AcqRel);
        }
        self.send(Control::Flush);
    }

    fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        self.send(Control::Volume(channel_volume(self.volume)));
    }

    fn delay_frames(&self) -> u64 {
        let Some(out) = &self.out else {
            return 0;
        };
        let ring = (out.capacity - out.producer.slots()) / 2;
        out.shared.pipeline_delay.load(Ordering::Acquire) + ring as u64
    }

    fn lost(&self) -> bool {
        self.out
            .as_ref()
            .is_some_and(|o| o.shared.failed.load(Ordering::Acquire))
    }

    fn watch_lost(&mut self, notify: LostNotify) {
        self.notify = Some(notify);
    }
}

/// Marks the output gone, and tells the watcher the first time, unless `close` is stopping
/// it on purpose. Runs on the PipeWire thread, from its state and core error callbacks.
fn mark_gone(shared: &Shared, notify: &Option<LostNotify>) {
    let was_failed = shared.failed.swap(true, Ordering::AcqRel);
    if !was_failed
        && !shared.closing.load(Ordering::Acquire)
        && let Some(notify) = notify
    {
        notify();
    }
}

/// The stream states that mean it is gone for good. A sound server restart closes the core's
/// socket, and libpipewire then moves every stream on it to Unconnected; a stream PipeWire
/// refused or broke ends in Error. Neither comes back: only a new stream plays again.
fn stream_gone(state: &StreamState) -> bool {
    matches!(state, StreamState::Unconnected | StreamState::Error(_))
}

/// Whether a core `error` event about object `id` means the connection itself is gone. Errors
/// about the core object (EPIPE when the daemon goes away) are the connection's, whatever the
/// code; one about another object is that object's, and the stream reports its own through
/// its state.
fn core_gone(id: u32) -> bool {
    id == pw::core::PW_ID_CORE
}

/// A wait for a cycle that is late, or before the first one: short, but no spin.
const RECHECK: Duration = Duration::from_millis(5);

/// Wakes this long after the cycle's expected start, so its callback has run.
const CYCLE_MARGIN_NS: u64 = 300_000;

/// How long a writer needing room for `frames` sleeps: until the callback has run often
/// enough to free it, from when the last cycle ran (`last_ns`) and its size. PipeWire frees
/// room a quantum at a time, so this is one wake per write where a deficit-sized sleep woke
/// several times per quantum.
fn cycle_wait(last_ns: u64, now_ns: u64, cycle_frames: u64, rate: u32, frames: usize) -> Duration {
    if last_ns == 0 || cycle_frames == 0 || rate == 0 {
        return RECHECK;
    }
    let period_ns = cycle_frames * 1_000_000_000 / u64::from(rate);
    let cycles = (frames as u64).div_ceil(cycle_frames).max(1);
    let wake = last_ns + cycles * period_ns + CYCLE_MARGIN_NS;
    if wake <= now_ns {
        return RECHECK;
    }
    // At most the ring's length: room is always free by then.
    Duration::from_nanos(wake - now_ns).min(Duration::from_secs_f64(RING_SECS))
}

/// CLOCK_MONOTONIC in ns: the clock `pw_time.now` uses.
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec to write into; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Notices a writer that is waiting on a stream PipeWire no longer runs.
struct StallWatch<'a> {
    shared: &'a Shared,
    cycles: u64,
    since: Instant,
}

impl<'a> StallWatch<'a> {
    fn new(shared: &'a Shared) -> Self {
        StallWatch {
            shared,
            cycles: shared.cycles.load(Ordering::Relaxed),
            since: Instant::now(),
        }
    }

    fn check(&mut self) -> Result<(), Error> {
        let now = self.shared.cycles.load(Ordering::Relaxed);
        if now != self.cycles {
            self.cycles = now;
            self.since = Instant::now();
        } else if self.since.elapsed() >= STALL {
            return Err(Error::Internal("the audio output stalled".into()));
        }
        Ok(())
    }
}

impl Output {
    fn connect(
        rate: u32,
        volume: f32,
        paused: bool,
        notify: Option<LostNotify>,
    ) -> Result<Output, Error> {
        let capacity = ((RING_SECS * f64::from(rate)) as usize) * 2;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let shared = Arc::new(Shared::default());
        let (control, inbox) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ytmfast-pipewire".into())
            .spawn(move || {
                // Dropped when this thread ends, however it ends.
                let _done = done_tx;
                let setup = Setup {
                    rate,
                    volume,
                    paused,
                    notify,
                    consumer,
                    shared: thread_shared,
                    inbox,
                };
                if let Err(e) = run(setup, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|_| Error::Internal("could not start the PipeWire thread".into()))?;
        let out = Output {
            rate,
            producer,
            capacity,
            shared,
            control,
            thread: Some(thread),
            done: Some(done_rx),
        };
        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(out),
            Ok(Err(e)) => {
                out.close();
                Err(e)
            }
            Err(_) => {
                out.close();
                Err(Error::Internal("PipeWire did not answer".into()))
            }
        }
    }

    /// `OutputRestarted` once the stream is gone (see `stream_gone`, `core_gone`).
    fn check_alive(&self) -> Result<(), Error> {
        if self.shared.failed.load(Ordering::Acquire) {
            return Err(Error::OutputRestarted);
        }
        Ok(())
    }

    /// How long until the callback has freed room for `frames` (see `cycle_wait`).
    fn wait_for_room(&self, frames: usize) -> Duration {
        cycle_wait(
            self.shared.cycle_ns.load(Ordering::Acquire),
            monotonic_ns(),
            self.shared.cycle_frames.load(Ordering::Acquire),
            self.rate,
            frames,
        )
    }

    /// Stops the PipeWire thread. It is joined only once it has ended, and left behind after
    /// `CLOSE_WAIT` if it hasn't: a thread stuck inside libpipewire (it never reaches its main
    /// loop, so never sees `Quit`) must not hold the audio thread, and every song after, for
    /// ever. That is also the one wait in `open` (after a failed or late connect) that had no
    /// bound.
    fn close(mut self) {
        self.shared.closing.store(true, Ordering::Release);
        let _ = self.control.send(Control::Quit);
        let Some(t) = self.thread.take() else {
            return;
        };
        let stuck = self.done.take().is_some_and(|done| {
            matches!(
                done.recv_timeout(CLOSE_WAIT),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            )
        });
        if stuck {
            eprintln!("ytmfast: the PipeWire thread did not stop; leaving it behind");
        } else {
            let _ = t.join();
        }
    }
}

struct Setup {
    rate: u32,
    volume: f32,
    paused: bool,
    notify: Option<LostNotify>,
    consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    inbox: pw::channel::Receiver<Control>,
}

/// The real-time callback's own state.
struct Rt {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    rate: u32,
    flush_seen: u64,
    /// Frames of silence sent since the last real frame. The pipeline's delay is the same
    /// whether it carries music or silence, so once this much silence has followed the music,
    /// the music has all been heard: the delay counts only what is left of the music.
    trailing_silence: u64,
}

fn no_pipewire<E>(_: E) -> Error {
    Error::Internal("could not connect to PipeWire".into())
}

/// The PipeWire thread: connects, reports ready, and runs the main loop until `Quit`.
fn run(setup: Setup, ready: &std::sync::mpsc::SyncSender<Result<(), Error>>) -> Result<(), Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(no_pipewire)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(no_pipewire)?;
    let core = context.connect_rc(None).map_err(no_pipewire)?;
    // The connection's own errors. libpipewire also moves the stream to Unconnected when the
    // daemon goes away, but this does not depend on that: any fatal error on the core marks
    // the output gone, so the next `open` connects again.
    //
    // Drop order matters here. The listener's hook sits in a list inside the pw_core, and
    // dropping the listener unlinks it by writing into that list. So the core must outlive the
    // listener: `core` is declared before `_core` (locals drop in reverse order) and the stream
    // gets a clone, never the last reference. Moving `core` into the stream let the stream's
    // drop free the pw_core first, and then `_core`'s drop wrote into freed memory on every
    // close (AddressSanitizer: heap-use-after-free in libspa's list remove).
    let core_shared = setup.shared.clone();
    let core_notify = setup.notify.clone();
    let _core = core
        .add_listener_local()
        .error(move |id, _seq, _res, _message| {
            if core_gone(id) {
                mark_gone(&core_shared, &core_notify);
            }
        })
        .register();
    let stream = StreamRc::new(
        core.clone(),
        APP_NAME,
        properties! {
            *pw::keys::APP_NAME => APP_NAME,
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => APP_NAME,
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
        },
    )
    .map_err(no_pipewire)?;

    // Two listeners: `process` runs on PipeWire's data thread and owns the ring's consumer;
    // `state_changed` runs on this thread. One listener's user data would be shared between
    // the two threads.
    let rt = Rt {
        consumer: setup.consumer,
        shared: setup.shared.clone(),
        rate: setup.rate,
        flush_seen: 0,
        trailing_silence: 0,
    };
    let _process = stream
        .add_local_listener_with_user_data(rt)
        .process(process)
        .register()
        .map_err(no_pipewire)?;

    let volume = Rc::new(RefCell::new(VolumeGate::new(setup.volume)));
    let state_volume = volume.clone();
    let state_shared = setup.shared.clone();
    let _state = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |s, _, _, new| {
            if stream_gone(&new) {
                mark_gone(&state_shared, &setup.notify);
                return;
            }
            if let Some(v) = state_volume.borrow_mut().state(&new) {
                set_volume(s, v);
            }
            if new == StreamState::Streaming {
                crate::trace::mark("stream running");
            }
        })
        .register()
        .map_err(no_pipewire)?;

    let format = format_pod(setup.rate)?;
    let mut params = [spa::pod::Pod::from_bytes(&format)
        .ok_or_else(|| Error::Internal("bad PipeWire format".into()))?];
    let mut flags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
    if setup.paused {
        flags |= StreamFlags::INACTIVE;
    }
    stream
        .connect(spa::utils::Direction::Output, None, flags, &mut params)
        .map_err(no_pipewire)?;

    let control_stream = stream.clone();
    let control_loop = mainloop.clone();
    let _inbox = setup.inbox.attach(mainloop.loop_(), move |c| match c {
        Control::Active(on) => {
            let _ = control_stream.set_active(on);
        }
        Control::Volume(v) => {
            let v = volume.borrow_mut().change(v);
            set_volume(&control_stream, v);
        }
        Control::Flush => {
            let _ = control_stream.flush(false);
        }
        Control::Quit => control_loop.quit(),
    });

    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// When the stream's volume goes to PipeWire: on every change of ours, and on state changes
/// only until the stream first runs.
///
/// Until then a control set on the stream doesn't stay: it is connected (Paused) before it is
/// linked, and linking sets up its ports again, which resets the volume to 1.0 (seen against
/// a private daemon: tests/gapless.rs). So the volume is sent on every state up to the first
/// Streaming, which comes after the link. After that, a state change is a pause or a resume:
/// sending the volume again there would undo a change the user made in a mixer since (step 1
/// parked Minor 5).
struct VolumeGate {
    /// PipeWire channel volume (already mapped from the slider value).
    value: f32,
    /// The stream has run once: its controls stay as set from now on.
    settled: bool,
}

impl VolumeGate {
    fn new(value: f32) -> VolumeGate {
        VolumeGate {
            value,
            settled: false,
        }
    }

    /// A new volume of ours: always sent.
    fn change(&mut self, v: f32) -> f32 {
        self.value = v;
        v
    }

    /// The stream's new state: what to send now, if anything.
    fn state(&mut self, state: &StreamState) -> Option<f32> {
        if self.settled || !matches!(state, StreamState::Paused | StreamState::Streaming) {
            return None;
        }
        self.settled = *state == StreamState::Streaming;
        Some(self.value)
    }
}

fn set_volume(stream: &pw::stream::Stream, v: f32) {
    let _ = stream.set_control(spa::sys::SPA_PROP_channelVolumes, &[v, v]);
}

/// The stream's one format: interleaved F32LE stereo at `rate`, front left and right.
fn format_pod(rate: u32) -> Result<Vec<u8>, Error> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Value::Object(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| Error::Internal("bad PipeWire format".into()))
}

/// The real-time callback: ring to PipeWire's buffer, silence for whatever the ring lacks.
fn process(stream: &pw::stream::Stream, rt: &mut Rt) {
    let request = rt.shared.flush_request.load(Ordering::Acquire);
    if request != rt.flush_seen {
        let stale = rt.consumer.slots();
        if let Ok(chunk) = rt.consumer.read_chunk(stale) {
            chunk.commit_all();
        }
        rt.flush_seen = request;
        rt.shared.primed.store(false, Ordering::Release);
        rt.shared.flush_done.store(request, Ordering::Release);
    }

    // SAFETY: `dequeue_raw_buffer` returns a buffer PipeWire lends us until we queue it back
    // (or null). Every pointer inside is checked for null before use; the data slice is
    // limited to the buffer's `maxsize`; the buffer is queued back exactly once.
    unsafe {
        let buf = stream.dequeue_raw_buffer();
        if buf.is_null() {
            return;
        }
        let spa_buf = (*buf).buffer;
        if !spa_buf.is_null() && (*spa_buf).n_datas > 0 && !(*spa_buf).datas.is_null() {
            let data = &mut *(*spa_buf).datas;
            if !data.data.is_null() && !data.chunk.is_null() {
                let room = data.maxsize as usize / STRIDE;
                let wanted = match (*buf).requested as usize {
                    0 => room,
                    r => r.min(room),
                };
                rt.shared
                    .cycle_frames
                    .store(wanted as u64, Ordering::Release);
                let bytes = std::slice::from_raw_parts_mut(data.data.cast::<u8>(), wanted * STRIDE);
                let filled = fill(&mut rt.consumer, bytes);
                if filled < wanted && rt.shared.primed.swap(false, Ordering::AcqRel) {
                    rt.shared.underruns.fetch_add(1, Ordering::Relaxed);
                }
                rt.trailing_silence = if filled > 0 {
                    (wanted - filled) as u64
                } else {
                    rt.trailing_silence + wanted as u64
                };
                let chunk = &mut *data.chunk;
                chunk.offset = 0;
                chunk.stride = STRIDE as i32;
                chunk.size = (wanted * STRIDE) as u32;
                // In frames: PipeWire sums these into `pw_time.queued`.
                (*buf).size = wanted as u64;
            }
        }
        stream.queue_raw_buffer(buf);
    }

    if let Ok(t) = stream.time() {
        let graph = t.rate();
        let device = if graph.denom > 0 && t.delay() > 0 {
            t.delay() as u64 * u64::from(graph.num) * u64::from(rt.rate) / u64::from(graph.denom)
        } else {
            0
        };
        let delay = (device + t.queued() + t.buffered()).saturating_sub(rt.trailing_silence);
        rt.shared.pipeline_delay.store(delay, Ordering::Release);
    }
    // The cycle's time from our own clock read (a vDSO call, no syscall): `pw_time.now` can
    // be stale when the stream's time isn't updated every cycle.
    rt.shared.cycle_ns.store(monotonic_ns(), Ordering::Release);
    rt.shared.cycles.fetch_add(1, Ordering::Relaxed);
}

/// Copies whole frames from the ring into `out` (F32LE bytes) and zeroes the rest; the number
/// of frames that came from the ring.
fn fill(consumer: &mut rtrb::Consumer<f32>, out: &mut [u8]) -> usize {
    let want = out.len() / 4;
    let take = consumer.slots().min(want) & !1;
    let mut written = 0;
    if let Ok(chunk) = consumer.read_chunk(take) {
        let (a, b) = chunk.as_slices();
        for (dst, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(a.iter().chain(b)) {
            *dst = s.to_le_bytes();
        }
        written = take;
        chunk.commit_all();
    }
    out[written * 4..].fill(0);
    written / 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn fill_copies_whole_frames_then_silence() {
        let (mut p, mut c) = rtrb::RingBuffer::new(16);
        let _ = p.push_partial_slice(&[0.5, -0.5, 0.25, -0.25, 1.0]);
        let mut out = [0xffu8; 4 * 8];
        // Four frames wanted; two whole frames (and an odd leftover sample) in the ring.
        let frames = fill(&mut c, &mut out);
        assert_eq!(frames, 2);
        let samples: Vec<f32> = out
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        assert_eq!(samples, [0.5, -0.5, 0.25, -0.25, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(c.slots(), 1, "the odd sample waits for its pair");
    }

    #[test]
    fn writer_sleeps_until_the_cycle_that_frees_room() {
        let ms = |n: f64| Duration::from_secs_f64(n / 1000.0);
        // 1024-frame cycles at 48 kHz are 21.33 ms; the last ran 5 ms ago. One packet (960
        // frames) needs one cycle: wake just after the next, about 16.6 ms from now.
        let last = 1_000_000_000;
        let now = last + 5_000_000;
        let w = cycle_wait(last, now, 1024, 48_000, 960);
        assert!(w >= ms(16.3) && w <= ms(16.9), "{w:?}");
        // 3000 frames need three cycles.
        let w = cycle_wait(last, now, 1024, 48_000, 3000);
        assert!(w >= ms(59.0) && w <= ms(59.6), "{w:?}");
        // Overdue (the callback is late): a short re-check, not a spin.
        let w = cycle_wait(last, last + 30_000_000, 1024, 48_000, 960);
        assert_eq!(w, ms(5.0));
        // No cycle seen yet: a fixed short wait.
        assert_eq!(cycle_wait(0, now, 0, 48_000, 960), ms(5.0));
    }

    #[test]
    fn volume_applied_once_per_change() {
        let mut v = VolumeGate::new(0.125);
        assert_eq!(v.state(&StreamState::Connecting), None);
        // Connected but not linked yet: the link will reset it, so it goes again until the
        // stream first runs.
        assert_eq!(v.state(&StreamState::Paused), Some(0.125));
        assert_eq!(v.change(0.5), 0.5);
        assert_eq!(v.state(&StreamState::Paused), Some(0.5));
        assert_eq!(v.state(&StreamState::Streaming), Some(0.5));
        // From then on, pause and resume leave it alone: a mixer may have changed it.
        assert_eq!(v.state(&StreamState::Paused), None);
        assert_eq!(v.state(&StreamState::Streaming), None);
        // A change of ours still goes, once.
        assert_eq!(v.change(0.25), 0.25);
        assert_eq!(v.state(&StreamState::Paused), None);
    }

    #[test]
    fn volume_is_cubic() {
        assert_eq!(channel_volume(1.0), 1.0);
        assert_eq!(channel_volume(0.0), 0.0);
        assert!((channel_volume(0.5) - 0.125).abs() < 1e-7);
    }

    // Connections made by `fake_connect`, per test thread (tests run in parallel).
    thread_local! {
        static CONNECTS: Cell<u32> = const { Cell::new(0) };
    }

    /// An `Output` with no PipeWire behind it: nothing reads its ring or its control channel.
    fn fake_connect(
        rate: u32,
        _volume: f32,
        _paused: bool,
        _notify: Option<LostNotify>,
    ) -> Result<Output, Error> {
        CONNECTS.with(|c| c.set(c.get() + 1));
        let (producer, _consumer) = rtrb::RingBuffer::new(16);
        let (control, _inbox) = pw::channel::channel();
        Ok(Output {
            rate,
            producer,
            capacity: 16,
            shared: Arc::new(Shared::default()),
            control,
            thread: None,
            done: None,
        })
    }

    #[test]
    fn failed_stream_is_reconnected() {
        // After a PipeWire restart the old stream is gone for good: the next song must get a
        // new one, not keep failing on the dead one.
        let mut sink = PipeWireSink::new();
        sink.connect = fake_connect;
        sink.open(48_000, 2).unwrap();
        sink.open(48_000, 2).unwrap();
        assert_eq!(CONNECTS.with(Cell::get), 1, "a healthy stream is kept");
        let shared = sink.out.as_ref().unwrap().shared.clone();
        shared.failed.store(true, Ordering::Release);
        sink.open(48_000, 2).unwrap();
        assert_eq!(CONNECTS.with(Cell::get), 2, "a failed stream is replaced");
        assert!(
            !sink
                .out
                .as_ref()
                .unwrap()
                .shared
                .failed
                .load(Ordering::Acquire)
        );
    }

    #[test]
    fn stream_states_that_mean_the_server_is_gone() {
        // A sound server restart leaves the stream Unconnected (the core's socket closed),
        // a refused or broken stream ends in Error: both are gone for good.
        assert!(stream_gone(&StreamState::Unconnected));
        assert!(stream_gone(&StreamState::Error("broken".into())));
        assert!(!stream_gone(&StreamState::Connecting));
        assert!(!stream_gone(&StreamState::Paused));
        assert!(!stream_gone(&StreamState::Streaming));
    }

    #[test]
    fn core_errors_that_mean_the_server_is_gone() {
        // An error on the core object itself is the connection's (EPIPE: the daemon went
        // away); an error about another object is that object's, which its owner handles.
        assert!(core_gone(pw::core::PW_ID_CORE));
        assert!(!core_gone(42));
    }

    #[test]
    fn a_lost_output_says_so_until_reopened() {
        let mut sink = PipeWireSink::new();
        sink.connect = fake_connect;
        assert!(!sink.lost(), "nothing open is nothing lost");
        sink.open(48_000, 2).unwrap();
        assert!(!sink.lost());
        let shared = sink.out.as_ref().unwrap().shared.clone();
        shared.failed.store(true, Ordering::Release);
        assert!(sink.lost());
        // A write into the dead stream names the restart, so the engine can act on it.
        assert_eq!(sink.write(&[0.0; 4]), Err(Error::OutputRestarted));
        sink.open(48_000, 2).unwrap();
        assert!(!sink.lost(), "open made a new stream");
    }

    #[test]
    fn close_never_waits_for_ever_on_a_stuck_thread() {
        // A PipeWire thread stuck inside libpipewire (a connect that never returns) must not
        // hold the audio thread in `close`, and so every song after it, for ever.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || {
            let _done = done_tx;
            let _ = release_rx.recv();
        });
        let (producer, _consumer) = rtrb::RingBuffer::new(16);
        let (control, _inbox) = pw::channel::channel();
        let out = Output {
            rate: 48_000,
            producer,
            capacity: 16,
            shared: Arc::new(Shared::default()),
            control,
            thread: Some(stuck),
            done: Some(done_rx),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            out.close();
            let _ = tx.send(());
        });
        let returned = rx.recv_timeout(CLOSE_WAIT + Duration::from_secs(2));
        drop(release_tx);
        assert!(returned.is_ok(), "close waited for ever on a stuck thread");
    }

    #[test]
    fn a_loss_is_told_once_and_never_for_our_own_close() {
        let calls = Arc::new(AtomicU64::new(0));
        let seen = calls.clone();
        let notify: Option<LostNotify> = Some(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        let shared = Shared::default();
        mark_gone(&shared, &notify);
        mark_gone(&shared, &notify);
        assert!(shared.failed.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "once per stream");
        // Closing it ourselves also leaves the stream Unconnected: that is not a loss.
        let closing = Shared::default();
        closing.closing.store(true, Ordering::Release);
        mark_gone(&closing, &notify);
        assert!(closing.failed.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn format_pod_builds() {
        let pod = format_pod(48_000).unwrap();
        assert!(spa::pod::Pod::from_bytes(&pod).is_some());
    }

    /// Plays 1 s of silence through the real daemon at volume 0. Ignored: tests never touch
    /// the user's audio; run it by hand with `cargo test -- --ignored pipewire`.
    #[test]
    #[ignore]
    fn pipewire_plays_silence() {
        let mut sink = PipeWireSink::new();
        sink.set_volume(0.0);
        sink.open(48_000, 2).unwrap();
        for _ in 0..50 {
            sink.write(&[0.0; 960 * 2]).unwrap();
        }
        assert!(sink.delay_frames() > 0);
        sink.flush();
        sink.pause(true);
    }
}
