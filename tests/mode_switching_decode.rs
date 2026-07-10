//! §4.5 mode-switching validation on a real Hybrid↔CELT stream
//! (RFC 6716 §4.5.1 / §4.5.2 / §4.5.3): the `mode-switching` fixture
//! switches from Hybrid (low-frequency tone) to CELT-only (full-band
//! content) mid-stream, and the black-box encoder emitted a §4.5.1
//! redundant CELT frame to bridge the transition. Decoding it
//! exercises the redundancy flag/position/size decode, the §4.5.1.3
//! buffer reduction, the 5 ms redundant frame's own decode, the
//! §4.5.1.4 cross-lap, and the §4.5.2 reset placement (the
//! end-position redundant frame takes the CELT reset and its warmed
//! state carries into the first CELT-only frames — a wrong placement
//! desynchronizes the inter-coded energy of the whole CELT segment).
//!
//! The comparison is segment-aware: the Hybrid segment's low band
//! rides the crate's non-normative SILK resampler (different group
//! delay than the reference decoder's), so it is validated by energy
//! parity; the CELT-only segment and the transition window are fully
//! normative paths and must match the reference decode at waveform
//! level. The tiny Ogg walker mirrors `celt_fixture_decode.rs`.

use oxideav_opus::{FrameDecodeStatus, OpusDecoder};

const FIXTURE: &[u8] = include_bytes!("fixtures/mode-switching.opus");
const EXPECTED: &[u8] = include_bytes!("fixtures/mode-switching.expected.wav");

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

/// The stream decodes end-to-end (both modes report real-audio
/// statuses), the Hybrid segment holds energy parity, and the
/// normative transition window + CELT-only segment match the
/// reference decode at waveform level (they are reference-exact at
/// ~107-109 dB; the 60 dB thresholds leave margin while still being
/// unreachable by a structurally wrong transition).
#[test]
fn hybrid_to_celt_switch_with_redundancy_matches_reference() {
    let mut packets = ogg_packets(FIXTURE);
    let pre_skip = u16::from_le_bytes([packets[0][10], packets[0][11]]) as usize;
    packets.drain(..2);

    let mut dec = OpusDecoder::new();
    let mut pcm: Vec<i16> = Vec::new();
    let mut hybrid_frames = 0usize;
    let mut celt_frames = 0usize;
    for (i, pk) in packets.iter().enumerate() {
        let out = dec
            .decode_packet(pk)
            .unwrap_or_else(|e| panic!("packet {i}: {e:?}"));
        assert_eq!(out.channels, 1);
        for fo in &out.frame_outcomes {
            match fo.status {
                FrameDecodeStatus::HybridDecoded => hybrid_frames += 1,
                FrameDecodeStatus::CeltDecoded | FrameDecodeStatus::CeltSilence => celt_frames += 1,
                other => panic!("packet {i}: unexpected status {other:?}"),
            }
        }
        pcm.extend_from_slice(&out.pcm);
    }
    assert!(hybrid_frames > 0, "fixture must exercise the Hybrid mode");
    assert!(celt_frames > 0, "fixture must exercise the CELT-only mode");

    let expected = wav_pcm_payload(EXPECTED);
    let got = &pcm[pre_skip..];
    let n = expected.len().min(got.len());

    // Segment map (from the fixture's generation recipe): tone →
    // Hybrid for the first 0.7 s, full-band content → CELT after.
    let switch = 33_600usize; // 0.7 s at 48 kHz
    assert!(n > switch + 10_000, "decoded stream too short: {n}");

    // Hybrid segment: energy parity (the non-normative SILK resampler
    // precludes waveform alignment in the low band).
    let (mut sw, mut sg) = (0.0f64, 0.0f64);
    for i in 0..switch {
        sw += f64::from(expected[i]) * f64::from(expected[i]);
        sg += f64::from(got[i]) * f64::from(got[i]);
    }
    let rms_ratio = (sg / sw).sqrt();
    assert!(
        (0.8..1.25).contains(&rms_ratio),
        "hybrid-segment energy parity violated: rms ratio {rms_ratio:.3}"
    );

    // Transition window (contains the §4.5.1.4 redundant-frame
    // cross-lap and the first CELT-only frames running on the
    // redundant frame's warmed state).
    let trans = snr_db(
        &expected[switch..switch + 6_400],
        &got[switch..switch + 6_400],
    );
    assert!(
        trans > 60.0,
        "transition window SNR {trans:.1} dB below threshold"
    );

    // CELT-only segment: reference-exact decode.
    let celt = snr_db(&expected[switch + 6_400..n], &got[switch + 6_400..n]);
    assert!(celt > 60.0, "CELT segment SNR {celt:.1} dB below threshold");
}
