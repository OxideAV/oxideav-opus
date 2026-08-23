//! §4.4 packet-loss re-convergence gates against the reference
//! listing decoder, at 48 kHz and on the reduced-rate timeline.
//!
//! The `loss-*.bits` captures are reference-encoder streams with one
//! or two packets replaced by zero-length entries (the demo capture
//! format's "lost packet" convention, which the reference decoder
//! conceals with its own PLC). Concealment is non-normative — the two
//! decoders diverge across the hole by design — so the gates pin what
//! IS pinned: bit-exact / float-floor agreement before the loss, and
//! re-convergence after it (the §4.2.7.9 LTP and §4.3.7 overlap
//! state pulled back onto the reference trajectory by the coded
//! stream), identical in shape at every output rate.
//!
//! Measured (min over gated arms): SILK tail 52.1 dB, CELT tail
//! 101.8 dB, Hybrid tail 53.0 dB — the same at 48 kHz and 16 kHz.

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
    out
}

fn pcm_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn snr(want: &[i16], got: &[i16]) -> f64 {
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    for (w, g) in want.iter().zip(got.iter()) {
        let d = f64::from(*w) - f64::from(*g);
        sig += f64::from(*w) * f64::from(*w);
        err += d * d;
    }
    if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sig / err.max(1e-9)).log10()
    }
}

/// Decode a loss capture at `rate` (zero-length entries run the §4.4
/// hold via `decode_packet`'s DTX/lost path — same as the reference's
/// lost-packet handling) and gate the windows.
///
/// `loss_end`: first packet index after the concealed run;
/// `pre_exact`: whether the pre-loss region must be bit-exact;
/// `mid_db` / `tail_db`: floors for packets `loss_end+3..loss_end+10`
/// and `loss_end+10..end`.
#[allow(clippy::too_many_arguments)]
fn gate(
    bits: &[u8],
    expected: &[u8],
    rate: u32,
    loss_end: usize,
    pre_exact: bool,
    pre_db: f64,
    mid_db: f64,
    tail_db: f64,
) {
    let packets = capture_packets(bits);
    let mut dec = OpusDecoder::with_output_rate(rate).expect("rate");
    let mut pcm: Vec<i16> = Vec::new();
    for p in &packets {
        // A zero-length capture entry is a LOST packet (no bytes on
        // the wire, not even a TOC): the §4.4 conceal_loss entry point
        // stands in, exactly as the reference decoder conceals it.
        let out = if p.is_empty() {
            dec.conceal_loss()
        } else {
            dec.decode_packet(p).expect("decode")
        };
        assert_eq!(out.sample_rate_hz, rate);
        pcm.extend_from_slice(&out.pcm);
    }
    let want = pcm_i16(expected);
    assert_eq!(pcm.len(), want.len(), "sample accounting at {rate} Hz");
    let f = rate as usize / 50; // 20 ms frames

    if pre_exact {
        assert_eq!(
            &pcm[..20 * f],
            &want[..20 * f],
            "pre-loss region must be bit-exact at {rate} Hz"
        );
    } else {
        let pre = snr(&want[..20 * f], &pcm[..20 * f]);
        assert!(pre >= pre_db, "pre-loss {pre:.1} dB at {rate} Hz");
    }
    let mid = snr(
        &want[(loss_end + 3) * f..(loss_end + 10) * f],
        &pcm[(loss_end + 3) * f..(loss_end + 10) * f],
    );
    let tail = snr(&want[(loss_end + 10) * f..], &pcm[(loss_end + 10) * f..]);
    eprintln!("loss gate @{rate}: mid {mid:.1} dB, tail {tail:.1} dB");
    assert!(
        mid >= mid_db,
        "re-convergence {mid:.1} dB < {mid_db} at {rate} Hz"
    );
    assert!(
        tail >= tail_db,
        "tail {tail:.1} dB < {tail_db} at {rate} Hz"
    );
}

#[test]
fn silk_loss_reconverges_at_48k_and_16k() {
    let bits = include_bytes!("fixtures/loss-silkwb.bits");
    gate(
        bits,
        include_bytes!("fixtures/loss-silkwb.expected48000.pcm"),
        48_000,
        22,
        true,
        0.0,
        15.0,
        45.0,
    );
    gate(
        bits,
        include_bytes!("fixtures/loss-silkwb.expected16000.pcm"),
        16_000,
        22,
        true,
        0.0,
        15.0,
        45.0,
    );
}

#[test]
fn celt_loss_reconverges_on_the_reduced_timeline() {
    gate(
        include_bytes!("fixtures/loss-celt.bits"),
        include_bytes!("fixtures/loss-celt.expected16000.pcm"),
        16_000,
        21,
        false,
        90.0,
        40.0,
        90.0,
    );
}

#[test]
fn hybrid_loss_reconverges_on_the_reduced_timeline() {
    gate(
        include_bytes!("fixtures/loss-hybrid.bits"),
        include_bytes!("fixtures/loss-hybrid.expected16000.pcm"),
        16_000,
        21,
        true,
        0.0,
        25.0,
        45.0,
    );
}
