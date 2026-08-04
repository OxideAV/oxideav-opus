//! Hybrid (SILK low band + CELT high band) encode → decode roundtrips
//! through the crate's own streaming decoder.
//!
//! The two layers are aligned on one timeline (see
//! `hybrid_packet_encode`): the whole encode→decode chain delays by
//! exactly 120 samples at 48 kHz, verified here by an SNR gate at that
//! lag against the input.

use oxideav_opus::decoder::{FrameDecodeStatus, OpusDecoder};
use oxideav_opus::hybrid_packet_encode::{HybridEncoderMono, HybridEncoderStereo};
use oxideav_opus::toc::Bandwidth;

const DELAY: usize = 120;

/// Broadband deterministic signal: voice-band tones + high-band tones
/// crossing the 8 kHz layer split.
fn sig(i: usize) -> f64 {
    let t = i as f64 / 48000.0;
    6000.0 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()
        + 3000.0 * (2.0 * std::f64::consts::PI * 880.0 * t + 0.4).sin()
        + 2000.0 * (2.0 * std::f64::consts::PI * 3300.0 * t + 1.0).sin()
        + 1500.0 * (2.0 * std::f64::consts::PI * 10500.0 * t + 0.2).sin()
        + 1000.0 * (2.0 * std::f64::consts::PI * 14700.0 * t + 2.0).sin()
}

fn roundtrip(bw: Bandwidth, tenths: u16, payload: usize, frames: usize) -> f64 {
    let mut enc = HybridEncoderMono::new(bw, tenths).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    let mut input: Vec<i16> = Vec::new();
    let mut decoded: Vec<i16> = Vec::new();
    for f in 0..frames {
        let pcm: Vec<i16> = (0..spf)
            .map(|j| sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        let packet = enc.encode_packet(&pcm, payload).expect("encode");
        assert_eq!(packet.len(), 1 + payload);
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::HybridDecoded,
            "frame {f}"
        );
        input.extend_from_slice(&pcm);
        decoded.extend_from_slice(&out.pcm);
    }
    // Settled-region SNR at the fixed 120-sample delay.
    let start = decoded.len() / 4;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for p in (start + DELAY)..decoded.len() {
        let x = f64::from(input[p - DELAY]);
        let d = f64::from(decoded[p]) - x;
        num += d * d;
        den += x * x;
    }
    10.0 * (den / num.max(1e-30)).log10()
}

#[test]
fn hybrid_fb_20ms_roundtrips() {
    let snr = roundtrip(Bandwidth::Fb, 200, 240, 25);
    println!("hybrid fb 20ms 96kbps: {snr:.2} dB");
    assert!(snr > 9.0, "snr {snr}");
}

#[test]
fn hybrid_fb_20ms_high_rate_roundtrips() {
    let snr = roundtrip(Bandwidth::Fb, 200, 280, 25);
    println!("hybrid fb 20ms 112kbps: {snr:.2} dB");
    assert!(snr > 14.0, "snr {snr}");
}

#[test]
fn hybrid_swb_20ms_roundtrips() {
    let snr = roundtrip(Bandwidth::Swb, 200, 240, 25);
    println!("hybrid swb 20ms: {snr:.2} dB");
    assert!(snr > 10.0, "snr {snr}");
}

#[test]
fn hybrid_fb_10ms_roundtrips() {
    let snr = roundtrip(Bandwidth::Fb, 100, 150, 50);
    println!("hybrid fb 10ms: {snr:.2} dB");
    assert!(snr > 9.0, "snr {snr}");
}

#[test]
fn hybrid_low_band_only_content_leans_on_silk() {
    // Content entirely under 8 kHz: the SILK layer carries it and the
    // CELT bands stay near-silent; the frame must still decode as
    // Hybrid with reasonable fidelity.
    let mut enc = HybridEncoderMono::new(Bandwidth::Fb, 200).unwrap();
    let mut dec = OpusDecoder::new();
    let mut input: Vec<i16> = Vec::new();
    let mut decoded: Vec<i16> = Vec::new();
    for f in 0..20 {
        let pcm: Vec<i16> = (0..960)
            .map(|j| {
                let t = (f * 960 + j) as f64 / 48000.0;
                ((2.0 * std::f64::consts::PI * 330.0 * t).sin() * 9000.0) as i16
            })
            .collect();
        let packet = enc.encode_packet(&pcm, 220).unwrap();
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::HybridDecoded
        );
        input.extend_from_slice(&pcm);
        decoded.extend_from_slice(&out.pcm);
    }
    let start = decoded.len() / 4;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for p in (start + DELAY)..decoded.len() {
        let x = f64::from(input[p - DELAY]);
        let d = f64::from(decoded[p]) - x;
        num += d * d;
        den += x * x;
    }
    let snr = 10.0 * (den / num.max(1e-30)).log10();
    println!("hybrid low-band tone: {snr:.2} dB");
    assert!(snr > 10.0, "snr {snr}");
}

