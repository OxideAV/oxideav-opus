//! Complexity-ladder integration gates.
//!
//! RFC 6716 leaves encoder complexity entirely free, so the ladder's
//! rungs are documented crate choices mapping one `set_complexity`
//! knob (0..=10) onto the measured election machinery:
//!
//! * CELT: `0..=1` skips the §5.3.1 pitch pre-filter analysis,
//!   `2..=7` runs its full decision ladder (the untouched default),
//!   `8..=10` arms the §5.3.1 tapset election.
//! * SILK: `0..=4` the single-state §5.2.3.8 quantiser (the
//!   untouched default), `5..=7` a 2-state trellis, `8..=10` the full
//!   4-state trellis.
//! * Hybrid: the SILK mapping on its SILK layer (Hybrid frames never
//!   run the pre-filter).
//!
//! The gates pin: default bit-identity at the documented default
//! rung, measured monotone quality up the ladder at equal rate, and
//! that every rung's stream decodes with exact accounting.

use oxideav_opus::celt_packet_encode::CeltEncoder;
use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::silk_encoder::SilkEncoderMono;
use oxideav_opus::toc::Bandwidth;
use oxideav_opus::vbr::{CeltVbrEncoder, HybridVbrEncoderMono};

/// Strongly periodic 48 kHz content (short pitch period — both the
/// pre-filter and the sharper tapsets win on it).
fn periodic_48k(seconds: f64) -> Vec<i16> {
    let n = (48000.0 * seconds) as usize;
    let mut out = vec![0i16; n];
    let mut phase = 0.0f64;
    let (mut y1, mut y2) = (0.0f64, 0.0f64);
    for slot in out.iter_mut() {
        let f0 = 48000.0 / 60.0;
        phase += f0 / 48000.0;
        if phase >= 1.0 {
            phase -= 1.0;
        }
        let x = if phase < f0 / 48000.0 * 2.0 { 1.0 } else { 0.0 };
        let w = 2.0 * std::f64::consts::PI * 800.0 / 48000.0;
        let r = 0.92;
        let y = x + 2.0 * r * w.cos() * y1 - r * r * y2;
        y2 = y1;
        y1 = y;
        *slot = (2200.0 * y).clamp(-30000.0, 30000.0) as i16;
    }
    out
}

/// Speech-like 16 kHz content for the SILK rungs.
fn speech_like_16k(seconds: f64) -> Vec<f32> {
    let n = (16000.0 * seconds) as usize;
    let mut out = vec![0.0f32; n];
    let mut lcg = 0x1234_5678u32;
    let mut phase = 0.0f64;
    let (mut y1, mut y2) = (0.0f64, 0.0f64);
    for (i, slot) in out.iter_mut().enumerate() {
        let t = i as f64 / 16000.0;
        let voiced = (t / 0.75).fract() < 0.6667;
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = ((lcg >> 16) as f64 - 32768.0) / 32768.0;
        let x = if voiced {
            let f0 = 90.0 + 50.0 * (t * 0.9).sin().abs();
            phase += f0 / 16000.0;
            if phase >= 1.0 {
                phase -= 1.0;
            }
            let pulse = if phase < f0 / 16000.0 * 1.5 { 1.0 } else { 0.0 };
            pulse + 0.02 * noise
        } else {
            0.25 * noise
        };
        let w = 2.0 * std::f64::consts::PI * 500.0 / 16000.0;
        let r = 0.95;
        let y = x + 2.0 * r * w.cos() * y1 - r * r * y2;
        y2 = y1;
        y1 = y;
        *slot = (0.18 * y) as f32;
    }
    out
}

/// One CELT arm at a complexity rung: fixed 40 B payload, decode SNR
/// against the 120-sample-delay-aligned input, pf-firing count, and
/// the packets.
fn celt_arm(pcm: &[i16], complexity: Option<u8>) -> (f64, usize, Vec<Vec<u8>>) {
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    if let Some(c) = complexity {
        enc.set_complexity(c);
    }
    let mut dec = OpusDecoder::new();
    let mut hist = vec![0i16; 120];
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    let mut pf = 0usize;
    let mut packets = Vec::new();
    for chunk in pcm.chunks_exact(960) {
        let (packet, info) = enc.encode_packet(chunk, 40).unwrap();
        if info.postfilter_on {
            pf += 1;
        }
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.samples_per_channel(), 960);
        let mut reference = hist.clone();
        reference.extend_from_slice(&chunk[..960 - 120]);
        for (&r, &t) in reference.iter().zip(out.pcm.iter()) {
            sig += f64::from(r) * f64::from(r);
            err += (f64::from(r) - f64::from(t)).powi(2);
        }
        hist.copy_from_slice(&chunk[960 - 120..]);
        packets.push(packet);
    }
    (10.0 * (sig / err).log10(), pf, packets)
}

