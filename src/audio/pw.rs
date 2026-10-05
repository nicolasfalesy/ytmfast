//! The real output: a PipeWire playback stream.
//!
//! PipeWire runs on its own thread (a `pw` main loop). The stream's `process` callback runs on
//! PipeWire's real-time data thread and only copies from a lock-free ring (`rtrb`, 200 ms)
//! into PipeWire's buffer: no locks, no allocation, no syscalls. Everything else (pause,
//! volume, flush) goes to the main loop as a message, because those calls belong on it.
//!
//! The audio thread writes into the ring and, when it is full, sleeps for as long as the
//! missing room takes to play: no busy wait, and decoding stays at most 200 ms ahead.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamRc, StreamState};

use crate::audio::sink::Sink;
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
    /// Set when the writer queues audio, cleared when the ring runs dry.
    primed: AtomicBool,
    /// The stream errored or was disconnected.
    failed: AtomicBool,
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
}

/// PipeWire playback. Connects on the first `open`; a new rate reconnects (the rare switch
/// between 48 kHz Opus and 44.1 kHz AAC).
pub struct PipeWireSink {
    out: Option<Output>,
    /// The slider value (0..=1), re-applied to each new stream.
    volume: f32,
    paused: bool,
}

impl PipeWireSink {
    pub fn new() -> PipeWireSink {
        PipeWireSink {
            out: None,
            volume: 1.0,
            paused: false,
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
            watch.check()?;
            std::thread::sleep(Duration::from_millis(2));
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
        if self.out.as_ref().is_some_and(|o| o.rate == rate) {
            return Ok(());
        }
        if let Some(old) = self.out.take() {
            old.close();
        }
        self.out = Some(Output::connect(
            rate,
            channel_volume(self.volume),
            self.paused,
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
            if out.shared.failed.load(Ordering::Acquire) {
                return Err(Error::Internal("the audio output stopped".into()));
            }
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
            // Full: sleep about as long as the missing room takes to play.
            let missing = frames.len().min(out.capacity) / 2;
            let wait = Duration::from_secs_f64(missing as f64 / f64::from(out.rate));
            std::thread::sleep(wait.clamp(Duration::from_millis(1), Duration::from_millis(50)));
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
    fn connect(rate: u32, volume: f32, paused: bool) -> Result<Output, Error> {
        let capacity = ((RING_SECS * f64::from(rate)) as usize) * 2;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let shared = Arc::new(Shared::default());
        let (control, inbox) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ytmfast-pipewire".into())
            .spawn(move || {
                let setup = Setup {
                    rate,
                    volume,
                    paused,
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

    fn close(mut self) {
        let _ = self.control.send(Control::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Setup {
    rate: u32,
    volume: f32,
    paused: bool,
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
    let stream = StreamRc::new(
        core,
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

    let volume = Rc::new(Cell::new(setup.volume));
    let state_volume = volume.clone();
    let state_shared = setup.shared.clone();
    let _state = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |s, _, _, new| match new {
            StreamState::Error(_) | StreamState::Unconnected => {
                state_shared.failed.store(true, Ordering::Release);
            }
            // Controls only stick once the stream is negotiated: apply the volume then.
            StreamState::Paused | StreamState::Streaming => {
                set_volume(s, state_volume.get());
            }
            StreamState::Connecting => {}
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
            volume.set(v);
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
    fn volume_is_cubic() {
        assert_eq!(channel_volume(1.0), 1.0);
        assert_eq!(channel_volume(0.0), 0.0);
        assert!((channel_volume(0.5) - 0.125).abs() < 1e-7);
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
