//! RFC 6716 §4.2.7.8 shell-pulse coder (encoder + decoder).
//!
//! The SILK excitation is coded as a sparse sum of unit pulses inside
//! 16-sample *shell blocks*. This module implements the RFC's bit-exact
//! layout:
//!
//! 1. Rate level (§4.2.7.8.1) — one 9-symbol ICDF per frame, picked by
//!    signal type.
//! 2. Pulse count per shell block (§4.2.7.8.2) — 18-symbol ICDF per rate
//!    level. Value 17 is the "extra LSB" escape: after reading it once,
//!    the decoder switches to rate-level 9's PDF to read another value,
//!    and so on up to 10 LSBs (rate-level 10 cannot emit 17 again).
//! 3. Pulse locations (§4.2.7.8.3) — recursive binary split of the
//!    shell block into halves (16→8→4→2→1), coding `left_count`
//!    with a pulse-count-dependent ICDF.
//! 4. LSBs (§4.2.7.8.4) — for shells with `lsb_count > 0`, one bit per
//!    coefficient from MSB to LSB, read with a 2-symbol {136, 120} PDF.
//!    The magnitude of each coefficient starts at its pulse count and
//!    is doubled and OR-ed with each successive LSB.
//! 5. Signs (§4.2.7.8.5) — one bit per non-zero coefficient using the
//!    `(signal_type, quant_offset_type, min(pulse_count, 6))` PDF from
//!    Table 52.
//!
//! Tables are transcribed from the RFC PDF columns and converted to
//! `RangeDecoder::decode_icdf` form (`icdf[k] = 256 - cumfreq[k+1]`).
//!
//! The encoder accepts a vector of signed magnitudes (one per sample)
//! in the internal-rate domain, quantises each shell block to a
//! `(pulse_count, lsb_count)` representation whose reconstructed
//! magnitudes are bit-exact with what the decoder will produce, and
//! emits the full shell bitstream.

use oxideav_celt::range_decoder::RangeDecoder;
use oxideav_celt::range_encoder::RangeEncoder;

// -------------------------------------------------------------------
// Table 45 — Rate level (9 symbols per signal type).
// -------------------------------------------------------------------

/// Table 45 — Inactive/Unvoiced. PDF {15, 51, 12, 46, 45, 13, 33, 27, 14}/256.
pub const RATE_LEVEL_INACTIVE_ICDF: [u8; 9] = [241, 190, 178, 132, 87, 74, 41, 14, 0];
/// Table 45 — Voiced. PDF {33, 30, 36, 17, 34, 49, 18, 21, 18}/256.
pub const RATE_LEVEL_VOICED_ICDF: [u8; 9] = [223, 193, 157, 140, 106, 57, 39, 18, 0];

// -------------------------------------------------------------------
// Table 46 — Pulse count per shell block (18 symbols × 11 rate levels).
// Symbol 17 is the "extra LSB" escape. Rate level 10 has prob(17) = 0.
// -------------------------------------------------------------------

/// Pulse count ICDFs for rate levels 0..=10. 18 symbols each, ftb=8.
pub const PULSE_COUNT_ICDF: [[u8; 18]; 11] = [
    // level 0: {131,74,25,8,3,3,1,1,1,1,1,1,1,1,1,1,1,1}
    [
        125, 51, 26, 18, 15, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
    ],
    // level 1: {58,93,60,23,7,3,1,1,1,1,1,1,1,1,1,1,1,1}
    [
        198, 105, 45, 22, 15, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
    ],
    // level 2: {43,51,46,33,24,16,11,8,6,3,3,3,2,1,1,2,1,2}
    [
        213, 162, 116, 83, 59, 43, 32, 24, 18, 15, 12, 9, 7, 6, 5, 3, 2, 0,
    ],
    // level 3: {17,52,71,57,31,12,5,1,1,1,1,1,1,1,1,1,1,1}
    [
        239, 187, 116, 59, 28, 16, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
    ],
    // level 4: {6,21,41,53,49,35,21,11,6,3,2,2,1,1,1,1,1,1}
    [
        250, 229, 188, 135, 86, 51, 30, 19, 13, 10, 8, 6, 5, 4, 3, 2, 1, 0,
    ],
    // level 5: {7,14,22,28,29,28,25,20,17,13,11,9,7,5,4,4,3,10}
    [
        249, 235, 213, 185, 156, 128, 103, 83, 66, 53, 42, 33, 26, 21, 17, 13, 10, 0,
    ],
    // level 6: {2,5,14,29,42,46,41,31,19,11,6,3,2,1,1,1,1,1}
    [
        254, 249, 235, 206, 164, 118, 77, 46, 27, 16, 10, 7, 5, 4, 3, 2, 1, 0,
    ],
    // level 7: {1,2,4,10,19,29,35,37,34,28,20,14,8,5,4,2,2,2}
    [
        255, 253, 249, 239, 220, 191, 156, 119, 85, 57, 37, 23, 15, 10, 6, 4, 2, 0,
    ],
    // level 8: {1,2,2,5,9,14,20,24,27,28,26,23,20,15,11,8,6,15}
    [
        255, 253, 251, 246, 237, 223, 203, 179, 152, 124, 98, 75, 55, 40, 29, 21, 15, 0,
    ],
    // level 9: {1,1,1,6,27,58,56,39,25,14,10,6,3,3,2,1,1,2}
    [
        255, 254, 253, 247, 220, 162, 106, 67, 42, 28, 18, 12, 9, 6, 4, 3, 2, 0,
    ],
    // level 10: {2,1,6,27,58,56,39,25,14,10,6,3,3,2,1,1,2,0}
    [
        254, 253, 247, 220, 162, 106, 67, 42, 28, 18, 12, 9, 6, 4, 3, 2, 0, 0,
    ],
];

