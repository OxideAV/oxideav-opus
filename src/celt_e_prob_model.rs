//! CELT coarse-energy Laplace-model parameter surface
//! (RFC 6716 §4.3.2.1, pp. 108–109).
//!
//! The §4.3.2.1 *coarse energy* of each CELT band is coded as the
//! Laplace-distributed difference between the band's 6 dB-quantised
//! log-energy and a 2-D predictor running both in time (across frames)
//! and in frequency (across bands). The decoder needs three pieces of
//! data to drive that decode:
//!
//! 1. The intra-frame prediction coefficients `(alpha, beta)`. RFC 6716
//!    §4.3.2.1 (p. 108) fixes the *intra* case at `alpha = 0` and
//!    `beta = 4915 / 32768` (Q15). The *inter* coefficients depend on
//!    the frame size; their numeric values are not given in the RFC
//!    body and are a documented gap for this module.
//! 2. The `e_prob_model` table — the per-band, per-mode parameters of
//!    the Laplace distribution. The RFC describes the table as keyed
//!    by `(LM, intra, band)` where `LM = log2(frame_size / 120)` so
//!    `LM = 0,1,2,3` selects the 120/240/480/960-sample CELT frame
//!    sizes, `intra ∈ {0,1}` selects inter vs. intra mode, and `band
//!    ∈ 0..21` indexes the §4.3 Table 55 MDCT bands. Each `(LM, intra,
//!    band)` triple yields a `{probability, decay}` Q8 pair (the
//!    probability of decoding a zero from the Laplace model, plus the
//!    geometric-decay rate for non-zero values).
//! 3. The `ec_laplace_decode` routine that actually consumes the
//!    range-coded symbol. This module owns only the *parameter
//!    surface* — the table lookup that hands `ec_laplace_decode` its
//!    `(prob, decay)` Q8 pair. The Laplace decoder itself, the 2-D
//!    predictor application, and the §4.3.2.2 fine-energy follow-up
//!    are out of scope for this module.
//!
//! The §4.3.2.1 narrative is verbatim transcribed from RFC 6716,
//! `docs/audio/opus/rfc6716-opus.txt`, pp. 108–109. The 336-byte
//! `e_prob_model` table data is uncopyrightable numeric facts
//! extracted into `docs/audio/celt/tables/e_prob_model.csv`
//! (see `docs/audio/celt/spec/celt-coarse-energy-and-allocation.md`
//! §1.2 for the canonical layout). The values are reproduced inline
//! here so the table is available without filesystem I/O at runtime.
//!
//! ## Layout
//!
//! [`E_PROB_MODEL`] is a `[[[u8; 42]; 2]; 4]`:
//!
//! * outer axis (`LM`): 4 CELT frame sizes (120/240/480/960 samples).
//! * middle axis (`intra`): `0 = inter`, `1 = intra` per §4.3.2.1.
//! * inner axis: the 21 Table 55 bands, with the two Q8 bytes
//!   `[prob_band_0, decay_band_0, prob_band_1, decay_band_1, ..., prob_band_20, decay_band_20]`
//!   packed in band-ascending order.
//!
//! The CSV row index `(2*LM + intra)` and the CSV column ordering
//! `lm,intra,prob0,decay0,...,prob20,decay20` from the
//! `e_prob_model.csv` extract correspond exactly to this layout.

use crate::celt_band_layout::CELT_NUM_BANDS;

/// Number of CELT frame sizes that index the `e_prob_model` outer axis
/// (`LM ∈ {0,1,2,3}` per §4.3.2.1 = 2.5 / 5 / 10 / 20 ms).
pub const E_PROB_MODEL_LM_COUNT: usize = 4;

/// Number of prediction modes per (LM, band) cell (§4.3.2.1:
/// `0 = inter`, `1 = intra`).
pub const E_PROB_MODEL_MODE_COUNT: usize = 2;

/// Index into `e_prob_model[LM][mode]` selecting the **inter**-frame
/// prediction parameters (§4.3.2.1: the prior frame's final fine
/// quantisation participates in the predictor).
pub const E_PROB_MODEL_MODE_INTER: usize = 0;

