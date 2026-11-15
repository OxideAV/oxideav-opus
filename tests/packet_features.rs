//! Packet-level features on streams the crate's own encoder writes:
//!
//! * every §3.2 frame-count code (0, 1, 2, 3 CBR / VBR, up to the
//!   48-frame / 120 ms ceiling) and §3.2.5 padding lengths across the
//!   255-chain boundaries, plus the Appendix B self-delimited forms —
//!   re-framing the same compressed frames must not change a single
//!   output sample nor the §6 final range of the packet's last frame;
//! * §3.2.1 zero-length (DTX / lost) frames inside multi-frame packets;
//! * RFC 7845 §5.1.1 multistream mapping: family 1 at every channel
//!   count 1..=8 and a family-255 style mapping with silent and
//!   duplicated channels, each output channel checked against the
//!   plain decode of the stream channel it maps to.

use oxideav_opus::toc::{Bandwidth, Mode};
use oxideav_opus::{
    Application, ChannelMappingTable, FrameDecodeStatus, MultistreamDecoder, OpusDecoder,
    OpusEncoder, OpusPacket, assemble_multistream_packet, compose_packet, compose_packet_code3,
    compose_self_delimited,
};

fn signal(samples: usize, channels: usize, seed: u32) -> Vec<i16> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(samples * channels);
    for i in 0..samples {
        let t = i as f64 / 48_000.0;
        let tau = std::f64::consts::TAU;
        let f0 = 140.0 + f64::from(seed % 7) * 23.0;
        let mut v = 0.0;
        for h in 1..=8 {
            v += (tau * f0 * f64::from(h) * t).sin() / f64::from(h);
        }
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = f64::from(state >> 8) / f64::from(1u32 << 24) - 0.5;
        let envelope = 0.4 + 0.6 * (tau * 2.3 * t).sin().abs();
        let left = 7000.0 * envelope * v + 900.0 * noise + 1500.0 * (tau * 3300.0 * t).sin();
        out.push(left as i16);
        if channels == 2 {
            out.push((0.6 * left + 2500.0 * (tau * 610.0 * t).sin()) as i16);
        }
    }
    out
}

/// Encode `count` code-0 frames and return each frame's TOC byte and
/// compressed bytes (the §3.2 framing stripped).
fn frames_of(
    mode: Mode,
    bandwidth: Bandwidth,
    tenths: u16,
    channels: usize,
    count: usize,
    cbr: bool,
) -> (u8, Vec<Vec<u8>>) {
    let mut enc = OpusEncoder::new(channels, Application::Audio, 28_000 * channels as u32).unwrap();
    enc.set_mode(Some(mode)).unwrap();
    enc.set_bandwidth(Some(bandwidth));
    enc.set_frame_tenths_ms(tenths).unwrap();
    enc.set_vbr(!cbr);
    let n = enc.frame_samples() * channels;
    let pcm = signal(
        enc.frame_samples() * count,
        channels,
        77 + u32::from(tenths),
    );
    let mut toc = 0u8;
    let mut frames = Vec::with_capacity(count);
    for chunk in pcm.chunks_exact(n) {
        let packet = enc.encode_frame(chunk).unwrap();
        let parsed = OpusPacket::parse(&packet).unwrap();
        assert_eq!(parsed.frames().len(), 1, "one frame per encoder packet");
        toc = packet[0] & 0xFC;
        frames.push(parsed.frames()[0].to_vec());
    }
    (toc, frames)
}

/// Decode a packet sequence on a fresh decoder: all PCM, and the final
/// range after each packet.
fn decode_all(packets: &[Vec<u8>], self_delimited: bool) -> (Vec<i16>, Vec<u32>) {
    let mut dec = OpusDecoder::new();
    let mut pcm = Vec::new();
    let mut ranges = Vec::new();
    for p in packets {
        let audio = if self_delimited {
            dec.decode_self_delimited_packet(p)
        } else {
            dec.decode_packet(p)
        }
        .expect("decode");
        for o in &audio.frame_outcomes {
            assert!(!matches!(
                o.status,
                FrameDecodeStatus::SilkDecodeError | FrameDecodeStatus::CeltDecodeError
            ));
        }
        pcm.extend_from_slice(&audio.pcm);
        ranges.push(dec.final_range());
    }
    (pcm, ranges)
}

