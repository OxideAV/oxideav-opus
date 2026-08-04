//! Encoder-side §5.3.1 pitch pre-filter — the reference listing's
//! pitch estimator (`pitch_downsample` / `pitch_search` /
//! `remove_doubling`) and the §4.3.7.1 comb filter applied "as the
//! inverse of the decoder's post-filter": the encoder combs the
//! pre-emphasized input with NEGATED gains and codes the §4.3.7.1
//! parameters (octave / fine period / 3-bit gain / tapset), so the
//! decoder's post-filter undoes the comb and the pitch harmonics ride
//! the filter instead of the coded spectrum.
//!
//! The pitch search follows §5.3.1's two criteria — continuity (the
//! previous period biases `remove_doubling`'s sub-multiple checks)
//! and avoidance of pitch multiples (the `T/k` sweep with the
//! `second_check` confirmation lags).
//!
//! ## Provenance
//!
//! RFC 6716 §4.3.7.1 / §5.3.1 + the §A embedded reference listing
//! (`pitch.c`, `celt_lpc.c`, `comb_filter`; staged
//! `docs/audio/opus/rfc6716-opus.txt`, hash-verified per §A.1). No
//! external library source was consulted.

use crate::celt_mdct_window::celt_overlap_window;
use crate::celt_post_filter::POST_FILTER_TAPS;

/// §4.3.7.1 comb-filter maximum period (the pre-filter keeps this
/// much unfiltered history).
pub const COMBFILTER_MAXPERIOD: usize = 1024;

/// §4.3.7.1 comb-filter minimum period.
pub const COMBFILTER_MINPERIOD: usize = 15;

/// The listing's `_celt_autocorr` (rectangular window / no overlap
/// taper — the pre-filter path passes `window == NULL`).
fn autocorr(x: &[f64], lag: usize) -> Vec<f64> {
    let n = x.len();
    let mut ac = vec![0.0f64; lag + 1];
    for (k, slot) in ac.iter_mut().enumerate() {
        let mut d = 0.0;
        for i in k..n {
            d += x[i] * x[i - k];
        }
        *slot = d;
    }
    ac
}

/// The listing's `_celt_lpc`: Levinson-Durbin over `p + 1`
/// autocorrelation values, bailing out at 30 dB prediction gain.
fn celt_lpc(ac: &[f64], p: usize) -> Vec<f64> {
    let mut lpc = vec![0.0f64; p];
    let mut error = ac[0];
    if ac[0] != 0.0 {
        for i in 0..p {
            let mut rr = 0.0;
            for j in 0..i {
                rr += lpc[j] * ac[i - j];
            }
            rr += ac[i + 1];
            let r = -rr / error;
            lpc[i] = r;
            for j in 0..((i + 1) >> 1) {
                let tmp1 = lpc[j];
                let tmp2 = lpc[i - 1 - j];
                lpc[j] = tmp1 + r * tmp2;
                lpc[i - 1 - j] = tmp2 + r * tmp1;
            }
            error -= r * r * error;
            if error < 0.001 * ac[0] {
                break;
            }
        }
    }
    lpc
}

/// The listing's `celt_fir`: `y[i] = x[i] + Σ num[j]·x[i-1-j]` with a
/// carried delay line.
fn celt_fir(x: &mut [f64], num: &[f64]) {
    let ord = num.len();
    let mut mem = vec![0.0f64; ord];
    for v in x.iter_mut() {
        let mut sum = *v;
        for (j, &c) in num.iter().enumerate() {
            sum += c * mem[j];
        }
        for j in (1..ord).rev() {
            mem[j] = mem[j - 1];
        }
        mem[0] = *v;
        *v = sum;
    }
}

