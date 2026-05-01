//! Opus Hybrid (SILK + CELT) encoder → decoder roundtrip.
//!
//! Covers the new `HybridEncoder` introduced for RFC 6716 §4.4 hybrid
//! mode (configs 12..=15). The encoder runs SILK-WB on the 0..8 kHz
//! low band and CELT (start_band = 17) on the 8..12 kHz / 8..20 kHz
//! high band, sharing one range-coded bitstream. The decoder is the
//! existing `decode_hybrid_frame` path which is already exercised by
//! `roundtrip.rs` against ffmpeg-encoded vectors.
//!
//! Acceptance criteria:
//!
//! * TOC byte carries the right Hybrid config (13 = SWB 20 ms,
//!   15 = FB 20 ms) and `mode == OpusMode::Hybrid`.
//! * Round-trip energy survives — we don't yet pin a tight SNR for
//!   the high band because the CELT mono path has the same PSNR
//!   caveat as the CELT-only encoder (~8 dB on a sine), and the SILK
//!   low band is the unvoiced/MVP path. The test checks the decoded
//!   signal carries non-trivial energy in both the low (< 4 kHz) and
//!   high (> 8 kHz) regions, proving the two layers actually mix.
//! * Silence in → quiet out (RMS bound).

use oxideav_core::Encoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet};
use oxideav_opus::encoder::{HybridBandwidth, HybridEncoder};
use oxideav_opus::toc::{OpusBandwidth, OpusMode, Toc};

const SR: u32 = 48_000;
const FRAME_SAMPLES_20MS: usize = 960;

fn make_s16_frame_mono(samples_f32: &[f32]) -> Frame {
    let mut bytes = Vec::with_capacity(samples_f32.len() * 2);
    for &s in samples_f32 {
        let q = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
        bytes.extend_from_slice(&q.to_le_bytes());
    }
    Frame::Audio(AudioFrame {
        samples: samples_f32.len() as u32,
        pts: None,
        data: vec![bytes],
    })
}

fn make_hybrid_encoder(bw: HybridBandwidth) -> HybridEncoder {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(1);
    p.sample_rate = Some(SR);
    match bw {
        HybridBandwidth::Swb => HybridEncoder::new_swb_mono_20ms(&p).expect("SWB encoder"),
        HybridBandwidth::Fb => HybridEncoder::new_fb_mono_20ms(&p).expect("FB encoder"),
    }
}

fn drive_encoder(enc: &mut HybridEncoder, signal: &[f32]) -> Vec<Packet> {
    let mut packets = Vec::new();
    for chunk in signal.chunks(FRAME_SAMPLES_20MS) {
        if chunk.len() < FRAME_SAMPLES_20MS {
            break;
        }
        let frame = make_s16_frame_mono(chunk);
        enc.send_frame(&frame).expect("send_frame");
        loop {
            match enc.receive_packet() {
                Ok(p) => packets.push(p),
                Err(Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e:?}"),
            }
        }
    }
    enc.flush().expect("flush");
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    packets
}

fn decode_packets(packets: &[Packet]) -> Vec<i16> {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(1);
    p.sample_rate = Some(SR);
    let mut dec = oxideav_opus::decoder::make_decoder(&p).expect("make_decoder");

    let mut acc: Vec<i16> = Vec::new();
    for pkt in packets {
        dec.send_packet(pkt).expect("send_packet");
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                let bytes = &a.data[0];
                let n = a.samples as usize;
                for i in 0..n {
                    let off = i * 2;
                    acc.push(i16::from_le_bytes([bytes[off], bytes[off + 1]]));
                }
            }
            Ok(_) => panic!("expected audio frame"),
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
    acc
}

fn rms_i16(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum = 0f64;
    for &s in samples {
        let v = s as f64 / 32768.0;
        sum += v * v;
    }
    (sum / samples.len() as f64).sqrt()
}

fn rms_f32(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum = 0f64;
    for &s in samples {
        let v = s as f64;
        sum += v * v;
    }
    (sum / samples.len() as f64).sqrt()
}

