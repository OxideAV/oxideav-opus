//! Registry-level payload-magic resolution — the `register(ctx)`
//! entry point's `OpusHead` claim (RFC 7845 §5.1 item 1).
//!
//! The Opus identification header opens with the fixed 8-octet
//! signature `"OpusHead"`; carriage formats that have no codec tag
//! (an Ogg logical stream's first packet is the canonical case)
//! resolve the codec from that payload prefix. These tests pin the
//! positive resolution and the refusals: the RFC 7845 §5.2 comment
//! header (`"OpusTags"`) and every proper truncation of the magic
//! must NOT resolve to `opus`.

use oxideav_core::stream::CodecResolver;
use oxideav_core::{CodecId, RuntimeContext};
use oxideav_opus::opus_head::OPUS_HEAD_MAGIC;

fn registered_context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_opus::register(&mut ctx);
    ctx
}

#[test]
fn opus_head_prefix_resolves_to_opus() {
    let ctx = registered_context();
    // A realistic §5.1 identification header: magic, version 1,
    // 2 channels, pre-skip 312, input rate 48 kHz, gain 0, family 0.
    let mut head = Vec::new();
    head.extend_from_slice(OPUS_HEAD_MAGIC);
    head.extend_from_slice(&[1, 2]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    assert_eq!(
        ctx.codecs.resolve_payload_magic_ref(&head),
        Some(&CodecId::new("opus"))
    );
}

#[test]
fn exact_length_magic_resolves() {
    let ctx = registered_context();
    // A packet that is nothing but the 8-byte magic still matches —
    // prefix matching includes the exact-length case.
    assert_eq!(
        ctx.codecs.resolve_payload_magic_ref(b"OpusHead"),
        Some(&CodecId::new("opus"))
    );
}

#[test]
fn resolver_trait_surface_agrees() {
    let ctx = registered_context();
    // The dyn-facing CodecResolver path must agree with the inherent
    // method (it is what container crates actually call).
    let resolver: &dyn CodecResolver = &ctx.codecs;
    assert_eq!(
        resolver.resolve_payload_magic(b"OpusHead\x01\x02\x38\x01"),
        Some(CodecId::new("opus"))
    );
}

#[test]
fn opus_tags_comment_header_does_not_resolve() {
    let ctx = registered_context();
    // RFC 7845 §5.2: the second header packet opens with "OpusTags".
    // It shares 4 leading octets with "OpusHead" but is NOT an
    // identification header and must not resolve.
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&9u32.to_le_bytes());
    tags.extend_from_slice(b"oxideav 0");
    tags.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(ctx.codecs.resolve_payload_magic_ref(&tags), None);
}

#[test]
fn truncations_of_the_magic_do_not_resolve() {
    let ctx = registered_context();
    // Every proper prefix of the magic, from 7 bytes down to empty:
    // a payload SHORTER than the claimed prefix carries insufficient
    // evidence and must not match.
    for len in (0..OPUS_HEAD_MAGIC.len()).rev() {
        let truncated = &OPUS_HEAD_MAGIC[..len];
        assert_eq!(
            ctx.codecs.resolve_payload_magic_ref(truncated),
            None,
            "truncation to {len} bytes must not resolve"
        );
    }
}

#[test]
fn corrupted_final_octet_does_not_resolve() {
    let ctx = registered_context();
    let mut wrong = *OPUS_HEAD_MAGIC;
    wrong[7] ^= 0x20; // "OpusHeaD"
    assert_eq!(ctx.codecs.resolve_payload_magic_ref(wrong.as_slice()), None);
}

#[test]
fn unrelated_payloads_do_not_resolve() {
    let ctx = registered_context();
    for payload in [
        b"\x01vorbis\x00".as_slice(),
        b"RIFF\x00\x00\x00\x00".as_slice(),
        b"\x00\x00\x00\x00\x00\x00\x00\x00".as_slice(),
    ] {
        assert_eq!(ctx.codecs.resolve_payload_magic_ref(payload), None);
    }
}

// ───────────────────── factory resolution ─────────────────────
//
// Round 450: the registration is no longer tag-only — the registry
// carries working decoder/encoder factories. These tests resolve the
// codec THROUGH a `RuntimeContext` registry (never by direct
// construction) and run real fixture audio through the resolved
// engines, pinning the dual-API convention: registry resolution and
// the direct `make_decoder` / `make_encoder` calls construct the same
// implementation.

use oxideav_core::{CodecParameters, Frame, Packet, Rational, TimeBase};

/// Recover the raw Opus packets from an Ogg-Opus byte stream (RFC 3533
/// page walk; packets end at every lacing value < 255). Test-only
/// fixture-loading scaffolding, mirroring `silk_fixture_decode.rs`.
fn ogg_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut off = 0usize;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    while off + 27 <= data.len() {
        assert_eq!(&data[off..off + 4], b"OggS", "lost Ogg page sync at {off}");
        let nseg = data[off + 26] as usize;
        let seg_table_end = off + 27 + nseg;
        assert!(seg_table_end <= data.len(), "truncated Ogg lacing table");
        let segtab = &data[off + 27..seg_table_end];
        let mut p = seg_table_end;
        for &s in segtab {
            let seg_end = p + s as usize;
            assert!(seg_end <= data.len(), "truncated Ogg page body");
            cur.extend_from_slice(&data[p..seg_end]);
            p = seg_end;
            if s < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        off = p;
    }
    packets
}

