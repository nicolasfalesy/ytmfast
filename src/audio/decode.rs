//! Decoding a track into interleaved stereo `f32` frames.
//!
//! Demux is symphonia (WebM for Opus, MP4 for AAC). Opus packets go to the system's libopus
//! (`audio::opus`) at 48 kHz; AAC goes to symphonia's own decoder at the track's rate.
//!
//! Trimming, so a track is exactly its music (gapless playback later depends on it):
//! - Start: the encoder's priming frames are dropped. Opus: the OpusHead pre-skip (312 frames
//!   from libopus). AAC: the MP4 edit list's media time (1024 frames from most encoders).
//!   symphonia 0.6 does neither (its MP4 reader ignores edit lists, and WebM leaves the
//!   pre-skip to the decoder), so this module does both.
//! - End: the last packet's stated duration, when it is shorter than what the packet decodes
//!   to (WebM writes the trimmed duration on the final block; MP4's last sample has a full
//!   duration), and for MP4 the movie's length (the edit list's total). WebM timestamps are in
//!   ms, so the WebM end trim is good to 1 ms; MP4 is exact. symphonia 0.6 reads WebM's
//!   `DiscardPadding` but doesn't pass it on, or the WebM end would be exact too.
//!
//! Positions: the output frame of a packet is set from its timestamp after an open or a seek,
//! then advanced by the frames actually decoded, so a straight run is exact. A seek lands
//! exactly on MP4 and within 1 ms on WebM (again its ms timestamps).

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::OnceLock;

use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::{Hint, Probe};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::packet::Packet;
use symphonia::core::units::{TimeBase, Timestamp};
use symphonia::default::formats::{IsoMp4Reader, MkvReader};

use crate::audio::fetch::TrackReader;
use crate::audio::opus::OpusDecoder;
use crate::error::Error;

/// YouTube's loudness normalisation: songs louder than the reference are turned down by the
/// difference; quieter songs are left alone (never turned up, which could clip).
pub fn loudness_gain(loudness_db: Option<f32>) -> f32 {
    match loudness_db {
        Some(db) if db > 0.0 && db.is_finite() => 10f32.powf(-db / 20.0),
        _ => 1.0,
    }
}

/// Opus always decodes at 48 kHz, whatever the input was.
const OPUS_RATE: u32 = 48_000;

/// Opus's longest packet is 120 ms: the most frames one packet can decode to.
const OPUS_MAX_FRAMES: usize = 5760;

/// How far before a seek target decoding starts, with the output dropped. Opus needs 80 ms
/// to converge after a reset (RFC 7845's recommended pre-roll); AAC needs one frame (23 ms)
/// for its overlap. One value for both keeps it simple.
const SEEK_PREROLL_SECS: f64 = 0.08;

/// Undecodable packets in a row that end the track. A single damaged packet is skipped (a
/// short glitch beats stopping the song); a run of them means the stream is not audio.
const MAX_BAD_PACKETS: u32 = 20;

/// The most of an MP4's `moov` box read to find the edit list: a song's is tens of KiB, and
/// a bogus size must not make the engine allocate much.
const MAX_MOOV: u64 = 4 << 20;

/// Only the two containers YouTube serves audio in. A probe of our own, without symphonia's
/// default metadata readers: those make the probe seek to the end of the file for trailing
/// tags, which would wait for the whole track to download before the first sound.
fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(|| {
        let mut probe = Probe::default();
        probe.register_format::<MkvReader<'_>>();
        probe.register_format::<IsoMp4Reader<'_>>();
        probe
    })
}

impl MediaSource for TrackReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.total_len()
    }
}

enum Codec {
    Opus(OpusDecoder),
    Symphonia(Box<dyn AudioDecoder>),
}

