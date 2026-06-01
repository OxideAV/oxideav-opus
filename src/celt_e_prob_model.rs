//! CELT coarse-energy Laplace probability model — RFC 6716 §4.3.2.1,
//! `e_prob_model`.
//!
//! ## What this module owns
//!
//! The §4.3.2.1 coarse-energy decoder codes each band's
//! prediction-error symbol with a Laplace distribution whose
//! parameters depend on:
//!
//! * the frame-size shift `LM = log2(frame_size / 120)` (so
//!   `LM ∈ {0, 1, 2, 3}` selects the 120 / 240 / 480 / 960-sample
//!   frame sizes; equivalently 2.5 / 5 / 10 / 20 ms at the §4.3 CELT
//!   internal rate of 48 kHz);
//! * a binary prediction-type flag picked off the §4.3.2.1
//!   `intra` symbol (= 0 ⇒ inter, the 2-D z-transform path that uses
//!   the prior frame's final-fine coarse-energy state, with mode-
//!   dependent `(alpha, beta)`; = 1 ⇒ intra, no time prediction, fixed
//!   `alpha = 0` and `beta = 4915/32768`); and
//! * the band index `b ∈ 0..=20`.
//!
//! For each `(LM, intra, b)` triple the table emits a `{prob, decay}`
//! Q8 pair where `prob` is the probability-of-zero parameter and
//! `decay` is the geometric decay rate consumed by the
//! `ec_laplace_decode()` primitive (RFC 6716 §4.3.2.1).
//!
//! ## What this module deliberately does **not** own
//!
//! * The `ec_laplace_decode()` primitive itself — it consumes the
//!   pair this module returns plus a range-coder, and lands in a
//!   subsequent round.
//! * The §4.3.2.1 `intra` flag *decoder*. The flag is part of the
//!   §4.3 / Table 56 prefix and already lives in
//!   [`crate::celt_header::CeltHeaderPrefix`].
//! * The full §4.3.2.1 `unquant_coarse_energy()` driver (which mixes
//!   the per-band Laplace decode with the 2-D prediction filter to
//!   reconstruct the coarse-energy envelope). That driver depends on
//!   this module + the Laplace primitive + the prediction filter and
//!   sits one round downstream.
//! * The §4.3.2.2 fine-energy refinement and §4.3.3 bit allocation
//!   (`cache_caps50` / `LOG2_FRAC_TABLE`), which are independent
//!   blockers tracked separately.
//!
//! ## Provenance
//!
//! The numeric table is sourced from `docs/audio/celt/tables/e_prob_model.csv`
//! (the per-`(LM, intra)` 42-byte row layout described in
//! `docs/audio/celt/tables/e_prob_model.meta`). The CSV is an
//! uncopyrightable-facts extraction (Feist v. Rural) under the OxideAV
//! clean-room workspace; the canonical name `e_prob_model` is the
//! identifier RFC 6716 §4.3.2.1 itself uses for the table.
//!
//! Algorithm narrative for the prediction filter, the `(alpha, beta)`
//! mode-dependent coefficients in the inter case, and the
//! `(alpha = 0, beta = 4915/32768)` intra-case coefficients is from
//! RFC 6716 §4.3.2.1 (`docs/audio/opus/rfc6716-opus.txt`) and the
//! companion clean-room narrative
//! `docs/audio/celt/spec/celt-coarse-energy-and-allocation.md` §1.
//!
//! No external library source consulted, quoted, paraphrased, or
//! used as a cross-check oracle.

use crate::celt_band_layout::{CeltFrameSize, CELT_NUM_BANDS};

/// Number of frame-size rows in `e_prob_model`. RFC 6716 §4.3.2.1
/// states the Laplace parameters cover the four CELT frame sizes
/// (120 / 240 / 480 / 960 samples ⇔ `LM = 0, 1, 2, 3`).
pub const E_PROB_MODEL_FRAME_SIZE_AXIS: usize = 4;

/// Number of prediction-type axes — inter (`intra = 0`) and intra
/// (`intra = 1`) per RFC 6716 §4.3.2.1.
pub const E_PROB_MODEL_PREDICTION_AXIS: usize = 2;

