//! Decoder conformance over a corpus built the way RFC 6716 §6
//! prescribes its own test vectors: bitstreams written by the Appendix A
//! reference listing's demo program (one mode-walk per SILK bandwidth,
//! per Hybrid bandwidth, and over every CELT bandwidth × frame size ×
//! channel count, plus bitrate sweeps with random frame sizes and FEC
//! toggles), each paired with that program's 48 kHz stereo decode.
//!
//! Two gates, both taken from §6:
//!
//! * the **final range** check — "the decoder's … final range coder
//!   state MUST match" the value the encoder recorded for every packet
//!   (the capture carries it per packet);
//! * the **quality measure** of §6.1 — the decode at every output rate
//!   (8 / 12 / 16 / 24 / 48 kHz) and both channel counts is compared
//!   with the 48 kHz stereo reference decode by the Appendix A.4
//!   comparison procedure (transcribed below from the listing embedded
//!   in the staged RFC text); a vector passes when the quality figure
//!   is non-negative.
//!
//! Capture format (the demo program's own): per packet a big-endian u32
//! payload length, a big-endian u32 final range, then the payload.
//! `<name>.dec` is raw s16le interleaved stereo at 48 kHz.
//!
//! The reference decodes are not part of the published package; when a
//! `.dec` file is absent the quality gate is skipped for that vector
//! and the final-range gate still runs.

use oxideav_opus::OpusDecoder;
use std::path::PathBuf;

const VECTORS: [&str; 12] = [
    "cv01-celt-modes",
    "cv02-celt-hq",
    "cv03-silk-nb-modes",
    "cv04-silk-mb-modes",
    "cv05-silk-wb-modes",
    "cv06-hybrid-swb-modes",
    "cv07-hybrid-fb-modes",
    "cv08-voip-sweep",
    "cv09-audio-sweep",
    "cv10-voip-mono-fec-dtx",
    "cv11-audio-cbr-lowrate",
    "cv12-lowdelay-cvbr",
];

const RATES: [u32; 5] = [48_000, 24_000, 16_000, 12_000, 8_000];

fn fixture(name: &str, ext: &str) -> Option<Vec<u8>> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/conformance");
    p.push(format!("{name}.{ext}"));
    std::fs::read(p).ok()
}

/// One captured packet: payload and the encoder's final range.
struct Captured {
    final_range: u32,
    payload: Vec<u8>,
}

fn parse_capture(raw: &[u8]) -> Vec<Captured> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= raw.len() {
        let len = u32::from_be_bytes(raw[off..off + 4].try_into().unwrap()) as usize;
        let final_range = u32::from_be_bytes(raw[off + 4..off + 8].try_into().unwrap());
        off += 8;
        assert!(off + len <= raw.len(), "truncated capture");
        out.push(Captured {
            final_range,
            payload: raw[off..off + len].to_vec(),
        });
        off += len;
    }
    out
}

fn pcm_from_bytes(raw: &[u8]) -> Vec<i16> {
    raw.as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

/// Decode a capture at `rate` onto `channels` output channels,
/// checking the final range of every packet when `check_range`.
fn decode_capture(packets: &[Captured], rate: u32, channels: u8, name: &str) -> Vec<i16> {
    let mut dec = OpusDecoder::with_output_rate(rate).expect("supported rate");
    let mut pcm = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        let audio = dec
            .decode_packet(&p.payload)
            .unwrap_or_else(|e| panic!("{name}: packet {i} failed to decode: {e:?}"));
        if p.final_range != 0 {
            assert_eq!(
                dec.final_range(),
                p.final_range,
                "{name}: final range mismatch at packet {i} (toc {:#04x}, {} bytes)",
                p.payload[0],
                p.payload.len()
            );
        }
        pcm.extend_from_slice(&audio.into_channels(channels).pcm);
    }
    pcm
}

// ---------------------------------------------------------------------
// §6.1 / Appendix A.4 quality measure.
// ---------------------------------------------------------------------

const NBANDS: usize = 21;
const NFREQS: usize = 240;
/// Band edges (in 100 Hz bins) the pseudo noise-to-mask ratio is
/// evaluated on.
const BANDS: [usize; NBANDS + 1] = [
    0, 2, 4, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 68, 80, 96, 120, 156, 200,
];
const TEST_WIN_SIZE: usize = 480;
const TEST_WIN_STEP: usize = 120;

