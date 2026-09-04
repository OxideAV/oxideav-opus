//! Signal analyser — per-frame **speech-vs-music evidence** and a
//! content-bandwidth estimate for the unified encoder's §2.1 / §5
//! mode and bandwidth election.
//!
//! RFC 6716 §5 lists the "type of signal (speech vs. music)" among
//! the inputs of the automatic configuration decision and leaves the
//! detection itself to the implementation ("An Opus encoder
//! implementation could also do automatic detection, but […] would
//! likely have to […] delay the mode switching decisions"). §2 pins
//! the semantics the detector serves: "The MDCT layer is not used for
//! speech when the audio bandwidth is WB or less, as it is not useful
//! there. On the other hand, non-speech signals are not always
//! adequately coded using linear prediction. Therefore, the MDCT
//! layer should be used for music signals." §2.1.3 adds that the
//! encoder "attempts to make the best bandwidth decision possible".
//!
//! Everything here is this crate's own design from those semantics
//! (no classifier was transcribed from anywhere): the analyser runs
//! on a fixed **10 ms block grid** (480 samples at 48 kHz, so every
//! §2.1.4 frame duration is a whole number of blocks and a decision
//! is refreshed at least once per 10 ms of input), computing per block
//!
//! * **tonality** — spectral flatness (arithmetic / geometric mean of
//!   a 512-point power spectrum, in dB; white noise ≈ 0, a pure tone
//!   ≈ 40);
//! * **spectral flux** — mean absolute change of 27 log-spaced band
//!   log-energies against the previous block (speech articulates
//!   several dB per 10 ms; sustained music moves much less);
//! * **harmonicity / pitch** — the normalised autocorrelation peak of
//!   an 8 kHz decimated 40 ms window over 50–500 Hz lags;
//! * **pitch stability** — the running mean of the log-lag step
//!   between consecutive voiced blocks (spoken pitch glides a few
//!   percent per block; held notes do not);
//! * **transient density** — the fraction of recent blocks whose
//!   2.5 ms sub-block energies span more than 12 dB;
//! * **envelope modulation** — the standard deviation of the block
//!   level over the last ~0.6 s (the syllabic 2–8 Hz modulation that
//!   is speech's strongest fingerprint);
//! * **high-frequency ratio** and the per-band content test that
//!   yields the **bandwidth estimate** (NB/MB/WB/SWB/FB);
//! * **stereo width** — one minus the L/R correlation.
//!
//! A fixed logistic combination of those features gives a per-block
//! music probability; an exponential smoother and a two-threshold
//! **hysteresis** with a dwell requirement turn it into the
//! [`SignalClass`] the encoder acts on, so the election never chases
//! block-level noise (the "delay the mode switching decisions" the
//! RFC anticipates). Silence and inactive blocks freeze every
//! estimate.

use crate::toc::Bandwidth;

/// Analysis block: 10 ms at 48 kHz.
pub const BLOCK_SAMPLES: usize = 480;

/// Power-spectrum window (log2 = 9): 93.75 Hz bins at 48 kHz.
const FFT_LEN: usize = 512;
const FFT_LOG2: usize = 9;

/// 8 kHz pitch-analysis window (40 ms) and lag range (50–500 Hz).
const PITCH_RATE_RATIO: usize = 6;
const PITCH_WINDOW: usize = 320;
const PITCH_MIN_LAG: usize = 16;
const PITCH_MAX_LAG: usize = 160;
/// Decimated history kept: window + max lag.
const PITCH_HIST: usize = PITCH_WINDOW + PITCH_MAX_LAG;

/// Log-spaced band edges (FFT bins) for the flux measure.
const FLUX_EDGES: [usize; 28] = [
    1, 2, 3, 4, 5, 6, 8, 10, 12, 14, 17, 20, 24, 28, 33, 39, 46, 54, 64, 75, 88, 104, 122, 144,
    170, 200, 235, 256,
];

/// Envelope-modulation history (active blocks): 64 × 10 ms.
const LEVEL_HIST: usize = 64;

/// Transient-density history: 100 blocks (1 s).
const TRANSIENT_HIST: usize = 100;

/// Content-bandwidth memory: a band stays "present" for this many
/// blocks after it was last heard (1.5 s), so brief dips never
/// trigger a §4.5 bandwidth transition.
const BANDWIDTH_HOLD_BLOCKS: u32 = 150;

