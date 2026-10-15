//! Decoding stays cheap: heap allocations and bytes per decoded packet (the same on every
//! machine, so checked everywhere), and CPU time per second of audio (release builds on a
//! machine in the state the limit was measured in, see `perf_common`).
//!
//! Local fixtures only. A counting allocator makes this its own test binary, and the one test
//! in it runs alone, so nothing else's allocations are counted.
//!
//! Limits are the measured value plus 20%. Measured 2026-10-14 on the fixtures in
//! tests/fixtures (release): Opus 1.98 allocations and 385 bytes per packet and 0.816 ms of CPU
//! per audio second; AAC 0.99, 364 and 0.474 (the audit's longer files gave 1.07 and 0.59 ms:
//! these 1-second files weigh the open less).

mod perf_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use ytmfast::audio::decode::Decoder;
use ytmfast::audio::fetch::TrackBuffer;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call to the system allocator unchanged, after counting it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size() as u64, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(n as u64, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const OPUS_MIME: &str = "audio/webm; codecs=\"opus\"";
const AAC_MIME: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

/// (fixture, mime, allocations per packet, bytes per packet, CPU ms per audio second): the
/// measured value times 1.2.
const LIMITS: [(&str, &str, f64, f64, f64); 2] = [
    ("sine440_48k.webm", OPUS_MIME, 2.4, 462.0, 0.98),
    ("sine440_44k.m4a", AAC_MIME, 1.2, 437.0, 0.57),
];

fn open(name: &str, mime: &str) -> Decoder {
    let bytes = std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the fixture is in the repo");
    Decoder::open(TrackBuffer::from_bytes(bytes).reader(), mime).expect("the fixture opens")
}

/// (allocations per packet, bytes per packet) once the first packets have sized the buffers.
fn per_packet(name: &str, mime: &str) -> (f64, f64, u64) {
    let mut dec = open(name, mime);
    for _ in 0..5 {
        dec.next_frames().unwrap();
    }
    let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    let mut packets = 0u64;
    while dec.next_frames().unwrap().is_some() {
        packets += 1;
    }
    let (a1, b1) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    assert!(packets >= 20, "{name}: only {packets} packets to count");
    (
        (a1 - a0) as f64 / packets as f64,
        (b1 - b0) as f64 / packets as f64,
        packets,
    )
}

/// CPU milliseconds per second of decoded audio, over `reps` whole decodes (open included).
fn cpu_ms_per_audio_second(name: &str, mime: &str, reps: u32) -> f64 {
    let t0 = perf_common::cpu_seconds();
    let mut audio = 0.0;
    for _ in 0..reps {
        let mut dec = open(name, mime);
        let rate = f64::from(dec.rate());
        let mut frames = 0u64;
        while let Some(f) = dec.next_frames().unwrap() {
            frames += (f.len() / 2) as u64;
        }
        audio += frames as f64 / rate;
    }
    (perf_common::cpu_seconds() - t0) * 1000.0 / audio
}

#[test]
fn decoding_stays_within_its_allocation_and_cpu_limits() {
    let speed = perf_common::speed_limits_apply("perf_decode");
    for (name, mime, max_allocs, max_bytes, max_cpu) in LIMITS {
        let (allocs, bytes, packets) = per_packet(name, mime);
        println!(
            "{name}: {packets} packets, {allocs:.2} allocations and {bytes:.0} bytes per packet"
        );
        assert!(
            allocs <= max_allocs,
            "{name}: {allocs:.2} allocations per packet, the limit is {max_allocs}"
        );
        assert!(
            bytes <= max_bytes,
            "{name}: {bytes:.0} bytes per packet, the limit is {max_bytes}"
        );
        if speed {
            // Median of 5 rounds of 20 whole decodes.
            let mut rounds: Vec<f64> = (0..5)
                .map(|_| cpu_ms_per_audio_second(name, mime, 20))
                .collect();
            let cpu = perf_common::median(&mut rounds);
            println!("{name}: {cpu:.3} ms of CPU per second of audio");
            assert!(
                cpu <= max_cpu,
                "{name}: {cpu:.3} ms CPU per audio second, the limit is {max_cpu}"
            );
        }
    }
}
