//! Objective quality versus bitrate, against a black-box encoder at
//! equal bitrate.
//!
//! Synthetic speech-like and music-like signals (mono and stereo) are
//! encoded by the unified `OpusEncoder` over a ladder of target rates
//! and decoded by the crate's decoder; the same sources go through the
//! black-box `opusenc` binary at the same target rates (when it is on
//! `PATH` — otherwise that half is skipped), whose Ogg output is
//! decoded by the very same crate decoder. Both decodes are graded
//! against the source with the crate's two own metrics: a bark-spaced
//! log-spectral distance (dB, lower is better — the perceptual proxy)
//! and segmental SNR (dB, higher is better — waveform level; only
//! meaningful where the coder is waveform matching).
//!
//! `quality_table` prints the full table (run it with `--nocapture`);
//! the assertions are regression floors on this crate's own figures
//! and a bound on its distance to the black-box encoder.

#![allow(dead_code)]

use oxideav_opus::{Application, OpusDecoder, OpusEncoder};
use std::f64::consts::TAU;

const FS: f64 = 48_000.0;

/// Deterministic LCG in [-1, 1).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() + 1.0) * 0.5 * (hi - lo)
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        let i = ((self.next() + 1.0) * 0.5 * items.len() as f64) as usize;
        items[i.min(items.len() - 1)]
    }
}

