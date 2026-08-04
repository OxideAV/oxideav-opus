//! Opus-level VBR encode → decode roundtrips (RFC 6716 §2.1.8 /
//! §3.2.1): the per-frame size election under the target-bitrate
//! drift controller, on the CELT-only and Hybrid arms, plus the
//! SILK arm's natural-size VBR emission, all through the crate's own
//! streaming decoder with exact frame/sample accounting.

use oxideav_opus::celt_packet_encode::CeltEncoder;
use oxideav_opus::decoder::{FrameDecodeStatus, OpusDecoder};
use oxideav_opus::silk_encoder::SilkEncoderMono;
use oxideav_opus::toc::Bandwidth;
use oxideav_opus::vbr::{
    CeltVbrEncoder, HybridVbrEncoderMono, SilkVbrEncoderMono, SilkVbrEncoderStereo,
};

/// Encode→decode delay at 48 kHz (the §4.3.7 MDCT overlap).
const DELAY: usize = 120;

/// Multi-tone deterministic test signal (per-channel phase offsets).
fn tone(i: usize, c: usize) -> f64 {
    let t = i as f64 / 48000.0;
    let p = c as f64 * 0.7;
    8000.0 * (2.0 * std::f64::consts::PI * 440.0 * t + p).sin()
        + 4000.0 * (2.0 * std::f64::consts::PI * 1318.5 * t + 0.3 + p).sin()
        + 2500.0 * (2.0 * std::f64::consts::PI * 3520.0 * t + 1.1 + p).cos()
}

fn gen_tone_pcm(frames: usize, spf: usize, channels: usize) -> Vec<i16> {
    let total = frames * spf;
    let mut pcm = Vec::with_capacity(total * channels);
    for i in 0..total {
        for c in 0..channels {
            pcm.push(tone(i, c).round().clamp(-32768.0, 32767.0) as i16);
        }
    }
    pcm
}

/// Broadband signal crossing the 8 kHz Hybrid layer split.
fn hybrid_sig(i: usize) -> f64 {
    let t = i as f64 / 48000.0;
    6000.0 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()
        + 3000.0 * (2.0 * std::f64::consts::PI * 880.0 * t + 0.4).sin()
        + 2000.0 * (2.0 * std::f64::consts::PI * 3300.0 * t + 1.0).sin()
        + 1500.0 * (2.0 * std::f64::consts::PI * 10500.0 * t + 0.2).sin()
        + 1000.0 * (2.0 * std::f64::consts::PI * 14700.0 * t + 2.0).sin()
}

/// Settled-region SNR of `decoded` against `input` at the fixed
/// 120-sample encode→decode delay, skipping `skip` leading samples
/// per channel.
fn delayed_snr(input: &[i16], decoded: &[i16], channels: usize, skip: usize) -> f64 {
    let n = decoded.len() / channels;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for p in (skip + DELAY)..n {
        for c in 0..channels {
            let x = f64::from(input[(p - DELAY) * channels + c]);
            let d = f64::from(decoded[p * channels + c]) - x;
            num += d * d;
            den += x * x;
        }
    }
    10.0 * (den / num.max(1e-30)).log10()
}

/// Drive a CELT VBR encoder over `pcm`, decode every packet, and
/// return (per-packet sizes, decoded PCM).
fn run_celt_vbr(enc: &mut CeltVbrEncoder, pcm: &[i16]) -> (Vec<usize>, Vec<i16>) {
    let spf = enc.frame_samples();
    let ch = enc.channels();
    let mut dec = OpusDecoder::new();
    let mut sizes = Vec::new();
    let mut decoded = Vec::new();
    for frame in pcm.chunks(spf * ch) {
        let (packet, _info) = enc.encode_frame(frame).expect("encode");
        sizes.push(packet.len());
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        assert_eq!(out.frame_outcomes.len(), 1, "code-0 packet has one frame");
        decoded.extend_from_slice(&out.pcm);
    }
    (sizes, decoded)
}

