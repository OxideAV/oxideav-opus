//! SILK ICDF tables from RFC 6716 §4.2.
//!
//! All tables are stored in "inverse-CDF" (icdf) form as expected by
//! `RangeDecoder::decode_icdf`: `icdf[k] = ft - cumfreq[k+1]`, where
//! `ft = 256` (so `ftb = 8`). Entries are monotonically non-increasing
//! and the last entry is zero.
//!
//! As of the round-next silence-rail fix, the §4.2.7.3 frame-type +
//! §4.2.7.4 sub-frame gain tables are bit-exact with the RFC PDFs (the
//! `silk::tables::gain_icdf_tests` mod pins each one). Some of the
//! larger NLSF / pitch-contour / shell-coder ICDFs further down are
//! still simplified approximations against the spec, kept until the
//! next round of clean-room transcription against `docs/audio/opus/`.
//! Search the file for "approximation" or "MVP" to find the remaining
//! ones.

// -------------------------------------------------------------------
// §4.2.7.3 Frame type coding.
// -------------------------------------------------------------------

/// Frame type when VAD_flag = 0 (inactive). RFC 6716 §4.2.7.3
/// Table 9: PDF `{26, 230}/256`. 2 symbols, mapping to frame type
/// 0 (Inactive/Low) and 1 (Inactive/High).
pub const FRAME_TYPE_INACTIVE_ICDF: [u8; 2] = [230, 0];

/// Frame type when VAD_flag = 1 (active). RFC 6716 §4.2.7.3 Table 9:
/// the 6-symbol PDF `{0, 0, 24, 74, 148, 10}/256` is stored here as
/// the trailing 4 non-zero entries `{24, 74, 148, 10}/256`. Decoded
/// symbols 0..=3 map to frame types 2..=5 (Unvoiced/Low,
/// Unvoiced/High, Voiced/Low, Voiced/High) — see `decode_frame_body`
/// in `silk::mod` where the `+2` offset is applied. Skipping the two
/// zero-prob symbols at the head is the standard libopus convention
/// because storing `ICDF[0] = ft = 256` would overflow the u8 cell.
pub const FRAME_TYPE_ACTIVE_ICDF: [u8; 4] = [232, 158, 10, 0];

// -------------------------------------------------------------------
// §4.2.7.4 Sub-frame gains.
// -------------------------------------------------------------------

/// First sub-frame gain MSB (3 bits = 8 symbols), one PDF per signal
/// type per RFC 6716 §4.2.7.4 Table 11.
///
/// Round-prior to the silence-rail fix these tables held simplified
/// approximations (e.g. inactive PDF `{32, 96, 48, 32, 16, 16, 16, 0}`
/// instead of the spec's `{32, 112, 68, 29, 12, 1, 1, 1}`); on
/// minimum-bitrate libopus packets the wrong cumfreq buckets shifted
/// the absolute MSB read by 1-3 indices, doubling the SF0 gain and
/// then compounding the error through the delta-coded SF1+ gains.
/// Combined with the broken `GAIN_DELTA_ICDF` and the broken
/// `FRAME_TYPE_ACTIVE_ICDF`, the cascade saturated the SILK synth IIR
/// to ±0.7 — the silence-saturation regression in commit a6ca9ea.
///
/// Inactive: PDF `{32, 112, 68, 29, 12, 1, 1, 1}/256`.
pub const GAIN_MSB_INACTIVE_ICDF: [u8; 8] = [224, 112, 44, 15, 3, 2, 1, 0];
/// Unvoiced: PDF `{2, 17, 45, 60, 62, 47, 19, 4}/256`.
pub const GAIN_MSB_UNVOICED_ICDF: [u8; 8] = [254, 237, 192, 132, 70, 23, 4, 0];
/// Voiced: PDF `{1, 3, 26, 71, 94, 50, 9, 2}/256`.
pub const GAIN_MSB_VOICED_ICDF: [u8; 8] = [255, 252, 226, 155, 61, 11, 2, 0];

/// First sub-frame gain LSB (3 bits uniform).
pub const GAIN_LSB_ICDF: [u8; 8] = [224, 192, 160, 128, 96, 64, 32, 0];

/// Delta gain coding for sub-frames 1..=3 (RFC 6716 §4.2.7.4 Table 13).
/// 41 symbols, centred at symbol 4 (which maps to "no change").
///
/// PDF (per RFC Table 13): `{6, 5, 11, 31, 132, 21, 8, 4, 3, 2, 2, 2,
/// 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
/// 1, 1, 1, 1, 1, 1, 1}/256`. ICDF is `256 - cumfreq[k+1]`.
///
/// Round-prior to the silence-rail fix this table held a hand-shifted
/// approximation that mis-decoded the high-probability symbol 4 ("no
/// change") as 5, and pushed the long tail (delta ≥ 8) toward smaller
/// indices. On minimum-bitrate libopus packets the wrong ICDF produced
/// `delta = 24` at the bitstream tail (instead of 4 = no change),
/// inflating the sub-frame 1 log_gain by ~+20 indices and driving the
/// SILK synthesis IIR to ±0.7 — exactly the silence-rail saturation
/// regression observed in commit a6ca9ea.
pub const GAIN_DELTA_ICDF: [u8; 41] = [
    250, 245, 234, 203, 71, 50, 42, 38, 35, 33, 31, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18,
    17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
];

// -------------------------------------------------------------------
// §4.2.7.5 NLSF stage-1 indices (5 bits each) — RFC 6716 Table 14.
// -------------------------------------------------------------------