/// The listing's `pitch_downsample`: 2× decimate (3-tap smoothing,
/// channels summed), whiten with a lag-windowed order-4 LPC (chirped
/// by 0.9 per tap), and apply a mild `1 + 0.8 z⁻¹` emphasis.
pub fn pitch_downsample(x: &[&[f64]], len: usize) -> Vec<f64> {
    let half = len >> 1;
    let mut x_lp = vec![0.0f64; half];
    for ch in x {
        for (i, slot) in x_lp.iter_mut().enumerate().take(half).skip(1) {
            *slot += 0.5 * (0.5 * (ch[2 * i - 1] + ch[2 * i + 1]) + ch[2 * i]);
        }
        x_lp[0] += 0.5 * (0.5 * ch[1] + ch[0]);
    }
    let mut ac = autocorr(&x_lp, 4);
    // Noise floor -40 dB + lag windowing.
    ac[0] *= 1.0001;
    for (i, v) in ac.iter_mut().enumerate().skip(1) {
        *v -= *v * (0.008 * i as f64) * (0.008 * i as f64);
    }
    let mut lpc = celt_lpc(&ac, 4);
    let mut tmp = 1.0f64;
    for c in lpc.iter_mut() {
        tmp *= 0.9;
        *c *= tmp;
    }
    celt_fir(&mut x_lp, &lpc);
    celt_fir(&mut x_lp, &[0.8]);
    x_lp
}