#[test]
fn celt_vbr_average_sits_on_target() {
    // FB mono 20 ms at 96 kb/s → 240-byte target packets.
    let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, 96_000, false).unwrap();
    let pcm = gen_tone_pcm(50, 960, 1);
    let (sizes, decoded) = run_celt_vbr(&mut enc, &pcm);
    assert_eq!(sizes.len(), 50, "exact packet count");
    assert_eq!(decoded.len(), pcm.len(), "exact sample count");
    let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    println!("celt vbr 96kbps avg packet: {avg:.2} bytes (target 240)");
    assert!((avg - 240.0).abs() < 240.0 * 0.02, "avg {avg} off target");
}

#[test]
fn celt_vbr_matches_cbr_quality_at_matched_rate() {
    // The headline parity gate: encode VBR at a target, then CBR at
    // the VBR stream's realized average size; VBR must not lose more
    // than a fraction of a dB (it usually wins on mixed content).
    for (bitrate, stereo, name) in [
        (64_000u32, false, "fb mono 64k"),
        (96_000, false, "fb mono 96k"),
        (128_000, true, "fb stereo 128k"),
    ] {
        let ch = usize::from(stereo) + 1;
        let pcm = gen_tone_pcm(40, 960, ch);
        let mut venc = CeltVbrEncoder::new(Bandwidth::Fb, 200, stereo, bitrate, false).unwrap();
        let (sizes, vdec) = run_celt_vbr(&mut venc, &pcm);
        let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
        let vbr_snr = delayed_snr(&pcm, &vdec, ch, 3 * 960);

        // CBR at the matched average rate (same total byte budget).
        let payload = (avg.round() as usize) - 1;
        let mut cenc = CeltEncoder::new(Bandwidth::Fb, 200, stereo).unwrap();
        let mut dec = OpusDecoder::new();
        let mut cdec = Vec::new();
        for frame in pcm.chunks(960 * ch) {
            let (packet, _) = cenc.encode_packet(frame, payload).expect("cbr encode");
            let out = dec.decode_packet(&packet).expect("cbr decode");
            cdec.extend_from_slice(&out.pcm);
        }
        let cbr_snr = delayed_snr(&pcm, &cdec, ch, 3 * 960);
        println!(
            "{name}: vbr {vbr_snr:.2} dB (avg {avg:.1} B) vs cbr {cbr_snr:.2} dB \
             ({} B)",
            payload + 1
        );
        assert!(
            vbr_snr > cbr_snr - 0.7,
            "{name}: vbr {vbr_snr} dB lost to cbr {cbr_snr} dB at matched rate"
        );
    }
}

#[test]
fn celt_vbr_beats_cbr_on_mixed_content_at_equal_total_bytes() {
    // tone | silence | tone. VBR spends ~nothing on the silent third
    // and holds the target on active frames; a CBR stream of the SAME
    // total bytes must spread them uniformly, starving the active
    // region. At equal total bytes the VBR active-region SNR must win
    // clearly.
    let spf = 960usize;
    let frames = 30usize;
    let mut pcm = gen_tone_pcm(frames, spf, 1);
    for v in pcm[10 * spf..20 * spf].iter_mut() {
        *v = 0;
    }
    let mut venc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, 96_000, false).unwrap();
    let (sizes, vdec) = run_celt_vbr(&mut venc, &pcm);
    let total: usize = sizes.iter().sum();
    let payload = total / frames - 1; // CBR at the same total budget
    let mut cenc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    let mut dec = OpusDecoder::new();
    let mut cdec = Vec::new();
    for frame in pcm.chunks(spf) {
        let (packet, _) = cenc.encode_packet(frame, payload).expect("cbr encode");
        let out = dec.decode_packet(&packet).expect("cbr decode");
        cdec.extend_from_slice(&out.pcm);
    }
    // Active-region SNR: the settled part of the leading tone burst.
    let snr_region = |dec: &[i16]| {
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        for p in (3 * spf + DELAY)..(10 * spf) {
            let x = f64::from(pcm[p - DELAY]);
            let d = f64::from(dec[p]) - x;
            num += d * d;
            den += x * x;
        }
        10.0 * (den / num.max(1e-30)).log10()
    };
    let vbr_snr = snr_region(&vdec);
    let cbr_snr = snr_region(&cdec);
    println!(
        "mixed content, equal total {total} B: vbr {vbr_snr:.2} dB vs cbr \
         {cbr_snr:.2} dB ({} B/packet)",
        payload + 1
    );
    assert!(
        vbr_snr > cbr_snr + 1.0,
        "vbr {vbr_snr} dB should beat cbr {cbr_snr} dB on mixed content"
    );
}

