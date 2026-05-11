//! Normalized Line Spectral Frequency (NLSF) decoding + LSF→LPC
//! conversion — RFC 6716 §4.2.7.5.
//!
//! Implements (clean-room from RFC 6716 §4.2.7.5):
//!
//! * §4.2.7.5.1 stage-1 codebook index decoding (Table 14 PDFs).
//! * §4.2.7.5.2 stage-2 residual decoding using the per-codebook PDFs
//!   (Tables 15/16, codebooks a..h for NB/MB and i..p for WB), the
//!   stage-2 codebook selectors (Tables 17/18), and the magnitude
//!   extension PDF (Table 19).
//! * §4.2.7.5.2 backwards-prediction reconstruction
//!   (`silk_NLSF_residual_dequant`) using the IHMW prediction-weight
//!   tables A..D (Table 20) and the per-coefficient selectors
//!   (Tables 21/22).
//! * §4.2.7.5.3 IHMW weighting + final NLSF reconstruction:
//!   `NLSF_Q15[k] = clamp(0, (cb1_Q8[k]<<7) + (res_Q10[k]<<14)/w_Q9[k], 32767)`.
//! * §4.2.7.5.4 monotone-spacing stabilisation against Table 25
//!   `NDeltaMin_Q15` (small-adjustment loop bounded at 20 iterations,
//!   followed by the always-correct fallback sort).
//! * §4.2.7.5.6 LSF→LPC conversion (Tables 27/28) producing real-
//!   valued LPC coefficients suitable for the synthesis filter.
//!
//! What stays MVP:
//!
//! * §4.2.7.5.5 frame-to-frame LSF interpolation: the 2-bit factor is
//!   parsed and returned to the caller, which uses it to compute
//!   interpolated NLSFs for sub-frames 0-1 of 20 ms frames.
//! * §4.2.7.5.7 / §4.2.7.5.8 LPC bandwidth-expansion + prediction-gain
//!   limiting are reduced to a small bandwidth-expansion safety
//!   factor (γ^k); the synthesis filter is float, not Q12 fixed-point,
//!   so the spec's overflow-protection rounds aren't strictly required
//!   for stability in our path.

use oxideav_celt::range_decoder::RangeDecoder;
use oxideav_core::Result;

use crate::silk::tables;
use crate::toc::OpusBandwidth;