/// Two-pole resonator (formant / drum body).
struct Resonator {
    a1: f64,
    a2: f64,
    y1: f64,
    y2: f64,
}
impl Resonator {
    fn new(hz: f64, bw: f64) -> Self {
        let r = (-std::f64::consts::PI * bw / FS).exp();
        Self {
            a1: 2.0 * r * (TAU * hz / FS).cos(),
            a2: -r * r,
            y1: 0.0,
            y2: 0.0,
        }
    }
    fn run(&mut self, x: f64) -> f64 {
        let y = x + self.a1 * self.y1 + self.a2 * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Speech-like signal: glottal pulse train with gliding f0, three
/// formants cycling through vowel targets at a syllabic rate,
/// unvoiced fricative onsets, phrase pauses. Mono (duplicated when
/// stereo is requested).
fn speech_like(seconds: f64, seed: u64, channels: usize) -> Vec<i16> {
    let mut rng = Rng(seed);
    let n = (seconds * FS) as usize;
    let vowels: [(f64, f64, f64); 6] = [
        (730.0, 1090.0, 2440.0),
        (270.0, 2290.0, 3010.0),
        (300.0, 870.0, 2240.0),
        (530.0, 1840.0, 2480.0),
        (570.0, 840.0, 2410.0),
        (440.0, 1020.0, 2240.0),
    ];
    let base_f0 = rng.uniform(105.0, 190.0);
    let mut out = vec![0.0f64; n];
    let mut t = 0usize;
    let mut phrase_left = rng.uniform(1.5, 2.5);
    let mut phase = 0.0f64;
    let mut glottal = [0.0f64; 2];
    let mut hp_prev = 0.0f64;
    while t < n {
        if phrase_left <= 0.0 {
            // Pause 250–450 ms.
            let pause = (rng.uniform(0.25, 0.45) * FS) as usize;
            t += pause;
            phrase_left = rng.uniform(1.5, 2.5);
            continue;
        }
        let syl = rng.uniform(0.14, 0.30);
        phrase_left -= syl;
        let syl_n = ((syl * FS) as usize).min(n - t);
        let (v1, v2, v3) = rng.pick(&vowels);
        let mut f1 = Resonator::new(v1 * rng.uniform(0.92, 1.08), 90.0);
        let mut f2 = Resonator::new(v2 * rng.uniform(0.92, 1.08), 110.0);
        let mut f3 = Resonator::new(v3 * rng.uniform(0.95, 1.05), 150.0);
        let onset_unvoiced = rng.next() > 0.3;
        let onset_n = if onset_unvoiced {
            (rng.uniform(0.03, 0.07) * FS) as usize
        } else {
            0
        };
        let f0_start = base_f0 * rng.uniform(0.85, 1.2);
        let f0_end = f0_start * rng.uniform(0.8, 1.15);
        let amp = rng.uniform(0.6, 1.0);
        for i in 0..syl_n {
            let frac = i as f64 / syl_n as f64;
            // Syllabic envelope: fast attack, slower decay.
            let env = if frac < 0.15 {
                frac / 0.15
            } else {
                1.0 - 0.7 * (frac - 0.15) / 0.85
            };
            let src = if i < onset_n {
                // Fricative burst: high-passed noise.
                let w = rng.next();
                let hp = w - hp_prev;
                hp_prev = w;
                0.5 * hp
            } else {
                let f0 = f0_start + (f0_end - f0_start) * frac;
                let f0 = f0 * (1.0 + 0.004 * rng.next());
                phase += f0 / FS;
                let pulse = if phase >= 1.0 {
                    phase -= 1.0;
                    1.0
                } else {
                    0.0
                };
                // Glottal tilt (two cascaded one-pole low-passes).
                glottal[0] += 0.12 * (pulse - glottal[0]);
                glottal[1] += 0.12 * (glottal[0] - glottal[1]);
                glottal[1] * 8.0 + 0.01 * rng.next()
            };
            let y = f3.run(f2.run(f1.run(src)));
            out[t + i] = amp * env * y;
        }
        t += syl_n;
    }
    normalise(&out, 9000.0, channels, None)
}

/// Music-like signal: a four-voice chord progression of harmonic
/// tones with vibrato and note envelopes, a bass line, and a
/// hi-hat / kick pattern at 120 BPM; stereo pans the voices and
/// decorrelates the sides.
fn music_like(seconds: f64, seed: u64, channels: usize) -> Vec<i16> {
    let mut rng = Rng(seed);
    let n = (seconds * FS) as usize;
    let mut left = vec![0.0f64; n];
    let mut right = vec![0.0f64; n];
    // Scale degrees (semitones) over a root.
    let scale: [i32; 10] = [0, 2, 4, 5, 7, 9, 11, 12, 14, 16];
    let root = 220.0 * 2f64.powf(rng.uniform(-0.3, 0.3));
    let beat = (0.5 * FS) as usize; // 120 BPM
    let mut t = 0usize;
    let mut voice_note = [0i32; 4];
    while t < n {
        // A chord holds 1–2 beats.
        let hold = beat * rng.pick(&[1usize, 2]);
        let len = hold.min(n - t);
        for (v, note) in voice_note.iter_mut().enumerate() {
            if rng.next() > -0.2 {
                *note = rng.pick(&scale) + [0, 0, 12, -12][v];
            }
        }
        for (v, &note) in voice_note.iter().enumerate() {
            let hz = root * 2f64.powf(note as f64 / 12.0);
            let pan = [0.25, 0.75, 0.4, 0.6][v];
            let vib_rate = rng.uniform(4.5, 6.5);
            let vib_depth = 0.004;
            let amp = rng.uniform(0.5, 1.0);
            let mut phases = [0.0f64; 8];
            for i in 0..len {
                let frac = i as f64 / len as f64;
                let env = if i < 960 {
                    i as f64 / 960.0
                } else {
                    (1.0 - 0.5 * frac).max(0.0)
                };
                let vib = 1.0 + vib_depth * (TAU * vib_rate * (t + i) as f64 / FS).sin();
                let mut s = 0.0;
                for (k, ph) in phases.iter_mut().enumerate() {
                    let h = (k + 1) as f64;
                    *ph += hz * h * vib / FS;
                    if *ph >= 1.0 {
                        *ph -= 1.0;
                    }
                    if hz * h < 20_000.0 {
                        s += (TAU * *ph).sin() / (h * h).sqrt();
                    }
                }
                let s = amp * env * s;
                left[t + i] += (1.0 - pan) * s;
                right[t + i] += pan * s;
            }
        }
        // Bass: root or fifth, one octave down, whole hold.
        let bass_hz = root / 2.0 * 2f64.powf(rng.pick(&[0.0, 7.0]) / 12.0);
        let mut ph = 0.0;
        for i in 0..len {
            ph += bass_hz / FS;
            if ph >= 1.0 {
                ph -= 1.0;
            }
            let env = (1.0 - i as f64 / len as f64).powf(0.5);
            let s = 1.2 * env * ((TAU * ph).sin() + 0.3 * (2.0 * TAU * ph).sin());
            left[t + i] += 0.5 * s;
            right[t + i] += 0.5 * s;
        }
        // Percussion on each beat inside the hold.
        let mut b = 0;
        while b < len {
            // Hi-hat: 25 ms of high-passed noise.
            let mut hp_prev = 0.0;
            for i in 0..((0.025 * FS) as usize).min(len - b) {
                let w = rng.next();
                let hp = w - hp_prev;
                hp_prev = w;
                let env = 1.0 - i as f64 / (0.025 * FS);
                left[t + b + i] += 0.35 * env * hp;
                right[t + b + i] += 0.35 * env * (hp + 0.3 * rng.next());
            }
            // Kick on every other beat: decaying 60 Hz sine.
            if ((t + b) / beat).is_multiple_of(2) {
                for i in 0..((0.12 * FS) as usize).min(len - b) {
                    let env = (-(i as f64) / (0.03 * FS)).exp();
                    let s = 1.5 * env * (TAU * 60.0 * i as f64 / FS).sin();
                    left[t + b + i] += s;
                    right[t + b + i] += s;
                }
            }
            b += beat;
        }
        t += len;
    }
    if channels == 2 {
        normalise(&left, 9000.0, 2, Some(&right))
    } else {
        let mono: Vec<f64> = left.iter().zip(&right).map(|(l, r)| l + r).collect();
        normalise(&mono, 9000.0, 1, None)
    }
}

/// Speech over a quiet music bed (−14 dB).
fn mixed(seconds: f64, seed: u64, channels: usize) -> Vec<i16> {
    let s = speech_like(seconds, seed, channels);
    let m = music_like(seconds, seed ^ 0x5eed, channels);
    s.iter()
        .zip(&m)
        .map(|(&a, &b)| (f64::from(a) + 0.2 * f64::from(b)).clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// Alternating 3 s speech / 3 s music segments.
fn alternating(segments: usize, seed: u64, channels: usize) -> Vec<i16> {
    let mut out = Vec::new();
    for k in 0..segments {
        if k % 2 == 0 {
            out.extend(speech_like(3.0, seed + k as u64, channels));
        } else {
            out.extend(music_like(3.0, seed + k as u64, channels));
        }
    }
    out
}

/// Stationary multitone (three incommensurate partials).
fn tones(seconds: f64, channels: usize) -> Vec<i16> {
    let n = (seconds * FS) as usize;
    let v: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / FS;
            (TAU * 313.7 * t).sin() + 0.6 * (TAU * 741.3 * t).sin() + 0.4 * (TAU * 1327.9 * t).sin()
        })
        .collect();
    normalise(&v, 9000.0, channels, None)
}

/// Digital silence with a short low-level noise tail.
fn silence(seconds: f64, channels: usize) -> Vec<i16> {
    let n = (seconds * FS) as usize;
    let mut rng = Rng(99);
    (0..n * channels)
        .map(|i| {
            if i / channels > n * 3 / 4 {
                (6.0 * rng.next()) as i16
            } else {
                0
            }
        })
        .collect()
}

/// Peak-normalise to `peak` and interleave (`right` when stereo and
/// distinct; else duplicate).
fn normalise(left: &[f64], peak: f64, channels: usize, right: Option<&[f64]>) -> Vec<i16> {
    let mut m = 1e-9f64;
    for &v in left {
        m = m.max(v.abs());
    }
    if let Some(r) = right {
        for &v in r {
            m = m.max(v.abs());
        }
    }
    let g = peak / m;
    let mut out = Vec::with_capacity(left.len() * channels);
    for (i, &l) in left.iter().enumerate() {
        out.push((l * g) as i16);
        if channels == 2 {
            let r = right.map_or(l, |r| r[i]);
            out.push((r * g) as i16);
        }
    }
    out
}

/// Optional real speech sample (48 kHz s16le, `channels` interleaved)
/// from `OPUS_SPEECH_S16`.
fn external_speech(channels: usize) -> Option<Vec<i16>> {
    let path = std::env::var_os("OPUS_SPEECH_S16")?;
    let bytes = std::fs::read(path).ok()?;
    let mono: Vec<i16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    Some(if channels == 2 {
        mono.iter().flat_map(|&s| [s, s]).collect()
    } else {
        mono
    })
}
// ---------------------------------------------------------------------------
// Metrics.

/// Segmental SNR over 20 ms segments (each floored at −10 dB and
/// capped at 60 dB; silent reference segments skipped).
fn seg_snr_db(input: &[i16], out: &[i16], channels: usize, lag: usize) -> f64 {
    let seg = 960 * channels;
    let n = (input.len().min(out.len().saturating_sub(lag * channels))) / seg;
    let mut acc = 0.0;
    let mut cnt = 0usize;
    for s in 0..n {
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for i in 0..seg {
            let w = f64::from(input[s * seg + i]);
            let d = w - f64::from(out[s * seg + i + lag * channels]);
            sig += w * w;
            err += d * d;
        }
        if sig < 1e3 * seg as f64 {
            continue;
        }
        acc += (10.0 * (sig / err.max(1e-9)).log10()).clamp(-10.0, 60.0);
        cnt += 1;
    }
    if cnt == 0 { 0.0 } else { acc / cnt as f64 }
}

/// Bark-spaced log-spectral distance (dB): RMS over 20 ms frames of
/// the per-band log-power difference (24 bands up to 20 kHz, each
/// band floored 70 dB under the frame peak), channels averaged.
fn lsd_db(input: &[i16], out: &[i16], channels: usize, lag: usize) -> f64 {
    const N: usize = 1024;
    let edges_hz = [
        20.0, 100.0, 200.0, 300.0, 400.0, 510.0, 630.0, 770.0, 920.0, 1080.0, 1270.0, 1480.0,
        1720.0, 2000.0, 2320.0, 2700.0, 3150.0, 3700.0, 4400.0, 5300.0, 6400.0, 7700.0, 9500.0,
        12000.0, 15500.0, 20000.0,
    ];
    let edges: Vec<usize> = edges_hz
        .iter()
        .map(|&h| ((h / FS) * N as f64).round() as usize)
        .collect();
    let hop = 960;
    let frames = input.len() / channels / hop;
    let mut acc = 0.0;
    let mut cnt = 0usize;
    let window: Vec<f64> = (0..N)
        .map(|i| 0.5 - 0.5 * (TAU * i as f64 / N as f64).cos())
        .collect();
    for f in 0..frames {
        let start = f * hop;
        if start + N > input.len() / channels || start + N + lag > out.len() / channels {
            break;
        }
        for c in 0..channels {
            let spec = |x: &[i16], off: usize| -> Vec<f64> {
                let mut re: Vec<f64> = (0..N)
                    .map(|i| f64::from(x[(off + i) * channels + c]) * window[i])
                    .collect();
                let mut im = vec![0.0; N];
                fft(&mut re, &mut im);
                (0..N / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect()
            };
            let a = spec(input, start);
            let b = spec(out, start + lag);
            let peak = a.iter().cloned().fold(0.0, f64::max);
            if peak < 1e6 {
                continue; // silent frame
            }
            let floor = peak * 1e-7;
            let mut d2 = 0.0;
            for w in edges.windows(2) {
                let ea: f64 = a[w[0]..w[1]].iter().sum::<f64>() + floor;
                let eb: f64 = b[w[0]..w[1]].iter().sum::<f64>() + floor;
                let d = 10.0 * (ea / eb).log10();
                d2 += d * d;
            }
            acc += d2 / (edges.len() - 1) as f64;
            cnt += 1;
        }
    }
    if cnt == 0 {
        0.0
    } else {
        (acc / cnt as f64).sqrt()
    }
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -TAU / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

// ---------------------------------------------------------------------------
// Analyser batteries.

// ---------------------------------------------------------------------------
// Harness.

/// Recover the raw Opus packets from an Ogg-Opus byte stream
/// (test-only container scaffolding).
fn ogg_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut off = 0usize;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    while off + 27 <= data.len() && &data[off..off + 4] == b"OggS" {
        let nseg = data[off + 26] as usize;
        let seg_table_end = off + 27 + nseg;
        if seg_table_end > data.len() {
            break;
        }
        let mut p = seg_table_end;
        for &s in &data[off + 27..seg_table_end] {
            let seg_end = (p + s as usize).min(data.len());
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

/// The lag (in samples per channel, 0..=1200) at which `out` best
/// matches `input`, by normalised cross-correlation of channel 0 over
/// a one-second window from the middle of the clip.
fn best_lag(input: &[i16], out: &[i16], channels: usize) -> usize {
    let frames = input.len() / channels;
    let start = frames / 3;
    let len = 48_000.min(frames - start);
    let mut best = (f64::MIN, 0usize);
    for lag in 0..=1200usize {
        if (start + len + lag) * channels > out.len() {
            break;
        }
        let (mut xy, mut yy) = (0.0f64, 0.0f64);
        for i in (0..len).step_by(3) {
            let x = f64::from(input[(start + i) * channels]);
            let y = f64::from(out[(start + i + lag) * channels]);
            xy += x * y;
            yy += y * y;
        }
        let score = xy / yy.sqrt().max(1.0);
        if score > best.0 {
            best = (score, lag);
        }
    }
    best.1
}

#[derive(Clone, Copy)]
struct Grade {
    kbps: f64,
    lsd: f64,
    /// LSD over the second half of the clip (the steady state, past
    /// any start-up mode decision).
    lsd_tail: f64,
    seg_snr: f64,
    /// Packets per operating mode (SILK-only, Hybrid, CELT-only).
    modes: [usize; 3],
}

fn grade(input: &[i16], out: &[i16], channels: usize, bytes: usize) -> Grade {
    let lag = best_lag(input, out, channels);
    let usable = input.len().min(out.len().saturating_sub(lag * channels));
    let usable = usable - usable % (960 * channels);
    let input = &input[..usable];
    let half = usable / 2 / (960 * channels) * (960 * channels);
    Grade {
        kbps: bytes as f64 * 8.0 / (usable / channels) as f64 * FS / 1000.0,
        lsd: lsd_db(input, out, channels, lag),
        lsd_tail: lsd_db(&input[half..], &out[half..], channels, lag),
        seg_snr: seg_snr_db(input, out, channels, lag),
        modes: [0; 3],
    }
}

/// This crate's encoder at `bitrate`, own decode.
fn own(pcm: &[i16], channels: usize, app: Application, bitrate: u32) -> Grade {
    let mut enc = OpusEncoder::new(channels, app, bitrate).expect("encoder");
    // The framework path's default: the signal-adaptive election on.
    enc.set_signal_adaptive(true);
    let n = enc.frame_samples() * channels;
    let mut dec = OpusDecoder::new();
    let mut out = Vec::with_capacity(pcm.len());
    let mut bytes = 0usize;
    let mut modes = [0usize; 3];
    for frame in pcm.chunks_exact(n) {
        let p = enc.encode_frame(frame).expect("encode");
        modes[match p[0] >> 3 {
            0..=11 => 0,
            12..=15 => 1,
            _ => 2,
        }] += 1;
        bytes += p.len();
        out.extend_from_slice(&dec.decode_packet(&p).expect("decode").pcm);
        assert_eq!(dec.final_range(), enc.final_range(), "final range");
    }
    let mut g = grade(pcm, &out, channels, bytes);
    g.modes = modes;
    g
}

fn write_wav(path: &std::path::Path, pcm: &[i16], channels: usize) {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&(channels as u16).to_le_bytes());
    w.extend_from_slice(&48_000u32.to_le_bytes());
    w.extend_from_slice(&(48_000u32 * channels as u32 * 2).to_le_bytes());
    w.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, w).expect("write wav");
}

/// The black-box encoder at `bitrate` (hard CBR so its realized rate
/// is the target, default settings otherwise, the hint the source
/// class implies), decoded by this crate's decoder. `None` when
/// the binary is unavailable or fails.
fn blackbox(pcm: &[i16], channels: usize, speech: bool, bitrate: u32, tag: &str) -> Option<Grade> {
    let dir = std::env::temp_dir().join(format!("oxideav-opus-quality-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let wav = dir.join(format!("{tag}.wav"));
    let opus = dir.join(format!("{tag}.opus"));
    write_wav(&wav, pcm, channels);
    let status = std::process::Command::new("opusenc")
        .arg("--quiet")
        .arg("--hard-cbr")
        .arg("--bitrate")
        .arg(format!("{:.3}", f64::from(bitrate) / 1000.0))
        .arg(if speech { "--speech" } else { "--music" })
        .arg(&wav)
        .arg(&opus)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let data = std::fs::read(&opus).ok()?;
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&opus);
    let _ = std::fs::remove_dir(&dir);
    let packets = ogg_packets(&data);
    let audio = packets.get(2..)?;
    let mut dec = OpusDecoder::new();
    let mut out = Vec::with_capacity(pcm.len());
    let mut bytes = 0usize;
    for p in audio {
        bytes += p.len();
        let decoded = dec.decode_packet(p).ok()?;
        if usize::from(decoded.channels) != channels {
            out.extend_from_slice(&decoded.into_channels(channels as u8).pcm);
        } else {
            out.extend_from_slice(&decoded.pcm);
        }
    }
    Some(grade(pcm, &out, channels, bytes))
}

struct Source {
    name: &'static str,
    speech: bool,
    channels: usize,
    pcm: Vec<i16>,
    rates: &'static [u32],
}

fn sources(seconds: f64) -> Vec<Source> {
    vec![
        Source {
            name: "speech mono",
            speech: true,
            channels: 1,
            pcm: speech_like(seconds, 11, 1),
            rates: &[8_000, 12_000, 16_000, 24_000, 32_000, 48_000, 64_000],
        },
        Source {
            name: "speech stereo",
            speech: true,
            channels: 2,
            pcm: speech_like(seconds, 12, 2),
            rates: &[16_000, 24_000, 32_000, 48_000, 64_000],
        },
        Source {
            name: "music mono",
            speech: false,
            channels: 1,
            pcm: music_like(seconds, 21, 1),
            rates: &[16_000, 24_000, 32_000, 48_000, 64_000, 96_000, 128_000],
        },
        Source {
            name: "music stereo",
            speech: false,
            channels: 2,
            pcm: music_like(seconds, 22, 2),
            rates: &[24_000, 32_000, 48_000, 64_000, 96_000, 128_000],
        },
    ]
}

/// The full table. Release builds grade 6 s clips; debug builds grade
/// 2 s clips on a thinned rate ladder to stay quick.
#[test]
fn quality_table() {
    let quick = cfg!(debug_assertions) && std::env::var_os("OPUS_QUALITY_FULL").is_none();
    let seconds = if quick { 2.0 } else { 6.0 };
    let mut rows = String::new();
    let mut worst_gap = (0.0f64, String::new());
    let mut worst_tail_gap = (0.0f64, String::new());
    let mut compared = 0usize;
    for src in sources(seconds) {
        let app = if src.speech {
            Application::Voip
        } else {
            Application::Audio
        };
        for (ri, &rate) in src.rates.iter().enumerate() {
            if quick && ri % 3 != 1 {
                continue;
            }
            let ours = own(&src.pcm, src.channels, app, rate);
            let tag = format!("{}-{rate}", src.name.replace(' ', "-"));
            let theirs = blackbox(&src.pcm, src.channels, src.speech, rate, &tag);
            assert!(
                ours.lsd.is_finite() && ours.lsd < 14.0,
                "{}: LSD {} dB at {rate} b/s",
                src.name,
                ours.lsd
            );
            // Rate control: the realized average sits near the target.
            assert!(
                ours.kbps < f64::from(rate) / 1000.0 * 1.25 + 1.0,
                "{}: {} kb/s realized for a {rate} b/s target",
                src.name,
                ours.kbps
            );
            match theirs {
                Some(t) => {
                    compared += 1;
                    let gap = ours.lsd - t.lsd;
                    let tail_gap = ours.lsd_tail - t.lsd_tail;
                    if gap > worst_gap.0 {
                        worst_gap = (gap, format!("{} @ {rate}", src.name));
                    }
                    if tail_gap > worst_tail_gap.0 {
                        worst_tail_gap = (tail_gap, format!("{} @ {rate}", src.name));
                    }
                    rows.push_str(&format!(
                        "| {} | {} | {}/{}/{} | {:.1} | {:.2} | {:.2} | {:.1} | {:.1} | {:.2} | {:.2} | {:.1} | {:+.2} | {:+.2} |\n",
                        src.name,
                        rate / 1000,
                        ours.modes[0],
                        ours.modes[1],
                        ours.modes[2],
                        ours.kbps,
                        ours.lsd,
                        ours.lsd_tail,
                        ours.seg_snr,
                        t.kbps,
                        t.lsd,
                        t.lsd_tail,
                        t.seg_snr,
                        gap,
                        tail_gap
                    ));
                }
                None => rows.push_str(&format!(
                    "| {} | {} | {}/{}/{} | {:.1} | {:.2} | {:.2} | {:.1} | - | - | - | - | - | - |\n",
                    src.name,
                    rate / 1000,
                    ours.modes[0],
                    ours.modes[1],
                    ours.modes[2],
                    ours.kbps,
                    ours.lsd,
                    ours.lsd_tail,
                    ours.seg_snr
                )),
            }
        }
    }
    eprintln!(
        "| source | target kb/s | own packets SILK/Hybrid/CELT | own kb/s | own LSD | own LSD 2nd half | own segSNR | black-box kb/s | black-box LSD | black-box LSD 2nd half | black-box segSNR | LSD gap | 2nd-half gap |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n{rows}"
    );
    eprintln!(
        "compared {compared} cells; worst LSD gap {:+.2} dB ({}), worst steady-state gap {:+.2} dB ({})",
        worst_gap.0, worst_gap.1, worst_tail_gap.0, worst_tail_gap.1
    );
    // Regression bound on the distance to the black-box encoder in the
    // steady state (measured worst case on the full matrix: see the
    // README table).
    if !quick {
        assert!(
            worst_tail_gap.0 < 1.5,
            "steady-state LSD gap {:+.2} dB at {}",
            worst_tail_gap.0,
            worst_tail_gap.1
        );
    }
}

/// Diagnostic aid (no assertions; runs only with
/// `OPUS_QUALITY_PROBE=1`): the music sources through each forced
/// mode / bandwidth at the low rates, whole-clip and second-half LSD.
#[test]
fn probe_forced_configurations() {
    use oxideav_opus::toc::{Bandwidth, Mode};
    if std::env::var_os("OPUS_QUALITY_PROBE").is_none() {
        return;
    }
    for (name, ch, pcm) in [
        ("music stereo", 2usize, music_like(6.0, 22, 2)),
        ("music mono", 1, music_like(6.0, 21, 1)),
        ("speech stereo", 2, speech_like(6.0, 12, 2)),
        ("speech mono", 1, speech_like(6.0, 11, 1)),
    ] {
        for rate in [
            10_000u32, 12_000, 16_000, 20_000, 24_000, 28_000, 32_000, 40_000, 48_000,
        ] {
            for (mode, bw) in [
                (Some(Mode::CeltOnly), None),
                (Some(Mode::CeltOnly), Some(Bandwidth::Nb)),
                (Some(Mode::CeltOnly), Some(Bandwidth::Wb)),
                (Some(Mode::CeltOnly), Some(Bandwidth::Swb)),
                (Some(Mode::CeltOnly), Some(Bandwidth::Fb)),
            ] {
                let mut enc = OpusEncoder::new(ch, Application::Audio, rate).unwrap();
                enc.set_mode(mode).unwrap();
                enc.set_bandwidth(bw);
                let n = enc.frame_samples() * ch;
                let mut dec = OpusDecoder::new();
                let mut out = Vec::new();
                let mut bytes = 0usize;
                for f in pcm.chunks_exact(n) {
                    let p = enc.encode_frame(f).unwrap();
                    bytes += p.len();
                    out.extend_from_slice(&dec.decode_packet(&p).unwrap().pcm);
                }
                let lag = best_lag(&pcm, &out, ch);
                let half = pcm.len() / 2 / (960 * ch) * (960 * ch);
                let usable = (out.len() - lag * ch) / (960 * ch) * (960 * ch);
                let usable = usable.min(pcm.len());
                eprintln!(
                    "{name} {rate} {mode:?} {bw:?}: lsd {:.2} snr {:.1} tail {:.2} kbps {:.1}",
                    lsd_db(&pcm[..usable], &out, ch, lag),
                    seg_snr_db(&pcm[..usable], &out, ch, lag),
                    lsd_db(&pcm[half..usable], &out[half..], ch, lag),
                    bytes as f64 * 8.0 / (pcm.len() / ch) as f64 * 48.0,
                );
            }
        }
    }
}
