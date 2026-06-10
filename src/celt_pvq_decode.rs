//! CELT §4.3.4.2 PVQ index-to-vector decoding
//! (RFC 6716 §4.3.4.2, p. 116–117).
//!
//! The §4.3.4 *Shape Decoding* layer encodes the unit-norm normalized
//! "shape" of every CELT MDCT band as a Pyramid Vector Quantizer
//! codeword. The size of the codebook is `V(N, K)` (the round-41
//! [`crate::celt_pvq_v::pvq_codebook_size`] primitive), the codeword
//! index `i ∈ 0..V(N, K)` is read with `ec_dec_uint(V(N, K))`
//! ([`crate::RangeDecoder::dec_uint`]), and this module turns that
//! index into the integer-magnitude pulse vector `X` with
//! `|X_0| + |X_1| + ... + |X_{N-1}| = K`.
//!
//! ## §4.3.4.2 index-to-vector algorithm
//!
//! RFC 6716 §4.3.4.2 (p. 116–117) states the decode directly:
//!
//! > The decoded vector X is recovered as follows. Let i be the index
//! > decoded with the procedure in Section 4.1.5 with `ft = V(N,K)`,
//! > so that `0 <= i < V(N,K)`. Let `k = K`. Then, for `j = 0` to
//! > `(N - 1)`, inclusive, do:
//! >
//! > 1. Let `p = (V(N-j-1,k) + V(N-j,k))/2`.
//! > 2. If `i < p`, then let `sgn = 1`, else let `sgn = -1` and set
//! >    `i = i - p`.
//! > 3. Let `k0 = k` and set `p = p - V(N-j-1,k)`.
//! > 4. While `p > i`, set `k = k - 1` and `p = p - V(N-j-1,k)`.
//! > 5. Set `X[j] = sgn*(k0 - k)` and `i = i - p`.
//! >
//! > The decoded vector X is then normalized such that its L2-norm
//! > equals one.
//!
//! The two halves of the divisor in step 1 are the count of
//! configurations whose `j`-th coordinate is strictly positive
//! (`sgn = +1`, the `i < p` branch) versus the rest (`sgn = -1`); the
//! `p` decrement loop in steps 3–4 walks the per-coordinate magnitude
//! `k0 - k` down to the slice the index falls into. The arithmetic is
//! `V(N, K)`-counting only — no probability model, no range-coder
//! interaction beyond the single up-front `ec_dec_uint(V(N, K))` read.
//!
//! This module owns the **integer pulse vector** half: the `X[j] ∈ Z`
//! magnitudes-and-signs reconstruction. The §4.3.4.2 final
//! "normalize such that the L2-norm equals one" step is a
//! floating-point operation that depends on the band's energy scaling
//! and runs at the §4.3.4 consumer site; this module returns the
//! integer pulse vector that feeds it and exposes
//! [`pvq_l1_norm`] / [`pvq_l2_norm_squared`] so a caller (and these
//! tests) can assert the `L1 = K` invariant that every conforming
//! codeword satisfies.
//!
//! ## Provenance
//!
//! Narrative + algorithm: RFC 6716 §4.3.4.2 (p. 116–117), reproduced
//! from `docs/audio/opus/rfc6716-opus.txt`. No external library source
//! was consulted; the five-step index-to-vector procedure is stated
//! verbatim in the standards-track text.

use crate::celt_pvq_v::{pvq_codebook_size, PvqVError, PVQ_V_K_MAX, PVQ_V_N_MAX};

/// Errors returnable by [`decode_pvq_vector`] and [`decode_pvq_vector_into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvqDecodeError {
    /// `N` exceeds [`crate::celt_pvq_v::PVQ_V_N_MAX`], or another
    /// `V(N, K)` evaluation rejected its arguments. Wraps the
    /// underlying [`PvqVError`].
    CodebookSize(PvqVError),
    /// The decoded index `i` is `>= V(N, K)`. RFC 6716 §4.3.4.2
    /// requires `0 <= i < V(N, K)`; a larger index cannot be produced
    /// by `ec_dec_uint(V(N, K))` on a conforming stream, so this
    /// signals a caller-side bookkeeping bug (an index obtained from
    /// the wrong `V`).
    IndexOutOfRange {
        /// The index the caller passed.
        index: u32,
        /// The codebook size `V(N, K)` the index must stay below.
        codebook_size: u32,
    },
    /// The caller-supplied output buffer is shorter than `N`.
    OutputBufferTooSmall {
        /// The required length (`N`).
        required: usize,
        /// The length the caller provided.
        provided: usize,
    },
}