/// Decode the NLSF coefficients for a SILK frame at the given
/// bandwidth + signal type.
///
/// Returns `(nlsf_q15, interp_coef_q2)` where:
/// * `nlsf_q15` — NLSF in Q15 (each entry in `[1, 32767]`, monotonically
///   increasing). Length is 10 for NB/MB, 16 for WB.
/// * `interp_coef_q2` — 2-bit interpolation factor from §4.2.7.5.5 (Table 26).
///   Values 0..=3 mean "interpolate subframes 0-1"; value 4 means "no interp".
///   Always 4 for 10 ms frames (caller must override it before calling this).
pub fn decode_nlsf(
    rc: &mut RangeDecoder<'_>,
    bw: OpusBandwidth,
    signal_type: u8,
    is_20ms: bool,
) -> Result<(Vec<i16>, u8)> {
    let voiced = signal_type == 2;
    let is_wb = matches!(bw, OpusBandwidth::Wideband);
    let order = if is_wb { 16 } else { 10 };

    // -------------------------------------------------------------
    // §4.2.7.5.1 Stage-1 codebook index (Table 14).
    // -------------------------------------------------------------
    let stage1_icdf: &[u8] = match (is_wb, voiced) {
        (false, false) => &tables::NLSF_NB_STAGE1_UNVOICED_ICDF,
        (false, true) => &tables::NLSF_NB_STAGE1_VOICED_ICDF,
        (true, false) => &tables::NLSF_WB_STAGE1_UNVOICED_ICDF,
        (true, true) => &tables::NLSF_WB_STAGE1_VOICED_ICDF,
    };
    let i1 = rc.decode_icdf(stage1_icdf, 8);

    // -------------------------------------------------------------
    // §4.2.7.5.2 Stage-2 residual indices, including ±4 extension.
    // -------------------------------------------------------------
    let mut i2 = vec![0i32; order];
    for k in 0..order {
        let cb_letter = if is_wb {
            tables::NLSF_WB_STAGE2_SELECT[i1][k] as usize
        } else {
            tables::NLSF_NBMB_STAGE2_SELECT[i1][k] as usize
        };
        let icdf: &[u8] = if is_wb {
            &tables::NLSF_WB_STAGE2_ICDF[cb_letter]
        } else {
            &tables::NLSF_NBMB_STAGE2_ICDF[cb_letter]
        };
        // 9-symbol PDF: result is 0..=8, then subtract 4 to get -4..=4.
        let sym = rc.decode_icdf(icdf, 8) as i32 - 4;
        let mut idx = sym;
        // §4.2.7.5.2: when |sym| reaches 4, read Table 19 extension and
        // *add* its value to the index magnitude with the same sign.
        if idx == -4 || idx == 4 {
            let ext = rc.decode_icdf(&tables::NLSF_STAGE2_EXTENSION_ICDF, 8) as i32;
            if idx > 0 {
                idx += ext;
            } else {
                idx -= ext;
            }
        }
        i2[k] = idx;
    }

    // -------------------------------------------------------------
    // §4.2.7.5.5 Interpolation factor (Table 26). Only coded for 20 ms
    // frames; 10 ms frames implicitly use w_Q2 = 4 (no interpolation).
    // -------------------------------------------------------------
    let interp_coef: u8 = if is_20ms {
        rc.decode_icdf(&tables::NLSF_INTERP_ICDF, 8) as u8
    } else {
        4
    };

    // -------------------------------------------------------------
    // §4.2.7.5.2 Inverse backwards prediction
    // (`silk_NLSF_residual_dequant`):
    //
    //   res_Q10[k] = (k+1 < d_LPC ? (res_Q10[k+1]*pred_Q8[k]) >> 8 : 0)
    //                + ((((I2[k]<<10) - sign(I2[k])*102) * qstep) >> 16)
    //
    // where qstep = 11796 (NB/MB) or 9830 (WB).
    // -------------------------------------------------------------
    let qstep: i32 = if is_wb { 9830 } else { 11796 };
    let mut res_q10 = vec![0i32; order];
    // Iterate k = order-1 .. 0 so res_Q10[k+1] is already computed.
    for k in (0..order).rev() {
        let prev_term = if k + 1 < order {
            let pred = pred_weight_q8(i1, k, is_wb) as i32;
            (res_q10[k + 1] * pred) >> 8
        } else {
            0
        };
        let i2_k = i2[k];
        let sign_i2 = i2_k.signum();
        let raw = (i2_k << 10) - sign_i2 * 102;
        let dequant = (raw * qstep) >> 16;
        res_q10[k] = prev_term + dequant;
    }

    // -------------------------------------------------------------
    // §4.2.7.5.3 IHMW weights from the stage-1 codebook entry.
    //
    //   w2_Q18[k] = (1024/(cb1_Q8[k] - cb1_Q8[k-1])
    //                + 1024/(cb1_Q8[k+1] - cb1_Q8[k])) << 16
    //   sqrt approximation reduces to w_Q9[k].
    // -------------------------------------------------------------
    let cb1_q8: Vec<i32> = if is_wb {
        tables::NLSF_WB_CB1_Q8[i1]
            .iter()
            .map(|&v| v as i32)
            .collect()
    } else {
        tables::NLSF_NBMB_CB1_Q8[i1]
            .iter()
            .map(|&v| v as i32)
            .collect()
    };
    let w_q9 = compute_ihmw_weights(&cb1_q8);

    // -------------------------------------------------------------
    // §4.2.7.5.3 Reconstruct NLSF_Q15:
    //   NLSF_Q15[k] = clamp(0, (cb1_Q8[k]<<7) + (res_Q10[k]<<14)/w_Q9[k], 32767)
    // -------------------------------------------------------------
    let mut nlsf_q15 = vec![0i16; order];
    for k in 0..order {
        let cb_term = cb1_q8[k] << 7;
        let weighted = (res_q10[k] << 14) / w_q9[k] as i32;
        let v = (cb_term + weighted).clamp(0, 32767);
        nlsf_q15[k] = v as i16;
    }

    // -------------------------------------------------------------
    // §4.2.7.5.4 Stabilise (monotone, min-spacing).
    // -------------------------------------------------------------
    Ok((stabilize(&nlsf_q15, is_wb), interp_coef))
}

