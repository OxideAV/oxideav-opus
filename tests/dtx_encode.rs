//! §2.1.9 discontinuous-transmission integration gates (RFC 6716).
//!
//! `set_dtx` on the SILK encoder arms: a packet whose every §4.2.2
//! interval sits below the §4.2.3 activity floor is — after a short
//! transmitted hangover — replaced by the 1-byte TOC-only marker
//! (one §3.2.1 zero-length frame), with one real packet still coded
//! every 400 ms ("only one frame is encoded every 400 milliseconds",
//! §2.1.9) as the comfort-noise refresh. While packets are
//! suppressed, every decoder-authoritative mirror is frozen — the
//! decoder decodes nothing for a zero-length frame — so coded
//! packets after a DTX run stay stream-exact.
//!
//! Gated below: marker placement (never on active content, hangover
//! first, exact refresh cadence), the wire shape of the marker, the
//! measured bitrate savings, the decode round trip (sample counts,
//! statuses, silence decay, voice quality on re-entry), the §4.2.5
//! FEC interaction (the hangover carries the last active packet's
//! LBRR), and the elected / CBR path pass-through.

use oxideav_opus::decoder::{FecDecodeStatus, FrameDecodeStatus, OpusDecoder};
use oxideav_opus::silk_encoder::{SilkEncoderMono, SilkEncoderStereo};
use oxideav_opus::toc::Bandwidth;

/// 20 ms of WB internal-rate samples.
const WB_20MS: usize = 320;

/// Deterministic voice-like content: a pitch-pulse train through a
/// resonator plus two tones, amplitude well above the activity floor.
fn voice(rate_hz: usize, n: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; n];
    let (mut y1, mut y2) = (0.0f64, 0.0f64);
    for (i, slot) in out.iter_mut().enumerate() {
        let t = i as f64 / rate_hz as f64;
        let period = (rate_hz as f64 / 110.0) as usize;
        let pulse = if i % period < 6 { 1.0 } else { 0.0 };
        let x = pulse + 0.4 * (2.0 * std::f64::consts::PI * 220.0 * t).sin();
        let w = 2.0 * std::f64::consts::PI * 500.0 / rate_hz as f64;
        let r = 0.94;
        let y = x + 2.0 * r * w.cos() * y1 - r * r * y2;
        y2 = y1;
        y1 = y;
        *slot = (0.15 * y) as f32;
    }
    out
}

/// voice | silence | voice at 16 kHz: (signal, silence packet range)
/// for 20 ms WB packets.
fn voice_silence_voice(voice_ms: usize, silence_ms: usize) -> (Vec<f32>, std::ops::Range<usize>) {
    let v = voice_ms * 16;
    let s = silence_ms * 16;
    let mut sig = voice(16_000, v);
    sig.extend(std::iter::repeat(0.0f32).take(s));
    sig.extend(voice(16_000, v));
    let first_silent_packet = v.div_ceil(WB_20MS);
    let first_voice_again = (v + s) / WB_20MS;
    (sig, first_silent_packet..first_voice_again)
}

fn encode_stream(sig: &[f32], dtx: bool, fec: bool) -> Vec<Vec<u8>> {
    let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    enc.set_dtx(dtx);
    enc.set_fec(fec);
    sig.chunks_exact(WB_20MS)
        .map(|c| enc.encode_packet(c).unwrap().packet)
        .collect()
}

/// Marker placement + cadence + wire shape + savings, mono WB 20 ms.
#[test]
fn dtx_suppresses_silence_with_400ms_refresh_cadence() {
    let (sig, silent) = voice_silence_voice(500, 3_000);
    let on = encode_stream(&sig, true, false);
    let off = encode_stream(&sig, false, false);
    assert_eq!(on.len(), off.len());

    // Never a marker on active content.
    for (k, p) in on.iter().enumerate() {
        if !silent.contains(&k) {
            assert!(p.len() > 1, "marker on active packet {k}");
        }
    }

    // The wire shape: config 9 (SILK WB 20 ms), mono, code 0 — the
    // §3.1 TOC alone, its single §3.2.1 frame zero-length.
    let markers: Vec<usize> = (0..on.len()).filter(|&k| on[k].len() == 1).collect();
    assert!(!markers.is_empty(), "no DTX markers emitted");
    for &k in &markers {
        assert_eq!(on[k], vec![0x48u8], "marker TOC at packet {k}");
        assert!(silent.contains(&k), "marker outside the silent run");
    }

    // Hangover: the first packets of the silent run are transmitted.
    assert!(on[silent.start].len() > 1, "no hangover packet");
    assert!(on[silent.start + 1].len() > 1, "hangover shorter than 2");

    // §2.1.9 cadence inside the run: between consecutive CODED
    // packets deep in the silent run sit exactly 400 ms of markers
    // (20 packets at 20 ms).
    let coded_in_run: Vec<usize> = (markers[0]..silent.end)
        .filter(|&k| on[k].len() > 1)
        .collect();
    assert!(
        coded_in_run.len() >= 5,
        "3 s of silence must carry several §2.1.9 refreshes: {coded_in_run:?}"
    );
    for w in coded_in_run.windows(2) {
        assert_eq!(
            w[1] - w[0],
            21,
            "refresh cadence: coded packets at {w:?} are not 20 markers apart"
        );
    }

    // Measured savings. The silent run is where DTX lives: the coded
    // inactive packets it replaces are already small (the header
    // floor), so the gate is on the run, plus strict whole-stream
    // improvement (the default-quality voice packets dominate the
    // absolute total).
    let bytes_on: usize = on.iter().map(Vec::len).sum();
    let bytes_off: usize = off.iter().map(Vec::len).sum();
    assert!(
        bytes_on < bytes_off,
        "whole-stream DTX regression: {bytes_on} vs {bytes_off}"
    );
    let run_on: usize = silent.clone().map(|k| on[k].len()).sum();
    let run_off: usize = silent.clone().map(|k| off[k].len()).sum();
    assert!(
        (run_on as f64) < run_off as f64 * 0.35,
        "silent-run DTX savings too small: {run_on} vs {run_off}"
    );
}