#[test]
fn celt_vbr_silence_collapses_to_minimum_packets() {
    // tone | digital silence | tone. Silence frames must emit 3-byte
    // packets, and the post-silence spend is bounded by the drift
    // clamp (≤ 2× target), converging back to target.
    let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, 96_000, false).unwrap();
    let spf = 960usize;
    let mut pcm = gen_tone_pcm(30, spf, 1);
    for v in pcm[10 * spf..20 * spf].iter_mut() {
        *v = 0;
    }
    let (sizes, decoded) = run_celt_vbr(&mut enc, &pcm);
    assert_eq!(decoded.len(), pcm.len());
    for (f, &s) in sizes.iter().enumerate().take(19).skip(11) {
        // Frame 10's input is silent but the MDCT overlap still
        // carries tone energy into it; from frame 11 the input
        // window is fully silent.
        assert_eq!(s, 3, "silent frame {f} emitted {s} bytes");
    }
    let max_after = *sizes[20..].iter().max().unwrap();
    assert!(
        max_after <= 2 * 240,
        "post-silence spend {max_after} exceeds the drift clamp"
    );
    let tail_avg = sizes[25..].iter().sum::<usize>() as f64 / 5.0;
    assert!(
        (tail_avg - 240.0).abs() < 240.0 * 0.05,
        "tail average {tail_avg} did not reconverge"
    );
}

#[test]
fn celt_vbr_boosts_transients_and_repays() {
    // A click train on a quiet bed: boosted packets must appear, all
    // packets decode, and the stream average still lands near target.
    let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, 64_000, false).unwrap();
    let spf = 960usize;
    let frames = 40usize;
    let mut pcm = vec![0i16; frames * spf];
    for (i, v) in pcm.iter_mut().enumerate() {
        // Quiet tone bed.
        let t = i as f64 / 48000.0;
        *v = (400.0 * (2.0 * std::f64::consts::PI * 330.0 * t).sin()) as i16;
    }
    // A hard click every 4th frame, mid-frame.
    for f in (4..frames).step_by(4) {
        for j in 0..80 {
            pcm[f * spf + spf / 2 + j] = 26_000;
        }
    }
    let (sizes, decoded) = run_celt_vbr(&mut enc, &pcm);
    assert_eq!(decoded.len(), pcm.len());
    let target = 160usize; // 64 kb/s, 20 ms
    let boosted = sizes.iter().filter(|&&s| s > target + target / 8).count();
    assert!(boosted >= 4, "no transient boosts fired: {sizes:?}");
    let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    assert!(
        (avg - target as f64).abs() < target as f64 * 0.05,
        "avg {avg} drifted from target {target}"
    );
}

