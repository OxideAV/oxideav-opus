//! RFC 6716 §2.1.4 / §3.2: 40 and 60 ms packets on every arm of the
//! unified encoder — native single SILK frames, and code-3
//! multi-frame packets of 20 ms frames on the CELT-only and Hybrid
//! arms (two / three frames under one TOC, §3.2.5 VBR lengths or CBR
//! padding, DTX markers with every frame zero-length, and the §4.5.1
//! transition redundancy in the first / last frame of the packet).

use oxideav_opus::celt_redundancy::{RedundancyDecision, RedundancyPosition};
use oxideav_opus::{
    Application, Bandwidth, FrameCountCode, Mode, OpusDecoder, OpusEncoder, OpusPacket, OpusTocByte,
};

fn multitone(samples: usize, channels: usize, amp: f64) -> Vec<i16> {
    (0..samples * channels)
        .map(|i| {
            let t = (i / channels) as f64 / 48_000.0;
            let v = (std::f64::consts::TAU * 313.7 * t).sin()
                + 0.6 * (std::f64::consts::TAU * 741.3 * t).sin()
                + 0.4 * (std::f64::consts::TAU * 1327.9 * t).sin();
            (amp * 0.5
                * v
                * if channels == 2 && i % 2 == 1 {
                    0.7
                } else {
                    1.0
                }) as i16
        })
        .collect()
}

fn snr_db(input: &[i16], out: &[i16], channels: usize, lag: usize, skip: usize) -> f64 {
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    let n = input.len().min(out.len()) / channels - lag;
    for i in skip..n {
        for k in 0..channels {
            let w = f64::from(input[i * channels + k]);
            let d = w - f64::from(out[(i + lag) * channels + k]);
            sig += w * w;
            err += d * d;
        }
    }
    10.0 * (sig / err.max(1e-9)).log10()
}

struct Stream {
    packets: Vec<Vec<u8>>,
    pcm: Vec<i16>,
}

fn encode(
    input: &[i16],
    channels: usize,
    tenths: u16,
    mode: Option<Mode>,
    bitrate: u32,
    vbr: bool,
    dtx: bool,
) -> Stream {
    let mut enc = OpusEncoder::new(channels, Application::Audio, bitrate).expect("encoder");
    enc.set_signal_adaptive(false);
    enc.set_frame_tenths_ms(tenths).expect("tenths");
    enc.set_mode(mode).expect("mode");
    enc.set_vbr(vbr);
    enc.set_dtx(dtx);
    let n = enc.frame_samples() * channels;
    let mut dec = OpusDecoder::new();
    let mut packets = Vec::new();
    let mut pcm = Vec::new();
    for frame in input.chunks_exact(n) {
        let p = enc.encode_frame(frame).expect("encode");
        let out = dec.decode_packet(&p).expect("decode");
        assert_eq!(
            out.samples_per_channel(),
            enc.frame_samples(),
            "packet duration on the decoder"
        );
        pcm.extend_from_slice(&out.pcm);
        packets.push(p);
    }
    Stream { packets, pcm }
}

fn shape(packet: &[u8]) -> (Mode, Bandwidth, u16, FrameCountCode, usize, usize) {
    let toc = OpusTocByte::parse(packet).expect("toc");
    let parsed = OpusPacket::parse(packet).expect("packet");
    (
        toc.mode,
        toc.bandwidth,
        toc.frame_size_tenths_ms,
        toc.frame_count_code,
        parsed.frames().len(),
        parsed.padding,
    )
}