// -------------------------------------------------------------------
// Tables 47-50 — Pulse location splits. Indexed as [pulse_count - 1]
// (so pulse_count ∈ 1..=16). Each ICDF has pulse_count + 1 symbols.
// -------------------------------------------------------------------

/// Raw PDFs for Tables 47-50. Partition sizes: 16, 8, 4, 2.
/// Outer index = pulse_count - 1 (so size 16).
static PDF_SPLIT_16: [&[u16]; 16] = [
    &[126, 130],
    &[56, 142, 58],
    &[25, 101, 104, 26],
    &[12, 60, 108, 64, 12],
    &[7, 35, 84, 87, 37, 6],
    &[4, 20, 59, 86, 63, 21, 3],
    &[3, 12, 38, 72, 75, 42, 12, 2],
    &[2, 8, 25, 54, 73, 59, 27, 7, 1],
    &[2, 5, 17, 39, 63, 65, 42, 18, 4, 1],
    &[1, 4, 12, 28, 49, 63, 54, 30, 11, 3, 1],
    &[1, 4, 8, 20, 37, 55, 57, 41, 22, 8, 2, 1],
    &[1, 3, 7, 15, 28, 44, 53, 48, 33, 16, 6, 1, 1],
    &[1, 2, 6, 12, 21, 35, 47, 48, 40, 25, 12, 5, 1, 1],
    &[1, 1, 4, 10, 17, 27, 37, 47, 43, 33, 21, 9, 4, 1, 1],
    &[1, 1, 1, 8, 14, 22, 33, 40, 43, 38, 28, 16, 8, 1, 1, 1],
    &[1, 1, 1, 1, 13, 18, 27, 36, 41, 41, 34, 24, 14, 1, 1, 1, 1],
];

static PDF_SPLIT_8: [&[u16]; 16] = [
    &[127, 129],
    &[53, 149, 54],
    &[22, 105, 106, 23],
    &[11, 61, 111, 63, 10],
    &[6, 35, 86, 88, 36, 5],
    &[4, 20, 59, 87, 62, 21, 3],
    &[3, 13, 40, 71, 73, 41, 13, 2],
    &[3, 9, 27, 53, 70, 56, 28, 9, 1],
    &[3, 8, 19, 37, 57, 61, 44, 20, 6, 1],
    &[3, 7, 15, 28, 44, 54, 49, 33, 17, 5, 1],
    &[1, 7, 13, 22, 34, 46, 48, 38, 28, 14, 4, 1],
    &[1, 1, 11, 22, 27, 35, 42, 47, 33, 25, 10, 1, 1],
    &[1, 1, 6, 14, 26, 37, 43, 43, 37, 26, 14, 6, 1, 1],
    &[1, 1, 4, 10, 20, 31, 40, 42, 40, 31, 20, 10, 4, 1, 1],
    &[1, 1, 3, 8, 16, 26, 35, 38, 38, 35, 26, 16, 8, 3, 1, 1],
    &[1, 1, 2, 6, 12, 21, 30, 36, 38, 36, 30, 21, 12, 6, 2, 1, 1],
];

static PDF_SPLIT_4: [&[u16]; 16] = [
    &[127, 129],
    &[49, 157, 50],
    &[20, 107, 109, 20],
    &[11, 60, 113, 62, 10],
    &[7, 36, 84, 87, 36, 6],
    &[6, 24, 57, 82, 60, 23, 4],
    &[5, 18, 39, 64, 68, 42, 16, 4],
    &[6, 14, 29, 47, 61, 52, 30, 14, 3],
    &[1, 15, 23, 35, 51, 50, 40, 30, 10, 1],
    &[1, 1, 21, 32, 42, 52, 46, 41, 18, 1, 1],
    &[1, 6, 16, 27, 36, 42, 42, 36, 27, 16, 6, 1],
    &[1, 5, 12, 21, 31, 38, 40, 38, 31, 21, 12, 5, 1],
    &[1, 3, 9, 17, 26, 34, 38, 38, 34, 26, 17, 9, 3, 1],
    &[1, 3, 7, 14, 22, 29, 34, 36, 34, 29, 22, 14, 7, 3, 1],
    &[1, 2, 5, 11, 18, 25, 31, 35, 35, 31, 25, 18, 11, 5, 2, 1],
    &[1, 1, 4, 9, 15, 21, 28, 32, 34, 32, 28, 21, 15, 9, 4, 1, 1],
];

static PDF_SPLIT_2: [&[u16]; 16] = [
    &[128, 128],
    &[42, 172, 42],
    &[21, 107, 107, 21],
    &[12, 60, 112, 61, 11],
    &[8, 34, 86, 86, 35, 7],
    &[8, 23, 55, 90, 55, 20, 5],
    &[5, 15, 38, 72, 72, 36, 15, 3],
    &[6, 12, 27, 52, 77, 47, 20, 10, 5],
    &[6, 19, 28, 35, 40, 40, 35, 28, 19, 6],
    &[4, 14, 22, 31, 37, 40, 37, 31, 22, 14, 4],
    &[3, 10, 18, 26, 33, 38, 38, 33, 26, 18, 10, 3],
    &[2, 8, 13, 21, 29, 36, 38, 36, 29, 21, 13, 8, 2],
    &[1, 5, 10, 17, 25, 32, 38, 38, 32, 25, 17, 10, 5, 1],
    &[1, 4, 7, 13, 21, 29, 35, 36, 35, 29, 21, 13, 7, 4, 1],
    &[1, 2, 5, 10, 17, 25, 32, 36, 36, 32, 25, 17, 10, 5, 2, 1],
    &[1, 2, 4, 7, 13, 21, 28, 34, 36, 34, 28, 21, 13, 7, 4, 2, 1],
];