/// A track being decoded. Blocking: reads wait for the download, so it belongs on the audio
/// thread.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    track_id: u32,
    time_base: TimeBase,
    codec: Codec,
    rate: u32,
    /// Encoder priming at the start of the track, in output frames.
    priming: i64,
    /// The first packet's timestamp: the zero that packet timestamps are measured from.
    first_pts: Timestamp,
    /// Where the playable audio ends, in output frames, when the container says.
    end_frame: Option<i64>,
    duration: Option<f64>,
    /// One packet read ahead, so the last packet is known to be the last.
    next: Option<Packet>,
    eof: bool,
    /// The output frame of the next packet's first decoded frame; `None` right after a seek,
    /// until the landing packet's timestamp sets it.
    cursor: Option<i64>,
    /// Frames before this output frame are dropped: the priming, or up to a seek target.
    drop_until: i64,
    /// One packet's frames, interleaved stereo.
    out: Vec<f32>,
    /// A symphonia packet's frames in its own channel layout, before the stereo mapping.
    scratch: Vec<f32>,
    bad_packets: u32,
    /// A spare cursor over the same track, to read its headers again (`reopen_format`).
    spare: Option<TrackReader>,
    /// The container's extension and type, for that second probe.
    ext: &'static str,
    kind: String,
}

impl Decoder {
    /// Opens a track. `mime` is the format's type from the player answer, such as
    /// `audio/webm; codecs="opus"` or `audio/mp4; codecs="mp4a.40.2"`.
    pub fn open(reader: TrackReader, mime: &str) -> Result<Decoder, Error> {
        let spare = reader.sibling();
        let mut dec = Self::open_source(reader, mime)?;
        dec.spare = Some(spare);
        Ok(dec)
    }

    fn open_source<R: MediaSource + 'static>(mut source: R, mime: &str) -> Result<Decoder, Error> {
        let kind = mime.split(';').next().unwrap_or("").trim();
        let ext = match kind {
            "audio/webm" => "webm",
            "audio/mp4" => "m4a",
            _ => return Err(Error::Unavailable("not a supported audio type".into())),
        };
        // symphonia can't see the MP4 edit list, so read it first (and rewind).
        let mp4_priming = if ext == "m4a" {
            let p = mp4_edit_media_time(&mut source).map_err(io_error)?;
            source.seek(SeekFrom::Start(0)).map_err(io_error)?;
            p
        } else {
            None
        };

        let format = probe_format(Box::new(source), ext, kind)?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| Error::StreamFailed("the track has no audio".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| Error::StreamFailed("the track has no audio".into()))?
            .clone();
        let track_id = track.id;
        let track_delay = track.delay;
        let time_base = track
            .time_base
            .or_else(|| params.sample_rate.and_then(TimeBase::try_from_recip))
            .ok_or_else(|| Error::StreamFailed("the track has no time base".into()))?;

        let (codec, rate, priming) = if params.codec == CODEC_ID_OPUS {
            let head = OpusHead::parse(params.extra_data.as_deref())?;
            let mut dec = OpusDecoder::new(OPUS_RATE, 2)
                .map_err(|_| Error::Internal("could not start the Opus decoder".into()))?;
            if head.output_gain != 0 {
                // The header's gain (Q7.8 dB) is part of the stream: RFC 7845 says apply it.
                dec.set_gain(i32::from(head.output_gain))
                    .map_err(|_| Error::StreamFailed("bad Opus output gain".into()))?;
            }
            (Codec::Opus(dec), OPUS_RATE, i64::from(head.pre_skip))
        } else if params.codec == CODEC_ID_AAC {
            let rate = params
                .sample_rate
                .ok_or_else(|| Error::StreamFailed("the track has no sample rate".into()))?;
            // gapless off: this module does the trimming, and must not have it done twice if
            // a later symphonia starts reading edit lists.
            let mut opts = AudioDecoderOptions::default();
            opts.gapless = false;
            let dec = symphonia::default::get_codecs()
                .make_audio_decoder(&params, &opts)
                .map_err(open_error)?;
            let priming = match mp4_priming {
                Some(t) => ts_to_frames(t, time_base, rate),
                None => i64::from(track_delay.unwrap_or(0)),
            };
            (Codec::Symphonia(dec), rate, priming)
        } else {
            return Err(Error::Unavailable("unsupported audio codec".into()));
        };

