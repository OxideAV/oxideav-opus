//! Reduced-output-rate decode gates (RFC 6716 §4.2.9 / §4.3.7): the
//! `OpusDecoder::with_output_rate` surface — "the sample rate desired
//! by the application" — validated whole-stream against the §A
//! reference listing decoder's own reduced-rate decodes.
//!
//! Every `*.expected<rate>.pcm` fixture is the raw s16le interleaved
//! output of the reference listing's demo program (opaque invocation
//! of the hash-verified, RFC 8251-patched build) decoding the SAME
//! `.opus` stream at that output rate. The gates pin:
//!
//! * **SILK-only** streams **bit-exactly** at every rate — the §4.2.7.9
//!   fixed-point core and the full decoder-side §4.2.9 resampler
//!   matrix (pass-through, fractional upsampling, and the AR2 +
//!   decimating-FIR downsampling chains) reproduce the reference
//!   sample-for-sample;
//! * **CELT-only** streams at the float-noise floor — the reduced-rate
//!   structure (spectrum zeroed above the output Nyquist before the
//!   inverse MDCT, de-emphasis decimation at phase 0) matches the
//!   reference construction, so only f64-vs-f32 arithmetic noise
//!   remains;
//! * **Hybrid** streams and the **mode-switching** stream (§4.5
//!   transitions, redundant-frame cross-laps) at the float-noise floor
//!   on the reduced-rate timeline.
//!
//! The Ogg page walker mirrors `silk_fixture_decode.rs` — test-only
//! fixture-loading scaffolding, not crate surface.

use oxideav_opus::OpusDecoder;

/// Recover the raw Opus packets from an Ogg-Opus byte stream.
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