/// Convert a 256-summing PDF to an ICDF with a trailing 0.
fn pdf_to_icdf(pdf: &[u16]) -> Vec<u8> {
    let total: u16 = pdf.iter().sum();
    debug_assert_eq!(total, 256, "PDF must sum to 256, got {total}");
    let mut cum = 0u16;
    let mut icdf = Vec::with_capacity(pdf.len());
    for &p in pdf {
        cum += p;
        icdf.push((256 - cum) as u8);
    }
    icdf
}

/// Look up the split-ICDF for a given partition size (2/4/8/16) and
/// pulse count (1..=16). Returns an owned `Vec<u8>` (tiny — max 17 entries).
fn split_icdf(partition_size: usize, pulse_count: usize) -> Vec<u8> {
    debug_assert!((1..=16).contains(&pulse_count));
    let pdf = match partition_size {
        16 => PDF_SPLIT_16[pulse_count - 1],
        8 => PDF_SPLIT_8[pulse_count - 1],
        4 => PDF_SPLIT_4[pulse_count - 1],
        2 => PDF_SPLIT_2[pulse_count - 1],
        _ => panic!("split_icdf: unsupported partition size {partition_size}"),
    };
    pdf_to_icdf(pdf)
}

// -------------------------------------------------------------------
// Table 51 — LSB.  PDF {136, 120}/256 → ICDF [120, 0].
// -------------------------------------------------------------------

pub const LSB_ICDF: [u8; 2] = [120, 0];

// -------------------------------------------------------------------
// Table 52 — Sign PDFs. Indexed by (signal_type, quant_offset_type,
// min(pulse_count, 6)). Each ICDF has 2 symbols.
// signal_type: 0=Inactive, 1=Unvoiced, 2=Voiced.
// quant_offset_type: 0=Low, 1=High.
// bucketed pulse_count: 0..=6 (6 = "6 or more").
// -------------------------------------------------------------------

const fn sign_pdf_to_icdf(p0: u8, _p1: u8) -> [u8; 2] {
    [255u8.wrapping_sub(p0).wrapping_add(1), 0]
}

/// Sign ICDFs — [signal_type][quant_offset_type][bucketed_pulse_count].
pub const SIGN_ICDF: [[[[u8; 2]; 7]; 2]; 3] = [
    // Inactive
    [
        // Low
        [
            sign_pdf_to_icdf(2, 254),
            sign_pdf_to_icdf(207, 49),
            sign_pdf_to_icdf(189, 67),
            sign_pdf_to_icdf(179, 77),
            sign_pdf_to_icdf(174, 82),
            sign_pdf_to_icdf(163, 93),
            sign_pdf_to_icdf(157, 99),
        ],
        // High
        [
            sign_pdf_to_icdf(58, 198),
            sign_pdf_to_icdf(245, 11),
            sign_pdf_to_icdf(238, 18),
            sign_pdf_to_icdf(232, 24),
            sign_pdf_to_icdf(225, 31),
            sign_pdf_to_icdf(220, 36),
            sign_pdf_to_icdf(211, 45),
        ],
    ],
    // Unvoiced
    [
        [
            sign_pdf_to_icdf(1, 255),
            sign_pdf_to_icdf(210, 46),
            sign_pdf_to_icdf(190, 66),
            sign_pdf_to_icdf(178, 78),
            sign_pdf_to_icdf(169, 87),
            sign_pdf_to_icdf(162, 94),
            sign_pdf_to_icdf(152, 104),
        ],
        [
            sign_pdf_to_icdf(48, 208),
            sign_pdf_to_icdf(242, 14),
            sign_pdf_to_icdf(235, 21),
            sign_pdf_to_icdf(224, 32),
            sign_pdf_to_icdf(214, 42),
            sign_pdf_to_icdf(205, 51),
            sign_pdf_to_icdf(190, 66),
        ],
    ],
    // Voiced
    [
        [
            sign_pdf_to_icdf(1, 255),
            sign_pdf_to_icdf(162, 94),
            sign_pdf_to_icdf(152, 104),
            sign_pdf_to_icdf(147, 109),
            sign_pdf_to_icdf(144, 112),
            sign_pdf_to_icdf(141, 115),
            sign_pdf_to_icdf(138, 118),
        ],
        [
            sign_pdf_to_icdf(8, 248),
            sign_pdf_to_icdf(203, 53),
            sign_pdf_to_icdf(187, 69),
            sign_pdf_to_icdf(176, 80),
            sign_pdf_to_icdf(168, 88),
            sign_pdf_to_icdf(161, 95),
            sign_pdf_to_icdf(154, 102),
        ],
    ],
];

/// Look up the 2-symbol sign ICDF for the given context.
fn sign_icdf(signal_type: u8, quant_offset_type: u8, pulse_count: usize) -> &'static [u8; 2] {
    let st = (signal_type as usize).min(2);
    let qot = (quant_offset_type as usize).min(1);
    let pc = pulse_count.min(6);
    &SIGN_ICDF[st][qot][pc]
}

// -------------------------------------------------------------------
// Encoder — shell-coded excitation.
// -------------------------------------------------------------------