/// Absolute activity floor (block RMS, dBFS re 32768).
const ACTIVITY_FLOOR_DB: f32 = -60.0;
/// Blocks must sit this far above the tracked noise floor.
const ACTIVITY_MARGIN_DB: f32 = 6.0;
/// The noise-floor tracker rises at most this fast (dB per block)
/// and never above this level, so a steady tone stays active.
const FLOOR_RISE_DB: f32 = 0.1;
const FLOOR_CAP_DB: f32 = -35.0;

/// Smoothing constant of the music probability (≈ 250 ms).
const PROB_SMOOTH: f32 = 0.04;
/// Hysteresis thresholds on the smoothed probability.
const MUSIC_ENTER: f32 = 0.62;
const MUSIC_LEAVE: f32 = 0.38;
/// Consecutive active blocks beyond a threshold before a class
/// change (400 ms), and the evidence needed before leaving
/// [`SignalClass::Unknown`] (800 ms: the modulation windows must
/// have filled — probabilities from half-filled windows are not
/// smoothed in at all, see [`SMOOTH_MIN_BLOCKS`]).
const CLASS_DWELL_BLOCKS: u32 = 40;
const FIRST_DECISION_BLOCKS: u32 = 80;
const SMOOTH_MIN_BLOCKS: usize = 32;

/// The class the election acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignalClass {
    /// Not enough active input yet (the encoder keeps its
    /// bitrate-only ladder).
    #[default]
    Unknown,
    /// Speech-like: the LP layer's territory below SWB.
    Speech,
    /// Music-like / non-speech: the MDCT layer's territory.
    Music,
}

/// Per-block features (the most recent active block's values; frozen
/// across inactive blocks).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalFeatures {
    /// Block RMS level, dBFS (0 dB = full-scale RMS).
    pub level_db: f32,
    /// The block cleared the activity floor.
    pub active: bool,
    /// Spectral flatness in dB (arithmetic / geometric mean power).
    pub tonality: f32,
    /// Mean absolute band log-energy change vs the previous block (dB).
    pub spectral_flux: f32,
    /// Normalised autocorrelation peak over the pitch lag range (0..1).
    pub harmonicity: f32,
    /// Standard deviation of the harmonicity over the last ~0.6 s of
    /// active blocks (speech alternates voiced / unvoiced; sustained
    /// music does not).
    pub harmonicity_variation: f32,
    /// Estimated pitch in Hz when the block is voiced (harmonicity
    /// above 0.5).
    pub pitch_hz: Option<f32>,
    /// 1 − (running log-lag step / 0.08), clamped to 0..1.
    pub pitch_stability: f32,
    /// Fraction of the last second's blocks that were transient.
    pub transient_density: f32,
    /// Standard deviation of the active-block level (dB) over ~0.6 s.
    pub envelope_modulation: f32,
    /// Energy fraction above 4 kHz.
    pub hf_ratio: f32,
    /// 1 − max(0, L/R correlation); 0 for mono input.
    pub stereo_width: f32,
    /// Instantaneous content bandwidth of this block.
    pub bandwidth: Bandwidth,
}

impl Default for SignalFeatures {
    fn default() -> Self {
        Self {
            level_db: -120.0,
            active: false,
            tonality: 0.0,
            spectral_flux: 0.0,
            harmonicity: 0.0,
            harmonicity_variation: 0.0,
            pitch_hz: None,
            pitch_stability: 0.0,
            transient_density: 0.0,
            envelope_modulation: 0.0,
            hf_ratio: 0.0,
            stereo_width: 0.0,
            bandwidth: Bandwidth::Nb,
        }
    }
}

/// The analyser's current verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalVerdict {
    /// Class after hysteresis — what the election acts on.
    pub class: SignalClass,
    /// Smoothed music probability (0 = speech, 1 = music).
    pub music_probability: f32,
    /// The last active block's instantaneous probability.
    pub instant_probability: f32,
    /// Content bandwidth with the hold-down memory: the widest band
    /// heard in the last [`BANDWIDTH_HOLD_BLOCKS`] active blocks.
    pub bandwidth: Bandwidth,
    /// Active blocks seen so far.
    pub active_blocks: u32,
    /// The most recent block features.
    pub features: SignalFeatures,
}

impl Default for SignalVerdict {
    fn default() -> Self {
        Self {
            class: SignalClass::Unknown,
            music_probability: 0.5,
            instant_probability: 0.5,
            bandwidth: Bandwidth::Fb,
            active_blocks: 0,
            features: SignalFeatures::default(),
        }
    }
}

