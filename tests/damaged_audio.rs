//! A damaged or hostile audio file must never take the audio thread down.
//!
//! The three fixtures are copies of `sine440_44k_frag.m4a` (YouTube's DASH audio shape) with a
//! few bytes changed, found by a mutation run against the demuxer: two make symphonia panic
//! inside `Decoder::open` / `next_frames`, one states a sample rate of 0 (a seek then divided
//! by zero). The decoder turns all of them into an ordinary error, and the audio thread then
//! goes on to play the next song.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use ytmfast::audio::decode::Decoder;
use ytmfast::audio::fetch::TrackBuffer;
use ytmfast::audio::player::{AudioEvent, AudioPlayer};
use ytmfast::audio::sink::NullSink;

const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";
const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn decode_all(bytes: &[u8]) {
    let reader = TrackBuffer::from_bytes(bytes.to_vec()).reader();
    if let Ok(mut dec) = Decoder::open(reader, AAC_MIME) {
        while let Ok(Some(_)) = dec.next_frames() {}
    }
}

#[test]
fn a_corrupt_trun_index_does_not_panic_the_decoder() {
    let bytes = fixture("damaged_trun_index.m4a");
    let r = catch_unwind(AssertUnwindSafe(|| decode_all(&bytes)));
    assert!(r.is_ok(), "symphonia's panic got out of the decoder");
}

#[test]
fn a_corrupt_stream_position_does_not_panic_the_decoder() {
    let bytes = fixture("damaged_stream_position.m4a");
    let r = catch_unwind(AssertUnwindSafe(|| decode_all(&bytes)));
    assert!(r.is_ok(), "symphonia's panic got out of the decoder");
}

#[test]
fn a_track_with_sample_rate_zero_is_refused_at_open() {
    let bytes = fixture("damaged_zero_rate.m4a");
    let r = catch_unwind(AssertUnwindSafe(|| {
        let reader = TrackBuffer::from_bytes(bytes.clone()).reader();
        if let Ok(mut dec) = Decoder::open(reader, AAC_MIME) {
            assert!(dec.rate() > 0, "open accepted a track with sample rate 0");
            let _ = dec.seek(0.5);
        }
    }));
    assert!(
        r.is_ok(),
        "a 0 Hz track got past open and seek divided by zero"
    );
}

#[test]
fn a_decoder_panic_is_reported_as_an_error_and_audio_keeps_working() {
    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let events = player.events();

    player.load(
        TrackBuffer::from_bytes(fixture("damaged_trun_index.m4a")).reader(),
        AAC_MIME,
        1.0,
        0.0,
        None,
    );
    loop {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(AudioEvent::Loading) => continue,
            Ok(AudioEvent::Error(_)) => break,
            Ok(other) => panic!("unexpected event {other:?}"),
            Err(e) => panic!("no Error event for a track that panicked the decoder: {e}"),
        }
    }

    // And the player still plays the next, good song.
    player.load(
        TrackBuffer::from_bytes(fixture("sine440_48k.webm")).reader(),
        OPUS_MIME,
        1.0,
        0.0,
        None,
    );
    player.play();
    let mut started = false;
    while let Ok(e) = events.recv_timeout(Duration::from_secs(5)) {
        if matches!(e, AudioEvent::Started) {
            started = true;
            break;
        }
    }
    assert!(
        started,
        "the audio thread did not start the next song after the bad one"
    );
}
