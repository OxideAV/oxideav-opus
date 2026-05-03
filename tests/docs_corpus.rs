//! Integration tests against the `docs/audio/opus/fixtures/` corpus.
//!
//! Each fixture under `../../docs/audio/opus/fixtures/<name>/` ships:
//! * `input.opus` — Opus packets wrapped in an Ogg container (RFC 7845).
//!   The first logical bitstream's identification packet is `OpusHead`;
//!   subsequent packets carry one or more Opus frames each.
//! * `expected.wav` — reference 16-bit signed PCM produced by FFmpeg's
//!   libopus decoder at 48 kHz.
//! * `notes.md` + `trace.txt` — implementor notes (not consumed by
//!   this driver).
//!
//! Pipeline per fixture:
//! 1. Open `input.opus` through [`oxideav_ogg::demux`]. The Ogg demuxer
//!    auto-populates `params.extradata` with the OpusHead identification
//!    packet, which the Opus decoder uses to detect channel mapping
//!    family 1 / 2 (multistream surround / ambisonics).
//! 2. Construct an Opus decoder — the multistream path kicks in
//!    automatically when extradata declares mapping family 1 (5.1
//!    surround) or 2 (ambisonics).
//! 3. Feed every Ogg packet that targets the first logical stream and
//!    accumulate the interleaved s16 PCM the decoder emits at 48 kHz.
//! 4. Parse `expected.wav` (handles both classic WAVEFORMAT and
//!    WAVEFORMATEXTENSIBLE used for ≥ 3-channel surround output).
//! 5. Compare per channel: exact match count, near-match (≤1 LSB),
//!    RMS error, max |diff|, PSNR over a 16-bit signed full scale.
//!
//! Tiering:
//! * `Tier::ReportOnly` — divergence is logged but not asserted. Opus
//!   is a lossy codec — the spec only requires the decoder's output to
//!   stay within the floating-point IMDCT envelope that libopus
//!   happens to use. Most clean-room re-implementations diverge by
//!   ±1 LSB on most samples, so every fixture is filed here.
//! * `Tier::BitExact` — reserved for future use if a fixture is
//!   shown to round-trip cleanly through both libopus and our decoder.
//!
//! The test logs `skip <name>: ...` and returns success when fixtures
//! aren't present (standalone-crate CI checkout has no `docs/`).

use std::fs;
use std::path::PathBuf;

use oxideav_core::{Error, Frame, NullCodecResolver, ReadSeek};
// `Box<dyn Decoder>` / `Box<dyn Demuxer>` resolve their trait methods
// through the dyn-vtable, so the `Decoder` / `Demuxer` traits don't
// need to be in scope at the call site here.

/// Locate `docs/audio/opus/fixtures/<name>/`. Tests run with CWD set
/// to the crate root, so we walk two levels up to reach the workspace
/// root and then into `docs/`.
fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from("../../docs/audio/opus/fixtures").join(name)
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum Tier {
    /// Must produce sample-for-sample identical PCM. Test fails on
    /// any divergence. Reserved for future use; no Opus fixture is
    /// currently expected to meet this bar (Opus is lossy).
    BitExact,
    /// Decode is permitted to diverge from the libopus reference;
    /// per-channel deltas are logged but not asserted. All fixtures
    /// start here per the brief.
    ReportOnly,
}

struct CorpusCase {
    name: &'static str,
    /// Expected channels (sanity-check vs OpusHead). None to skip.
    channels: Option<u16>,
    /// Expected output sample rate (always 48 000 — Opus only emits
    /// at 48 kHz). None to skip.
    sample_rate: Option<u32>,
    tier: Tier,
}

/// Decoded output from one fixture.
struct DecodedPcm {
    /// Interleaved s16le samples (channel-major within each frame).
    samples: Vec<i16>,
    channels: u16,
    sample_rate: u32,
}

/// Reference PCM extracted from `expected.wav`. Same shape as
/// `DecodedPcm`.
struct RefPcm {
    samples: Vec<i16>,
    channels: u16,
    sample_rate: u32,
}

/// Per-channel diff numbers + aggregate match percentage and PSNR.
struct ChannelStat {
    rms_ref: f64,
    rms_ours: f64,
    rms_err: f64, // sum-of-squares until psnr_db converts back to MSE
    exact: usize,
    near: usize, // |delta| <= 1 LSB
    total: usize,
    max_abs_err: i32,
}

