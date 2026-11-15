#![no_main]

//! Coverage-guided harness for the unified `OpusEncoder`: the fuzz
//! input picks the channel count, application profile and every §2.1
//! knob (bitrate, packet duration, forced or automatic mode and
//! bandwidth, VBR / constrained VBR / CBR, DTX, FEC + loss percentage,
//! complexity, transition redundancy), re-draws a subset of them
//! between packets so §4.5 configuration switches happen mid-stream,
//! and supplies the PCM.
//!
//! Contract under test, for arbitrary PCM and any reachable knob
//! sequence: the encoder never panics; every packet respects the §3.2
//! size limits and decodes through the streaming `OpusDecoder` without
//! an error status and with the exact sample count; and the decoder
//! ends each packet on the encoder's reported final range (RFC 6716
//! §6).

use libfuzzer_sys::fuzz_target;
use oxideav_opus::decoder::{FrameDecodeStatus, OpusDecoder};
use oxideav_opus::opus_encoder::{Application, OpusEncoder};
use oxideav_opus::toc::{Bandwidth, Mode};

const DURATIONS: [u16; 6] = [25, 50, 100, 200, 400, 600];

fn apply_knobs(enc: &mut OpusEncoder, a: u8, b: u8, c: u8) {
    // Bitrate 6 .. ~260 kb/s.
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

fuzz_target!(|data: &[u8]| {
    let mut it = data.iter().copied();
    let (Some(head), Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next(), it.next())
    else {
        return;
    };
    let channels = 1 + usize::from(head & 1);
    let application = match (head >> 1) & 3 {
        0 => Application::Voip,
        1 => Application::Audio,
        _ => Application::RestrictedLowDelay,
    };
    let Ok(mut enc) = OpusEncoder::new(channels, application, 6_000 + u32::from(a) * 1_000) else {
        return;
    };
    // Low rungs keep the executions fast; the ladder itself is
    // exercised by the two low bits.
    enc.set_complexity((head >> 3) & 3);
    enc.set_signal_adaptive(head & 0x20 != 0);
    apply_knobs(&mut enc, a, b, c);

    let rest: Vec<u8> = it.collect();
    let mut dec = OpusDecoder::new();
    let mut off = 0usize;
    for packet_index in 0..6 {
        if packet_index > 0 && off + 3 <= rest.len() && rest[off] & 3 == 0 {
            apply_knobs(&mut enc, rest[off], rest[off + 1], rest[off + 2]);
            off += 3;
        }
        let spf = enc.frame_samples();
        let mut pcm = vec![0i16; spf * channels];
        // The input bytes are stretched (held for 8 samples and offset
        // by a slow ramp) so a short input still covers whole frames
        // with non-trivial, non-white content.
        for (i, v) in pcm.iter_mut().enumerate() {
            let idx = off + (i / 8) * 2;
            let lo = rest.get(idx).copied().unwrap_or(0);
            let hi = rest.get(idx + 1).copied().unwrap_or(0);
            *v = i16::from_le_bytes([lo, hi]).wrapping_add((i as i16).wrapping_mul(3));
        }
        off += (pcm.len() / 8) * 2;
        let packet = enc.encode_frame(&pcm).expect("encode");
        assert!(
            (1..=1275 * 3 + 7).contains(&packet.len()),
            "size {}",
            packet.len()
        );
        let out = dec.decode_packet(&packet).expect("own decode");
        assert_eq!(out.samples_per_channel(), spf);
        assert_eq!(usize::from(out.channels), channels);
        for o in &out.frame_outcomes {
            assert!(
                !matches!(
                    o.status,
                    FrameDecodeStatus::SilkDecodeError | FrameDecodeStatus::CeltDecodeError
                ),
                "error status {:?}",
                o.status
            );
        }
        assert_eq!(dec.final_range(), enc.final_range(), "final range");
        if off >= rest.len() {
            break;
        }
    }
});