/// Index into `e_prob_model[LM][mode]` selecting the **intra**-frame
/// prediction parameters (§4.3.2.1: `alpha = 0`, the prior frame
/// drops out, only the in-frame frequency predictor runs).
pub const E_PROB_MODEL_MODE_INTRA: usize = 1;

/// Two bytes per band: `[prob, decay]` Q8 pair feeding
/// `ec_laplace_decode` (§4.3.2.1).
pub const E_PROB_MODEL_BYTES_PER_BAND: usize = 2;

/// 42 bytes per `(LM, mode)` row = 21 bands × 2 bytes per band.
pub const E_PROB_MODEL_BYTES_PER_ROW: usize = CELT_NUM_BANDS * E_PROB_MODEL_BYTES_PER_BAND;

/// Total table footprint: 4 × 2 × 42 = 336 bytes.
pub const E_PROB_MODEL_TOTAL_BYTES: usize =
    E_PROB_MODEL_LM_COUNT * E_PROB_MODEL_MODE_COUNT * E_PROB_MODEL_BYTES_PER_ROW;

/// §4.3.2.1 *intra-frame* prediction coefficient `beta`, fixed at
/// `4915 / 32768` per RFC 6716 §4.3.2.1 (p. 108). Stored as the Q15
/// numerator (denominator implicit).
pub const INTRA_PRED_BETA_Q15: u16 = 4915;

/// Q15 fixed-point denominator paired with [`INTRA_PRED_BETA_Q15`].
pub const Q15_ONE: u32 = 32768;

/// §4.3.2.1 *intra-frame* prediction coefficient `alpha`, fixed at
/// `0` per RFC 6716 §4.3.2.1 (p. 108). Exposed as a Q15 numerator
/// against [`Q15_ONE`] for symmetry with [`INTRA_PRED_BETA_Q15`].
pub const INTRA_PRED_ALPHA_Q15: u16 = 0;

/// §4.3.2.1 Laplace-model `(prob, decay)` Q8 pair for a single band.
///
/// `prob` is the probability of `0` returned by the Laplace decoder
/// (in Q8, so `255 ≈ 0.996`); `decay` is the geometric-decay rate of
/// the non-zero tail (also Q8). Both fields are unsigned bytes per
/// the §4.3.2.1 narrative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EProbPair {
    /// Probability of `0` returned by `ec_laplace_decode` (Q8).
    pub prob: u8,
    /// Geometric-decay rate of the Laplace tail (Q8).
    pub decay: u8,
}

/// §4.3.2.1 coarse-energy prediction mode selector.
///
/// The §4.3.2.1 `intra` flag in the CELT header (decoded by
/// [`crate::celt_header::CeltHeaderPrefix`]) routes to one of these
/// two cases. The selector is the inner-axis index into
/// [`E_PROB_MODEL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnergyPredictionMode {
    /// Inter-frame prediction (the default). §4.3.2.1: the predictor
    /// runs across the prior frame's final fine quantisation; the
    /// `alpha` coefficient depends on the frame size and is a
    /// documented gap for this module.
    Inter,
    /// Intra-frame prediction (the §4.3.2.1 carve-out signalled by the
    /// CELT header `intra` flag). `alpha = 0` and `beta = 4915/32768`;
    /// the prior frame drops out of the predictor entirely.
    Intra,
}

impl EnergyPredictionMode {
    /// Decode the §4.3.2.1 `intra` header bit into a mode selector.
    ///
    /// `intra_flag = true` → [`EnergyPredictionMode::Intra`];
    /// `intra_flag = false` → [`EnergyPredictionMode::Inter`].
    pub const fn from_intra_flag(intra_flag: bool) -> Self {
        if intra_flag {
            EnergyPredictionMode::Intra
        } else {
            EnergyPredictionMode::Inter
        }
    }

    /// Inner-axis index into [`E_PROB_MODEL`].
    pub const fn table_index(self) -> usize {
        match self {
            EnergyPredictionMode::Inter => E_PROB_MODEL_MODE_INTER,
            EnergyPredictionMode::Intra => E_PROB_MODEL_MODE_INTRA,
        }
    }
}

