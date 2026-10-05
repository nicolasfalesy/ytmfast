//! Decoding (`audio::decode`) against two small generated fixtures:
//!
//! - `sine440_48k.webm`: 2 s of a 440 Hz sine, stereo, Opus 64 kb/s in WebM (like itag 774).
//! - `sine440_44k.m4a`: the same at 44.1 kHz, AAC-LC 128 kb/s in MP4 (like itag 141).
//!
//! Both were made with ffmpeg's `sine` source (see the commands in `tests/fixtures/README`).

use ytmfast::audio::decode::{Decoder, loudness_gain};
use ytmfast::audio::fetch::{TrackBuffer, TrackReader};

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

fn fixture(name: &str) -> TrackReader {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).expect("the fixture should exist");
    TrackBuffer::from_bytes(bytes).reader()
}

/// Every frame of the track, interleaved stereo.
fn decode_all(dec: &mut Decoder) -> Vec<f32> {
    let mut out = Vec::new();
    while let Some(frames) = dec.next_frames().expect("decoding should not fail") {
        out.extend_from_slice(frames);
    }
    out
}

/// The dominant frequency of one channel, from its zero crossings (two per period). The
/// first and last 100 ms are left out: the encoders' start and end ramps aren't the sine.
fn frequency(samples: &[f32], channel: usize, rate: u32) -> f64 {
    let skip = (rate / 10) as usize;
    let mono: Vec<f32> = samples.iter().skip(channel).step_by(2).copied().collect();
    let body = &mono[skip..mono.len() - skip];
    let crossings = body
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f64 / 2.0 / (body.len() as f64 / f64::from(rate))
}

fn check_sine(name: &str, mime: &str, rate: u32) {
    let mut dec = Decoder::open(fixture(name), mime).expect("the fixture should open");
    assert_eq!(dec.rate(), rate);
    let samples = decode_all(&mut dec);
    assert_eq!(samples.len() % 2, 0, "stereo frames come in pairs");
    let seconds = (samples.len() / 2) as f64 / f64::from(rate);
    assert!(
        (seconds - 2.0).abs() <= 0.020,
        "{name}: {seconds} s decoded, wanted 2 s ± 20 ms"
    );
    for channel in 0..2 {
        let f = frequency(&samples, channel, rate);
        assert!(
            (f - 440.0).abs() <= 5.0,
            "{name}: channel {channel} at {f} Hz"
        );
    }
    // A real signal, not silence: the sine source's amplitude is 1/8.
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((0.08..=0.2).contains(&peak), "{name}: peak {peak}");
}

#[test]
fn decodes_opus_webm() {
    check_sine("sine440_48k.webm", OPUS_MIME, 48_000);
}

#[test]
fn decodes_aac_m4a() {
    check_sine("sine440_44k.m4a", AAC_MIME, 44_100);
}

/// The Opus pre-skip (312 frames of encoder priming, from the OpusHead) is dropped, and the
/// AAC priming (1024 frames, from the MP4 edit list) too: the first frame out is the sine's
/// first sample, which ffmpeg's `sine` source starts at 0, so the opening frames are quiet
/// and rise. Without the trim the first frames are the codec's start-up (Opus) or a whole
/// extra frame of near-silence (AAC), and the track runs long.
#[test]
fn priming_is_trimmed() {
    for (name, mime, rate) in [
        ("sine440_48k.webm", OPUS_MIME, 48_000u32),
        ("sine440_44k.m4a", AAC_MIME, 44_100),
    ] {
        let mut dec = Decoder::open(fixture(name), mime).unwrap();
        let samples = decode_all(&mut dec);
        let frames = samples.len() / 2;
        // The exact length: ±1 ms for WebM (its timestamps are in ms), exact for MP4.
        let want = rate as usize * 2;
        let slack = if name.ends_with("webm") { 48 } else { 0 };
        assert!(
            frames.abs_diff(want) <= slack,
            "{name}: {frames} frames, wanted {want}"
        );
        // A quarter period of 440 Hz in: the sine is near its first peak (1/8).
        let quarter = (rate as usize) / 440 / 4;
        let early = samples[2 * quarter].abs();
        assert!(early > 0.05, "{name}: sample {quarter} is {early}");
    }
}