#[test]
fn celt_constrained_vbr_never_outruns_the_reservoir() {
    // Constrained mode on transient-heavy content: every packet obeys
    // target + banked-reservoir, and every window of n packets obeys
    // n*target + cap.
    let bitrate = 64_000u32;
    let target_bits = 1280.0; // 20 ms at 64 kb/s
    let cap_bits = 6400.0; // 100 ms default reservoir
    let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, bitrate, true).unwrap();
    let spf = 960usize;
    let frames = 40usize;
    let mut pcm = gen_tone_pcm(frames, spf, 1);
    // Silence stretches bank credit; clicks try to spend it.
    for v in pcm[5 * spf..10 * spf].iter_mut() {
        *v = 0;
    }
    for f in (12..frames).step_by(3) {
        for j in 0..80 {
            pcm[f * spf + j] = 26_000;
        }
    }
    let mut ceilings = Vec::new();
    let spf_ch = spf;
    let mut dec = OpusDecoder::new();
    let mut sizes = Vec::new();
    for frame in pcm.chunks(spf_ch) {
        // Snapshot the discipline's current ceiling BEFORE encoding.
        ceilings.push(enc.rate_control().constrained_ceiling_bits());
        let (packet, _) = enc.encode_frame(frame).expect("encode");
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        sizes.push(packet.len());
    }
    for (f, (&s, &ceil)) in sizes.iter().zip(ceilings.iter()).enumerate() {
        assert!(
            (s * 8) as f64 <= ceil + 7.0 + 1e-6, // byte rounding headroom
            "frame {f}: {s} bytes busts the constrained ceiling {ceil} bits"
        );
    }
    let bits: Vec<f64> = sizes.iter().map(|&s| (s * 8) as f64).collect();
    for w in [1usize, 5, 20, frames] {
        for win in bits.windows(w) {
            let sum: f64 = win.iter().sum();
            assert!(
                sum <= w as f64 * target_bits + cap_bits + 7.0 * w as f64,
                "constrained window {w} bust"
            );
        }
    }
}

#[test]
fn celt_vbr_rate_ladder_is_monotone() {
    let pcm = gen_tone_pcm(20, 960, 1);
    let mut last = -100.0f64;
    for bitrate in [16_000u32, 32_000, 64_000, 128_000] {
        let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, bitrate, false).unwrap();
        let (sizes, decoded) = run_celt_vbr(&mut enc, &pcm);
        let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
        let snr = delayed_snr(&pcm, &decoded, 1, 3 * 960);
        println!("celt vbr ladder {bitrate} b/s: avg {avg:.1} B, {snr:.2} dB");
        assert!(
            (avg - f64::from(bitrate) / 50.0 / 8.0).abs() < 4.0,
            "ladder rate {bitrate} avg {avg} off target"
        );
        assert!(snr > last - 1.5, "ladder not monotone at {bitrate}");
        last = snr.max(last);
    }
    assert!(last > 20.0, "top of ladder too weak: {last}");
}

#[test]
fn celt_vbr_short_frames_track_target() {
    // 2.5 ms and 5 ms frames: the election works at every LM.
    for (tenths, fps) in [(25u16, 400usize), (50, 200)] {
        let mut enc = CeltVbrEncoder::new(Bandwidth::Fb, tenths, false, 96_000, false).unwrap();
        let spf = enc.frame_samples();
        let frames = 2 * fps / 10; // 200 ms of audio
        let pcm = gen_tone_pcm(frames, spf, 1);
        let (sizes, decoded) = run_celt_vbr(&mut enc, &pcm);
        assert_eq!(decoded.len(), pcm.len());
        let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
        let target = 96_000.0 / fps as f64 / 8.0;
        println!("celt vbr {tenths} tenths-ms avg {avg:.2} B (target {target})");
        assert!(
            (avg - target).abs() < target * 0.05,
            "short-frame avg {avg} off {target}"
        );
    }
}