/// §4.3.2.1 `e_prob_model` table — 4 frame sizes × 2 modes × 21 bands
/// × `{prob, decay}` Q8 pair.
///
/// Indexing convention: `E_PROB_MODEL[LM][mode][band * 2 + 0]` = `prob`,
/// `E_PROB_MODEL[LM][mode][band * 2 + 1]` = `decay`. Use
/// [`e_prob_pair`] for a typed accessor.
///
/// Data provenance: `docs/audio/celt/tables/e_prob_model.csv` (Q8
/// numeric facts; see the CSV's `.meta` sidecar for the canonical
/// layout). RFC 6716 §4.3.2.1 names the table `e_prob_model` and
/// describes it as held in `quant_bands.c`; only the numeric data is
/// reproduced here.
pub const E_PROB_MODEL: [[[u8; E_PROB_MODEL_BYTES_PER_ROW]; E_PROB_MODEL_MODE_COUNT];
    E_PROB_MODEL_LM_COUNT] = [
    // LM = 0 (120-sample frame, 2.5 ms at 48 kHz)
    [
        // inter
        [
            72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128, 64, 128, 92, 78, 92, 79,
            92, 78, 90, 79, 116, 41, 115, 40, 114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10,
            177, 11,
        ],
        // intra
        [
            24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132, 55, 132, 61, 114, 70,
            96, 74, 88, 75, 88, 87, 74, 89, 66, 91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43,
            78, 50,
        ],
    ],
    // LM = 1 (240-sample frame, 5 ms at 48 kHz)
    [
        // inter
        [
            83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74, 93, 74, 109, 40, 114, 36, 117,
            34, 117, 34, 143, 17, 145, 18, 146, 19, 162, 12, 165, 10, 178, 7, 189, 6, 190, 8, 177,
            9,
        ],
        // intra
        [
            23, 178, 54, 115, 63, 102, 66, 98, 69, 99, 74, 89, 71, 91, 73, 91, 78, 89, 86, 80, 92,
            66, 93, 64, 102, 59, 103, 60, 104, 60, 117, 52, 123, 44, 138, 35, 133, 31, 97, 38, 77,
            45,
        ],
    ],
    // LM = 2 (480-sample frame, 10 ms at 48 kHz)
    [
        // inter
        [
            61, 90, 93, 60, 105, 42, 107, 41, 110, 45, 116, 38, 113, 38, 112, 38, 124, 26, 132, 27,
            136, 19, 140, 20, 155, 14, 159, 16, 158, 18, 170, 13, 177, 10, 187, 8, 192, 6, 175, 9,
            159, 10,
        ],
        // intra
        [
            21, 178, 59, 110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73, 87, 72, 92, 75, 98, 72, 105,
            58, 107, 54, 115, 52, 114, 55, 112, 56, 129, 51, 132, 40, 150, 33, 140, 29, 98, 35, 77,
            42,
        ],
    ],
    // LM = 3 (960-sample frame, 20 ms at 48 kHz)
    [
        // inter
        [
            42, 121, 96, 66, 108, 43, 111, 40, 117, 44, 123, 32, 120, 36, 119, 33, 127, 33, 134,
            34, 139, 21, 147, 23, 152, 20, 158, 25, 154, 26, 166, 21, 173, 16, 184, 13, 184, 10,
            150, 13, 139, 15,
        ],
        // intra
        [
            22, 178, 63, 114, 74, 82, 84, 83, 92, 82, 103, 62, 96, 72, 96, 67, 101, 73, 107, 72,
            113, 55, 118, 52, 125, 52, 118, 52, 117, 55, 135, 49, 137, 39, 157, 32, 145, 29, 97,
            33, 77, 40,
        ],
    ],
];

/// Errors returned by [`e_prob_pair`] for out-of-range indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EProbModelError {
    /// `LM` is outside `0..4` (§4.3.2.1 only defines four frame
    /// sizes).
    LmOutOfRange { lm: u32 },
    /// `band` is outside `0..21` (the Table 55 band count).
    BandOutOfRange { band: u32 },
}

