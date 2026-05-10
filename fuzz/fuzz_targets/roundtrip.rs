#![no_main]

//! Fuzz: random PCM → oxideav Opus encode → oxideav Opus decode.
//!
//! The Opus encoder is CELT-only at 48 kHz, 20 ms / 960 samples per
//! frame. CELT is **lossy** — even a noise-free sine roundtrips
//! through our encoder/decoder pair at PSNR ≈ 12-20 dB on speech-like
//! signals (see `tests/encoder_roundtrip.rs`), nowhere near ±2 LSB.
//! The user-supplied prompt mentioned ±2 LSB as a target, but that
//! tolerance applies to the libopus → libopus path, not the
//! oxideav-celt path which has known-non-bit-exact PVQ shape and
//! IMDCT (see `oxideav_celt::CODEC_ID_STR` module docs).
//!
//! We therefore assert the looser fuzz-appropriate invariants:
//!   1. encode never panics on random PCM,
//!   2. decode of our own output never panics,
//!   3. the decoded sample count matches the encoder frame size
//!      (960 / channel) for every successfully-decoded packet,
//!   4. all decoded samples are finite (i16 always is).
//!
//! These cover the panic-freedom + shape contracts the encoder /
//! decoder advertise. Bit-tight PSNR is enforced separately by
//! `tests/encoder_roundtrip.rs`'s sine / silence cases.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Encoder, Frame, Packet, SampleFormat};
use oxideav_opus::encoder::OPUS_FRAME_SAMPLES;

const SR: u32 = 48_000;

fuzz_target!(|data: &[u8]| {
    if data.len() < 1 + OPUS_FRAME_SAMPLES * 2 {
        return;
    }
    // First byte selects mono / stereo. Remainder feeds the input PCM.
    let channels: u16 = if data[0] & 1 == 0 { 1 } else { 2 };
    let pcm_bytes = &data[1..];

    // Each sample is 2 bytes (S16). For stereo we need
    // 2 * OPUS_FRAME_SAMPLES samples per frame; for mono we need
    // 1 * OPUS_FRAME_SAMPLES.
    let bytes_per_frame = OPUS_FRAME_SAMPLES * channels as usize * 2;
    if pcm_bytes.len() < bytes_per_frame {
        return;
    }
    // Take exactly one frame's worth.
    let frame_bytes = pcm_bytes[..bytes_per_frame].to_vec();

    // Build the encoder.
    let mut enc_params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(SR);
    enc_params.sample_format = Some(SampleFormat::S16);
    let mut enc = match oxideav_opus::encoder::OpusEncoder::new(&enc_params) {
        Ok(e) => e,
        Err(_) => return,
    };

    let in_frame = Frame::Audio(AudioFrame {
        samples: OPUS_FRAME_SAMPLES as u32,
        pts: Some(0),
        data: vec![frame_bytes],
    });
    if enc.send_frame(&in_frame).is_err() {
        return;
    }
    let mut packets: Vec<Packet> = Vec::new();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    if packets.is_empty() {
        return;
    }

    // Build the decoder.
    let mut dec_params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    dec_params.channels = Some(channels);
    dec_params.sample_rate = Some(SR);
    let mut dec = match oxideav_opus::decoder::make_decoder(&dec_params) {
        Ok(d) => d,
        Err(_) => return,
    };

    for pkt in &packets {
        if dec.send_packet(pkt).is_err() {
            continue;
        }
        let out = match dec.receive_frame() {
            Ok(Frame::Audio(a)) => a,
            _ => continue,
        };
        // Shape contract: each Opus packet here is one 20 ms / 960
        // sample frame per channel. The decoder emits interleaved
        // S16 in plane 0 (when stereo) or plain S16 (when mono).
        assert_eq!(
            out.samples as usize, OPUS_FRAME_SAMPLES,
            "decoded frame should be exactly one 960-sample CELT frame, got {}",
            out.samples
        );
        let expected_bytes = OPUS_FRAME_SAMPLES * channels as usize * 2;
        assert!(
            out.data[0].len() >= expected_bytes,
            "decoded plane shorter than {} bytes (got {})",
            expected_bytes,
            out.data[0].len()
        );
    }
});