/// Power spectrum of one Hann-windowed block: the first bins of the
/// DFT of `window[j] * x[j]`, scaled by `scale`² and floored by the
/// measure's fixed 100000 bias.
struct Spectrum {
    window: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Spectrum {
    fn new(size: usize) -> Self {
        let pi = std::f32::consts::PI;
        Self {
            window: (0..size)
                .map(|j| 0.5 - 0.5 * ((2.0 * pi / (size as f32 - 1.0)) * j as f32).cos())
                .collect(),
            cos: (0..size)
                .map(|j| ((2.0 * pi / size as f32) * j as f32).cos())
                .collect(),
            sin: (0..size)
                .map(|j| ((2.0 * pi / size as f32) * j as f32).sin())
                .collect(),
        }
    }

    /// Bins `0..nbins` of the windowed block `samples` (one channel,
    /// `window.len()` samples).
    fn power(&self, samples: &[f32], nbins: usize, scale: f32, out: &mut [f32]) {
        let size = self.window.len();
        let block: Vec<f32> = self
            .window
            .iter()
            .zip(samples)
            .map(|(w, v)| w * v)
            .collect();
        for (bin, slot) in out.iter_mut().enumerate().take(nbins) {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            let mut ti = 0usize;
            for &v in &block {
                re += self.cos[ti] * v;
                im -= self.sin[ti] * v;
                ti += bin;
                if ti >= size {
                    ti -= size;
                }
            }
            re *= scale;
            im *= scale;
            *slot = re * re + im * im + 100_000.0;
        }
    }
}

/// One channel of `size` frames of an interleaved signal from `start`.
fn channel_block(signal: &[f32], nch: usize, ch: usize, start: usize, size: usize) -> Vec<f32> {
    (0..size).map(|k| signal[(start + k) * nch + ch]).collect()
}

/// The reference side of the measure: per-frame bin powers and
/// per-frame band energies with the masking spread applied, for a
/// 48 kHz reference signal of `nch` channels.
struct ReferenceAnalysis {
    nch: usize,
    nframes: usize,
    /// `[frame][bin][ch]` bin powers (NFREQS bins).
    bins: Vec<f32>,
    /// `[frame][band][ch]` masked band energies.
    masks: Vec<f32>,
}

fn analyse_reference(reference_stereo: &[i16], nch: usize) -> ReferenceAnalysis {
    // The reference is always 48 kHz stereo; the mono comparison
    // averages the pair.
    let frames = reference_stereo.len() / 2;
    let x: Vec<f32> = if nch == 1 {
        reference_stereo
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| 0.5 * (f32::from(p[0]) + f32::from(p[1])))
            .collect()
    } else {
        reference_stereo.iter().map(|&s| f32::from(s)).collect()
    };
    assert!(frames >= TEST_WIN_SIZE, "reference too short");
    let nframes = (frames - TEST_WIN_SIZE + TEST_WIN_STEP) / TEST_WIN_STEP;
    let spec = Spectrum::new(TEST_WIN_SIZE);
    let mut bins = vec![0.0f32; nframes * NFREQS * nch];
    let mut masks = vec![0.0f32; nframes * NBANDS * nch];
    let mut row = vec![0.0f32; NFREQS];
    for f in 0..nframes {
        for c in 0..nch {
            let block = channel_block(&x, nch, c, f * TEST_WIN_STEP, TEST_WIN_SIZE);
            spec.power(&block, BANDS[NBANDS], 1.0, &mut row);
            for (k, &p) in row.iter().enumerate().take(BANDS[NBANDS]) {
                bins[(f * NFREQS + k) * nch + c] = p;
            }
            for b in 0..NBANDS {
                let sum: f32 = row[BANDS[b]..BANDS[b + 1]].iter().sum();
                masks[(f * NBANDS + b) * nch + c] = sum / (BANDS[b + 1] - BANDS[b]) as f32;
            }
        }
    }
    for f in 0..nframes {
        // Frequency masking, low to high: 10 dB/Bark slope.
        for b in 1..NBANDS {
            for c in 0..nch {
                masks[(f * NBANDS + b) * nch + c] += 0.1 * masks[(f * NBANDS + b - 1) * nch + c];
            }
        }
        // Frequency masking, high to low: 15 dB/Bark slope.
        for b in (0..NBANDS - 1).rev() {
            for c in 0..nch {
                masks[(f * NBANDS + b) * nch + c] += 0.03 * masks[(f * NBANDS + b + 1) * nch + c];
            }
        }
        // Temporal masking: -3 dB per 2.5 ms.
        if f > 0 {
            for b in 0..NBANDS {
                for c in 0..nch {
                    masks[(f * NBANDS + b) * nch + c] +=
                        0.5 * masks[((f - 1) * NBANDS + b) * nch + c];
                }
            }
        }
        // Some cross-talk between the channels is allowed.
        if nch == 2 {
            for b in 0..NBANDS {
                let l = masks[(f * NBANDS + b) * 2];
                let r = masks[(f * NBANDS + b) * 2 + 1];
                masks[(f * NBANDS + b) * 2] += 0.01 * r;
                masks[(f * NBANDS + b) * 2 + 1] += 0.01 * l;
            }
        }
    }
    ReferenceAnalysis {
        nch,
        nframes,
        bins,
        masks,
    }
}