        // The length: an MP4 movie's duration is its edit list's total, so it is the exact end;
        // WebM's segment duration is only good to the ms and its end comes from the last packet.
        let info = *format.media_info();
        let header_secs = info
            .time_base
            .zip(info.duration)
            .and_then(|(tb, d)| tb.calc_duration(d))
            .map(|t| t.as_secs_f64())
            .filter(|s| *s > 0.0);
        let end_frame = match (&codec, header_secs) {
            (Codec::Symphonia(_), Some(s)) => Some((s * f64::from(rate)).round() as i64),
            _ => None,
        };
        let duration = header_secs;

        let mut dec = Decoder {
            format,
            track_id,
            time_base,
            codec,
            rate,
            priming,
            first_pts: Timestamp::new(0),
            end_frame,
            duration,
            next: None,
            eof: false,
            cursor: Some(-priming),
            drop_until: 0,
            out: Vec::with_capacity(OPUS_MAX_FRAMES * 2),
            scratch: Vec::new(),
            bad_packets: 0,
            spare: None,
            ext,
            kind: kind.to_string(),
        };
        dec.fill_next()?;
        match &dec.next {
            Some(p) => dec.first_pts = p.pts,
            None => return Err(Error::StreamFailed("the track has no audio packets".into())),
        }
        Ok(dec)
    }

    /// The song's length from the resolver (`length_seconds`), for a file whose header states
    /// none (fragmented MP4, YouTube's DASH audio, may not). It then clamps seeks like a stated
    /// length, and caps the end at the hint plus 1 s. Only a cap: the resolver's length is in
    /// whole seconds, so cutting at it could drop up to a second of music, while the padding a
    /// real trim removes is one codec frame. A stated length always wins.
    pub fn with_length_hint(mut self, seconds: Option<f64>) -> Decoder {
        let Some(hint) = seconds.filter(|s| s.is_finite() && *s > 0.0) else {
            return self;
        };
        if self.duration.is_none() {
            self.duration = Some(hint);
        }
        if self.end_frame.is_none() {
            self.end_frame = Some(((hint + 1.0) * f64::from(self.rate)).round() as i64);
        }
        self
    }

    /// The output rate: 48 kHz for Opus, the track's rate for AAC.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The track's length in seconds, when the container states it.
    pub fn duration(&self) -> Option<f64> {
        self.duration
    }

    /// The next frames, interleaved stereo; `None` at the end of the track.
    pub fn next_frames(&mut self) -> Result<Option<&[f32]>, Error> {
        loop {
            let Some(packet) = self.next.take() else {
                return Ok(None);
            };
            self.fill_next()?;
            let is_last = self.next.is_none();

            let begin = match self.cursor {
                Some(c) => c,
                None => self.anchor(&packet),
            };
            let n = match self.decode(&packet)? {
                Some(n) => n as i64,
                None => continue,
            };
            self.cursor = Some(begin + n);

            let mut hi = n;
            if is_last && packet.dur.get() > 0 {
                // A final packet that says it is shorter than it decodes to: the rest is the
                // encoder's padding.
                hi = hi.min(ts_to_frames(
                    packet.dur.get() as i64,
                    self.time_base,
                    self.rate,
                ));
            }
            if let Some(end) = self.end_frame {
                hi = hi.min(end - begin);
            }
            let lo = (self.drop_until - begin).clamp(0, n);
            if hi <= lo {
                if self.end_frame.is_some_and(|end| begin >= end) {
                    // Past the stated end: the rest is padding.
                    self.next = None;
                    return Ok(None);
                }
                continue;
            }
            return Ok(Some(&self.out[lo as usize * 2..hi as usize * 2]));
        }
    }

    /// Moves to `seconds`, clamped to the track (at most 1 s before the end, so a seek never
    /// lands on the very end), and returns where output resumes.
    pub fn seek(&mut self, seconds: f64) -> Result<f64, Error> {
        let mut target = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        if let Some(length) = self.duration {
            target = target.min((length - 1.0).max(0.0));
        }
        let rate = f64::from(self.rate);
        let target_frame = (target * rate).round() as i64;
        let from_frame = ((target - SEEK_PREROLL_SECS).max(0.0) * rate).round() as i64;
        let ts = self.first_pts.get().saturating_add(frames_to_ts(
            from_frame + self.priming,
            self.time_base,
            self.rate,
        ));

        if self.eof && self.spare.is_some() {
            // symphonia 0.6's demuxers can't seek once they have read to the end of the file:
            // MKV's element stack has left the Segment ("not an ancestor", mkv 0.6.1) and MP4
            // has no atom left to read ("no atom pending read"). That is any seek in a song's
            // last 200 ms (its end is decoded that far ahead), and every seek in the gapless
            // handover window. A demuxer read fresh from the headers seeks fine.
            self.reopen_format()?;
        }
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(ts),
                track_id: self.track_id,
            },
        );
        self.next = None;
        self.eof = false;
        self.bad_packets = 0;
        self.drop_until = target_frame;
        self.cursor = None;
        match &mut self.codec {
            Codec::Opus(d) => d
                .reset()
                .map_err(|_| Error::Internal("could not reset the Opus decoder".into()))?,
            Codec::Symphonia(d) => d.reset(),
        }
        match seeked {
            Ok(_) => {}
            Err(SymError::SeekError(_)) => {
                // Past the end (the length was unknown): the track is over.
                self.eof = true;
                return Ok(target_frame as f64 / rate);
            }
            Err(e) => return Err(stream_error(e)),
        }
        self.fill_next()?;
        let landed = match &self.next {
            Some(p) => self.anchor(p),
            None => target_frame,
        };
        // An accurate seek lands at or before the target; if a container lands after it,
        // output starts where it landed.
        Ok(landed.max(target_frame) as f64 / rate)
    }

    /// A new demuxer over the same track, from a fresh cursor (the headers are in memory by
    /// now, so this doesn't wait on the download).
    fn reopen_format(&mut self) -> Result<(), Error> {
        let Some(spare) = &self.spare else {
            return Err(Error::Internal("the track can't be read again".into()));
        };
        self.format = probe_format(Box::new(spare.sibling()), self.ext, &self.kind)?;
        Ok(())
    }

    /// The output frame a packet starts at, from its timestamp.
    fn anchor(&self, packet: &Packet) -> i64 {
        let since_first = packet.pts.get().saturating_sub(self.first_pts.get());
        ts_to_frames(since_first, self.time_base, self.rate) - self.priming
    }

    /// Reads the next packet of our track into `next`, or marks the end.
    fn fill_next(&mut self) -> Result<(), Error> {
        while !self.eof {
            match self.format.next_packet() {
                Ok(Some(p)) if p.track_id == self.track_id => {
                    self.next = Some(p);
                    return Ok(());
                }
                Ok(Some(_)) => continue,
                Ok(None) => self.eof = true,
                Err(e) => return Err(stream_error(e)),
            }
        }
        Ok(())
    }

    /// Decodes one packet into `out` (interleaved stereo); the frame count, or `None` for a
    /// damaged packet that was skipped.
    fn decode(&mut self, packet: &Packet) -> Result<Option<usize>, Error> {
        let frames = match &mut self.codec {
            Codec::Opus(dec) => {
                self.out.resize(OPUS_MAX_FRAMES * 2, 0.0);
                dec.decode_float(&packet.data, &mut self.out)
                    .map_err(|_| ())
            }
            Codec::Symphonia(dec) => match dec.decode(packet) {
                Ok(buf) => {
                    let channels = buf.spec().channels().count().max(1);
                    let frames = buf.frames();
                    self.scratch.resize(frames * channels, 0.0);
                    buf.copy_to_slice_interleaved(&mut self.scratch[..]);
                    to_stereo(&self.scratch, channels, &mut self.out);
                    Ok(frames)
                }
                Err(SymError::DecodeError(_)) => Err(()),
                Err(e) => return Err(stream_error(e)),
            },
        };
        match frames {
            Ok(n) => {
                self.bad_packets = 0;
                Ok(Some(n))
            }
            Err(()) => {
                self.bad_packets += 1;
                if self.bad_packets >= MAX_BAD_PACKETS {
                    return Err(Error::StreamFailed("the track's audio is damaged".into()));
                }
                Ok(None)
            }
        }
    }
}

