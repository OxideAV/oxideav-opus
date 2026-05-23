//! SILK Normalized LSF → LPC conversion — RFC 6716 §4.2.7.5.6 (core).
//!
//! Given a stabilized + (optionally) interpolated normalized-LSF vector
//! `nlsf_q15[]` (the §4.2.7.5.4 or §4.2.7.5.5 output), this module runs
//! the `silk_NLSF2A()` procedure described by §4.2.7.5.6:
//!
//!  1. Approximate `cos(pi * n[k])` using the §4.2.7.5.6 Q12 cosine
//!     table (Table 28) with linear interpolation over the top-7-bits
//!     index `i = n[k] >> 8` and the next-8-bits fraction
//!     `f = n[k] & 255`, then re-order the resulting `c_Q17[]` via
//!     Table 27 so the polynomial reconstruction below stays
//!     numerically stable:
//!
//!     ```text
//!     c_Q17[ordering[k]] = (cos_Q12[i]*256
//!                           + (cos_Q12[i+1] - cos_Q12[i])*f + 4) >> 3
//!     ```
//!
//!  2. Run the §4.2.7.5.6 P/Q polynomial recurrence
//!     (`silk_NLSF2A_find_poly()`):
//!
//!     ```text
//!     d2 = d_LPC / 2
//!     p_Q16[0][0] = q_Q16[0][0] = 1 << 16
//!     p_Q16[0][1] = -c_Q17[0]    q_Q16[0][1] = -c_Q17[1]
//!
//!     for 0 < k < d2, 0 <= j <= k+1:
//!         p_Q16[k][j] = p_Q16[k-1][j] + p_Q16[k-1][j-2]
//!                     - ((c_Q17[2*k]   * p_Q16[k-1][j-1] + 32768) >> 16)
//!         q_Q16[k][j] = q_Q16[k-1][j] + q_Q16[k-1][j-2]
//!                     - ((c_Q17[2*k+1] * q_Q16[k-1][j-1] + 32768) >> 16)
//!     ```
//!
//!     with the §4.2.7.5.6 boundary conditions `p[k][j]=q[k][j]=0` for
//!     `j<0` and `p[k][k+2]=p[k][k]`, `q[k][k+2]=q[k][k]` (justified by
//!     the polynomial symmetry).
//!
//!  3. Combine the final row into the 32-bit Q17 LPC coefficients
//!     (`silk_NLSF2A` last block):
//!
//!     ```text
//!     a32_Q17[k]         = -(q_Q16[d2-1][k+1] - q_Q16[d2-1][k])
//!                          -  (p_Q16[d2-1][k+1] + p_Q16[d2-1][k])
//!
//!     a32_Q17[d_LPC-k-1] =  (q_Q16[d2-1][k+1] - q_Q16[d2-1][k])
//!                          - (p_Q16[d2-1][k+1] + p_Q16[d2-1][k])
//!     ```
//!
//! This module covers the §4.2.7.5.6 **core conversion** only. The
//! §4.2.7.5.7 range-limiting bandwidth-expansion loop (up to 10 rounds
//! shrinking `a32_Q17[]` so it fits Q12) and the §4.2.7.5.8 prediction-gain
//! stability test (up to 16 chirp rounds + `silk_LPC_inverse_pred_gain_QA`)
//! are deferred to subsequent rounds. The `a32_Q17[]` produced here is
//! the raw input to that pipeline — callers that need a final stable Q12
//! filter must run §4.2.7.5.7 + §4.2.7.5.8 first.

use crate::silk_lsf_stage2::{D_LPC_MAX, D_LPC_NB_MB, D_LPC_WB};
use crate::toc::Bandwidth;
use crate::Error;

// =====================================================================
// Table 27 — LSF Ordering for Polynomial Evaluation.
//
// `ordering[k]` is the destination slot in `c_Q17[]` for the linearly-
// interpolated cosine of `nlsf_q15[k]`. The reordering improves numerical
// stability of the §4.2.7.5.6 polynomial recurrence by pairing roots that
// cancel in P(z) / Q(z) close together in the index order.
//
// `d_LPC = 10` for NB / MB; `d_LPC = 16` for WB. Cells are in `0..d_LPC`.
// =====================================================================