/// The CELT rungs: 0 (pf off) < 4 (default, pf on) < 10 (tapset
/// election) in measured quality at equal rate; the untouched
/// encoder is bit-identical to the documented default rung.
#[test]
fn celt_ladder_is_monotone_and_default_rung_is_bit_identical() {
    let pcm = periodic_48k(1.0);
    let (snr_0, pf_0, _) = celt_arm(&pcm, Some(0));
    let (snr_4, pf_4, p_4) = celt_arm(&pcm, Some(4));
    let (snr_10, pf_10, _) = celt_arm(&pcm, Some(10));
    let (_, _, p_default) = celt_arm(&pcm, None);

    // Rung 0 skips the pre-filter entirely; the higher rungs fire it
    // on this strongly periodic content.
    assert_eq!(pf_0, 0, "rung 0 must not run the pre-filter");
    assert!(pf_4 > 40 && pf_10 > 40, "pf must fire: {pf_4} / {pf_10}");

    // Measured monotone quality at equal rate (on this content at
    // 40 B: 14.6 / 19.1 / 20.3 dB — the pre-filter buys +4.5 dB, the
    // election +1.2 dB more; gated with headroom).
    assert!(
        snr_4 >= snr_0 + 0.5,
        "pre-filter rung bought nothing: {snr_4:.2} vs {snr_0:.2} dB"
    );
    assert!(
        snr_10 >= snr_4 + 0.5,
        "election rung bought nothing: {snr_10:.2} vs {snr_4:.2} dB"
    );

    // Untouched == the documented default rung (bit-identical).
    assert_eq!(p_default, p_4, "untouched encoder must equal rung 4");
}

/// The SILK rungs: monotone measured quality at equal elected rate,
/// and untouched == the documented default rung.
#[test]
fn silk_ladder_is_monotone_and_default_rung_is_bit_identical() {
    let pcm = speech_like_16k(2.0);
    let run = |complexity: Option<u8>| -> (f64, f64, Vec<Vec<u8>>) {
        let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
        if let Some(c) = complexity {
            enc.set_complexity(c);
        }
        let mut packets = Vec::new();
        let mut recon: Vec<f32> = Vec::new();
        let mut bytes = 0usize;
        for chunk in pcm.chunks_exact(320) {
            let out = enc.encode_packet_elected(chunk, 40).unwrap();
            bytes += out.packet.len();
            recon.extend_from_slice(&out.reconstructed);
            packets.push(out.packet);
        }
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for (&r, &t) in pcm[..recon.len()].iter().zip(recon.iter()) {
            sig += f64::from(r) * f64::from(r);
            err += (f64::from(r) - f64::from(t)).powi(2);
        }
        (
            10.0 * (sig / err).log10(),
            bytes as f64 / packets.len() as f64,
            packets,
        )
    };
    let (snr_0, rate_0, p_0) = run(Some(0));
    let (snr_7, rate_7, _) = run(Some(7));
    let (snr_10, rate_10, _) = run(Some(10));
    let (_, _, p_default) = run(None);
    let (_, _, p_4) = run(Some(4));

    // Equal elected rate across the rungs.
    assert!((rate_7 - rate_0).abs() < 2.0 && (rate_10 - rate_0).abs() < 2.0);

    // Monotone measured quality (on this content at an elected 40 B:
    // 9.0 / 9.8 / 10.2 dB — 2 states take most of the trellis win, 4
    // the rest; ties allowed within the mirror-vs-stream tolerance).
    assert!(
        snr_7 >= snr_0 + 0.2,
        "2-state rung bought nothing: {snr_7:.2} vs {snr_0:.2} dB"
    );
    assert!(
        snr_10 >= snr_7 - 0.1,
        "4-state rung regressed: {snr_10:.2} vs {snr_7:.2} dB"
    );
    assert!(
        snr_10 >= snr_0 + 0.4,
        "the ladder top bought nothing: {snr_10:.2} vs {snr_0:.2} dB"
    );

    // Untouched == rung 4 == rung 0 (all the single-state quantiser
    // at default quality — the election only changes the quantiser).
    assert_eq!(p_default, p_4, "untouched encoder must equal rung 4");
    assert_eq!(p_default, p_0, "rungs 0..=4 share the single-state arm");
}

/// The Hybrid and VBR arms take the knob: every rung's stream
/// decodes with exact accounting, and the top rung genuinely changes
/// the coded packets (the trellis / election engages).
#[test]
fn hybrid_and_vbr_arms_take_the_complexity_knob() {
    // Hybrid VBR: FB 20 ms mono at 64 kb/s on periodic content.
    let pcm48: Vec<i16> = periodic_48k(1.0);
    let run_hybrid = |c: u8| -> Vec<Vec<u8>> {
        let mut enc = HybridVbrEncoderMono::new(Bandwidth::Fb, 200, 64000, false).unwrap();
        enc.set_complexity(c);
        let mut dec = OpusDecoder::new();
        let mut packets = Vec::new();
        for chunk in pcm48.chunks_exact(960) {
            let packet = enc.encode_frame(chunk).unwrap();
            assert_eq!(
                dec.decode_packet(&packet).unwrap().samples_per_channel(),
                960
            );
            packets.push(packet);
        }
        packets
    };
    let h_0 = run_hybrid(0);
    let h_10 = run_hybrid(10);
    assert_ne!(h_0, h_10, "the trellis must engage in the Hybrid arm");

    // CELT VBR: the election rung engages inside the elected encodes.
    let run_celt_vbr = |c: u8| -> Vec<Vec<u8>> {
        let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, 16000, false).unwrap();
        enc.set_complexity(c);
        let mut dec = OpusDecoder::new();
        let mut packets = Vec::new();
        for chunk in pcm48.chunks_exact(960) {
            let (packet, _) = enc.encode_frame(chunk).unwrap();
            assert_eq!(
                dec.decode_packet(&packet).unwrap().samples_per_channel(),
                960
            );
            packets.push(packet);
        }
        packets
    };
    let c_0 = run_celt_vbr(0);
    let c_10 = run_celt_vbr(10);
    assert_ne!(c_0, c_10, "the ladder must engage in the CELT VBR arm");
}