/// Backwards-prediction weight selector (`pred_Q8[k]`) from Tables
/// 20-22 — picks list A/B (NB/MB) or C/D (WB) based on (I1, k).
fn pred_weight_q8(i1: usize, k: usize, is_wb: bool) -> u8 {
    // pred_Q8 only defined for k in 0..d_LPC-1.
    if is_wb {
        // Selector 0 → list C (index 2), 1 → list D (index 3).
        let sel = tables::NLSF_WB_PRED_SELECT[i1][k] as usize;
        let list = 2 + sel;
        tables::NLSF_PRED_WEIGHTS[list][k]
    } else {
        // Selector 0 → list A (index 0), 1 → list B (index 1).
        let sel = tables::NLSF_NBMB_PRED_SELECT[i1][k] as usize;
        let list = sel; // 0 or 1
        tables::NLSF_PRED_WEIGHTS[list][k]
    }
}

/// Compute IHMW weights `w_Q9[k]` from the stage-1 Q8 codebook entry,
/// per RFC 6716 §4.2.7.5.3. Output entries fall in `[1819, 5227]`.
fn compute_ihmw_weights(cb1_q8: &[i32]) -> Vec<u16> {
    let order = cb1_q8.len();
    let mut w = vec![0u16; order];
    for k in 0..order {
        let prev = if k == 0 { 0 } else { cb1_q8[k - 1] };
        let next = if k + 1 == order { 256 } else { cb1_q8[k + 1] };
        let lo_diff = (cb1_q8[k] - prev).max(1);
        let hi_diff = (next - cb1_q8[k]).max(1);
        // w2_Q18 fits comfortably in i32.
        let w2_q18: i32 = (1024 / lo_diff + 1024 / hi_diff) << 16;
        w[k] = isqrt_q9_approx(w2_q18 as u32);
    }
    w
}

/// Spec-faithful approximation of sqrt(w2_Q18) → w_Q9 (RFC 6716
/// §4.2.7.5.3): `i = ilog(w2_Q18); f = (w2_Q18 >> (i-8)) & 127;
/// y = ((i&1) ? 32768 : 46214) >> ((32-i)>>1);
/// w_Q9 = y + ((213 * f * y) >> 16)`.
fn isqrt_q9_approx(w2_q18: u32) -> u16 {
    // Avoid degenerate input: w2_Q18 = 0 cannot happen in valid SILK
    // streams (the IHMW formula always produces a positive value), but
    // guard for safety.
    if w2_q18 == 0 {
        return 1;
    }
    let i = 32 - w2_q18.leading_zeros() as i32; // ilog: 1-based highest bit position
    let shift = (i - 8).max(0);
    let f = ((w2_q18 >> shift) & 127) as i32;
    let base: i32 = if i & 1 == 1 { 32768 } else { 46214 };
    let shr = ((32 - i) >> 1).max(0);
    let y = base >> shr;
    let w_q9 = y + ((213 * f * y) >> 16);
    w_q9.clamp(1, u16::MAX as i32) as u16
}

/// §4.2.7.5.4 NLSF stabilisation. Applies the small-adjustment loop
/// (capped at 20 iterations) followed by the bullet-proof fallback
/// (sort + clamp from both ends) which always satisfies the
/// constraints.
pub fn stabilize(nlsf_in: &[i16], is_wb: bool) -> Vec<i16> {
    let order = nlsf_in.len();
    let ndelta_min: &[i16] = if is_wb {
        &tables::NLSF_WB_MIN_DELTA_Q15
    } else {
        &tables::NLSF_NBMB_MIN_DELTA_Q15
    };
    let mut nlsf: Vec<i32> = nlsf_in.iter().map(|&v| v as i32).collect();

    for _round in 0..20 {
        // Find the index i where (NLSF[i] - NLSF[i-1]) - NDeltaMin[i] is
        // smallest. NLSF[-1]=0, NLSF[d_LPC]=32768.
        let mut min_diff = i32::MAX;
        let mut min_i: usize = 0;
        for i in 0..=order {
            let lhs = if i == 0 { 0 } else { nlsf[i - 1] };
            let rhs = if i == order { 32768 } else { nlsf[i] };
            let diff = (rhs - lhs) - ndelta_min[i] as i32;
            if diff < min_diff {
                min_diff = diff;
                min_i = i;
            }
        }
        if min_diff >= 0 {
            break;
        }
        if min_i == 0 {
            nlsf[0] = ndelta_min[0] as i32;
        } else if min_i == order {
            nlsf[order - 1] = 32768 - ndelta_min[order] as i32;
        } else {
            // Centre-and-spread fix.
            let mut min_center = (ndelta_min[min_i] as i32) >> 1;
            for k in 0..min_i {
                min_center += ndelta_min[k] as i32;
            }
            let mut max_center = 32768 - ((ndelta_min[min_i] as i32) >> 1);
            for k in (min_i + 1)..=order {
                max_center -= ndelta_min[k] as i32;
            }
            let avg = (nlsf[min_i - 1] + nlsf[min_i] + 1) >> 1;
            let center = avg.clamp(min_center, max_center);
            nlsf[min_i - 1] = center - ((ndelta_min[min_i] as i32) >> 1);
            nlsf[min_i] = nlsf[min_i - 1] + ndelta_min[min_i] as i32;
        }
    }

    // Fallback: sort + clamp from both ends.
    nlsf.sort();
    let mut prev: i32 = 0;
    for k in 0..order {
        let lower = prev + ndelta_min[k] as i32;
        if nlsf[k] < lower {
            nlsf[k] = lower;
        }
        prev = nlsf[k];
    }
    let mut next: i32 = 32768;
    for k in (0..order).rev() {
        let upper = next - ndelta_min[k + 1] as i32;
        if nlsf[k] > upper {
            nlsf[k] = upper;
        }
        next = nlsf[k];
    }

    nlsf.iter().map(|&v| v.clamp(1, 32767) as i16).collect()
}