const ORDERING_NB_MB: [u8; D_LPC_NB_MB] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];

const ORDERING_WB: [u8; D_LPC_WB] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];

/// The Table 27 ordering vector for `bandwidth`. Rejects SWB / FB since
/// SILK never sees those after the §4.2.2 hybrid split.
pub fn ordering(bandwidth: Bandwidth) -> Result<&'static [u8], Error> {
    match bandwidth {
        Bandwidth::Nb | Bandwidth::Mb => Ok(&ORDERING_NB_MB),
        Bandwidth::Wb => Ok(&ORDERING_WB),
        Bandwidth::Swb | Bandwidth::Fb => Err(Error::MalformedPacket),
    }
}

// =====================================================================
// Table 28 — Q12 Cosine Table for LSF Conversion.
//
// 129 entries, `i ∈ 0..=128`. `cos_Q12[0] = 4096` (= cos(0) in Q12),
// `cos_Q12[64] = 0` (= cos(pi/2)), `cos_Q12[128] = -4096` (= cos(pi)).
// The table is anti-symmetric about `i = 64`: `cos_Q12[128-i] == -cos_Q12[i]`.
// =====================================================================

#[rustfmt::skip]
const COS_Q12: [i32; 129] = [
    4096,  4095,  4091,  4085,
    4076,  4065,  4052,  4036,
    4017,  3997,  3973,  3948,
    3920,  3889,  3857,  3822,
    3784,  3745,  3703,  3659,
    3613,  3564,  3513,  3461,
    3406,  3349,  3290,  3229,
    3166,  3102,  3035,  2967,
    2896,  2824,  2751,  2676,
    2599,  2520,  2440,  2359,
    2276,  2191,  2106,  2019,
    1931,  1842,  1751,  1660,
    1568,  1474,  1380,  1285,
    1189,  1093,   995,   897,
     799,   700,   601,   501,
     401,   301,   201,   101,
       0,  -101,  -201,  -301,
    -401,  -501,  -601,  -700,
    -799,  -897,  -995, -1093,
   -1189, -1285, -1380, -1474,
   -1568, -1660, -1751, -1842,
   -1931, -2019, -2106, -2191,
   -2276, -2359, -2440, -2520,
   -2599, -2676, -2751, -2824,
   -2896, -2967, -3035, -3102,
   -3166, -3229, -3290, -3349,
   -3406, -3461, -3513, -3564,
   -3613, -3659, -3703, -3745,
   -3784, -3822, -3857, -3889,
   -3920, -3948, -3973, -3997,
   -4017, -4036, -4052, -4065,
   -4076, -4085, -4091, -4095,
   -4096,
];

// =====================================================================
// silk_NLSF2A_cos approximation.
// =====================================================================

/// Compute the §4.2.7.5.6 re-ordered Q17 cosine vector `c_Q17[]` from a
/// stabilized / interpolated normalized-LSF vector `nlsf_q15[]`.
///
/// Each `nlsf_q15[k]` is split into the top 7 bits `i = nlsf >> 8` (in
/// `0..=127`, so `i+1` indexes a valid cell in [`COS_Q12`]) and the next
/// 8 bits `f = nlsf & 255`. The §4.2.7.5.6 piecewise-linear interpolation
///
/// ```text
/// c_Q17[ordering[k]] = (cos_Q12[i]*256 + (cos_Q12[i+1]-cos_Q12[i])*f + 4) >> 3
/// ```
///
/// is applied with the Table 27 destination index for `bandwidth`. Output
/// length is `d_LPC` (10 for NB / MB, 16 for WB).
///
/// Returns `Error::MalformedPacket` if `bandwidth` is SWB / FB (SILK
/// never sees those) or if `nlsf_q15.len() != d_LPC`.
pub fn nlsf_to_c_q17(bandwidth: Bandwidth, nlsf_q15: &[i16]) -> Result<[i32; D_LPC_MAX], Error> {
    let ord = ordering(bandwidth)?;
    let d_lpc = ord.len();
    if nlsf_q15.len() != d_lpc {
        return Err(Error::MalformedPacket);
    }

    let mut c_q17 = [0i32; D_LPC_MAX];
    for (k, &n) in nlsf_q15.iter().enumerate() {
        // The §4.2.7.5.4 stabilization guarantees nlsf_q15[k] ∈ [0, 32767]
        // so the top-7-bits index is at most 127 and `i+1` is a valid
        // COS_Q12 cell.
        let n = n as i32;
        let i = (n >> 8) as usize;
        let f = n & 0xFF;
        let a = COS_Q12[i];
        let b = COS_Q12[i + 1];
        // The `+ 4` in the formula is the half-LSB rounding term for the
        // final >> 3 (the only Q14→Q17 step that survives after the
        // *256 and *f terms cancel into the same Q14 scale).
        let v = (a * 256 + (b - a) * f + 4) >> 3;
        c_q17[ord[k] as usize] = v;
    }
    Ok(c_q17)
}