impl core::fmt::Display for PvqDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            PvqDecodeError::CodebookSize(e) => {
                write!(
                    f,
                    "oxideav-opus: PVQ vector decode codebook-size error: {e}"
                )
            }
            PvqDecodeError::IndexOutOfRange {
                index,
                codebook_size,
            } => write!(
                f,
                "oxideav-opus: PVQ vector decode index {index} out of range \
                 (must be < V(N, K) = {codebook_size}) per RFC 6716 §4.3.4.2"
            ),
            PvqDecodeError::OutputBufferTooSmall { required, provided } => write!(
                f,
                "oxideav-opus: PVQ vector decode output buffer too small: \
                 required={required}, provided={provided}"
            ),
        }
    }
}

impl std::error::Error for PvqDecodeError {}

impl From<PvqVError> for PvqDecodeError {
    fn from(e: PvqVError) -> Self {
        PvqDecodeError::CodebookSize(e)
    }
}

/// Decodes the §4.3.4.2 PVQ codeword `index` into its integer pulse
/// vector `X` of length `N`.
///
/// `index` is the value the §4.3.4.2 decode reads with
/// `ec_dec_uint(V(N, K))` (see [`crate::RangeDecoder::dec_uint`]); it
/// must satisfy `0 <= index < V(N, K)`. The returned `Vec<i32>` has
/// length `N` and satisfies `sum |X[j]| == K` (the §4.3.4 PVQ L1
/// invariant). The vector is **not** yet normalized — the §4.3.4.2
/// "L2-norm equals one" scaling is a floating-point step that runs at
/// the §4.3.4 consumer site.
///
/// # Errors
///
/// * [`PvqDecodeError::CodebookSize`] if `V(N, K)` cannot be evaluated
///   (e.g. `N > PVQ_V_N_MAX`, `K > PVQ_V_K_MAX`, or the size overflows
///   the §4.1.5 `ec_dec_uint` range).
/// * [`PvqDecodeError::IndexOutOfRange`] if `index >= V(N, K)`.
pub fn decode_pvq_vector(n: u32, k: u32, index: u32) -> Result<Vec<i32>, PvqDecodeError> {
    let mut out = vec![0i32; n as usize];
    decode_pvq_vector_into(n, k, index, &mut out)?;
    Ok(out)
}

/// Decodes the §4.3.4.2 PVQ codeword `index` into a caller-supplied
/// buffer, filling `out[0..N]` with the integer pulse vector.
///
/// Behaves exactly like [`decode_pvq_vector`] but writes into `out`
/// (whose length must be at least `N`) instead of allocating. Returns
/// the number of coordinates written (`N`).
///
/// # Errors
///
/// In addition to the [`decode_pvq_vector`] errors:
///
/// * [`PvqDecodeError::OutputBufferTooSmall`] if `out.len() < N`.
pub fn decode_pvq_vector_into(
    n: u32,
    k: u32,
    index: u32,
    out: &mut [i32],
) -> Result<usize, PvqDecodeError> {
    let n_usize = n as usize;
    if out.len() < n_usize {
        return Err(PvqDecodeError::OutputBufferTooSmall {
            required: n_usize,
            provided: out.len(),
        });
    }

    // The codebook size bounds the index. Evaluating it first also
    // validates (N, K) against the §4.1.5 ec_dec_uint range.
    let codebook_size = pvq_codebook_size(n, k)?;
    if index >= codebook_size {
        return Err(PvqDecodeError::IndexOutOfRange {
            index,
            codebook_size,
        });
    }

    // N == 0 has the single empty codeword (V(0, 0) = 1, index 0).
    if n == 0 {
        return Ok(0);
    }

    // §4.3.4.2 five-step recovery. `i` and `k` are mutated in place.
    //
    // All intermediate V(.,.) values are bounded by V(N, K) ≤ 2**32-1
    // and i < V(N, K), so u64 working arithmetic never overflows. We
    // keep `i` and `p` in u64 to make the comparisons total without a
    // sign concern (every quantity is a non-negative count).
    let mut i: u64 = index as u64;
    let mut k_cur: u32 = k;

    for j in 0..n {
        // Step 1: p = (V(N-j-1, k) + V(N-j, k)) / 2.
        //
        // N-j-1 and N-j are both ≤ N ≤ PVQ_V_N_MAX so the lookups are
        // in range; k_cur ≤ K ≤ PVQ_V_K_MAX. V(N-j, k) ≥ V(N-j-1, k)
        // (monotone in the dimension) so the sum is even iff the RFC's
        // integer division is exact — it always is here because the
        // two terms differ by an even count of sign-symmetric
        // configurations; we nonetheless use the floor the RFC writes.
        let v_lower = pvq_codebook_size(n - j - 1, k_cur)? as u64;
        let v_upper = pvq_codebook_size(n - j, k_cur)? as u64;
        let mut p: u64 = (v_lower + v_upper) / 2;

        // Step 2: sign selection.
        let sgn: i32 = if i < p {
            1
        } else {
            i -= p;
            -1
        };

        // Step 3: k0 = k; p = p - V(N-j-1, k).
        let k0 = k_cur;
        // v_lower is V(N-j-1, k_cur); reuse it.
        p -= v_lower;

        // Step 4: while p > i, decrement k and subtract V(N-j-1, k).
        while p > i {
            k_cur -= 1;
            let v = pvq_codebook_size(n - j - 1, k_cur)? as u64;
            p -= v;
        }

        // Step 5: X[j] = sgn * (k0 - k); i = i - p.
        let magnitude = (k0 - k_cur) as i32;
        out[j as usize] = sgn * magnitude;
        i -= p;
    }

    Ok(n_usize)
}