/// Stage-1 NLSF index PDF for NB/MB, inactive or unvoiced (Table 14).
pub const NLSF_NB_STAGE1_UNVOICED_ICDF: [u8; 32] = [
    212, 178, 148, 129, 108, 96, 85, 82, 79, 77, 61, 59, 57, 56, 51, 49, 48, 45, 42, 41, 40, 38,
    36, 34, 31, 30, 21, 12, 10, 3, 1, 0,
];
/// Stage-1 NLSF index PDF for NB/MB, voiced (Table 14).
pub const NLSF_NB_STAGE1_VOICED_ICDF: [u8; 32] = [
    255, 245, 244, 236, 233, 225, 217, 203, 190, 176, 175, 161, 149, 136, 125, 114, 102, 91, 81,
    71, 60, 52, 43, 35, 28, 20, 19, 18, 12, 11, 5, 0,
];
/// Stage-1 NLSF index PDF for WB, inactive or unvoiced (Table 14).
pub const NLSF_WB_STAGE1_UNVOICED_ICDF: [u8; 32] = [
    225, 204, 201, 184, 183, 175, 158, 154, 153, 135, 119, 115, 113, 110, 109, 99, 98, 95, 79, 68,
    52, 50, 48, 45, 43, 32, 31, 27, 18, 10, 3, 0,
];
/// Stage-1 NLSF index PDF for WB, voiced (Table 14).
pub const NLSF_WB_STAGE1_VOICED_ICDF: [u8; 32] = [
    255, 251, 235, 230, 212, 201, 196, 182, 167, 166, 163, 151, 138, 124, 110, 104, 90, 78, 76, 70,
    69, 57, 45, 34, 24, 21, 11, 6, 5, 4, 3, 0,
];

// -------------------------------------------------------------------
// §4.2.7.5.2 NLSF stage-2 residual codebooks.
// -------------------------------------------------------------------

/// Per-codebook stage-2 ICDFs for NB/MB (Table 15, codebooks a..h).
/// 9 symbols per codebook representing residual values -4..=4 *before*
/// the optional Table 19 extension.
pub const NLSF_NBMB_STAGE2_ICDF: [[u8; 9]; 8] = [
    // a
    [255, 254, 253, 238, 14, 3, 2, 1, 0],
    // b
    [255, 254, 252, 218, 35, 3, 2, 1, 0],
    // c
    [255, 254, 250, 208, 59, 4, 2, 1, 0],
    // d
    [255, 254, 246, 194, 71, 10, 2, 1, 0],
    // e
    [255, 252, 236, 183, 82, 8, 2, 1, 0],
    // f
    [255, 252, 235, 180, 90, 17, 2, 1, 0],
    // g
    [255, 248, 224, 171, 97, 30, 4, 1, 0],
    // h
    [255, 254, 236, 173, 95, 37, 7, 1, 0],
];

/// Per-codebook stage-2 ICDFs for WB (Table 16, codebooks i..p).
pub const NLSF_WB_STAGE2_ICDF: [[u8; 9]; 8] = [
    // i
    [255, 254, 253, 244, 12, 3, 2, 1, 0],
    // j
    [255, 254, 252, 224, 38, 3, 2, 1, 0],
    // k
    [255, 254, 251, 209, 57, 4, 2, 1, 0],
    // l
    [255, 254, 244, 195, 69, 4, 2, 1, 0],
    // m
    [255, 251, 232, 184, 84, 7, 2, 1, 0],
    // n
    [255, 254, 240, 186, 86, 14, 2, 1, 0],
    // o
    [255, 254, 239, 178, 91, 30, 5, 1, 0],
    // p
    [255, 248, 227, 177, 100, 19, 2, 1, 0],
];

/// Codebook selector for NB/MB stage-2 residual decoding (Table 17).
/// Indexed by `[I1][k]`, where I1 is the stage-1 index (0..=31) and
/// `k` is the LSF coefficient index (0..=9). Values are 0..=7,
/// indexing into [`NLSF_NBMB_STAGE2_ICDF`] (codebooks a..h).
pub const NLSF_NBMB_STAGE2_SELECT: [[u8; 10]; 32] = [
    /* I1=0  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], // aaaa aaaa aa
    /* I1=1  */ [1, 3, 1, 2, 2, 1, 2, 1, 1, 1], // bdbcc bcbbb
    /* I1=2  */ [2, 1, 1, 1, 1, 1, 1, 1, 1, 1], // cbbbb bbbbb
    /* I1=3  */ [1, 2, 2, 2, 2, 1, 2, 1, 1, 1], // bccc cbcbb b
    /* I1=4  */ [2, 3, 3, 3, 3, 2, 2, 2, 2, 2], // cdddd cccc c
    /* I1=5  */ [0, 5, 3, 3, 2, 2, 2, 2, 1, 1], // afdd ccccb b
    /* I1=6  */
    [0, 2, 2, 2, 2, 2, 2, 2, 2, 1], // accc ccccc b  (note Table 17 row label "g" is a typo for 6 in RFC, signal type is the I1 index — the row is g/c/c/c/c/c/c/c/c/b which maps to a,c,c,c,c,c,c,c,c,b)
    /* I1=7  */ [2, 3, 6, 4, 4, 4, 5, 4, 5, 5], // cdge eee fef f
    /* I1=8  */ [2, 4, 5, 5, 4, 5, 4, 6, 4, 4], // ceff ef egee
    /* I1=9  */ [2, 4, 4, 7, 4, 5, 4, 5, 5, 4], // ceeh efef fe
    /* I1=10 */ [4, 3, 3, 3, 2, 3, 2, 2, 2, 2], // edd dcdc ccc
    /* I1=11 */ [1, 5, 5, 6, 4, 5, 4, 5, 5, 5], // bff geff ef ff
    /* I1=12 */ [2, 7, 4, 6, 5, 5, 5, 5, 5, 5], // cheg fff fff f
    /* I1=13 */ [2, 7, 5, 5, 5, 5, 5, 6, 5, 4], // chff fff fgfe
    /* I1=14 */ [3, 3, 5, 4, 4, 5, 4, 5, 4, 4], // ddfe efef ee
    /* I1=15 */ [2, 3, 3, 5, 5, 4, 4, 4, 4, 4], // cdd ffeeeee
    /* I1=16 */ [2, 4, 4, 6, 4, 5, 4, 5, 5, 5], // cee gefef ff
    /* I1=17 */ [2, 5, 4, 6, 5, 5, 5, 4, 5, 4], // cfeg fff efe
    /* I1=18 */ [2, 7, 4, 5, 4, 5, 4, 5, 5, 5], // cheff efef ff
    /* I1=19 */ [2, 5, 4, 6, 7, 6, 5, 6, 5, 4], // cfeg hgfg fe
    /* I1=20 */ [3, 6, 7, 4, 6, 5, 5, 6, 4, 5], // dghe gffge f
    /* I1=21 */ [2, 7, 6, 4, 4, 4, 5, 4, 5, 5], // chge eef e f f
    /* I1=22 */ [4, 5, 5, 4, 6, 6, 5, 6, 5, 4], // effe ggfg fe
    /* I1=23 */ [2, 5, 5, 6, 5, 6, 4, 6, 4, 4], // cffg fgegee
    /* I1=24 */ [4, 5, 5, 5, 3, 7, 4, 5, 5, 4], // efff dhefee
    /* I1=25 */ [2, 3, 4, 5, 5, 6, 4, 5, 5, 4], // cdef fgef fe
    /* I1=26 */ [2, 3, 2, 3, 3, 4, 2, 3, 3, 3], // cdcd dec ddd
    /* I1=27 */ [1, 1, 2, 2, 2, 2, 2, 3, 2, 2], // bbcc cccdcc
    /* I1=28 */ [4, 5, 5, 6, 6, 6, 5, 6, 4, 5], // efgg gfgef
    /* I1=29 */ [3, 5, 5, 4, 4, 4, 4, 3, 3, 2], // dffeeeeddc
    /* I1=30 */ [2, 5, 3, 7, 5, 5, 4, 4, 5, 4], // cfdh ffe efe
    /* I1=31 */ [4, 4, 5, 4, 5, 6, 5, 6, 5, 4], // eef ef gf gfe
];

