//! Encoder-side §4.3.4.5 time-frequency analysis — the reference
//! listing's `tf_analysis`: choose each band's `tf_change` flag by
//! measuring, per band, which Haar merge/split level minimises an
//! L1-sparsity metric of the normalized coefficients, then smooth the
//! per-band choices with a Viterbi pass whose transition cost (λ,
//! sized by the frame's byte budget) charges flag flips — flips cost
//! coded bits, so the smoothing trades metric gain against rate.
//!
//! ## Provenance
//!
//! RFC 6716 §4.3.4.5 / §5.3 + the §A embedded reference listing
//! (`tf_analysis` / `l1_metric` / `haar1`; staged
//! `docs/audio/opus/rfc6716-opus.txt`, hash-verified per §A.1). No
//! external library source was consulted.

use crate::celt_band_layout::CELT_NUM_BANDS;
use crate::celt_rate_alloc::band_edge;
use crate::celt_tf_adjust::{TF_ADJ_NONTRANSIENT_SELECT0, TF_ADJ_TRANSIENT_SELECT0};

/// One in-place Haar butterfly level: `stride` interleaved lanes,
/// pairs `(2j, 2j+1)` within each lane combined as
/// `(x+y, x−y)/√2` (the listing's `haar1`).
fn haar1(x: &mut [f64], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    const INVSQRT2: f64 = std::f64::consts::FRAC_1_SQRT_2;
    for i in 0..stride {
        for j in 0..n0 {
            let a = INVSQRT2 * x[stride * 2 * j + i];
            let b = INVSQRT2 * x[stride * (2 * j + 1) + i];
            x[stride * 2 * j + i] = a + b;
            x[stride * (2 * j + 1) + i] = a - b;
        }
    }
}

/// The listing's `l1_metric`: sum over the `2^lm` interleaved lanes of
/// each lane's L2 norm, scaled by `1/√(2^lm)` and inflated by a
/// width-dependent bias per Haar level (narrow bands pay more per
/// level).
fn l1_metric(tmp: &[f64], n: usize, lm: usize, width: usize) -> f64 {
    let lanes = 1usize << lm;
    let mut l1 = 0.0f64;
    for i in 0..lanes {
        let mut l2 = 0.0f64;
        let mut j = 0;
        while (j << lm) + i < n {
            let v = tmp[(j << lm) + i];
            l2 += v * v;
            j += 1;
        }
        l1 += l2.sqrt();
    }
    l1 /= (lanes as f64).sqrt();
    let bias = match width {
        1 => 0.12,
        2 => 0.05,
        _ => 0.02,
    } * lm as f64;
    l1 + bias * l1
}

/// The Viterbi transition/target values: the §4.3.4.5 TF adjustment
/// each `(tf_select, curr)` pair produces (the listing's
/// `tf_select_table[LM][4*isTransient + 2*tf_select + curr]`; the
/// analysis always emits `tf_select = 0`, so only the select-0 halves
/// are consulted).
fn tf_target(lm: usize, is_transient: bool, curr: usize) -> i32 {
    let t = if is_transient {
        TF_ADJ_TRANSIENT_SELECT0[lm][curr]
    } else {
        TF_ADJ_NONTRANSIENT_SELECT0[lm][curr]
    };
    i32::from(t)
}