/// DTX enabled on fully active content is bit-identical to DTX off.
#[test]
fn dtx_on_active_content_is_bit_identical() {
    let sig = voice(16_000, 16_000);
    let on = encode_stream(&sig, true, false);
    let off = encode_stream(&sig, false, false);
    assert_eq!(on, off);
}

/// The DTX stream decodes end-to-end: exact sample counts, per-frame
/// statuses, §4.4 silence decay inside the run, and clean voice on
/// re-entry (the frozen-mirror guarantee, measured black-box).
#[test]
fn dtx_stream_decodes_with_exact_counts_and_clean_reentry() {
    let (sig, silent) = voice_silence_voice(500, 3_000);
    let on = encode_stream(&sig, true, false);

    let mut dec = OpusDecoder::new();
    let mut pcm48: Vec<i16> = Vec::new();
    for (k, p) in on.iter().enumerate() {
        let out = dec.decode_packet(p).expect("decode");
        assert_eq!(out.channels, 1);
        assert_eq!(out.samples_per_channel(), 960, "packet {k}");
        let want = if p.len() == 1 {
            FrameDecodeStatus::DtxOrLost
        } else {
            FrameDecodeStatus::SilkParamsDecoded
        };
        assert_eq!(out.frame_outcomes[0].status, want, "packet {k}");
        pcm48.extend_from_slice(&out.pcm);
    }

    // Deep inside the DTX run the output has decayed to (near) the
    // silence floor.
    let deep = (silent.start + 60) * 960..(silent.start + 62) * 960;
    let deep_peak = pcm48[deep]
        .iter()
        .map(|&s| i32::from(s).abs())
        .max()
        .unwrap();
    assert!(deep_peak <= 8, "DTX region did not decay: peak {deep_peak}");

    // Voice re-entry: the decoded post-silence voice carries energy
    // comparable to the decoded pre-silence voice (no blown gains, no
    // dead output — the mirrors stayed decoder-exact through the run).
    let e = |r: std::ops::Range<usize>| -> f64 {
        pcm48[r].iter().map(|&s| f64::from(s) * f64::from(s)).sum()
    };
    let pre = e(5 * 960..20 * 960);
    let post_start = (silent.end + 5) * 960;
    let post = e(post_start..post_start + 15 * 960);
    assert!(
        post > pre * 0.25 && post < pre * 4.0,
        "re-entry energy off: pre {pre:.1} post {post:.1}"
    );
}

/// §4.2.5 FEC × DTX: the hangover transmits the LBRR of the last
/// ACTIVE packet, so losing that packet stays recoverable; markers
/// never carry redundancy (they are 1 byte by construction).
#[test]
fn dtx_hangover_carries_the_last_active_packets_fec() {
    let (sig, silent) = voice_silence_voice(500, 3_000);
    let on = encode_stream(&sig, true, true);

    // Feed the stream up to (excluding) the last active packet, drop
    // it, and recover it from the next packet (the first hangover
    // packet, which must be a real coded packet carrying its LBRR).
    let last_active = silent.start - 1;
    let mut dec = OpusDecoder::new();
    for p in &on[..last_active] {
        dec.decode_packet(p).expect("decode");
    }
    assert!(
        on[last_active + 1].len() > 1,
        "packet after the last active one was suppressed"
    );
    let rec = dec
        .decode_packet_fec(&on[last_active + 1])
        .expect("fec decode");
    assert_eq!(rec.status, FecDecodeStatus::Recovered);
    let energy: f64 = rec.pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    assert!(energy > 0.0, "FEC recovery silent");
}