/// Streaming analyser over 48 kHz interleaved S16 input.
#[derive(Debug, Clone)]
pub struct SignalAnalyser {
    channels: usize,
    /// Interleaved samples not yet forming a whole block.
    pending: Vec<i16>,
    /// Last [`FFT_LEN`] mono-downmix samples (oldest first).
    mono_hist: Vec<f32>,
    /// 8 kHz decimated history (oldest first).
    pitch_hist: Vec<f32>,
    /// Decimation low-pass state (6-sample box × 2, cascaded).
    lp_box1: [f32; PITCH_RATE_RATIO],
    lp_box2: [f32; PITCH_RATE_RATIO],
    lp_pos: usize,
    /// Previous block's band log-energies (flux).
    prev_band_db: [f32; FLUX_EDGES.len() - 1],
    prev_band_valid: bool,
    /// Active-block level ring (envelope modulation) and the parallel
    /// harmonicity ring (voiced / unvoiced alternation).
    level_hist: [f32; LEVEL_HIST],
    harm_hist: [f32; LEVEL_HIST],
    level_count: usize,
    level_pos: usize,
    /// Transient flags ring.
    transient_hist: [bool; TRANSIENT_HIST],
    transient_pos: usize,
    transient_sum: usize,
    transient_count: usize,
    /// Pitch tracking: last voiced lag (8 kHz samples) if the previous
    /// block was voiced, and the running log-lag jitter.
    prev_lag: Option<f32>,
    pitch_jitter: f32,
    /// Stereo width EMA.
    width_ema: f32,
    /// Noise-floor tracker (dB).
    noise_floor_db: f32,
    /// Blocks since each upper band (4–6 / 6–8 / 8–12 / 12–20 kHz) was
    /// last present.
    band_age: [u32; 4],
    /// Blocks of the most recent [`Self::analyse`] call that were
    /// active / total (the encoder's DTX gate reads them).
    last_call_active: u32,
    last_call_blocks: u32,
    /// Hysteresis state.
    smoothed: f32,
    above: u32,
    below: u32,
    verdict: SignalVerdict,
    /// Window + twiddles.
    window: Vec<f32>,
}