/// The quality figure of the §6.1 comparison for a decode at `rate`
/// with the analysis' channel count: `Err` when the sample counts do
/// not line up, otherwise `Q` in percent (negative = fails).
fn quality(reference: &ReferenceAnalysis, decoded: &[i16], rate: u32) -> Result<f64, String> {
    let nch = reference.nch;
    let nframes = reference.nframes;
    let downsample = (48_000 / rate) as usize;
    let ybands = match rate {
        8_000 => 13,
        12_000 => 15,
        16_000 => 17,
        24_000 => 19,
        _ => NBANDS,
    };
    let ylen = decoded.len() / nch;
    let xlen = (nframes - 1) * TEST_WIN_STEP + TEST_WIN_SIZE;
    if ylen * downsample < xlen {
        return Err(format!(
            "decode too short: {} samples at 48 kHz, reference analysis needs {xlen}",
            ylen * downsample
        ));
    }
    let y: Vec<f32> = decoded.iter().map(|&s| f32::from(s)).collect();
    let spec = Spectrum::new(TEST_WIN_SIZE / downsample);
    let step = TEST_WIN_STEP / downsample;
    let top = BANDS[ybands];

    // Masked bin powers of both sides, `[frame][bin][ch]` over `top` bins.
    let mut xs = vec![0.0f32; nframes * top * nch];
    let mut ys = vec![0.0f32; nframes * top * nch];
    let mut row = vec![0.0f32; top];
    for f in 0..nframes {
        for c in 0..nch {
            let block = channel_block(&y, nch, c, f * step, TEST_WIN_SIZE / downsample);
            spec.power(&block, top, downsample as f32, &mut row);
            for b in 0..ybands {
                let mask = 0.1 * reference.masks[(f * NBANDS + b) * nch + c];
                for k in BANDS[b]..BANDS[b + 1] {
                    xs[(f * top + k) * nch + c] = reference.bins[(f * NFREQS + k) * nch + c] + mask;
                    ys[(f * top + k) * nch + c] = row[k] + mask;
                }
            }
        }
    }
    // Average consecutive frames so the comparison is a little less
    // sensitive to the analysis phase.
    for k in 0..top {
        for c in 0..nch {
            let mut xprev = xs[k * nch + c];
            let mut yprev = ys[k * nch + c];
            for f in 1..nframes {
                let xi = (f * top + k) * nch + c;
                let (xcur, ycur) = (xs[xi], ys[xi]);
                xs[xi] += xprev;
                ys[xi] += yprev;
                xprev = xcur;
                yprev = ycur;
            }
        }
    }
    // At a reduced rate the last 300 Hz are left out (transition-band
    // freedom); at 12 kHz the last band already stops 400 Hz short.
    let max_compare = match rate {
        48_000 => BANDS[NBANDS],
        12_000 => top,
        _ => top - 3,
    };
    let mut err = 0.0f64;
    for f in 0..nframes {
        let mut ef = 0.0f64;
        for b in 0..ybands {
            let mut eb = 0.0f64;
            for k in BANDS[b]..BANDS[b + 1].min(max_compare) {
                for c in 0..nch {
                    let i = (f * top + k) * nch + c;
                    let ratio = ys[i] / xs[i];
                    let mut d = ratio - (f64::from(ratio).ln() as f32) - 1.0;
                    // Less weight around the 8 kHz layer cross-over.
                    if (79..=81).contains(&k) {
                        d *= 0.1;
                    }
                    if k == 80 {
                        d *= 0.1;
                    }
                    eb += f64::from(d);
                }
            }
            eb /= ((BANDS[b + 1] - BANDS[b]) * nch) as f64;
            ef += eb * eb;
        }
        ef /= NBANDS as f64;
        ef *= ef;
        err += ef * ef;
    }
    let err = (err / nframes as f64).powf(1.0 / 16.0);
    Ok(100.0 * (1.0 - 0.5 * (1.0 + err).ln() / 1.13f64.ln()))
}