/// The s16le PCM payload of a RIFF/WAVE file (the `data` chunk body).
fn wav_pcm_payload(wav: &[u8]) -> Vec<i16> {
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    let mut off = 12usize;
    while off + 8 <= wav.len() {
        let id = &wav[off..off + 4];
        let len =
            u32::from_le_bytes([wav[off + 4], wav[off + 5], wav[off + 6], wav[off + 7]]) as usize;
        if id == b"data" {
            let body = &wav[off + 8..off + 8 + len];
            return body
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
        }
        off += 8 + len + (len & 1);
    }
    panic!("no data chunk in wav");
}

/// Signal-to-noise ratio (dB) of `got` against `want` over the
/// overlapping prefix.
fn snr_db(want: &[i16], got: &[i16]) -> f64 {
    let n = want.len().min(got.len());
    assert!(n > 0);
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    for i in 0..n {
        let w = f64::from(want[i]);
        let g = f64::from(got[i]);
        sig += w * w;
        err += (w - g) * (w - g);
    }
    if err == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (sig / err).log10()
}

/// Wrap raw Opus packet bytes as a framework packet.
fn core_packet(bytes: &[u8]) -> Packet {
    Packet::new(0, TimeBase(Rational::new(1, 48_000)), bytes.to_vec())
}

/// Decode one Ogg-Opus fixture through a registry-RESOLVED decoder
/// (extradata = the stream's OpusHead packet, so the adapter owns the
/// §5.1 pre-skip and output gain) and return `(interleaved pcm,
/// channels)`.
fn registry_decode_fixture(stream: &[u8]) -> (Vec<i16>, usize) {
    let ctx = registered_context();
    assert!(ctx.codecs.has_decoder(&CodecId::new("opus")));

    let packets = ogg_packets(stream);
    assert!(packets.len() > 2, "fixture must have headers + audio");
    let mut params = CodecParameters::audio(CodecId::new("opus"));
    params.extradata = packets[0].clone();
    let mut dec = ctx.codecs.first_decoder(&params).expect("resolve decoder");

    let mut pcm: Vec<i16> = Vec::new();
    let mut channels = 0usize;
    for pk in &packets[2..] {
        dec.send_packet(&core_packet(pk)).expect("decode");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(f)) => {
                    let ch = f.data[0].len() / 2 / f.samples.max(1) as usize;
                    channels = ch;
                    pcm.extend(
                        f.data[0]
                            .chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]])),
                    );
                }
                Ok(_) => panic!("expected audio frames"),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_frame: {e}"),
            }
        }
    }
    (pcm, channels)
}

#[test]
fn registry_resolved_decoder_reproduces_the_silk_reference_decode() {
    // The NB mono SILK fixture decodes bit-exactly against its shipped
    // reference decode through the DIRECT decoder
    // (tests/silk_reference_waveform.rs); the registry-resolved path
    // must reproduce the same waveform, with the RFC 7845 §5.1
    // pre-skip applied by the adapter itself.
    let stream = include_bytes!("fixtures/silk-nb-mono-16kbps.opus");
    let expected = wav_pcm_payload(include_bytes!("fixtures/silk-nb-mono-16kbps.expected.wav"));
    let (pcm, channels) = registry_decode_fixture(stream);
    assert_eq!(channels, 1);
    let snr = snr_db(&expected, &pcm);
    assert!(
        snr >= 100.0,
        "registry-resolved decode must sit at the reference floor, got {snr:.1} dB"
    );
}

#[test]
fn registry_resolved_decoder_handles_stereo_and_celt_streams() {
    // WB stereo SILK (bit-exact reference agreement) and FB stereo
    // CELT (float-noise floor ~88 dB direct) both decode through the
    // registry-resolved adapter at their established gates.
    let silk = include_bytes!("fixtures/silk-wb-stereo-20kbps.opus");
    let silk_ref = wav_pcm_payload(include_bytes!(
        "fixtures/silk-wb-stereo-20kbps.expected.wav"
    ));
    let (pcm, channels) = registry_decode_fixture(silk);
    assert_eq!(channels, 2);
    let snr = snr_db(&silk_ref, &pcm);
    assert!(snr >= 100.0, "stereo SILK registry decode: {snr:.1} dB");

    let celt = include_bytes!("fixtures/celt-fb-stereo-128kbps.opus");
    let celt_ref = wav_pcm_payload(include_bytes!(
        "fixtures/celt-fb-stereo-128kbps.expected.wav"
    ));
    let (pcm, channels) = registry_decode_fixture(celt);
    assert_eq!(channels, 2);
    let snr = snr_db(&celt_ref, &pcm);
    assert!(snr >= 60.0, "CELT registry decode: {snr:.1} dB");
}