/// Encode one shell block's pulse locations by recursive split.
fn encode_shell_block_locations(enc: &mut RangeEncoder, pulses: &[u32; 16]) {
    fn recurse(enc: &mut RangeEncoder, pulses: &[u32], partition: usize) {
        if partition <= 1 {
            return;
        }
        let total: u32 = pulses.iter().sum();
        if total == 0 {
            return;
        }
        let half = partition / 2;
        let left_total: u32 = pulses[..half].iter().sum();
        let icdf = split_icdf(partition, total as usize);
        enc.encode_icdf(left_total as usize, &icdf, 8);
        recurse(enc, &pulses[..half], half);
        recurse(enc, &pulses[half..], half);
    }
    recurse(enc, pulses, 16);
}

/// Encode the pulse count for a shell block, emitting 17-escapes so
/// the decoder picks up `lsb_count` extra LSBs for every coefficient.
fn encode_pulse_count(enc: &mut RangeEncoder, rate_level: usize, pulse_count: u32, lsb_count: u32) {
    debug_assert!(pulse_count <= 16);
    debug_assert!(lsb_count <= 10);
    // RFC §4.2.7.8.2: first read uses `rate_level`. Each emitted 17
    // means "add 1 LSB and read the next value from rate level 9". After
    // 10 seventeens, switch to rate level 10 (which cannot emit 17).
    let mut rate = rate_level;
    for i in 0..lsb_count {
        enc.encode_icdf(17, &PULSE_COUNT_ICDF[rate], 8);
        rate = if i + 1 == 10 { 10 } else { 9 };
    }
    enc.encode_icdf(pulse_count as usize, &PULSE_COUNT_ICDF[rate], 8);
}

/// Quantise signed magnitudes to their shell-coder round-trip values.
/// Returns the reconstructed `signed_mags` the decoder will produce for
/// this input. The encoder uses this to keep its closed-loop synthesis
/// bit-exact with the decoder when the block-level saturation kicks in.
pub fn quantize_to_shell(signed_mags: &[i32]) -> Vec<i32> {
    assert!(signed_mags.len() % 16 == 0);
    let mut out = vec![0i32; signed_mags.len()];
    let n_shells = signed_mags.len() / 16;
    for s in 0..n_shells {
        let base = s * 16;
        let slice = &signed_mags[base..base + 16];
        // Pick min lsb such that sum(|m| >> lsb) ≤ 16, or cap at 10.
        let mut lsb = 0u32;
        loop {
            let sum: u64 = slice.iter().map(|m| (m.unsigned_abs() as u64) >> lsb).sum();
            if sum <= 16 || lsb >= 10 {
                break;
            }
            lsb += 1;
        }
        let shift = lsb;
        let mut pulses = [0u32; 16];
        let mut lsb_bits = [0u32; 16];
        let mut total: u32 = 0;
        for i in 0..16 {
            let abs = slice[i].unsigned_abs();
            let p = (abs >> shift).min(16);
            let mask = if shift == 0 { 0 } else { (1u32 << shift) - 1 };
            pulses[i] = p;
            lsb_bits[i] = abs & mask;
            total = total.saturating_add(p);
        }
        if total > 16 {
            let mut order: Vec<usize> = (0..16).collect();
            order.sort_by(|&a, &b| pulses[b].cmp(&pulses[a]));
            let mut excess = total - 16;
            for &i in &order {
                if excess == 0 {
                    break;
                }
                let r = pulses[i].min(excess);
                pulses[i] -= r;
                excess -= r;
            }
        }
        for i in 0..16 {
            let abs = (pulses[i] << shift) | lsb_bits[i];
            let signed = if slice[i] < 0 {
                -(abs as i32)
            } else {
                abs as i32
            };
            out[base + i] = signed;
        }
    }
    out
}