/// Raw s16le bytes → samples.
fn pcm_from_bytes(raw: &[u8]) -> Vec<i16> {
    raw.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Signal-to-noise ratio (dB) of `got` against `want` over the
/// overlapping prefix; `f64::INFINITY` on an exact match.
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

/// Largest absolute per-sample difference over the overlapping prefix.
fn max_abs_diff(want: &[i16], got: &[i16]) -> i32 {
    want.iter()
        .zip(got.iter())
        .map(|(&w, &g)| (i32::from(w) - i32::from(g)).abs())
        .max()
        .unwrap_or(0)
}

/// Decode a whole Ogg-Opus stream at `rate` Hz, returning the
/// interleaved PCM (no pre-skip trim — the reference decodes compare
/// from sample zero) and asserting exact per-packet sample accounting.
fn decode_at(stream: &[u8], rate: u32) -> (Vec<i16>, usize) {
    let packets = ogg_packets(stream);
    assert!(packets.len() > 2);
    let mut dec = OpusDecoder::with_output_rate(rate).expect("supported rate");
    assert_eq!(dec.output_rate_hz(), rate);
    let mut pcm: Vec<i16> = Vec::new();
    let mut channels = 0usize;
    for (i, pk) in packets[2..].iter().enumerate() {
        let out = dec
            .decode_packet(pk)
            .unwrap_or_else(|e| panic!("packet {i} failed at {rate} Hz: {e:?}"));
        assert_eq!(out.sample_rate_hz, rate);
        channels = out.channels as usize;
        pcm.extend_from_slice(&out.pcm);
    }
    (pcm, channels)
}

/// Run one fixture gate: decode at `rate`, compare against the
/// reference listing's reduced-rate decode, require `min_db` (and
/// bit-exactness when `min_db` is infinite).
fn gate(stream: &[u8], expected_raw: &[u8], rate: u32, channels: usize, min_db: f64) {
    let expected = pcm_from_bytes(expected_raw);
    let (pcm, ch) = decode_at(stream, rate);
    assert_eq!(ch, channels);
    assert_eq!(
        pcm.len(),
        expected.len(),
        "sample-count mismatch at {rate} Hz"
    );
    let snr = snr_db(&expected, &pcm);
    eprintln!("reduced-rate gate: {rate} Hz, {channels} ch, snr {snr:.1} dB");
    if min_db.is_infinite() {
        assert_eq!(
            max_abs_diff(&expected, &pcm),
            0,
            "expected a bit-exact reduced-rate SILK decode at {rate} Hz (snr {snr:.1} dB)"
        );
    } else {
        assert!(
            snr >= min_db,
            "reduced-rate decode at {rate} Hz: {snr:.1} dB < {min_db} dB floor"
        );
    }
}

// ───────────────────────── SILK-only ─────────────────────────

#[test]
fn silk_nb_mono_at_8k_is_bit_exact() {
    // NB internal 8 kHz → 8 kHz output: the pass-through path (with
    // the decoder delay matrix's 4-sample compensation).
    gate(
        include_bytes!("fixtures/silk-nb-mono-16kbps.opus"),
        include_bytes!("fixtures/silk-nb-mono-16kbps.expected8000.pcm"),
        8_000,
        1,
        f64::INFINITY,
    );
}

#[test]
fn silk_nb_mono_at_24k_is_bit_exact() {
    // NB 8 kHz → 24 kHz: the allpass + fractional-FIR upsampling path
    // at a non-48 kHz target.
    gate(
        include_bytes!("fixtures/silk-nb-mono-16kbps.opus"),
        include_bytes!("fixtures/silk-nb-mono-16kbps.expected24000.pcm"),
        24_000,
        1,
        f64::INFINITY,
    );
}

#[test]
fn silk_mb_60ms_mono_at_8k_is_bit_exact() {
    // MB 12 kHz → 8 kHz: the 2:3 AR2 + 18-tap decimating FIR.
    gate(
        include_bytes!("fixtures/silk-mb-60ms-mono-20kbps.opus"),
        include_bytes!("fixtures/silk-mb-60ms-mono-20kbps.expected8000.pcm"),
        8_000,
        1,
        f64::INFINITY,
    );
}

#[test]
fn silk_wb_stereo_at_8k_is_bit_exact() {
    // WB 16 kHz → 8 kHz stereo: the 1:2 AR2 + 24-tap symmetric FIR on
    // both unmixed channels.
    gate(
        include_bytes!("fixtures/silk-wb-stereo-20kbps.opus"),
        include_bytes!("fixtures/silk-wb-stereo-20kbps.expected8000.pcm"),
        8_000,
        2,
        f64::INFINITY,
    );
}

// ───────────────────────── CELT-only ─────────────────────────

#[test]
fn celt_low_latency_at_12k_hits_the_float_floor() {
    // 2.5 ms CELT frames at a ÷4 decimation: WB coded spectrum capped
    // at the 6 kHz output Nyquist, per-frame decimation phase 0.
    gate(
        include_bytes!("fixtures/celt-2.5ms-low-latency.opus"),
        include_bytes!("fixtures/celt-2.5ms-low-latency.expected12000.pcm"),
        12_000,
        2,
        80.0,
    );
}

#[test]
fn celt_fb_stereo_at_24k_hits_the_float_floor() {
    // 20 ms FB stereo CELT at ÷2: the coded FB spectrum is bounded at
    // N/2 bins before the inverse MDCT.
    gate(
        include_bytes!("fixtures/celt-fb-stereo-128kbps.opus"),
        include_bytes!("fixtures/celt-fb-stereo-128kbps.expected24000.pcm"),
        24_000,
        2,
        95.0,
    );
}

// ───────────────────────── Hybrid + transitions ─────────────────────────

#[test]
fn hybrid_fb_mono_at_16k_keeps_the_reference_timeline() {
    // Hybrid at 16 kHz output: WB SILK passes through (copy path)
    // while the CELT bands land above the 8 kHz Nyquist and vanish —
    // the summed timeline must still match the reference decode.
    gate(
        include_bytes!("fixtures/hybrid-fb-mono-28kbps.opus"),
        include_bytes!("fixtures/hybrid-fb-mono-28kbps.expected16000.pcm"),
        16_000,
        1,
        65.0,
    );
}

#[test]
fn hybrid_fb_mono_at_24k_hits_the_float_floor() {
    // Hybrid at 24 kHz: WB SILK through the fractional upsampler plus
    // the CELT layer's ÷2 decimation, summed per §4.4.
    gate(
        include_bytes!("fixtures/hybrid-fb-mono-28kbps.opus"),
        include_bytes!("fixtures/hybrid-fb-mono-28kbps.expected24000.pcm"),
        24_000,
        1,
        65.0,
    );
}

#[test]
fn mode_switching_at_24k_hits_the_float_floor() {
    // The §4.5 transition machinery (redundant 5 ms CELT frames, both
    // cross-lap placements, deferred resets) on the ÷2 timeline.
    gate(
        include_bytes!("fixtures/mode-switching.opus"),
        include_bytes!("fixtures/mode-switching.expected24000.pcm"),
        24_000,
        1,
        95.0,
    );
}

#[test]
fn mode_switching_at_8k_stays_coherent() {
    // ÷6 decimation across SILK→Hybrid→CELT switches: every layer of
    // the stream collapses onto the 4 kHz band.
    gate(
        include_bytes!("fixtures/mode-switching.opus"),
        include_bytes!("fixtures/mode-switching.expected8000.pcm"),
        8_000,
        1,
        95.0,
    );
}

// ───────────────────────── surface checks ─────────────────────────

#[test]
fn unsupported_rates_are_rejected() {
    for rate in [0u32, 11_025, 22_050, 44_100, 96_000] {
        assert!(OpusDecoder::with_output_rate(rate).is_none());
    }
}

#[test]
fn reset_keeps_the_configured_rate() {
    let mut dec = OpusDecoder::with_output_rate(16_000).expect("rate");
    dec.reset();
    assert_eq!(dec.output_rate_hz(), 16_000);
}

#[test]
fn conceal_loss_runs_at_the_configured_rate() {
    // Decode one packet at 12 kHz, then conceal a loss: the concealed
    // frame must match the last packet's duration at the reduced rate.
    let stream = include_bytes!("fixtures/silk-nb-mono-16kbps.opus");
    let packets = ogg_packets(stream);
    let mut dec = OpusDecoder::with_output_rate(12_000).expect("rate");
    let first = dec.decode_packet(&packets[2]).expect("decode");
    let per_frame = first.pcm.len() / first.frame_outcomes.len();
    let concealed = dec.conceal_loss();
    assert_eq!(concealed.sample_rate_hz, 12_000);
    assert_eq!(concealed.pcm.len(), per_frame * first.channels as usize);
}

// ───────────────────────── multistream ─────────────────────────

#[test]
fn multistream_51_at_24k_assembles_on_the_reduced_timeline() {
    use oxideav_opus::multistream::MultistreamDecoder;
    use oxideav_opus::opus_head::OpusHead;

    let stream = include_bytes!("fixtures/multistream-5.1.opus");
    let packets = ogg_packets(stream);
    let head = OpusHead::parse(&packets[0]).expect("OpusHead");
    let mut dec =
        MultistreamDecoder::from_head_with_output_rate(&head, 24_000).expect("supported rate");
    assert_eq!(dec.output_rate_hz(), 24_000);

    // Reference timeline: the 48 kHz decode of the same stream.
    let mut dec48 = MultistreamDecoder::from_head(&head);
    let mut samples_24k = 0usize;
    let mut samples_48k = 0usize;
    let mut energy = 0f64;
    for pk in &packets[2..] {
        let out = dec.decode_packet(pk).expect("24 kHz decode");
        assert_eq!(out.sample_rate_hz, 24_000);
        assert_eq!(out.channels, 6);
        samples_24k += out.samples_per_channel;
        for &s in &out.pcm {
            energy += f64::from(s) * f64::from(s);
        }
        samples_48k += dec48
            .decode_packet(pk)
            .expect("48 kHz decode")
            .samples_per_channel;
    }
    // Exact ÷2 sample accounting against the 48 kHz decode, and real
    // audio on the reduced timeline.
    assert_eq!(samples_24k * 2, samples_48k);
    assert!(energy > 0.0);
}

#[test]
fn multistream_n1_at_12k_matches_the_plain_decoder() {
    use oxideav_opus::multistream::MultistreamDecoder;
    use oxideav_opus::opus_head::ChannelMappingTable;

    // A family-0 style single-stream mono table: the N=1 multistream
    // decode at a reduced rate must be byte-identical to a plain
    // reduced-rate decoder on the same packets.
    let stream = include_bytes!("fixtures/silk-nb-mono-16kbps.opus");
    let packets = ogg_packets(stream);
    let mapping = ChannelMappingTable {
        stream_count: 1,
        coupled_count: 0,
        mapping: vec![0],
    };
    let mut ms = MultistreamDecoder::with_output_rate(mapping, 12_000).expect("rate");
    let mut plain = OpusDecoder::with_output_rate(12_000).expect("rate");
    for pk in &packets[2..] {
        let a = ms.decode_packet(pk).expect("multistream");
        let b = plain.decode_packet(pk).expect("plain");
        assert_eq!(a.sample_rate_hz, 12_000);
        assert_eq!(a.pcm, b.pcm);
    }
}

// ───────────────────────── FEC at reduced rates ─────────────────────────

#[test]
fn fec_recovery_runs_on_the_reduced_timeline() {
    use oxideav_opus::decoder::FecDecodeStatus;

    // Walk the FEC fixture at 12 kHz, simulating a loss before every
    // packet: at least one packet must recover real LBRR audio, every
    // recovery must land the exact reduced-rate sample count, and the
    // stream must keep decoding cleanly afterwards.
    let stream = include_bytes!("fixtures/fec-on.opus");
    let packets = ogg_packets(stream);
    let mut dec = OpusDecoder::with_output_rate(12_000).expect("rate");
    let mut recovered = 0usize;
    let mut recovered_energy = 0f64;
    for pk in &packets[2..] {
        let fec = dec.decode_packet_fec(pk).expect("fec parse");
        assert_eq!(fec.sample_rate_hz, 12_000);
        if fec.status == FecDecodeStatus::Recovered {
            recovered += 1;
            let routing_samples = fec.pcm.len() / fec.channels.max(1) as usize;
            // 20 ms at 12 kHz = 240 per channel (fixture is 20 ms).
            assert_eq!(routing_samples, 240);
            for &s in &fec.pcm {
                recovered_energy += f64::from(s) * f64::from(s);
            }
        }
        let out = dec.decode_packet(pk).expect("regular decode");
        assert_eq!(out.sample_rate_hz, 12_000);
    }
    assert!(recovered > 0, "FEC fixture must recover at least one frame");
    assert!(recovered_energy > 0.0, "recovered audio must carry signal");
}