/// Returns the L1 norm `sum |X[j]|` of a pulse vector.
///
/// Every conforming §4.3.4.2 PVQ codeword satisfies `pvq_l1_norm(X)
/// == K`. Exposed so the §4.3.4 consumer can assert the invariant
/// before normalizing.
pub fn pvq_l1_norm(x: &[i32]) -> u64 {
    x.iter().map(|&v| (v as i64).unsigned_abs()).sum()
}

/// Returns the squared L2 norm `sum X[j]**2` of a pulse vector.
///
/// The §4.3.4.2 final step scales `X` by `1 / sqrt(pvq_l2_norm_squared(X))`
/// to reach unit L2 norm. Exposed for the §4.3.4 consumer; the
/// floating-point division/sqrt is intentionally left to the caller.
pub fn pvq_l2_norm_squared(x: &[i32]) -> u64 {
    x.iter().map(|&v| (v as i64 * v as i64) as u64).sum()
}

/// Caller-side bookkeeping bound on `N` mirrored from
/// [`crate::celt_pvq_v::PVQ_V_N_MAX`] for convenience.
pub const PVQ_DECODE_N_MAX: u32 = PVQ_V_N_MAX;

/// Caller-side bookkeeping bound on `K` mirrored from
/// [`crate::celt_pvq_v::PVQ_V_K_MAX`] for convenience.
pub const PVQ_DECODE_K_MAX: u32 = PVQ_V_K_MAX;

#[cfg(test)]
mod tests {
    use super::*;

    // ---- §4.3.4.2 L1-norm invariant -----------------------------------------
    //
    // Every codeword index 0..V(N, K) must decode to a vector whose
    // L1 norm equals K. Sweeping the full index range for a range of
    // (N, K) is the single strongest property test: it confirms the
    // step-1..5 walk is a bijection onto the K-pulse lattice.

    #[test]
    fn every_index_decodes_to_l1_norm_k() {
        for n in 1..=6u32 {
            for k in 0..=6u32 {
                let v = pvq_codebook_size(n, k).unwrap();
                for index in 0..v {
                    let x = decode_pvq_vector(n, k, index).unwrap();
                    assert_eq!(x.len(), n as usize, "len at (N={n}, K={k}, i={index})");
                    assert_eq!(
                        pvq_l1_norm(&x),
                        k as u64,
                        "L1 norm at (N={n}, K={k}, i={index}): got {x:?}"
                    );
                }
            }
        }
    }

    // ---- §4.3.4.2 bijection -------------------------------------------------
    //
    // The decode must be injective: distinct indices produce distinct
    // vectors. Combined with the L1 = K property and the codebook size
    // V(N, K), injectivity over 0..V(N, K) proves surjectivity onto the
    // K-pulse lattice (a counting argument).