impl SignalAnalyser {
    /// New analyser for `channels` (1 or 2) interleaved channels.
    #[must_use]
    pub fn new(channels: usize) -> Self {
        let channels = channels.clamp(1, 2);
        let window = (0..FFT_LEN)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_LEN as f32).cos())
            .collect();
        Self {
            channels,
            pending: Vec::with_capacity(BLOCK_SAMPLES * channels),
            mono_hist: vec![0.0; FFT_LEN],
            pitch_hist: vec![0.0; PITCH_HIST],
            lp_box1: [0.0; PITCH_RATE_RATIO],
            lp_box2: [0.0; PITCH_RATE_RATIO],
            lp_pos: 0,
            prev_band_db: [0.0; FLUX_EDGES.len() - 1],
            prev_band_valid: false,
            level_hist: [0.0; LEVEL_HIST],
            harm_hist: [0.0; LEVEL_HIST],
            level_count: 0,
            level_pos: 0,
            transient_hist: [false; TRANSIENT_HIST],
            transient_pos: 0,
            transient_sum: 0,
            transient_count: 0,
            prev_lag: None,
            pitch_jitter: 0.04,
            width_ema: 0.0,
            noise_floor_db: -120.0,
            band_age: [u32::MAX; 4],
            last_call_active: 0,
            last_call_blocks: 0,
            smoothed: 0.5,
            above: 0,
            below: 0,
            verdict: SignalVerdict::default(),
            window,
        }
    }

    /// Channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Forget everything (a fresh stream).
    pub fn reset(&mut self) {
        *self = Self::new(self.channels);
    }

    /// The current verdict.
    #[must_use]
    pub fn verdict(&self) -> SignalVerdict {
        self.verdict
    }

    /// Feed interleaved S16 input (any length that is a multiple of
    /// the channel count); every completed 10 ms block updates the
    /// verdict, which is returned.
    pub fn analyse(&mut self, pcm: &[i16]) -> SignalVerdict {
        let block_len = BLOCK_SAMPLES * self.channels;
        self.last_call_active = 0;
        self.last_call_blocks = 0;
        let mut input = pcm;
        if !self.pending.is_empty() {
            let need = block_len - self.pending.len();
            let take = need.min(input.len());
            self.pending.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.pending.len() == block_len {
                let block = std::mem::take(&mut self.pending);
                self.process_block(&block);
                self.pending = block;
                self.pending.clear();
            }
        }
        while input.len() >= block_len {
            let (block, rest) = input.split_at(block_len);
            self.process_block(block);
            input = rest;
        }
        self.pending.extend_from_slice(input);
        self.verdict
    }

    /// Whether every block completed by the most recent
    /// [`Self::analyse`] call sat below the activity floor (`false`
    /// when that call completed no block, so a caller feeding 2.5 /
    /// 5 ms frames never suppresses on a partial block). This is the
    /// §2.1.9 "silence or background noise" test: the tracked noise
    /// floor plus a margin, not digital silence.
    #[must_use]
    pub fn last_frame_inactive(&self) -> bool {
        self.last_call_blocks > 0 && self.last_call_active == 0
    }

    /// One 10 ms block of interleaved input.
    fn process_block(&mut self, block: &[i16]) {
        let ch = self.channels;
        // ---- Mono downmix, level, stereo correlation, sub-block energies.
        let mut mono = [0.0f32; BLOCK_SAMPLES];
        let (mut sll, mut srr, mut slr) = (0.0f64, 0.0f64, 0.0f64);
        let mut energy = 0.0f64;
        let mut sub_e = [0.0f64; 4];
        for (i, m) in mono.iter_mut().enumerate() {
            let l = f32::from(block[i * ch]);
            let r = if ch == 2 {
                f32::from(block[i * ch + 1])
            } else {
                l
            };
            *m = 0.5 * (l + r);
            sll += f64::from(l) * f64::from(l);
            srr += f64::from(r) * f64::from(r);
            slr += f64::from(l) * f64::from(r);
            let e = f64::from(*m) * f64::from(*m);
            energy += e;
            sub_e[i / (BLOCK_SAMPLES / 4)] += e;
        }
        let rms = (energy / BLOCK_SAMPLES as f64).sqrt() as f32;
        let level_db = 20.0 * (rms / 32768.0).max(1e-9).log10();

        // ---- Activity + noise floor.
        if level_db < self.noise_floor_db {
            self.noise_floor_db = level_db;
        } else {
            self.noise_floor_db = (self.noise_floor_db + FLOOR_RISE_DB).min(FLOOR_CAP_DB);
        }
        let active =
            level_db > ACTIVITY_FLOOR_DB && level_db > self.noise_floor_db + ACTIVITY_MARGIN_DB;
        self.last_call_blocks += 1;
        self.last_call_active += u32::from(active);

        // ---- Histories (always rolled, so silence gaps stay in the
        // windows the features see).
        self.mono_hist.copy_within(BLOCK_SAMPLES.., 0);
        self.mono_hist[FFT_LEN - BLOCK_SAMPLES..].copy_from_slice(&mono);
        self.push_decimated(&mono);

        let mut f = self.verdict.features;
        f.level_db = level_db;
        f.active = active;
        if !active {
            self.prev_band_valid = false;
            self.prev_lag = None;
            self.verdict.features = f;
            self.age_bands();
            return;
        }

        // ---- Power spectrum.
        let spec = self.power_spectrum();
        let total: f64 = spec[1..].iter().sum();
        let total = total.max(1e-9);
        // Spectral flatness over ~190 Hz – 8 kHz.
        let lo = 2;
        let hi = 86;
        let mut am = 0.0f64;
        let mut lg = 0.0f64;
        for &p in &spec[lo..hi] {
            let p = p + 1e-3;
            am += p;
            lg += p.ln();
        }
        let n = (hi - lo) as f64;
        let tonality = (10.0 * ((am / n).ln() - lg / n) / std::f64::consts::LN_10) as f32;
        let hf: f64 = spec[43..].iter().sum();
        let hf_ratio = (hf / total) as f32;

        // Content bandwidth: a band is present when it holds more than
        // −45 dB of the block energy and clears an absolute floor.
        let band_sum = |a: usize, b: usize| -> f64 { spec[a..b.min(FFT_LEN / 2)].iter().sum() };
        let floor = 1e-4 * total.max(1e3);
        let abs_floor = 1e2; // ≈ −80 dBFS per band
        let present = [
            band_sum(43, 64),   // 4–6 kHz
            band_sum(64, 86),   // 6–8 kHz
            band_sum(86, 128),  // 8–12 kHz
            band_sum(128, 256), // 12–20 kHz
        ]
        .map(|e| e > floor && e > abs_floor);
        for (age, p) in self.band_age.iter_mut().zip(present) {
            *age = if p { 0 } else { age.saturating_add(1) };
        }
        f.bandwidth = Self::bandwidth_from(present);

        // Spectral flux across log-spaced bands.
        // Bands more than 60 dB under the block total sit on a floor,
        // so window leakage in empty bands does not register as flux.
        let flux_floor = 1e-6 * total + 1e2;
        let mut band_db = [0.0f32; FLUX_EDGES.len() - 1];
        for (i, b) in band_db.iter_mut().enumerate() {
            let e = band_sum(FLUX_EDGES[i], FLUX_EDGES[i + 1]);
            *b = (10.0 * (e + flux_floor).log10()) as f32;
        }
        let spectral_flux = if self.prev_band_valid {
            let s: f32 = band_db
                .iter()
                .zip(self.prev_band_db.iter())
                .map(|(a, b)| (a - b).abs())
                .sum();
            s / band_db.len() as f32
        } else {
            f.spectral_flux
        };
        self.prev_band_db = band_db;
        self.prev_band_valid = true;

        // ---- Pitch.
        let (harmonicity, lag) = self.pitch_search();
        let voiced = harmonicity > 0.5;
        if voiced {
            if let Some(prev) = self.prev_lag {
                let step = (lag / prev).log2().abs().min(0.5);
                self.pitch_jitter += 0.1 * (step - self.pitch_jitter);
            }
            self.prev_lag = Some(lag);
        } else {
            self.prev_lag = None;
        }
        let pitch_hz = voiced.then(|| 8000.0 / lag);
        let pitch_stability = (1.0 - self.pitch_jitter / 0.08).clamp(0.0, 1.0);

        // ---- Transients: 2.5 ms sub-block energy span.
        let emax = sub_e.iter().cloned().fold(0.0f64, f64::max);
        let emin = sub_e.iter().cloned().fold(f64::MAX, f64::min);
        let transient = emax > emin.max(1.0) * 15.85; // 12 dB
        if self.transient_count == TRANSIENT_HIST {
            if self.transient_hist[self.transient_pos] {
                self.transient_sum -= 1;
            }
        } else {
            self.transient_count += 1;
        }
        self.transient_hist[self.transient_pos] = transient;
        if transient {
            self.transient_sum += 1;
        }
        self.transient_pos = (self.transient_pos + 1) % TRANSIENT_HIST;
        let transient_density = self.transient_sum as f32 / self.transient_count as f32;

        // ---- Envelope modulation + harmonicity variation over active
        // blocks.
        self.level_hist[self.level_pos] = level_db;
        self.harm_hist[self.level_pos] = harmonicity;
        self.level_pos = (self.level_pos + 1) % LEVEL_HIST;
        self.level_count = (self.level_count + 1).min(LEVEL_HIST);
        let std_dev = |vals: &[f32]| -> f32 {
            let mean = vals.iter().sum::<f32>() / vals.len() as f32;
            let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / vals.len() as f32;
            var.sqrt()
        };
        let (envelope_modulation, harmonicity_variation) = if self.level_count >= 16 {
            (
                std_dev(&self.level_hist[..self.level_count]),
                std_dev(&self.harm_hist[..self.level_count]),
            )
        } else {
            (f.envelope_modulation, f.harmonicity_variation)
        };

        // ---- Stereo width.
        if ch == 2 {
            let denom = (sll * srr).sqrt();
            let corr = if denom > 1e-6 {
                (slr / denom) as f32
            } else {
                1.0
            };
            let width = 1.0 - corr.max(0.0);
            self.width_ema += 0.1 * (width - self.width_ema);
        }
        let stereo_width = self.width_ema;

        f.tonality = tonality;
        f.spectral_flux = spectral_flux;
        f.harmonicity = harmonicity;
        f.harmonicity_variation = harmonicity_variation;
        f.pitch_hz = pitch_hz;
        f.pitch_stability = pitch_stability;
        f.transient_density = transient_density;
        f.envelope_modulation = envelope_modulation;
        f.hf_ratio = hf_ratio;
        f.stereo_width = stereo_width;

        // ---- Score → probability → hysteresis.
        let p = music_probability(&f, ch == 2);
        if self.level_count >= SMOOTH_MIN_BLOCKS {
            self.smoothed += PROB_SMOOTH * (p - self.smoothed);
        }
        self.verdict.active_blocks = self.verdict.active_blocks.saturating_add(1);
        if self.smoothed > MUSIC_ENTER {
            self.above += 1;
            self.below = 0;
        } else if self.smoothed < MUSIC_LEAVE {
            self.below += 1;
            self.above = 0;
        } else {
            self.above = 0;
            self.below = 0;
        }
        let class = match self.verdict.class {
            SignalClass::Unknown if self.verdict.active_blocks >= FIRST_DECISION_BLOCKS => {
                if self.smoothed > 0.5 {
                    SignalClass::Music
                } else {
                    SignalClass::Speech
                }
            }
            SignalClass::Speech if self.above >= CLASS_DWELL_BLOCKS => SignalClass::Music,
            SignalClass::Music if self.below >= CLASS_DWELL_BLOCKS => SignalClass::Speech,
            c => c,
        };

        self.verdict.class = class;
        self.verdict.music_probability = self.smoothed;
        self.verdict.instant_probability = p;
        self.verdict.bandwidth = self.held_bandwidth();
        self.verdict.features = f;
    }

    /// Age the content-bandwidth memory across an inactive block.
    fn age_bands(&mut self) {
        for age in &mut self.band_age {
            *age = age.saturating_add(1);
        }
        self.verdict.bandwidth = self.held_bandwidth();
    }

    fn held_bandwidth(&self) -> Bandwidth {
        if self.band_age.iter().all(|&a| a == u32::MAX) {
            // Nothing heard yet: no cap.
            return Bandwidth::Fb;
        }
        Self::bandwidth_from(self.band_age.map(|a| a < BANDWIDTH_HOLD_BLOCKS))
    }

    fn bandwidth_from(present: [bool; 4]) -> Bandwidth {
        if present[3] {
            Bandwidth::Fb
        } else if present[2] {
            Bandwidth::Swb
        } else if present[1] {
            Bandwidth::Wb
        } else if present[0] {
            Bandwidth::Mb
        } else {
            Bandwidth::Nb
        }
    }

    /// Decimate one block of mono to 8 kHz (two cascaded 6-sample box
    /// filters, one output per 6 inputs) and roll the pitch history.
    fn push_decimated(&mut self, mono: &[f32; BLOCK_SAMPLES]) {
        let out_n = BLOCK_SAMPLES / PITCH_RATE_RATIO;
        self.pitch_hist.copy_within(out_n.., 0);
        let base = PITCH_HIST - out_n;
        for (k, chunk) in mono.chunks_exact(PITCH_RATE_RATIO).enumerate() {
            let mut acc = 0.0f32;
            for &x in chunk {
                self.lp_box1[self.lp_pos] = x;
                let s1: f32 = self.lp_box1.iter().sum::<f32>() / PITCH_RATE_RATIO as f32;
                self.lp_box2[self.lp_pos] = s1;
                acc = self.lp_box2.iter().sum::<f32>() / PITCH_RATE_RATIO as f32;
                self.lp_pos = (self.lp_pos + 1) % PITCH_RATE_RATIO;
            }
            self.pitch_hist[base + k] = acc;
        }
    }

    /// Normalised autocorrelation peak over the lag range on the
    /// latest 40 ms of 8 kHz signal: `(peak, lag)`.
    fn pitch_search(&self) -> (f32, f32) {
        let x = &self.pitch_hist;
        let start = PITCH_HIST - PITCH_WINDOW;
        let e0: f64 = x[start..]
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum();
        if e0 < 1.0 {
            return (0.0, PITCH_MIN_LAG as f32);
        }
        let mut corr = [0.0f32; PITCH_MAX_LAG + 1];
        let mut best = 0.0f32;
        for (lag, c) in corr.iter_mut().enumerate().skip(PITCH_MIN_LAG) {
            let mut xy = 0.0f64;
            let mut e1 = 0.0f64;
            for n in start..PITCH_HIST {
                let a = f64::from(x[n]);
                let b = f64::from(x[n - lag]);
                xy += a * b;
                e1 += b * b;
            }
            *c = (xy / (e0 * e1).sqrt().max(1e-9)) as f32;
            best = best.max(*c);
        }
        // Octave rule: the SHORTEST lag within a small margin of the
        // peak wins, so a period's multiples (which correlate just as
        // well on a stationary signal) never pull the estimate down
        // an octave between blocks.
        let lag = (PITCH_MIN_LAG..=PITCH_MAX_LAG)
            .find(|&l| corr[l] >= best - 0.05)
            .unwrap_or(PITCH_MIN_LAG);
        (corr[lag], lag as f32)
    }

    /// Hann-windowed 512-point power spectrum of the mono history.
    fn power_spectrum(&self) -> [f64; FFT_LEN / 2] {
        let mut re = [0.0f64; FFT_LEN];
        let mut im = [0.0f64; FFT_LEN];
        for (i, (r, (&x, &w))) in re
            .iter_mut()
            .zip(self.mono_hist.iter().zip(self.window.iter()))
            .enumerate()
        {
            let _ = i;
            *r = f64::from(x * w);
        }
        fft_in_place(&mut re, &mut im);
        let mut p = [0.0f64; FFT_LEN / 2];
        for (k, v) in p.iter_mut().enumerate() {
            *v = re[k] * re[k] + im[k] * im[k];
        }
        p
    }
}

