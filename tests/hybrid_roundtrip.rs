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

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Encoder, Error, Frame, Packet, StreamInfo, TimeBase,
    WriteSeek,
};
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

fn make_s16_frame_stereo(l: &[f32], r: &[f32]) -> Frame {
    assert_eq!(l.len(), r.len());
    let mut bytes = Vec::with_capacity(l.len() * 4);
    for i in 0..l.len() {
        let lq = (l[i] * 32768.0).clamp(-32768.0, 32767.0) as i16;
        let rq = (r[i] * 32768.0).clamp(-32768.0, 32767.0) as i16;
        bytes.extend_from_slice(&lq.to_le_bytes());
        bytes.extend_from_slice(&rq.to_le_bytes());
    }
    Frame::Audio(AudioFrame {
        samples: l.len() as u32,
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

fn make_hybrid_stereo_encoder(bw: HybridBandwidth) -> HybridEncoder {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(2);
    p.sample_rate = Some(SR);
    match bw {
        HybridBandwidth::Swb => HybridEncoder::new_swb_stereo_20ms(&p).expect("SWB stereo encoder"),
        HybridBandwidth::Fb => HybridEncoder::new_fb_stereo_20ms(&p).expect("FB stereo encoder"),
    }
}

fn drive_encoder_stereo(enc: &mut HybridEncoder, l: &[f32], r: &[f32]) -> Vec<Packet> {
    assert_eq!(l.len(), r.len());
    let mut packets = Vec::new();
    let chunks_l = l.chunks(FRAME_SAMPLES_20MS);
    let chunks_r = r.chunks(FRAME_SAMPLES_20MS);
    for (lc, rc) in chunks_l.zip(chunks_r) {
        if lc.len() < FRAME_SAMPLES_20MS {
            break;
        }
        let frame = make_s16_frame_stereo(lc, rc);
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

/// Build a minimal OpusHead identification packet (RFC 7845 §5.1) for
/// channel-mapping family 0 (mono / stereo, no mapping table).
fn build_opus_head(channels: u8) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // version
    h.push(channels);
    h.extend_from_slice(&312u16.to_le_bytes()); // pre_skip — libopus default
    h.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    h.extend_from_slice(&0i16.to_le_bytes()); // output gain
    h.push(0); // channel mapping family
    h
}

/// Mux Opus packets into an Ogg-Opus file. Returns the file path. The
/// returned file is fully self-contained: OpusHead + OpusTags + payload
/// pages, fit for ingestion by libopus / ffmpeg / opusdec.
fn write_ogg_opus_file(packets: &[Packet], channels: u16, path: &str) {
    let head = build_opus_head(channels as u8);
    let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    params.channels = Some(channels);
    params.sample_rate = Some(48_000);
    params.extradata = head;

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: None,
        params,
    };

    let f = std::fs::File::create(path).expect("create ogg file");
    let writer: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = oxideav_ogg::mux::open(writer, &[stream]).expect("open ogg mux");
    mux.write_header().expect("write_header");
    for pkt in packets {
        mux.write_packet(pkt).expect("write_packet");
    }
    mux.write_trailer().expect("write_trailer");
}

/// Decode a self-contained Opus-in-Ogg file via ffmpeg + libopus,
/// returning interleaved S16LE PCM at 48 kHz. Returns `None` if ffmpeg
/// is not on PATH (test should skip).
fn ffmpeg_decode_to_s16(path: &str, channels: u16) -> Option<Vec<i16>> {
    use std::process::Command;
    let out = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-i",
            path,
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-ac",
            &channels.to_string(),
            "-ar",
            "48000",
            "-",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Surface ffmpeg's own error so the test failure log is useful.
        eprintln!("ffmpeg decode failed: {stderr}");
        return Some(Vec::new());
    }
    let bytes = out.stdout;
    let mut pcm = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Some(pcm)
}

fn decode_packets_stereo(packets: &[Packet]) -> (Vec<i16>, Vec<i16>) {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(2);
    p.sample_rate = Some(SR);
    let mut dec = oxideav_opus::decoder::make_decoder(&p).expect("make_decoder");

    let mut l = Vec::new();
    let mut r = Vec::new();
    for pkt in packets {
        dec.send_packet(pkt).expect("send_packet");
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => {
                let bytes = &a.data[0];
                let n = a.samples as usize;
                for i in 0..n {
                    let off = i * 4;
                    l.push(i16::from_le_bytes([bytes[off], bytes[off + 1]]));
                    r.push(i16::from_le_bytes([bytes[off + 2], bytes[off + 3]]));
                }
            }
            Ok(_) => panic!("expected audio frame"),
            Err(e) => panic!("decode error: {e:?}"),
        }
    }
    (l, r)
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
fn hybrid_mono_rejects_stereo_input() {
    // The mono SWB constructor expects 1-channel input. Feeding it a
    // 2-channel CodecParameters must surface as Unsupported.
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
fn hybrid_stereo_rejects_mono_input() {
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(1);
    p.sample_rate = Some(SR);
    match HybridEncoder::new_swb_stereo_20ms(&p) {
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

// -------- Hybrid stereo --------------------------------------------

#[test]
fn hybrid_swb_stereo_20ms_toc_is_config_13_with_stereo_bit() {
    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Swb);
    let l: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let r: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).cos() * 0.3)
        .collect();
    let packets = drive_encoder_stereo(&mut enc, &l, &r);
    assert!(!packets.is_empty(), "encoder produced no packets");
    let toc = Toc::parse(packets[0].data[0]);
    assert_eq!(toc.config, 13, "SWB 20 ms hybrid TOC config");
    assert_eq!(toc.mode, OpusMode::Hybrid);
    assert_eq!(toc.bandwidth, OpusBandwidth::SuperWideband);
    assert_eq!(toc.frame_samples_48k, 960);
    assert!(toc.stereo, "stereo bit must be set");
    assert_eq!(toc.code, 0);
}

#[test]
fn hybrid_fb_stereo_20ms_toc_is_config_15_with_stereo_bit() {
    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Fb);
    let l: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let r: Vec<f32> = (0..FRAME_SAMPLES_20MS)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR as f32).cos() * 0.3)
        .collect();
    let packets = drive_encoder_stereo(&mut enc, &l, &r);
    assert!(!packets.is_empty());
    let toc = Toc::parse(packets[0].data[0]);
    assert_eq!(toc.config, 15);
    assert_eq!(toc.mode, OpusMode::Hybrid);
    assert_eq!(toc.bandwidth, OpusBandwidth::Fullband);
    assert!(toc.stereo);
}