#[test]
fn gain_math() {
    assert_eq!(loudness_gain(None), 1.0);
    assert_eq!(loudness_gain(Some(-3.0)), 1.0);
    assert_eq!(loudness_gain(Some(0.0)), 1.0);
    assert!((loudness_gain(Some(6.0)) - 0.501).abs() <= 0.001);
}

#[test]
fn seek_clamps() {
    for (name, mime) in [
        ("sine440_48k.webm", OPUS_MIME),
        ("sine440_44k.m4a", AAC_MIME),
    ] {
        let mut dec = Decoder::open(fixture(name), mime).unwrap();
        let length = dec.duration().expect("both fixtures state their length");
        assert!((length - 2.0).abs() <= 0.02, "{name}: length {length}");

        assert_eq!(dec.seek(-5.0).unwrap(), 0.0, "{name}");
        let back = dec.seek(99.0).unwrap();
        assert!(
            (back - (length - 1.0)).abs() <= 0.002,
            "{name}: seek(99) gave {back}, length {length}"
        );
        // The rest of the track is what's left after the seek point.
        let rest = decode_all(&mut dec).len() / 2;
        let rest = rest as f64 / f64::from(dec.rate());
        assert!(
            (rest - 1.0).abs() <= 0.02,
            "{name}: {rest} s after the seek"
        );
    }
}

/// A seek lands where it says: the frames after `seek(t)` are the ones a straight decode
/// has at `t`. Exact for MP4 (sample timestamps); within 1 ms for WebM, whose timestamps are
/// in ms. Found by sliding the seeked audio along the straight decode for the best match.
#[test]
fn seek_lands_on_the_target() {
    for (name, mime, slack) in [
        ("sine440_48k.webm", OPUS_MIME, 48usize),
        ("sine440_44k.m4a", AAC_MIME, 0),
    ] {
        let mut dec = Decoder::open(fixture(name), mime).unwrap();
        let all = decode_all(&mut dec);
        let rate = dec.rate() as usize;
        let mut dec = Decoder::open(fixture(name), mime).unwrap();
        let at = dec.seek(0.75).unwrap();
        assert!((at - 0.75).abs() <= 0.001, "{name}: seek(0.75) gave {at}");
        let after = decode_all(&mut dec);
        let start = (at * rate as f64).round() as usize;
        // 2 ms of the left channel, compared at offsets up to ±2 ms.
        let n = rate / 500;
        let error = |offset: isize| {
            (0..n)
                .map(|i| {
                    let a = all[2 * (start as isize + offset + i as isize) as usize];
                    (a - after[2 * i]).abs()
                })
                .fold(0.0f32, f32::max)
        };
        let reach = (rate / 500) as isize;
        let (best, worst) = (-reach..=reach)
            .map(|o| (o, error(o)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        assert!(worst < 0.02, "{name}: no match (best error {worst})");
        assert!(
            best.unsigned_abs() <= slack,
            "{name}: landed {best} frames off"
        );
    }
}

#[test]
fn wrong_mime_is_refused() {
    let err = Decoder::open(fixture("sine440_48k.webm"), "video/mp4; codecs=\"avc1\"")
        .err()
        .expect("a video type should be refused");
    assert_eq!(err.code(), "unavailable");
}

#[test]
fn garbage_is_a_stream_error() {
    let reader = TrackBuffer::from_bytes(vec![0x5a; 4096]).reader();
    let err = Decoder::open(reader, OPUS_MIME).err().expect("garbage");
    assert_eq!(err.code(), "stream_failed");
}