#[test]
fn hybrid_vbr_tracks_target_and_decodes() {
    // 96 kb/s: above this content's SILK-layer floor, so the tracking
    // discipline (not the floor raise) governs the realized rate.
    for (bw, name) in [(Bandwidth::Fb, "fb"), (Bandwidth::Swb, "swb")] {
        let mut enc = HybridVbrEncoderMono::new(bw, 200, 96_000, false).unwrap();
        let spf = enc.frame_samples();
        let frames = 30usize;
        let mut dec = OpusDecoder::new();
        let mut sizes = Vec::new();
        let mut input = Vec::new();
        let mut decoded = Vec::new();
        for f in 0..frames {
            let pcm: Vec<i16> = (0..spf)
                .map(|j| hybrid_sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
                .collect();
            let packet = enc.encode_frame(&pcm).expect("encode");
            sizes.push(packet.len());
            let out = dec.decode_packet(&packet).expect("decode");
            assert_eq!(out.samples_per_channel(), spf);
            assert_eq!(
                out.frame_outcomes[0].status,
                FrameDecodeStatus::HybridDecoded
            );
            input.extend_from_slice(&pcm);
            decoded.extend_from_slice(&out.pcm);
        }
        let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
        let snr = delayed_snr(&input, &decoded, 1, decoded.len() / 4);
        println!("hybrid vbr {name} 96kbps: avg {avg:.1} B (target 240), {snr:.2} dB");
        assert!((avg - 240.0).abs() < 240.0 * 0.03, "{name} avg {avg}");
        assert!(snr > 9.0, "{name} snr {snr}");
    }
}

#[test]
fn hybrid_vbr_floor_raise_survives_starving_targets() {
    // A target far below the SILK layer's natural size: every packet
    // is raised to the floor, still decodes as Hybrid, and the
    // stream reports its (honest) above-target rate.
    let mut enc = HybridVbrEncoderMono::new(Bandwidth::Fb, 200, 16_000, false).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    let mut sizes = Vec::new();
    for f in 0..20 {
        let pcm: Vec<i16> = (0..spf)
            .map(|j| hybrid_sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        let packet = enc.encode_frame(&pcm).expect("floor-raised encode");
        assert!(packet.len() >= 3);
        let out = dec.decode_packet(&packet).expect("decode");
        assert_eq!(out.samples_per_channel(), spf);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::HybridDecoded
        );
        sizes.push(packet.len());
    }
    let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    let target = 16_000.0 / 50.0 / 8.0; // 40 bytes
    println!("hybrid vbr floor-raised avg: {avg:.1} B (starving target {target})");
    assert!(
        avg > target,
        "floor raise must show up in the realized rate"
    );
}

#[test]
fn hybrid_vbr_matches_cbr_quality_at_matched_rate() {
    use oxideav_opus::hybrid_packet_encode::HybridEncoderMono;
    let frames = 30usize;
    let mut venc = HybridVbrEncoderMono::new(Bandwidth::Fb, 200, 96_000, false).unwrap();
    let spf = venc.frame_samples();
    let mut dec = OpusDecoder::new();
    let mut input = Vec::new();
    let mut vdec = Vec::new();
    let mut sizes = Vec::new();
    for f in 0..frames {
        let pcm: Vec<i16> = (0..spf)
            .map(|j| hybrid_sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        let packet = venc.encode_frame(&pcm).expect("encode");
        sizes.push(packet.len());
        let out = dec.decode_packet(&packet).expect("decode");
        input.extend_from_slice(&pcm);
        vdec.extend_from_slice(&out.pcm);
    }
    let avg = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;
    let vbr_snr = delayed_snr(&input, &vdec, 1, input.len() / 4);

    let payload = (avg.round() as usize) - 1;
    let mut cenc = HybridEncoderMono::new(Bandwidth::Fb, 200).unwrap();
    let mut dec2 = OpusDecoder::new();
    let mut cdec = Vec::new();
    for frame in input.chunks(spf) {
        let packet = cenc.encode_packet(frame, payload).expect("cbr encode");
        let out = dec2.decode_packet(&packet).expect("cbr decode");
        cdec.extend_from_slice(&out.pcm);
    }
    let cbr_snr = delayed_snr(&input, &cdec, 1, input.len() / 4);
    println!(
        "hybrid parity: vbr {vbr_snr:.2} dB (avg {avg:.1} B) vs cbr {cbr_snr:.2} dB \
         ({} B)",
        payload + 1
    );
    assert!(
        vbr_snr > cbr_snr - 0.7,
        "hybrid vbr {vbr_snr} dB lost to cbr {cbr_snr} dB at matched rate"
    );
}

#[test]
fn silk_natural_vbr_saves_bytes_at_identical_quality() {
    // The SILK arm's Opus-level VBR is the natural quality-driven
    // emission (§2.1.8: the LP layer is VBR). Against the CBR-padded
    // transport of the SAME coded frames, the decode is bit-identical
    // and the natural stream is strictly smaller.
    let mut venc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    let mut cenc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    let spf = venc.frame_samples();
    let frames = 25usize;
    let pcm: Vec<f32> = (0..frames * spf)
        .map(|i| {
            let t = i as f64 / 16000.0;
            (0.35 * (2.0 * std::f64::consts::PI * 233.0 * t).sin()
                + 0.15 * (2.0 * std::f64::consts::PI * 587.0 * t + 0.6).sin()) as f32
        })
        .collect();
    // Probe pass: the constant transport size a CBR channel would
    // need is the worst-case natural packet.
    // (+1: the §3.2.5 code-3 re-frame spends one count byte).
    let mut probe = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    let cbr_size = pcm
        .chunks(spf)
        .map(|frame| probe.encode_packet(frame).expect("probe").packet.len())
        .max()
        .unwrap()
        + 1;
    let mut vdec = OpusDecoder::new();
    let mut cdec = OpusDecoder::new();
    let mut vbytes = 0usize;
    let mut cbytes = 0usize;
    for frame in pcm.chunks(spf) {
        let vout = venc.encode_packet(frame).expect("vbr encode");
        let cout = cenc.encode_packet_cbr(frame, cbr_size).expect("cbr encode");
        assert!(vout.packet.len() <= cbr_size, "natural exceeded the pad");
        assert_eq!(cout.packet.len(), cbr_size);
        vbytes += vout.packet.len();
        cbytes += cout.packet.len();
        let v = vdec.decode_packet(&vout.packet).expect("vbr decode");
        let c = cdec.decode_packet(&cout.packet).expect("cbr decode");
        assert_eq!(v.pcm, c.pcm, "padding must not change the decode");
    }
    println!(
        "silk natural vbr: {vbytes} B vs cbr transport {cbytes} B \
         ({:.1}% saved)",
        100.0 * (1.0 - vbytes as f64 / cbytes as f64)
    );
    assert!(vbytes < cbytes, "natural VBR saved nothing");
}

#[test]
fn vbr_streams_have_exact_frame_accounting() {
    // Every arm: packet count == frame count fed in; every packet is
    // code 0 (one frame); total decoded samples == frames * spf.
    let mut celt = CeltVbrEncoder::new(Bandwidth::Fb, 200, false, 64_000, false).unwrap();
    let pcm = gen_tone_pcm(17, 960, 1);
    let (sizes, decoded) = run_celt_vbr(&mut celt, &pcm);
    assert_eq!(sizes.len(), 17);
    assert_eq!(decoded.len(), 17 * 960);

    let mut hyb = HybridVbrEncoderMono::new(Bandwidth::Fb, 200, 64_000, false).unwrap();
    let spf = hyb.frame_samples();
    let mut dec = OpusDecoder::new();
    let mut n_packets = 0usize;
    let mut n_samples = 0usize;
    for f in 0..9 {
        let frame: Vec<i16> = (0..spf)
            .map(|j| hybrid_sig(f * spf + j).round().clamp(-32768.0, 32767.0) as i16)
            .collect();
        let packet = hyb.encode_frame(&frame).unwrap();
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.frame_outcomes.len(), 1);
        n_packets += 1;
        n_samples += out.samples_per_channel();
    }
    assert_eq!(n_packets, 9);
    assert_eq!(n_samples, 9 * spf);
}

/// Deterministic mixed internal-rate content for the SILK arm: tone +
/// harmonic + a small noise floor (a continuous rate ladder, unlike a
/// pure tone).
fn silk_sig(i: usize, rate_hz: f64) -> f32 {
    let t = i as f64 / rate_hz;
    let mut x = 0.25 * (2.0 * std::f64::consts::PI * 400.0 * t).sin()
        + 0.1 * (2.0 * std::f64::consts::PI * 1250.0 * t + 0.7).sin();
    let mut lcg = (i as u32)
        .wrapping_mul(1_664_525)
        .wrapping_add(1_013_904_223);
    lcg ^= lcg >> 13;
    x += 0.03 * ((lcg & 0xffff) as f64 / 32768.0 - 1.0);
    x as f32
}

#[test]
fn silk_vbr_mono_tracks_target_and_decodes() {
    // The elected SILK-only arm: realized average within a few
    // percent of target, every packet decoding with the exact §3
    // sample count.
    for &(bw, rate_hz, bps) in &[
        (Bandwidth::Nb, 8_000.0, 12_000u32),
        (Bandwidth::Wb, 16_000.0, 20_000),
        (Bandwidth::Wb, 16_000.0, 32_000),
    ] {
        let mut enc = SilkVbrEncoderMono::new(bw, 200, bps, false).unwrap();
        let spf = enc.frame_samples();
        let target = enc.rate_control().target_bits_per_packet() / 8.0;
        let mut dec = OpusDecoder::new();
        let frames = 40usize;
        let mut total = 0usize;
        for f in 0..frames {
            let frame: Vec<f32> = (0..spf).map(|i| silk_sig(f * spf + i, rate_hz)).collect();
            let packet = enc.encode_frame(&frame).unwrap();
            total += packet.len();
            let out = dec.decode_packet(&packet).unwrap();
            assert_eq!(out.samples_per_channel(), 960);
            assert_eq!(out.channels, 1);
        }
        let avg = total as f64 / frames as f64;
        println!("silk vbr {bw:?} {bps} b/s: avg {avg:.1} B vs target {target:.1} B");
        assert!(
            (avg - target).abs() <= 0.08 * target,
            "{bw:?} {bps}: avg {avg:.1} off target {target:.1}"
        );
    }
}

#[test]
fn silk_vbr_stereo_tracks_target_with_fec() {
    let mut enc = SilkVbrEncoderStereo::new(Bandwidth::Wb, 200, 28_000, false).unwrap();
    enc.set_fec(true);
    let spf = enc.frame_samples();
    let target = enc.rate_control().target_bits_per_packet() / 8.0;
    let mut dec = OpusDecoder::new();
    let frames = 40usize;
    let mut total = 0usize;
    for f in 0..frames {
        let mut left = Vec::with_capacity(spf);
        let mut right = Vec::with_capacity(spf);
        for i in 0..spf {
            let v = silk_sig(f * spf + i, 16_000.0);
            left.push(v);
            right.push(0.4 * v);
        }
        let nv = silk_sig((f + 1) * spf, 16_000.0);
        let packet = enc
            .encode_frame(&left, &right, Some((nv, 0.4 * nv)))
            .unwrap();
        total += packet.len();
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.samples_per_channel(), 960);
        assert_eq!(out.channels, 2);
    }
    let avg = total as f64 / frames as f64;
    println!("silk vbr stereo+fec: avg {avg:.1} B vs target {target:.1} B");
    assert!(
        (avg - target).abs() <= 0.10 * target,
        "stereo avg {avg:.1} off target {target:.1}"
    );
}

