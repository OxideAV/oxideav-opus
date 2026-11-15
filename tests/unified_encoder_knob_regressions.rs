//! Regressions found by the `unified_encode_roundtrip` fuzz target:
//! knob sequences on which the unified encoder must keep producing
//! packets its own decoder accepts with a matching final range.
//!
//! [`replay`] interprets an input exactly as the fuzz target does (the
//! channel count, application and every §2.1 knob drawn from the
//! leading bytes, a subset re-drawn between packets, the rest
//! stretched into PCM).

use oxideav_opus::toc::{Bandwidth, Mode};
use oxideav_opus::{Application, FrameDecodeStatus, OpusDecoder, OpusEncoder};

const DURATIONS: [u16; 6] = [25, 50, 100, 200, 400, 600];

fn apply_knobs(enc: &mut OpusEncoder, a: u8, b: u8, c: u8) {
    let _ = enc.set_bitrate(6_000 + u32::from(a) * 1_000);
    let _ = enc.set_frame_tenths_ms(DURATIONS[usize::from(b & 7) % DURATIONS.len()]);
    let mode = match (b >> 3) & 3 {
        0 => None,
        1 => Some(Mode::SilkOnly),
        2 => Some(Mode::Hybrid),
        _ => Some(Mode::CeltOnly),
    };
    let _ = enc.set_mode(mode);
    let bandwidth = match (b >> 5) & 7 {
        1 => Some(Bandwidth::Nb),
        2 => Some(Bandwidth::Mb),
        3 => Some(Bandwidth::Wb),
        4 => Some(Bandwidth::Swb),
        5 => Some(Bandwidth::Fb),
        _ => None,
    };
    enc.set_bandwidth(bandwidth);
    enc.set_vbr(c & 1 == 0);
    let _ = enc.set_constrained_vbr(c & 2 != 0);
    enc.set_dtx(c & 4 != 0);
    enc.set_fec(c & 8 != 0);
    enc.set_packet_loss_perc(if c & 8 != 0 { (c >> 4) * 4 } else { 0 });
    enc.set_transition_redundancy(c & 16 == 0);
}

fn replay(data: &[u8]) {
    let (head, a, b, c) = (data[0], data[1], data[2], data[3]);
    let channels = 1 + usize::from(head & 1);
    let application = match (head >> 1) & 3 {
        0 => Application::Voip,
        1 => Application::Audio,
        _ => Application::RestrictedLowDelay,
    };
    let mut enc =
        OpusEncoder::new(channels, application, 6_000 + u32::from(a) * 1_000).expect("encoder");
    enc.set_complexity((head >> 3) & 3);
    enc.set_signal_adaptive(head & 0x20 != 0);
    apply_knobs(&mut enc, a, b, c);

    let rest = &data[4..];
    let mut dec = OpusDecoder::new();
    let mut off = 0usize;
    for packet_index in 0..6 {
        if packet_index > 0 && off + 3 <= rest.len() && rest[off] & 3 == 0 {
            apply_knobs(&mut enc, rest[off], rest[off + 1], rest[off + 2]);
            off += 3;
        }
        let spf = enc.frame_samples();
        let mut pcm = vec![0i16; spf * channels];
        for (i, v) in pcm.iter_mut().enumerate() {
            let idx = off + (i / 8) * 2;
            let lo = rest.get(idx).copied().unwrap_or(0);
            let hi = rest.get(idx + 1).copied().unwrap_or(0);
            *v = i16::from_le_bytes([lo, hi]).wrapping_add((i as i16).wrapping_mul(3));
        }
        off += (pcm.len() / 8) * 2;
        let packet = enc
            .encode_frame(&pcm)
            .unwrap_or_else(|e| panic!("packet {packet_index}: encode failed: {e:?}"));
        let out = dec
            .decode_packet(&packet)
            .unwrap_or_else(|e| panic!("packet {packet_index}: decode failed: {e:?}"));
        assert_eq!(out.samples_per_channel(), spf, "packet {packet_index}");
        for o in &out.frame_outcomes {
            assert!(
                !matches!(
                    o.status,
                    FrameDecodeStatus::SilkDecodeError | FrameDecodeStatus::CeltDecodeError
                ),
                "packet {packet_index}: {:?}",
                o.status
            );
        }
        assert_eq!(
            dec.final_range(),
            enc.final_range(),
            "packet {packet_index}"
        );
        if off >= rest.len() {
            break;
        }
    }
}

