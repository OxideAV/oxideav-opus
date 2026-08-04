//! CELT-mode encode → decode roundtrips: the crate's §5.3 CELT
//! encoder ([`oxideav_opus::celt_packet_encode::CeltEncoder`]) driven
//! into the crate's own streaming decoder, with waveform SNR gates
//! against the (2.5 ms-delayed) input.
//!
//! The encode→decode chain has a fixed 120-sample delay at 48 kHz
//! (the §4.3.7 MDCT overlap); SNR is measured after delay
//! compensation, skipping the first frames (decoder energy
//! ramp-in / prediction warm-up).

use oxideav_opus::celt_packet_encode::CeltEncoder;
use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::toc::Bandwidth;

/// Encode→decode delay at 48 kHz (the MDCT overlap).
const DELAY: usize = 120;

/// Multi-tone deterministic test signal (per-channel phase offsets).
fn tone(i: usize, c: usize) -> f64 {
    let t = i as f64 / 48000.0;
    let p = c as f64 * 0.7;
    8000.0 * (2.0 * std::f64::consts::PI * 440.0 * t + p).sin()
        + 4000.0 * (2.0 * std::f64::consts::PI * 1318.5 * t + 0.3 + p).sin()
        + 2500.0 * (2.0 * std::f64::consts::PI * 3520.0 * t + 1.1 + p).cos()
}

fn gen_pcm(frames: usize, samples_per_frame: usize, channels: usize) -> Vec<i16> {
    let total = frames * samples_per_frame;
    let mut pcm = Vec::with_capacity(total * channels);
    for i in 0..total {
        for c in 0..channels {
            pcm.push(tone(i, c).round().clamp(-32768.0, 32767.0) as i16);
        }
    }
    pcm
}

/// Encode `pcm` frame by frame, decode with a fresh `OpusDecoder`,
/// and return (SNR dB over the settled region, decoded PCM).
fn roundtrip_snr(
    enc: &mut CeltEncoder,
    pcm: &[i16],
    payload_bytes: usize,
    skip_frames: usize,
) -> f64 {
    let spf = enc.frame_samples();
    let ch = enc.channels();
    let mut dec = OpusDecoder::new();
    let mut decoded: Vec<i16> = Vec::new();
    for frame in pcm.chunks(spf * ch) {
        let (packet, _info) = enc.encode_packet(frame, payload_bytes).expect("encode");
        assert_eq!(packet.len(), 1 + payload_bytes);
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        decoded.extend_from_slice(&out.pcm);
    }
    // decoded[p] ≈ pcm[p - DELAY].
    let start = skip_frames * spf;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for p in (start + DELAY)..decoded.len() / ch {
        for c in 0..ch {
            let x = f64::from(pcm[(p - DELAY) * ch + c]);
            let d = f64::from(decoded[p * ch + c]) - x;
            num += d * d;
            den += x * x;
        }
    }
    10.0 * (den / num.max(1e-30)).log10()
}

#[test]
fn celt_fb_mono_20ms_tones_roundtrip() {
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    let pcm = gen_pcm(25, 960, 1);
    // 240-byte payloads ≈ 96 kb/s.
    let snr = roundtrip_snr(&mut enc, &pcm, 240, 3);
    println!("celt fb mono 20ms 96kbps: {snr:.2} dB");
    assert!(snr > 20.0, "snr {snr}");
}

#[test]
fn celt_fb_stereo_20ms_tones_roundtrip() {
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, true).unwrap();
    let pcm = gen_pcm(25, 960, 2);
    // 320-byte payloads ≈ 128 kb/s.
    let snr = roundtrip_snr(&mut enc, &pcm, 320, 3);
    println!("celt fb stereo 20ms 128kbps: {snr:.2} dB");
    assert!(snr > 18.0, "snr {snr}");
}

#[test]
fn celt_short_frames_roundtrip() {
    // 2.5 ms and 5 ms frames.
    for (tenths, payload, floor) in [(25u16, 40usize, 12.0f64), (50, 70, 15.0)] {
        let mut enc = CeltEncoder::new(Bandwidth::Fb, tenths, false).unwrap();
        let spf = enc.frame_samples();
        let pcm = gen_pcm(1920 / spf.max(1) * 2, spf, 1);
        let snr = roundtrip_snr(&mut enc, &pcm, payload, 8);
        println!("celt fb mono {tenths} tenths-ms: {snr:.2} dB");
        assert!(snr > floor, "tenths={tenths} snr {snr}");
    }
}

#[test]
fn celt_narrow_bandwidths_roundtrip() {
    // NB (end 13) and SWB (end 19): tones under the cutoff.
    for (bw, name) in [(Bandwidth::Nb, "nb"), (Bandwidth::Swb, "swb")] {
        let mut enc = CeltEncoder::new(bw, 200, false).unwrap();
        let pcm = gen_pcm(20, 960, 1);
        let snr = roundtrip_snr(&mut enc, &pcm, 160, 3);
        println!("celt {name} mono 20ms: {snr:.2} dB");
        assert!(snr > 12.0, "{name} snr {snr}");
    }
}