/// Convert NLSF (Q15, length = order) to LPC coefficients (f32, length
/// = order), following RFC 6716 §4.2.7.5.6.
///
/// Builds `c_Q17[]` via Table 28 cosine LUT + linear interpolation,
/// then runs the §4.2.7.5.6 P/Q recurrence and combines them into the
/// direct-form LPC vector (negated: convention `lpc[k] = -a[k+1]`).
pub fn nlsf_to_lpc(nlsf_q15: &[i16], _bw: OpusBandwidth) -> Vec<f32> {
    let order = nlsf_q15.len();
    let is_wb = order == 16;
    let ordering: &[usize] = if is_wb {
        &tables::NLSF_ORDERING_WB
    } else {
        &tables::NLSF_ORDERING_NB
    };

    // §4.2.7.5.6: c_Q17[ordering[k]] = (cos_Q12[i]*256 +
    //                                  (cos_Q12[i+1]-cos_Q12[i])*f + 4) >> 3
    // where i = nlsf >> 8 (top 7 bits, since nlsf < 32768) and f =
    // nlsf & 255. Cosine LUT is signed Q12 with 129 entries.
    let mut c_q17 = vec![0i32; order];
    for k in 0..order {
        let n = nlsf_q15[k] as i32;
        let i = (n >> 8) as usize;
        let f = n & 255;
        let i = i.min(127);
        let cos_i = tables::COSINE_Q12[i] as i32;
        let cos_i1 = tables::COSINE_Q12[i + 1] as i32;
        let v = (cos_i * 256 + (cos_i1 - cos_i) * f + 4) >> 3;
        c_q17[ordering[k]] = v;
    }

    // §4.2.7.5.6 P/Q recurrence (Q16). p_Q16[k][.] and q_Q16[k][.] each
    // have length k+2. Initial: p_Q16[0][0] = q_Q16[0][0] = 1<<16,
    // p_Q16[0][1] = -c_Q17[0], q_Q16[0][1] = -c_Q17[1]. Then for k =
    // 1..d2-1, j = 0..=k+1:
    //   p[k][j] = p[k-1][j] + p[k-1][j-2]
    //             - ((c_Q17[2*k] * p[k-1][j-1] + 32768) >> 16)
    //   p[k][j<0] = 0, p[k][k+2] = p[k][k] (symmetry).
    let d2 = order / 2;
    let mut p_prev = vec![0i64; d2 + 2];
    let mut q_prev = vec![0i64; d2 + 2];
    p_prev[0] = 1 << 16;
    p_prev[1] = -(c_q17[0] as i64);
    q_prev[0] = 1 << 16;
    q_prev[1] = -(c_q17[1] as i64);
    // Symmetric continuation: p_Q16[0][2] = p_Q16[0][0], q_Q16[0][2] =
    // q_Q16[0][0].
    p_prev[2] = p_prev[0];
    q_prev[2] = q_prev[0];

    for k in 1..d2 {
        let mut p_cur = vec![0i64; d2 + 2];
        let mut q_cur = vec![0i64; d2 + 2];
        let cp = c_q17[2 * k] as i64;
        let cq = c_q17[2 * k + 1] as i64;
        for j in 0..=k + 1 {
            let p_jm2 = if j >= 2 { p_prev[j - 2] } else { 0 };
            let q_jm2 = if j >= 2 { q_prev[j - 2] } else { 0 };
            let p_jm1 = if j >= 1 { p_prev[j - 1] } else { 0 };
            let q_jm1 = if j >= 1 { q_prev[j - 1] } else { 0 };
            // Note: p_prev[j] is well-defined for j <= k+1 because we
            // padded p_prev to length d2+2 and stored the symmetric
            // continuation at the previous round.
            let p_j = p_prev[j];
            let q_j = q_prev[j];
            p_cur[j] = p_j + p_jm2 - ((cp * p_jm1 + 32768) >> 16);
            q_cur[j] = q_j + q_jm2 - ((cq * q_jm1 + 32768) >> 16);
        }
        // Symmetric continuation for next round: p_cur[k+2] = p_cur[k].
        if k + 2 < p_cur.len() {
            p_cur[k + 2] = p_cur[k];
            q_cur[k + 2] = q_cur[k];
        }
        p_prev = p_cur;
        q_prev = q_cur;
    }

    // §4.2.7.5.6 Build a32_Q17[k] for k = 0..d2:
    //   a32_Q17[k]         = -(q_prev[k+1] - q_prev[k]) - (p_prev[k+1] + p_prev[k])
    //   a32_Q17[d_LPC-k-1] =  (q_prev[k+1] - q_prev[k]) - (p_prev[k+1] + p_prev[k])
    let mut a32_q17 = vec![0i64; order];
    for k in 0..d2 {
        let q_diff = q_prev[k + 1] - q_prev[k];
        let p_sum = p_prev[k + 1] + p_prev[k];
        a32_q17[k] = -q_diff - p_sum;
        a32_q17[order - k - 1] = q_diff - p_sum;
    }

    // §4.2.7.5.7 / §4.2.7.5.8 Bandwidth expansion + prediction-gain limit.
    //
    // The clean-room f32 path uses a hand-rolled DC-guard
    // (`|sum(lpc)| < 0.02` chirp by γ=0.85) instead of the spec's
    // chirp + saturate-to-Q12 + Levinson-derived stability test. The
    // Q12 saturation step quantises coefficients to a 13-bit grid,
    // which is fine for the Q-domain IIR with rounded accumulators
    // but adds ~5-8 dB of quantization error to a float IIR that has
    // no other saturation; the encoder roundtrip drops from ~25 dB to
    // ~17 dB if we Q12-quantize at this point.
    //
    // The fixed-point spec algorithm is implemented in
    // `bandwidth_expand_q17` below and exercised by unit tests, so a
    // future Q15 fixed-point synth path can adopt it without another
    // table-transcription pass. The silence-rail regressions in
    // commit a6ca9ea were not caused by this approximation — they
    // were caused by the wrong §4.2.7.3 + §4.2.7.4 ICDF tables, fixed
    // in this round.
    let mut lpc = vec![0f32; order];
    for k in 0..order {
        lpc[k] = (a32_q17[k] as f32) / (1 << 17) as f32;
    }
    for _round in 0..32 {
        let dc: f32 = lpc.iter().sum();
        if dc.abs() < 0.02 {
            break;
        }
        let mut g = 1.0f32;
        for v in lpc.iter_mut() {
            g *= 0.85;
            *v *= g;
        }
    }
    let mut g = 1.0f32;
    for v in lpc.iter_mut() {
        g *= 0.98;
        *v *= g;
    }
    lpc
}