#[test]
fn short_celt_packet_then_long_duration_knob_change() {
    replay(&[
        0x00, 0x27, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x0d, 0x0d, 0x0d, 0x0d, 0x0d, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x23, 0x04, 0x23, 0x07,
    ]);
}

/// Encode `packets` packets, re-drawing the packet duration from
/// `durations` every `every` packets, and hold each to a clean decode
/// with the right sample count and final range. Returns the packet
/// durations actually coded.
fn duration_walk(
    channels: usize,
    application: Application,
    bitrate: u32,
    mode: Option<Mode>,
    durations: &[u16],
    every: usize,
    packets: usize,
) -> Vec<u16> {
    let mut enc = OpusEncoder::new(channels, application, bitrate).unwrap();
    enc.set_frame_tenths_ms(durations[0]).unwrap();
    enc.set_mode(mode).unwrap();
    let mut dec = OpusDecoder::new();
    let mut coded = Vec::new();
    let mut t = 0usize;
    for k in 0..packets {
        if k % every == 0 {
            let tenths = durations[(k / every) % durations.len()];
            enc.set_frame_tenths_ms(tenths).unwrap();
        }
        let spf = enc.frame_samples();
        let pcm: Vec<i16> = (0..spf * channels)
            .map(|i| {
                let x = (t + i / channels) as f64 / 48_000.0;
                (6000.0 * (std::f64::consts::TAU * 220.0 * x).sin()
                    + 2500.0 * (std::f64::consts::TAU * 3100.0 * x).sin()) as i16
            })
            .collect();
        t += spf;
        let label = format!(
            "{mode:?} {channels} ch packet {k} ({} ms)",
            spf as f64 / 48.0
        );
        let packet = enc
            .encode_frame(&pcm)
            .unwrap_or_else(|e| panic!("{label}: encode failed: {e:?}"));
        let out = dec
            .decode_packet(&packet)
            .unwrap_or_else(|e| panic!("{label}: decode failed: {e:?}"));
        assert_eq!(out.samples_per_channel(), spf, "{label}");
        for o in &out.frame_outcomes {
            assert!(
                !matches!(
                    o.status,
                    FrameDecodeStatus::SilkDecodeError | FrameDecodeStatus::CeltDecodeError
                ),
                "{label}: {:?}",
                o.status
            );
        }
        assert_eq!(dec.final_range(), enc.final_range(), "{label}: final range");
        coded.push((spf * 10 / 48) as u16);
    }
    coded
}

/// `set_frame_tenths_ms` in the middle of a stream: every arm keeps
/// encoding at the new duration (it used to fail every later packet
/// with an error once the duration differed from the one the stream
/// opened with).
#[test]
fn packet_duration_changes_mid_stream_on_every_arm() {
    let silk_like: [u16; 4] = [200, 100, 600, 400];
    let all: [u16; 6] = [200, 25, 400, 50, 600, 100];
    for channels in [1usize, 2] {
        for (mode, durations, bitrate) in [
            (Some(Mode::SilkOnly), &silk_like[..], 20_000u32),
            (Some(Mode::Hybrid), &silk_like[..], 40_000),
            (Some(Mode::CeltOnly), &all[..], 64_000),
            // Automatic: 2.5 / 5 ms force the MDCT layer and the other
            // durations fall back to the rate ladder (SILK at 16 kb/s,
            // Hybrid at 28 kb/s) — an immediate switch into the short
            // frames, a normative transition back out.
            (None, &all[..], 16_000),
            (None, &all[..], 28_000),
        ] {
            let coded = duration_walk(
                channels,
                Application::Voip,
                bitrate * channels as u32,
                mode,
                durations,
                4,
                4 * durations.len() * 2,
            );
            for (k, &tenths) in coded.iter().enumerate() {
                assert_eq!(tenths, durations[(k / 4) % durations.len()], "packet {k}");
            }
        }
    }
}