impl ChannelStat {
    fn new() -> Self {
        Self {
            rms_ref: 0.0,
            rms_ours: 0.0,
            rms_err: 0.0,
            exact: 0,
            near: 0,
            total: 0,
            max_abs_err: 0,
        }
    }

    fn match_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.exact as f64 / self.total as f64 * 100.0
        }
    }

    fn near_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.near as f64 / self.total as f64 * 100.0
        }
    }

    /// PSNR over a 16-bit signed full scale (peak = 32767). Returns
    /// `f64::INFINITY` on perfect match.
    fn psnr_db(&self) -> f64 {
        if self.total == 0 || self.rms_err == 0.0 {
            return f64::INFINITY;
        }
        let mse = self.rms_err / self.total as f64;
        let peak = 32767.0_f64;
        10.0 * (peak * peak / mse).log10()
    }
}

/// Demux the fixture's input.opus into Opus packets and decode the
/// FIRST logical stream end-to-end.
fn decode_fixture_pcm(case: &CorpusCase) -> Option<DecodedPcm> {
    let dir = fixture_dir(case.name);
    let opus_path = dir.join("input.opus");
    let file = match fs::File::open(&opus_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skip {}: missing {} ({e})", case.name, opus_path.display());
            return None;
        }
    };
    let rs: Box<dyn ReadSeek> = Box::new(file);
    let mut demux = match oxideav_ogg::demux::open(rs, &NullCodecResolver) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: ogg demuxer open failed: {e}", case.name);
            return None;
        }
    };

    let streams = demux.streams();
    if streams.is_empty() {
        eprintln!("skip {}: ogg has no streams", case.name);
        return None;
    }
    let stream = streams[0].clone();
    let params = stream.params.clone();
    if params.codec_id.as_str() != "opus" {
        eprintln!(
            "skip {}: first stream is not opus (got {})",
            case.name,
            params.codec_id.as_str()
        );
        return None;
    }
    let channels = params.channels.unwrap_or(0);
    let sample_rate = params.sample_rate.unwrap_or(0);
    if channels == 0 || sample_rate == 0 {
        eprintln!(
            "skip {}: stream advertises bogus channels/rate ({channels}/{sample_rate})",
            case.name
        );
        return None;
    }
    if let Some(want) = case.channels {
        assert_eq!(
            channels, want,
            "{}: OpusHead says {channels} channels, expected {want}",
            case.name
        );
    }
    if let Some(want) = case.sample_rate {
        // Opus always *outputs* at 48 kHz regardless of the OpusHead's
        // input_sample_rate field — the demuxer follows that rule, but
        // a fixture may set its own expectation against the input rate.
        assert_eq!(
            sample_rate, want,
            "{}: OpusHead-derived sample rate {sample_rate}, expected {want}",
            case.name
        );
    }

    let mut decoder = match oxideav_opus::decoder::make_decoder(&params) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: decoder ctor failed: {e}", case.name);
            return None;
        }
    };

    let stream_index = stream.index;
    let mut samples: Vec<i16> = Vec::new();
    // Decoder emits at 48 kHz regardless of the OpusHead input rate;
    // capture the actual channel count it advertises in the first
    // audio frame in case multistream remixed the layout.
    let mut decoder_errors = 0usize;
    let mut decoder_channels = channels;
    loop {
        let pkt = match demux.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => break,
            Err(e) => {
                eprintln!(
                    "{}: demux error after {} samples: {e}",
                    case.name,
                    samples.len()
                );
                break;
            }
        };
        if pkt.stream_index != stream_index {
            continue;
        }
        if let Err(e) = decoder.send_packet(&pkt) {
            decoder_errors += 1;
            if decoder_errors <= 3 {
                eprintln!("{}: send_packet error: {e}", case.name);
            }
            continue;
        }
        match decoder.receive_frame() {
            Ok(Frame::Audio(af)) => {
                // Decoder emits interleaved s16le in af.data[0]. The
                // multistream path mixes its sub-streams up to the
                // OpusHead-declared output channel count, so we trust
                // the byte length over the cached `channels`.
                let plane = &af.data[0];
                let bytes = plane.len();
                if af.samples > 0 {
                    let derived_ch = bytes / (af.samples as usize * 2);
                    if derived_ch as u16 != decoder_channels && derived_ch > 0 {
                        decoder_channels = derived_ch as u16;
                    }
                }
                for chunk in plane.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
            }
            Ok(other) => {
                eprintln!("{}: unexpected non-audio frame: {other:?}", case.name);
            }
            Err(Error::NeedMore) => continue,
            Err(Error::Eof) => break,
            Err(e) => {
                decoder_errors += 1;
                if decoder_errors <= 3 {
                    eprintln!("{}: receive_frame error: {e}", case.name);
                }
            }
        }
    }
    if decoder_errors > 0 {
        eprintln!(
            "{}: total decoder errors: {decoder_errors} (decoded {} samples / {} per channel)",
            case.name,
            samples.len(),
            samples.len() / decoder_channels.max(1) as usize
        );
    }

    // Opus always decodes at 48 kHz per RFC 6716, even when the
    // OpusHead's input_sample_rate hint is something else (8/12/16/
    // 24 kHz for SILK). Surface that explicitly so the comparator
    // doesn't get tripped by an artificial sample-rate mismatch.
    Some(DecodedPcm {
        samples,
        channels: decoder_channels,
        sample_rate: 48_000,
    })
}