#[test]
fn hybrid_rejects_busted_budget() {
    // A payload too small for the (rate-uncontrolled) SILK layer must
    // error rather than emit a corrupt packet.
    let mut enc = HybridEncoderMono::new(Bandwidth::Fb, 200).unwrap();
    let pcm: Vec<i16> = (0..960)
        .map(|j| ((j as f64 * 0.05).sin() * 8000.0) as i16)
        .collect();
    assert!(enc.encode_packet(&pcm, 20).is_err());
}

#[test]
fn hybrid_elected_payload_honours_roomy_elections_exactly() {
    // When the election leaves room past the SILK floor, the packet
    // is exactly the elected size and decodes as Hybrid.
    let mut enc = HybridEncoderMono::new(Bandwidth::Fb, 200).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    for f in 0..10 {
        let pcm: Vec<i16> = (0..spf)
            .map(|j| sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        let packet = enc.encode_packet_elected(&pcm, 400).expect("encode");
        assert_eq!(packet.len(), 401, "roomy election must be exact");
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::HybridDecoded
        );
    }
}

#[test]
fn hybrid_elected_payload_raises_starving_elections_to_the_floor() {
    // A 2-byte election cannot carry the SILK layer: the size is
    // raised to the floor instead of erroring, and every raised
    // packet still decodes as Hybrid with the exact sample count —
    // across all four Hybrid configs (SWB/FB × 10/20 ms).
    for bw in [Bandwidth::Swb, Bandwidth::Fb] {
        for tenths in [100u16, 200] {
            let mut enc = HybridEncoderMono::new(bw, tenths).unwrap();
            let spf = enc.frame_samples();
            let mut dec = OpusDecoder::new();
            for f in 0..10 {
                let pcm: Vec<i16> = (0..spf)
                    .map(|j| sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
                    .collect();
                let packet = enc.encode_packet_elected(&pcm, 2).expect("floor raise");
                assert!(packet.len() > 3, "floor packet is SILK-sized");
                assert!(packet.len() <= 1276);
                let out = dec.decode_packet(&packet).expect("decode");
                assert_eq!(out.samples_per_channel(), spf);
                assert_eq!(
                    out.frame_outcomes[0].status,
                    FrameDecodeStatus::HybridDecoded,
                    "bw {bw:?} tenths {tenths} frame {f}"
                );
            }
        }
    }
}

/// Amplitude-panned broadband stereo pair for the stereo Hybrid arm.
fn stereo_sig(i: usize) -> (f64, f64) {
    let base = sig(i);
    let t = i as f64 / 48000.0;
    let hi = 800.0 * (2.0 * std::f64::consts::PI * 12500.0 * t + 0.9).sin();
    (base + hi, 0.5 * base - hi)
}

fn stereo_roundtrip(bw: Bandwidth, tenths: u16, payload: usize, frames: usize) -> (f64, f64) {
    let mut enc = HybridEncoderStereo::new(bw, tenths).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    let mut input: Vec<i16> = Vec::new();
    let mut decoded: Vec<i16> = Vec::new();
    for f in 0..frames {
        let mut pcm = Vec::with_capacity(2 * spf);
        for j in 0..spf {
            let (l, r) = stereo_sig(f * spf + j);
            pcm.push(l.round().clamp(-32768.0, 32767.0) as i16);
            pcm.push(r.round().clamp(-32768.0, 32767.0) as i16);
        }
        let packet = enc.encode_packet(&pcm, payload).expect("encode");
        assert_eq!(packet.len(), 1 + payload);
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        assert_eq!(out.channels, 2);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::HybridDecoded,
            "frame {f}"
        );
        input.extend_from_slice(&pcm);
        decoded.extend_from_slice(&out.pcm);
    }
    // Per-channel settled-region SNR at the 120-sample delay.
    let n = decoded.len() / 2;
    let start = n / 4;
    let mut snrs = [0.0f64; 2];
    for (c, snr) in snrs.iter_mut().enumerate() {
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        for p in (start + DELAY)..n {
            let x = f64::from(input[(p - DELAY) * 2 + c]);
            let d = f64::from(decoded[p * 2 + c]) - x;
            num += d * d;
            den += x * x;
        }
        *snr = 10.0 * (den / num.max(1e-30)).log10();
    }
    (snrs[0], snrs[1])
}

