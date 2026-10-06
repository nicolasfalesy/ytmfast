//! Opus decoding with the system's libopus (`libopus.so.0`, Arch's `opus` package), through the
//! four calls the decoder needs. Linking the system library instead of a crate that builds
//! its own copy makes the binary smaller, drops the cmake build, and takes libopus fixes with
//! system updates. `tests/decode.rs` pins the output bit for bit (`opus_output_is_pinned`).

use std::ffi::c_int;
use std::ptr::NonNull;

/// libopus's decoder state: opaque, only ever behind a pointer.
#[repr(C)]
struct RawDecoder {
    _private: [u8; 0],
}

// Linked by build.rs, which finds libopus with pkg-config.
unsafe extern "C" {
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut RawDecoder;
    fn opus_decode_float(
        st: *mut RawDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decoder_ctl(st: *mut RawDecoder, request: c_int, ...) -> c_int;
    fn opus_decoder_destroy(st: *mut RawDecoder);
}

/// From `opus_defines.h`.
const OPUS_OK: c_int = 0;
const OPUS_RESET_STATE: c_int = 4028;
const OPUS_SET_GAIN_REQUEST: c_int = 4034;

/// A libopus error code (negative; see `opus_defines.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusError(pub i32);

/// One libopus decoder.
pub struct OpusDecoder {
    raw: NonNull<RawDecoder>,
    channels: usize,
}

// SAFETY: the decoder state is plain heap memory with no tie to the thread that made it, and
// every call takes `&mut self`, so it is only ever used from one thread at a time.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    /// A decoder for `channels` (1 or 2) at `rate` (8, 12, 16, 24 or 48 kHz).
    pub fn new(rate: u32, channels: u8) -> Result<OpusDecoder, OpusError> {
        let rate = i32::try_from(rate).map_err(|_| OpusError(-1))?;
        let mut error: c_int = OPUS_OK;
        // SAFETY: plain values in, and `error` is a valid place for libopus to write the code.
        let raw = unsafe { opus_decoder_create(rate, c_int::from(channels), &mut error) };
        match NonNull::new(raw) {
            Some(raw) if error == OPUS_OK => Ok(OpusDecoder {
                raw,
                channels: usize::from(channels),
            }),
            Some(raw) => {
                // SAFETY: `raw` came from opus_decoder_create and is freed once, here.
                unsafe { opus_decoder_destroy(raw.as_ptr()) };
                Err(OpusError(error))
            }
            None => Err(OpusError(error.min(-1))),
        }
    }

    /// Decodes one packet into `out` (interleaved), and returns the frames decoded. `out`
    /// must hold the packet's frames: 5760 per channel (120 ms) is always enough.
    pub fn decode_float(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, OpusError> {
        let len = i32::try_from(packet.len()).map_err(|_| OpusError(-1))?;
        let frame_size = c_int::try_from(out.len() / self.channels).map_err(|_| OpusError(-1))?;
        // An empty packet is libopus's "lost packet" call, which wants a null pointer.
        let data = if packet.is_empty() {
            std::ptr::null()
        } else {
            packet.as_ptr()
        };
        // SAFETY: `data` is `len` readable bytes (or null with len 0); `out` has room for
        // `frame_size` frames of `channels` samples, which is all libopus writes.
        let n = unsafe {
            opus_decode_float(
                self.raw.as_ptr(),
                data,
                len,
                out.as_mut_ptr(),
                frame_size,
                0,
            )
        };
        usize::try_from(n).map_err(|_| OpusError(n))
    }

    /// The decoder's output gain, in Q7.8 dB (an OpusHead's output gain).
    pub fn set_gain(&mut self, q78_db: i32) -> Result<(), OpusError> {
        // SAFETY: OPUS_SET_GAIN takes one opus_int32 (an i32) after the request.
        let r = unsafe { opus_decoder_ctl(self.raw.as_ptr(), OPUS_SET_GAIN_REQUEST, q78_db) };
        check(r)
    }

    /// Forgets the previous packets (after a seek), keeping the settings.
    pub fn reset(&mut self) -> Result<(), OpusError> {
        // SAFETY: OPUS_RESET_STATE takes no argument.
        let r = unsafe { opus_decoder_ctl(self.raw.as_ptr(), OPUS_RESET_STATE) };
        check(r)
    }
}

fn check(r: c_int) -> Result<(), OpusError> {
    if r == OPUS_OK {
        Ok(())
    } else {
        Err(OpusError(r))
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from opus_decoder_create and is freed once, here.
        unsafe { opus_decoder_destroy(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 20 ms CELT packet of digital silence (TOC 0xfc: config 31, stereo, one frame).
    const SILENCE: [u8; 3] = [0xfc, 0xff, 0xfe];

    #[test]
    fn decodes_a_packet() {
        let mut d = OpusDecoder::new(48_000, 2).unwrap();
        let mut out = vec![1.0f32; 5760 * 2];
        let n = d.decode_float(&SILENCE, &mut out).unwrap();
        assert_eq!(n, 960, "20 ms at 48 kHz");
        assert!(out[..n * 2].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn a_lost_packet_is_concealed() {
        let mut d = OpusDecoder::new(48_000, 2).unwrap();
        let mut out = vec![0.0f32; 5760 * 2];
        d.decode_float(&SILENCE, &mut out).unwrap();
        // Empty: libopus fills in as many frames as `out` holds (a lost packet's length).
        let mut lost = vec![0.0f32; 960 * 2];
        assert_eq!(d.decode_float(&[], &mut lost).unwrap(), 960);
    }

    #[test]
    fn controls_and_errors() {
        assert!(OpusDecoder::new(44_100, 2).is_err(), "not an Opus rate");
        assert!(OpusDecoder::new(48_000, 3).is_err(), "not 1 or 2 channels");
        let mut d = OpusDecoder::new(48_000, 2).unwrap();
        d.set_gain(-512).unwrap();
        assert!(d.set_gain(40_000).is_err(), "out of range");
        d.reset().unwrap();
        let mut small = [0.0f32; 4];
        assert!(
            d.decode_float(&SILENCE, &mut small).is_err(),
            "no room for the packet"
        );
    }
}