/// Fixed logistic combination of the block features → music
/// probability. Centres sit between typical speech and music values
/// measured on the crate's corpus (`tests/signal_adaptive_election.rs`);
/// every term is clamped so no single feature can swamp the rest.
#[must_use]
pub fn music_probability(f: &SignalFeatures, stereo: bool) -> f32 {
    let term = |v: f32, centre: f32, scale: f32, weight: f32| -> f32 {
        (weight * (v - centre) / scale).clamp(-2.0, 2.0)
    };
    let mut s = 0.0f32;
    // Syllabic level modulation is speech's strongest fingerprint
    // (corpus: speech 7.8–9.3 dB, speech over a music bed 4–5 dB,
    // music 2–2.2 dB, steady tones 0.2 dB).
    s -= term(f.envelope_modulation, 4.0, 1.5, 2.0);
    // Voiced / unvoiced alternation is speech; sustained harmonicity is
    // music.
    s -= term(f.harmonicity_variation, 0.15, 0.08, 1.0);
    // Articulation: spectra that move several dB per 10 ms are speech
    // (clamped tighter: a single onset block's flux spike must not
    // outweigh the sustained evidence).
    s -= term(f.spectral_flux, 3.5, 2.0, 0.7).clamp(-1.0, 1.0);
    // Sustained tonal content leans music, but voiced speech is tonal
    // too (synthetic speech is even more so), so the weight is small.
    s += term(f.tonality, 25.0, 10.0, 0.2);
    // Held pitch leans music; polyphony defeats the lag tracker, so
    // the weight is small.
    s += term(f.pitch_stability, 0.6, 0.2, 0.3);
    if stereo {
        s += term(f.stereo_width, 0.05, 0.1, 0.7).max(-0.5);
    }
    1.0 / (1.0 + (-s).exp())
}