/// The demuxer for `source`, a WebM or MP4 (`ext`) of type `kind`.
fn probe_format(
    source: Box<dyn MediaSource>,
    ext: &str,
    kind: &str,
) -> Result<Box<dyn FormatReader>, Error> {
    let mut hint = Hint::new();
    hint.with_extension(ext).mime_type(kind);
    let mss = MediaSourceStream::new(source, Default::default());
    probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(open_error)
}

/// Interleaved frames of any channel count to interleaved stereo: mono is copied to both
/// sides; more than two channels keep the front pair (YouTube's audio is stereo).
fn to_stereo(src: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    match channels {
        1 => out.extend(src.iter().flat_map(|&s| [s, s])),
        2 => out.extend_from_slice(src),
        _ => out.extend(src.chunks_exact(channels).flat_map(|f| [f[0], f[1]])),
    }
}

/// Track time-base units to output frames.
fn ts_to_frames(ts: i64, tb: TimeBase, rate: u32) -> i64 {
    let v =
        i128::from(ts) * i128::from(tb.numer.get()) * i128::from(rate) / i128::from(tb.denom.get());
    v as i64
}

/// Output frames to track time-base units (rounded down, so a seek lands at or before).
fn frames_to_ts(frames: i64, tb: TimeBase, rate: u32) -> i64 {
    let v = i128::from(frames) * i128::from(tb.denom.get())
        / (i128::from(tb.numer.get()) * i128::from(rate));
    v as i64
}