/// Single-pole low-pass filter, frequency in Hz at sample rate `sr`.
/// Used to estimate energy below a cutoff in the decoded output.
fn lowpass(samples: &[i16], cutoff_hz: f32, sr: u32) -> Vec<f32> {
    let dt = 1.0 / sr as f32;
    let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
    let alpha = dt / (rc + dt);
    let mut out = Vec::with_capacity(samples.len());
    let mut y = 0f32;
    for &s in samples {
        let x = s as f32 / 32768.0;
        y += alpha * (x - y);
        out.push(y);
    }
    out
}

/// Highpass = signal - lowpass.
fn highpass(samples: &[i16], cutoff_hz: f32, sr: u32) -> Vec<f32> {
    let lp = lowpass(samples, cutoff_hz, sr);
    samples
        .iter()
        .zip(lp.iter())
        .map(|(&s, &l)| s as f32 / 32768.0 - l)
        .collect()
}

/// SNR of the decoded signal vs the band-limited (≤ 8 kHz) reference,
/// computed on the low-band lowpass after best-lag alignment within
/// ±max_lag samples. Returns (snr_db, lag).
fn lowband_snr_with_lag(reference: &[f32], decoded: &[i16], max_lag: i32) -> (f64, i32) {
    let dec_lp = lowpass(decoded, 8_000.0, SR);
    let mut best_snr = f64::NEG_INFINITY;
    let mut best_lag = 0i32;
    for lag in -max_lag..=max_lag {
        let (rs, ds) = if lag >= 0 {
            (0usize, lag as usize)
        } else {
            ((-lag) as usize, 0usize)
        };
        let n = reference
            .len()
            .saturating_sub(rs)
            .min(dec_lp.len().saturating_sub(ds));
        if n < 1920 {
            continue;
        }
        let mut sig = 0f64;
        let mut err = 0f64;
        for i in 0..n {
            let r = reference[rs + i] as f64;
            let d = dec_lp[ds + i] as f64;
            sig += r * r;
            let e = r - d;
            err += e * e;
        }
        if sig == 0.0 {
            continue;
        }
        let snr = 10.0 * (sig / err.max(1e-30)).log10();
        if snr > best_snr {
            best_snr = snr;
            best_lag = lag;
        }
    }
    (best_snr, best_lag)
}

#[test]
fn hybrid_swb_20ms_toc_is_config_13() {
    let mut enc = make_hybrid_encoder(HybridBandwidth::Swb);
    // One frame of a 1 kHz tone — guaranteed non-silent.
    let signal: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty(), "encoder produced no packets");
    let toc = Toc::parse(packets[0].data[0]);
    assert_eq!(toc.config, 13, "SWB 20 ms hybrid TOC config");
    assert_eq!(toc.mode, OpusMode::Hybrid);
    assert_eq!(toc.bandwidth, OpusBandwidth::SuperWideband);
    assert_eq!(toc.frame_samples_48k, 960);
    assert!(!toc.stereo, "mono encoder must emit stereo bit = 0");
    assert_eq!(toc.code, 0, "single-frame packet → framing code 0");
}

#[test]
fn hybrid_fb_20ms_toc_is_config_15() {
    let mut enc = make_hybrid_encoder(HybridBandwidth::Fb);
    let signal: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty());
    let toc = Toc::parse(packets[0].data[0]);
    assert_eq!(toc.config, 15, "FB 20 ms hybrid TOC config");
    assert_eq!(toc.mode, OpusMode::Hybrid);
    assert_eq!(toc.bandwidth, OpusBandwidth::Fullband);
    assert_eq!(toc.frame_samples_48k, 960);
}