#[test]
fn silk_vbr_constrained_never_outruns_the_reservoir() {
    // Constrained discipline on the SILK arm: no packet exceeds the
    // controller's pre-encode ceiling (the content's floor sits well
    // below the target, so floor raises cannot bust it).
    let mut enc = SilkVbrEncoderMono::new(Bandwidth::Wb, 200, 24_000, true).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    for f in 0..60 {
        let ceiling_bits = enc.rate_control().constrained_ceiling_bits();
        let frame: Vec<f32> = (0..spf).map(|i| silk_sig(f * spf + i, 16_000.0)).collect();
        let packet = enc.encode_frame(&frame).unwrap();
        assert!(
            (packet.len() * 8) as f64 <= ceiling_bits + 8.0,
            "packet {} bits outran ceiling {ceiling_bits}",
            packet.len() * 8
        );
        dec.decode_packet(&packet).unwrap();
    }
}

#[test]
fn silk_vbr_silence_banks_and_repays() {
    // A silent stretch collapses to near-floor packets; the drift
    // clamp bounds the post-silence spree to 2x target.
    let mut enc = SilkVbrEncoderMono::new(Bandwidth::Wb, 200, 24_000, false).unwrap();
    let spf = enc.frame_samples();
    let target = enc.rate_control().target_bits_per_packet() / 8.0;
    let mut dec = OpusDecoder::new();
    let mut sizes = Vec::new();
    for f in 0..30 {
        let silent = (10..20).contains(&f);
        let frame: Vec<f32> = (0..spf)
            .map(|i| {
                if silent {
                    0.0
                } else {
                    silk_sig(f * spf + i, 16_000.0)
                }
            })
            .collect();
        let packet = enc.encode_frame(&frame).unwrap();
        sizes.push(packet.len());
        dec.decode_packet(&packet).unwrap();
    }
    // Silent packets sit far below target (inactive frames at the
    // header floor)...
    let silent_avg = sizes[12..20].iter().sum::<usize>() as f64 / 8.0;
    assert!(
        silent_avg < 0.4 * target,
        "silent packets {silent_avg:.1} B did not collapse (target {target:.1})"
    );
    // ...and every packet ever emitted obeys the drift-clamped bound.
    for &s in &sizes {
        assert!(
            (s as f64) <= 2.0 * target + 1.0,
            "post-silence spree {s} B busts the 2x-target clamp"
        );
    }
}