#[test]
fn registry_resolved_decoder_assembles_multistream_51() {
    // The 5.1 fixture's OpusHead (mapping family 1, 4 streams / 2
    // coupled) routes the registry decoder through the multistream
    // assembly; the output must match the shipped reference decode.
    let stream = include_bytes!("fixtures/multistream-5.1.opus");
    let expected = wav_pcm_payload(include_bytes!("fixtures/multistream-5.1.expected.wav"));
    let (pcm, channels) = registry_decode_fixture(stream);
    assert_eq!(channels, 6);
    let snr = snr_db(&expected, &pcm);
    assert!(snr >= 60.0, "5.1 registry decode: {snr:.1} dB");
}

#[test]
fn registry_resolved_encoder_roundtrips_through_resolved_decoder() {
    let ctx = registered_context();
    assert!(ctx.codecs.has_encoder(&CodecId::new("opus")));

    let mut params = CodecParameters::audio(CodecId::new("opus"));
    params.channels = Some(2);
    params.bit_rate = Some(96_000);
    let mut enc = ctx.codecs.first_encoder(&params).expect("resolve encoder");

    // 200 ms of a stereo tone pair.
    let samples = 9_600usize;
    let mut bytes = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        let t = i as f64 / 48_000.0;
        let l = (8_000.0 * (std::f64::consts::TAU * 440.0 * t).sin()) as i16;
        let r = (8_000.0 * (std::f64::consts::TAU * 660.0 * t).sin()) as i16;
        bytes.extend_from_slice(&l.to_le_bytes());
        bytes.extend_from_slice(&r.to_le_bytes());
    }
    enc.send_frame(&Frame::Audio(oxideav_core::AudioFrame {
        samples: samples as u32,
        pts: Some(0),
        data: vec![bytes],
    }))
    .expect("send_frame");
    enc.flush().expect("flush");

    // The encoder's output params carry a composed OpusHead; hand it
    // to a freshly resolved decoder exactly like a container would.
    let mut dec_params = CodecParameters::audio(CodecId::new("opus"));
    dec_params.extradata = enc.output_params().extradata.clone();
    let mut dec = ctx.codecs.first_decoder(&dec_params).expect("decoder");

    let mut decoded = 0usize;
    let mut energy = 0.0f64;
    loop {
        let packet = match enc.receive_packet() {
            Ok(p) => p,
            Err(oxideav_core::Error::Eof) => break,
            Err(oxideav_core::Error::NeedMore) => continue,
            Err(e) => panic!("receive_packet: {e}"),
        };
        dec.send_packet(&packet).expect("decode");
        while let Ok(Frame::Audio(f)) = dec.receive_frame() {
            decoded += f.samples as usize;
            for b in f.data[0].chunks_exact(2) {
                let v = f64::from(i16::from_le_bytes([b[0], b[1]]));
                energy += v * v;
            }
        }
    }
    // 10 packets × 960 samples minus the declared 120-sample pre-skip.
    assert_eq!(decoded, samples - 120);
    assert!(energy > 0.0);
}

#[test]
fn registry_resolved_decoder_honours_a_reduced_sample_rate() {
    // `CodecParameters::sample_rate = 8000` must route the resolved
    // decoder through the reduced-rate decode surface. The NB SILK
    // fixture decodes BIT-EXACTLY against the reference listing
    // decoder's own 8 kHz decode (shipped fixture), so the adapter's
    // output must equal that reference minus the rescaled RFC 7845
    // §5.1 pre-skip (48 kHz pre-skip × 8/48, exactly divisible here).
    let stream = include_bytes!("fixtures/silk-nb-mono-16kbps.opus");
    let expected_full = {
        let raw = include_bytes!("fixtures/silk-nb-mono-16kbps.expected8000.pcm");
        raw.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<i16>>()
    };
    let ctx = registered_context();
    let packets = ogg_packets(stream);
    let head = &packets[0];
    let pre_skip_48k = u16::from_le_bytes([head[10], head[11]]) as usize;
    let pre_skip_8k = pre_skip_48k * 8_000 / 48_000;
    assert_eq!(pre_skip_8k * 48_000, pre_skip_48k * 8_000, "exact rescale");

    let mut params = CodecParameters::audio(CodecId::new("opus"));
    params.extradata = head.clone();
    params.sample_rate = Some(8_000);
    let mut dec = ctx.codecs.first_decoder(&params).expect("resolve decoder");
    let mut pcm: Vec<i16> = Vec::new();
    for pk in &packets[2..] {
        dec.send_packet(&core_packet(pk)).expect("decode");
        while let Ok(Frame::Audio(f)) = dec.receive_frame() {
            pcm.extend(
                f.data[0]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]])),
            );
        }
    }
    assert_eq!(
        pcm,
        expected_full[pre_skip_8k..],
        "bit-exact 8 kHz registry decode"
    );
}