    #[test]
    fn decode_is_injective_over_full_index_range() {
        use std::collections::HashSet;
        for n in 1..=6u32 {
            for k in 0..=6u32 {
                let v = pvq_codebook_size(n, k).unwrap();
                let mut seen: HashSet<Vec<i32>> = HashSet::new();
                for index in 0..v {
                    let x = decode_pvq_vector(n, k, index).unwrap();
                    assert!(
                        seen.insert(x.clone()),
                        "duplicate vector {x:?} at (N={n}, K={k}, i={index})"
                    );
                }
                assert_eq!(seen.len() as u32, v, "coverage at (N={n}, K={k})");
            }
        }
    }

    // ---- §4.3.4.2 worked points ---------------------------------------------

    #[test]
    fn k_zero_decodes_to_all_zero_vector() {
        // V(N, 0) = 1; the single codeword (index 0) is the all-zero
        // vector for every N.
        for n in 0..=8u32 {
            let x = decode_pvq_vector(n, 0, 0).unwrap();
            assert_eq!(x, vec![0i32; n as usize], "all-zero at N={n}");
        }
    }

    #[test]
    fn n_one_k_one_two_signed_pulses() {
        // V(1, 1) = 2: the two codewords are [+1] and [-1].
        let mut got: Vec<Vec<i32>> = (0..2)
            .map(|i| decode_pvq_vector(1, 1, i).unwrap())
            .collect();
        got.sort();
        assert_eq!(got, vec![vec![-1], vec![1]]);
    }

    #[test]
    fn n_one_k_three_two_signed_pulses() {
        // V(1, 3) = 2: a single coordinate must carry all three pulses,
        // so the only codewords are [+3] and [-3].
        let mut got: Vec<Vec<i32>> = (0..2)
            .map(|i| decode_pvq_vector(1, 3, i).unwrap())
            .collect();
        got.sort();
        assert_eq!(got, vec![vec![-3], vec![3]]);
    }

    #[test]
    fn n_two_k_one_four_codewords() {
        // V(2, 1) = 4: [+1,0], [-1,0], [0,+1], [0,-1].
        let mut got: Vec<Vec<i32>> = (0..4)
            .map(|i| decode_pvq_vector(2, 1, i).unwrap())
            .collect();
        got.sort();
        assert_eq!(got, vec![vec![-1, 0], vec![0, -1], vec![0, 1], vec![1, 0]]);
    }

    #[test]
    fn n_two_k_two_full_codebook() {
        // V(2, 2) = 8. Enumerate the eight 2-D pulse vectors with
        // |x0| + |x1| = 2:
        //   (±2, 0), (0, ±2), (±1, ±1) — that's 2 + 2 + 4 = 8.
        let mut got: Vec<Vec<i32>> = (0..8)
            .map(|i| decode_pvq_vector(2, 2, i).unwrap())
            .collect();
        got.sort();
        let mut expected = vec![
            vec![-2, 0],
            vec![-1, -1],
            vec![-1, 1],
            vec![0, -2],
            vec![0, 2],
            vec![1, -1],
            vec![1, 1],
            vec![2, 0],
        ];
        expected.sort();
        assert_eq!(got, expected);
        // Each has L1 = 2.
        for v in &got {
            assert_eq!(pvq_l1_norm(v), 2);
        }
    }

    #[test]
    fn n_three_k_two_full_codebook_count_and_norm() {
        // V(3, 2) = 18. Don't hand-enumerate all 18, but verify the
        // count, the L1 invariant, and injectivity.
        use std::collections::HashSet;
        let v = pvq_codebook_size(3, 2).unwrap();
        assert_eq!(v, 18);
        let mut seen = HashSet::new();
        for index in 0..v {
            let x = decode_pvq_vector(3, 2, index).unwrap();
            assert_eq!(pvq_l1_norm(&x), 2);
            assert!(seen.insert(x));
        }
        assert_eq!(seen.len(), 18);
    }

    // ---- §4.3.4.2 sign symmetry --------------------------------------------

    #[test]
    fn index_zero_is_all_positive_leading_pulse() {
        // By the step-2 "i < p ⇒ sgn = +1" rule, index 0 always takes
        // the positive branch on the first non-zero coordinate. For
        // K ≥ 1, N ≥ 1, index 0 places all K pulses on coordinate 0
        // with a positive sign (it is the lexicographically-first
        // configuration the walk reaches).
        for n in 1..=6u32 {
            for k in 1..=6u32 {
                let x = decode_pvq_vector(n, k, 0).unwrap();
                assert_eq!(x[0], k as i32, "index-0 leading pulse at (N={n}, K={k})");
                for (idx, &val) in x.iter().enumerate().skip(1) {
                    assert_eq!(val, 0, "index-0 trailing coord {idx} at (N={n}, K={k})");
                }
            }
        }
    }