/// Number of CELT bands the Laplace model parameters cover. Matches
/// [`CELT_NUM_BANDS`]; pinned to the same constant so a future change
/// in the band partition is caught at compile time.
pub const E_PROB_MODEL_BAND_AXIS: usize = CELT_NUM_BANDS;

/// Number of bytes per `(LM, intra, band)` cell — one
/// probability-of-zero byte followed by one decay byte. Both are Q8
/// per RFC 6716 §4.3.2.1.
pub const E_PROB_MODEL_PAIRS_AXIS: usize = 2;

/// Total entry count of `e_prob_model`:
/// `4 × 2 × 21 × 2 = 336` bytes.
pub const E_PROB_MODEL_BYTE_COUNT: usize = E_PROB_MODEL_FRAME_SIZE_AXIS
    * E_PROB_MODEL_PREDICTION_AXIS
    * E_PROB_MODEL_BAND_AXIS
    * E_PROB_MODEL_PAIRS_AXIS;

/// Bytes per `(LM, intra)` row in [`E_PROB_MODEL`] — 21 bands × 2
/// bytes per band. Each row is the full per-band parameter vector
/// at a given frame size and prediction type.
pub const E_PROB_MODEL_ROW_BYTES: usize = E_PROB_MODEL_BAND_AXIS * E_PROB_MODEL_PAIRS_AXIS;

/// Number of distinct `LM` values RFC 6716 §4.3.2.1 enumerates. Same
/// as [`E_PROB_MODEL_FRAME_SIZE_AXIS`] but expressed as the maximum
/// permissible `LM` argument value (= 3) for callers that want to
/// validate a runtime `LM` against the spec.
pub const E_PROB_MODEL_LM_MAX: u8 = (E_PROB_MODEL_FRAME_SIZE_AXIS as u8) - 1;

/// Intra-case `alpha` numerator from RFC 6716 §4.3.2.1
/// (*"alpha = 0, beta = 4915/32768 when using intra energy"*).
pub const INTRA_ALPHA_NUMERATOR: u16 = 0;

/// Intra-case `beta` numerator from RFC 6716 §4.3.2.1.
pub const INTRA_BETA_NUMERATOR: u16 = 4915;

/// Common denominator for the intra-case `(alpha, beta)` Q15-style
/// fractions in RFC 6716 §4.3.2.1.
pub const INTRA_BETA_DENOMINATOR: u32 = 32768;

/// `e_prob_model`, indexed as `[LM][intra][band * 2 + pair_index]`
/// where `pair_index = 0` is the probability-of-zero byte and
/// `pair_index = 1` is the decay byte. Every byte is Q8 per
/// RFC 6716 §4.3.2.1.
///
/// Layout matches the spec exactly so a future-LM extension or a
/// per-band breakpoint change shows up as a single row delta rather
/// than a global re-shape.
///
/// Sourced from `docs/audio/celt/tables/e_prob_model.csv`.
#[rustfmt::skip]
pub const E_PROB_MODEL: [[[u8; E_PROB_MODEL_ROW_BYTES]; E_PROB_MODEL_PREDICTION_AXIS];
    E_PROB_MODEL_FRAME_SIZE_AXIS] = [
    // LM = 0 (120-sample / 2.5 ms frames)
    [
        // intra = 0 (inter)
        [
             72,127, 65,129, 66,128, 65,128, 64,128, 62,128, 64,128,
             64,128, 92, 78, 92, 79, 92, 78, 90, 79,116, 41,115, 40,
            114, 40,132, 26,132, 26,145, 17,161, 12,176, 10,177, 11,
        ],
        // intra = 1 (intra)
        [
             24,179, 48,138, 54,135, 54,132, 53,134, 56,133, 55,132,
             55,132, 61,114, 70, 96, 74, 88, 75, 88, 87, 74, 89, 66,
             91, 67,100, 59,108, 50,120, 40,122, 37, 97, 43, 78, 50,
        ],
    ],
    // LM = 1 (240-sample / 5 ms frames)
    [
        // intra = 0 (inter)
        [
             83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74,
             93, 74,109, 40,114, 36,117, 34,117, 34,143, 17,145, 18,
            146, 19,162, 12,165, 10,178,  7,189,  6,190,  8,177,  9,
        ],
        // intra = 1 (intra)
        [
             23,178, 54,115, 63,102, 66, 98, 69, 99, 74, 89, 71, 91,
             73, 91, 78, 89, 86, 80, 92, 66, 93, 64,102, 59,103, 60,
            104, 60,117, 52,123, 44,138, 35,133, 31, 97, 38, 77, 45,
        ],
    ],
    // LM = 2 (480-sample / 10 ms frames)
    [
        // intra = 0 (inter)
        [
             61, 90, 93, 60,105, 42,107, 41,110, 45,116, 38,113, 38,
            112, 38,124, 26,132, 27,136, 19,140, 20,155, 14,159, 16,
            158, 18,170, 13,177, 10,187,  8,192,  6,175,  9,159, 10,
        ],
        // intra = 1 (intra)
        [
             21,178, 59,110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73,
             87, 72, 92, 75, 98, 72,105, 58,107, 54,115, 52,114, 55,
            112, 56,129, 51,132, 40,150, 33,140, 29, 98, 35, 77, 42,
        ],
    ],
    // LM = 3 (960-sample / 20 ms frames)
    [
        // intra = 0 (inter)
        [
             42,121, 96, 66,108, 43,111, 40,117, 44,123, 32,120, 36,
            119, 33,127, 33,134, 34,139, 21,147, 23,152, 20,158, 25,
            154, 26,166, 21,173, 16,184, 13,184, 10,150, 13,139, 15,
        ],
        // intra = 1 (intra)
        [
             22,178, 63,114, 74, 82, 84, 83, 92, 82,103, 62, 96, 72,
             96, 67,101, 73,107, 72,113, 55,118, 52,125, 52,118, 52,
            117, 55,135, 49,137, 39,157, 32,145, 29, 97, 33, 77, 40,
        ],
    ],
];