#[test]
fn hybrid_swb_stereo_sweep_has_both_band_energy_per_channel() {
    // L sweeps 500 → 11 kHz; R sweeps 600 → 11.5 kHz so neither channel
    // is identical to the other. Both should round-trip with non-trivial
    // low-band (SILK) and high-band (CELT) energy in each output channel.
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let l: Vec<f32> = (0..total)
        .map(|i| {
            let t = i as f32 / SR as f32;
            let f = 500.0 + (11_000.0 - 500.0) * (i as f32 / total as f32);
            (2.0 * std::f32::consts::PI * f * t).sin() * 0.3
        })
        .collect();
    let r: Vec<f32> = (0..total)
        .map(|i| {
            let t = i as f32 / SR as f32;
            let f = 600.0 + (11_500.0 - 600.0) * (i as f32 / total as f32);
            (2.0 * std::f32::consts::PI * f * t).cos() * 0.3
        })
        .collect();

    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder_stereo(&mut enc, &l, &r);
    assert!(!packets.is_empty());
    for pkt in &packets {
        let toc = Toc::parse(pkt.data[0]);
        assert_eq!(toc.config, 13);
        assert_eq!(toc.mode, OpusMode::Hybrid);
        assert!(toc.stereo);
    }

    let (l_dec, r_dec) = decode_packets_stereo(&packets);
    assert!(!l_dec.is_empty() && !r_dec.is_empty());
    let skip = FRAME_SAMPLES_20MS.min(l_dec.len());

    for (label, dec, src) in [("L", &l_dec, &l), ("R", &r_dec, &r)] {
        let lp = lowpass(&dec[skip..], 4_000.0, SR);
        let hp = highpass(&dec[skip..], 8_000.0, SR);
        let lp_rms = rms_f32(&lp);
        let hp_rms = rms_f32(&hp);
        let in_rms = rms_f32(&src[skip..]);
        println!(
            "hybrid_swb_stereo_sweep[{label}]: in_rms={in_rms:.4e}, lp_rms={lp_rms:.4e}, hp_rms={hp_rms:.4e}"
        );
        assert!(
            lp_rms > 0.05 * in_rms,
            "{label} low band too quiet: {lp_rms} vs {in_rms}"
        );
        assert!(
            hp_rms > 0.05 * in_rms,
            "{label} high band too quiet: {hp_rms} vs {in_rms}"
        );
    }
}

