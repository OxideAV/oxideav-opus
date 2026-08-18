//! §2.1.9 DTX decode gates against reference material: the shipped
//! `dtx-refenc-voice-silence.bits` capture was produced by the
//! RFC 6716 §A reference listing's demo program (opaque invocation,
//! `-dtx`, voip 16 kHz mono 16 kb/s) over 1 s voice | 3 s digital
//! silence | 1 s voice | 3 s low-level noise, and the two `.pcm`
//! windows are that program's own decoder output (48 kHz s16le) for
//! the pre-DTX voice region and the steady post-resume region.
//!
//! Capture framing: per packet, a big-endian u32 payload length, a
//! big-endian u32 range-coder word (ignored here), then the payload —
//! the §A demo program's capture format.
//!
//! Gates: the stream's shape (markers only inside the DTX runs, one
//! §2.1.9 refresh cadence), exact per-packet sample counts, BIT-EXACT
//! agreement with the reference decode up to the first suppression,
//! near-silence across the DTX run (the §4.4 hold decays where the
//! reference generates its own comfort floor — both at the noise
//! floor), and steady re-convergence after resume.

use oxideav_opus::decoder::{FrameDecodeStatus, OpusDecoder};

const CAPTURE: &[u8] = include_bytes!("fixtures/dtx-refenc-voice-silence.bits");
const PRE_EXPECTED: &[u8] = include_bytes!("fixtures/dtx-refenc.pre.expected48.pcm");
const TAIL_EXPECTED: &[u8] = include_bytes!("fixtures/dtx-refenc.tail.expected48.pcm");

/// Reference-decode window pinned by `dtx-refenc.tail.expected48.pcm`
/// (packets 215..250 — one §2.1.9 refresh period into the second
/// voice segment, past the resume transient).
const TAIL_START_PACKET: usize = 215;

fn capture_packets() -> Vec<Vec<u8>> {
    let mut off = 0usize;
    let mut out = Vec::new();
    while off + 8 <= CAPTURE.len() {
        let len = u32::from_be_bytes(CAPTURE[off..off + 4].try_into().unwrap()) as usize;
        off += 8;
        out.push(CAPTURE[off..off + len].to_vec());
        off += len;
    }
    assert_eq!(off, CAPTURE.len(), "trailing bytes in the capture");
    out
}

fn pcm_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn reference_dtx_stream_decodes_with_pinned_agreement() {
    let packets = capture_packets();
    assert_eq!(packets.len(), 401, "fixture shape drifted");

    // Stream shape: a long marker population, all 1-byte with the
    // WB 20 ms SILK TOC, none before the first suppression.
    let markers: Vec<usize> = (0..packets.len())
        .filter(|&k| packets[k].len() == 1)
        .collect();
    assert!(markers.len() > 250, "marker count {}", markers.len());
    for &k in &markers {
        assert_eq!(packets[k], vec![0x48u8], "marker TOC at {k}");
    }
    let first_marker = markers[0];
    assert_eq!(first_marker, 56, "fixture shape drifted");

    // Decode the whole capture.
    let mut dec = OpusDecoder::new();
    let mut pcm: Vec<i16> = Vec::new();
    for (k, p) in packets.iter().enumerate() {
        let out = dec.decode_packet(p).expect("decode");
        assert_eq!(out.channels, 1, "packet {k}");
        assert_eq!(out.samples_per_channel(), 960, "packet {k}");
        let status = out.frame_outcomes[0].status;
        if p.len() == 1 {
            assert_eq!(status, FrameDecodeStatus::DtxOrLost, "packet {k}");
        } else {
            assert_eq!(status, FrameDecodeStatus::SilkParamsDecoded, "packet {k}");
        }
        pcm.extend_from_slice(&out.pcm);
    }

    // Gate 1: BIT-EXACT against the reference decode up to the first
    // suppression (coded SILK decode is exact; nothing non-normative
    // has happened yet).
    let pre = pcm_i16(PRE_EXPECTED);
    assert_eq!(pre.len(), first_marker * 960);
    assert_eq!(&pcm[..pre.len()], &pre[..], "pre-DTX region not bit-exact");

    // Gate 2: the DTX run sits at the silence floor (the §4.4 hold
    // decays to silence; the reference decoder's own comfort floor is
    // a few LSB — both are perceptual silence).
    let deep = 80 * 960..120 * 960;
    let peak = pcm[deep].iter().map(|&s| i32::from(s).abs()).max().unwrap();
    assert!(peak <= 64, "DTX region not at the silence floor: {peak}");

    // Gate 3: steady re-convergence after the resume — the residual
    // gap is only the reference decoder's non-normative
    // post-concealment smoothing propagated through LTP, and it has
    // died down by one refresh period into the second voice segment.
    let tail = pcm_i16(TAIL_EXPECTED);
    let ours = &pcm[TAIL_START_PACKET * 960..TAIL_START_PACKET * 960 + tail.len()];
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    let mut maxdiff = 0i32;
    for (&a, &b) in ours.iter().zip(&tail) {
        sig += f64::from(b) * f64::from(b);
        let d = i32::from(a) - i32::from(b);
        err += f64::from(d) * f64::from(d);
        maxdiff = maxdiff.max(d.abs());
    }
    let snr = 10.0 * (sig / err.max(1e-9)).log10();
    assert!(snr >= 45.0, "post-resume SNR {snr:.1} dB below the gate");
    assert!(maxdiff <= 256, "post-resume max |diff| {maxdiff}");
}