/// The parts of the Opus identification header (RFC 7845 §5.1) the decoder needs.
struct OpusHead {
    pre_skip: u16,
    output_gain: i16,
}

impl OpusHead {
    fn parse(data: Option<&[u8]>) -> Result<OpusHead, Error> {
        let bad = || Error::StreamFailed("the Opus header is missing or damaged".into());
        let d = data.ok_or_else(bad)?;
        if d.len() < 19 || &d[..8] != b"OpusHead" {
            return Err(bad());
        }
        let channels = d[9];
        let mapping_family = d[18];
        if channels > 2 || mapping_family != 0 {
            // Surround Opus needs the multistream decoder; YouTube's music is stereo.
            return Err(Error::Unavailable("surround Opus is not supported".into()));
        }
        Ok(OpusHead {
            pre_skip: u16::from_le_bytes([d[10], d[11]]),
            output_gain: i16::from_le_bytes([d[16], d[17]]),
        })
    }
}

/// The first non-empty edit's media time from an MP4's edit list (`moov/trak/edts/elst`), in
/// the track's time base: the frames of encoder priming before the music. `None` when the file
/// has no edit list. Leaves the reader anywhere; the caller rewinds.
fn mp4_edit_media_time(r: &mut (impl Read + Seek)) -> io::Result<Option<i64>> {
    r.seek(SeekFrom::Start(0))?;
    loop {
        let Some((kind, size)) = box_header(r)? else {
            return Ok(None);
        };
        match (&kind, size) {
            (b"moov", Some(len)) if len <= MAX_MOOV => {
                let mut moov = vec![0; len as usize];
                r.read_exact(&mut moov)?;
                return Ok(find_elst(&moov));
            }
            (b"moov", _) => return Ok(None),
            // Runs to the end of the file: nothing after it.
            (_, None) => return Ok(None),
            (_, Some(len)) => {
                // Checked: a 64-bit size near 2^64 must not wrap into a seek backwards (an
                // endless loop on the audio thread, from a file off the network).
                let here = r.stream_position()?;
                let next = here.checked_add(len).ok_or_else(bad_box)?;
                r.seek(SeekFrom::Start(next))?;
            }
        }
    }
}

