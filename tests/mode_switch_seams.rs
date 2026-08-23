//! RFC 6716 §4.5 configuration-switch seams WITHOUT redundancy,
//! gated against the §A reference listing decoder on synthetic
//! switch streams (`tests/fixtures/switch-*.bits`, see the fixtures
//! README):
//!
//! * **Hybrid → WB SILK** — normative: "requires adding in the final
//!   contents of the CELT overlap buffer to the first SILK-only
//!   packet … by decoding a 2.5 ms silence frame with the CELT
//!   decoder" (Figure 18 `H -> c + S`). Gate: the seam is at the
//!   reference floor (the flush reproduces the listing's overlap
//!   tail sample-for-sample).
//! * **SILK NB ↔ WB** — a SILK bandwidth change without redundancy:
//!   the synthesis and resampler states re-create at the new rate
//!   while the §4.2.8 output-buffering delay sample carries. Gate:
//!   **bit-exact** whole-stream.
//! * **CELT ↔ SILK / Hybrid** — non-normative; §4.5 "RECOMMENDED that
//!   the decoder use a concealment technique (e.g., make use of a PLC
//!   algorithm) to fill in the gap". The decoder extrapolates 5 ms
//!   from the previous mode and crossfades it into the new-mode
//!   frame's head (Figure 19 `P & …`). Concealment algorithms are
//!   implementation choices, so the gate is a whole-stream floor that
//!   a hard switch fails (measured: hard switch ≈ 27 dB; with the
//!   fill 34–40 dB; everything away from the seam at ≥ 97 dB).

use oxideav_opus::OpusDecoder;

fn capture_packets(bits: &[u8]) -> Vec<Vec<u8>> {
    let mut off = 0usize;
    let mut out = Vec::new();
    while off + 8 <= bits.len() {
        let len = u32::from_be_bytes(bits[off..off + 4].try_into().unwrap()) as usize;
        off += 8;
        out.push(bits[off..off + len].to_vec());
        off += len;
    }
    assert_eq!(off, bits.len(), "trailing bytes in the capture");
    out
}

fn pcm_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn snr_db(want: &[i16], got: &[i16]) -> f64 {
    let n = want.len().min(got.len());
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    for i in 0..n {
        let w = f64::from(want[i]);
        let d = w - f64::from(got[i]);
        sig += w * w;
        err += d * d;
    }
    if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sig / err).log10()
    }
}

/// Decode a switch capture at 48 kHz and return `(ours, reference)`.
fn decode_switch(bits: &[u8], expected: &[u8]) -> (Vec<i16>, Vec<i16>) {
    let packets = capture_packets(bits);
    assert_eq!(packets.len(), 30, "fixture shape: 15 + 15 packets");
    let mut dec = OpusDecoder::new();
    let mut pcm = Vec::new();
    for (k, p) in packets.iter().enumerate() {
        let out = dec
            .decode_packet(p)
            .unwrap_or_else(|e| panic!("packet {k}: {e:?}"));
        assert_eq!(out.samples_per_channel(), 960, "packet {k}");
        assert_eq!(out.channels, 1, "packet {k}");
        pcm.extend_from_slice(&out.pcm);
    }
    let want = pcm_i16(expected);
    assert_eq!(pcm.len(), want.len());
    (pcm, want)
}

/// SNR over the ±10 ms window around the seam (packet 15's start).
fn seam_snr(ours: &[i16], want: &[i16]) -> f64 {
    let seam = 15 * 960;
    snr_db(&want[seam - 480..seam + 480], &ours[seam - 480..seam + 480])
}

#[test]
fn hybrid_to_wb_silk_flushes_the_celt_overlap() {
    let (ours, want) = decode_switch(
        include_bytes!("fixtures/switch-hybrid-to-silkwb.bits"),
        include_bytes!("fixtures/switch-hybrid-to-silkwb.expected48.pcm"),
    );
    let whole = snr_db(&want, &ours);
    let seam = seam_snr(&ours, &want);
    eprintln!("hybrid->silkwb: whole {whole:.1} dB, seam {seam:.1} dB");
    assert!(whole >= 100.0, "whole-stream {whole:.1} dB");
    assert!(
        seam >= 100.0,
        "seam {seam:.1} dB — the §4.5 overlap flush is missing"
    );
    // The SILK-only tail after the seam is bit-exact.
    assert_eq!(&ours[16 * 960..], &want[16 * 960..]);
}

#[test]
fn silk_bandwidth_changes_are_bit_exact() {
    for (bits, expected) in [
        (
            include_bytes!("fixtures/switch-silknb-to-silkwb.bits").as_slice(),
            include_bytes!("fixtures/switch-silknb-to-silkwb.expected48.pcm").as_slice(),
        ),
        (
            include_bytes!("fixtures/switch-silkwb-to-silknb.bits").as_slice(),
            include_bytes!("fixtures/switch-silkwb-to-silknb.expected48.pcm").as_slice(),
        ),
    ] {
        let (ours, want) = decode_switch(bits, expected);
        assert_eq!(ours, want, "SILK bandwidth switch must be bit-exact");
    }
}

#[test]
fn non_normative_switches_get_the_recommended_plc_fill() {
    // Floors a hard switch fails (≈ 27 dB whole-stream): the fill
    // removes the worst of the seam discontinuity even though the
    // concealment algorithms differ between decoders.
    for (name, bits, expected, floor) in [
        (
            "celt->silkwb",
            include_bytes!("fixtures/switch-celt-to-silkwb.bits").as_slice(),
            include_bytes!("fixtures/switch-celt-to-silkwb.expected48.pcm").as_slice(),
            32.0,
        ),
        (
            "silkwb->celt",
            include_bytes!("fixtures/switch-silkwb-to-celt.bits").as_slice(),
            include_bytes!("fixtures/switch-silkwb-to-celt.expected48.pcm").as_slice(),
            30.0,
        ),
        (
            "hybrid->celt",
            include_bytes!("fixtures/switch-hybrid-to-celt.bits").as_slice(),
            include_bytes!("fixtures/switch-hybrid-to-celt.expected48.pcm").as_slice(),
            30.0,
        ),
    ] {
        let (ours, want) = decode_switch(bits, expected);
        let whole = snr_db(&want, &ours);
        let seam = seam_snr(&ours, &want);
        // Away from the seam both halves sit at the reference floor.
        let pre = snr_db(&want[..14 * 960], &ours[..14 * 960]);
        let post = snr_db(&want[16 * 960..], &ours[16 * 960..]);
        eprintln!("{name}: whole {whole:.1} dB, seam {seam:.1} dB, pre {pre:.1}, post {post:.1}");
        assert!(
            whole >= floor,
            "{name}: whole-stream {whole:.1} dB < {floor}"
        );
        assert!(pre >= 90.0, "{name}: pre-seam {pre:.1} dB");
        assert!(post >= 90.0, "{name}: post-seam {post:.1} dB");
        assert!(seam >= 12.0, "{name}: seam {seam:.1} dB");
    }
}