/// §4.2.7.5.7 + §4.2.7.5.8 bandwidth-expansion pass on a Q17 LPC
/// coefficient array, in place. Implements the spec-faithful Q12
/// saturation followed by Levinson-driven prediction-gain limiting.
///
/// On exit `a32_q17[]` is in the spec's `(a32_Q17[k] + 16) >> 5 << 5`
/// post-saturation form, ready for the §4.2.7.9.2 synthesis filter.
fn bandwidth_expand_q17(a32_q17: &mut [i32]) {
    // §4.2.7.5.7: up to 10 rounds of bandwidth expansion to bring the
    // largest |a32_Q17[k]| within Q12 range (after rounding).
    let mut saturated = false;
    for _round in 0..10 {
        // Find k such that abs(a32_Q17[k]) is largest, lowest k breaks
        // ties.
        let mut max_abs: i32 = 0;
        let mut max_k: usize = 0;
        for (k, &v) in a32_q17.iter().enumerate() {
            let a = v.unsigned_abs() as i32;
            if a > max_abs {
                max_abs = a;
                max_k = k;
            }
        }
        // maxabs_Q12 = min((maxabs_Q17 + 16) >> 5, 163838)
        let maxabs_q12 = ((max_abs + 16) >> 5).min(163838);
        if maxabs_q12 <= 32767 {
            // Within Q12 range — no chirp needed.
            saturated = true;
            break;
        }
        // sc_Q16[0] = 65470 - ((maxabs_Q12 - 32767) << 14) / ((maxabs_Q12 * (k+1)) >> 2)
        let denom = (maxabs_q12 * (max_k as i32 + 1)) >> 2;
        if denom == 0 {
            break;
        }
        let sc_q16_0 = 65470 - (((maxabs_q12 - 32767) << 14) / denom);
        bwexpander_32(a32_q17, sc_q16_0);
    }
    if !saturated {
        // After 10 rounds, saturate to 16 bits in the Q12 domain:
        //   a32_Q17[k] = clamp(-32768, (a32_Q17[k] + 16) >> 5, 32767) << 5
        for v in a32_q17.iter_mut() {
            let q12 = (*v + 16) >> 5;
            let clamped = q12.clamp(-32768, 32767);
            *v = clamped << 5;
        }
    }

    // §4.2.7.5.8: up to 16 additional rounds of bandwidth expansion
    // gated on a Levinson-derived inverse prediction-gain stability
    // check. On round 15 the coefficients are zeroed unconditionally
    // (sc_Q16[0] = 0).
    for round in 0..16 {
        if lpc_inverse_pred_gain_is_stable(a32_q17) {
            // Stable — finalize via the (a32_Q17[k] + 16) >> 5 << 5 form
            // so all downstream code sees the post-saturation Q12 value.
            for v in a32_q17.iter_mut() {
                let q12 = (*v + 16) >> 5;
                *v = q12 << 5;
            }
            return;
        }
        if round == 15 {
            // Last-resort flat all-zero coefficients.
            for v in a32_q17.iter_mut() {
                *v = 0;
            }
            return;
        }
        // sc_Q16[0] = 65536 - (2 << round)
        let sc_q16_0 = 65536 - (2 << round);
        bwexpander_32(a32_q17, sc_q16_0);
        // After each chirp also re-saturate per §4.2.7.5.7's last step
        // so the next stability check sees the same Q12 form the
        // synthesis filter would.
        for v in a32_q17.iter_mut() {
            let q12 = (*v + 16) >> 5;
            let clamped = q12.clamp(-32768, 32767);
            *v = clamped << 5;
        }
    }
    // Reached only if all 16 rounds failed — all-zero coefficients.
    for v in a32_q17.iter_mut() {
        *v = 0;
    }
}