    #[test]
    fn last_index_is_all_negative_leading_pulse() {
        // The last index V(N, K) - 1 takes the sgn = -1 branch on the
        // first coordinate (i >= p) and, like index 0 mirrored, places
        // all K pulses negatively on the last coordinate's sign-flipped
        // counterpart. We assert the milder property that its L1 = K
        // and that coordinate 0 is non-positive (the -1 branch fired).
        for n in 1..=6u32 {
            for k in 1..=6u32 {
                let v = pvq_codebook_size(n, k).unwrap();
                let x = decode_pvq_vector(n, k, v - 1).unwrap();
                assert_eq!(pvq_l1_norm(&x), k as u64);
                // The last codeword's leading coordinate is ≤ 0 because
                // i = V(N,K)-1 ≥ p triggers the negative branch
                // whenever coordinate 0 is non-zero.
                assert!(
                    x[0] <= 0,
                    "last-index leading coord at (N={n}, K={k}): {x:?}"
                );
            }
        }
    }

    // ---- L2 helpers ---------------------------------------------------------

    #[test]
    fn l2_norm_squared_matches_manual() {
        assert_eq!(pvq_l2_norm_squared(&[3, 0]), 9);
        assert_eq!(pvq_l2_norm_squared(&[1, 1]), 2);
        assert_eq!(pvq_l2_norm_squared(&[-2, 1, -1]), 6);
        assert_eq!(pvq_l2_norm_squared(&[]), 0);
    }

    #[test]
    fn l1_norm_matches_manual() {
        assert_eq!(pvq_l1_norm(&[3, 0]), 3);
        assert_eq!(pvq_l1_norm(&[-2, 1, -1]), 4);
        assert_eq!(pvq_l1_norm(&[0, 0, 0]), 0);
    }

    // ---- into-buffer variant ------------------------------------------------

    #[test]
    fn decode_into_matches_allocating_variant() {
        for n in 1..=5u32 {
            for k in 0..=5u32 {
                let v = pvq_codebook_size(n, k).unwrap();
                for index in 0..v {
                    let owned = decode_pvq_vector(n, k, index).unwrap();
                    let mut buf = vec![0i32; n as usize + 3];
                    let written = decode_pvq_vector_into(n, k, index, &mut buf).unwrap();
                    assert_eq!(written, n as usize);
                    assert_eq!(&buf[..n as usize], owned.as_slice());
                    // Trailing slots left untouched.
                    assert_eq!(&buf[n as usize..], &[0, 0, 0]);
                }
            }
        }
    }

    #[test]
    fn decode_into_rejects_short_buffer() {
        let mut buf = vec![0i32; 2];
        let result = decode_pvq_vector_into(3, 2, 0, &mut buf);
        assert_eq!(
            result,
            Err(PvqDecodeError::OutputBufferTooSmall {
                required: 3,
                provided: 2,
            })
        );
    }

    #[test]
    fn decode_into_exact_length_buffer_ok() {
        let mut buf = vec![0i32; 3];
        let written = decode_pvq_vector_into(3, 2, 5, &mut buf).unwrap();
        assert_eq!(written, 3);
        assert_eq!(pvq_l1_norm(&buf), 2);
    }

    // ---- index-out-of-range rejection --------------------------------------

    #[test]
    fn rejects_index_equal_to_codebook_size() {
        let v = pvq_codebook_size(3, 2).unwrap();
        let result = decode_pvq_vector(3, 2, v);
        assert_eq!(
            result,
            Err(PvqDecodeError::IndexOutOfRange {
                index: v,
                codebook_size: v,
            })
        );
    }

    #[test]
    fn rejects_index_above_codebook_size() {
        let v = pvq_codebook_size(2, 3).unwrap();
        let result = decode_pvq_vector(2, 3, v + 100);
        assert_eq!(
            result,
            Err(PvqDecodeError::IndexOutOfRange {
                index: v + 100,
                codebook_size: v,
            })
        );
    }

    #[test]
    fn last_valid_index_is_accepted() {
        let v = pvq_codebook_size(4, 3).unwrap();
        let x = decode_pvq_vector(4, 3, v - 1).unwrap();
        assert_eq!(pvq_l1_norm(&x), 3);
    }

