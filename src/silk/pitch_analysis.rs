//! Encoder-side pitch analysis for SILK — RFC 6716 §4.2.7.6.
//!
//! Short-time open-loop pitch estimation via autocorrelation on a
//! downsampled version of the input signal. Used by the SILK encoder to
//! decide voiced vs unvoiced and, when voiced, to pick a pitch lag in
//! the per-bandwidth range of Table 28 before LTP filtering.
//!
//! # Strategy
//!
//! The RFC describes the decoder side of pitch (pitch_lags[], LTP taps)
//! in §4.2.7.6; the encoder-side analysis is implementation-defined.
//! For a first-cut, we:
//!
//! 1. Downsample the internal-rate PCM to 8 kHz (same as libopus's first
//!    pitch stage). At 8 kHz the pitch range [16, 144] covers human
//!    speech f0 (~56-500 Hz).
//! 2. Compute normalized autocorrelation peaks at each candidate lag.
//! 3. Pick the lag with the highest normalized correlation.
//! 4. Decide voiced when the correlation exceeds a fixed threshold AND
//!    the signal's energy is above a noise floor.
//!
//! This is an *open-loop* estimate — the closed-loop refinement that
//! minimizes the LTP-filtered residual comes in a later pass.
//!
//! # Returned quantities
//!
//! `PitchEstimate` carries:
//! * `voiced` — whether this frame should be encoded as voiced.
//! * `lag_internal` — primary pitch lag at the *internal* SILK rate,
//!   clamped to `pitch_lag_bounds(bw)`. Zero when `voiced == false`.
//! * `correlation` — normalized autocorrelation at the chosen lag, in
//!   [0, 1]. Used by later stages (LTP tap quantisation) as a gain
//!   hint.

use crate::silk::ltp::pitch_lag_bounds;
use crate::toc::OpusBandwidth;

/// Output of the open-loop pitch analyzer.
#[derive(Copy, Clone, Debug, Default)]
pub struct PitchEstimate {
    /// True when the frame is periodic enough to benefit from LTP.
    pub voiced: bool,
    /// Primary pitch lag at the *internal* SILK sampling rate (after
    /// scaling the 8 kHz search result back up). 0 when unvoiced.
    pub lag_internal: i32,
    /// Normalized autocorrelation at the chosen lag, in [0, 1].
    pub correlation: f32,
}

/// Voicing threshold: a frame is declared voiced only when the
/// normalized autocorrelation at the best lag exceeds this value.
/// Picked empirically for speech-like input: sine waves are 1.0, noise
/// hovers around 0.1-0.2, vowels around 0.5-0.9.
const VOICING_CORR_THRESHOLD: f32 = 0.4;

/// Minimum RMS energy required to even run the pitch search. Below this
/// we declare unvoiced unconditionally — cheap rejection of silence /
/// DC frames.
const VOICING_ENERGY_THRESHOLD: f32 = 1e-4;

/// Downsample the internal-rate PCM to 8 kHz for the pitch search.
///
/// NB is already 8 kHz (identity); MB is 12 kHz (ratio 3/2 → we drop to
/// 6 kHz via /2 which is close enough for the correlation peak to
/// survive); WB is 16 kHz (ratio 2 → exact /2).
///
/// For simplicity we use a box-average over `ratio` samples. The
/// autocorrelation peak location is preserved under box filtering as
/// long as the filter is << pitch period, which holds at all three
/// rates for the 16-sample min lag.
fn downsample_for_pitch(pcm: &[f32], bw: OpusBandwidth) -> (Vec<f32>, u32) {
    match bw {
        OpusBandwidth::Narrowband => (pcm.to_vec(), 8_000),
        OpusBandwidth::Mediumband => {
            // 12 kHz → 6 kHz (box /2). Pitch range at 6 kHz is
            // [12, 108] which still covers the same f0 range.
            (box_downsample(pcm, 2), 6_000)
        }
        OpusBandwidth::Wideband => {
            // 16 kHz → 8 kHz (box /2).
            (box_downsample(pcm, 2), 8_000)
        }
        _ => (box_downsample(pcm, 2), 8_000),
    }
}