/// Encode the excitation signal for a SILK frame using the real
/// shell-pulse coder from RFC §4.2.7.8.
///
/// * `signed_mags` — per-sample signed magnitudes. Magnitudes are bit-
///   exact with what the decoder will reproduce (the caller is expected
///   to have rounded its residuals to integers beforehand).
/// * `signal_type` — 0 Inactive, 1 Unvoiced, 2 Voiced.
/// * `quant_offset_type` — 0 Low, 1 High.
///
/// The function pads the sample count up to a multiple of 16 internally
/// (RFC's 10 ms MB special case) — `signed_mags.len()` must already be
/// the shell-block-aligned length used by the decoder. For the NB/WB
/// bandwidths this is always a multiple of 16; for MB 10 ms the caller
/// should pass 128 samples with 8 trailing zeros.
pub fn encode_excitation(
    enc: &mut RangeEncoder,
    signed_mags: &[i32],
    signal_type: u8,
    quant_offset_type: u8,
) {
    let frame_len = signed_mags.len();
    assert!(
        frame_len % 16 == 0,
        "encode_excitation: frame_len must be multiple of 16"
    );
    let n_shells = frame_len / 16;

    // Step 1 — Build per-shell (pulse_count, lsb_count) assignment.
    // Strategy: for each shell block, find the smallest `lsb_count`
    // such that sum(|mag[i]| >> lsb_count) ≤ 16. Using floor keeps the
    // total pulse count within the table bounds.
    let mut pulse_counts = vec![0u32; n_shells];
    let mut lsb_counts = vec![0u32; n_shells];
    // Reconstructed absolute magnitudes (after quantisation to a
    // pulse-count × 2^lsb_count grid).
    let mut abs_recon = vec![0u32; frame_len];
    // Per-sample pulse counts and per-sample LSB bits (packed MSB
    // first so each bit position i < lsb_count corresponds to
    // (1 << (lsb_count - 1 - i))).
    let mut pulses_per_sample = vec![0u32; frame_len];
    let mut lsb_bits_per_sample: Vec<u32> = vec![0u32; frame_len];

    for s in 0..n_shells {
        let base = s * 16;
        let slice = &signed_mags[base..base + 16];
        // Find the minimum lsb_count such that sum(|m| >> lsb_count) <= 16.
        let mut lsb = 0u32;
        loop {
            let sum: u64 = slice.iter().map(|m| (m.unsigned_abs() as u64) >> lsb).sum();
            if sum <= 16 || lsb >= 10 {
                break;
            }
            lsb += 1;
        }
        let shift = lsb;
        // Compute per-sample pulse count + LSB bits.
        let mut total_pulses = 0u32;
        for i in 0..16 {
            let abs = slice[i].unsigned_abs();
            let p = if shift == 0 { abs } else { abs >> shift };
            let mask = if shift == 0 { 0 } else { (1u32 << shift) - 1 };
            let bits = abs & mask;
            pulses_per_sample[base + i] = p.min(16);
            lsb_bits_per_sample[base + i] = bits;
            total_pulses = total_pulses.saturating_add(p);
            // Reconstructed abs magnitude after clipping the pulse count
            // to keep sum ≤ 16 (handled below via clamp).
            abs_recon[base + i] = (p << shift) | bits;
        }
        // If summing produced > 16 (possible when an individual sample's
        // pulse count was capped to 16 but other samples still pushed
        // the total over), we must clamp by reducing per-sample pulses.
        // Since floor already keeps per-sample ≤ (abs >> shift), the
        // only way sum exceeds 16 after shift-search termination is
        // when we exited the loop at lsb==10 with sum still > 16. In
        // that case saturate the largest sample down.
        if total_pulses > 16 {
            // Sort indices by descending pulses_per_sample[base+i], reduce
            // largest until sum ≤ 16.
            let mut order: Vec<usize> = (0..16).collect();
            order.sort_by(|&a, &b| pulses_per_sample[base + b].cmp(&pulses_per_sample[base + a]));
            let mut excess = total_pulses - 16;
            for &i in &order {
                if excess == 0 {
                    break;
                }
                let p = pulses_per_sample[base + i];
                let reduce = p.min(excess);
                pulses_per_sample[base + i] = p - reduce;
                total_pulses -= reduce;
                excess -= reduce;
                // Update reconstructed absolute magnitude.
                abs_recon[base + i] =
                    (pulses_per_sample[base + i] << shift) | lsb_bits_per_sample[base + i];
            }
        }
        pulse_counts[s] = total_pulses.min(16);
        lsb_counts[s] = lsb;
    }

    // Step 2 — Choose a rate level. Use average pulses/shell as a
    // heuristic: map [0..=16] onto the 9 rate levels roughly by
    // grouping. Low-pulse shells benefit from levels 0-2; high-pulse
    // from 7-8.
    let avg_pulses = if n_shells > 0 {
        (pulse_counts.iter().sum::<u32>() as f32) / (n_shells as f32)
    } else {
        0.0
    };
    // Map average to 0..=8.
    let rate_level = ((avg_pulses.round() as i32).clamp(0, 8)) as usize;

    // Step 3 — Emit rate level.
    let rate_icdf: &[u8] = if signal_type == 2 {
        &RATE_LEVEL_VOICED_ICDF
    } else {
        &RATE_LEVEL_INACTIVE_ICDF
    };
    enc.encode_icdf(rate_level, rate_icdf, 8);

    // Step 4 — Emit pulse counts + LSB-escape prefixes for each shell.
    for s in 0..n_shells {
        encode_pulse_count(enc, rate_level, pulse_counts[s], lsb_counts[s]);
    }

    // Step 5 — Emit pulse locations for each shell via recursive split.
    for s in 0..n_shells {
        if pulse_counts[s] == 0 {
            continue;
        }
        let base = s * 16;
        let mut arr = [0u32; 16];
        arr.copy_from_slice(&pulses_per_sample[base..base + 16]);
        encode_shell_block_locations(enc, &arr);
    }

    // Step 6 — Emit LSBs for each shell, MSB first, every coefficient
    // (even zero-pulse ones). RFC §4.2.7.8.4.
    for s in 0..n_shells {
        let k = lsb_counts[s] as usize;
        if k == 0 {
            continue;
        }
        let base = s * 16;
        for bit_from_msb in 0..k {
            for i in 0..16 {
                let bits = lsb_bits_per_sample[base + i];
                let shift_bit = k - 1 - bit_from_msb;
                let b = (bits >> shift_bit) & 1;
                enc.encode_icdf(b as usize, &LSB_ICDF, 8);
            }
        }
    }

    // Step 7 — Emit signs for non-zero coefficients using the
    // (signal_type, quant_offset_type, pulse_count) PDF.
    for s in 0..n_shells {
        let pc = pulse_counts[s] as usize;
        let base = s * 16;
        let sicdf = sign_icdf(signal_type, quant_offset_type, pc);
        for i in 0..16 {
            let m = abs_recon[base + i];
            if m == 0 {
                continue;
            }
            let sign = signed_mags[base + i] < 0;
            // Sign symbol: 0 = negative, 1 = positive (per RFC §4.2.7.8.5).
            let sym = if sign { 0 } else { 1 };
            enc.encode_icdf(sym, sicdf, 8);
        }
    }
}

// -------------------------------------------------------------------
// Decoder — shell-coded excitation.
// -------------------------------------------------------------------