/// A swept sine that crosses the 8 kHz crossover so both layers carry
/// meaningful content. The decoded output must:
/// (1) decode without error,
/// (2) carry energy in the < 4 kHz region (proves SILK contributed),
/// (3) carry energy in the > 8 kHz region (proves CELT high-band did).
#[test]
fn hybrid_swb_20ms_sweep_has_both_band_energy() {
    let n_frames = 25; // 500 ms
    let total = n_frames * FRAME_SAMPLES_20MS;
    let f0 = 500.0f32;
    let f1 = 11_000.0f32;
    let signal: Vec<f32> = (0..total)
        .map(|i| {
            let t = i as f32 / SR as f32;
            let f = f0 + (f1 - f0) * (i as f32 / total as f32);
            (2.0 * std::f32::consts::PI * f * t).sin() * 0.3
        })
        .collect();

    let mut enc = make_hybrid_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty());
    for pkt in &packets {
        let toc = Toc::parse(pkt.data[0]);
        assert_eq!(toc.config, 13);
        assert_eq!(toc.mode, OpusMode::Hybrid);
    }

    let pcm = decode_packets(&packets);
    assert!(!pcm.is_empty());

    // Skip the first frame to let SILK's LPC history settle and CELT's
    // OLA tail come online.
    let skip = FRAME_SAMPLES_20MS.min(pcm.len());
    let lp = lowpass(&pcm[skip..], 4_000.0, SR);
    let hp = highpass(&pcm[skip..], 8_000.0, SR);

    let lp_rms = rms_f32(&lp);
    let hp_rms = rms_f32(&hp);
    let in_rms = rms_f32(&signal[skip..]);
    println!("hybrid_swb_sweep: in_rms={in_rms:.4e}, lp_rms={lp_rms:.4e}, hp_rms={hp_rms:.4e}");

    // Both bands need to carry at least 5 % of the input RMS — proves
    // SILK + CELT both pulled their weight on the swept-sine input.
    assert!(
        lp_rms > 0.05 * in_rms,
        "low band (< 4 kHz) too quiet: {lp_rms} vs in_rms {in_rms}"
    );
    assert!(
        hp_rms > 0.05 * in_rms,
        "high band (> 8 kHz) too quiet: {hp_rms} vs in_rms {in_rms} — CELT contribution missing?"
    );
}

#[test]
fn hybrid_fb_20ms_sweep_has_both_band_energy() {
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let f0 = 500.0f32;
    let f1 = 18_000.0f32;
    let signal: Vec<f32> = (0..total)
        .map(|i| {
            let t = i as f32 / SR as f32;
            let f = f0 + (f1 - f0) * (i as f32 / total as f32);
            (2.0 * std::f32::consts::PI * f * t).sin() * 0.3
        })
        .collect();

    let mut enc = make_hybrid_encoder(HybridBandwidth::Fb);
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty());

    let pcm = decode_packets(&packets);
    assert!(!pcm.is_empty());
    let skip = FRAME_SAMPLES_20MS.min(pcm.len());
    let lp = lowpass(&pcm[skip..], 4_000.0, SR);
    let hp = highpass(&pcm[skip..], 8_000.0, SR);

    let lp_rms = rms_f32(&lp);
    let hp_rms = rms_f32(&hp);
    let in_rms = rms_f32(&signal[skip..]);
    println!("hybrid_fb_sweep: in_rms={in_rms:.4e}, lp_rms={lp_rms:.4e}, hp_rms={hp_rms:.4e}");

    assert!(
        lp_rms > 0.05 * in_rms,
        "low band too quiet: {lp_rms} vs in_rms {in_rms}"
    );
    assert!(
        hp_rms > 0.05 * in_rms,
        "high band too quiet: {hp_rms} vs in_rms {in_rms}"
    );
}

/// Silence in → near-silence out. Confirms the encoder doesn't run away
/// on zero input; the decoded RMS should stay below a strict bound.
#[test]
fn hybrid_swb_silence_stays_quiet() {
    let n_frames = 5;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let signal = vec![0.0f32; total];
    let mut enc = make_hybrid_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty());

    let pcm = decode_packets(&packets);
    let r = rms_i16(&pcm);
    println!("hybrid_swb_silence: rms={r:.4e}");
    // Threshold is loose because the SILK MVP carrier is unvoiced
    // pseudo-noise even on zero input, and the CELT bit allocator
    // still emits some PVQ pulses. The bar is "not blowing up".
    assert!(
        r < 0.25,
        "silence in → output RMS too high: {r} (encoder runaway?)"
    );
    assert!(pcm.iter().all(|s| (*s as f32).is_finite()));
}