#[test]
fn silk_vbr_multiframe_durations_track_target() {
    // 40 ms and 60 ms packets (2-3 SILK frames per §4.2.2): the
    // election spans the whole packet; frame accounting stays exact.
    for &(tenths, samples48) in &[(400u16, 1920usize), (600, 2880)] {
        let mut enc = SilkVbrEncoderMono::new(Bandwidth::Wb, tenths, 20_000, false).unwrap();
        let spf = enc.frame_samples();
        let target = enc.rate_control().target_bits_per_packet() / 8.0;
        let mut dec = OpusDecoder::new();
        let frames = 20usize;
        let mut total = 0usize;
        for f in 0..frames {
            let frame: Vec<f32> = (0..spf).map(|i| silk_sig(f * spf + i, 16_000.0)).collect();
            let packet = enc.encode_frame(&frame).unwrap();
            total += packet.len();
            let out = dec.decode_packet(&packet).unwrap();
            assert_eq!(out.samples_per_channel(), samples48);
        }
        let avg = total as f64 / frames as f64;
        println!("silk vbr {tenths} tenths-ms: avg {avg:.1} B vs target {target:.1} B");
        assert!(
            (avg - target).abs() <= 0.08 * target,
            "{tenths}: avg {avg:.1} off target {target:.1}"
        );
    }
}

#[test]
fn silk_vbr_stereo_natural_use_without_lookahead() {
    // Stream-end ergonomics: None lookahead on every frame still
    // tracks and decodes.
    let mut enc = SilkVbrEncoderStereo::new(Bandwidth::Nb, 200, 16_000, false).unwrap();
    let spf = enc.frame_samples();
    let mut dec = OpusDecoder::new();
    for f in 0..10 {
        let mut left = Vec::with_capacity(spf);
        let mut right = Vec::with_capacity(spf);
        for i in 0..spf {
            let v = silk_sig(f * spf + i, 8_000.0);
            left.push(v);
            right.push(-v);
        }
        let packet = enc.encode_frame(&left, &right, None).unwrap();
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.channels, 2);
    }
}