/// A box header: its type and payload length (`None` = to the end of the file). `None` at a
/// clean end of file.
fn box_header(r: &mut impl Read) -> io::Result<Option<([u8; 4], Option<u64>)>> {
    let mut h = [0u8; 8];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    let kind = [h[4], h[5], h[6], h[7]];
    let payload = match size {
        0 => None,
        1 => {
            let mut l = [0u8; 8];
            r.read_exact(&mut l)?;
            Some(u64::from_be_bytes(l).checked_sub(16).ok_or_else(bad_box)?)
        }
        n => Some(u64::from(n).checked_sub(8).ok_or_else(bad_box)?),
    };
    Ok(Some((kind, payload)))
}

fn bad_box() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "bad MP4 box size")
}

/// Walks `moov`'s children down `trak/edts/elst` and reads the first non-empty edit.
fn find_elst(moov: &[u8]) -> Option<i64> {
    let trak = child(moov, b"trak")?;
    let edts = child(trak, b"edts")?;
    let elst = child(edts, b"elst")?;
    let version = *elst.first()?;
    let count = u32::from_be_bytes(elst.get(4..8)?.try_into().ok()?);
    let entry = if version == 1 { 20 } else { 12 };
    (0..count as usize).find_map(|i| {
        let e = elst.get(8 + i * entry..8 + (i + 1) * entry)?;
        let media_time = if version == 1 {
            i64::from_be_bytes(e[8..16].try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(e[4..8].try_into().ok()?))
        };
        // -1 is an empty edit (a gap before the media): skip it.
        (media_time >= 0).then_some(media_time)
    })
}

/// The payload of the first child box of type `kind` in `parent`.
fn child<'a>(parent: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut at = 0usize;
    while at + 8 <= parent.len() {
        let size = u32::from_be_bytes(parent[at..at + 4].try_into().ok()?) as usize;
        let (header, len) = match size {
            0 => (8, parent.len() - at),
            1 => {
                let l = u64::from_be_bytes(parent.get(at + 8..at + 16)?.try_into().ok()?);
                (16, usize::try_from(l).ok()?)
            }
            n => (8, n),
        };
        // Checked: `at + len` must not wrap (a huge 64-bit size) into a loop or a reversed
        // slice.
        let end = at.checked_add(len)?;
        if len < header || end > parent.len() {
            return None;
        }
        if &parent[at + 4..at + 8] == kind {
            return Some(&parent[at + header..end]);
        }
        at = end;
    }
    None
}

/// An io error from the track reader: the download's own `Error` when it carries one.
fn io_error(e: io::Error) -> Error {
    if let Some(ours) = e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
        return ours.clone();
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof => Error::StreamFailed("the track ended early".into()),
        _ => Error::StreamFailed("could not read the track".into()),
    }
}

/// symphonia's errors while reading or decoding. Its messages are fixed strings (no URLs).
fn stream_error(e: SymError) -> Error {
    match e {
        SymError::IoError(e) => io_error(e),
        SymError::DecodeError(m) => Error::StreamFailed(format!("damaged audio ({m})")),
        SymError::Unsupported(m) => Error::Unavailable(format!("unsupported audio ({m})")),
        SymError::LimitError(m) => Error::StreamFailed(format!("audio over a limit ({m})")),
        SymError::SeekError(_) => Error::StreamFailed("could not seek in the track".into()),
        SymError::ResetRequired => Error::StreamFailed("the track changed format part way".into()),
        _ => Error::StreamFailed("could not read the track".into()),
    }
}