/// Selects between the two §4.3.2.1 prediction-filter modes. The
/// §4.3.2.1 `intra` Table-56 flag dispatches one of these — see
/// [`crate::celt_header::CeltHeaderPrefix`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EnergyPrediction {
    /// `intra = 0` — time prediction enabled. The
    /// `(alpha, beta)` filter coefficients depend on the frame size
    /// (RFC 6716 §4.3.2.1); the prediction term comes from the
    /// previous frame's final-fine coarse energy.
    Inter = 0,
    /// `intra = 1` — time prediction disabled. RFC 6716 §4.3.2.1
    /// pins the coefficients to `alpha = 0` and
    /// `beta = 4915/32768`; the prediction term is therefore the
    /// purely-spatial within-frame filter and the previous frame's
    /// state is not consulted.
    Intra = 1,
}

impl EnergyPrediction {
    /// `intra` flag value RFC 6716 §4.3.2.1 uses in the table index.
    #[inline]
    pub const fn intra_axis_index(self) -> usize {
        self as usize
    }

    /// Decode the §4.3 / Table 56 `intra` flag (a single binary
    /// symbol) into the matching prediction mode. Provided as a
    /// total function over `bool` so the §4.3 header decoder can
    /// drive it directly without an intermediate `match`.
    #[inline]
    pub const fn from_intra_flag(intra: bool) -> Self {
        if intra {
            EnergyPrediction::Intra
        } else {
            EnergyPrediction::Inter
        }
    }
}

/// Translate a [`CeltFrameSize`] into the §4.3.2.1 `LM` axis index
/// used by [`E_PROB_MODEL`].
///
/// `LM = log2(frame_size_in_samples / 120)` per RFC 6716 §4.3.2.1.
/// At the §4.3 internal rate of 48 kHz the four CELT frame sizes
/// (2.5 / 5 / 10 / 20 ms) map to 120 / 240 / 480 / 960 samples and
/// hence `LM = 0, 1, 2, 3` in the same order as
/// [`CeltFrameSize::column_index`].
#[inline]
pub const fn lm_from_celt_frame_size(frame_size: CeltFrameSize) -> u8 {
    frame_size.column_index() as u8
}

/// Inverse of [`lm_from_celt_frame_size`]. Returns `None` for `LM`
/// values outside the §4.3.2.1 documented range
/// (`0..=`[`E_PROB_MODEL_LM_MAX`]).
#[inline]
pub const fn celt_frame_size_from_lm(lm: u8) -> Option<CeltFrameSize> {
    match lm {
        0 => Some(CeltFrameSize::Ms2_5),
        1 => Some(CeltFrameSize::Ms5),
        2 => Some(CeltFrameSize::Ms10),
        3 => Some(CeltFrameSize::Ms20),
        _ => None,
    }
}