/// In-place iterative radix-2 complex FFT (own textbook
/// implementation).
fn fft_in_place(re: &mut [f64; FFT_LEN], im: &mut [f64; FFT_LEN]) {
    // Bit reversal.
    for i in 0..FFT_LEN {
        let j = i.reverse_bits() >> (usize::BITS as usize - FFT_LOG2);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= FFT_LEN {
        let ang = -std::f64::consts::TAU / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..FFT_LEN).step_by(len) {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let a = start + k;
                let b = a + len / 2;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(an: &mut SignalAnalyser, pcm: &[i16]) -> SignalVerdict {
        let mut v = an.verdict();
        for frame in pcm.chunks(960 * an.channels()) {
            v = an.analyse(frame);
        }
        v
    }

    fn tone(seconds: f32, hz: f32, amp: f32) -> Vec<i16> {
        (0..(seconds * 48_000.0) as usize)
            .map(|i| (amp * (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin()) as i16)
            .collect()
    }

    /// Deterministic white noise (LCG).
    fn noise(seconds: f32, amp: f32, seed: &mut u32) -> Vec<i16> {
        (0..(seconds * 48_000.0) as usize)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let u = (*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                (amp * 2.0 * u) as i16
            })
            .collect()
    }

    #[test]
    fn fft_matches_direct_dft_on_a_bin() {
        let mut re = [0.0f64; FFT_LEN];
        let mut im = [0.0f64; FFT_LEN];
        for (i, r) in re.iter_mut().enumerate() {
            *r = (std::f64::consts::TAU * 7.0 * i as f64 / FFT_LEN as f64).cos();
        }
        fft_in_place(&mut re, &mut im);
        assert!((re[7] - FFT_LEN as f64 / 2.0).abs() < 1e-9);
        assert!((re[FFT_LEN - 7] - FFT_LEN as f64 / 2.0).abs() < 1e-9);
        let leak: f64 = (0..FFT_LEN)
            .filter(|&k| k != 7 && k != FFT_LEN - 7)
            .map(|k| re[k].abs() + im[k].abs())
            .sum();
        assert!(leak < 1e-8, "leak {leak}");
    }

    #[test]
    fn silence_is_inactive_and_unknown() {
        let mut an = SignalAnalyser::new(1);
        let v = feed(&mut an, &vec![0i16; 48_000]);
        assert!(!v.features.active);
        assert_eq!(v.class, SignalClass::Unknown);
        assert_eq!(v.active_blocks, 0);
    }

    #[test]
    fn pure_tone_is_tonal_harmonic_and_stable() {
        let mut an = SignalAnalyser::new(1);
        let v = feed(&mut an, &tone(2.0, 220.0, 8000.0));
        let f = v.features;
        assert!(f.active);
        assert!(f.tonality > 25.0, "tonality {}", f.tonality);
        assert!(f.harmonicity > 0.95, "harmonicity {}", f.harmonicity);
        let hz = f.pitch_hz.expect("voiced");
        assert!((hz - 220.0).abs() < 12.0, "pitch {hz}");
        assert!(f.pitch_stability > 0.9, "stability {}", f.pitch_stability);
        assert!(f.spectral_flux < 1.0, "flux {}", f.spectral_flux);
        assert!(f.envelope_modulation < 0.5);
        assert_eq!(f.bandwidth, Bandwidth::Nb);
        assert_eq!(v.class, SignalClass::Music);
    }

    #[test]
    fn white_noise_is_flat_and_fullband() {
        let mut an = SignalAnalyser::new(1);
        let mut seed = 7;
        let v = feed(&mut an, &noise(2.0, 8000.0, &mut seed));
        let f = v.features;
        assert!(f.active);
        assert!(f.tonality < 4.0, "tonality {}", f.tonality);
        assert!(f.harmonicity < 0.5, "harmonicity {}", f.harmonicity);
        assert!(f.hf_ratio > 0.7, "hf {}", f.hf_ratio);
        assert_eq!(f.bandwidth, Bandwidth::Fb);
        assert_eq!(v.bandwidth, Bandwidth::Fb);
    }

    #[test]
    fn bandwidth_holds_down_then_releases() {
        let mut an = SignalAnalyser::new(1);
        let mut seed = 3;
        feed(&mut an, &noise(1.0, 8000.0, &mut seed));
        assert_eq!(an.verdict().bandwidth, Bandwidth::Fb);
        // A narrowband tone: the FB memory holds for 1.5 s, then drops.
        let v = feed(&mut an, &tone(1.0, 300.0, 8000.0));
        assert_eq!(v.features.bandwidth, Bandwidth::Nb);
        assert_eq!(v.bandwidth, Bandwidth::Fb, "held");
        let v = feed(&mut an, &tone(1.0, 300.0, 8000.0));
        assert_eq!(v.bandwidth, Bandwidth::Nb, "released");
    }

    #[test]
    fn stereo_width_zero_for_dual_mono_and_positive_for_decorrelated() {
        let mut an = SignalAnalyser::new(2);
        let mono = tone(1.0, 440.0, 8000.0);
        let dual: Vec<i16> = mono.iter().flat_map(|&s| [s, s]).collect();
        let v = feed(&mut an, &dual);
        assert!(v.features.stereo_width < 0.01);

        let mut an = SignalAnalyser::new(2);
        let mut seed = 11;
        let l = noise(1.0, 8000.0, &mut seed);
        let r = noise(1.0, 8000.0, &mut seed);
        let wide: Vec<i16> = l.iter().zip(r.iter()).flat_map(|(&a, &b)| [a, b]).collect();
        let v = feed(&mut an, &wide);
        assert!(
            v.features.stereo_width > 0.8,
            "width {}",
            v.features.stereo_width
        );
    }

    #[test]
    fn class_needs_dwell_before_flipping() {
        let mut an = SignalAnalyser::new(1);
        // 2 s of white-ish speech-like modulation would be noisy to
        // synthesise here; instead pin the mechanics: a tone settles
        // to Music and a 100 ms noise burst cannot flip it.
        feed(&mut an, &tone(2.0, 220.0, 8000.0));
        assert_eq!(an.verdict().class, SignalClass::Music);
        let mut seed = 5;
        let v = feed(&mut an, &noise(0.1, 8000.0, &mut seed));
        assert_eq!(v.class, SignalClass::Music);
    }

    #[test]
    fn odd_frame_sizes_accumulate_into_blocks() {
        let mut an = SignalAnalyser::new(1);
        let t = tone(1.0, 220.0, 8000.0);
        // 2.5 ms frames.
        let mut v = an.verdict();
        for frame in t.chunks(120) {
            v = an.analyse(frame);
        }
        assert!(v.active_blocks >= 95, "{}", v.active_blocks);
        // Compare with 60 ms feeding: same block count.
        let mut an2 = SignalAnalyser::new(1);
        let mut v2 = an2.verdict();
        for frame in t.chunks(2880) {
            v2 = an2.analyse(frame);
        }
        assert_eq!(v.active_blocks, v2.active_blocks);
    }
}
