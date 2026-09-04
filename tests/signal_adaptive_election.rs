//! Signal-adaptive mode / bandwidth election (RFC 6716 §2, §2.1.1,
//! §2.1.3, §5 "type of signal (speech vs. music)") — the crate's
//! synthetic corpus (speech-like, music-like, mixed, tones, silence)
//! and the batteries that grade the analyser's verdicts and the
//! unified encoder's `adaptive` election against fixed-mode encodes at
//! equal rate.
//!
//! Quality is graded with two own metrics on the crate's own decode:
//! segmental SNR (20 ms segments, floored, waveform-level) and a
//! bark-spaced log-spectral distance (LSD, dB — the perceptual proxy
//! the election is judged on; lower is better).

#![allow(dead_code)]

use oxideav_opus::signal_analysis::{SignalAnalyser, SignalClass, SignalFeatures};
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
            if (t + b) / beat % 2 == 0 {
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
        .chunks_exact(2)
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
    if cnt == 0 {
        0.0
    } else {
        acc / cnt as f64
    }
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

struct ClassStats {
    name: &'static str,
    blocks: usize,
    music_blocks: usize,
    speech_blocks: usize,
    unknown_blocks: usize,
    /// Active blocks in the second half of the signal, and how many
    /// of them were called Music / Speech.
    late_blocks: usize,
    late_music: usize,
    late_speech: usize,
    /// Class changes after the first decision.
    flips: usize,
    sum: SignalFeatures,
    final_class: SignalClass,
}

fn analyse_class(name: &'static str, pcm: &[i16], channels: usize) -> ClassStats {
    let mut an = SignalAnalyser::new(channels);
    let mut st = ClassStats {
        name,
        blocks: 0,
        music_blocks: 0,
        speech_blocks: 0,
        unknown_blocks: 0,
        late_blocks: 0,
        late_music: 0,
        late_speech: 0,
        flips: 0,
        sum: SignalFeatures::default(),
        final_class: SignalClass::Unknown,
    };
    st.sum.tonality = 0.0;
    st.sum.level_db = 0.0;
    let total_blocks = pcm.len() / (480 * channels);
    let mut prev = SignalClass::Unknown;
    for (i, frame) in pcm.chunks(480 * channels).enumerate() {
        let v = an.analyse(frame);
        if v.class != prev {
            if prev != SignalClass::Unknown {
                st.flips += 1;
            }
            prev = v.class;
        }
        if !v.features.active {
            continue;
        }
        st.blocks += 1;
        let late = i >= total_blocks / 2;
        if late {
            st.late_blocks += 1;
        }
        match v.class {
            SignalClass::Music => {
                st.music_blocks += 1;
                st.late_music += usize::from(late);
            }
            SignalClass::Speech => {
                st.speech_blocks += 1;
                st.late_speech += usize::from(late);
            }
            SignalClass::Unknown => st.unknown_blocks += 1,
        }
        let f = v.features;
        st.sum.level_db += f.level_db;
        st.sum.tonality += f.tonality;
        st.sum.spectral_flux += f.spectral_flux;
        st.sum.harmonicity += f.harmonicity;
        st.sum.harmonicity_variation += f.harmonicity_variation;
        st.sum.pitch_stability += f.pitch_stability;
        st.sum.transient_density += f.transient_density;
        st.sum.envelope_modulation += f.envelope_modulation;
        st.sum.hf_ratio += f.hf_ratio;
        st.sum.stereo_width += f.stereo_width;
        st.final_class = v.class;
    }
    st
}

fn report(st: &ClassStats) {
    let n = st.blocks.max(1) as f32;
    let s = &st.sum;
    println!(
        "{:<12} blocks {:5} music {:5} speech {:5} unk {:4} late {:3}/{:3} flips {} | lvl {:6.1} ton {:5.1} flux {:5.2} harm {:4.2} hvar {:4.2} pstab {:4.2} trans {:4.2} env {:5.2} hf {:4.2} width {:4.2}",
        st.name,
        st.blocks,
        st.music_blocks,
        st.speech_blocks,
        st.unknown_blocks,
        st.late_speech,
        st.late_music,
        st.flips,
        s.level_db / n,
        s.tonality / n,
        s.spectral_flux / n,
        s.harmonicity / n,
        s.harmonicity_variation / n,
        s.pitch_stability / n,
        s.transient_density / n,
        s.envelope_modulation / n,
        s.hf_ratio / n,
        s.stereo_width / n,
    );
}

/// The analyser separates the corpus classes: speech-like material
/// settles on `Speech`, music-like / tonal material on `Music`, with
/// the decided fraction dominating after the dwell.
#[test]
fn analyser_separates_corpus_classes() {
    let mut rows = Vec::new();
    for seed in [1u64, 2, 3] {
        rows.push(analyse_class("speech", &speech_like(6.0, seed, 1), 1));
        rows.push(analyse_class("music", &music_like(6.0, seed, 1), 1));
        rows.push(analyse_class("music-st", &music_like(6.0, seed, 2), 2));
        rows.push(analyse_class("mixed", &mixed(6.0, seed, 1), 1));
    }
    rows.push(analyse_class("tones", &tones(4.0, 1), 1));
    rows.push(analyse_class("silence", &silence(4.0, 1), 1));
    if let Some(ext) = external_speech(1) {
        rows.push(analyse_class("ext-speech", &ext, 1));
    }
    if std::env::var_os("OPUS_SIGNAL_REPORT").is_some() {
        for r in &rows {
            report(r);
        }
    }
    for r in &rows {
        match r.name {
            "mixed" => {
                // Speech over a music bed is genuinely ambiguous (the
                // bed alone plays through the pauses); the requirement
                // is a bounded number of class changes.
                assert!(r.flips <= 4, "{}: {} flips", r.name, r.flips);
            }
            "speech" | "ext-speech" => {
                assert_eq!(r.final_class, SignalClass::Speech, "{} final", r.name);
                assert!(
                    r.late_speech * 10 >= r.late_blocks * 9,
                    "{}: late speech {} of {}",
                    r.name,
                    r.late_speech,
                    r.late_blocks
                );
                assert!(r.flips <= 2, "{}: {} flips", r.name, r.flips);
            }
            "music" | "music-st" | "tones" => {
                assert_eq!(r.final_class, SignalClass::Music, "{} final", r.name);
                assert!(
                    r.late_music * 10 >= r.late_blocks * 9,
                    "{}: late music {} of {}",
                    r.name,
                    r.late_music,
                    r.late_blocks
                );
                assert_eq!(r.flips, 0, "{}: {} flips", r.name, r.flips);
            }
            "silence" => {
                assert_eq!(r.blocks, 0, "silence must stay inactive");
                assert_eq!(r.final_class, SignalClass::Unknown);
            }
            _ => unreachable!(),
        }
    }
}

/// Alternating speech / music segments: the class follows the content
/// with a bounded number of flips (one per boundary, none inside a
/// segment once settled).
#[test]
fn analyser_tracks_alternating_content_with_hysteresis() {
    let channels = 1;
    let pcm = alternating(6, 21, channels);
    let mut an = SignalAnalyser::new(channels);
    let mut prev = SignalClass::Unknown;
    let mut flips = Vec::new();
    for (i, frame) in pcm.chunks(480 * channels).enumerate() {
        let v = an.analyse(frame);
        if v.class != prev {
            flips.push((i, v.class));
            prev = v.class;
        }
    }
    if std::env::var_os("OPUS_SIGNAL_REPORT").is_some() {
        println!("flips: {flips:?}");
    }
    // Unknown → first class, then one flip per boundary (5), with a
    // small allowance for a bounce at a boundary.
    assert!(flips.len() >= 5 && flips.len() <= 8, "flips {flips:?}");
    // Each 3 s segment (300 blocks) is decided correctly by its second
    // half.
    let mut an = SignalAnalyser::new(channels);
    for (i, frame) in pcm.chunks(480 * channels).enumerate() {
        let v = an.analyse(frame);
        let seg = i / 300;
        let into = i % 300;
        if into >= 150 && v.features.active {
            let want = if seg % 2 == 0 {
                SignalClass::Speech
            } else {
                SignalClass::Music
            };
            assert_eq!(v.class, want, "block {i} (segment {seg})");
        }
    }
}

// ---------------------------------------------------------------------------
// Equal-rate election batteries.

use oxideav_opus::{Application, Bandwidth, Mode, OpusDecoder, OpusEncoder, OpusTocByte};

/// End-to-end stream lag on the 120-sample timeline.
const LAG: usize = 120;

struct Run {
    packets: Vec<Vec<u8>>,
    /// (mode, bandwidth) per packet.
    configs: Vec<(Mode, Bandwidth)>,
    /// Configuration changes across the stream.
    switches: usize,
    /// Of which the analyser caused.
    signal_switches: u32,
    kbps: f64,
    seg_snr: f64,
    lsd: f64,
    /// LSD over the second half of the stream (the steady state
    /// after the class decision).
    lsd_tail: f64,
}

fn config_of(packet: &[u8]) -> (Mode, Bandwidth) {
    let toc = OpusTocByte::parse(packet).expect("toc");
    (toc.mode, toc.bandwidth)
}

fn run(
    pcm: &[i16],
    channels: usize,
    app: Application,
    bitrate: u32,
    forced: Option<Mode>,
    adaptive: bool,
) -> Run {
    let mut enc = OpusEncoder::new(channels, app, bitrate).expect("encoder");
    enc.set_mode(forced).expect("mode");
    enc.set_signal_adaptive(adaptive);
    let n = enc.frame_samples() * channels;
    let frames = pcm.len() / n;
    let mut packets = Vec::with_capacity(frames);
    let mut dec = OpusDecoder::new();
    let mut out = Vec::with_capacity(pcm.len() + 4800);
    for f in 0..frames {
        let p = enc.encode_frame(&pcm[f * n..(f + 1) * n]).expect("encode");
        out.extend_from_slice(&dec.decode_packet(&p).expect("decode").pcm);
        packets.push(p);
    }
    let configs: Vec<_> = packets.iter().map(|p| config_of(p)).collect();
    let switches = configs.windows(2).filter(|w| w[0] != w[1]).count();
    let bytes: usize = packets.iter().map(Vec::len).sum();
    let seconds = frames as f64 * enc.frame_samples() as f64 / FS;
    let input = &pcm[..frames * n];
    let half = (frames / 2) * n;
    Run {
        seg_snr: seg_snr_db(input, &out, channels, LAG),
        lsd: lsd_db(input, &out, channels, LAG),
        lsd_tail: lsd_db(&input[half..], &out[half..], channels, LAG),
        kbps: bytes as f64 * 8.0 / seconds / 1000.0,
        packets,
        configs,
        switches,
        signal_switches: enc.signal_switches(),
    }
}

fn mode_tag(cfgs: &[(Mode, Bandwidth)]) -> String {
    let mut counts = std::collections::BTreeMap::new();
    for c in cfgs {
        *counts
            .entry(format!("{:?}/{:?}", c.0, c.1))
            .or_insert(0usize) += 1;
    }
    counts
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// One class of the corpus with its ground-truth class.
struct Clip {
    name: &'static str,
    channels: usize,
    pcm: Vec<i16>,
    class: SignalClass,
}

fn corpus(seconds: f64, seed: u64) -> Vec<Clip> {
    let mut clips = vec![
        Clip {
            name: "speech",
            channels: 1,
            pcm: speech_like(seconds, seed, 1),
            class: SignalClass::Speech,
        },
        Clip {
            name: "music",
            channels: 1,
            pcm: music_like(seconds, seed, 1),
            class: SignalClass::Music,
        },
        Clip {
            name: "music-st",
            channels: 2,
            pcm: music_like(seconds, seed, 2),
            class: SignalClass::Music,
        },
        Clip {
            name: "mixed",
            channels: 1,
            pcm: mixed(seconds, seed, 1),
            class: SignalClass::Unknown,
        },
        Clip {
            name: "tones",
            channels: 1,
            pcm: tones(seconds, 1),
            class: SignalClass::Music,
        },
    ];
    if let Some(ext) = external_speech(1) {
        clips.push(Clip {
            name: "ext-speech",
            channels: 1,
            pcm: ext,
            class: SignalClass::Speech,
        });
    }
    clips
}

struct Row {
    clip: &'static str,
    rate: u32,
    arm: &'static str,
    run: Run,
}

fn battery(clips: &[Clip], rates: &[u32], arms: &[(&'static str, Option<Mode>, bool)]) -> Vec<Row> {
    let mut rows = Vec::new();
    for clip in clips {
        for &rate in rates {
            let rate = if clip.channels == 2 {
                rate * 3 / 2
            } else {
                rate
            };
            for &(arm, forced, adaptive) in arms {
                let run = run(
                    &clip.pcm,
                    clip.channels,
                    Application::Audio,
                    rate,
                    forced,
                    adaptive,
                );
                rows.push(Row {
                    clip: clip.name,
                    rate,
                    arm,
                    run,
                });
            }
        }
    }
    rows
}

fn print_rows(rows: &[Row]) {
    for r in rows {
        println!(
            "{:<10} {:>6} {:<8} kbps {:6.1} segsnr {:6.2} lsd {:5.2} tail {:5.2} sw {:2} (sig {}) {}",
            r.clip,
            r.rate,
            r.arm,
            r.run.kbps,
            r.run.seg_snr,
            r.run.lsd,
            r.run.lsd_tail,
            r.run.switches,
            r.run.signal_switches,
            mode_tag(&r.run.configs)
        );
    }
}

const ARMS: &[(&str, Option<Mode>, bool)] = &[
    ("adaptive", None, true),
    ("auto", None, false),
    ("silk", Some(Mode::SilkOnly), false),
    ("hybrid", Some(Mode::Hybrid), false),
    ("celt", Some(Mode::CeltOnly), false),
];

/// Full matrix for tuning the ladders (`OPUS_ELECTION_REPORT=full`).
#[test]
fn election_report_full_matrix() {
    if std::env::var("OPUS_ELECTION_REPORT").ok().as_deref() != Some("full") {
        return;
    }
    let clips = corpus(6.0, 2);
    let rows = battery(&clips, &[12_000, 16_000, 24_000, 32_000, 48_000], ARMS);
    print_rows(&rows);
}

/// Hybrid SILK-share sweep (`OPUS_ELECTION_REPORT=share`).
#[test]
fn election_report_hybrid_share() {
    if std::env::var("OPUS_ELECTION_REPORT").ok().as_deref() != Some("share") {
        return;
    }
    let clips = corpus(6.0, 2);
    for clip in &clips {
        for rate in [16_000u32, 24_000, 32_000, 48_000] {
            for share in [0.5f64, 0.6, 0.7, 0.8, 0.9] {
                let mut enc = OpusEncoder::new(clip.channels, Application::Audio, rate).unwrap();
                enc.set_mode(Some(Mode::Hybrid)).unwrap();
                enc.set_hybrid_silk_share(share);
                let n = enc.frame_samples() * clip.channels;
                let frames = clip.pcm.len() / n;
                let mut dec = OpusDecoder::new();
                let mut out = Vec::new();
                let mut bytes = 0usize;
                for f in 0..frames {
                    let p = enc.encode_frame(&clip.pcm[f * n..(f + 1) * n]).unwrap();
                    bytes += p.len();
                    out.extend_from_slice(&dec.decode_packet(&p).unwrap().pcm);
                }
                let input = &clip.pcm[..frames * n];
                println!(
                    "{:<10} {:>6} share {share:.1} kbps {:5.1} segsnr {:6.2} lsd {:5.2}",
                    clip.name,
                    rate,
                    bytes as f64 * 8.0 / (frames as f64 * 0.02) / 1000.0,
                    seg_snr_db(input, &out, clip.channels, LAG),
                    lsd_db(input, &out, clip.channels, LAG)
                );
            }
        }
    }
}

fn find<'a>(rows: &'a [Row], clip: &str, rate: u32, arm: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.clip == clip && r.rate == rate && r.arm == arm)
        .expect("row")
}

/// Equal-rate election battery (CI): the adaptive election never
/// loses to the bitrate-only ladder on speech, wins on music / tones
/// where the analyser routes to the MDCT layer (measured against the
/// CELT-only fixed encode too), does not regress mixed content, and
/// switches only at the class decision. The tuning matrices above
/// carry every arm; this one keeps CI to the decisive comparisons.
#[test]
fn adaptive_election_equal_rate_battery() {
    let clips: Vec<Clip> = corpus(5.0, 5)
        .into_iter()
        .filter(|c| matches!(c.name, "speech" | "music" | "mixed" | "tones"))
        .collect();
    let rates = [16_000u32, 24_000];
    let arms: &[(&str, Option<Mode>, bool)] = &[
        ("adaptive", None, true),
        ("auto", None, false),
        ("celt", Some(Mode::CeltOnly), false),
    ];
    let rows = battery(&clips, &rates, arms);
    if std::env::var_os("OPUS_ELECTION_REPORT").is_some() {
        print_rows(&rows);
    }
    let mut music_wins = 0usize;
    for clip in &clips {
        for &rate in &rates {
            let adaptive = &find(&rows, clip.name, rate, "adaptive").run;
            let auto = &find(&rows, clip.name, rate, "auto").run;
            let celt = &find(&rows, clip.name, rate, "celt").run;
            // Rate discipline: the adaptive stream sits on target like
            // the bitrate-only one.
            assert!(
                adaptive.kbps <= auto.kbps.max(rate as f64 / 1000.0) * 1.10 + 0.5,
                "{} {rate}: adaptive rate {:.1} vs auto {:.1}",
                clip.name,
                adaptive.kbps,
                auto.kbps
            );
            match clip.class {
                SignalClass::Speech => {
                    // Speech keeps the §2.1.1 ladder: identical stream.
                    assert!(
                        adaptive.lsd <= auto.lsd + 0.25,
                        "{} {rate}: adaptive LSD {:.2} regressed vs auto {:.2}",
                        clip.name,
                        adaptive.lsd,
                        auto.lsd
                    );
                    assert_eq!(adaptive.switches, 0, "{} {rate}: switched", clip.name);
                }
                SignalClass::Music => {
                    assert!(
                        adaptive.lsd <= auto.lsd + 0.25,
                        "{} {rate}: adaptive LSD {:.2} regressed vs auto {:.2}",
                        clip.name,
                        adaptive.lsd,
                        auto.lsd
                    );
                    // The steady state (second half of the clip) is
                    // the CELT-only encode.
                    assert!(
                        adaptive.lsd_tail <= celt.lsd_tail + 0.5,
                        "{} {rate}: adaptive tail LSD {:.2} too far from CELT {:.2}",
                        clip.name,
                        adaptive.lsd_tail,
                        celt.lsd_tail
                    );
                    if adaptive.lsd + 0.5 < auto.lsd {
                        music_wins += 1;
                    }
                    assert_eq!(
                        adaptive.signal_switches, 1,
                        "{} {rate}: one class decision",
                        clip.name
                    );
                    assert!(
                        adaptive.switches <= 2,
                        "{} {rate}: {} switches",
                        clip.name,
                        adaptive.switches
                    );
                    assert_eq!(
                        adaptive.configs.last().map(|c| c.0),
                        Some(Mode::CeltOnly),
                        "{} {rate}: steady state must be the MDCT layer",
                        clip.name
                    );
                }
                SignalClass::Unknown => {
                    assert!(
                        adaptive.lsd <= auto.lsd + 0.25,
                        "{} {rate}: adaptive LSD {:.2} regressed vs auto {:.2}",
                        clip.name,
                        adaptive.lsd,
                        auto.lsd
                    );
                    assert!(
                        adaptive.switches <= 4,
                        "{} {rate}: {} switches",
                        clip.name,
                        adaptive.switches
                    );
                }
            }
        }
    }
    // The MDCT routing pays off on the music-class rows (measured
    // 0.5–5 dB LSD over the bitrate-only ladder).
    assert!(music_wins >= 3, "only {music_wins} music-class wins");
}

/// Black-box capture dump: with `OPUS_DUMP_DIR` set, write every
/// corpus class's adaptive encode (mono + stereo, alternating
/// speech/music included) in the reference demo program's capture
/// framing plus the 48 kHz s16le input and our own decode, for
/// out-of-tree validation against black-box decoders. A no-op in CI.
#[test]
fn dump_adaptive_captures_for_blackbox() {
    let Some(dir) = std::env::var_os("OPUS_DUMP_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let mut clips = corpus(6.0, 3);
    clips.push(Clip {
        name: "alternating",
        channels: 1,
        pcm: alternating(4, 31, 1),
        class: SignalClass::Unknown,
    });
    clips.push(Clip {
        name: "alternating-st",
        channels: 2,
        pcm: alternating(4, 37, 2),
        class: SignalClass::Unknown,
    });
    clips.push(Clip {
        name: "silence",
        channels: 1,
        pcm: silence(4.0, 1),
        class: SignalClass::Unknown,
    });
    for clip in &clips {
        for rate in [16_000u32, 32_000] {
            let rate = if clip.channels == 2 {
                rate * 3 / 2
            } else {
                rate
            };
            let r = run(
                &clip.pcm,
                clip.channels,
                Application::Audio,
                rate,
                None,
                true,
            );
            let name = format!(
                "adaptive-{}-{}k-{}ch",
                clip.name,
                rate / 1000,
                clip.channels
            );
            let mut bits = Vec::new();
            for p in &r.packets {
                bits.extend_from_slice(&(p.len() as u32).to_be_bytes());
                bits.extend_from_slice(&0u32.to_be_bytes());
                bits.extend_from_slice(p);
            }
            std::fs::write(dir.join(format!("{name}.bits")), bits).expect("write bits");
            let n = r.packets.len() * 960 * clip.channels;
            let mut raw = Vec::with_capacity(n * 2);
            for s in &clip.pcm[..n] {
                raw.extend_from_slice(&s.to_le_bytes());
            }
            std::fs::write(dir.join(format!("{name}.input.s16")), raw).expect("write input");
            let mut dec = OpusDecoder::new();
            let mut own = Vec::new();
            for p in &r.packets {
                for s in dec.decode_packet(p).expect("decode").pcm {
                    own.extend_from_slice(&s.to_le_bytes());
                }
            }
            std::fs::write(dir.join(format!("{name}.own.s16")), own).expect("write own");
            println!(
                "{name}: {} packets, {} switches ({} signal) {}",
                r.packets.len(),
                r.switches,
                r.signal_switches,
                mode_tag(&r.configs)
            );
        }
    }
}