#[test]
fn quality_measure_is_100_on_identical_signals_and_fails_on_noise() {
    // A deterministic stereo signal against itself scores exactly 100;
    // against an unrelated one it fails.
    let mut seed = 0x1234_5678u32;
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 16) as i16
    };
    let a: Vec<i16> = (0..2 * 9600).map(|_| next() / 4).collect();
    let tone: Vec<i16> = (0..2 * 9600)
        .map(|i| (8000.0 * ((i / 2) as f64 * 0.05).sin()) as i16)
        .collect();
    for nch in [1usize, 2] {
        let r = analyse_reference(&a, nch);
        let same: Vec<i16> = if nch == 2 {
            a.clone()
        } else {
            a.as_chunks::<2>()
                .0
                .iter()
                .map(|p| ((i32::from(p[0]) + i32::from(p[1])) / 2) as i16)
                .collect()
        };
        let q = quality(&r, &same, 48_000).unwrap();
        assert!(q > 99.9, "identical signals must score ~100, got {q}");
        let other: Vec<i16> = if nch == 2 {
            tone.clone()
        } else {
            tone.iter().step_by(2).copied().collect()
        };
        let q = quality(&r, &other, 48_000).unwrap();
        assert!(q < 0.0, "unrelated signals must fail, got {q}");
    }
}

/// The final-range gate: every packet of every vector, at 48 kHz.
#[test]
fn final_range_matches_on_every_packet() {
    let mut checked = 0usize;
    for name in VECTORS {
        let Some(raw) = fixture(name, "bit") else {
            eprintln!("skip {name}: capture not present");
            continue;
        };
        let packets = parse_capture(&raw);
        assert!(!packets.is_empty(), "{name}: empty capture");
        decode_capture(&packets, 48_000, 2, name);
        checked += packets.iter().filter(|p| p.final_range != 0).count();
    }
    eprintln!("final range verified on {checked} packets");
}

/// The same gate over the crate's older reference-encoder captures
/// (DTX run, configuration-switch seams, loss patterns): every packet
/// that carries a recorded range must reproduce it. A zero-length
/// record is a lost packet and is concealed.
#[test]
fn final_range_matches_on_the_legacy_captures() {
    const CAPTURES: [&str; 10] = [
        "dtx-refenc-voice-silence",
        "switch-celt-to-silkwb",
        "switch-hybrid-to-celt",
        "switch-hybrid-to-silkwb",
        "switch-silknb-to-silkwb",
        "switch-silkwb-to-celt",
        "switch-silkwb-to-silknb",
        "loss-celt",
        "loss-hybrid",
        "loss-silkwb",
    ];
    let mut checked = 0usize;
    for name in CAPTURES {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push(format!("tests/fixtures/{name}.bits"));
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        let packets = parse_capture(&raw);
        assert!(!packets.is_empty(), "{name}: empty capture");
        let mut dec = OpusDecoder::new();
        for (i, p) in packets.iter().enumerate() {
            if p.payload.is_empty() {
                dec.conceal_loss();
                assert_eq!(dec.final_range(), 0, "{name}: concealment reports zero");
                continue;
            }
            dec.decode_packet(&p.payload)
                .unwrap_or_else(|e| panic!("{name}: packet {i}: {e:?}"));
            if p.final_range != 0 {
                assert_eq!(
                    dec.final_range(),
                    p.final_range,
                    "{name}: final range mismatch at packet {i}"
                );
                checked += 1;
            }
        }
    }
    eprintln!("final range verified on {checked} legacy-capture packets");
    assert!(checked > 100);
}