/// Group `frames` into packets of `group` frames with `build`, and
/// check the decode against the code-0 reference.
fn check_grouping(
    label: &str,
    frames: &[Vec<u8>],
    group: usize,
    reference: &(Vec<i16>, Vec<u32>),
    self_delimited: bool,
    build: impl Fn(&[&[u8]]) -> Vec<u8>,
) {
    let packets: Vec<Vec<u8>> = frames
        .chunks_exact(group)
        .map(|g| {
            let refs: Vec<&[u8]> = g.iter().map(Vec::as_slice).collect();
            build(&refs)
        })
        .collect();
    let (pcm, ranges) = decode_all(&packets, self_delimited);
    let whole = packets.len() * group;
    let per_frame = reference.0.len() / frames.len();
    assert_eq!(
        pcm,
        reference.0[..whole * per_frame],
        "{label}: PCM differs from the code-0 decode"
    );
    for (i, r) in ranges.iter().enumerate() {
        assert_eq!(
            *r,
            reference.1[(i + 1) * group - 1],
            "{label}: final range of packet {i}"
        );
    }
}

#[test]
fn every_frame_count_code_and_padding_decodes_identically() {
    let configs = [
        ("silk", Mode::SilkOnly, Bandwidth::Wb, 200u16, 1usize, false),
        ("hybrid", Mode::Hybrid, Bandwidth::Fb, 200, 2, false),
        ("celt-cbr", Mode::CeltOnly, Bandwidth::Fb, 200, 1, true),
        ("celt-10ms", Mode::CeltOnly, Bandwidth::Swb, 100, 2, false),
    ];
    for (name, mode, bandwidth, tenths, channels, cbr) in configs {
        let (toc, frames) = frames_of(mode, bandwidth, tenths, channels, 12, cbr);
        let code0: Vec<Vec<u8>> = frames
            .iter()
            .map(|f| compose_packet(toc, &[f.as_slice()]).unwrap())
            .collect();
        let reference = decode_all(&code0, false);
        assert!(reference.1.iter().all(|&r| r != 0), "{name}: ranges");

        // Code 1: two frames of equal size (only where the sizes agree).
        if frames.iter().all(|f| f.len() == frames[0].len()) {
            check_grouping(
                &format!("{name} code 1"),
                &frames,
                2,
                &reference,
                false,
                |g| compose_packet(toc | 1, g).unwrap(),
            );
            check_grouping(
                &format!("{name} code 1 self-delimited"),
                &frames,
                2,
                &reference,
                true,
                |g| compose_self_delimited(toc | 1, g, false, 0).unwrap(),
            );
        }
        // Code 2: two frames, the first length explicit.
        check_grouping(
            &format!("{name} code 2"),
            &frames,
            2,
            &reference,
            false,
            |g| compose_packet(toc | 2, g).unwrap(),
        );
        check_grouping(
            &format!("{name} code 2 self-delimited"),
            &frames,
            2,
            &reference,
            true,
            |g| compose_self_delimited(toc | 2, g, false, 0).unwrap(),
        );
        // Code 3, VBR, 1..=6 frames (up to the 120 ms ceiling at 20 ms).
        let max_group = (1200 / usize::from(tenths)).min(6);
        for group in 1..=max_group {
            for padding in [0usize, 1, 253, 254, 255, 509, 700] {
                check_grouping(
                    &format!("{name} code 3 vbr M={group} pad={padding}"),
                    &frames,
                    group,
                    &reference,
                    false,
                    |g| compose_packet_code3(toc | 3, g, true, padding).unwrap(),
                );
            }
            check_grouping(
                &format!("{name} code 3 self-delimited M={group}"),
                &frames,
                group,
                &reference,
                true,
                |g| compose_self_delimited(toc | 3, g, true, 3).unwrap(),
            );
        }
        // Code 3, CBR, one frame, padded.
        for padding in [0usize, 2, 254, 300] {
            check_grouping(
                &format!("{name} code 3 cbr pad={padding}"),
                &frames,
                1,
                &reference,
                false,
                |g| compose_packet_code3(toc | 3, g, false, padding).unwrap(),
            );
        }
    }
}