#[test]
fn hybrid_stereo_fb_20ms_roundtrips() {
    let (l, r) = stereo_roundtrip(Bandwidth::Fb, 200, 360, 25);
    println!("hybrid stereo fb 20ms 144kbps: L {l:.2} dB / R {r:.2} dB");
    assert!(l > 8.0 && r > 8.0, "L {l} R {r}");
}

#[test]
fn hybrid_stereo_swb_20ms_roundtrips() {
    let (l, r) = stereo_roundtrip(Bandwidth::Swb, 200, 300, 25);
    println!("hybrid stereo swb 20ms: L {l:.2} dB / R {r:.2} dB");
    assert!(l > 8.0 && r > 8.0, "L {l} R {r}");
}

#[test]
fn hybrid_stereo_fb_10ms_roundtrips() {
    let (l, r) = stereo_roundtrip(Bandwidth::Fb, 100, 180, 40);
    println!("hybrid stereo fb 10ms: L {l:.2} dB / R {r:.2} dB");
    assert!(l > 7.0 && r > 7.0, "L {l} R {r}");
}

#[test]
fn hybrid_stereo_rate_ladder_is_monotone() {
    // (The low rung must clear the stereo SILK layer's natural size —
    // the documented rejection path — hence 280 bytes.)
    let lo = stereo_roundtrip(Bandwidth::Fb, 200, 330, 20);
    let hi = stereo_roundtrip(Bandwidth::Fb, 200, 500, 20);
    println!("hybrid stereo ladder: 330 B {lo:?} / 500 B {hi:?}");
    assert!(hi.0 + 1.0 >= lo.0 && hi.1 + 1.0 >= lo.1, "{lo:?} vs {hi:?}");
}

#[test]
fn hybrid_stereo_mid_only_content_decodes() {
    // Identical L and R: the side channel is silent, the §4.2.7.2
    // mid-only escape engages, and the packet still decodes to both
    // channels carrying the signal.
    let mut enc = HybridEncoderStereo::new(Bandwidth::Fb, 200).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    let mut input: Vec<i16> = Vec::new();
    let mut decoded: Vec<i16> = Vec::new();
    for f in 0..15 {
        let mut pcm = Vec::with_capacity(2 * spf);
        for j in 0..spf {
            let v = sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16;
            pcm.push(v);
            pcm.push(v);
        }
        let packet = enc.encode_packet(&pcm, 240).expect("encode");
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.channels, 2);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::HybridDecoded
        );
        input.extend_from_slice(&pcm);
        decoded.extend_from_slice(&out.pcm);
    }
    let n = decoded.len() / 2;
    let start = n / 4;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for p in (start + DELAY)..n {
        for c in 0..2 {
            let x = f64::from(input[(p - DELAY) * 2 + c]);
            let d = f64::from(decoded[p * 2 + c]) - x;
            num += d * d;
            den += x * x;
        }
    }
    let snr = 10.0 * (den / num.max(1e-30)).log10();
    println!("hybrid stereo mid-only content: {snr:.2} dB");
    assert!(snr > 9.0, "snr {snr}");
}

#[test]
fn hybrid_stereo_elected_payload_flows() {
    // Roomy elections are honoured exactly; starving ones floor-raise.
    let mut enc = HybridEncoderStereo::new(Bandwidth::Fb, 200).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    for f in 0..10 {
        let mut pcm = Vec::with_capacity(2 * spf);
        for j in 0..spf {
            let (l, r) = stereo_sig(f * spf + j);
            pcm.push(l.round().clamp(-32768.0, 32767.0) as i16);
            pcm.push(r.round().clamp(-32768.0, 32767.0) as i16);
        }
        let elected = if f % 2 == 0 { 400 } else { 10 };
        let packet = enc.encode_packet_elected(&pcm, elected).expect("encode");
        if f % 2 == 0 {
            assert_eq!(packet.len(), 401, "roomy election must be exact");
        } else {
            assert!(packet.len() > 11, "starving election must floor-raise");
        }
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
    }
}

#[test]
fn hybrid_stereo_rejects_bad_configs() {
    assert!(HybridEncoderStereo::new(Bandwidth::Nb, 200).is_err());
    assert!(HybridEncoderStereo::new(Bandwidth::Fb, 400).is_err());
    assert!(HybridEncoderStereo::new(Bandwidth::Fb, 200).is_ok());
}