/// Stereo: the marker carries the stereo TOC flag, the cadence holds,
/// and the round trip decodes two channels with exact counts.
#[test]
fn stereo_dtx_markers_and_roundtrip() {
    let n = 16_000 / 2 + 2 * 16_000 + 16_000 / 2; // 0.5 s + 2 s + 0.5 s
    let mono = {
        let (sig, _) = voice_silence_voice(500, 2_000);
        sig
    };
    assert_eq!(mono.len(), n);
    let left: Vec<f32> = mono.iter().map(|&v| v * 0.9).collect();
    let right: Vec<f32> = mono.iter().map(|&v| v * 0.5).collect();

    let mut enc = SilkEncoderStereo::new(Bandwidth::Wb).unwrap();
    enc.set_dtx(true);
    let total = left.len() / WB_20MS;
    let mut packets = Vec::new();
    for k in 0..total {
        let l = &left[k * WB_20MS..(k + 1) * WB_20MS];
        let r = &right[k * WB_20MS..(k + 1) * WB_20MS];
        let next = if (k + 1) * WB_20MS < left.len() {
            Some((left[(k + 1) * WB_20MS], right[(k + 1) * WB_20MS]))
        } else {
            None
        };
        packets.push(enc.encode_packet(l, r, next).unwrap().packet);
    }

    let markers: Vec<usize> = (0..total).filter(|&k| packets[k].len() == 1).collect();
    assert!(
        markers.len() > 60,
        "stereo DTX barely fired: {}",
        markers.len()
    );
    for &k in &markers {
        // Config 9, stereo flag SET, code 0.
        assert_eq!(packets[k], vec![0x4Cu8], "stereo marker TOC at {k}");
    }

    let mut dec = OpusDecoder::new();
    for (k, p) in packets.iter().enumerate() {
        let out = dec.decode_packet(p).expect("decode");
        assert_eq!(out.channels, 2, "packet {k}");
        assert_eq!(out.samples_per_channel(), 960, "packet {k}");
    }
}

/// The elected and CBR paths pass markers through untouched: no
/// election over a 1-byte packet, no CBR padding of a marker.
#[test]
fn elected_and_cbr_paths_pass_markers_through() {
    let silence = vec![0.0f32; WB_20MS];
    let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    enc.set_dtx(true);
    // Hangover drains on the first packets…
    for _ in 0..3 {
        let out = enc.encode_packet_elected(&silence, 40).unwrap();
        assert!(!out.packet.is_empty());
    }
    // …then the elected path yields markers.
    let out = enc.encode_packet_elected(&silence, 40).unwrap();
    assert!(out.is_dtx(), "elected path did not pass the marker through");
    assert_eq!(out.packet.len(), 1);

    // CBR: the marker is NOT padded to the target.
    let out = enc.encode_packet_cbr(&silence, 40).unwrap();
    assert!(out.is_dtx());
    assert_eq!(out.packet.len(), 1, "marker was padded");

    // A fresh CBR encoder on active content still pads (the target
    // leaves headroom over the default-quality voice packet).
    let mut enc2 = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    enc2.set_dtx(true);
    let v = voice(16_000, WB_20MS);
    let out = enc2.encode_packet_cbr(&v, 1200).unwrap();
    assert_eq!(out.packet.len(), 1200);
}

// ---------------------------------------------------------------
// Hybrid arms (§2.1.9 on configs 12–15) + the VBR pass-throughs.
// ---------------------------------------------------------------

use oxideav_opus::hybrid_packet_encode::{HybridEncoderMono, HybridEncoderStereo};
use oxideav_opus::vbr::SilkVbrEncoderMono;

/// 48 kHz i16 voice | silence | voice (20 ms hybrid packets).
fn voice_silence_voice_48k(
    voice_ms: usize,
    silence_ms: usize,
) -> (Vec<i16>, std::ops::Range<usize>) {
    let v = voice_ms * 48;
    let s = silence_ms * 48;
    let f: Vec<f32> = voice(48_000, v);
    let mut sig: Vec<i16> = f.iter().map(|&x| (x * 6000.0) as i16).collect();
    sig.extend(std::iter::repeat(0i16).take(s));
    sig.extend(f.iter().map(|&x| (x * 6000.0) as i16));
    (sig, v.div_ceil(960)..(v + s) / 960)
}