/// Parse a minimal RIFF/WAVE file: locate the `fmt ` chunk to read
/// channels + sample-rate + bits-per-sample, then return the `data`
/// chunk as interleaved s16le samples. Skips any LIST/INFO/JUNK
/// chunks between `fmt ` and `data`. Recognises both classic
/// WAVEFORMAT (tag 0x0001) and WAVEFORMATEXTENSIBLE (tag 0xFFFE)
/// carrying a PCM SubFormat GUID — required for the multistream-5.1
/// fixture which uses the EXTENSIBLE variant.
fn parse_wav(bytes: &[u8]) -> Option<RefPcm> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut i = 12usize;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut format_tag: u16 = 0;
    let mut subformat_data1: u32 = 0;
    let mut have_extensible_subformat = false;
    let mut data: Option<&[u8]> = None;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body_start = i + 8;
        let body_end = body_start + sz;
        if body_end > bytes.len() {
            break;
        }
        match id {
            b"fmt " => {
                if sz < 16 {
                    return None;
                }
                format_tag = u16::from_le_bytes([bytes[body_start], bytes[body_start + 1]]);
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                sample_rate = u32::from_le_bytes([
                    bytes[body_start + 4],
                    bytes[body_start + 5],
                    bytes[body_start + 6],
                    bytes[body_start + 7],
                ]);
                bits_per_sample =
                    u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]);
                if format_tag == 0xFFFE && sz >= 40 {
                    subformat_data1 = u32::from_le_bytes([
                        bytes[body_start + 24],
                        bytes[body_start + 25],
                        bytes[body_start + 26],
                        bytes[body_start + 27],
                    ]);
                    have_extensible_subformat = true;
                }
            }
            b"data" => {
                data = Some(&bytes[body_start..body_end]);
                break;
            }
            _ => {}
        }
        i = body_end + (sz & 1);
    }
    let data = data?;
    let effective_tag = if format_tag == 0xFFFE {
        if have_extensible_subformat {
            subformat_data1 as u16
        } else {
            1
        }
    } else {
        format_tag
    };
    if effective_tag != 1 {
        // Opus's libopus-decoded reference is always integer PCM; if a
        // fixture ever ships float we'd want to know.
        eprintln!(
            "  parse_wav: unsupported effective format tag 0x{:04x} (only PCM s16 expected)",
            effective_tag
        );
        return None;
    }
    if channels == 0 || sample_rate == 0 || bits_per_sample != 16 {
        return None;
    }
    let mut samples = Vec::with_capacity(data.len() / 2);
    for chunk in data.chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Some(RefPcm {
        samples,
        channels,
        sample_rate,
    })
}

fn read_reference(case: &CorpusCase) -> Option<RefPcm> {
    let dir = fixture_dir(case.name);
    let wav_path = dir.join("expected.wav");
    let bytes = match fs::read(&wav_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip {}: missing {} ({e})", case.name, wav_path.display());
            return None;
        }
    };
    parse_wav(&bytes)
}

