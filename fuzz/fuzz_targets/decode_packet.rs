#![no_main]

//! Coverage-guided fuzz harness for `OpusDecoder::decode_packet` — the
//! top-level RFC 6716 packet → PCM path (§3.1 TOC parse, §3.2 frame
//! split, §4.5 multi-frame loop, SILK / CELT / Hybrid routing, and all
//! the per-layer decode underneath).
//!
//! The input is carved into up to 8 consecutive packets fed to ONE
//! stateful decoder, so cross-packet state (the §4.5.2 mode-transition
//! resets, SILK synthesis / stereo-unmix histories, CELT overlap /
//! coarse-energy state) is exercised, not just single-shot decode.
//!
//! Contract under test: every byte sequence produces `Ok(DecodedAudio)`
//! or `Err(..)`. Panics, debug-mode integer overflows, and
//! index-out-of-bounds are all bugs — RFC 6716 §3.4 requires malformed
//! packets to be rejected, never to crash the decoder.

use libfuzzer_sys::fuzz_target;
use oxideav_opus::decoder::OpusDecoder;

/// §4.2.9 supported output rates the harness cycles through, so the
/// reduced-rate surface (SILK direct-to-rate resampling, CELT
/// Nyquist bound + decimation, seam fills on the reduced timeline) is
/// fuzzed alongside the 48 kHz path.
const RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte: low 3 bits = number of packets (1..=8) the rest is
    // split into; next bits select the decoder output rate.
    let n = 1 + (data[0] & 7) as usize;
    let rate = RATES[((data[0] >> 3) % 5) as usize];
    let body = &data[1..];
    let mut dec = OpusDecoder::with_output_rate(rate).expect("supported rate");
    if body.is_empty() {
        let _ = dec.decode_packet(body);
        return;
    }
    let chunk = body.len().div_ceil(n);
    for part in body.chunks(chunk.max(1)) {
        let _ = dec.decode_packet(part);
    }
    // Also exercise the Appendix-B self-delimited entry on the whole
    // body with the same (already warmed-up) state.
    let _ = dec.decode_self_delimited_packet(body);
});