/// Hybrid mono: markers carry the Hybrid TOC, the §2.1.9 cadence
/// holds, and the stream round-trips with per-packet statuses.
#[test]
fn hybrid_mono_dtx_markers_cadence_and_roundtrip() {
    let (sig, silent) = voice_silence_voice_48k(500, 3_000);
    let mut enc = HybridEncoderMono::new(Bandwidth::Fb, 200).unwrap();
    enc.set_dtx(true);
    let packets: Vec<Vec<u8>> = sig
        .chunks_exact(960)
        .map(|c| enc.encode_packet_elected(c, 80).unwrap())
        .collect();

    let markers: Vec<usize> = (0..packets.len())
        .filter(|&k| packets[k].len() == 1)
        .collect();
    assert!(
        markers.len() > 100,
        "hybrid DTX barely fired: {}",
        markers.len()
    );
    for &k in &markers {
        // Config 15 (Hybrid FB 20 ms), mono, code 0.
        assert_eq!(packets[k], vec![0x78u8], "hybrid marker TOC at {k}");
        assert!(silent.contains(&k), "marker outside the silent run");
    }
    // §2.1.9 cadence between coded packets deep in the run.
    let coded_in_run: Vec<usize> = (markers[0]..silent.end)
        .filter(|&k| packets[k].len() > 1)
        .collect();
    for w in coded_in_run.windows(2) {
        assert_eq!(w[1] - w[0], 21, "hybrid refresh cadence at {w:?}");
    }

    let mut dec = OpusDecoder::new();
    for (k, p) in packets.iter().enumerate() {
        let out = dec.decode_packet(p).expect("decode");
        assert_eq!(out.samples_per_channel(), 960, "packet {k}");
        let want = if p.len() == 1 {
            FrameDecodeStatus::DtxOrLost
        } else {
            FrameDecodeStatus::HybridDecoded
        };
        assert_eq!(out.frame_outcomes[0].status, want, "packet {k}");
    }
}

/// Hybrid stereo: the stereo Hybrid marker + a clean two-channel
/// round trip across the run.
#[test]
fn hybrid_stereo_dtx_markers_and_roundtrip() {
    let (mono, silent) = voice_silence_voice_48k(400, 2_000);
    let inter: Vec<i16> = mono
        .iter()
        .flat_map(|&s| [(f64::from(s) * 0.9) as i16, (f64::from(s) * 0.5) as i16])
        .collect();
    let mut enc = HybridEncoderStereo::new(Bandwidth::Fb, 200).unwrap();
    enc.set_dtx(true);
    let packets: Vec<Vec<u8>> = inter
        .chunks_exact(1920)
        .map(|c| enc.encode_packet_elected(c, 120).unwrap())
        .collect();
    let markers: Vec<usize> = (0..packets.len())
        .filter(|&k| packets[k].len() == 1)
        .collect();
    assert!(markers.len() > 60, "stereo hybrid DTX barely fired");
    for &k in &markers {
        // Config 15, stereo flag SET, code 0.
        assert_eq!(packets[k], vec![0x7Cu8], "stereo hybrid marker at {k}");
        assert!(silent.contains(&k));
    }
    let mut dec = OpusDecoder::new();
    for (k, p) in packets.iter().enumerate() {
        let out = dec.decode_packet(p).expect("decode");
        assert_eq!(out.channels, 2, "packet {k}");
        assert_eq!(out.samples_per_channel(), 960, "packet {k}");
    }
}

/// SILK VBR arm: markers commit 1 byte to the drift, the realized
/// silent-run rate collapses far below target, and the stream still
/// decodes packet for packet.
#[test]
fn silk_vbr_dtx_collapses_the_silent_run() {
    let (sig, silent) = voice_silence_voice(500, 3_000);
    let mut on = SilkVbrEncoderMono::new(Bandwidth::Wb, 200, 24_000, false).unwrap();
    on.set_dtx(true);
    let mut off = SilkVbrEncoderMono::new(Bandwidth::Wb, 200, 24_000, false).unwrap();
    let pk_on: Vec<Vec<u8>> = sig
        .chunks_exact(WB_20MS)
        .map(|c| on.encode_frame(c).unwrap())
        .collect();
    let pk_off: Vec<Vec<u8>> = sig
        .chunks_exact(WB_20MS)
        .map(|c| off.encode_frame(c).unwrap())
        .collect();

    let markers = pk_on.iter().filter(|p| p.len() == 1).count();
    assert!(markers > 100, "VBR DTX barely fired: {markers}");
    let run_on: usize = silent.clone().map(|k| pk_on[k].len()).sum();
    let run_off: usize = silent.clone().map(|k| pk_off[k].len()).sum();
    assert!(
        (run_on as f64) < run_off as f64 * 0.5,
        "VBR silent-run savings too small: {run_on} vs {run_off}"
    );

    let mut dec = OpusDecoder::new();
    for p in &pk_on {
        let out = dec.decode_packet(p).expect("decode");
        assert_eq!(out.samples_per_channel(), 960);
    }
}