fn box_downsample(pcm: &[f32], ratio: usize) -> Vec<f32> {
    if ratio <= 1 {
        return pcm.to_vec();
    }
    let n_out = pcm.len() / ratio;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let mut s = 0f32;
        for k in 0..ratio {
            s += pcm[i * ratio + k];
        }
        out.push(s / ratio as f32);
    }
    out
}

/// Compute normalized autocorrelation at a single lag:
///   r(l) = sum(x[n]*x[n-l]) / sqrt(sum(x[n]^2) * sum(x[n-l]^2))
///
/// Returns a value in [-1, 1]; we only care about the positive peaks.
fn normalized_autocorr(x: &[f32], lag: usize) -> f32 {
    if lag == 0 || lag >= x.len() {
        return 0.0;
    }
    let mut num = 0f64;
    let mut e0 = 0f64;
    let mut e1 = 0f64;
    for n in lag..x.len() {
        let a = x[n] as f64;
        let b = x[n - lag] as f64;
        num += a * b;
        e0 += a * a;
        e1 += b * b;
    }
    let denom = (e0 * e1).sqrt();
    if denom < 1e-12 {
        return 0.0;
    }
    (num / denom) as f32
}

/// Run open-loop pitch analysis on one SILK frame of internal-rate PCM.
///
/// `pcm_internal` is the input PCM at the internal sampling rate
/// (8 / 12 / 16 kHz for NB / MB / WB). Length must be the full frame
/// length (160, 240 or 320 for a 20 ms frame).
pub fn analyze_pitch(pcm_internal: &[f32], bw: OpusBandwidth) -> PitchEstimate {
    // Quick-reject silence.
    let rms_sq: f64 = pcm_internal.iter().map(|v| (*v as f64) * (*v as f64)).sum();
    let rms = (rms_sq / pcm_internal.len().max(1) as f64).sqrt() as f32;
    if rms < VOICING_ENERGY_THRESHOLD {
        return PitchEstimate::default();
    }

    let (ds, ds_rate) = downsample_for_pitch(pcm_internal, bw);
    if ds.len() < 32 {
        return PitchEstimate::default();
    }

    // Search range at the downsampled rate.
    // Map the internal-rate lag range to the downsampled rate.
    let (min_lag_int, max_lag_int) = pitch_lag_bounds(bw);
    let internal_rate = match bw {
        OpusBandwidth::Narrowband => 8_000.0_f32,
        OpusBandwidth::Mediumband => 12_000.0,
        OpusBandwidth::Wideband => 16_000.0,
        _ => 16_000.0,
    };
    let scale = ds_rate as f32 / internal_rate;
    let min_lag_ds = ((min_lag_int as f32 * scale).floor() as i32).max(2);
    let max_lag_ds = ((max_lag_int as f32 * scale).ceil() as i32).min(ds.len() as i32 - 1);
    if max_lag_ds <= min_lag_ds {
        return PitchEstimate::default();
    }

    // Brute-force search for the max correlation. `ds` is at most
    // 320 samples so this is cheap (<= 320 × 144 = 46k f32 muladds).
    let search_len = (max_lag_ds - min_lag_ds + 1) as usize;
    let mut corrs = Vec::with_capacity(search_len);
    let mut best_lag_ds = min_lag_ds;
    let mut best_corr = -1f32;
    for lag in min_lag_ds..=max_lag_ds {
        let c = normalized_autocorr(&ds, lag as usize);
        corrs.push(c);
        if c > best_corr {
            best_corr = c;
            best_lag_ds = lag;
        }
    }

    // Octave-error guard: if the winning lag is high and there's a
    // sub-harmonic (half-lag) with "good enough" correlation, prefer
    // the fundamental. We look for any local peak at `best_lag_ds / k`
    // for k ∈ {2, 3} that reaches >= 0.85 × best_corr.
    for k in 2..=3 {
        let cand = best_lag_ds / k;
        if cand < min_lag_ds {
            continue;
        }
        // Find local max in a small window around `cand` (±1).
        let lo = (cand - 1).max(min_lag_ds);
        let hi = (cand + 1).min(max_lag_ds);
        let mut cand_best_lag = cand;
        let mut cand_best_corr = -1f32;
        for l in lo..=hi {
            let c = corrs[(l - min_lag_ds) as usize];
            if c > cand_best_corr {
                cand_best_corr = c;
                cand_best_lag = l;
            }
        }
        if cand_best_corr >= 0.85 * best_corr {
            best_lag_ds = cand_best_lag;
            best_corr = cand_best_corr;
        }
    }

    if best_corr < VOICING_CORR_THRESHOLD {
        return PitchEstimate {
            voiced: false,
            lag_internal: 0,
            correlation: best_corr.max(0.0),
        };
    }

    // Scale the winning lag back up to the internal rate and clamp.
    let lag_internal_f = best_lag_ds as f32 / scale;
    let lag_internal = (lag_internal_f.round() as i32).clamp(min_lag_int, max_lag_int);

    PitchEstimate {
        voiced: true,
        lag_internal,
        correlation: best_corr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    fn synth_sine(freq: f32, rate: u32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / rate as f32).sin() * amp)
            .collect()
    }

    fn synth_harmonic(f0: f32, rate: u32, n: usize, amp: f32) -> Vec<f32> {
        // f0 + 2*f0 + 3*f0 → strongly periodic at 1/f0.
        (0..n)
            .map(|i| {
                let t = i as f32 / rate as f32;
                ((2.0 * PI * f0 * t).sin()
                    + 0.5 * (2.0 * PI * 2.0 * f0 * t).sin()
                    + 0.25 * (2.0 * PI * 3.0 * f0 * t).sin())
                    * amp
            })
            .collect()
    }

    #[test]
    fn silence_is_unvoiced() {
        let pcm = vec![0f32; 160];
        let p = analyze_pitch(&pcm, OpusBandwidth::Narrowband);
        assert!(!p.voiced);
        assert_eq!(p.lag_internal, 0);
    }

    #[test]
    fn pure_sine_200hz_voiced_at_nb() {
        // 200 Hz @ 8 kHz → period = 40 samples.
        let pcm = synth_sine(200.0, 8_000, 160, 0.3);
        let p = analyze_pitch(&pcm, OpusBandwidth::Narrowband);
        assert!(p.voiced, "200 Hz sine should be voiced (corr={})", p.correlation);
        // Allow ±1 sample quantisation error.
        assert!(
            (p.lag_internal - 40).abs() <= 2,
            "expected lag ≈ 40 samples, got {}",
            p.lag_internal
        );
        assert!(p.correlation > 0.8);
    }

    #[test]
    fn harmonic_150hz_voiced_at_wb() {
        // 150 Hz harmonic mix @ 16 kHz → period ≈ 107 samples.
        let pcm = synth_harmonic(150.0, 16_000, 320, 0.3);
        let p = analyze_pitch(&pcm, OpusBandwidth::Wideband);
        assert!(p.voiced, "harmonic @ 150 Hz should be voiced (corr={})", p.correlation);
        // Lag = 16000 / 150 ≈ 106.67 internal samples.
        assert!(
            (p.lag_internal - 107).abs() <= 4,
            "expected lag ≈ 107, got {}",
            p.lag_internal
        );
    }

    #[test]
    fn white_noise_is_unvoiced() {
        // Deterministic "noise" via simple LCG.
        let mut s = 0x1234_5678u32;
        let pcm: Vec<f32> = (0..320)
            .map(|_| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                (s >> 16) as i16 as f32 / 32768.0 * 0.3
            })
            .collect();
        let p = analyze_pitch(&pcm, OpusBandwidth::Wideband);
        assert!(!p.voiced, "white noise should be unvoiced (corr={})", p.correlation);
    }

    #[test]
    fn lag_scales_across_bandwidths() {
        // Same 200 Hz sine at NB (8k), MB (12k), WB (16k) → lag
        // should scale with sample rate.
        let nb = synth_sine(200.0, 8_000, 160, 0.3);
        let mb = synth_sine(200.0, 12_000, 240, 0.3);
        let wb = synth_sine(200.0, 16_000, 320, 0.3);
        let pn = analyze_pitch(&nb, OpusBandwidth::Narrowband);
        let pm = analyze_pitch(&mb, OpusBandwidth::Mediumband);
        let pw = analyze_pitch(&wb, OpusBandwidth::Wideband);
        assert!(pn.voiced && pm.voiced && pw.voiced);
        // Expected: 40, 60, 80.
        assert!((pn.lag_internal - 40).abs() <= 2);
        assert!((pm.lag_internal - 60).abs() <= 4);
        assert!((pw.lag_internal - 80).abs() <= 4);
    }
}