/// CELT-only and Hybrid 40 / 60 ms packets are code-3 packets of two /
/// three 20 ms frames, decode to the full packet duration, and sit on
/// the target rate at the quality of the 20 ms stream.
#[test]
fn celt_and_hybrid_long_packets_are_code3_of_20ms_frames() {
    for (mode, bitrate) in [(Mode::CeltOnly, 48_000u32), (Mode::Hybrid, 32_000)] {
        for channels in [1usize, 2] {
            let seconds = 2.0;
            let input = multitone((48_000.0 * seconds) as usize + 2880, channels, 9000.0);
            let short = encode(&input, channels, 200, Some(mode), bitrate, true, false);
            let ref_snr = snr_db(&input, &short.pcm, channels, 120, 4800);
            // Same bitrate-driven bandwidth decision as the 20 ms stream.
            let want_bw = shape(&short.packets[0]).1;
            for tenths in [400u16, 600] {
                let m = usize::from(tenths / 200);
                let s = encode(&input, channels, tenths, Some(mode), bitrate, true, false);
                for p in &s.packets {
                    let (pm, bw, fs, code, frames, pad) = shape(p);
                    assert_eq!(pm, mode);
                    assert_eq!(bw, want_bw);
                    assert_eq!(fs, 200, "20 ms frames inside the packet");
                    assert_eq!(code, FrameCountCode::Arbitrary);
                    assert_eq!(frames, m, "{tenths} tenths → {m} frames");
                    assert_eq!(pad, 0, "VBR packets carry no padding");
                }
                let bytes: usize = s.packets.iter().map(Vec::len).sum();
                let kbps = bytes as f64 * 8.0
                    / (s.packets.len() as f64 * f64::from(tenths) / 10_000.0)
                    / 1000.0;
                assert!(
                    (kbps - bitrate as f64 / 1000.0).abs() < 0.12 * bitrate as f64 / 1000.0,
                    "{mode:?} {channels}ch {tenths}: {kbps:.1} kb/s vs {bitrate}"
                );
                let snr = snr_db(&input, &s.pcm, channels, 120, 4800);
                assert!(
                    snr > ref_snr - 1.5,
                    "{mode:?} {channels}ch {tenths}: {snr:.2} dB vs 20 ms {ref_snr:.2} dB"
                );
            }
        }
    }
}

/// Hard CBR pads the code-3 framing to the exact per-packet size.
#[test]
fn long_packet_cbr_pads_to_exact_size() {
    let input = multitone(48_000 + 2880, 1, 9000.0);
    for (mode, bitrate) in [(Mode::CeltOnly, 32_000u32), (Mode::Hybrid, 32_000)] {
        for tenths in [400u16, 600] {
            let s = encode(&input, 1, tenths, Some(mode), bitrate, false, false);
            let want = (bitrate as usize * usize::from(tenths)) / (8 * 10_000);
            for p in &s.packets {
                assert_eq!(p.len(), want, "{mode:?} {tenths}: {} vs {want}", p.len());
                let (_, _, _, code, frames, _) = shape(p);
                assert_eq!(code, FrameCountCode::Arbitrary);
                assert_eq!(frames, usize::from(tenths / 200));
            }
        }
    }
}

/// §2.1.9 DTX on a multi-frame CELT / Hybrid packet: the marker is the
/// code-3 framing with every frame zero-length (2 bytes), the decoder
/// holds through it, and the stream resumes.
#[test]
fn long_packet_dtx_marker_is_all_zero_length_frames() {
    let mut input = multitone(48_000, 1, 9000.0);
    input.extend(std::iter::repeat_n(0i16, 48_000 * 2));
    input.extend(multitone(48_000, 1, 9000.0));
    for mode in [Mode::CeltOnly, Mode::Hybrid] {
        let s = encode(&input, 1, 400, Some(mode), 32_000, true, true);
        let markers: Vec<&Vec<u8>> = s.packets.iter().filter(|p| p.len() == 2).collect();
        assert!(
            markers.len() >= 30,
            "{mode:?}: {} markers over 2 s of silence",
            markers.len()
        );
        for p in &markers {
            let parsed = OpusPacket::parse(p).expect("marker");
            assert_eq!(parsed.frames().len(), 2);
            assert!(parsed.frames().iter().all(|f| f.is_empty()));
        }
        // The stream resumes and decodes to the right length.
        assert_eq!(s.pcm.len(), input.len());
        let tail = snr_db(&input[48_000 * 3..], &s.pcm[48_000 * 3..], 1, 120, 4800);
        assert!(tail > 8.0, "{mode:?}: post-DTX tail {tail:.2} dB");
    }
}