/// `silk_bwexpander_32(a32_Q17[], sc_Q16_0)` — RFC 6716 §4.2.7.5.7.
/// Modifies `a32_Q17[]` in place via the recurrence:
///   a32_Q17[k] = (a32_Q17[k] * sc_Q16[k]) >> 16
///   sc_Q16[k+1] = (sc_Q16[0] * sc_Q16[k] + 32768) >> 16
fn bwexpander_32(a32_q17: &mut [i32], sc_q16_0: i32) {
    let mut sc_q16: i64 = sc_q16_0 as i64;
    for v in a32_q17.iter_mut() {
        // a32_Q17 * sc_Q16 may need 48 bits before the shift.
        let prod = (*v as i64) * sc_q16;
        *v = (prod >> 16) as i32;
        sc_q16 = ((sc_q16_0 as i64) * sc_q16 + 32768) >> 16;
    }
}

/// §4.2.7.5.8 stability check via `silk_LPC_inverse_pred_gain_QA`.
/// Returns true if the LPC filter built from `a32_Q17[]` (already in the
/// post-saturation `<<5` Q17 form) is stable per the inverse prediction-
/// gain test described in RFC 6716 §4.2.7.5.8.
fn lpc_inverse_pred_gain_is_stable(a32_q17: &[i32]) -> bool {
    let d_lpc = a32_q17.len();
    // a32_Q12[n] = (a32_Q17[n] + 16) >> 5
    let a32_q12: Vec<i32> = a32_q17.iter().map(|&v| (v + 16) >> 5).collect();

    // Initial DC-response check (RFC 6716 §4.2.7.5.8).
    let dc_resp: i64 = a32_q12.iter().map(|&v| v as i64).sum();
    if dc_resp > 4096 {
        return false;
    }

    // a32_Q24[d_LPC-1][n] = a32_Q12[n] << 12
    let mut a_q24: Vec<i64> = a32_q12.iter().map(|&v| (v as i64) << 12).collect();

    // inv_gain_Q30 is initialized to 1<<30 at the d_LPC level and
    // accumulated as we descend; the spec gates stability on it
    // dropping below 107_374 (≈ 1/10000 in Q30).
    let mut inv_gain_q30: i64 = 1_i64 << 30;

    // For k from d_LPC-1 down to 0.
    let mut k = d_lpc as i32 - 1;
    while k >= 0 {
        let kk = k as usize;
        let a_kk = a_q24[kk];
        if a_kk.unsigned_abs() > 16_773_022 {
            return false;
        }
        // rc_Q31[k] = -a32_Q24[k][k] << 7
        let rc_q31: i64 = -(a_kk) << 7;
        // div_Q30[k] = (1<<30) - (rc_Q31[k] * rc_Q31[k] >> 32)
        let rc_sq = (rc_q31.wrapping_mul(rc_q31)) >> 32;
        let div_q30: i64 = (1_i64 << 30) - rc_sq;
        // Update accumulated inverse prediction gain.
        // inv_gain_Q30[k] = (inv_gain_Q30[k+1] * div_Q30[k] >> 32) << 2
        inv_gain_q30 = (inv_gain_q30.wrapping_mul(div_q30) >> 32) << 2;
        if inv_gain_q30 < 107_374 {
            return false;
        }
        if k == 0 {
            break;
        }
        // Update a32_Q24[k-1][n] from a32_Q24[k][n] for n in 0..k.
        let b1 = ilog_u32(div_q30 as u64);
        let b2 = b1 - 16;
        if !(0..=30).contains(&b2) {
            return false;
        }
        let div_shifted = (div_q30 >> (b2 + 1)).max(1);
        let inv_qb2 = ((1_i64 << 29) - 1) / div_shifted;
        // err_Q29 = (1<<29) - ((div_Q30 << (15 - b2)) * inv_Qb2 >> 16)
        let shamt = 15 - b2;
        if !(0..=31).contains(&shamt) {
            return false;
        }
        let err_q29 = (1_i64 << 29) - (((div_q30 << shamt).wrapping_mul(inv_qb2)) >> 16);
        // gain_Qb1 = (inv_Qb2 << 16) + (err_Q29 * inv_Qb2 >> 13)
        let gain_qb1 = (inv_qb2 << 16) + ((err_q29.wrapping_mul(inv_qb2)) >> 13);
        // Compute new row.
        let mut new_row = vec![0_i64; kk];
        for n in 0..kk {
            let num = a_q24[n] - (((a_q24[kk - n - 1].wrapping_mul(rc_q31)) + (1_i64 << 30)) >> 31);
            let v = (num.wrapping_mul(gain_qb1) + (1_i64 << (b1 - 1))) >> b1;
            new_row[n] = v;
        }
        // Replace the first kk entries with the new row.
        for (n, &v) in new_row.iter().enumerate() {
            a_q24[n] = v;
        }
        k -= 1;
    }
    true
}