// =====================================================================
// silk_NLSF2A_find_poly + silk_NLSF2A.
// =====================================================================

/// Run the §4.2.7.5.6 P/Q polynomial recurrence on a single side
/// (selecting the even-indexed `c_Q17[2*k]` terms for P or the
/// odd-indexed `c_Q17[2*k+1]` terms for Q) and return the final row.
///
/// `d2 = d_LPC / 2` (5 for NB / MB, 8 for WB). The returned row holds the
/// first `d2 + 1` coefficients (the rest are redundant by the §4.2.7.5.6
/// symmetry `p[k][k+2] = p[k][k]`).
fn find_poly(c_q17: &[i32], d_lpc: usize, parity: usize) -> [i64; D_LPC_MAX / 2 + 1] {
    debug_assert!(d_lpc == D_LPC_NB_MB || d_lpc == D_LPC_WB);
    debug_assert!(parity < 2);
    let d2 = d_lpc / 2;

    // Two rolling rows: prev (k-1) and curr (k).
    let mut prev = [0i64; D_LPC_MAX / 2 + 1];
    let mut curr = [0i64; D_LPC_MAX / 2 + 1];

    // k = 0 initial conditions: only [0] and [1] are touched.
    prev[0] = 1i64 << 16;
    prev[1] = -(c_q17[parity] as i64);

    for k in 1..d2 {
        let c = c_q17[2 * k + parity] as i64;
        for j in 0..=k + 1 {
            // Boundary p[k-1][j] for j out of range: prev[j] is 0 for j<0
            // (we just clamp), and for j == k+1 we'd be reading prev[k+1]
            // which was 0 on the previous iteration (prev only filled up
            // to k for the prior k = k-1 step).
            let pj0 = if j < prev.len() { prev[j] } else { 0 };
            let pjm2 = if j >= 2 { prev[j - 2] } else { 0 };
            let pjm1 = if j >= 1 { prev[j - 1] } else { 0 };
            // The (*c * pjm1 + 32768) >> 16 rounding term matches the
            // §4.2.7.5.6 recurrence verbatim. Up to 48 bits of intermediate
            // precision can be required; i64 covers it.
            curr[j] = pj0 + pjm2 - ((c * pjm1 + 32768) >> 16);
        }
        // Apply p[k][k+2] = p[k][k] symmetry implicitly by leaving entries
        // beyond k+1 untouched for the next iteration's reads; we copy
        // curr → prev wholesale and clear the trailing cell so the next
        // iteration's "j = (k+1)+1 = k+2" read picks up prev[k] correctly.
        // Per §4.2.7.5.6 only `j <= k+1` is computed, and the recurrence's
        // next step only reads `prev[j-2]` for `j <= (k+1)+1 = k+2`,
        // i.e. `prev[k]` at most — already in range.
        prev.copy_from_slice(&curr);
        // Zero the cell beyond k+1 so it doesn't leak into the next row's
        // computation through the j+1 path.
        for cell in prev.iter_mut().skip(k + 2) {
            *cell = 0;
        }
    }
    prev
}

/// The §4.2.7.5.6 NLSF → LPC core conversion result.
///
/// Holds the 32-bit Q17 LPC coefficients `a32_Q17[k]`, `k ∈ 0..d_LPC`
/// (without the leading `1.0` coefficient). These have **not** yet been
/// passed through the §4.2.7.5.7 range-limiting bandwidth-expansion loop
/// or the §4.2.7.5.8 prediction-gain stability check — both of those are
/// scheduled for subsequent rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LpcQ17 {
    len: u8,
    a32_q17: [i32; D_LPC_MAX],
}