/// Codebook selector for WB stage-2 residual decoding (Table 18).
/// Values 0..=7 index into [`NLSF_WB_STAGE2_ICDF`] (codebooks i..p).
pub const NLSF_WB_STAGE2_SELECT: [[u8; 16]; 32] = [
    /* I1=0  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=1  */ [2, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 1, 1, 1, 0, 3],
    /* I1=2  */ [2, 5, 5, 3, 7, 4, 4, 5, 2, 5, 4, 5, 5, 4, 3, 3],
    /* I1=3  */ [0, 2, 1, 2, 2, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 1],
    /* I1=4  */ [0, 6, 5, 4, 6, 4, 7, 5, 4, 4, 4, 5, 5, 4, 4, 3],
    /* I1=5  */ [0, 3, 5, 5, 4, 3, 3, 5, 3, 3, 3, 3, 3, 3, 2, 4],
    /* I1=6  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=7  */ [0, 2, 6, 3, 7, 2, 5, 3, 4, 5, 5, 4, 3, 3, 2, 3],
    /* I1=8  */ [0, 6, 2, 6, 6, 4, 5, 4, 6, 5, 4, 4, 5, 3, 3, 3],
    /* I1=9  */ [2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=10 */ [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=11 */ [2, 2, 3, 4, 5, 3, 3, 3, 3, 3, 3, 3, 2, 2, 1, 3],
    /* I1=12 */ [2, 2, 3, 3, 4, 3, 3, 3, 3, 3, 3, 3, 3, 2, 1, 3],
    /* I1=13 */ [3, 4, 4, 4, 6, 4, 4, 5, 3, 5, 4, 4, 5, 4, 3, 4],
    /* I1=14 */ [0, 6, 4, 5, 4, 7, 5, 2, 6, 5, 7, 4, 4, 3, 5, 3],
    /* I1=15 */ [0, 1, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 1, 0],
    /* I1=16 */ [1, 6, 5, 7, 5, 4, 5, 3, 4, 5, 4, 4, 4, 3, 3, 4],
    /* I1=17 */ [1, 3, 3, 4, 4, 3, 3, 5, 2, 3, 3, 5, 5, 5, 3, 4],
    /* I1=18 */ [2, 3, 3, 2, 2, 2, 3, 2, 1, 2, 1, 2, 1, 1, 1, 4],
    /* I1=19 */ [0, 2, 3, 5, 3, 3, 2, 2, 2, 1, 1, 0, 0, 0, 0, 0],
    /* I1=20 */ [3, 4, 3, 5, 3, 3, 2, 2, 1, 1, 1, 1, 1, 2, 2, 4],
    /* I1=21 */ [2, 6, 3, 7, 7, 4, 5, 4, 5, 3, 5, 3, 3, 2, 3, 3],
    /* I1=22 */ [2, 3, 5, 6, 6, 3, 5, 3, 4, 4, 3, 3, 3, 3, 2, 4],
    /* I1=23 */ [1, 3, 3, 4, 4, 4, 4, 3, 5, 5, 5, 3, 1, 1, 1, 1],
    /* I1=24 */ [2, 5, 3, 6, 6, 4, 7, 4, 4, 5, 3, 4, 4, 3, 3, 3],
    /* I1=25 */ [0, 6, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=26 */ [0, 6, 6, 3, 5, 2, 5, 5, 3, 4, 4, 7, 7, 4, 4, 4],
    /* I1=27 */ [3, 3, 7, 3, 5, 4, 3, 3, 3, 2, 2, 3, 3, 3, 2, 3],
    /* I1=28 */ [0, 0, 1, 0, 0, 0, 2, 1, 2, 1, 1, 2, 2, 2, 1, 1],
    /* I1=29 */ [0, 3, 2, 5, 3, 3, 2, 3, 2, 1, 0, 0, 1, 0, 0, 1],
    /* I1=30 */ [3, 5, 5, 4, 7, 5, 3, 3, 2, 3, 2, 2, 1, 0, 1, 0],
    /* I1=31 */ [2, 3, 5, 3, 4, 3, 3, 3, 2, 1, 2, 6, 4, 0, 0, 0],
];

/// Stage-2 extension PDF (Table 19) — read when the stage-2 symbol
/// magnitude reaches the rail (-4 or +4) to add 0..=6 to the magnitude.
pub const NLSF_STAGE2_EXTENSION_ICDF: [u8; 7] = [100, 40, 16, 7, 3, 1, 0];

// -------------------------------------------------------------------
// §4.2.7.5.3 Prediction weight tables (Table 20) and selectors
// (Tables 21/22).
// -------------------------------------------------------------------

/// Backwards-prediction weight lists A..D (RFC Table 20).
/// Index A/B used for NB/MB; C/D used for WB.
/// The columns are coefficient indices 0..(d_LPC-2) — so 9 entries
/// for NB/MB and 15 for WB. Lists A and B short-pad with zeros to 15
/// just so we can share storage; only NB/MB callers index 0..=8.
pub const NLSF_PRED_WEIGHTS: [[u8; 15]; 4] = [
    // A
    [
        179, 138, 140, 148, 151, 149, 153, 151, 163, 0, 0, 0, 0, 0, 0,
    ],
    // B
    [116, 67, 82, 59, 92, 72, 100, 89, 92, 0, 0, 0, 0, 0, 0],
    // C
    [
        175, 148, 160, 176, 178, 173, 174, 164, 177, 174, 196, 182, 198, 192, 182,
    ],
    // D
    [
        68, 62, 66, 60, 72, 117, 85, 90, 118, 136, 151, 142, 160, 142, 155,
    ],
];

/// Prediction-weight selector for NB/MB (Table 21).
/// Indexed `[I1][k]`. Values 0/1 select list A (=0) or B (=1).
pub const NLSF_NBMB_PRED_SELECT: [[u8; 9]; 32] = [
    /* I1=0  */ [0, 1, 0, 0, 0, 0, 0, 0, 0],
    /* I1=1  */ [1, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=2  */ [0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=3  */ [1, 1, 1, 0, 0, 0, 0, 1, 0],
    /* I1=4  */ [0, 1, 0, 0, 0, 0, 0, 0, 0],
    /* I1=5  */ [0, 1, 0, 0, 0, 0, 0, 0, 0],
    /* I1=6  */ [1, 0, 1, 1, 0, 0, 0, 1, 0],
    /* I1=7  */ [0, 1, 1, 0, 0, 1, 1, 0, 0],
    /* I1=8  */ [0, 0, 1, 1, 0, 1, 0, 1, 1],
    /* I1=9  */ [0, 0, 1, 1, 0, 0, 1, 1, 1],
    /* I1=10 */ [0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=11 */ [0, 1, 0, 1, 1, 1, 1, 1, 0],
    /* I1=12 */ [0, 1, 0, 1, 1, 1, 1, 1, 0],
    /* I1=13 */ [0, 1, 1, 1, 1, 1, 1, 1, 0],
    /* I1=14 */ [1, 0, 1, 1, 0, 1, 1, 1, 1],
    /* I1=15 */ [0, 1, 1, 1, 1, 1, 0, 1, 0],
    /* I1=16 */ [0, 0, 1, 1, 0, 1, 0, 1, 0],
    /* I1=17 */ [0, 0, 1, 1, 1, 0, 1, 1, 1],
    /* I1=18 */ [0, 1, 1, 0, 0, 1, 1, 1, 0],
    /* I1=19 */ [0, 0, 0, 1, 1, 1, 0, 1, 0],
    /* I1=20 */ [0, 1, 1, 0, 0, 1, 0, 1, 0],
    /* I1=21 */ [0, 1, 1, 0, 0, 0, 1, 1, 0],
    /* I1=22 */ [0, 0, 0, 0, 0, 1, 1, 1, 1],
    /* I1=23 */ [0, 0, 1, 1, 0, 0, 0, 1, 1],
    /* I1=24 */ [0, 0, 0, 1, 0, 1, 1, 1, 1],
    /* I1=25 */ [0, 1, 1, 1, 1, 1, 1, 1, 0],
    /* I1=26 */ [0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=27 */ [0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=28 */ [0, 0, 1, 0, 1, 1, 0, 1, 0],
    /* I1=29 */ [1, 0, 0, 1, 0, 0, 0, 0, 0],
    /* I1=30 */ [0, 0, 0, 1, 1, 0, 1, 0, 1],
    /* I1=31 */ [1, 0, 1, 1, 0, 1, 1, 1, 1],
];

/// Prediction-weight selector for WB (Table 22).
/// Indexed `[I1][k]`. Values 0/1 select list C (=0) or D (=1).
pub const NLSF_WB_PRED_SELECT: [[u8; 15]; 32] = [
    /* I1=0  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=1  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=2  */ [0, 0, 1, 0, 0, 1, 1, 1, 0, 1, 1, 1, 1, 0, 0],
    /* I1=3  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
    /* I1=4  */ [0, 1, 1, 0, 1, 0, 1, 1, 0, 1, 1, 1, 1, 1, 0],
    /* I1=5  */ [0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=6  */ [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0],
    /* I1=7  */ [0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0, 1, 0, 1],
    /* I1=8  */ [0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1, 1],
    /* I1=9  */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=10 */ [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=11 */ [0, 0, 1, 0, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 0],
    /* I1=12 */ [0, 0, 1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 0, 0],
    /* I1=13 */ [0, 0, 0, 0, 1, 1, 0, 1, 0, 1, 1, 1, 1, 0, 0],
    /* I1=14 */ [0, 1, 0, 0, 0, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1],
    /* I1=15 */ [0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0],
    /* I1=16 */ [0, 1, 1, 0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 0],
    /* I1=17 */ [0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 0, 0],
    /* I1=18 */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=19 */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
    /* I1=20 */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    /* I1=21 */ [0, 1, 0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1, 1, 0],
    /* I1=22 */ [0, 0, 1, 1, 1, 1, 0, 1, 1, 0, 0, 1, 1, 0, 0],
    /* I1=23 */ [0, 1, 1, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 1, 0],
    /* I1=24 */ [0, 0, 0, 1, 1, 0, 1, 0, 1, 1, 1, 1, 1, 1, 1],
    /* I1=25 */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=26 */ [0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 1, 1, 1, 1, 1],
    /* I1=27 */ [0, 0, 0, 0, 0, 1, 0, 1, 1, 1, 1, 0, 1, 1, 1],
    /* I1=28 */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=29 */ [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    /* I1=30 */ [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
    /* I1=31 */ [0, 0, 1, 0, 0, 1, 1, 1, 0, 0, 1, 0, 0, 1, 0],
];

// -------------------------------------------------------------------
// §4.2.7.5.3 Stage-1 codebook vectors (Tables 23 / 24).
// -------------------------------------------------------------------

/// NB/MB stage-1 codebook vectors (Table 23, Q8). 32 entries × 10
/// coefficients.
pub const NLSF_NBMB_CB1_Q8: [[u8; 10]; 32] = [
    [12, 35, 60, 83, 108, 132, 157, 180, 206, 228],
    [15, 32, 55, 77, 101, 125, 151, 175, 201, 225],
    [19, 42, 66, 89, 114, 137, 162, 184, 209, 230],
    [12, 25, 50, 72, 97, 120, 147, 172, 200, 223],
    [26, 44, 69, 90, 114, 135, 159, 180, 205, 225],
    [13, 22, 53, 80, 106, 130, 156, 180, 205, 228],
    [15, 25, 44, 64, 90, 115, 142, 168, 196, 222],
    [19, 24, 62, 82, 100, 120, 145, 168, 190, 214],
    [22, 31, 50, 79, 103, 120, 151, 170, 203, 227],
    [21, 29, 45, 65, 106, 124, 150, 171, 196, 224],
    [30, 49, 75, 97, 121, 142, 165, 186, 209, 229],
    [19, 25, 52, 70, 93, 116, 143, 166, 192, 219],
    [26, 34, 62, 75, 97, 118, 145, 167, 194, 217],
    [25, 33, 56, 70, 91, 113, 143, 165, 196, 223],
    [21, 34, 51, 72, 97, 117, 145, 171, 196, 222],
    [20, 29, 50, 67, 90, 117, 144, 168, 197, 221],
    [22, 31, 48, 66, 95, 117, 146, 168, 196, 222],
    [24, 33, 51, 77, 116, 134, 158, 180, 200, 224],
    [21, 28, 70, 87, 106, 124, 149, 170, 194, 217],
    [26, 33, 53, 64, 83, 117, 152, 173, 204, 225],
    [27, 34, 65, 95, 108, 129, 155, 174, 210, 225],
    [20, 26, 72, 99, 113, 131, 154, 176, 200, 219],
    [34, 43, 61, 78, 93, 114, 155, 177, 205, 229],
    [23, 29, 54, 97, 124, 138, 163, 179, 209, 229],
    [30, 38, 56, 89, 118, 129, 158, 178, 200, 231],
    [21, 29, 49, 63, 85, 111, 142, 163, 193, 222],
    [27, 48, 77, 103, 133, 158, 179, 196, 215, 232],
    [29, 47, 74, 99, 124, 151, 176, 198, 220, 237],
    [33, 42, 61, 76, 93, 121, 155, 174, 207, 225],
    [29, 53, 87, 112, 136, 154, 170, 188, 208, 227],
    [24, 30, 52, 84, 131, 150, 166, 186, 203, 229],
    [37, 48, 64, 84, 104, 118, 156, 177, 201, 230],
];

/// WB stage-1 codebook vectors (Table 24, Q8). 32 entries × 16
/// coefficients.
pub const NLSF_WB_CB1_Q8: [[u8; 16]; 32] = [
    [
        7, 23, 38, 54, 69, 85, 100, 116, 131, 147, 162, 178, 193, 208, 223, 239,
    ],
    [
        13, 25, 41, 55, 69, 83, 98, 112, 127, 142, 157, 171, 187, 203, 220, 236,
    ],
    [
        15, 21, 34, 51, 61, 78, 92, 106, 126, 136, 152, 167, 185, 205, 225, 240,
    ],
    [
        10, 21, 36, 50, 63, 79, 95, 110, 126, 141, 157, 173, 189, 205, 221, 237,
    ],
    [
        17, 20, 37, 51, 59, 78, 89, 107, 123, 134, 150, 164, 184, 205, 224, 240,
    ],
    [
        10, 15, 32, 51, 67, 81, 96, 112, 129, 142, 158, 173, 189, 204, 220, 236,
    ],
    [
        8, 21, 37, 51, 65, 79, 98, 113, 126, 138, 155, 168, 179, 192, 209, 218,
    ],
    [
        12, 15, 34, 55, 63, 78, 87, 108, 118, 131, 148, 167, 185, 203, 219, 236,
    ],
    [
        16, 19, 32, 36, 56, 79, 91, 108, 118, 136, 154, 171, 186, 204, 220, 237,
    ],
    [
        11, 28, 43, 58, 74, 89, 105, 120, 135, 150, 165, 180, 196, 211, 226, 241,
    ],
    [
        6, 16, 33, 46, 60, 75, 92, 107, 123, 137, 156, 169, 185, 199, 214, 225,
    ],
    [
        11, 19, 30, 44, 57, 74, 89, 105, 121, 135, 152, 169, 186, 202, 218, 234,
    ],
    [
        12, 19, 29, 46, 57, 71, 88, 100, 120, 132, 148, 165, 182, 199, 216, 233,
    ],
    [
        17, 23, 35, 46, 56, 77, 92, 106, 123, 134, 152, 167, 185, 204, 222, 237,
    ],
    [
        14, 17, 45, 53, 63, 75, 89, 107, 115, 132, 151, 171, 188, 206, 221, 240,
    ],
    [
        9, 16, 29, 40, 56, 71, 88, 103, 119, 137, 154, 171, 189, 205, 222, 237,
    ],
    [
        16, 19, 36, 48, 57, 76, 87, 105, 118, 132, 150, 167, 185, 202, 218, 236,
    ],
    [
        12, 17, 29, 54, 71, 81, 94, 104, 126, 136, 149, 164, 182, 201, 221, 237,
    ],
    [
        15, 28, 47, 62, 79, 97, 115, 129, 142, 155, 168, 180, 194, 208, 223, 238,
    ],
    [
        8, 14, 30, 45, 62, 78, 94, 111, 127, 143, 159, 175, 192, 207, 223, 239,
    ],
    [
        17, 30, 49, 62, 79, 92, 107, 119, 132, 145, 160, 174, 190, 204, 220, 235,
    ],
    [
        14, 19, 36, 45, 61, 76, 91, 108, 121, 138, 154, 172, 189, 205, 222, 238,
    ],
    [
        12, 18, 31, 45, 60, 76, 91, 107, 123, 138, 154, 171, 187, 204, 221, 236,
    ],
    [
        13, 17, 31, 43, 53, 70, 83, 103, 114, 131, 149, 167, 185, 203, 220, 237,
    ],
    [
        17, 22, 35, 42, 58, 78, 93, 110, 125, 139, 155, 170, 188, 206, 224, 240,
    ],
    [
        8, 15, 34, 50, 67, 83, 99, 115, 131, 146, 162, 178, 193, 209, 224, 239,
    ],
    [
        13, 16, 41, 66, 73, 86, 95, 111, 128, 137, 150, 163, 183, 206, 225, 241,
    ],
    [
        17, 25, 37, 52, 63, 75, 92, 102, 119, 132, 144, 160, 175, 191, 212, 231,
    ],
    [
        19, 31, 49, 65, 83, 100, 117, 133, 147, 161, 174, 187, 200, 213, 227, 242,
    ],
    [
        18, 31, 52, 68, 88, 103, 117, 126, 138, 149, 163, 177, 192, 207, 223, 239,
    ],
    [
        16, 29, 47, 61, 76, 90, 106, 119, 133, 147, 161, 176, 193, 209, 224, 240,
    ],
    [
        15, 21, 35, 50, 61, 73, 86, 97, 110, 119, 129, 141, 175, 198, 218, 237,
    ],
];

// -------------------------------------------------------------------
// §4.2.7.5.4 Minimum spacing (Table 25).
// -------------------------------------------------------------------

/// Minimum spacing (in Q15) for NB/MB NLSF coefficients (Table 25).
/// 11 entries — `MIN_DELTA[k]` is the minimum allowed value of
/// `NLSF[k] - NLSF[k-1]`, with sentinel `NLSF[-1]=0` and
/// `NLSF[d_LPC]=32768`.
pub const NLSF_NBMB_MIN_DELTA_Q15: [i16; 11] = [250, 3, 6, 3, 3, 3, 4, 3, 3, 3, 461];

/// Minimum spacing (Q15) for WB NLSF coefficients (Table 25). 17 entries.
pub const NLSF_WB_MIN_DELTA_Q15: [i16; 17] =
    [100, 3, 40, 3, 3, 3, 5, 14, 14, 10, 11, 3, 8, 9, 7, 3, 347];

// -------------------------------------------------------------------
// §4.2.7.5.5 Interpolation factor (Table 26).
// -------------------------------------------------------------------

/// PDF for the 2-bit (4-symbol) NLSF interpolation factor (Table 26).
pub const NLSF_INTERP_ICDF: [u8; 5] = [243, 221, 192, 181, 0];

// -------------------------------------------------------------------
// §4.2.7.5.6 LSF→LPC ordering (Table 27) and cosine table (Table 28).
// -------------------------------------------------------------------

/// LSF-coefficient ordering used by `silk_NLSF2A` to construct P/Q
/// (Table 27). Two columns: index 0 = NB/MB (10 entries), index 1 = WB
/// (16 entries).
pub const NLSF_ORDERING_NB: [usize; 10] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];
pub const NLSF_ORDERING_WB: [usize; 16] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];

/// Q12 cosine table for LSF→LPC conversion (Table 28). 129 entries.
pub const COSINE_Q12: [i16; 129] = [
    4096, 4095, 4091, 4085, 4076, 4065, 4052, 4036, 4017, 3997, 3973, 3948, 3920, 3889, 3857, 3822,
    3784, 3745, 3703, 3659, 3613, 3564, 3513, 3461, 3406, 3349, 3290, 3229, 3166, 3102, 3035, 2967,
    2896, 2824, 2751, 2676, 2599, 2520, 2440, 2359, 2276, 2191, 2106, 2019, 1931, 1842, 1751, 1660,
    1568, 1474, 1380, 1285, 1189, 1093, 995, 897, 799, 700, 601, 501, 401, 301, 201, 101, 0, -101,
    -201, -301, -401, -501, -601, -700, -799, -897, -995, -1093, -1189, -1285, -1380, -1474, -1568,
    -1660, -1751, -1842, -1931, -2019, -2106, -2191, -2276, -2359, -2440, -2520, -2599, -2676,
    -2751, -2824, -2896, -2967, -3035, -3102, -3166, -3229, -3290, -3349, -3406, -3461, -3513,
    -3564, -3613, -3659, -3703, -3745, -3784, -3822, -3857, -3889, -3920, -3948, -3973, -3997,
    -4017, -4036, -4052, -4065, -4076, -4085, -4091, -4095, -4096,
];

// -------------------------------------------------------------------
// §4.2.7.6 Long-term prediction (pitch + LTP filter).
// -------------------------------------------------------------------

/// Primary pitch lag high part (NB). 32 symbols.
pub const PITCH_LAG_NB_HIGH_ICDF: [u8; 32] = [
    224, 192, 176, 160, 144, 128, 112, 100, 88, 80, 72, 64, 56, 48, 44, 40, 36, 32, 28, 24, 22, 20,
    18, 16, 14, 12, 10, 8, 6, 4, 2, 0,
];
/// Primary pitch lag low part (NB). 4 symbols ≈ uniform.
pub const PITCH_LAG_NB_LOW_ICDF: [u8; 4] = [192, 128, 64, 0];
/// Pitch delta (RFC Table 31). 21 symbols.
pub const PITCH_DELTA_ICDF: [u8; 21] = [
    220, 200, 180, 160, 140, 120, 104, 88, 72, 60, 48, 36, 28, 22, 16, 12, 8, 6, 4, 2, 0,
];
/// Pitch contour index, NB 20 ms (11 symbols).
pub const PITCH_CONTOUR_NB_20MS_ICDF: [u8; 11] = [224, 192, 160, 128, 96, 72, 56, 40, 24, 12, 0];
/// LTP periodicity index (3 symbols).
pub const LTP_PERIODICITY_ICDF: [u8; 3] = [200, 60, 0];
/// LTP filter indexing — 8 symbols (periodicity 0).
pub const LTP_FILTER_P0_ICDF: [u8; 8] = [220, 180, 140, 100, 72, 48, 24, 0];
/// 16 symbols (periodicity 1).
pub const LTP_FILTER_P1_ICDF: [u8; 16] = [
    240, 224, 208, 192, 176, 152, 128, 104, 80, 64, 48, 36, 24, 16, 8, 0,
];
/// 32 symbols (periodicity 2).
pub const LTP_FILTER_P2_ICDF: [u8; 32] = [
    248, 240, 224, 208, 192, 176, 160, 148, 136, 124, 112, 100, 88, 76, 64, 56, 48, 40, 36, 32, 28,
    24, 20, 16, 14, 12, 10, 8, 6, 4, 2, 0,
];
/// LTP scaling factor index. 3 symbols.
pub const LTP_SCALING_ICDF: [u8; 3] = [128, 64, 0];

// -------------------------------------------------------------------
// §4.2.7.7 LCG seed (2-bit uniform).
// -------------------------------------------------------------------

pub const LCG_SEED_ICDF: [u8; 4] = [192, 128, 64, 0];

// -------------------------------------------------------------------
// §4.2.7.8 Excitation coding.
// -------------------------------------------------------------------

/// Rate-level ICDF (9 symbols per RFC, we use 11 with the last two as
/// fallbacks).
pub const RATE_LEVEL_INACTIVE_ICDF: [u8; 10] = [240, 192, 160, 128, 96, 72, 48, 24, 8, 0];
pub const RATE_LEVEL_VOICED_ICDF: [u8; 10] = [224, 192, 160, 128, 96, 64, 40, 20, 8, 0];

/// Pulse count ICDFs per rate level — 18 symbols each. The MVP
/// decoder doesn't recursively shell-decode so the exact distribution
/// doesn't matter, but the decoder does read one symbol per shell
/// block so the ICDF must be valid.
pub const PULSE_COUNT_ICDF: [[u8; 18]; 11] = [
    // Rate level 0 — mostly zero pulses.
    [
        240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 24, 16, 8, 0,
    ],
    [
        232, 216, 200, 184, 168, 152, 136, 120, 104, 88, 72, 56, 40, 28, 20, 12, 6, 0,
    ],
    [
        224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 36, 28, 20, 12, 6, 0,
    ],
    [
        216, 200, 184, 168, 152, 136, 120, 104, 88, 72, 60, 48, 36, 28, 20, 12, 6, 0,
    ],
    [
        208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 56, 48, 36, 28, 20, 12, 6, 0,
    ],
    [
        200, 184, 168, 152, 136, 120, 104, 88, 72, 60, 48, 40, 32, 24, 18, 12, 6, 0,
    ],
    [
        192, 176, 160, 144, 128, 112, 96, 80, 64, 56, 48, 40, 32, 24, 18, 12, 6, 0,
    ],
    [
        184, 168, 152, 136, 120, 104, 88, 72, 60, 48, 40, 32, 24, 18, 14, 10, 6, 0,
    ],
    [
        176, 160, 144, 128, 112, 96, 80, 64, 56, 48, 40, 32, 24, 18, 14, 10, 6, 0,
    ],
    [
        168, 152, 136, 120, 104, 88, 72, 60, 48, 40, 32, 24, 18, 14, 10, 8, 4, 0,
    ],
    [
        160, 144, 128, 112, 96, 80, 64, 56, 48, 40, 32, 24, 18, 14, 10, 8, 4, 0,
    ],
];

/// 4-way pulse split ICDF, placeholder (not used by the MVP decoder
/// since we skip shell decoding).
pub const SHELL_4WAY_SPLIT_ICDF: [[u8; 16]; 4] = [
    [
        240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 16, 0,
    ],
    [
        240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 16, 0,
    ],
    [
        240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 16, 0,
    ],
    [
        240, 224, 208, 192, 176, 160, 144, 128, 112, 96, 80, 64, 48, 32, 16, 0,
    ],
];

// Keep the helper used in lsf.rs in sync: 11-symbol uniform ICDF.
pub const NLSF_RESIDUAL_UNIFORM_11_ICDF: [u8; 11] =
    [232, 208, 184, 160, 136, 112, 88, 64, 40, 20, 0];

// -------------------------------------------------------------------
// §4.2.7.1 Stereo prediction weights and mid-only flag
// (verbatim from libopus silk/tables_other.c).
// -------------------------------------------------------------------

/// Joint iCDF for the 5×5 grid of stereo prediction weight coarse indices
/// (`ix[0][2]`, `ix[1][2]`).
pub const STEREO_PRED_JOINT_ICDF: [u8; 25] = [
    249, 247, 246, 245, 244, 234, 210, 202, 201, 200, 197, 174, 82, 59, 56, 55, 54, 46, 22, 12, 11,
    10, 9, 7, 0,
];

/// Uniform iCDF for the 3-value component (`ix[n][0]`).
pub const STEREO_UNIFORM3_ICDF: [u8; 3] = [171, 85, 0];

/// Uniform iCDF for the 5-value sub-step component (`ix[n][1]`).
pub const STEREO_UNIFORM5_ICDF: [u8; 5] = [205, 154, 102, 51, 0];

/// Flag picking "only mid channel is coded" (2-symbol, ~50:50).
pub const STEREO_ONLY_CODE_MID_ICDF: [u8; 2] = [64, 0];

/// Stereo predictor quantization table (Q13), 16 entries. Consulted with
/// `idx = ix[0] + 3*ix[2]` ∈ [0, 15] and `idx+1` for interpolation.
pub const STEREO_PRED_QUANT_Q13: [i16; 16] = [
    -13732, -10050, -8266, -7526, -6500, -5000, -2950, -820, 820, 2950, 5000, 6500, 7526, 8266,
    10050, 13732,
];

// -------------------------------------------------------------------
// §4.2.4 LBRR flag packing for multi-frame (40/60 ms) packets.
// -------------------------------------------------------------------

/// LBRR flags iCDF for a 40 ms packet (2 sub-frames, 3 symbols).
pub const LBRR_FLAGS_2_ICDF: [u8; 3] = [203, 150, 0];
/// LBRR flags iCDF for a 60 ms packet (3 sub-frames, 7 symbols).
pub const LBRR_FLAGS_3_ICDF: [u8; 7] = [215, 195, 166, 125, 110, 82, 0];

#[cfg(test)]
mod gain_icdf_tests {
    //! Pin the §4.2.7.3 + §4.2.7.4 gain / frame-type ICDFs against the
    //! RFC 6716 PDF tables verbatim. Round-prior to the silence-rail
    //! fix all four of these tables held simplified approximations
    //! that on minimum-bitrate libopus packets mis-decoded the gain
    //! delta into the long tail and saturated the SILK synth IIR to
    //! ±0.7-1.0 (commit a6ca9ea silence-rail report). These tests
    //! prevent the regression by reconstructing each PDF from the
    //! stored ICDF and asserting it equals the RFC table.
    use super::*;
    fn pdf_from_icdf(icdf: &[u8]) -> Vec<u8> {
        let mut pdf = Vec::with_capacity(icdf.len());
        let mut prev: u16 = 256;
        for &v in icdf {
            // Each PDF entry = `prev_cumfreq - cumfreq`, where
            // `cumfreq = 256 - icdf[k]`.
            let cum = 256u16 - v as u16;
            assert!(cum >= 256u16 - prev, "icdf must be non-increasing");
            pdf.push((cum - (256u16 - prev)) as u8);
            prev = v as u16;
        }
        let total: u32 = pdf.iter().map(|&p| p as u32).sum();
        assert_eq!(total, 256, "PDF must sum to 256");
        pdf
    }

    #[test]
    fn frame_type_inactive_icdf_matches_rfc_table9() {
        // RFC 6716 §4.2.7.3 Table 9, Inactive row.
        assert_eq!(pdf_from_icdf(&FRAME_TYPE_INACTIVE_ICDF), vec![26, 230]);
    }

    #[test]
    fn frame_type_active_icdf_matches_rfc_table9() {
        // RFC 6716 §4.2.7.3 Table 9, Active row: `{0, 0, 24, 74, 148,
        // 10}/256` with the leading two zero-prob entries dropped (see
        // table doc comment for the offset convention).
        assert_eq!(
            pdf_from_icdf(&FRAME_TYPE_ACTIVE_ICDF),
            vec![24, 74, 148, 10]
        );
    }

    #[test]
    fn gain_msb_inactive_icdf_matches_rfc_table11() {
        assert_eq!(
            pdf_from_icdf(&GAIN_MSB_INACTIVE_ICDF),
            vec![32, 112, 68, 29, 12, 1, 1, 1]
        );
    }

    #[test]
    fn gain_msb_unvoiced_icdf_matches_rfc_table11() {
        assert_eq!(
            pdf_from_icdf(&GAIN_MSB_UNVOICED_ICDF),
            vec![2, 17, 45, 60, 62, 47, 19, 4]
        );
    }

    #[test]
    fn gain_msb_voiced_icdf_matches_rfc_table11() {
        assert_eq!(
            pdf_from_icdf(&GAIN_MSB_VOICED_ICDF),
            vec![1, 3, 26, 71, 94, 50, 9, 2]
        );
    }

    #[test]
    fn gain_lsb_icdf_matches_rfc_table12() {
        assert_eq!(
            pdf_from_icdf(&GAIN_LSB_ICDF),
            vec![32, 32, 32, 32, 32, 32, 32, 32]
        );
    }

    #[test]
    fn gain_delta_icdf_matches_rfc_table13() {
        // RFC 6716 §4.2.7.4 Table 13: 41-symbol delta gain PDF.
        let mut expected = vec![6u8, 5, 11, 31, 132, 21, 8, 4, 3, 2, 2, 2];
        expected.extend(std::iter::repeat(1u8).take(29));
        assert_eq!(pdf_from_icdf(&GAIN_DELTA_ICDF), expected);
    }
}