/// Opening: a file no reader recognises is a broken stream, not an unsupported one.
fn open_error(e: SymError) -> Error {
    match e {
        SymError::Unsupported(_) => Error::StreamFailed("not a recognised audio file".into()),
        e => stream_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn elst_v0(entries: &[(u32, i32)]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (dur, time) in entries {
            p.extend_from_slice(&dur.to_be_bytes());
            p.extend_from_slice(&time.to_be_bytes());
            p.extend_from_slice(&[0, 1, 0, 0]);
        }
        bx(b"elst", &p)
    }

    fn file(elst: Vec<u8>) -> io::Cursor<Vec<u8>> {
        let mut f = bx(b"ftyp", b"M4A \0\0\0\0");
        let trak = bx(
            b"trak",
            &[bx(b"tkhd", &[0; 84]), bx(b"edts", &elst)].concat(),
        );
        f.extend(bx(b"moov", &[bx(b"mvhd", &[0; 100]), trak].concat()));
        f.extend(bx(b"mdat", &[0; 32]));
        io::Cursor::new(f)
    }

    #[test]
    fn edit_list_media_time() {
        assert_eq!(
            mp4_edit_media_time(&mut file(elst_v0(&[(88200, 1024)]))).unwrap(),
            Some(1024)
        );
        // A leading empty edit is skipped.
        assert_eq!(
            mp4_edit_media_time(&mut file(elst_v0(&[(10, -1), (88200, 2112)]))).unwrap(),
            Some(2112)
        );
        assert_eq!(
            mp4_edit_media_time(&mut file(bx(b"free", &[]))).unwrap(),
            None
        );
    }

    /// Runs `f` on its own thread and fails if it takes over 2 s: the bug these tests pin
    /// was an endless loop, which must fail the test, not hang the suite.
    fn within_2s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("the box walker looped (no answer in 2 s)")
    }

    /// A 64-bit size of 2^64 − 8: `as i64` made it −8, a seek back onto the same box.
    fn huge_box(kind: &[u8; 4]) -> Vec<u8> {
        let mut b = 1u32.to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(&(u64::MAX - 7).to_be_bytes());
        b
    }

    #[test]
    fn top_level_huge_largesize_does_not_loop() {
        let mut f = bx(b"free", &[]);
        f.extend(huge_box(b"skip"));
        let r =
            within_2s(move || mp4_edit_media_time(&mut io::Cursor::new(f)).map_err(|e| e.kind()));
        assert!(matches!(r, Ok(None) | Err(_)), "{r:?}");
    }

    #[test]
    fn child_huge_largesize_does_not_loop_or_panic() {
        let mut moov = bx(b"free", &[]);
        moov.extend(huge_box(b"skip"));
        moov.extend(vec![0; 16]);
        // `at + len` wrapped to a small number in a release build (an overflow panic in debug).
        assert_eq!(
            within_2s(move || child(&moov, b"trak").map(<[u8]>::len)),
            None
        );
    }

    #[test]
    fn edit_list_survives_bad_sizes() {
        // A child box claiming more than its parent holds is ignored, not followed.
        let mut f = file(elst_v0(&[(1, 1024)])).into_inner();
        let at = f.windows(4).position(|w| w == b"elst").unwrap() - 4;
        f[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(mp4_edit_media_time(&mut io::Cursor::new(f)).unwrap(), None);
        // A truncated file is a clean "no edit list" or an io error, never a panic.
        let short = file(elst_v0(&[(1, 1024)])).into_inner()[..30].to_vec();
        let _ = mp4_edit_media_time(&mut io::Cursor::new(short));
    }

    #[test]
    fn opus_head() {
        let mut h = b"OpusHead".to_vec();
        h.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0x00, 0x01, 0]);
        let head = OpusHead::parse(Some(&h)).unwrap();
        assert_eq!(head.pre_skip, 312);
        assert_eq!(head.output_gain, 256);
        let mut surround = h.clone();
        surround[9] = 6;
        surround[18] = 1;
        assert_eq!(
            OpusHead::parse(Some(&surround)).err().unwrap().code(),
            "unavailable"
        );
        assert_eq!(
            OpusHead::parse(Some(b"OpusTags")).err().unwrap().code(),
            "stream_failed"
        );
        assert!(OpusHead::parse(None).is_err());
    }

    #[test]
    fn stereo_mapping() {
        let mut out = Vec::new();
        to_stereo(&[0.1, 0.2], 1, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2]);
        to_stereo(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 3, &mut out);
        assert_eq!(out, [0.1, 0.2, 0.4, 0.5]);
    }
}