/// Per-band `tf_change` analysis over the normalized coefficients
/// `x` (channel planes of `plane` values each, band `i` spanning
/// `m·edge(i)..m·edge(i+1)`). Returns the per-band 0/1 flags for
/// `start..end` (other bands zero); `tf_select` is always 0, exactly
/// as the listing's analysis emits.
#[allow(clippy::too_many_arguments)]
pub fn tf_analysis(
    x: &[f64],
    plane: usize,
    channels: usize,
    start: usize,
    end: usize,
    is_transient: bool,
    effective_bytes: i64,
    lm: usize,
) -> [i32; CELT_NUM_BANDS] {
    let mut tf_res = [0i32; CELT_NUM_BANDS];
    let len = end - start;
    if len == 0 {
        return tf_res;
    }
    // Starved frames: every band takes the transient default.
    if effective_bytes < 15 * channels as i64 {
        for slot in tf_res.iter_mut().take(end).skip(start) {
            *slot = i32::from(is_transient);
        }
        return tf_res;
    }
    let lambda = if effective_bytes < 40 {
        12
    } else if effective_bytes < 60 {
        6
    } else if effective_bytes < 100 {
        4
    } else {
        3
    };

    let m = 1usize << lm;
    let mut metric = vec![0i32; len];
    for (idx, band) in (start..end).enumerate() {
        let lo = m * band_edge(band) as usize;
        let hi = m * band_edge(band + 1) as usize;
        let n = hi - lo;
        let mut tmp: Vec<f64> = x[lo..hi].to_vec();
        if channels == 2 {
            for (j, slot) in tmp.iter_mut().enumerate() {
                *slot += x[plane + lo + j];
            }
        }
        let mut best_l1 = l1_metric(&tmp, n, if is_transient { lm } else { 0 }, n >> lm);
        let mut best_level = 0i32;
        for k in 0..lm {
            let b = if is_transient { lm - k - 1 } else { k + 1 };
            if is_transient {
                haar1(&mut tmp, n >> (lm - k), 1 << (lm - k));
            } else {
                haar1(&mut tmp, n >> k, 1 << k);
            }
            let l1 = l1_metric(&tmp, n, b, n >> lm);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = k as i32 + 1;
            }
        }
        metric[idx] = if is_transient {
            best_level
        } else {
            -best_level
        };
    }

    // Viterbi forward pass over the flip costs.
    let mut path0 = vec![0i32; len];
    let mut path1 = vec![0i32; len];
    let mut cost0 = 0i32;
    let mut cost1 = if is_transient { 0 } else { lambda };
    for i in 1..len {
        let (curr0, p0) = {
            let from0 = cost0;
            let from1 = cost1 + lambda;
            if from0 < from1 {
                (from0, 0)
            } else {
                (from1, 1)
            }
        };
        let (curr1, p1) = {
            let from0 = cost0 + lambda;
            let from1 = cost1;
            if from0 < from1 {
                (from0, 0)
            } else {
                (from1, 1)
            }
        };
        path0[i] = p0;
        path1[i] = p1;
        cost0 = curr0 + (metric[i] - tf_target(lm, is_transient, 0)).abs();
        cost1 = curr1 + (metric[i] - tf_target(lm, is_transient, 1)).abs();
    }
    let mut flags = vec![0i32; len];
    flags[len - 1] = i32::from(cost0 >= cost1);
    // Backward pass.
    for i in (0..len.saturating_sub(1)).rev() {
        flags[i] = if flags[i + 1] == 1 {
            path1[i + 1]
        } else {
            path0[i + 1]
        };
    }
    for (idx, band) in (start..end).enumerate() {
        tf_res[band] = flags[idx];
    }
    tf_res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haar1_is_orthonormal_per_pair() {
        let mut x = vec![3.0, 1.0, -2.0, 4.0];
        haar1(&mut x, 4, 1);
        let s = std::f64::consts::FRAC_1_SQRT_2;
        assert!((x[0] - s * 4.0).abs() < 1e-12);
        assert!((x[1] - s * 2.0).abs() < 1e-12);
        assert!((x[2] - s * 2.0).abs() < 1e-12);
        assert!((x[3] - s * -6.0).abs() < 1e-12);
    }

    #[test]
    fn starved_frames_default_to_the_transient_flag() {
        let x = vec![0.0; 2 * 8 * 100];
        let r = tf_analysis(&x, 800, 2, 0, 21, true, 20, 3);
        assert!(r[..21].iter().all(|&v| v == 1));
        let r = tf_analysis(&x, 800, 2, 0, 21, false, 20, 3);
        assert!(r[..21].iter().all(|&v| v == 0));
    }

    #[test]
    fn flat_content_keeps_flags_zero() {
        // A time-flat tone concentrates in frequency: on a
        // non-transient frame no merge helps, flags stay 0.
        let m = 8usize;
        let plane = m * band_edge(CELT_NUM_BANDS) as usize;
        let mut x = vec![0.0f64; plane];
        // One strong coefficient per band (already maximally sparse).
        for band in 0..CELT_NUM_BANDS {
            let lo = m * band_edge(band) as usize;
            x[lo] = 1.0;
        }
        let r = tf_analysis(&x, plane, 1, 0, 21, false, 200, 3);
        assert!(r[..21].iter().all(|&v| v == 0), "{r:?}");
    }

    #[test]
    fn pairwise_correlated_content_flags_a_merge() {
        // Non-transient frame whose coefficients come in equal
        // adjacent pairs: one Haar level turns each pair (v, v) into
        // (v·√2, 0) — a strict L1 gain — so the analysis must flag
        // tf_change = 1 across the bands.
        let lm = 3usize;
        let m = 1usize << lm;
        let plane = m * band_edge(CELT_NUM_BANDS) as usize;
        let mut x = vec![0.0f64; plane];
        let mut lcg = 9876u32;
        for band in 0..21 {
            let lo = m * band_edge(band) as usize;
            let hi = m * band_edge(band + 1) as usize;
            let mut j = lo;
            while j + 1 < hi {
                lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let v = ((lcg >> 16) as f64 / 32768.0) - 1.0;
                x[j] = v;
                x[j + 1] = v;
                j += 2;
            }
        }
        let r = tf_analysis(&x, plane, 1, 0, 21, false, 200, lm);
        assert!(r[..21].iter().any(|&v| v == 1), "{r:?}");
    }
}