/// Look up the Laplace `(prob, decay)` Q8 pair for one CELT band.
///
/// `lm` is `log2(frame_size/120) ∈ 0..=3`; `mode` selects inter vs.
/// intra; `band` is the §4.3 Table 55 band index `0..=20`. Returns
/// an [`EProbPair`] holding the pair the §4.3.2.1
/// `ec_laplace_decode` would consume for this `(LM, mode, band)`.
pub fn e_prob_pair(
    lm: u32,
    mode: EnergyPredictionMode,
    band: u32,
) -> Result<EProbPair, EProbModelError> {
    if lm >= E_PROB_MODEL_LM_COUNT as u32 {
        return Err(EProbModelError::LmOutOfRange { lm });
    }
    if band >= CELT_NUM_BANDS as u32 {
        return Err(EProbModelError::BandOutOfRange { band });
    }
    let row = &E_PROB_MODEL[lm as usize][mode.table_index()];
    let off = (band as usize) * E_PROB_MODEL_BYTES_PER_BAND;
    Ok(EProbPair {
        prob: row[off],
        decay: row[off + 1],
    })
}

/// Borrow the full 42-byte `(prob, decay)` row for a single
/// `(LM, mode)` cell of [`E_PROB_MODEL`].
///
/// This is the §4.3.2.1 "one row of 21 `{prob,decay}` pairs"
/// (`docs/audio/celt/tables/e_prob_model.csv` row layout). Returned
/// as a borrowed slice so callers may iterate the band loop without
/// re-indexing.
pub fn e_prob_row(
    lm: u32,
    mode: EnergyPredictionMode,
) -> Result<&'static [u8; E_PROB_MODEL_BYTES_PER_ROW], EProbModelError> {
    if lm >= E_PROB_MODEL_LM_COUNT as u32 {
        return Err(EProbModelError::LmOutOfRange { lm });
    }
    Ok(&E_PROB_MODEL[lm as usize][mode.table_index()])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Table-shape invariants ----

    #[test]
    fn table_shape_constants_match_struct() {
        assert_eq!(E_PROB_MODEL_LM_COUNT, 4);
        assert_eq!(E_PROB_MODEL_MODE_COUNT, 2);
        assert_eq!(E_PROB_MODEL_BYTES_PER_BAND, 2);
        assert_eq!(E_PROB_MODEL_BYTES_PER_ROW, 42);
        assert_eq!(E_PROB_MODEL_TOTAL_BYTES, 336);
    }

    #[test]
    fn table_inner_row_length_matches_band_count_times_two() {
        for (lm, by_lm) in E_PROB_MODEL.iter().enumerate() {
            for (mode, row) in by_lm.iter().enumerate() {
                assert_eq!(
                    row.len(),
                    E_PROB_MODEL_BYTES_PER_ROW,
                    "(lm={lm},mode={mode}) inner row length mismatch"
                );
                assert_eq!(
                    row.len(),
                    CELT_NUM_BANDS * 2,
                    "row should be 21 bands × 2 bytes"
                );
            }
        }
    }

    #[test]
    fn table_total_bytes_matches_lm_times_mode_times_row() {
        let total: usize = E_PROB_MODEL
            .iter()
            .map(|by_lm| by_lm.iter().map(|row| row.len()).sum::<usize>())
            .sum();
        assert_eq!(total, E_PROB_MODEL_TOTAL_BYTES);
    }

    // ---- Intra prediction coefficients (RFC 6716 §4.3.2.1 p.108) ----

    #[test]
    fn intra_alpha_is_zero_per_rfc() {
        assert_eq!(INTRA_PRED_ALPHA_Q15, 0);
    }

    #[test]
    fn intra_beta_is_4915_over_32768_per_rfc() {
        assert_eq!(INTRA_PRED_BETA_Q15, 4915);
        assert_eq!(Q15_ONE, 32768);
        // The Q15 ratio 4915/32768 = 0.14999389648437500 — within
        // ~6.1e-6 of the RFC's textual 0.15 approximation. We don't
        // assert a float here; we pin the numerator/denominator.
    }

    // ---- EnergyPredictionMode mapping ----

    #[test]
    fn intra_flag_true_routes_to_intra() {
        assert_eq!(
            EnergyPredictionMode::from_intra_flag(true),
            EnergyPredictionMode::Intra
        );
    }

    #[test]
    fn intra_flag_false_routes_to_inter() {
        assert_eq!(
            EnergyPredictionMode::from_intra_flag(false),
            EnergyPredictionMode::Inter
        );
    }

    #[test]
    fn mode_table_indices_match_csv_layout() {
        assert_eq!(EnergyPredictionMode::Inter.table_index(), 0);
        assert_eq!(EnergyPredictionMode::Intra.table_index(), 1);
        assert_eq!(
            EnergyPredictionMode::Inter.table_index(),
            E_PROB_MODEL_MODE_INTER
        );
        assert_eq!(
            EnergyPredictionMode::Intra.table_index(),
            E_PROB_MODEL_MODE_INTRA
        );
    }

    // ---- Spot-check the Q8 values against the CSV extract ----
    //
    // These pins reproduce a hand-picked sample from
    // `docs/audio/celt/tables/e_prob_model.csv` so a future edit that
    // accidentally reorders the table or drops a byte trips the test
    // suite. Each row references the CSV row + the column position of
    // the byte.

    #[test]
    fn csv_row_0_lm0_inter_first_pair_band_0() {
        // CSV row 0: "0,0,72,127,..." — LM=0, intra=0, band 0 = (72, 127).
        let p = e_prob_pair(0, EnergyPredictionMode::Inter, 0).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 72,
                decay: 127
            }
        );
    }

    #[test]
    fn csv_row_0_lm0_inter_last_pair_band_20() {
        // CSV row 0 final pair: "...,177,11" — band 20 = (177, 11).
        let p = e_prob_pair(0, EnergyPredictionMode::Inter, 20).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 177,
                decay: 11
            }
        );
    }

    #[test]
    fn csv_row_1_lm0_intra_first_pair_band_0() {
        // CSV row 1: "0,1,24,179,..." — LM=0, intra=1, band 0 = (24, 179).
        let p = e_prob_pair(0, EnergyPredictionMode::Intra, 0).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 24,
                decay: 179
            }
        );
    }

    #[test]
    fn csv_row_3_lm1_intra_band_5() {
        // CSV row 3: "1,1,23,178,54,115,63,102,66,98,69,99,74,89,..."
        // → band 5 (the 6th band) `(prob, decay) = (74, 89)`.
        let p = e_prob_pair(1, EnergyPredictionMode::Intra, 5).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 74,
                decay: 89
            }
        );
    }

    #[test]
    fn csv_row_4_lm2_inter_band_10() {
        // CSV row 4: "2,0,61,90,93,60,105,42,107,41,110,45,116,38,113,38,112,38,124,26,132,27,136,19,..."
        // → band 10 (11th band) = pair starting at column 22 → (136, 19).
        let p = e_prob_pair(2, EnergyPredictionMode::Inter, 10).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 136,
                decay: 19
            }
        );
    }

    #[test]
    fn csv_row_6_lm3_inter_first_pair_band_0() {
        // CSV row 6: "3,0,42,121,..." — LM=3, intra=0, band 0 = (42, 121).
        let p = e_prob_pair(3, EnergyPredictionMode::Inter, 0).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 42,
                decay: 121
            }
        );
    }

    #[test]
    fn csv_row_7_lm3_intra_last_pair_band_20() {
        // CSV row 7 final pair "...,77,40" — band 20 = (77, 40).
        let p = e_prob_pair(3, EnergyPredictionMode::Intra, 20).unwrap();
        assert_eq!(
            p,
            EProbPair {
                prob: 77,
                decay: 40
            }
        );
    }

    // ---- Error-path coverage ----

    #[test]
    fn e_prob_pair_rejects_lm_out_of_range() {
        let err = e_prob_pair(4, EnergyPredictionMode::Inter, 0).unwrap_err();
        assert_eq!(err, EProbModelError::LmOutOfRange { lm: 4 });
        let err = e_prob_pair(u32::MAX, EnergyPredictionMode::Intra, 0).unwrap_err();
        assert_eq!(err, EProbModelError::LmOutOfRange { lm: u32::MAX });
    }

    #[test]
    fn e_prob_pair_rejects_band_out_of_range() {
        let err = e_prob_pair(0, EnergyPredictionMode::Inter, 21).unwrap_err();
        assert_eq!(err, EProbModelError::BandOutOfRange { band: 21 });
        let err = e_prob_pair(2, EnergyPredictionMode::Intra, 100).unwrap_err();
        assert_eq!(err, EProbModelError::BandOutOfRange { band: 100 });
    }

    #[test]
    fn e_prob_row_returns_full_42_byte_row() {
        let row = e_prob_row(0, EnergyPredictionMode::Inter).unwrap();
        assert_eq!(row.len(), 42);
        // First two bytes are the band-0 pair `(72, 127)`.
        assert_eq!(row[0], 72);
        assert_eq!(row[1], 127);
        // Last two bytes are the band-20 pair `(177, 11)`.
        assert_eq!(row[40], 177);
        assert_eq!(row[41], 11);
    }

    #[test]
    fn e_prob_row_rejects_lm_out_of_range() {
        let err = e_prob_row(99, EnergyPredictionMode::Inter).unwrap_err();
        assert_eq!(err, EProbModelError::LmOutOfRange { lm: 99 });
    }

    // ---- Property-style sweeps over the full table surface ----

    #[test]
    fn every_lm_mode_band_lookup_succeeds() {
        for lm in 0..E_PROB_MODEL_LM_COUNT as u32 {
            for mode in [EnergyPredictionMode::Inter, EnergyPredictionMode::Intra] {
                for band in 0..CELT_NUM_BANDS as u32 {
                    let p = e_prob_pair(lm, mode, band).unwrap_or_else(|e| {
                        panic!("lookup failed for (lm={lm},mode={mode:?},band={band}): {e:?}")
                    });
                    // Sanity: prob and decay are stored as u8, so
                    // each field naturally satisfies 0..=255; nothing
                    // further to assert at the type level.
                    let _ = p.prob;
                    let _ = p.decay;
                }
            }
        }
    }

    #[test]
    fn pair_lookup_matches_row_lookup_for_every_cell() {
        for lm in 0..E_PROB_MODEL_LM_COUNT as u32 {
            for mode in [EnergyPredictionMode::Inter, EnergyPredictionMode::Intra] {
                let row = e_prob_row(lm, mode).unwrap();
                for band in 0..CELT_NUM_BANDS as u32 {
                    let pair = e_prob_pair(lm, mode, band).unwrap();
                    let off = (band as usize) * 2;
                    assert_eq!(
                        pair.prob, row[off],
                        "(lm={lm},mode={mode:?},band={band}) prob mismatch"
                    );
                    assert_eq!(
                        pair.decay,
                        row[off + 1],
                        "(lm={lm},mode={mode:?},band={band}) decay mismatch"
                    );
                }
            }
        }
    }

    #[test]
    fn intra_rows_have_lower_band0_probability_than_inter() {
        // Sanity property derived from §4.3.2.1: the intra rows are
        // the "no time predictor" case, which leaves wider Laplace
        // tails for the first band (prediction is least effective at
        // band 0). The CSV-extracted data should reflect that —
        // band-0 `prob` is markedly lower in the intra row than the
        // inter row for every LM.
        for lm in 0..E_PROB_MODEL_LM_COUNT as u32 {
            let inter = e_prob_pair(lm, EnergyPredictionMode::Inter, 0).unwrap();
            let intra = e_prob_pair(lm, EnergyPredictionMode::Intra, 0).unwrap();
            assert!(
                intra.prob < inter.prob,
                "(lm={lm}) intra band-0 prob {} should be < inter band-0 prob {}",
                intra.prob,
                inter.prob
            );
        }
    }
}