/// Frame size in 48-kHz samples for the given `LM`, i.e.
/// `120 << LM`. Returns `None` for `LM > 3`.
#[inline]
pub const fn frame_size_samples_from_lm(lm: u8) -> Option<u32> {
    if lm > E_PROB_MODEL_LM_MAX {
        return None;
    }
    Some(120u32 << lm)
}

/// `e_prob_model[lm][intra]` 42-byte row (21 bands × `{prob, decay}`).
///
/// Returns `None` for `lm > 3` so callers can validate a runtime
/// `LM` against [`E_PROB_MODEL_LM_MAX`] in one call.
#[inline]
pub const fn e_prob_model_row(
    lm: u8,
    prediction: EnergyPrediction,
) -> Option<&'static [u8; E_PROB_MODEL_ROW_BYTES]> {
    if lm > E_PROB_MODEL_LM_MAX {
        return None;
    }
    Some(&E_PROB_MODEL[lm as usize][prediction.intra_axis_index()])
}

/// `(prob, decay)` Q8 pair from `e_prob_model[lm][intra][band]`.
///
/// Returns `None` if either `lm > 3` or `band >= CELT_NUM_BANDS`.
/// `prob` is the probability-of-zero parameter and `decay` is the
/// geometric decay rate — see RFC 6716 §4.3.2.1 and
/// `docs/audio/celt/spec/celt-coarse-energy-and-allocation.md` §1.2.
#[inline]
pub const fn e_prob_model_pair(
    lm: u8,
    prediction: EnergyPrediction,
    band: usize,
) -> Option<(u8, u8)> {
    if lm > E_PROB_MODEL_LM_MAX || band >= E_PROB_MODEL_BAND_AXIS {
        return None;
    }
    let row = &E_PROB_MODEL[lm as usize][prediction.intra_axis_index()];
    let base = band * E_PROB_MODEL_PAIRS_AXIS;
    Some((row[base], row[base + 1]))
}

/// Probability-of-zero Q8 byte from `e_prob_model[lm][intra][band]`.
/// Returns `None` on out-of-range axes — see [`e_prob_model_pair`].
#[inline]
pub const fn e_prob_model_prob_zero(
    lm: u8,
    prediction: EnergyPrediction,
    band: usize,
) -> Option<u8> {
    match e_prob_model_pair(lm, prediction, band) {
        Some((p, _)) => Some(p),
        None => None,
    }
}