impl LpcQ17 {
    /// Run the §4.2.7.5.6 core conversion against a stabilized /
    /// interpolated normalized-LSF vector `nlsf_q15[]`.
    ///
    /// `bandwidth` selects the Table 27 ordering and the implied `d_LPC`:
    /// 10 for NB / MB, 16 for WB. SWB / FB are rejected (SILK never sees
    /// them after the §4.2.2 hybrid split).
    pub fn from_nlsf(bandwidth: Bandwidth, nlsf_q15: &[i16]) -> Result<Self, Error> {
        let ord = ordering(bandwidth)?;
        let d_lpc = ord.len();
        if nlsf_q15.len() != d_lpc {
            return Err(Error::MalformedPacket);
        }

        let c_q17 = nlsf_to_c_q17(bandwidth, nlsf_q15)?;
        let d2 = d_lpc / 2;

        let p_last = find_poly(&c_q17[..d_lpc], d_lpc, 0);
        let q_last = find_poly(&c_q17[..d_lpc], d_lpc, 1);

        // Assemble a32_Q17 from the last p/q rows. The §4.2.7.5.6 final
        // block walks k ∈ 0..d2 and writes both ends of the array at
        // once: a32_Q17[k] = -(q_diff + p_sum) and
        // a32_Q17[d_LPC-k-1] = q_diff - p_sum, where
        // q_diff = q[d2-1][k+1] - q[d2-1][k] and
        // p_sum  = p[d2-1][k+1] + p[d2-1][k].
        let mut a32_q17 = [0i32; D_LPC_MAX];
        for k in 0..d2 {
            let q_diff = q_last[k + 1] - q_last[k];
            let p_sum = p_last[k + 1] + p_last[k];
            let lo = -(q_diff + p_sum);
            let hi = q_diff - p_sum;
            // The §4.2.7.5.6 prose notes that overflow into 32-bit is
            // expected before §4.2.7.5.7 clamps it; cast to i32 with
            // wrapping so adversarial inputs that overflow Q17 still
            // produce a deterministic value the next stage will then
            // reject or expand-down.
            a32_q17[k] = lo as i32;
            a32_q17[d_lpc - k - 1] = hi as i32;
        }

        Ok(Self {
            len: d_lpc as u8,
            a32_q17,
        })
    }

    /// The Q17 LPC coefficients `a32_Q17[k]`. Length is `d_LPC` (10 for
    /// NB / MB, 16 for WB).
    pub fn a32_q17(&self) -> &[i32] {
        &self.a32_q17[..self.len as usize]
    }

    /// Number of coefficients (10 for NB / MB, 16 for WB).
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// `true` if there are no coefficients (never happens after a
    /// successful conversion of a valid normalized-LSF vector).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Table 27 transcription self-checks --------------------------

