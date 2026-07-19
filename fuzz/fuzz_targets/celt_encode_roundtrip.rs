#![no_main]

//! Coverage-guided harness for the round-418 CELT encoder: the fuzz
//! input picks a stream configuration (bandwidth × frame size × mono/
//! stereo), a per-packet payload size, and supplies the PCM samples;
//! every produced packet MUST decode cleanly through the streaming
//! `OpusDecoder` with the exact per-frame sample count. A panic,
//! a packet the decoder rejects, or a range-coder desync inside the
//! decode is an encoder (or decoder) bug — arbitrary PCM at any legal
//! payload size must always produce a conforming stream.

use libfuzzer_sys::fuzz_target;
use oxideav_opus::celt_packet_encode::CeltEncoder;
use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::toc::Bandwidth;

fuzz_target!(|data: &[u8]| {
    let mut it = data.iter().copied();
    let (Some(cfg), Some(p0), Some(p1)) = (it.next(), it.next(), it.next()) else {
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
    // Payload 2..=1275.
    let payload = 2 + (usize::from(p0) | (usize::from(p1) << 8)) % 1274;

    let Ok(mut enc) = CeltEncoder::new(bandwidth, tenths, stereo) else {
        return;
    };
    let mut dec = OpusDecoder::new();
    let spf = enc.frame_samples();
    let ch = enc.channels();

    // Build PCM frames from the remaining bytes (16-bit LE pairs,
    // zero-extended), up to 4 packets.
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
        let (packet, _info) = enc.encode_packet(&pcm, payload).expect("CELT encode");
        assert_eq!(packet.len(), 1 + payload);
        let out = dec.decode_packet(&packet).expect("own decode");
        assert_eq!(out.samples_per_channel(), spf);
        if off >= rest.len() {
            break;
        }
    }
});