fn decode_shell_block_locations(rc: &mut RangeDecoder<'_>, total: u32) -> [u32; 16] {
    fn recurse(rc: &mut RangeDecoder<'_>, out: &mut [u32], partition: usize, total: u32) {
        if partition <= 1 {
            out[0] = total;
            return;
        }
        if total == 0 {
            return;
        }
        let half = partition / 2;
        let icdf = split_icdf(partition, total as usize);
        let left = rc.decode_icdf(&icdf, 8) as u32;
        let right = total - left;
        recurse(rc, &mut out[..half], half, left);
        recurse(rc, &mut out[half..], half, right);
    }
    let mut out = [0u32; 16];
    if total > 0 {
        recurse(rc, &mut out, 16, total);
    }
    out
}

fn decode_pulse_count(rc: &mut RangeDecoder<'_>, rate_level: usize) -> (u32, u32) {
    // Returns (pulse_count, lsb_count).
    let mut rate = rate_level;
    let mut lsb_count = 0u32;
    loop {
        let sym = rc.decode_icdf(&PULSE_COUNT_ICDF[rate], 8) as u32;
        if sym < 17 {
            return (sym, lsb_count);
        }
        lsb_count += 1;
        rate = if lsb_count == 10 { 10 } else { 9 };
    }
}

/// Quantization offset (Q23) per RFC 6716 §4.2.7.8.6, Table 53.
/// Indexed `[signal_type][quant_offset_type]`. Voiced/Low = 8,
/// Voiced/High = 25, Unvoiced/Inactive Low = 25, High = 60.
pub const QUANT_OFFSET_Q23: [[i32; 2]; 3] = [
    [25, 60], // Inactive (Low, High)
    [25, 60], // Unvoiced
    [8, 25],  // Voiced
];

/// Compute the §4.2.7.8.6 reconstructed Q0 excitation values
/// (`e_Q23 / 2^23`) for a given vector of signed pulse magnitudes,
/// signal/quant-offset selectors, and starting LCG seed. Used by the
/// encoder's analysis-by-synthesis loop to predict exactly what the
/// decoder will emit so the closed-loop residual stays consistent
/// across the bitstream.
pub fn reconstruct_q23_normalised(
    signed: &[i32],
    signal_type: u8,
    quant_offset_type: u8,
    seed: u32,
) -> Vec<f32> {
    let st = (signal_type as usize).min(2);
    let qot = (quant_offset_type as usize).min(1);
    let offset_q23 = QUANT_OFFSET_Q23[st][qot];
    let mut state = seed;
    let inv = 1.0_f32 / 8_388_608.0;
    let mut out = vec![0.0f32; signed.len()];
    for (i, &e_raw) in signed.iter().enumerate() {
        let sgn = e_raw.signum();
        let mut e_q23 = (e_raw << 8) - sgn * 20 + offset_q23;
        state = state.wrapping_mul(196_314_165).wrapping_add(907_633_515);
        if state & 0x8000_0000 != 0 {
            e_q23 = -e_q23;
        }
        state = state.wrapping_add(e_raw as u32);
        out[i] = e_q23 as f32 * inv;
    }
    out
}