#[test]
fn forty_eight_frames_in_one_packet() {
    // 48 × 2.5 ms = the §3.2.5 frame-count and 120 ms ceilings at once.
    let (toc, frames) = frames_of(Mode::CeltOnly, Bandwidth::Fb, 25, 1, 96, true);
    let code0: Vec<Vec<u8>> = frames
        .iter()
        .map(|f| compose_packet(toc, &[f.as_slice()]).unwrap())
        .collect();
    let reference = decode_all(&code0, false);
    check_grouping("48 frames cbr", &frames, 48, &reference, false, |g| {
        compose_packet_code3(toc | 3, g, false, 0).unwrap()
    });
    check_grouping("48 frames vbr", &frames, 48, &reference, false, |g| {
        compose_packet_code3(toc | 3, g, true, 17).unwrap()
    });
    // One more frame would exceed 120 ms: the composer refuses it and
    // a hand-built header is rejected by the decoder.
    let refs: Vec<&[u8]> = frames[..49].iter().map(Vec::as_slice).collect();
    assert!(compose_packet_code3(toc | 3, &refs, false, 0).is_err());
    let mut bad = vec![toc | 3, 49];
    for f in &frames[..49] {
        bad.extend_from_slice(f);
    }
    assert!(OpusDecoder::new().decode_packet(&bad).is_err());
}

#[test]
fn zero_length_frames_inside_multi_frame_packets_are_concealed() {
    for (mode, bandwidth, channels) in [
        (Mode::SilkOnly, Bandwidth::Wb, 1usize),
        (Mode::CeltOnly, Bandwidth::Fb, 2),
        (Mode::Hybrid, Bandwidth::Swb, 1),
    ] {
        let (toc, frames) = frames_of(mode, bandwidth, 200, channels, 6, false);
        let mut dec = OpusDecoder::new();
        // Warm up on two whole frames.
        for f in &frames[..2] {
            dec.decode_packet(&compose_packet(toc, &[f.as_slice()]).unwrap())
                .unwrap();
        }
        // [coded, empty, coded]: the middle frame is held by §4.4.
        let empty: &[u8] = &[];
        let packet = compose_packet_code3(
            toc | 3,
            &[frames[2].as_slice(), empty, frames[3].as_slice()],
            true,
            0,
        )
        .unwrap();
        let audio = dec.decode_packet(&packet).unwrap();
        assert_eq!(audio.frame_outcomes.len(), 3);
        assert_eq!(audio.frame_outcomes[1].status, FrameDecodeStatus::DtxOrLost);
        assert_eq!(audio.samples_per_channel(), 3 * 960);
        assert_ne!(dec.final_range(), 0, "last frame is coded");
        // The hold is audio, not a hole: it carries energy after a
        // non-silent predecessor.
        let mid = &audio.pcm[960 * channels..2 * 960 * channels];
        assert!(
            mid.iter().any(|&s| s != 0),
            "{mode:?}: concealment is silent"
        );
        // [coded, empty]: a packet ending on a zero-length frame
        // reports a zero final range.
        let packet = compose_packet(toc | 2, &[frames[4].as_slice(), empty]).unwrap();
        let audio = dec.decode_packet(&packet).unwrap();
        assert_eq!(audio.frame_outcomes[1].status, FrameDecodeStatus::DtxOrLost);
        assert_eq!(dec.final_range(), 0);
        // A one-byte packet is a whole DTX frame.
        let audio = dec.decode_packet(&[toc]).unwrap();
        assert_eq!(audio.frame_outcomes[0].status, FrameDecodeStatus::DtxOrLost);
        assert_eq!(audio.samples_per_channel(), 960);
        assert_eq!(dec.final_range(), 0);
    }
}