/// The §6.1 matrix: every vector × output rate × channel count.
///
/// Debug builds (the default `cargo test`) run the 48 kHz stereo
/// column and one reduced-rate mono cell per vector to keep the suite
/// quick; release builds (or `OPUS_CONFORMANCE_FULL=1`) run the full
/// matrix.
#[test]
fn quality_measure_passes_at_every_rate_and_channel_count() {
    let full = !cfg!(debug_assertions) || std::env::var_os("OPUS_CONFORMANCE_FULL").is_some();
    let mut failures = Vec::new();
    let mut table = String::new();
    for (vi, name) in VECTORS.iter().enumerate() {
        let (Some(raw), Some(dec)) = (fixture(name, "bit"), fixture(name, "dec")) else {
            eprintln!("skip {name}: capture or reference decode not present");
            continue;
        };
        let packets = parse_capture(&raw);
        let reference = pcm_from_bytes(&dec);
        let mut row = format!("| `{name}` |");
        for channels in [2u8, 1] {
            let analysis = analyse_reference(&reference, channels as usize);
            for (ri, &rate) in RATES.iter().enumerate() {
                let quick_pick = if channels == 2 {
                    ri == 0
                } else {
                    ri == 1 + (vi % 4)
                };
                if !full && !quick_pick {
                    continue;
                }
                let pcm = decode_capture(&packets, rate, channels, name);
                match quality(&analysis, &pcm, rate) {
                    Ok(q) => {
                        row.push_str(&format!(" {q:.1} |"));
                        if q < 0.0 {
                            failures.push(format!("{name} {rate} Hz {channels} ch: Q = {q:.1}"));
                        }
                    }
                    Err(e) => failures.push(format!("{name} {rate} Hz {channels} ch: {e}")),
                }
            }
        }
        table.push_str(&row);
        table.push('\n');
    }
    eprintln!("{table}");
    assert!(
        failures.is_empty(),
        "conformance failures:\n{}",
        failures.join("\n")
    );
}

/// Diagnostic aid (no assertions): with `OPUS_CONFORMANCE_TRACE=<name>`
/// prints the per-packet agreement of the 48 kHz stereo decode with the
/// reference decode, and with `OPUS_CONFORMANCE_DUMP=<dir>` also writes
/// the decode there as `<name>.<rate>.<channels>.sw` for every rate and
/// channel count.
#[test]
fn trace_one_vector() {
    let Some(name) = std::env::var_os("OPUS_CONFORMANCE_TRACE") else {
        return;
    };
    let name = name.to_string_lossy().into_owned();
    let (Some(raw), Some(dec)) = (fixture(&name, "bit"), fixture(&name, "dec")) else {
        eprintln!("trace: {name} not present");
        return;
    };
    let packets = parse_capture(&raw);
    let reference = pcm_from_bytes(&dec);
    let mut decoder = OpusDecoder::new();
    let mut pos = 0usize;
    for (i, p) in packets.iter().enumerate() {
        let audio = decoder.decode_packet(&p.payload).unwrap().into_channels(2);
        let n = audio.pcm.len();
        let seg = &reference[pos.min(reference.len())..(pos + n).min(reference.len())];
        let (mut sig, mut noise) = (0.0f64, 0.0f64);
        for (a, b) in audio.pcm.iter().zip(seg) {
            let d = f64::from(*a) - f64::from(*b);
            sig += f64::from(*b) * f64::from(*b);
            noise += d * d;
        }
        let snr = if noise == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (sig.max(1.0) / noise).log10()
        };
        let toc = p.payload.first().copied().unwrap_or(0);
        eprintln!(
            "pkt {i:4} t={:7.3}s cfg {:2} {} code {} len {:4} snr {snr:6.1} dB rms_ref {:7.1}",
            pos as f64 / 96_000.0,
            toc >> 3,
            if toc & 4 != 0 { "st" } else { "mo" },
            toc & 3,
            p.payload.len(),
            (sig / seg.len().max(1) as f64).sqrt(),
        );
        pos += n;
    }
    if let Some(dir) = std::env::var_os("OPUS_CONFORMANCE_DUMP") {
        for channels in [1u8, 2] {
            for rate in RATES {
                let pcm = decode_capture(&packets, rate, channels, &name);
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                let mut path = PathBuf::from(&dir);
                path.push(format!("{name}.{rate}.{channels}.sw"));
                std::fs::write(path, bytes).unwrap();
            }
        }
    }
}