/// The listing's `find_best_pitch`: track the two lags maximising the
/// normalized squared correlation under a running `Syy`.
fn find_best_pitch(xcorr: &[f64], y: &[f64], len: usize, max_pitch: usize) -> [usize; 2] {
    let mut best_num = [-1.0f64; 2];
    let mut best_den = [0.0f64; 2];
    let mut best_pitch = [0usize, 1];
    let mut syy = 1.0f64;
    for &v in y.iter().take(len) {
        syy += v * v;
    }
    for (i, &xc) in xcorr.iter().enumerate().take(max_pitch) {
        if xc > 0.0 {
            let num = xc * xc;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best_pitch[1] = best_pitch[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best_pitch[0] = i;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best_pitch[1] = i;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        syy = syy.max(1.0);
    }
    best_pitch
}

/// The listing's `pitch_search`: coarse 4×-decimated correlation
/// sweep, a 2×-decimated refinement around the two best coarse lags,
/// and pseudo-interpolation of the winner. `x_lp` is the current
/// frame (2×-decimated), `y` the lag history + frame; returns the lag
/// at the 2×-decimated rate scaled back to full (even) resolution.
pub fn pitch_search(x_lp: &[f64], y: &[f64], len: usize, max_pitch: usize) -> usize {
    let x_lp4: Vec<f64> = (0..len >> 2).map(|j| x_lp[2 * j]).collect();
    let lag = len + max_pitch;
    let y_lp4: Vec<f64> = (0..lag >> 2).map(|j| y[2 * j]).collect();

    // Coarse search with 4x decimation.
    let mut xcorr4 = vec![0.0f64; max_pitch >> 2];
    for (i, slot) in xcorr4.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (j, &xv) in x_lp4.iter().enumerate() {
            sum += xv * y_lp4[i + j];
        }
        *slot = sum.max(-1.0);
    }
    let best4 = find_best_pitch(&xcorr4, &y_lp4, len >> 2, max_pitch >> 2);

    // Finer search with 2x decimation around the two candidates.
    let mut xcorr = vec![0.0f64; max_pitch >> 1];
    for (i, slot) in xcorr.iter_mut().enumerate() {
        let i_i = i as i64;
        if (i_i - 2 * best4[0] as i64).abs() > 2 && (i_i - 2 * best4[1] as i64).abs() > 2 {
            continue;
        }
        let mut sum = 0.0;
        for (j, &xv) in x_lp.iter().enumerate().take(len >> 1) {
            sum += xv * y[i + j];
        }
        *slot = sum.max(-1.0);
    }
    let best = find_best_pitch(&xcorr, y, len >> 1, max_pitch >> 1);

    // Refine by pseudo-interpolation.
    let offset: i64 = if best[0] > 0 && best[0] < (max_pitch >> 1) - 1 {
        let a = xcorr[best[0] - 1];
        let b = xcorr[best[0]];
        let c = xcorr[best[0] + 1];
        if (c - a) > 0.7 * (b - a) {
            1
        } else if (a - c) > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    (2 * best[0] as i64 - offset).max(0) as usize
}

/// The listing's sub-multiple confirmation lags.
const SECOND_CHECK: [usize; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];

/// The listing's `remove_doubling`: walk the sub-multiples `T/k` of
/// the detected lag, preferring a shorter period whose correlation
/// (with a confirmation lag and a continuity bonus near the previous
/// period) is strong enough, then pseudo-interpolate and return the
/// pitch gain.
pub fn remove_doubling(
    x: &[f64],
    maxperiod: usize,
    minperiod: usize,
    n: usize,
    t0_full: usize,
    prev_period: usize,
    prev_gain: f64,
) -> (usize, f64) {
    let minperiod0 = minperiod;
    let maxperiod = maxperiod / 2;
    let minperiod = minperiod / 2;
    let t0 = (t0_full / 2).min(maxperiod - 1);
    let prev_period = prev_period / 2;
    let n = n / 2;
    let x0 = maxperiod; // offset of the frame start within x

    let idx = |i: i64| -> f64 { x[(x0 as i64 + i) as usize] };

    let (mut xx, mut xy, mut yy) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n as i64 {
        xy += idx(i) * idx(i - t0 as i64);
        xx += idx(i) * idx(i);
        yy += idx(i - t0 as i64) * idx(i - t0 as i64);
    }
    let mut best_xy = xy;
    let mut best_yy = yy;
    let g0 = xy / (1.0 + xx * yy).sqrt();
    let mut g = g0;
    let mut t = t0;

    for (k, &check) in SECOND_CHECK.iter().enumerate().skip(2) {
        let t1 = (2 * t0 + k) / (2 * k);
        if t1 < minperiod {
            break;
        }
        let t1b = if k == 2 {
            if t1 + t0 > maxperiod {
                t0
            } else {
                t0 + t1
            }
        } else {
            (2 * check * t0 + k) / (2 * k)
        };
        let (mut xy1, mut yy1) = (0.0f64, 0.0f64);
        for i in 0..n as i64 {
            xy1 += idx(i) * idx(i - t1 as i64);
            yy1 += idx(i - t1 as i64) * idx(i - t1 as i64);
            xy1 += idx(i) * idx(i - t1b as i64);
            yy1 += idx(i - t1b as i64) * idx(i - t1b as i64);
        }
        let g1 = xy1 / (1.0 + 2.0 * xx * yy1).sqrt();
        let cont = if (t1 as i64 - prev_period as i64).abs() <= 1 {
            prev_gain
        } else if (t1 as i64 - prev_period as i64).abs() <= 2 && 5 * k * k < t0 {
            0.5 * prev_gain
        } else {
            0.0
        };
        if g1 > 0.3 + 0.4 * g0 - cont {
            best_xy = xy1;
            best_yy = yy1;
            t = t1;
            g = g1;
        }
    }

    let mut pg = if best_yy <= best_xy {
        1.0
    } else {
        best_xy / (best_yy + 1.0)
    };
    let mut xcorr = [0.0f64; 3];
    for (k, slot) in xcorr.iter_mut().enumerate() {
        let t1 = t as i64 + k as i64 - 1;
        let mut xy = 0.0;
        for i in 0..n as i64 {
            xy += idx(i) * idx(i - t1);
        }
        *slot = xy;
    }
    let offset: i64 = if (xcorr[2] - xcorr[0]) > 0.7 * (xcorr[1] - xcorr[0]) {
        1
    } else if (xcorr[0] - xcorr[2]) > 0.7 * (xcorr[1] - xcorr[2]) {
        -1
    } else {
        0
    };
    if pg > g {
        pg = g;
    }
    let t0_out = ((2 * t as i64 + offset).max(minperiod0 as i64)) as usize;
    (t0_out, pg)
}

/// The §4.3.7.1 comb filter (the listing's `comb_filter`), applied
/// in the encoder direction with NEGATED gains: `y[i] = x[i] +
/// Σ g·tap·x[i−T∓k]`, crossfading from the old `(T0, g0, tapset0)`
/// to the new `(T1, g1, tapset1)` over the first `overlap` samples
/// through the squared MDCT window. `x` carries `history` samples of
/// lookback ahead of the region filtered into `y`.
#[allow(clippy::too_many_arguments)]
pub fn comb_filter(
    y: &mut [f64],
    x: &[f64],
    x_offset: usize,
    t0: usize,
    t1: usize,
    n: usize,
    g0: f64,
    g1: f64,
    tapset0: u8,
    tapset1: u8,
) -> Result<(), crate::Error> {
    if x_offset < t0 + 2 || x_offset < t1 + 2 || x.len() < x_offset + n || y.len() < n {
        return Err(crate::Error::MalformedPacket);
    }
    let taps0 = POST_FILTER_TAPS[usize::from(tapset0.min(2))];
    let taps1 = POST_FILTER_TAPS[usize::from(tapset1.min(2))];
    let (g00, g01, g02) = (g0 * taps0[0], g0 * taps0[1], g0 * taps0[2]);
    let (g10, g11, g12) = (g1 * taps1[0], g1 * taps1[1], g1 * taps1[2]);
    let window = celt_overlap_window();
    let overlap = window.len().min(n);
    let at = |i: usize, d: i64| -> f64 { x[(x_offset as i64 + i as i64 + d) as usize] };
    for (i, slot) in y.iter_mut().enumerate().take(overlap) {
        let f = window[i] * window[i];
        let t0i = -(t0 as i64);
        let t1i = -(t1 as i64);
        *slot = at(i, 0)
            + (1.0 - f) * g00 * at(i, t0i)
            + (1.0 - f) * g01 * (at(i, t0i - 1) + at(i, t0i + 1))
            + (1.0 - f) * g02 * (at(i, t0i - 2) + at(i, t0i + 2))
            + f * g10 * at(i, t1i)
            + f * g11 * (at(i, t1i - 1) + at(i, t1i + 1))
            + f * g12 * (at(i, t1i - 2) + at(i, t1i + 2));
    }
    let t1i = -(t1 as i64);
    for (i, slot) in y.iter_mut().enumerate().take(n).skip(overlap) {
        *slot = at(i, 0)
            + g10 * at(i, t1i)
            + g11 * (at(i, t1i - 1) + at(i, t1i + 1))
            + g12 * (at(i, t1i - 2) + at(i, t1i + 2));
    }
    Ok(())
}

/// One frame's pre-filter decision (before the gain threshold).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrefilterAnalysis {
    /// Detected §4.3.7.1 period (15..=1022 after clamping).
    pub pitch_index: usize,
    /// Analysis pitch gain, scaled by the listing's 0.7 factor.
    pub gain: f64,
}

/// The listing's pitch-analysis half of `run_prefilter`: downsample
/// the (per-channel) `COMBFILTER_MAXPERIOD + N` pre-emphasized
/// history+frame buffers, search, and un-double, biased toward the
/// previous frame's period/gain for continuity (§5.3.1).
pub fn prefilter_analysis(
    pre: &[&[f64]],
    n: usize,
    prev_period: usize,
    prev_gain: f64,
) -> PrefilterAnalysis {
    let x_lp = pitch_downsample(pre, COMBFILTER_MAXPERIOD + n);
    let pitch = pitch_search(
        &x_lp[COMBFILTER_MAXPERIOD >> 1..],
        &x_lp,
        n,
        COMBFILTER_MAXPERIOD - COMBFILTER_MINPERIOD,
    );
    let pitch_index = COMBFILTER_MAXPERIOD - pitch;
    let (pitch_index, gain) = remove_doubling(
        &x_lp,
        COMBFILTER_MAXPERIOD,
        COMBFILTER_MINPERIOD,
        n,
        pitch_index,
        prev_period,
        prev_gain,
    );
    let pitch_index = pitch_index.min(COMBFILTER_MAXPERIOD - 2);
    PrefilterAnalysis {
        pitch_index,
        gain: 0.7 * gain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A strongly periodic pre-emphasized signal must be detected at
    /// (a divisor-corrected version of) its true period with a strong
    /// gain.
    #[test]
    fn periodic_signal_is_detected() {
        let n = 960usize;
        let period = 200.0f64;
        let buf: Vec<f64> = (0..COMBFILTER_MAXPERIOD + n)
            .map(|i| {
                let t = i as f64;
                6000.0 * (std::f64::consts::TAU * t / period).sin()
                    + 2000.0 * (std::f64::consts::TAU * 2.0 * t / period + 0.7).sin()
            })
            .collect();
        let a = prefilter_analysis(&[&buf], n, 0, 0.0);
        // The detected period is the fundamental or a near sub/super
        // multiple lattice point; for this two-harmonic signal the
        // fundamental wins.
        assert!(
            (a.pitch_index as f64 - period).abs() <= 2.0,
            "period {} vs {period}",
            a.pitch_index
        );
        assert!(a.gain > 0.4, "gain {}", a.gain);
    }

    /// Noise must not report a strong pitch gain.
    #[test]
    fn noise_reports_weak_gain() {
        let mut lcg = 1u32;
        let buf: Vec<f64> = (0..COMBFILTER_MAXPERIOD + 960)
            .map(|_| {
                lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((lcg >> 16) as f64 - 32768.0) / 8.0
            })
            .collect();
        let a = prefilter_analysis(&[&buf], 960, 0, 0.0);
        assert!(a.gain < 0.25, "noise gain {}", a.gain);
    }

    /// The comb filter with zero gains is the identity, and a comb
    /// followed by the decoder-direction comb with negated gain
    /// approximately cancels on the steady region.
    #[test]
    fn comb_roundtrip_cancels() {
        let n = 480usize;
        let hist = COMBFILTER_MAXPERIOD;
        let x: Vec<f64> = (0..hist + n)
            .map(|i| (i as f64 * 0.037).sin() * 1000.0 + (i as f64 * 0.011).cos() * 400.0)
            .collect();
        // Zero gain: identity.
        let mut y = vec![0.0f64; n];
        comb_filter(&mut y, &x, hist, 100, 100, n, 0.0, 0.0, 0, 0).unwrap();
        for (a, b) in y.iter().zip(&x[hist..]) {
            assert!((a - b).abs() < 1e-12);
        }
        // Forward FIR comb with -g, then the decoder-direction IIR
        // comb with +g (feeding back its own recovered output — the
        // §4.3.7.1 post-filter structure): exact inverse up to fp
        // roundoff on the steady region.
        let g = 0.5f64;
        let t = 100usize;
        let mut fwd = x.clone();
        let mut yf = vec![0.0f64; n];
        comb_filter(&mut yf, &x, hist, t, t, n, -g, -g, 0, 0).unwrap();
        fwd[hist..].copy_from_slice(&yf);
        let taps = POST_FILTER_TAPS[0];
        let mut rec = x.clone();
        for i in 0..n {
            let p = hist + i;
            rec[p] = fwd[p]
                + g * taps[0] * rec[p - t]
                + g * taps[1] * (rec[p - t - 1] + rec[p - t + 1])
                + g * taps[2] * (rec[p - t - 2] + rec[p - t + 2]);
        }
        for i in 0..n {
            assert!(
                (rec[hist + i] - x[hist + i]).abs() < 1e-6,
                "sample {i}: {} vs {}",
                rec[hist + i],
                x[hist + i]
            );
        }
    }
}