/// Decay-rate Q8 byte from `e_prob_model[lm][intra][band]`. Returns
/// `None` on out-of-range axes — see [`e_prob_model_pair`].
#[inline]
pub const fn e_prob_model_decay(lm: u8, prediction: EnergyPrediction, band: usize) -> Option<u8> {
    match e_prob_model_pair(lm, prediction, band) {
        Some((_, d)) => Some(d),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // Axis constants — pinned to RFC 6716 §4.3.2.1 wording.
    // ----------------------------------------------------------------

    #[test]
    fn axis_constants_match_rfc_section_4_3_2_1() {
        assert_eq!(E_PROB_MODEL_FRAME_SIZE_AXIS, 4);
        assert_eq!(E_PROB_MODEL_PREDICTION_AXIS, 2);
        assert_eq!(E_PROB_MODEL_BAND_AXIS, 21);
        assert_eq!(E_PROB_MODEL_PAIRS_AXIS, 2);
    }

    #[test]
    fn band_axis_matches_celt_num_bands() {
        // Defensive: if the §4.3 band partition ever changes
        // upstream, this catches the drift before the Laplace
        // decoder fans out.
        assert_eq!(E_PROB_MODEL_BAND_AXIS, CELT_NUM_BANDS);
    }

    #[test]
    fn byte_count_matches_axis_product() {
        assert_eq!(E_PROB_MODEL_BYTE_COUNT, 4 * 2 * 21 * 2);
        assert_eq!(E_PROB_MODEL_BYTE_COUNT, 336);
    }

    #[test]
    fn row_bytes_matches_band_pair_product() {
        assert_eq!(E_PROB_MODEL_ROW_BYTES, 21 * 2);
        assert_eq!(E_PROB_MODEL_ROW_BYTES, 42);
    }

    #[test]
    fn lm_max_matches_axis() {
        assert_eq!(
            E_PROB_MODEL_LM_MAX as usize,
            E_PROB_MODEL_FRAME_SIZE_AXIS - 1
        );
    }

    #[test]
    fn intra_coefficients_match_rfc_section_4_3_2_1() {
        // RFC 6716 §4.3.2.1: "alpha = 0, beta = 4915/32768 when
        // using intra energy".
        assert_eq!(INTRA_ALPHA_NUMERATOR, 0);
        assert_eq!(INTRA_BETA_NUMERATOR, 4915);
        assert_eq!(INTRA_BETA_DENOMINATOR, 32768);
        // 4915/32768 ≈ 0.15001 — sanity-check the decimal value
        // the RFC narrative implies (the Laplace decay halves a few
        // dozen times across the band partition, so any single-bit
        // typo in the numerator would push this out of `[0.05, 0.5]`).
        let beta = INTRA_BETA_NUMERATOR as f64 / INTRA_BETA_DENOMINATOR as f64;
        assert!(beta > 0.149 && beta < 0.151);
    }

    // ----------------------------------------------------------------
    // Shape invariants over the whole table.
    // ----------------------------------------------------------------

    #[test]
    fn table_total_byte_count() {
        let mut count = 0usize;
        for row_per_prediction in E_PROB_MODEL.iter() {
            for row in row_per_prediction.iter() {
                count += row.len();
            }
        }
        assert_eq!(count, E_PROB_MODEL_BYTE_COUNT);
    }

    #[test]
    fn every_byte_is_q8() {
        // Make the Q8-ness explicit (every cell fits in 8 bits) so a
        // future re-typing of the table — e.g. swapping `u8` for
        // `i16` — has to walk through this test and re-establish
        // the contract the §4.3.2.1 Laplace decoder relies on.
        let element_type_is_u8 = core::mem::size_of_val(&E_PROB_MODEL[0][0][0]) == 1;
        assert!(element_type_is_u8);
        let mut total = 0usize;
        for row_per_prediction in E_PROB_MODEL.iter() {
            for row in row_per_prediction.iter() {
                total += row.len();
            }
        }
        assert_eq!(total, E_PROB_MODEL_BYTE_COUNT);
    }

    #[test]
    fn every_row_has_42_bytes() {
        for row_per_prediction in E_PROB_MODEL.iter() {
            for row in row_per_prediction.iter() {
                assert_eq!(row.len(), E_PROB_MODEL_ROW_BYTES);
            }
        }
    }

    // ----------------------------------------------------------------
    // Cell-level pins from `docs/audio/celt/tables/e_prob_model.csv`.
    //
    // We pin the four corners + a representative middle cell per row,
    // plus every (LM=0, intra=0) band so the most-frequently-used row
    // is bit-exact against the CSV.
    // ----------------------------------------------------------------

    #[test]
    fn lm0_inter_row_full_pin() {
        // CSV row 2: 0,0,72,127,65,129,...,177,11
        let row = e_prob_model_row(0, EnergyPrediction::Inter).unwrap();
        let expected: [u8; 42] = [
            72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128, 64, 128, 92, 78, 92, 79,
            92, 78, 90, 79, 116, 41, 115, 40, 114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10,
            177, 11,
        ];
        assert_eq!(row, &expected);
    }

    #[test]
    fn lm0_intra_row_full_pin() {
        // CSV row 3: 0,1,24,179,48,138,...,78,50
        let row = e_prob_model_row(0, EnergyPrediction::Intra).unwrap();
        let expected: [u8; 42] = [
            24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132, 55, 132, 61, 114, 70,
            96, 74, 88, 75, 88, 87, 74, 89, 66, 91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43,
            78, 50,
        ];
        assert_eq!(row, &expected);
    }

    #[test]
    fn lm1_inter_corner_pins() {
        // CSV row 4: 1,0,83,78,...,177,9
        assert_eq!(
            e_prob_model_pair(1, EnergyPrediction::Inter, 0),
            Some((83, 78))
        );
        assert_eq!(
            e_prob_model_pair(1, EnergyPrediction::Inter, 20),
            Some((177, 9))
        );
        // Middle band 10 of CSV row 4: ...,117,34,...
        assert_eq!(
            e_prob_model_pair(1, EnergyPrediction::Inter, 10),
            Some((117, 34))
        );
    }

    #[test]
    fn lm1_intra_corner_pins() {
        // CSV row 5: 1,1,23,178,...,77,45
        assert_eq!(
            e_prob_model_pair(1, EnergyPrediction::Intra, 0),
            Some((23, 178))
        );
        assert_eq!(
            e_prob_model_pair(1, EnergyPrediction::Intra, 20),
            Some((77, 45))
        );
        // CSV row 5 band 5: 74,89
        assert_eq!(
            e_prob_model_pair(1, EnergyPrediction::Intra, 5),
            Some((74, 89))
        );
    }

    #[test]
    fn lm2_inter_corner_pins() {
        // CSV row 6: 2,0,61,90,...,159,10
        assert_eq!(
            e_prob_model_pair(2, EnergyPrediction::Inter, 0),
            Some((61, 90))
        );
        assert_eq!(
            e_prob_model_pair(2, EnergyPrediction::Inter, 20),
            Some((159, 10))
        );
        // Band 14 = 158, 18
        assert_eq!(
            e_prob_model_pair(2, EnergyPrediction::Inter, 14),
            Some((158, 18))
        );
    }

    #[test]
    fn lm2_intra_corner_pins() {
        // CSV row 7: 2,1,21,178,...,77,42
        assert_eq!(
            e_prob_model_pair(2, EnergyPrediction::Intra, 0),
            Some((21, 178))
        );
        assert_eq!(
            e_prob_model_pair(2, EnergyPrediction::Intra, 20),
            Some((77, 42))
        );
        // Band 17 = 150, 33
        assert_eq!(
            e_prob_model_pair(2, EnergyPrediction::Intra, 17),
            Some((150, 33))
        );
    }

    #[test]
    fn lm3_inter_corner_pins() {
        // CSV row 8: 3,0,42,121,...,139,15
        assert_eq!(
            e_prob_model_pair(3, EnergyPrediction::Inter, 0),
            Some((42, 121))
        );
        assert_eq!(
            e_prob_model_pair(3, EnergyPrediction::Inter, 20),
            Some((139, 15))
        );
        // Band 8 = 127, 33
        assert_eq!(
            e_prob_model_pair(3, EnergyPrediction::Inter, 8),
            Some((127, 33))
        );
    }

    #[test]
    fn lm3_intra_corner_pins() {
        // CSV row 9: 3,1,22,178,...,77,40
        assert_eq!(
            e_prob_model_pair(3, EnergyPrediction::Intra, 0),
            Some((22, 178))
        );
        assert_eq!(
            e_prob_model_pair(3, EnergyPrediction::Intra, 20),
            Some((77, 40))
        );
        // Band 9 = 107, 72
        assert_eq!(
            e_prob_model_pair(3, EnergyPrediction::Intra, 9),
            Some((107, 72))
        );
    }

    // ----------------------------------------------------------------
    // Out-of-range axes return `None`.
    // ----------------------------------------------------------------

    #[test]
    fn lm_out_of_range_returns_none() {
        assert_eq!(e_prob_model_row(4, EnergyPrediction::Inter), None);
        assert_eq!(e_prob_model_row(255, EnergyPrediction::Intra), None);
        assert_eq!(e_prob_model_pair(4, EnergyPrediction::Inter, 0), None);
        assert_eq!(e_prob_model_pair(7, EnergyPrediction::Intra, 0), None);
        assert_eq!(e_prob_model_prob_zero(4, EnergyPrediction::Inter, 0), None);
        assert_eq!(e_prob_model_decay(4, EnergyPrediction::Inter, 0), None);
    }

    #[test]
    fn band_out_of_range_returns_none() {
        assert_eq!(
            e_prob_model_pair(0, EnergyPrediction::Inter, CELT_NUM_BANDS),
            None
        );
        assert_eq!(e_prob_model_pair(3, EnergyPrediction::Intra, 100), None);
        assert_eq!(
            e_prob_model_prob_zero(0, EnergyPrediction::Inter, CELT_NUM_BANDS),
            None
        );
        assert_eq!(
            e_prob_model_decay(0, EnergyPrediction::Inter, CELT_NUM_BANDS),
            None
        );
    }

    #[test]
    fn band_axis_boundary_in_range() {
        // Last legal band index = 20.
        assert!(e_prob_model_pair(0, EnergyPrediction::Inter, CELT_NUM_BANDS - 1).is_some());
        assert!(e_prob_model_pair(3, EnergyPrediction::Intra, CELT_NUM_BANDS - 1).is_some());
    }

    // ----------------------------------------------------------------
    // Accessor consistency.
    // ----------------------------------------------------------------

    #[test]
    fn prob_decay_helpers_match_pair_helper() {
        for lm in 0..E_PROB_MODEL_FRAME_SIZE_AXIS as u8 {
            for prediction in [EnergyPrediction::Inter, EnergyPrediction::Intra] {
                for band in 0..E_PROB_MODEL_BAND_AXIS {
                    let (p, d) = e_prob_model_pair(lm, prediction, band).unwrap();
                    assert_eq!(e_prob_model_prob_zero(lm, prediction, band), Some(p));
                    assert_eq!(e_prob_model_decay(lm, prediction, band), Some(d));
                }
            }
        }
    }

    #[test]
    fn row_helper_matches_pair_helper() {
        for lm in 0..E_PROB_MODEL_FRAME_SIZE_AXIS as u8 {
            for prediction in [EnergyPrediction::Inter, EnergyPrediction::Intra] {
                let row = e_prob_model_row(lm, prediction).unwrap();
                for band in 0..E_PROB_MODEL_BAND_AXIS {
                    let (p, d) = e_prob_model_pair(lm, prediction, band).unwrap();
                    assert_eq!(row[band * 2], p);
                    assert_eq!(row[band * 2 + 1], d);
                }
            }
        }
    }

    // ----------------------------------------------------------------
    // EnergyPrediction enum.
    // ----------------------------------------------------------------

    #[test]
    fn energy_prediction_axis_indices() {
        assert_eq!(EnergyPrediction::Inter.intra_axis_index(), 0);
        assert_eq!(EnergyPrediction::Intra.intra_axis_index(), 1);
    }

    #[test]
    fn energy_prediction_from_intra_flag() {
        assert_eq!(
            EnergyPrediction::from_intra_flag(false),
            EnergyPrediction::Inter
        );
        assert_eq!(
            EnergyPrediction::from_intra_flag(true),
            EnergyPrediction::Intra
        );
    }

    // ----------------------------------------------------------------
    // LM / CeltFrameSize round-trip.
    // ----------------------------------------------------------------

    #[test]
    fn lm_round_trip_via_celt_frame_size() {
        for lm in 0..=E_PROB_MODEL_LM_MAX {
            let fs = celt_frame_size_from_lm(lm).unwrap();
            assert_eq!(lm_from_celt_frame_size(fs), lm);
        }
    }

    #[test]
    fn lm_round_trip_via_frame_size_samples() {
        // 120 / 240 / 480 / 960 samples per RFC 6716 §4.3.2.1.
        let expected = [120u32, 240, 480, 960];
        for (lm, &samples) in expected.iter().enumerate() {
            assert_eq!(frame_size_samples_from_lm(lm as u8), Some(samples));
        }
        assert_eq!(frame_size_samples_from_lm(4), None);
        assert_eq!(frame_size_samples_from_lm(255), None);
    }

    #[test]
    fn celt_frame_size_from_lm_rejects_out_of_range() {
        assert_eq!(celt_frame_size_from_lm(4), None);
        assert_eq!(celt_frame_size_from_lm(7), None);
        assert_eq!(celt_frame_size_from_lm(255), None);
    }

    #[test]
    fn lm_from_celt_frame_size_total_function() {
        // Every CELT frame size maps to a valid LM in 0..=3.
        for fs in [
            CeltFrameSize::Ms2_5,
            CeltFrameSize::Ms5,
            CeltFrameSize::Ms10,
            CeltFrameSize::Ms20,
        ] {
            let lm = lm_from_celt_frame_size(fs);
            assert!(lm <= E_PROB_MODEL_LM_MAX);
            // And the inverse round-trips.
            assert_eq!(celt_frame_size_from_lm(lm), Some(fs));
        }
    }

    // ----------------------------------------------------------------
    // Spec-level cross-checks. These pin the qualitative invariants
    // RFC 6716 §4.3.2.1 calls out, so a typo in the CSV upstream that
    // happens to compile would still surface here.
    // ----------------------------------------------------------------

    #[test]
    fn intra_prob_zero_is_strictly_less_than_inter_for_low_bands() {
        // The RFC narrative says intra coding lacks the time
        // predictor, so the prediction error is larger in
        // magnitude on average → the Laplace mean is further from
        // 0, i.e. the *probability of 0* under intra coding is
        // strictly less than under inter coding for the early
        // (low-frequency) bands. This is a directional cross-check
        // on the §4.3.2.1 / §1.2 narrative — pinning it catches
        // an accidental row swap.
        for lm in 0..E_PROB_MODEL_FRAME_SIZE_AXIS as u8 {
            for band in 0..=4 {
                let (p_inter, _) = e_prob_model_pair(lm, EnergyPrediction::Inter, band).unwrap();
                let (p_intra, _) = e_prob_model_pair(lm, EnergyPrediction::Intra, band).unwrap();
                assert!(
                    p_intra < p_inter,
                    "lm={lm} band={band} expected intra prob < inter prob, got intra={p_intra} inter={p_inter}"
                );
            }
        }
    }

    #[test]
    fn intra_decay_is_strictly_greater_than_inter_for_low_bands() {
        // Complementary to the prob-zero invariant: with a wider
        // prediction-error distribution the Laplace decay
        // parameter under intra coding is larger (slower decay
        // away from 0) for the same low-frequency bands.
        for lm in 0..E_PROB_MODEL_FRAME_SIZE_AXIS as u8 {
            for band in 0..=4 {
                let (_, d_inter) = e_prob_model_pair(lm, EnergyPrediction::Inter, band).unwrap();
                let (_, d_intra) = e_prob_model_pair(lm, EnergyPrediction::Intra, band).unwrap();
                assert!(
                    d_intra > d_inter,
                    "lm={lm} band={band} expected intra decay > inter decay, got intra={d_intra} inter={d_inter}"
                );
            }
        }
    }

    #[test]
    fn all_prob_zero_bytes_are_nonzero() {
        // Every Laplace prob-of-0 parameter in the CSV is strictly
        // positive: the lowest is 21 (LM=2, intra=1, band=0).
        // A 0 would be a corrupted CSV entry and would also
        // produce a degenerate decoder.
        for lm in 0..E_PROB_MODEL_FRAME_SIZE_AXIS as u8 {
            for prediction in [EnergyPrediction::Inter, EnergyPrediction::Intra] {
                for band in 0..E_PROB_MODEL_BAND_AXIS {
                    let (p, _) = e_prob_model_pair(lm, prediction, band).unwrap();
                    assert!(
                        p > 0,
                        "lm={lm} prediction={prediction:?} band={band} prob_zero=0"
                    );
                }
            }
        }
    }

    #[test]
    fn all_decay_bytes_are_nonzero() {
        // Same property for the decay byte — the lowest in the
        // CSV is 6 (LM=2 intra=0 band=18). A 0 would mean an
        // infinitely-narrow Laplace.
        for lm in 0..E_PROB_MODEL_FRAME_SIZE_AXIS as u8 {
            for prediction in [EnergyPrediction::Inter, EnergyPrediction::Intra] {
                for band in 0..E_PROB_MODEL_BAND_AXIS {
                    let (_, d) = e_prob_model_pair(lm, prediction, band).unwrap();
                    assert!(
                        d > 0,
                        "lm={lm} prediction={prediction:?} band={band} decay=0"
                    );
                }
            }
        }
    }

    #[test]
    fn row_extreme_values_minimum_prob_zero_pin() {
        // Pin the global minimum prob-of-zero cell so a future
        // CSV regeneration can't silently shift it.
        let (p, d) = e_prob_model_pair(2, EnergyPrediction::Intra, 0).unwrap();
        assert_eq!((p, d), (21, 178));
    }

    #[test]
    fn row_extreme_values_maximum_prob_zero_pin() {
        // CSV maximum (192, found at LM=2 inter band=18).
        let (p, _) = e_prob_model_pair(2, EnergyPrediction::Inter, 18).unwrap();
        assert_eq!(p, 192);
    }
}
