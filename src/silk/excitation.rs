//! Excitation signal decoding — RFC 6716 §4.2.7.8.
//!
//! The SILK excitation is coded as a sum of:
//!
//! * **Pulses** — a shell-quantized sparse distribution inside 16-
//!   sample *shell blocks*.
//! * **LSBs** — up to ten extra bits per sample that double the
//!   magnitude range.
//! * **Signs** — one sign bit per non-zero sample.
//! * **LCG noise** — a pseudorandom 32-bit LCG seeded from
//!   §4.2.7.7 that dithers every output sample.
//!
//! This module is a thin wrapper that forwards to the real shell-pulse
//! coder in [`super::shell`], which implements the full RFC §4.2.7.8
//! bitstream layout (rate level + per-shell pulse counts with LSB
//! escapes + recursive pulse-location split + LSBs + signs).
//!
//! A deprecated 13-bit per-sample `MAG_NIBBLE_ICDF` carrier is retained
//! only for the encoder's old comparison test — production encode/decode
//! paths use the shell coder exclusively.

use oxideav_celt::range_decoder::RangeDecoder;
use oxideav_core::Result;

use crate::silk::shell;

/// Legacy 16-symbol uniform ICDF retained for the pre-shell-coder
/// bitrate comparison tests. The encoder no longer emits this.
#[doc(hidden)]
pub const MAG_NIBBLE_ICDF: [u8; 16] = [
    240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 16, 0,
];

/// Decode the excitation for a full SILK frame via the RFC §4.2.7.8
/// shell-pulse coder. Returns a `Vec<f32>` of length `frame_len` in
/// normalised Q0 excitation form (magnitude divided by 128).
pub fn decode_excitation(
    rc: &mut RangeDecoder<'_>,
    frame_len: usize,
    _subframe_len: usize,
    signal_type: u8,
    quant_offset_type: u8,
    seed: u32,
) -> Result<Vec<f32>> {
    // Shell blocks are always 16 samples. If frame_len is not a multiple
    // of 16 (10 ms MB = 120 samples, 10 ms WB also aligned via subframe
    // multiples) we round up and truncate on return — RFC §4.2.7.8.
    let aligned = frame_len.div_ceil(16) * 16;
    let mut out = shell::decode_excitation(rc, aligned, signal_type, quant_offset_type, seed);
    out.truncate(frame_len);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_celt::range_encoder::RangeEncoder;

    #[test]
    fn magnitude_nibble_roundtrip() {
        // Encode every possible nibble value and decode it back via
        // the shared ICDF. This pins the bitstream contract used by
        // the MVP carrier.
        for v in 0..16 {
            let mut enc = RangeEncoder::new(8);
            enc.encode_icdf(v, &MAG_NIBBLE_ICDF, 8);
            let buf = enc.done().unwrap();
            let mut dec = RangeDecoder::new(&buf);
            let got = dec.decode_icdf(&MAG_NIBBLE_ICDF, 8);
            assert_eq!(got, v);
        }
    }
}
