#![no_main]

//! Coverage-guided harness for the round-431 Opus-level VBR election:
//! the fuzz input picks a CELT stream configuration (bandwidth × frame
//! size × mono/stereo), a target bitrate, the constrained-VBR flag,
//! and supplies the PCM; every elected packet MUST decode cleanly
//! through the streaming `OpusDecoder` with the exact per-frame sample
//! count, sizes must respect the §3.2.1 limits, and in constrained
//! mode every packet must obey the controller's reservoir ceiling
//! snapshot taken before the encode. A panic, a rejected packet, or a
//! ceiling bust is an election (or codec) bug — arbitrary PCM at any
//! reachable target must always produce a conforming VBR stream.

use libfuzzer_sys::fuzz_target;
use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::toc::Bandwidth;
use oxideav_opus::vbr::CeltVbrEncoder;

fuzz_target!(|data: &[u8]| {
    let mut it = data.iter().copied();
    let (Some(cfg), Some(r0), Some(r1)) = (it.next(), it.next(), it.next()) else {
        return;
    };
    let bandwidth = match cfg & 3 {
        0 => Bandwidth::Nb,
        1 => Bandwidth::Wb,
        2 => Bandwidth::Swb,
        _ => Bandwidth::Fb,
    };
    let tenths: u16 = match (cfg >> 2) & 3 {
        0 => 25,
        1 => 50,
        2 => 100,
        _ => 200,
    };
    let stereo = (cfg >> 4) & 1 == 1;
    let constrained = (cfg >> 5) & 1 == 1;
    // Target 6 kb/s .. 511 kb/s (8 b/s steps over the 16-bit seed);
    // the controller rejects the per-frame-unreachable corners
    // itself.
    let bitrate = 6_000 + ((u32::from(r0) | (u32::from(r1) << 8)) * 8) % 505_000;

    let Ok(mut enc) = CeltVbrEncoder::new(bandwidth, tenths, stereo, bitrate, constrained) else {
        return;
    };
    let mut dec = OpusDecoder::new();
    let spf = enc.frame_samples();
    let ch = enc.channels();

    let rest: Vec<u8> = it.collect();
    let mut off = 0usize;
    for _ in 0..4 {
        let mut pcm = vec![0i16; spf * ch];
        for v in pcm.iter_mut() {
            let lo = rest.get(off).copied().unwrap_or(0);
            let hi = rest.get(off + 1).copied().unwrap_or(0);
            *v = i16::from_le_bytes([lo, hi]);
            off += 2;
        }
        let ceiling_bits = enc.rate_control().constrained_ceiling_bits();
        let (packet, _info) = enc.encode_frame(&pcm).expect("VBR encode");
        assert!((3..=1276).contains(&packet.len()), "size {}", packet.len());
        if constrained {
            // Byte rounding may add up to 7 bits past the ceiling.
            assert!(
                (packet.len() * 8) as f64 <= ceiling_bits + 7.0 + 1e-6,
                "constrained ceiling bust: {} bytes vs {} bits",
                packet.len(),
                ceiling_bits
            );
        }
        let out = dec.decode_packet(&packet).expect("own decode");
        assert_eq!(out.samples_per_channel(), spf);
        if off >= rest.len() {
            break;
        }
    }
});