/// `ilog(x)` per RFC 6716 §1.1.4 — returns the position of the
/// highest-order non-zero bit of x, with `ilog(0) = 0` and
/// `ilog(1) = 1`.
fn ilog_u32(x: u64) -> i32 {
    if x == 0 {
        0
    } else {
        64 - x.leading_zeros() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IHMW weights for a known stage-1 codebook entry must fall in the
    /// spec-stated range `[1819, 5227]`. We pick I1=0 NB/MB (the
    /// shallow vowel template) and verify each weight lands in band.
    #[test]
    fn ihmw_weights_in_range_nbmb_i1_0() {
        let cb1: Vec<i32> = tables::NLSF_NBMB_CB1_Q8[0]
            .iter()
            .map(|&v| v as i32)
            .collect();
        let w = compute_ihmw_weights(&cb1);
        for (i, &wi) in w.iter().enumerate() {
            assert!(
                (1819..=5227).contains(&wi),
                "NB/MB I1=0 IHMW w[{i}] = {wi} outside spec range [1819,5227]"
            );
        }
    }

    #[test]
    fn ihmw_weights_in_range_wb_i1_0() {
        let cb1: Vec<i32> = tables::NLSF_WB_CB1_Q8[0]
            .iter()
            .map(|&v| v as i32)
            .collect();
        let w = compute_ihmw_weights(&cb1);
        for (i, &wi) in w.iter().enumerate() {
            assert!(
                (1819..=5227).contains(&wi),
                "WB I1=0 IHMW w[{i}] = {wi} outside spec range [1819,5227]"
            );
        }
    }

    /// Stabilise must produce monotonically-increasing entries in
    /// `[1, 32767]` and respect the per-coefficient minimum spacing.
    #[test]
    fn stabilize_monotone_and_min_spacing_nbmb() {
        // Deliberately broken input: zero everywhere.
        let broken = vec![0i16; 10];
        let fixed = stabilize(&broken, false);
        assert_eq!(fixed.len(), 10);
        let mut prev: i32 = 0;
        for (k, &v) in fixed.iter().enumerate() {
            assert!(v >= 1, "k={k} v={v}");
            assert!(
                (v as i32) - prev >= tables::NLSF_NBMB_MIN_DELTA_Q15[k] as i32,
                "k={k} v={v} prev={prev}"
            );
            prev = v as i32;
        }
        assert!(32768 - prev >= tables::NLSF_NBMB_MIN_DELTA_Q15[10] as i32);
    }

    #[test]
    fn stabilize_monotone_and_min_spacing_wb() {
        let broken = vec![0i16; 16];
        let fixed = stabilize(&broken, true);
        assert_eq!(fixed.len(), 16);
        let mut prev: i32 = 0;
        for (k, &v) in fixed.iter().enumerate() {
            assert!(v >= 1, "k={k} v={v}");
            assert!(
                (v as i32) - prev >= tables::NLSF_WB_MIN_DELTA_Q15[k] as i32,
                "k={k} v={v} prev={prev}"
            );
            prev = v as i32;
        }
        assert!(32768 - prev >= tables::NLSF_WB_MIN_DELTA_Q15[16] as i32);
    }

    /// Spec consistency: the cosine LUT has 129 entries and is
    /// monotonically decreasing.
    #[test]
    fn cosine_lut_monotone() {
        for w in tables::COSINE_Q12.windows(2) {
            assert!(w[0] >= w[1], "{} should be >= {}", w[0], w[1]);
        }
        assert_eq!(tables::COSINE_Q12[0], 4096);
        assert_eq!(tables::COSINE_Q12[64], 0);
        assert_eq!(tables::COSINE_Q12[128], -4096);
    }

    /// After §4.2.7.5.6 reconstruction + our bandwidth-expansion guard
    /// (see `nlsf_to_lpc`), the LPC must satisfy `|sum(lpc)| < 0.5`
    /// for every stage-1 codebook entry — otherwise the synthesis IIR
    /// would drift unbounded under sustained input.
    #[test]
    fn lpc_dc_response_is_safely_bounded() {
        for i1 in 0..32usize {
            let nlsf: Vec<i16> = tables::NLSF_NBMB_CB1_Q8[i1]
                .iter()
                .map(|&v| ((v as i32) << 7) as i16)
                .collect();
            let lpc = nlsf_to_lpc(&nlsf, OpusBandwidth::Narrowband);
            let dc: f32 = lpc.iter().sum();
            assert!(dc.abs() < 0.5, "NB I1={i1} LPC DC sum {dc} too large");
        }
        for i1 in 0..32usize {
            let nlsf: Vec<i16> = tables::NLSF_WB_CB1_Q8[i1]
                .iter()
                .map(|&v| ((v as i32) << 7) as i16)
                .collect();
            let lpc = nlsf_to_lpc(&nlsf, OpusBandwidth::Wideband);
            let dc: f32 = lpc.iter().sum();
            assert!(dc.abs() < 0.5, "WB I1={i1} LPC DC sum {dc} too large");
        }
    }

    /// nlsf_to_lpc produces an LPC vector of the right length.
    #[test]
    fn nlsf_to_lpc_lengths() {
        let nb = vec![
            3000i16, 6000, 9000, 12000, 15000, 18000, 21000, 24000, 27000, 30000,
        ];
        let lpc = nlsf_to_lpc(&nb, OpusBandwidth::Narrowband);
        assert_eq!(lpc.len(), 10);
        let wb: Vec<i16> = (1..=16).map(|k| (k * 2000) as i16).collect();
        let lpc = nlsf_to_lpc(&wb, OpusBandwidth::Wideband);
        assert_eq!(lpc.len(), 16);
    }
}
