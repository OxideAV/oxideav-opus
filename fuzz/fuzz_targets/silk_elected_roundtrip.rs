#![no_main]

//! Coverage-guided harness for the round-437 SILK-layer rate control:
//! the fuzz input picks a SILK-only configuration (internal bandwidth
//! × packet duration × mono/stereo × FEC), a per-packet byte-target
//! script, and supplies the internal-rate PCM; every elected packet
//! MUST decode cleanly through the streaming `OpusDecoder` with the
//! exact §3 sample count and stay inside the §3.2.1 limits. The
//! election may floor-raise above a starving target but must never
//! exceed the §3.2.1 maximum or fail on arbitrary PCM — a panic or a
//! decode error is a rate-control (or noise-shaping-quantizer) bug.

use libfuzzer_sys::fuzz_target;
use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::silk_encoder::{SilkEncoderMono, SilkEncoderStereo};
use oxideav_opus::toc::Bandwidth;

fuzz_target!(|data: &[u8]| {
    let mut it = data.iter().copied();
    let Some(cfg) = it.next() else {
        return;
    };
    let bandwidth = match cfg & 3 {
        0 => Bandwidth::Nb,
        1 => Bandwidth::Mb,
        _ => Bandwidth::Wb,
    };
    let tenths: u16 = match (cfg >> 2) & 3 {
        0 => 100,
        1 => 200,
        2 => 400,
        _ => 600,
    };
    let stereo = (cfg >> 4) & 1 == 1;
    let fec = (cfg >> 5) & 1 == 1;

    let rest: Vec<u8> = it.collect();
    if rest.len() < 4 {
        return;
    }

    // Per-packet script: 1 target byte + PCM bytes (i8 → f32 scaled),
    // cycled over the remaining input. Cap the work per input.
    let expected_per_channel_48k = 48 * usize::from(tenths) / 10;
    let mut dec = OpusDecoder::new();
    let max_packets = 6usize;

    if stereo {
        let Ok(mut enc) = SilkEncoderStereo::with_packet_duration(bandwidth, tenths) else {
            return;
        };
        enc.set_fec(fec);
        let spf = enc.frame_samples();
        let mut pos = 0usize;
        for _ in 0..max_packets {
            if pos >= rest.len() {
                break;
            }
            let target = 2 + usize::from(rest[pos]) * 5; // 2..=1277
            pos += 1;
            let mut left = Vec::with_capacity(spf);
            let mut right = Vec::with_capacity(spf);
            for i in 0..spf {
                let b = rest[(pos + 2 * i) % rest.len()] as i8;
                let c = rest[(pos + 2 * i + 1) % rest.len()] as i8;
                left.push(f32::from(b) / 128.0);
                right.push(f32::from(c) / 128.0);
            }
            pos += spf.min(rest.len());
            let out = enc
                .encode_packet_elected(&left, &right, None, target)
                .expect("stereo election must succeed on arbitrary PCM");
            assert!(out.packet.len() <= 1276, "packet busts §3.2.1");
            let audio = dec
                .decode_packet(&out.packet)
                .expect("elected stereo packet must decode");
            assert_eq!(audio.channels, 2);
            assert_eq!(audio.samples_per_channel(), expected_per_channel_48k);
        }
    } else {
        let Ok(mut enc) = SilkEncoderMono::with_packet_duration(bandwidth, tenths) else {
            return;
        };
        enc.set_fec(fec);
        let spf = enc.frame_samples();
        let mut pos = 0usize;
        for _ in 0..max_packets {
            if pos >= rest.len() {
                break;
            }
            let target = 2 + usize::from(rest[pos]) * 5;
            pos += 1;
            let mut pcm = Vec::with_capacity(spf);
            for i in 0..spf {
                let b = rest[(pos + i) % rest.len()] as i8;
                pcm.push(f32::from(b) / 128.0);
            }
            pos += spf.min(rest.len());
            let out = enc
                .encode_packet_elected(&pcm, target)
                .expect("mono election must succeed on arbitrary PCM");
            assert!(out.packet.len() <= 1276, "packet busts §3.2.1");
            let audio = dec
                .decode_packet(&out.packet)
                .expect("elected mono packet must decode");
            assert_eq!(audio.channels, 1);
            assert_eq!(audio.samples_per_channel(), expected_per_channel_48k);
        }
    }
});