/// Compute per-channel match/PSNR statistics. Compares only the
/// overlapping prefix of decoded vs reference (lossy decoders often
/// produce slightly more samples than the reference because of
/// MDCT/LPC pre-roll).
fn compare(ours: &DecodedPcm, refp: &RefPcm) -> Vec<ChannelStat> {
    let chs = ours.channels.min(refp.channels) as usize;
    if chs == 0 {
        return Vec::new();
    }
    let frames_ours = ours.samples.len() / ours.channels.max(1) as usize;
    let frames_ref = refp.samples.len() / refp.channels.max(1) as usize;
    let n = frames_ours.min(frames_ref);

    let mut stats: Vec<ChannelStat> = (0..chs).map(|_| ChannelStat::new()).collect();
    for f in 0..n {
        for (ch, s) in stats.iter_mut().enumerate() {
            let our = ours.samples[f * ours.channels as usize + ch] as i64;
            let r = refp.samples[f * refp.channels as usize + ch] as i64;
            let err = (our - r).abs();
            s.total += 1;
            if err == 0 {
                s.exact += 1;
            }
            if err <= 1 {
                s.near += 1;
            }
            if err as i32 > s.max_abs_err {
                s.max_abs_err = err as i32;
            }
            s.rms_ref += (r * r) as f64;
            s.rms_ours += (our * our) as f64;
            s.rms_err += (err * err) as f64;
        }
    }
    for s in &mut stats {
        if s.total > 0 {
            s.rms_ref = (s.rms_ref / s.total as f64).sqrt();
            s.rms_ours = (s.rms_ours / s.total as f64).sqrt();
            // s.rms_err kept as sum-of-squares for psnr_db.
        }
    }
    stats
}

/// Decode → compare → log → tier-aware assert.
fn evaluate(case: &CorpusCase) {
    eprintln!("--- {} (tier={:?}) ---", case.name, case.tier);
    let Some(ours) = decode_fixture_pcm(case) else {
        return;
    };
    let Some(refp) = read_reference(case) else {
        eprintln!("{}: could not parse expected.wav", case.name);
        return;
    };

    eprintln!(
        "{}: decoded ch={} sr={} samples={} ({} frames); reference ch={} sr={} samples={} ({} frames)",
        case.name,
        ours.channels,
        ours.sample_rate,
        ours.samples.len(),
        ours.samples.len() / ours.channels.max(1) as usize,
        refp.channels,
        refp.sample_rate,
        refp.samples.len(),
        refp.samples.len() / refp.channels.max(1) as usize,
    );

    if ours.channels != refp.channels {
        eprintln!(
            "{}: WARN channel count mismatch (decoded {} vs reference {})",
            case.name, ours.channels, refp.channels
        );
    }
    if ours.sample_rate != refp.sample_rate {
        eprintln!(
            "{}: WARN sample-rate mismatch (decoded {} vs reference {})",
            case.name, ours.sample_rate, refp.sample_rate
        );
    }

    let stats = compare(&ours, &refp);
    if stats.is_empty() {
        eprintln!("{}: no overlapping channels to compare", case.name);
        return;
    }

    let mut total_exact = 0usize;
    let mut total_near = 0usize;
    let mut total_samples = 0usize;
    let mut max_err_overall = 0i32;
    let mut psnr_min: f64 = f64::INFINITY;
    for (i, s) in stats.iter().enumerate() {
        let psnr = s.psnr_db();
        if psnr < psnr_min {
            psnr_min = psnr;
        }
        let rms_err_disp = if s.total > 0 {
            (s.rms_err / s.total as f64).sqrt()
        } else {
            0.0
        };
        eprintln!(
            "  ch{i}: rms_ref={:.1} rms_ours={:.1} rms_err={:.2} match={:.4}% near<=1LSB={:.4}% max_abs_err={} psnr={:.2} dB",
            s.rms_ref,
            s.rms_ours,
            rms_err_disp,
            s.match_pct(),
            s.near_pct(),
            s.max_abs_err,
            psnr,
        );
        total_exact += s.exact;
        total_near += s.near;
        total_samples += s.total;
        if s.max_abs_err > max_err_overall {
            max_err_overall = s.max_abs_err;
        }
    }
    let agg_pct = if total_samples > 0 {
        total_exact as f64 / total_samples as f64 * 100.0
    } else {
        0.0
    };
    let near_pct = if total_samples > 0 {
        total_near as f64 / total_samples as f64 * 100.0
    } else {
        0.0
    };
    eprintln!(
        "{}: aggregate match={:.4}% near<=1LSB={:.4}% max_abs_err={} min_psnr={:.2} dB",
        case.name, agg_pct, near_pct, max_err_overall, psnr_min,
    );

    match case.tier {
        Tier::BitExact => {
            assert_eq!(
                total_exact, total_samples,
                "{}: not bit-exact (max_abs_err={} match={:.4}%)",
                case.name, max_err_overall, agg_pct,
            );
        }
        Tier::ReportOnly => {
            // Logged; never gates CI. Underlying float-rounding deltas
            // are tracked as follow-up tasks if PSNR drops well below
            // the libopus envelope.
        }
    }
}