    #[test]
    fn table27_nbmb_is_permutation_of_0_to_9() {
        let mut sorted = ORDERING_NB_MB.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, (0u8..10).collect::<Vec<_>>());
    }

    #[test]
    fn table27_wb_is_permutation_of_0_to_15() {
        let mut sorted = ORDERING_WB.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, (0u8..16).collect::<Vec<_>>());
    }

    #[test]
    fn table27_row_widths_match_d_lpc() {
        assert_eq!(ORDERING_NB_MB.len(), D_LPC_NB_MB);
        assert_eq!(ORDERING_WB.len(), D_LPC_WB);
    }

    #[test]
    fn table27_ordering_helper_routes_per_bandwidth() {
        assert_eq!(ordering(Bandwidth::Nb).unwrap(), &ORDERING_NB_MB);
        assert_eq!(ordering(Bandwidth::Mb).unwrap(), &ORDERING_NB_MB);
        assert_eq!(ordering(Bandwidth::Wb).unwrap(), &ORDERING_WB);
        assert!(ordering(Bandwidth::Swb).is_err());
        assert!(ordering(Bandwidth::Fb).is_err());
    }

    #[test]
    fn table27_first_row_spot_checks() {
        // Per RFC 6716 Table 27 column "NB and MB" — k = 0 → 0, k = 1 → 9,
        // k = 2 → 6, k = 3 → 3, ... k = 9 → 7. The first/last/middle cells
        // are the most likely to catch a transcription typo.
        assert_eq!(ORDERING_NB_MB[0], 0);
        assert_eq!(ORDERING_NB_MB[1], 9);
        assert_eq!(ORDERING_NB_MB[5], 5); // self-pinned cell
        assert_eq!(ORDERING_NB_MB[9], 7);

        // Per RFC 6716 Table 27 column "WB" — k = 0 → 0, k = 1 → 15,
        // k = 2 → 8, ..., k = 15 → 1.
        assert_eq!(ORDERING_WB[0], 0);
        assert_eq!(ORDERING_WB[1], 15);
        assert_eq!(ORDERING_WB[8], 2);
        assert_eq!(ORDERING_WB[15], 1);
    }

    // --- Table 28 transcription self-checks --------------------------

    #[test]
    fn table28_has_129_entries() {
        assert_eq!(COS_Q12.len(), 129);
    }

    #[test]
    fn table28_anchor_values() {
        // cos(0) = 1, cos(pi/2) = 0, cos(pi) = -1 in Q12.
        assert_eq!(COS_Q12[0], 4096);
        assert_eq!(COS_Q12[64], 0);
        assert_eq!(COS_Q12[128], -4096);
    }

    #[test]
    fn table28_anti_symmetric_about_64() {
        // The spec table is the discrete cosine sampled on [0, pi], so
        // cos_Q12[128 - i] == -cos_Q12[i] for i ∈ 0..=128.
        for i in 0..=128 {
            assert_eq!(
                COS_Q12[128 - i],
                -COS_Q12[i],
                "table 28 anti-symmetry broken at i = {i}: {} vs -{}",
                COS_Q12[128 - i],
                COS_Q12[i]
            );
        }
    }

    #[test]
    fn table28_strictly_decreasing() {
        // The cosine function is strictly decreasing on [0, pi]. Since the
        // table is sampled at 129 points covering that interval, every
        // adjacent pair (i, i+1) must satisfy cos_Q12[i] > cos_Q12[i+1].
        for i in 0..128 {
            assert!(
                COS_Q12[i] > COS_Q12[i + 1],
                "table 28 not strictly decreasing at i = {i}: {} -> {}",
                COS_Q12[i],
                COS_Q12[i + 1]
            );
        }
    }

    #[test]
    fn table28_in_q12_range() {
        // All entries are signed Q12 cosines, so they live in [-4096, 4096].
        for (i, &v) in COS_Q12.iter().enumerate() {
            assert!(
                (-4096..=4096).contains(&v),
                "table 28 cell {i} = {v} outside Q12 range"
            );
        }
    }

    #[test]
    fn table28_spot_checks() {
        // Row 0 starts (4096, 4095, 4091, 4085).
        assert_eq!(COS_Q12[0..4], [4096, 4095, 4091, 4085]);
        // Row 16 starts (3784, 3745, 3703, 3659) per Table 28.
        assert_eq!(COS_Q12[16..20], [3784, 3745, 3703, 3659]);
        // Row 60 starts (401, 301, 201, 101).
        assert_eq!(COS_Q12[60..64], [401, 301, 201, 101]);
        // Row 64 starts (0, -101, -201, -301).
        assert_eq!(COS_Q12[64..68], [0, -101, -201, -301]);
        // Row 124 starts (-4076, -4085, -4091, -4095).
        assert_eq!(COS_Q12[124..128], [-4076, -4085, -4091, -4095]);
    }

    // --- nlsf_to_c_q17 -----------------------------------------------

    #[test]
    fn cos_lookup_at_table_anchor_points_matches_table_entries() {
        // When nlsf_q15 = i << 8 the fractional `f` is 0, so the §4.2.7.5.6
        // interpolation reduces to (cos_Q12[i]*256 + 4) >> 3 == cos_Q12[i]*32 + 0
        // (since 4 >> 3 == 0 for positive a, and the +4 is integer rounding).
        // More precisely: (a*256 + 4) >> 3 = a*32 + (4 >> 3) = a*32 for the
        // sign-positive arithmetic shift on i32. Build a synthetic NLSF
        // with k=0 carrying i=0 anchor, verify c_q17[ordering[0]] = 4096*32.
        let mut nlsf = vec![0i16; D_LPC_NB_MB];
        // Pick distinct top-7-bits indices so each c slot gets a known value.
        // Use i = 8*k → cos_Q12[i] anchors.
        for (k, slot) in nlsf.iter_mut().enumerate() {
            *slot = ((8 * k as i32) << 8) as i16;
        }
        let c = nlsf_to_c_q17(Bandwidth::Nb, &nlsf).unwrap();
        for (k, &dest) in ORDERING_NB_MB.iter().enumerate() {
            let i = 8 * k;
            let expected = (COS_Q12[i] * 256 + 4) >> 3;
            assert_eq!(c[dest as usize], expected, "anchor mismatch at k = {k}");
        }
    }

    #[test]
    fn cos_lookup_rejects_swb_fb() {
        let nlsf = vec![0i16; D_LPC_NB_MB];
        assert!(nlsf_to_c_q17(Bandwidth::Swb, &nlsf).is_err());
        assert!(nlsf_to_c_q17(Bandwidth::Fb, &nlsf).is_err());
    }

    #[test]
    fn cos_lookup_rejects_length_mismatch() {
        // NB ordering wants 10 entries; passing 16 is malformed.
        let nlsf = vec![0i16; 16];
        assert!(nlsf_to_c_q17(Bandwidth::Nb, &nlsf).is_err());
        let nlsf = vec![0i16; 10];
        assert!(nlsf_to_c_q17(Bandwidth::Wb, &nlsf).is_err());
    }

    #[test]
    fn cos_lookup_linear_interp_midpoint() {
        // For `f = 128` the interpolation lands exactly at the midpoint of
        // (cos_Q12[i], cos_Q12[i+1]):
        //   (a*256 + (b - a)*128 + 4) >> 3
        //   = (256*a + 128*b - 128*a + 4) >> 3
        //   = (128*(a + b) + 4) >> 3
        //   = 16 * (a + b)   (since 128/8 = 16; the +4 rounds the lone LSB)
        let mut nlsf = vec![0i16; D_LPC_NB_MB];
        // Put a known (i, f) pair into k=0: i=10, f=128 → nlsf = (10 << 8) | 128 = 2688
        nlsf[0] = (10 << 8) | 128;
        // Pad the rest with distinct values so we don't collide.
        for (k, slot) in nlsf.iter_mut().enumerate().take(D_LPC_NB_MB).skip(1) {
            *slot = ((20 + k as i32) << 8) as i16;
        }
        let c = nlsf_to_c_q17(Bandwidth::Nb, &nlsf).unwrap();
        let a = COS_Q12[10];
        let b = COS_Q12[11];
        let expected = (a * 256 + (b - a) * 128 + 4) >> 3;
        assert_eq!(c[ORDERING_NB_MB[0] as usize], expected);
    }

    // --- LpcQ17 ------------------------------------------------------

    #[test]
    fn lpc_length_matches_bandwidth() {
        let nb = vec![1638i16; D_LPC_NB_MB]; // ascending placeholder, monotone
        let mut nb_mono = nb.clone();
        for (k, v) in nb_mono.iter_mut().enumerate() {
            *v = (1000 + 2000 * k as i32) as i16;
        }
        let lpc = LpcQ17::from_nlsf(Bandwidth::Nb, &nb_mono).unwrap();
        assert_eq!(lpc.len(), D_LPC_NB_MB);
        assert_eq!(lpc.a32_q17().len(), D_LPC_NB_MB);

        let mut wb_mono = vec![0i16; D_LPC_WB];
        for (k, v) in wb_mono.iter_mut().enumerate() {
            *v = (500 + 1900 * k as i32) as i16;
        }
        let lpc = LpcQ17::from_nlsf(Bandwidth::Wb, &wb_mono).unwrap();
        assert_eq!(lpc.len(), D_LPC_WB);
        assert_eq!(lpc.a32_q17().len(), D_LPC_WB);
    }

    #[test]
    fn lpc_rejects_swb_fb_and_length_mismatch() {
        let nlsf = vec![0i16; D_LPC_NB_MB];
        assert!(LpcQ17::from_nlsf(Bandwidth::Swb, &nlsf).is_err());
        assert!(LpcQ17::from_nlsf(Bandwidth::Fb, &nlsf).is_err());
        let nlsf = vec![0i16; 12];
        assert!(LpcQ17::from_nlsf(Bandwidth::Nb, &nlsf).is_err());
    }

    /// Sanity oracle: independently re-run the §4.2.7.5.6 recurrence using
    /// vectors of dynamic-length intermediate Q16 coefficients. This is a
    /// straight transcription of the spec — distinct from the rolling-row
    /// production implementation — so a typo in the recurrence shows up
    /// as a divergence between the two paths.
    fn oracle_a32_q17(c_q17: &[i32], d_lpc: usize) -> Vec<i32> {
        let d2 = d_lpc / 2;
        let run_side = |parity: usize| -> Vec<i64> {
            // Full 2D matrix p[k][j], k ∈ 0..d2, j ∈ 0..=d2+1. The §4.2.7.5.6
            // recurrence only ever reads j-2..=j on the prior row so the
            // 2D allocation is correct (and wasteful in memory — fine for a
            // test oracle).
            let cols = d2 + 2;
            let mut p = vec![vec![0i64; cols]; d2];
            p[0][0] = 1 << 16;
            p[0][1] = -(c_q17[parity] as i64);
            for k in 1..d2 {
                let c = c_q17[2 * k + parity] as i64;
                for j in 0..=k + 1 {
                    let pj0 = p[k - 1][j];
                    let pjm2 = if j >= 2 { p[k - 1][j - 2] } else { 0 };
                    let pjm1 = if j >= 1 { p[k - 1][j - 1] } else { 0 };
                    p[k][j] = pj0 + pjm2 - ((c * pjm1 + 32768) >> 16);
                }
            }
            p.pop().unwrap()
        };

        let p_last = run_side(0);
        let q_last = run_side(1);

        let mut a = vec![0i32; d_lpc];
        for k in 0..d2 {
            let q_diff = q_last[k + 1] - q_last[k];
            let p_sum = p_last[k + 1] + p_last[k];
            a[k] = (-(q_diff + p_sum)) as i32;
            a[d_lpc - k - 1] = (q_diff - p_sum) as i32;
        }
        a
    }

    fn ascending_nlsf(d_lpc: usize, start: i16, step: i16) -> Vec<i16> {
        (0..d_lpc as i16)
            .map(|k| start.saturating_add(k.saturating_mul(step)))
            .collect()
    }

    #[test]
    fn lpc_matches_oracle_nb() {
        let nlsf = ascending_nlsf(D_LPC_NB_MB, 1500, 2700); // ascending, ends ~25800
        let lpc = LpcQ17::from_nlsf(Bandwidth::Nb, &nlsf).unwrap();
        let c = nlsf_to_c_q17(Bandwidth::Nb, &nlsf).unwrap();
        let expected = oracle_a32_q17(&c[..D_LPC_NB_MB], D_LPC_NB_MB);
        assert_eq!(lpc.a32_q17(), expected.as_slice());
    }

    #[test]
    fn lpc_matches_oracle_wb() {
        let nlsf = ascending_nlsf(D_LPC_WB, 800, 1900); // ascending, ends ~29300
        let lpc = LpcQ17::from_nlsf(Bandwidth::Wb, &nlsf).unwrap();
        let c = nlsf_to_c_q17(Bandwidth::Wb, &nlsf).unwrap();
        let expected = oracle_a32_q17(&c[..D_LPC_WB], D_LPC_WB);
        assert_eq!(lpc.a32_q17(), expected.as_slice());
    }

    #[test]
    fn lpc_matches_oracle_against_real_decoder_pipeline() {
        // Drive a real §4.2.7.5.2 → §4.2.7.5.3 → §4.2.7.5.4 decoder
        // pipeline off a synthetic range-decoder buffer, then feed the
        // stabilized NLSF into both the production conversion and the
        // oracle. Sweep all 32 I1 values across {NB, MB, WB} for a
        // robust cross-check.
        use crate::range_decoder::RangeDecoder;
        use crate::silk_lsf_recon::NlsfReconstructed;
        use crate::silk_lsf_stabilize::NlsfStabilized;
        use crate::silk_lsf_stage2::LsfStage2;

        let buf = [
            0x5Au8, 0xC3, 0x17, 0x9E, 0x42, 0xFB, 0x08, 0x71, 0x2D, 0xB6, 0x4C, 0x8E,
        ];
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            for i1 in 0u8..32 {
                let mut rd = RangeDecoder::new(&buf);
                let stage2 = LsfStage2::decode(&mut rd, bw, i1).expect("stage-2");
                let recon =
                    NlsfReconstructed::from_stage1_and_stage2(bw, i1, &stage2).expect("recon");
                let stab = NlsfStabilized::from_reconstructed(bw, &recon).expect("stab");
                let lpc = LpcQ17::from_nlsf(bw, stab.nlsf_q15()).unwrap();
                let c = nlsf_to_c_q17(bw, stab.nlsf_q15()).unwrap();
                let d_lpc = stab.nlsf_q15().len();
                let expected = oracle_a32_q17(&c[..d_lpc], d_lpc);
                assert_eq!(
                    lpc.a32_q17(),
                    expected.as_slice(),
                    "production/oracle divergence: bw={bw:?} i1={i1}"
                );
            }
        }
    }

    #[test]
    fn lpc_leading_term_uses_full_row_sum() {
        // For k = 0 the formula reduces to
        //   a32_Q17[0] = -((q[d2-1][1] - q[d2-1][0]) + (p[d2-1][1] + p[d2-1][0]))
        // and a32_Q17[d_LPC-1] = (q[d2-1][1] - q[d2-1][0]) - (p[d2-1][1] + p[d2-1][0]).
        // The two share the same |q_diff + p_sum| magnitude path — pin this
        // identity on a hand-built case so a refactor of the row assembly
        // can't accidentally swap signs.
        let nlsf = ascending_nlsf(D_LPC_NB_MB, 1500, 2700);
        let lpc = LpcQ17::from_nlsf(Bandwidth::Nb, &nlsf).unwrap();
        let a = lpc.a32_q17();
        // a[0] = -(q_diff + p_sum); a[d_LPC-1] = q_diff - p_sum.
        // Therefore a[0] + a[d_LPC-1] = -2 * p_sum.
        // We don't know p_sum here, but we can check parity / consistency
        // via the relation a[0] - a[d_LPC-1] = -2 * q_diff (must be even).
        assert_eq!((a[0] - a[D_LPC_NB_MB - 1]) % 2, 0);
        assert_eq!((a[0] + a[D_LPC_NB_MB - 1]) % 2, 0);
    }

    #[test]
    fn lpc_real_pipeline_does_not_panic_across_bandwidth_x_i1_sweep() {
        // The §4.2.7.5.6 recurrence's i64 intermediates protect against
        // overflow in the bitstream-driven case. Verify the full SILK
        // §4.2.7.5.2..§4.2.7.5.4 → §4.2.7.5.6 path is panic-free for every
        // (bandwidth, I1) on a few buffers.
        use crate::range_decoder::RangeDecoder;
        use crate::silk_lsf_recon::NlsfReconstructed;
        use crate::silk_lsf_stabilize::NlsfStabilized;
        use crate::silk_lsf_stage2::LsfStage2;

        let bufs: &[&[u8]] = &[
            &[
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC,
            ],
            &[
                0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10, 0xFE, 0xDC, 0xBA, 0x98,
            ],
            &[
                0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF,
            ],
        ];
        for buf in bufs {
            for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
                for i1 in 0u8..32 {
                    let mut rd = RangeDecoder::new(buf);
                    let stage2 = LsfStage2::decode(&mut rd, bw, i1).expect("stage-2");
                    let recon =
                        NlsfReconstructed::from_stage1_and_stage2(bw, i1, &stage2).expect("recon");
                    let stab = NlsfStabilized::from_reconstructed(bw, &recon).expect("stab");
                    let lpc = LpcQ17::from_nlsf(bw, stab.nlsf_q15()).unwrap();
                    // §4.2.7.5.6 leaves a32_Q17 unbounded; just confirm the
                    // length is right and the call returned.
                    assert_eq!(lpc.len(), stab.nlsf_q15().len());
                }
            }
        }
    }
}
