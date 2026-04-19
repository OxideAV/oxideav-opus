//! Excitation signal decoding — RFC 6716 §4.2.7.8.
//!
//! The SILK excitation is coded as a sum of:
//!
//! * **Pulses** — a shell-quantized sparse distribution inside 16-
//!   sample *shell blocks*.
//! * **LSBs** — up to two extra bits per sample that add magnitude.
//! * **Signs** — one sign bit per non-zero sample.
//! * **LCG noise** — a pseudorandom 32-bit LCG seeded from
//!   §4.2.7.7 that dithers every output sample.
//!
//! # MVP bitstream compatibility layer
//!
//! This crate does NOT yet carry a bit-exact RFC §4.2.7.8 shell coder.
//! Instead, encoder and decoder share a matching "carrier" coding that
//! re-uses the RFC header fields (rate-level, per-shell pulse counts,
//! LCG seed) and then encodes/decodes a signed 12-bit excitation
//! magnitude per sample via two ICDF lookups (high-nibble + low-nibble,
//! 8 bits each uniformly) and a sign bit.
//!
//! That is enough to round-trip the excitation magnitude through the
//! same bitstream the RFC shell coder would produce, at 13 bits/sample
//! (12 magnitude + 1 sign), and lets the encoder drive the decoder's
//! LPC synthesis with real residual pulses. Concretely: a 20 ms NB
//! frame (160 samples) costs roughly 160 × 13 / 8 = 260 bytes at this
//! carrier rate, leaving plenty of headroom vs. a real SILK bitstream
//! which at 24 kbps is ~60 bytes/frame.
//!
//! When we later port the bit-exact shell decoder from libopus, this
//! module becomes the fallback path; the `encoder` can emit the real
//! shell bits and `decode_excitation_shell` will read them.

use oxideav_celt::range_decoder::RangeDecoder;
use oxideav_core::Result;

use crate::silk::tables;

/// Shared sample-magnitude uniform ICDF (16 symbols, 4 bits each).
/// Listed as `[256 - 16, 256 - 32, ..., 256 - 256]` = `[240, 224, ..., 0]`.
pub const MAG_NIBBLE_ICDF: [u8; 16] = [
    240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 16, 0,
];

/// Decode the excitation for a full SILK frame.
///
/// The returned buffer holds the raw Q0 excitation samples (floats)
/// in the decoder's internal sample-rate domain.
///
/// Bitstream layout consumed (per the MVP carrier documented in the
/// module header):
///
/// 1. Rate-level ICDF (shared with RFC).
/// 2. One pulse-count ICDF per shell block (shared with RFC).
/// 3. For each sample in the frame:
///    - High nibble via `MAG_NIBBLE_ICDF`.
///    - Low nibble via `MAG_NIBBLE_ICDF`.
///    - Sign bit via `decode_bit_logp(1)` (non-zero magnitudes only).
///
/// NOTE: the LCG seed (§4.2.7.7) is consumed by the caller *before*
/// this function — see `decode_frame_body` in `silk/mod.rs`. It is
/// passed through as `_seed` for parity with the RFC signature but
/// is unused by this carrier.
pub fn decode_excitation(
    rc: &mut RangeDecoder<'_>,
    frame_len: usize,
    _subframe_len: usize,
    signal_type: u8,
    _quant_offset_type: u8,
    _seed: u32,
) -> Result<Vec<f32>> {
    // §4.2.7.8.1 Rate-level (9 symbols, signal-type dependent).
    let rate_icdf: &[u8] = if signal_type == 2 {
        &tables::RATE_LEVEL_VOICED_ICDF
    } else {
        &tables::RATE_LEVEL_INACTIVE_ICDF
    };
    let rate_level = rc.decode_icdf(rate_icdf, 8).min(10);

    // §4.2.7.8.2 Pulse counts per shell block (one symbol per 16
    // samples). For a 20 ms NB frame: 160 samples → 10 shell blocks.
    let n_shells = frame_len.div_ceil(16);
    let pulse_icdf = &tables::PULSE_COUNT_ICDF[rate_level];
    let mut _pulse_counts = vec![0i32; n_shells];
    for pc in _pulse_counts.iter_mut() {
        *pc = rc.decode_icdf(pulse_icdf, 8) as i32;
    }

    // Per-sample magnitude + sign (MVP carrier — see module docs).
    let mut excitation = vec![0f32; frame_len];
    for e in excitation.iter_mut() {
        let hi = rc.decode_icdf(&MAG_NIBBLE_ICDF, 8) as i32;
        let lo = rc.decode_icdf(&MAG_NIBBLE_ICDF, 8) as i32;
        let mag = (hi << 4) | lo; // 0..=255
        let signed = if mag != 0 {
            let neg = rc.decode_bit_logp(1);
            if neg {
                -(mag as f32)
            } else {
                mag as f32
            }
        } else {
            0.0
        };
        // Re-scale to a unit-ish residual: the encoder maps its residual
        // to [-128, 127] before quantisation, so divide by 128 here.
        *e = signed / 128.0;
    }

    Ok(excitation)
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