/// Encode `packets` 20 ms packets of a mono or stereo stream.
fn stream_packets(channels: usize, seed: u32, packets: usize) -> Vec<Vec<u8>> {
    let mut enc = OpusEncoder::new(channels, Application::Audio, 48_000 * channels as u32).unwrap();
    enc.set_mode(Some(Mode::CeltOnly)).unwrap();
    let n = enc.frame_samples() * channels;
    signal(enc.frame_samples() * packets, channels, seed)
        .chunks_exact(n)
        .map(|c| enc.encode_frame(c).unwrap())
        .collect()
}

/// Decode a multistream layout and check each output channel against
/// the plain decode of the decoded channel it maps to.
fn check_mapping(label: &str, table: ChannelMappingTable) {
    const PACKETS: usize = 8;
    let n = usize::from(table.stream_count);
    let m = usize::from(table.coupled_count);
    let streams: Vec<Vec<Vec<u8>>> = (0..n)
        .map(|s| stream_packets(if s < m { 2 } else { 1 }, 100 + s as u32, PACKETS))
        .collect();
    // Plain per-stream decodes, de-interleaved into decoded channels
    // (coupled streams first: 2 each, then the mono streams).
    let mut decoded: Vec<Vec<i16>> = Vec::new();
    for (s, packets) in streams.iter().enumerate() {
        let ch = if s < m { 2 } else { 1 };
        let mut dec = OpusDecoder::new();
        let mut planes = vec![Vec::new(); ch];
        for p in packets {
            let audio = dec.decode_packet(p).unwrap();
            for frame in audio.pcm.chunks_exact(ch) {
                for (c, plane) in planes.iter_mut().enumerate() {
                    plane.push(frame[c]);
                }
            }
        }
        decoded.extend(planes);
    }
    let mut ms = MultistreamDecoder::new(table.clone());
    let outputs = table.mapping.len();
    assert_eq!(usize::from(ms.output_channels()), outputs, "{label}");
    let mut pos = 0usize;
    for k in 0..PACKETS {
        let parts: Vec<&[u8]> = streams.iter().map(|s| s[k].as_slice()).collect();
        let packet = assemble_multistream_packet(&parts).unwrap();
        let audio = ms.decode_packet(&packet).unwrap();
        assert_eq!(usize::from(audio.channels), outputs, "{label}");
        assert_eq!(audio.samples_per_channel, 960, "{label}");
        for (i, frame) in audio.pcm.chunks_exact(outputs).enumerate() {
            for (c, &index) in table.mapping.iter().enumerate() {
                let want = if index == 255 {
                    0
                } else {
                    decoded[usize::from(index)][pos + i]
                };
                assert_eq!(frame[c], want, "{label}: packet {k} sample {i} channel {c}");
            }
        }
        pos += 960;
    }
}

#[test]
fn family_one_layouts_route_every_channel_count() {
    // RFC 7845 §5.1.1.2 channel orders, with the usual pairing of
    // left/right speakers into coupled streams.
    let layouts: [(u8, u8, &[u8]); 8] = [
        (1, 0, &[0]),                      // mono
        (1, 1, &[0, 1]),                   // L R
        (2, 1, &[0, 2, 1]),                // L C R
        (2, 2, &[0, 1, 2, 3]),             // FL FR RL RR
        (3, 2, &[0, 4, 1, 2, 3]),          // FL FC FR RL RR
        (4, 2, &[0, 4, 1, 2, 3, 5]),       // + LFE
        (5, 2, &[0, 4, 1, 2, 3, 5, 6]),    // FL FC FR SL SR RC LFE
        (5, 3, &[0, 6, 1, 2, 3, 4, 5, 7]), // FL FC FR SL SR RL RR LFE
    ];
    for (streams, coupled, mapping) in layouts {
        check_mapping(
            &format!("family 1, {} channels", mapping.len()),
            ChannelMappingTable::new(streams, coupled, mapping.to_vec()),
        );
    }
}

#[test]
fn discrete_mapping_with_silent_and_duplicated_channels() {
    // Family-255 style: no speaker meaning; a silent output (255), one
    // decoded channel feeding two outputs, and a reversed order.
    check_mapping(
        "discrete",
        ChannelMappingTable::new(3, 1, vec![3, 255, 1, 0, 2, 1, 255, 3, 0, 2]),
    );
}