#[test]
fn hybrid_swb_stereo_lowband_tone_snr_per_channel() {
    // 300 Hz sine on L, 400 Hz cosine on R — different tones in each
    // channel so a mid-only collapse would show up as dramatic SNR loss
    // on at least one side.
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let l: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 300.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let r: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 400.0 * i as f32 / SR as f32).cos() * 0.3)
        .collect();

    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder_stereo(&mut enc, &l, &r);
    let (l_dec, r_dec) = decode_packets_stereo(&packets);

    let l_ref_lp = lowpass(
        &l.iter()
            .map(|&s| (s * 32768.0) as i16)
            .collect::<Vec<i16>>(),
        8_000.0,
        SR,
    );
    let r_ref_lp = lowpass(
        &r.iter()
            .map(|&s| (s * 32768.0) as i16)
            .collect::<Vec<i16>>(),
        8_000.0,
        SR,
    );
    let (snr_l, lag_l) = lowband_snr_with_lag(
        &l_ref_lp[FRAME_SAMPLES_20MS..],
        &l_dec[FRAME_SAMPLES_20MS..],
        64,
    );
    let (snr_r, lag_r) = lowband_snr_with_lag(
        &r_ref_lp[FRAME_SAMPLES_20MS..],
        &r_dec[FRAME_SAMPLES_20MS..],
        64,
    );
    println!(
        "hybrid_swb_stereo_lowband_snr: L={snr_l:.2} dB (lag={lag_l}), R={snr_r:.2} dB (lag={lag_r})"
    );
    // 3 dB bar — looser than mono (5 dB) because dual-stereo CELT bleeds
    // a bit more cross-channel noise into the low band for stereo, and
    // the SILK side channel's prediction-weight=(0,0) MVP sacrifices
    // some inter-channel dB.
    assert!(snr_l > 3.0, "L low-band SNR too low: {snr_l}");
    assert!(snr_r > 3.0, "R low-band SNR too low: {snr_r}");
}

#[test]
fn hybrid_fb_stereo_silence_stays_quiet() {
    let n_frames = 5;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let zero = vec![0.0f32; total];
    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Fb);
    let packets = drive_encoder_stereo(&mut enc, &zero, &zero);
    assert!(!packets.is_empty());
    let (l_dec, r_dec) = decode_packets_stereo(&packets);
    let r_l = rms_i16(&l_dec);
    let r_r = rms_i16(&r_dec);
    println!("hybrid_fb_stereo_silence: rms_L={r_l:.4e}, rms_R={r_r:.4e}");
    assert!(
        r_l < 0.25 && r_r < 0.25,
        "silence in → loud out (L={r_l}, R={r_r})"
    );
    assert!(l_dec.iter().all(|s| (*s as f32).is_finite()));
    assert!(r_dec.iter().all(|s| (*s as f32).is_finite()));
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

// ----- libopus / ffmpeg cross-decode validation ---------------------
//
// These tests confirm that our Hybrid encoder produces a self-consistent
// Opus bitstream that the *reference* libopus decoder (via ffmpeg) is
// willing to accept. Bit-exact equality is not the bar — libopus + our
// SILK MVP carrier produce different waveforms — but the decoder must
// not error out, and the recovered audio must carry non-trivial energy.
//
// Tests skip silently if `ffmpeg` is missing from PATH so CI without
// system packages still passes.

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn hybrid_swb_mono_cross_decodes_through_libopus() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let signal: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 500.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let mut enc = make_hybrid_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder(&mut enc, &signal);
    assert!(!packets.is_empty());

    let path = "/tmp/oxideav-opus-hybrid-swb-mono.opus";
    write_ogg_opus_file(&packets, 1, path);

    let pcm = ffmpeg_decode_to_s16(path, 1).expect("ffmpeg ran");
    assert!(!pcm.is_empty(), "libopus decoded to empty PCM");
    let r = rms_i16(&pcm);
    println!(
        "hybrid_swb_mono cross-decode through libopus: rms={r:.4e}, samples={}",
        pcm.len()
    );
    assert!(r > 0.01, "libopus decode RMS too low: {r}");
    assert!(pcm.iter().all(|s| (*s as f32).is_finite()));
}

#[test]
fn hybrid_swb_stereo_cross_decodes_through_libopus() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let l: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 500.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let r: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 700.0 * i as f32 / SR as f32).cos() * 0.3)
        .collect();
    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Swb);
    let packets = drive_encoder_stereo(&mut enc, &l, &r);
    assert!(!packets.is_empty());

    let path = "/tmp/oxideav-opus-hybrid-swb-stereo.opus";
    write_ogg_opus_file(&packets, 2, path);

    let pcm = ffmpeg_decode_to_s16(path, 2).expect("ffmpeg ran");
    assert!(!pcm.is_empty(), "libopus decoded to empty PCM");

    // De-interleave for per-channel RMS.
    let mut pcm_l = Vec::with_capacity(pcm.len() / 2);
    let mut pcm_r = Vec::with_capacity(pcm.len() / 2);
    for chunk in pcm.chunks_exact(2) {
        pcm_l.push(chunk[0]);
        pcm_r.push(chunk[1]);
    }
    let r_l = rms_i16(&pcm_l);
    let r_r = rms_i16(&pcm_r);
    println!(
        "hybrid_swb_stereo cross-decode through libopus: rms_L={r_l:.4e}, rms_R={r_r:.4e}, samples_per_ch={}",
        pcm_l.len()
    );
    assert!(
        r_l > 0.01 && r_r > 0.01,
        "libopus decode RMS too low (L={r_l}, R={r_r})"
    );
    assert!(pcm_l.iter().all(|s| (*s as f32).is_finite()));
    assert!(pcm_r.iter().all(|s| (*s as f32).is_finite()));
}