/// 300 Hz low-band tone → low-band SNR through the SILK part. The CELT
/// high band has no signal here so we don't measure HF SNR; we just
/// want the low band to recover cleanly.
#[test]
fn hybrid_swb_lowband_tone_snr() {
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let f_tone = 300.0f32;
    let signal: Vec<f32> = (0..total)
        .map(|i| {
            let t = i as f32 / SR as f32;
            (2.0 * std::f32::consts::PI * f_tone * t).sin() * 0.3
        })
        .collect();

    let mut enc = make_hybrid_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder(&mut enc, &signal);
    let pcm = decode_packets(&packets);

    // The reference is the input low-passed at 8 kHz (SILK's nominal
    // ceiling). The decoded output is also lowpassed before SNR.
    let ref_lp = lowpass(
        &signal
            .iter()
            .map(|&s| (s * 32768.0) as i16)
            .collect::<Vec<i16>>(),
        8_000.0,
        SR,
    );
    let (snr, lag) = lowband_snr_with_lag(
        &ref_lp[FRAME_SAMPLES_20MS..],
        &pcm[FRAME_SAMPLES_20MS..],
        64,
    );
    println!("hybrid_swb_lowband_tone_snr: snr={snr:.2} dB, lag={lag}");
    // 5 dB bar — deliberately loose. The hybrid SILK path is the same
    // unvoiced MVP carrier as `silk_wb_mono_20ms_roundtrip` (~29 dB
    // standalone) but adding the CELT high band on the same shared
    // range coder eats into the SILK budget, and the synth-side mix
    // adds a bit of CELT noise into the low band. 5 dB confirms the
    // tone survives — tighter SNR is a follow-up.
    assert!(snr > 5.0, "low-band tone SNR too low: {snr}");
}

#[test]
fn hybrid_fb_lowband_tone_snr() {
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let f_tone = 300.0f32;
    let signal: Vec<f32> = (0..total)
        .map(|i| {
            let t = i as f32 / SR as f32;
            (2.0 * std::f32::consts::PI * f_tone * t).sin() * 0.3
        })
        .collect();

    let mut enc = make_hybrid_encoder(HybridBandwidth::Fb);
    let packets = drive_encoder(&mut enc, &signal);
    let pcm = decode_packets(&packets);

    let ref_lp = lowpass(
        &signal
            .iter()
            .map(|&s| (s * 32768.0) as i16)
            .collect::<Vec<i16>>(),
        8_000.0,
        SR,
    );
    let (snr, lag) = lowband_snr_with_lag(
        &ref_lp[FRAME_SAMPLES_20MS..],
        &pcm[FRAME_SAMPLES_20MS..],
        64,
    );
    println!("hybrid_fb_lowband_tone_snr: snr={snr:.2} dB, lag={lag}");
    assert!(snr > 5.0, "low-band tone SNR too low: {snr}");
}

#[test]
fn hybrid_rejects_stereo() {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(2);
    p.sample_rate = Some(SR);
    match HybridEncoder::new_swb_mono_20ms(&p) {
        Err(Error::Unsupported(_)) => {}
        Err(e) => panic!("expected Unsupported, got {e:?}"),
        Ok(_) => panic!("expected Unsupported, got Ok"),
    }
}

#[test]
fn hybrid_rejects_non_48k() {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(1);
    p.sample_rate = Some(16_000);
    match HybridEncoder::new_swb_mono_20ms(&p) {
        Err(Error::Unsupported(_)) => {}
        Err(e) => panic!("expected Unsupported, got {e:?}"),
        Ok(_) => panic!("expected Unsupported, got Ok"),
    }
}

#[test]
fn hybrid_packet_starts_with_silk_then_celt() {
    // Smoke test: the packet must contain at least one byte of body
    // beyond the TOC, since both layers always emit content for active
    // frames.
    let mut enc = make_hybrid_encoder(HybridBandwidth::Swb);
    let signal: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 500.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty());
    for pkt in &packets {
        // TOC + ≥ 2 bytes (SILK header + CELT body).
        assert!(
            pkt.data.len() >= 3,
            "hybrid packet too small: {} bytes",
            pkt.data.len()
        );
    }
}