    // ---- codebook-size error propagation -----------------------------------

    #[test]
    fn propagates_n_out_of_range() {
        let result = decode_pvq_vector(PVQ_V_N_MAX + 1, 2, 0);
        match result {
            Err(PvqDecodeError::CodebookSize(PvqVError::NOutOfRange { .. })) => {}
            other => panic!("expected CodebookSize(NOutOfRange), got {other:?}"),
        }
    }

    #[test]
    fn propagates_k_out_of_range() {
        let result = decode_pvq_vector(4, PVQ_V_K_MAX + 1, 0);
        match result {
            Err(PvqDecodeError::CodebookSize(PvqVError::KOutOfRange { .. })) => {}
            other => panic!("expected CodebookSize(KOutOfRange), got {other:?}"),
        }
    }

    #[test]
    fn propagates_overflow_for_large_codebook() {
        // V(176, 176) overflows the §4.1.5 ec_dec_uint range; the
        // decode must propagate the overflow rather than attempt the
        // walk.
        let result = decode_pvq_vector(176, 176, 0);
        match result {
            Err(PvqDecodeError::CodebookSize(PvqVError::OverflowsDecUintRange { .. })) => {}
            other => panic!("expected CodebookSize(OverflowsDecUintRange), got {other:?}"),
        }
    }

    // ---- N = 0 edge ---------------------------------------------------------

    #[test]
    fn n_zero_k_zero_empty_vector() {
        // V(0, 0) = 1; the single codeword is the empty vector.
        let x = decode_pvq_vector(0, 0, 0).unwrap();
        assert!(x.is_empty());
    }

    #[test]
    fn n_zero_k_positive_has_no_codewords() {
        // V(0, K) = 0 for K ≥ 1: index 0 is already out of range.
        let result = decode_pvq_vector(0, 3, 0);
        assert_eq!(
            result,
            Err(PvqDecodeError::IndexOutOfRange {
                index: 0,
                codebook_size: 0,
            })
        );
    }

    // ---- larger-N spot check ------------------------------------------------

    #[test]
    fn larger_band_spot_check_l1_invariant() {
        // A handful of points at a larger N (a realistic small CELT
        // band) — full enumeration is too large, so sweep a stride of
        // indices and check the L1 = K invariant and length.
        let n = 16u32;
        let k = 4u32;
        let v = pvq_codebook_size(n, k).unwrap();
        let stride = (v / 97).max(1);
        let mut index = 0u32;
        while index < v {
            let x = decode_pvq_vector(n, k, index).unwrap();
            assert_eq!(x.len(), n as usize);
            assert_eq!(pvq_l1_norm(&x), k as u64, "L1 at (N={n}, K={k}, i={index})");
            index += stride;
        }
        // Always cover the last index.
        let x_last = decode_pvq_vector(n, k, v - 1).unwrap();
        assert_eq!(pvq_l1_norm(&x_last), k as u64);
    }

    // ---- constant pins ------------------------------------------------------

    #[test]
    fn mirrored_bounds_match_pvq_v() {
        assert_eq!(PVQ_DECODE_N_MAX, PVQ_V_N_MAX);
        assert_eq!(PVQ_DECODE_K_MAX, PVQ_V_K_MAX);
    }

    // ---- error-Display sanity ----------------------------------------------

    #[test]
    fn display_messages_mention_the_failing_input() {
        let oob = PvqDecodeError::IndexOutOfRange {
            index: 50,
            codebook_size: 18,
        };
        let msg = format!("{oob}");
        assert!(msg.contains("50"));
        assert!(msg.contains("18"));
        assert!(msg.contains("4.3.4.2"));

        let small = PvqDecodeError::OutputBufferTooSmall {
            required: 7,
            provided: 3,
        };
        let msg = format!("{small}");
        assert!(msg.contains('7'));
        assert!(msg.contains('3'));

        let cb = PvqDecodeError::CodebookSize(PvqVError::OverflowsDecUintRange { n: 176, k: 176 });
        let msg = format!("{cb}");
        assert!(msg.contains("176"));
    }

    #[test]
    fn from_pvq_v_error_conversion() {
        let e: PvqDecodeError = PvqVError::NOutOfRange {
            provided: 999,
            max: PVQ_V_N_MAX,
        }
        .into();
        assert!(matches!(
            e,
            PvqDecodeError::CodebookSize(PvqVError::NOutOfRange { .. })
        ));
    }
}