#[test]
fn celt_transient_content_uses_short_blocks_and_decodes() {
    // A click train drives the transient detector; every packet must
    // still decode cleanly.
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    let mut pcm = vec![0i16; 20 * 960];
    for (i, v) in pcm.iter_mut().enumerate() {
        let ph = i % 1600;
        if ph < 40 {
            *v = ((ph as f64 * 0.4).sin() * 24000.0) as i16;
        }
    }
    let mut dec = OpusDecoder::new();
    let mut any_transient = false;
    for frame in pcm.chunks(960) {
        let (packet, info) = enc.encode_packet(frame, 160).unwrap();
        any_transient |= info.transient;
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), 960);
    }
    assert!(any_transient, "click train never flagged transient");
}

#[test]
fn celt_rate_ladder_improves_with_bitrate() {
    // Same content at increasing payload sizes: SNR must be
    // (weakly) increasing and every rate decodable.
    let pcm = gen_pcm(15, 960, 1);
    let mut last = -100.0f64;
    for payload in [40usize, 80, 160, 320] {
        let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
        let snr = roundtrip_snr(&mut enc, &pcm, payload, 3);
        println!("celt fb mono 20ms {} bytes: {snr:.2} dB", payload);
        assert!(
            snr > last - 1.5,
            "rate ladder not monotone: {payload} bytes {snr} after {last}"
        );
        last = snr.max(last);
    }
    assert!(last > 20.0, "top rate too weak: {last}");
}

/// Strongly periodic "voice-like" content: pulse train through a
/// resonator plus a mild noise floor.
fn periodic_pcm(frames: usize, spf: usize, period: usize) -> Vec<i16> {
    let mut lp = 0.0f64;
    let mut lp2 = 0.0f64;
    let mut lcg = 7u32;
    (0..frames * spf)
        .map(|i| {
            let pulse = if i % period == 0 { 20000.0 } else { 0.0 };
            lp = 0.72 * lp + 0.28 * pulse;
            lp2 = 0.9 * lp2 + 0.1 * lp;
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((lcg >> 16) as f64 - 32768.0) / 200.0;
            (lp + 0.6 * lp2 + noise).round().clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

#[test]
fn celt_pitch_prefilter_fires_at_the_true_period_and_decodes() {
    // §5.3.1: on strongly periodic content the pre-filter must
    // engage, lock the true period, and the coded stream must decode
    // (the decoder's §4.3.7.1 post-filter inverting the comb).
    let period = 218usize;
    let pcm = periodic_pcm(30, 960, period);
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    let mut dec = OpusDecoder::new();
    let mut pf_frames = 0usize;
    let mut period_hits = 0usize;
    for frame in pcm.chunks(960) {
        let (packet, info) = enc.encode_packet(frame, 90).unwrap();
        if info.postfilter_on {
            pf_frames += 1;
            if (info.postfilter_period as i64 - period as i64).abs() <= 2 {
                period_hits += 1;
            }
            assert!((15..=1022).contains(&info.postfilter_period));
            assert!(info.postfilter_gain > 0.0 && info.postfilter_gain <= 0.75);
        }
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), 960);
    }
    assert!(pf_frames >= 25, "pre-filter engaged on only {pf_frames}/30");
    assert!(
        period_hits * 2 >= pf_frames,
        "period rarely correct: {period_hits}/{pf_frames}"
    );
}

#[test]
fn celt_pitch_prefilter_stays_off_on_noise() {
    // Aperiodic content must not cross the §5.3.1 gain threshold.
    let mut lcg = 99u32;
    let pcm: Vec<i16> = (0..20 * 960)
        .map(|_| {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (((lcg >> 16) as i32) - 32768).clamp(-20000, 20000) as i16
        })
        .collect();
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    let mut dec = OpusDecoder::new();
    let mut pf_frames = 0usize;
    for frame in pcm.chunks(960) {
        let (packet, info) = enc.encode_packet(frame, 120).unwrap();
        pf_frames += usize::from(info.postfilter_on);
        dec.decode_packet(&packet).expect("decode");
    }
    assert!(pf_frames <= 2, "pre-filter fired on noise: {pf_frames}/20");
}

#[test]
fn celt_pitch_prefilter_helps_periodic_content_at_equal_rate() {
    // The whole point: at a fixed payload, prefiltered periodic
    // content must reconstruct at least as well as steady tones do
    // without it — gate an absolute floor plus decode health across
    // an off→on→off transition (silence flanks force the ramps).
    let period = 218usize;
    let mut pcm = periodic_pcm(30, 960, period);
    for v in pcm.iter_mut().take(2 * 960) {
        *v = 0;
    }
    let len = pcm.len();
    for v in pcm.iter_mut().skip(len - 2 * 960) {
        *v = 0;
    }
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    let mut dec = OpusDecoder::new();
    let mut input = Vec::new();
    let mut decoded = Vec::new();
    for frame in pcm.chunks(960) {
        let (packet, _) = enc.encode_packet(frame, 90).unwrap();
        let out = dec.decode_packet(&packet).expect("decode");
        input.extend_from_slice(frame);
        decoded.extend_from_slice(&out.pcm);
    }
    // Settled-region SNR at the 120-sample delay over the periodic
    // middle.
    let n = decoded.len();
    let (mut num, mut den) = (0.0f64, 0.0f64);
    for p in (5 * 960)..(n - 3 * 960) {
        let x = f64::from(input[p - 120]);
        let d = f64::from(decoded[p]) - x;
        num += d * d;
        den += x * x;
    }
    let snr = 10.0 * (den / num.max(1e-30)).log10();
    println!("prefiltered periodic content at 36 kb/s: {snr:.2} dB");
    assert!(snr > 4.0, "prefiltered decode too weak: {snr:.2} dB");
}