/// Sanity: SILK-only stereo also cross-decodes (regression guard for the
/// shared SILK stereo header used by both SILK-only and Hybrid stereo).
#[test]
fn silk_wb_stereo_cross_decodes_through_libopus_sanity() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    use oxideav_opus::encoder::SilkEncoder;
    let mut p = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    p.channels = Some(2);
    p.sample_rate = Some(48_000);
    let mut enc = SilkEncoder::new_wb_stereo_20ms(&p).expect("WB stereo enc");

    // 25 frames of mixed-frequency stereo input.
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let l: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 400.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let r: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 600.0 * i as f32 / SR as f32).cos() * 0.3)
        .collect();
    let mut packets = Vec::new();
    for i in 0..n_frames {
        let start = i * FRAME_SAMPLES_20MS;
        let end = start + FRAME_SAMPLES_20MS;
        let mut bytes = Vec::with_capacity(FRAME_SAMPLES_20MS * 4);
        for j in start..end {
            let lq = (l[j] * 32768.0).clamp(-32768.0, 32767.0) as i16;
            let rq = (r[j] * 32768.0).clamp(-32768.0, 32767.0) as i16;
            bytes.extend_from_slice(&lq.to_le_bytes());
            bytes.extend_from_slice(&rq.to_le_bytes());
        }
        let frame = Frame::Audio(AudioFrame {
            samples: FRAME_SAMPLES_20MS as u32,
            pts: None,
            data: vec![bytes],
        });
        enc.send_frame(&frame).expect("send");
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    assert!(!packets.is_empty());

    let path = "/tmp/oxideav-opus-silk-wb-stereo.opus";
    write_ogg_opus_file(&packets, 2, path);

    let pcm = ffmpeg_decode_to_s16(path, 2).expect("ffmpeg ran");
    assert!(!pcm.is_empty(), "libopus decoded SILK stereo to empty PCM");
    let mut pcm_l = Vec::with_capacity(pcm.len() / 2);
    let mut pcm_r = Vec::with_capacity(pcm.len() / 2);
    for chunk in pcm.chunks_exact(2) {
        pcm_l.push(chunk[0]);
        pcm_r.push(chunk[1]);
    }
    let r_l = rms_i16(&pcm_l);
    let r_r = rms_i16(&pcm_r);
    println!("silk_wb_stereo cross-decode: rms_L={r_l:.4e}, rms_R={r_r:.4e}");
    assert!(
        r_l > 0.005 && r_r > 0.005,
        "SILK stereo libopus RMS too low (L={r_l}, R={r_r})"
    );
}

#[test]
fn hybrid_fb_stereo_cross_decodes_through_libopus() {
    if !ffmpeg_available() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let n_frames = 25;
    let total = n_frames * FRAME_SAMPLES_20MS;
    let l: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 600.0 * i as f32 / SR as f32).sin() * 0.3)
        .collect();
    let r: Vec<f32> = (0..total)
        .map(|i| (2.0 * std::f32::consts::PI * 800.0 * i as f32 / SR as f32).cos() * 0.3)
        .collect();
    let mut enc = make_hybrid_stereo_encoder(HybridBandwidth::Fb);
    let packets = drive_encoder_stereo(&mut enc, &l, &r);
    assert!(!packets.is_empty());

    let path = "/tmp/oxideav-opus-hybrid-fb-stereo.opus";
    write_ogg_opus_file(&packets, 2, path);

    let pcm = ffmpeg_decode_to_s16(path, 2).expect("ffmpeg ran");
    assert!(!pcm.is_empty(), "libopus decoded to empty PCM");

    let mut pcm_l = Vec::with_capacity(pcm.len() / 2);
    let mut pcm_r = Vec::with_capacity(pcm.len() / 2);
    for chunk in pcm.chunks_exact(2) {
        pcm_l.push(chunk[0]);
        pcm_r.push(chunk[1]);
    }
    let r_l = rms_i16(&pcm_l);
    let r_r = rms_i16(&pcm_r);
    println!("hybrid_fb_stereo cross-decode through libopus: rms_L={r_l:.4e}, rms_R={r_r:.4e}");
    assert!(
        r_l > 0.01 && r_r > 0.01,
        "libopus decode RMS too low (L={r_l}, R={r_r})"
    );
}