// ---------------------------------------------------------------------------
// Per-fixture tests — every entry maps 1:1 to a directory under
// docs/audio/opus/fixtures/. All start `Tier::ReportOnly` per the
// brief: Opus is lossy and our decoder is permitted to diverge from
// libopus within the spec's float-IMDCT envelope.
//
// Per-stream sample_rate: the Ogg demuxer extracts OpusHead's
// `input_sample_rate` field so the params still expose it (12/16/
// 24/48 kHz on the SILK-flavoured fixtures), but the decoder always
// emits at 48 kHz. The `sample_rate` field on each CorpusCase below
// pins the OpusHead value, not the decoded rate.
// ---------------------------------------------------------------------------

#[test]
fn corpus_celt_2_5ms_low_latency() {
    evaluate(&CorpusCase {
        name: "celt-2.5ms-low-latency",
        channels: Some(2),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_celt_fb_stereo_128kbps() {
    evaluate(&CorpusCase {
        name: "celt-fb-stereo-128kbps",
        channels: Some(2),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_code_0_single_frame() {
    evaluate(&CorpusCase {
        name: "code-0-single-frame",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_code_1_two_equal_frames() {
    evaluate(&CorpusCase {
        name: "code-1-two-equal-frames",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_code_2_two_different_frames() {
    evaluate(&CorpusCase {
        name: "code-2-two-different-frames",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_code_3_arbitrary_frames_with_padding() {
    evaluate(&CorpusCase {
        name: "code-3-arbitrary-frames-with-padding",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_fec_on() {
    // FEC adds LBRR redundancy bits inside the SILK header; the
    // decoder parses the flags and decode-discards the redundancy
    // payload via a scratch SILK channel. Loss-free output is
    // unaffected.
    evaluate(&CorpusCase {
        name: "fec-on",
        channels: Some(1),
        // OpusHead input rate = 16 kHz (SILK NB encoding); decoder
        // still outputs 48 kHz.
        sample_rate: Some(16_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_hybrid_fb_mono_28kbps() {
    evaluate(&CorpusCase {
        name: "hybrid-fb-mono-28kbps",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_mode_switching() {
    evaluate(&CorpusCase {
        name: "mode-switching",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_multistream_5_1() {
    // Channel mapping family 1 (Vorbis surround). The OpusHead carries
    // stream_count / coupled_count / channel-mapping table; the
    // decoder factory routes us through `MultistreamOpusDecoder` which
    // demuxes each Ogg packet into N sub-streams and mixes them up to
    // the 6-channel output.
    evaluate(&CorpusCase {
        name: "multistream-5.1",
        channels: Some(6),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_pair_cbr_64kbps() {
    evaluate(&CorpusCase {
        name: "pair-cbr-64kbps",
        channels: Some(2),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_pair_mono_48k_64kbps() {
    evaluate(&CorpusCase {
        name: "pair-mono-48k-64kbps",
        channels: Some(1),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_pair_stereo_48k_64kbps() {
    evaluate(&CorpusCase {
        name: "pair-stereo-48k-64kbps",
        channels: Some(2),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_pair_vbr_64kbps() {
    evaluate(&CorpusCase {
        name: "pair-vbr-64kbps",
        channels: Some(2),
        sample_rate: Some(48_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_silence_low_bitrate() {
    evaluate(&CorpusCase {
        name: "silence-low-bitrate",
        channels: Some(1),
        sample_rate: Some(16_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_silk_mb_60ms_mono_20kbps() {
    evaluate(&CorpusCase {
        name: "silk-mb-60ms-mono-20kbps",
        channels: Some(1),
        sample_rate: Some(12_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_silk_nb_mono_16kbps() {
    evaluate(&CorpusCase {
        name: "silk-nb-mono-16kbps",
        channels: Some(1),
        sample_rate: Some(8_000),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_silk_wb_stereo_20kbps() {
    evaluate(&CorpusCase {
        name: "silk-wb-stereo-20kbps",
        channels: Some(2),
        sample_rate: Some(16_000),
        tier: Tier::ReportOnly,
    });
}