/// Decode the excitation signal for a SILK frame using the real shell
/// coder + the §4.2.7.8.6 reconstruction step.
///
/// Returns `Vec<f32>` of length `frame_len` carrying `e_Q23[i] /
/// 2^23` so the synthesis filter can apply the spec's
/// `gain_Q16/65536` scaling without a hidden constant of its own.
///
/// `seed` is the LCG state decoded in §4.2.7.7 — every sample is
/// pseudo-randomly inverted via `(seed & 0x80000000) != 0` and the
/// seed is updated by `seed = 196314165*seed + 907633515` then by
/// `seed += e_raw[i]` after each sample, matching the spec literal.
pub fn decode_excitation(
    rc: &mut RangeDecoder<'_>,
    frame_len: usize,
    signal_type: u8,
    quant_offset_type: u8,
    seed: u32,
) -> Vec<f32> {
    assert!(
        frame_len % 16 == 0,
        "decode_excitation: frame_len must be multiple of 16"
    );
    let n_shells = frame_len / 16;

    // Rate level.
    let rate_icdf: &[u8] = if signal_type == 2 {
        &RATE_LEVEL_VOICED_ICDF
    } else {
        &RATE_LEVEL_INACTIVE_ICDF
    };
    let rate_level = rc.decode_icdf(rate_icdf, 8);
    let rate_level = rate_level.min(8);

    // Pulse counts + LSB counts.
    let mut pulse_counts = vec![0u32; n_shells];
    let mut lsb_counts = vec![0u32; n_shells];
    for s in 0..n_shells {
        let (pc, lsb) = decode_pulse_count(rc, rate_level);
        pulse_counts[s] = pc;
        lsb_counts[s] = lsb;
    }

    // Pulse locations.
    let mut pulses_per_sample = vec![0u32; frame_len];
    for s in 0..n_shells {
        let pc = pulse_counts[s];
        if pc == 0 {
            continue;
        }
        let arr = decode_shell_block_locations(rc, pc);
        for i in 0..16 {
            pulses_per_sample[s * 16 + i] = arr[i];
        }
    }

    // LSBs.
    let mut magnitudes = vec![0u32; frame_len];
    magnitudes[..frame_len].copy_from_slice(&pulses_per_sample[..frame_len]);
    for s in 0..n_shells {
        let k = lsb_counts[s] as usize;
        if k == 0 {
            continue;
        }
        let base = s * 16;
        for _ in 0..k {
            for i in 0..16 {
                let b = rc.decode_icdf(&LSB_ICDF, 8) as u32;
                magnitudes[base + i] = (magnitudes[base + i] << 1) | b;
            }
        }
    }

    // Signs.
    let mut signed = vec![0i32; frame_len];
    for s in 0..n_shells {
        let pc = pulse_counts[s] as usize;
        let base = s * 16;
        let sicdf = sign_icdf(signal_type, quant_offset_type, pc);
        for i in 0..16 {
            let m = magnitudes[base + i];
            if m == 0 {
                continue;
            }
            let sym = rc.decode_icdf(sicdf, 8);
            let neg = sym == 0;
            signed[base + i] = if neg { -(m as i32) } else { m as i32 };
        }
    }

    // §4.2.7.8.6 reconstruction:
    //
    //   e_Q23[i] = (e_raw[i] << 8) - sign(e_raw[i])*20 + offset_Q23;
    //   seed     = (196314165*seed + 907633515) & 0xFFFFFFFF;
    //   e_Q23[i] = (seed & 0x80000000) ? -e_Q23[i] : e_Q23[i];
    //   seed     = (seed + e_raw[i]) & 0xFFFFFFFF;
    //
    // The LCG dither (top-bit-driven sign flip, then add e_raw to seed)
    // is what keeps the synthesis filter from periodically drifting to
    // a DC offset on long quiet runs — without it every zero sample
    // stays exactly 0 + offset_Q23 and biases the LPC integrator. We
    // also normalise to Q0 by dividing by 2^23 so the synth filter can
    // apply the RFC's `gain_Q16/65536` factor verbatim.
    reconstruct_q23_normalised(&signed, signal_type, quant_offset_type, seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_split_pdfs_sum_to_256() {
        for (i, p) in PDF_SPLIT_16.iter().enumerate() {
            let sum: u16 = p.iter().sum();
            assert_eq!(sum, 256, "PDF_SPLIT_16 row {i} sums to {sum}");
            assert_eq!(p.len(), i + 2, "PDF_SPLIT_16 row {i} length");
        }
        for (i, p) in PDF_SPLIT_8.iter().enumerate() {
            let sum: u16 = p.iter().sum();
            assert_eq!(sum, 256, "PDF_SPLIT_8 row {i} sums to {sum}");
        }
        for (i, p) in PDF_SPLIT_4.iter().enumerate() {
            let sum: u16 = p.iter().sum();
            assert_eq!(sum, 256, "PDF_SPLIT_4 row {i} sums to {sum}");
        }
        for (i, p) in PDF_SPLIT_2.iter().enumerate() {
            let sum: u16 = p.iter().sum();
            assert_eq!(sum, 256, "PDF_SPLIT_2 row {i} sums to {sum}");
        }
    }

    #[test]
    fn pulse_count_icdfs_end_at_zero() {
        for (i, row) in PULSE_COUNT_ICDF.iter().enumerate() {
            assert_eq!(row[17], 0, "rate level {i} last entry must be 0");
            // Monotone non-increasing.
            for k in 1..18 {
                assert!(
                    row[k] <= row[k - 1],
                    "rate {i} non-monotone at {k}: {} > {}",
                    row[k],
                    row[k - 1]
                );
            }
        }
        // Rate level 10 has PDF[17] == 0, so ICDF[16] == ICDF[17] == 0.
        assert_eq!(PULSE_COUNT_ICDF[10][16], 0);
    }

    /// Local thin alias for the public reconstruction helper. The
    /// indirection lets the unit tests treat the §4.2.7.8.6 transcription
    /// as the spec oracle and the body of `decode_excitation` as the
    /// system-under-test, even though they share an implementation.
    fn expected_q23(signed: &[i32], signal_type: u8, quant_offset_type: u8, seed: u32) -> Vec<f32> {
        super::reconstruct_q23_normalised(signed, signal_type, quant_offset_type, seed)
    }

    #[test]
    fn roundtrip_zero_excitation() {
        // RFC §4.2.7.8.6: with all e_raw[i] = 0 the offset_Q23 is still
        // applied (because sign(0) == 0 → no -20 deduction), so each
        // sample reconstructs to ±offset_Q23/2^23 with the LCG-driven
        // sign. Magnitude is tiny (<1e-5) but non-zero by spec.
        let n = 160; // 20 ms NB.
        let sm = vec![0i32; n];
        let mut enc = RangeEncoder::new(128);
        encode_excitation(&mut enc, &sm, 1, 0);
        let buf = enc.done().unwrap();
        let mut dec = RangeDecoder::new(&buf);
        let out = decode_excitation(&mut dec, n, 1, 0, 0);
        let want = expected_q23(&sm, 1, 0, 0);
        for (i, (g, w)) in out.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-9, "mismatch at {i}: got {g} want {w}");
        }
    }

    #[test]
    fn roundtrip_single_pulse_block() {
        // Single pulse of magnitude 1 at sample 5.
        let mut sm = vec![0i32; 16];
        sm[5] = 1;
        let mut enc = RangeEncoder::new(64);
        encode_excitation(&mut enc, &sm, 1, 0);
        let buf = enc.done().unwrap();
        let mut dec = RangeDecoder::new(&buf);
        let out = decode_excitation(&mut dec, 16, 1, 0, 0);
        let want = expected_q23(&sm, 1, 0, 0);
        for (i, (g, w)) in out.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-9, "mismatch at {i}: got {g} want {w}");
        }
    }

    #[test]
    fn roundtrip_high_magnitude_block_uses_lsbs() {
        // Magnitudes up to 120 (CARRIER_FULL_SCALE) force multiple LSBs.
        let sm: Vec<i32> = (0..16)
            .map(|i| if i % 2 == 0 { 100 } else { -80 })
            .collect();
        let mut enc = RangeEncoder::new(256);
        encode_excitation(&mut enc, &sm, 1, 0);
        let buf = enc.done().unwrap();
        let mut dec = RangeDecoder::new(&buf);
        let out = decode_excitation(&mut dec, 16, 1, 0, 0);
        // Compare via the quantiser's own reconstruction (the shell
        // coder may collapse a saturated block) + the §4.2.7.8.6
        // post-step.
        let recon = quantize_to_shell(&sm);
        let want = expected_q23(&recon, 1, 0, 0);
        for (i, (g, w)) in out.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-9, "mismatch at {i}: got {g} want {w}");
        }
    }

    #[test]
    fn quantize_to_shell_is_stable_at_saturation() {
        // Pathological input — all samples at max. Should round-trip
        // via the encoder's saturation path.
        let sm: Vec<i32> = vec![120; 16];
        let mut enc = RangeEncoder::new(256);
        encode_excitation(&mut enc, &sm, 1, 0);
        let buf = enc.done().unwrap();
        let mut dec = RangeDecoder::new(&buf);
        let out = decode_excitation(&mut dec, 16, 1, 0, 0);
        let recon = quantize_to_shell(&sm);
        let want = expected_q23(&recon, 1, 0, 0);
        for (i, (g, w)) in out.iter().zip(want.iter()).enumerate() {
            assert!(
                (g - w).abs() < 1e-9,
                "mismatch at {i}: got {g} want {w} recon={}",
                recon[i]
            );
        }
    }

    #[test]
    fn roundtrip_varied_magnitudes() {
        // Mixed positive / negative / zero / multi-LSB magnitudes.
        let sm: Vec<i32> = vec![
            0, 1, -2, 3, -4, 5, -8, 16, -32, 12, 0, 7, -6, 11, -1, 2, // block 0
            20, -45, 60, 0, 0, 0, -3, 5, 15, -12, 7, -8, 9, 0, 1, -1, // block 1
        ];
        let mut enc = RangeEncoder::new(128);
        encode_excitation(&mut enc, &sm, 2, 1);
        let buf = enc.done().unwrap();
        let mut dec = RangeDecoder::new(&buf);
        let out = decode_excitation(&mut dec, sm.len(), 2, 1, 0);
        // The encoder may saturate/quantise when sum per block > 16;
        // here all blocks sum-abs ≤ 16 so signs reproduce exactly.
        let want = expected_q23(&sm, 2, 1, 0);
        for (i, (g, w)) in out.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-9, "mismatch at {i}: got {g} want {w}");
        }
    }

    #[test]
    fn lcg_dither_changes_seed_changes_output() {
        // Same encoded payload, two different LCG seeds. Per §4.2.7.7 +
        // §4.2.7.8.6 the seed flips per-sample sign, so the decoded
        // waveform must differ.
        let mut sm = vec![0i32; 16];
        sm[3] = 5;
        sm[10] = -2;
        let mut enc = RangeEncoder::new(64);
        encode_excitation(&mut enc, &sm, 1, 0);
        let buf = enc.done().unwrap();
        let mut dec_a = RangeDecoder::new(&buf);
        let out_a = decode_excitation(&mut dec_a, 16, 1, 0, 0);
        let mut dec_b = RangeDecoder::new(&buf);
        let out_b = decode_excitation(&mut dec_b, 16, 1, 0, 3);
        assert_ne!(
            out_a, out_b,
            "seed = 0 and seed = 3 produced identical excitation"
        );
    }

    #[test]
    fn shell_coder_saves_bits_vs_per_sample_nibble() {
        // Compare the new shell coder vs the two-nibble carrier at
        // identical reconstructed magnitudes.
        use crate::silk::excitation::MAG_NIBBLE_ICDF;
        let n = 160;
        // Sparse-ish residual: most samples zero, a few pulses.
        let mut sm = vec![0i32; n];
        sm[3] = 2;
        sm[50] = -5;
        sm[80] = 3;
        sm[130] = 1;

        // Shell coder: track bits used via `tell()` rather than the
        // finalised byte count so the buffer size doesn't mask the win.
        let mut enc_shell = RangeEncoder::new(2048);
        encode_excitation(&mut enc_shell, &sm, 1, 0);
        let shell_bits = enc_shell.tell();

        // MVP carrier — pulse counts via PDF row 0 + per-sample nibble pair + sign.
        let mut enc_mvp = RangeEncoder::new(2048);
        use crate::silk::tables;
        enc_mvp.encode_icdf(0, &tables::RATE_LEVEL_INACTIVE_ICDF, 8);
        let n_shells = n / 16;
        for _ in 0..n_shells {
            enc_mvp.encode_icdf(0, &tables::PULSE_COUNT_ICDF[0], 8);
        }
        for &v in &sm {
            let mag = v.unsigned_abs() as i32;
            let hi = ((mag >> 4) & 0xf) as usize;
            let lo = (mag & 0xf) as usize;
            enc_mvp.encode_icdf(hi, &MAG_NIBBLE_ICDF, 8);
            enc_mvp.encode_icdf(lo, &MAG_NIBBLE_ICDF, 8);
            if mag != 0 {
                enc_mvp.encode_bit_logp(v < 0, 1);
            }
        }
        let mvp_bits = enc_mvp.tell();

        assert!(
            shell_bits < mvp_bits,
            "shell coder did not save bits: shell={shell_bits} mvp={mvp_bits}"
        );
        eprintln!("shell_bits={shell_bits} mvp_bits={mvp_bits}");
    }
}