/// Transitions into and out of multi-frame packets keep the §4.5.1
/// redundancy at its Figure 18 position: SILK 40 ms → CELT 40 ms
/// carries the end-position frame in the LAST frame of the last SILK
/// packet, CELT → Hybrid the beginning-position frame in the FIRST
/// frame of the first Hybrid packet, and every packet decodes at the
/// full duration.
#[test]
fn long_packet_transitions_place_redundancy() {
    let channels = 1;
    let legs: &[(usize, u32)] = &[(15, 12_000), (15, 48_000), (15, 28_000), (15, 12_000)];
    let total: usize = legs.iter().map(|(c, _)| c * 1920).sum();
    let input = multitone(total, channels, 9000.0);
    let mut enc = OpusEncoder::new(channels, Application::Audio, legs[0].1).expect("encoder");
    enc.set_signal_adaptive(false);
    enc.set_frame_tenths_ms(400).expect("40 ms");
    let mut dec = OpusDecoder::new();
    let mut off = 0usize;
    let mut log = Vec::new();
    for &(count, rate) in legs {
        enc.set_bitrate(rate).expect("rate");
        for _ in 0..count {
            let p = enc.encode_frame(&input[off..off + 1920]).expect("encode");
            off += 1920;
            let out = dec.decode_packet(&p).expect("decode");
            assert_eq!(out.samples_per_channel(), 1920);
            let (mode, _, fs, _, frames, _) = shape(&p);
            log.push((mode, fs, frames, dec.last_redundancy()));
        }
    }
    // Leg 1 (12k): SILK NB 40 ms native single frames.
    assert!(log[..15]
        .iter()
        .all(|l| l.0 == Mode::SilkOnly && l.1 == 400 && l.2 == 1));
    // The transition carrier (first packet of leg 2 is coded in the
    // old configuration) carries END redundancy; the rest of leg 2 is
    // CELT 2 × 20 ms.
    assert_eq!(log[15].0, Mode::SilkOnly);
    assert!(
        matches!(
            log[15].3,
            RedundancyDecision::Present {
                position: RedundancyPosition::End,
                ..
            }
        ),
        "SILK→CELT end redundancy: {:?}",
        log[15].3
    );
    assert!(log[16..30]
        .iter()
        .all(|l| l.0 == Mode::CeltOnly && l.1 == 200 && l.2 == 2));
    // CELT → Hybrid: the first Hybrid packet's first frame carries the
    // BEGINNING redundancy (the decoder reports the last frame it
    // decoded, so check the packet whose first frame carried it by
    // decoding it alone).
    assert_eq!(log[30].0, Mode::CeltOnly);
    assert!(log[31..45]
        .iter()
        .all(|l| l.0 == Mode::Hybrid && l.1 == 200 && l.2 == 2));
    // Hybrid → SILK NB: end redundancy in the LAST Hybrid frame of the
    // carrier packet.
    assert_eq!(log[45].0, Mode::Hybrid);
    assert!(
        matches!(
            log[45].3,
            RedundancyDecision::Present {
                position: RedundancyPosition::End,
                ..
            }
        ),
        "Hybrid→SILK end redundancy: {:?}",
        log[45].3
    );
    assert!(log[46..]
        .iter()
        .all(|l| l.0 == Mode::SilkOnly && l.1 == 400));
}

/// The beginning-position redundancy of a CELT → Hybrid switch sits
/// in the first frame of the first multi-frame Hybrid packet.
#[test]
fn long_packet_begin_redundancy_is_in_the_first_frame() {
    let input = multitone(1920 * 40, 1, 9000.0);
    let mut enc = OpusEncoder::new(1, Application::Audio, 48_000).expect("encoder");
    enc.set_signal_adaptive(false);
    enc.set_frame_tenths_ms(400).expect("40 ms");
    let mut packets = Vec::new();
    for (i, frame) in input.chunks_exact(1920).enumerate() {
        if i == 20 {
            enc.set_bitrate(28_000).expect("rate");
        }
        packets.push(enc.encode_frame(frame).expect("encode"));
    }
    // Packet 21 is the first Hybrid packet.
    let (mode, _, _, _, frames, _) = shape(&packets[21]);
    assert_eq!(mode, Mode::Hybrid);
    assert_eq!(frames, 2);
    let mut dec = OpusDecoder::new();
    for p in &packets[..21] {
        dec.decode_packet(p).expect("decode");
    }
    // Decode the two frames of packet 21 separately as code-0 packets
    // with the same TOC config: the first carries the redundancy.
    let parsed = OpusPacket::parse(&packets[21]).expect("packet");
    let toc0 = packets[21][0] & !0x03; // code 0
    let mut reds = Vec::new();
    for f in parsed.frames() {
        let single = [&[toc0][..], f].concat();
        dec.decode_packet(&single).expect("frame decode");
        reds.push(dec.last_redundancy());
    }
    assert!(
        matches!(
            reds[0],
            RedundancyDecision::Present {
                position: RedundancyPosition::Beginning,
                ..
            }
        ),
        "first frame: {:?}",
        reds[0]
    );
    assert_eq!(reds[1], RedundancyDecision::NotPresent, "second frame");
}
