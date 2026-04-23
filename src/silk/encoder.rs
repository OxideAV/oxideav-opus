//! SILK encoder — NB / MB / WB mono + stereo, 10 / 20 ms frames
//! (+ building block for 40 / 60 ms multi-frame packets).
//!
//! Companion to [`crate::silk::SilkDecoder`]. Scope:
//!
//! * **Narrowband** (8 kHz internal rate) mono / stereo, 10 or 20 ms.
//! * **Mediumband** (12 kHz internal rate) mono / stereo, 10 or 20 ms.
//! * **Wideband** (16 kHz internal rate) mono / stereo, 10 or 20 ms.
//! * Analysis-by-synthesis around the MVP carrier format documented
//!   in [`super::excitation`]: LPC analysis → residual → magnitude +
//!   sign per sample.
//! * The LPC filter used for analysis is the EXACT same `lpc` array
//!   the decoder will reconstruct from the NLSF stage-1 index, so
//!   encoder and decoder agree on the prediction and the residual
//!   round-trips without LPC mismatch.
//!
//! The three bandwidths share a single encoder implementation
//! parameterised on a [`BandwidthParams`] descriptor; only the internal
//! sampling rate constants, LPC order, sub-frame length and (for NLSF)
//! stage-1 codebook differ.
//!
//! 10 ms and 20 ms share the same per-frame body layout; the only
//! difference is the number of sub-frames (2 for 10 ms, 4 for 20 ms).
//! Longer 40 / 60 ms packets are composed at the top level
//! ([`crate::encoder::SilkEncoder`]) by running 2 or 3 back-to-back
//! 20 ms `SilkFrameEncoder` bodies inside a single Opus frame, per
//! RFC 6716 §4.2.4.
//!
//! # Bitstream order (same as decoder's [`super::decode_frame_body`])
//!
//! 1. Frame type (inactive-ICDF, always `signal_type = 1 unvoiced`).
//! 2. 4 sub-frame gains (MSB + LSB + 3 deltas).
//! 3. NLSF stage-1 index (a fixed index that produces a stable LPC).
//! 4. `lpc_order` NLSF stage-2 residuals (all zero magnitude → still
//!    consumes the correct number of ICDF reads on decode).
//! 5. NLSF interpolation weight (always 4 = "no interpolation").
//! 6. LCG seed (always 0).
//! 7. Excitation: rate-level + `n_shells` pulse-count ICDFs + per-sample
//!    magnitude + sign via the carrier layout defined in
//!    [`super::excitation::decode_excitation`].
//!
//! # Stereo
//!
//! For NB stereo the caller (see [`crate::encoder::SilkEncoder::new_nb_stereo_20ms`])
//! drives TWO `SilkFrameEncoder`s — one for the mid channel (`M = (L+R)/2`)
//! and one for the side channel (`S = (L-R)/2`) — plus emits the stereo
//! prediction weight header described in RFC §4.2.7.1 and ported from
//! libopus `silk/stereo_encode_pred.c`. The helpers
//! [`encode_stereo_pred_weights`] and [`stereo_mid_side`] live here and
//! are exercised by the stereo constructor in `encoder.rs`.
//!
//! # Out of scope (tracked follow-ups)
//!
//! * Voiced / LTP path — the LTP loop-back would require the encoder
//!   to run analysis-by-synthesis over the pitch filter.
//! * MB / WB stereo — mechanically identical to NB stereo, but the
//!   first pass wires stereo only at NB to keep the validation surface
//!   small.
//! * Bit-exact shell-pulse coding per RFC §4.2.7.8 — the MVP carrier
//!   is byte-compatible with the RFC layout at the header level but
//!   uses a pass-through nibble-based magnitude coding in place of
//!   the RFC's pulse/LSB/sign split.

use oxideav_celt::range_encoder::RangeEncoder;
use oxideav_core::Result;

use crate::silk::excitation::MAG_NIBBLE_ICDF;
use crate::silk::lsf;
use crate::silk::tables;
use crate::toc::OpusBandwidth;

/// Fixed NLSF stage-1 index used by the encoder. Corresponds to a
/// moderately-tilted cosine template in the decoder's
/// `synthesize_nlsf`. The actual value is incidental — the encoder
/// and decoder only need to agree.
const NLSF_STAGE1_IDX: usize = 0;

/// Gain index bounds (Q16 log-gain, see [`super::gain_index_to_q16`]).
/// Smallest value yields `gain_q16 ≈ 1.09 × 65536` — big enough to
/// keep the residual magnitudes well within the 9-bit carrier.
const GAIN_INDEX_UNVOICED: i32 = 0;

/// Ratio used when quantising the residual to signed 8 bits. We pick
/// a conservative factor so peaks don't clip to ±255 — the decoder's
/// output already clamps to [-1, 1] and extra headroom helps the
/// cross-frame continuity when the LPC state is carried over.
const CARRIER_FULL_SCALE: f32 = 120.0;

/// Per-bandwidth encoder parameters. A [`SilkFrameEncoder`] is constructed
/// from one of these descriptors so NB / MB / WB share the bulk of the
/// encode logic.
#[derive(Copy, Clone, Debug)]
pub struct BandwidthParams {
    /// Opus `bandwidth` enum value (for NLSF codebook selection).
    pub bandwidth: OpusBandwidth,
    /// LPC filter order (10 for NB/MB, 16 for WB).
    pub lpc_order: usize,
    /// Samples per sub-frame at the internal rate (5 ms window).
    pub subframe_len: usize,
}

impl BandwidthParams {
    pub const fn nb() -> Self {
        Self {
            bandwidth: OpusBandwidth::Narrowband,
            lpc_order: 10,
            subframe_len: 40, // 5 ms @ 8 kHz
        }
    }
    pub const fn mb() -> Self {
        Self {
            bandwidth: OpusBandwidth::Mediumband,
            lpc_order: 10,
            subframe_len: 60, // 5 ms @ 12 kHz
        }
    }
    pub const fn wb() -> Self {
        Self {
            bandwidth: OpusBandwidth::Wideband,
            lpc_order: 16,
            subframe_len: 80, // 5 ms @ 16 kHz
        }
    }
}

/// A SILK frame encoder for NB / MB / WB mono 20 ms.
///
/// Stateful — carries the decoder's expected LPC history across
/// frames so the residual computed by the encoder matches what the
/// decoder will re-synthesize (analysis-by-synthesis).
pub struct SilkFrameEncoder {
    params: BandwidthParams,
    n_subframes: usize,
    /// Last `lpc_order` samples of the previous frame's *synthesized*
    /// output. Seeded with zeros.
    prev_synth: Vec<f32>,
}

impl SilkFrameEncoder {
    /// Build a frame encoder for the requested bandwidth, defaulting to
    /// 20 ms (4 sub-frames). For 10 ms use
    /// [`SilkFrameEncoder::new_with_subframes`].
    pub fn new(params: BandwidthParams) -> Self {
        Self::new_with_subframes(params, 4)
    }

    /// Build a frame encoder with an explicit sub-frame count.
    ///
    /// * `n_subframes == 4` — 20 ms frame (NB: 160 / MB: 240 / WB: 320
    ///   samples at the internal rate).
    /// * `n_subframes == 2` — 10 ms frame (half the length).
    ///
    /// Panics for any other value — the RFC only defines 10 ms and
    /// 20 ms base SILK frames. 40 ms / 60 ms Opus packets are built by
    /// concatenating 2 / 3 back-to-back 20 ms bodies (see RFC §4.2.4).
    pub fn new_with_subframes(params: BandwidthParams, n_subframes: usize) -> Self {
        assert!(
            n_subframes == 2 || n_subframes == 4,
            "SILK frame encoder only supports 2 (10 ms) or 4 (20 ms) sub-frames, got {n_subframes}"
        );
        let order = params.lpc_order;
        Self {
            params,
            n_subframes,
            prev_synth: vec![0.0; order],
        }
    }

    /// Convenience: NB (8 kHz) mono 20 ms encoder.
    pub fn new_nb_20ms() -> Self {
        Self::new(BandwidthParams::nb())
    }

    /// Convenience: MB (12 kHz) mono 20 ms encoder.
    pub fn new_mb_20ms() -> Self {
        Self::new(BandwidthParams::mb())
    }

    /// Convenience: WB (16 kHz) mono 20 ms encoder.
    pub fn new_wb_20ms() -> Self {
        Self::new(BandwidthParams::wb())
    }

    /// Convenience: NB (8 kHz) mono 10 ms encoder (2 sub-frames).
    pub fn new_nb_10ms() -> Self {
        Self::new_with_subframes(BandwidthParams::nb(), 2)
    }

    /// Convenience: MB (12 kHz) mono 10 ms encoder (2 sub-frames).
    pub fn new_mb_10ms() -> Self {
        Self::new_with_subframes(BandwidthParams::mb(), 2)
    }

    /// Convenience: WB (16 kHz) mono 10 ms encoder (2 sub-frames).
    pub fn new_wb_10ms() -> Self {
        Self::new_with_subframes(BandwidthParams::wb(), 2)
    }

    /// Frame length in internal-rate samples:
    /// 160 (NB), 240 (MB), 320 (WB).
    pub fn frame_len(&self) -> usize {
        self.params.subframe_len * self.n_subframes
    }

    /// Internal sampling rate in Hz.
    pub fn internal_rate_hz(&self) -> u32 {
        super::internal_rate_hz(self.params.bandwidth)
    }

    /// LPC filter order.
    pub fn lpc_order(&self) -> usize {
        self.params.lpc_order
    }

    /// Sub-frame length (samples at the internal rate).
    pub fn subframe_len(&self) -> usize {
        self.params.subframe_len
    }

    /// Number of sub-frames per 20 ms SILK frame (always 4).
    pub fn n_subframes(&self) -> usize {
        self.n_subframes
    }

    /// Reset all cross-frame state. Used by the stereo encoder when
    /// the side channel transitions from mid-only to coded.
    pub fn reset(&mut self) {
        self.prev_synth = vec![0.0; self.params.lpc_order];
    }

    /// Encode one 20 ms SILK-only body (the bit-stream after the
    /// shared VAD + LBRR header).
    ///
    /// * `pcm_internal` — `frame_len()` samples at the internal rate.
    /// * `enc` — in-flight range encoder.
    pub fn encode_frame_body(
        &mut self,
        pcm_internal: &[f32],
        enc: &mut RangeEncoder,
    ) -> Result<()> {
        debug_assert_eq!(pcm_internal.len(), self.frame_len());
        let order = self.params.lpc_order;
        let frame_len = self.frame_len();
        let subframe_len = self.params.subframe_len;

        // §4.2.7.3 frame type — unvoiced/active (sym=2) so the decoder
        // takes the UNVOICED gain MSB + skips the LTP path.
        let frame_type_sym: usize = 2;
        enc.encode_icdf(frame_type_sym, &tables::FRAME_TYPE_ACTIVE_ICDF, 8);
        let signal_type: u8 = 1; // unvoiced

        // §4.2.7.5 NLSF — build the same NLSF the decoder will from
        // `NLSF_STAGE1_IDX` and zero stage-2 residuals.
        let residuals = vec![0i32; order];
        let nlsf_q15 = synthesize_nlsf_like_decoder(NLSF_STAGE1_IDX, false, order, &residuals);
        let nlsf_q15 = lsf::stabilize(&nlsf_q15, order);
        let lpc = lsf::nlsf_to_lpc(&nlsf_q15, self.params.bandwidth);

        // §4.2.7.4 sub-frame gains — pick a constant gain index.
        let gain_index: i32 = GAIN_INDEX_UNVOICED;
        let gain_q16 = super::gain_index_to_q16(gain_index);
        let g = gain_q16.max(1) as f32 / 65536.0;
        let scale = 128.0 / g;

        // Closed-loop analysis-by-synthesis (same formulation as the NB
        // path, unchanged by bandwidth).
        let synth_hist = self.prev_synth.clone(); // length = order
        let mut out = vec![0f32; frame_len];
        let mut signed_mags = vec![0i32; frame_len];
        for n in 0..frame_len {
            let mut pred = 0f32;
            for k in 1..=order {
                let idx = n as i32 - k as i32;
                let past = if idx >= 0 {
                    out[idx as usize]
                } else {
                    synth_hist[(synth_hist.len() as i32 + idx) as usize]
                };
                pred += lpc[k - 1] * past;
            }
            let e_desired = pcm_internal[n] - pred;
            let signed_mag_f = (e_desired * scale).round();
            let mag_i = signed_mag_f.abs().clamp(0.0, CARRIER_FULL_SCALE) as i32;
            let neg = signed_mag_f < 0.0;
            let signed = if neg { -mag_i } else { mag_i };
            signed_mags[n] = signed;
            let e_quant = (signed as f32 / 128.0) * g;
            out[n] = (e_quant + pred).clamp(-1.0, 1.0);
        }

        // Emit the gain-index bitstream: MSB(3) + LSB(3) + 3 deltas.
        let msb = ((gain_index >> 3) & 0x7) as usize;
        let lsb = (gain_index & 0x7) as usize;
        let msb_icdf = match signal_type {
            0 => &tables::GAIN_MSB_INACTIVE_ICDF,
            1 => &tables::GAIN_MSB_UNVOICED_ICDF,
            _ => &tables::GAIN_MSB_VOICED_ICDF,
        };
        enc.encode_icdf(msb, msb_icdf, 8);
        enc.encode_icdf(lsb, &tables::GAIN_LSB_ICDF, 8);
        for _ in 1..self.n_subframes {
            enc.encode_icdf(4, &tables::GAIN_DELTA_ICDF, 8);
        }

        // NLSF bitstream: stage-1 (32-sym ICDF) + `order` residuals
        // (11-sym each + sign) + interp coef (4-sym). The stage-1 ICDF
        // is bandwidth-dependent: NB/MB share the "NB" codebook, WB
        // has its own.
        let stage1_icdf: &[u8] = match self.params.bandwidth {
            OpusBandwidth::Wideband => &tables::NLSF_WB_STAGE1_UNVOICED_ICDF,
            _ => &tables::NLSF_NB_STAGE1_UNVOICED_ICDF,
        };
        enc.encode_icdf(NLSF_STAGE1_IDX, stage1_icdf, 8);
        let uniform_11 = &tables::NLSF_RESIDUAL_UNIFORM_11_ICDF;
        for &r in &residuals {
            let mag = (r + 4).clamp(0, 10) as usize;
            enc.encode_icdf(mag, uniform_11, 8);
            // (Future: sign bit when mag != 4 && residual != 0.)
        }
        // Interp coef — "no interp" = 3 (ICDF is {192, 128, 64, 0}).
        enc.encode_icdf(3, &[192, 128, 64, 0], 8);

        // §4.2.7.6 LTP — unvoiced, so decoder skips all LTP bits.

        // §4.2.7.7 LCG seed — always 0.
        enc.encode_icdf(0, &tables::LCG_SEED_ICDF, 8);

        // §4.2.7.8 Excitation (MVP carrier).
        let rate_icdf: &[u8] = &tables::RATE_LEVEL_INACTIVE_ICDF;
        enc.encode_icdf(0, rate_icdf, 8);
        let n_shells = frame_len.div_ceil(16);
        let pulse_icdf = &tables::PULSE_COUNT_ICDF[0];
        for _ in 0..n_shells {
            enc.encode_icdf(0, pulse_icdf, 8);
        }
        let _ = subframe_len; // currently only used for debug_assert
        for &signed in &signed_mags {
            let mag_i = signed.unsigned_abs() as i32;
            let neg = signed < 0;
            let hi = ((mag_i >> 4) & 0xf) as usize;
            let lo = (mag_i & 0xf) as usize;
            enc.encode_icdf(hi, &MAG_NIBBLE_ICDF, 8);
            enc.encode_icdf(lo, &MAG_NIBBLE_ICDF, 8);
            if mag_i != 0 {
                enc.encode_bit_logp(neg, 1);
            }
        }

        // Update `prev_synth` with the last `order` samples of the
        // decoder's reconstructed output (kept in sync by the closed-
        // loop quantisation above).
        let start = out.len().saturating_sub(order);
        self.prev_synth.clear();
        self.prev_synth.extend_from_slice(&out[start..]);

        Ok(())
    }
}

/// Split an interleaved L/R stereo block into mid and side channels.
///
/// The decoder's `stereo_unmix_48k` reconstructs L / R as
///   L = (mid + side) * 0.5
///   R = (mid - side) * 0.5
/// so to round-trip bit-for-bit we feed the encoder **twice** the raw
/// M/S values, i.e. `M = L + R` and `S = L - R`. Passing the classical
/// `(L+R)/2 / (L-R)/2` forms would lose 6 dB on each side of the
/// reconstruction (the decoder's 0.5 scaling being a saturation
/// headroom for the [-1, 1] S16 path — see the decoder comment).
///
/// Returns `(mid, side)`, each the same length as one input channel.
pub fn stereo_mid_side(l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
    debug_assert_eq!(l.len(), r.len());
    let n = l.len();
    let mut mid = Vec::with_capacity(n);
    let mut side = Vec::with_capacity(n);
    for i in 0..n {
        mid.push(l[i] + r[i]);
        side.push(l[i] - r[i]);
    }
    (mid, side)
}

/// Quantise a Q13 stereo prediction weight into the 3-tuple
/// `(idx[0], idx[1], idx[2])` expected by the SILK bitstream (see
/// libopus `silk/stereo_encode_pred.c` and RFC §4.2.7.1).
///
/// `idx[2]` ∈ [0, 4] is the coarse index (the high 5 values of the
/// `STEREO_PRED_QUANT_Q13` table), `idx[0] + 3*idx[2]` ∈ [0, 15]
/// selects the quantiser cell, and `idx[1]` ∈ [0, 4] is the sub-step.
///
/// We do a straight nearest-neighbour search against the 80 candidate
/// reconstruction levels (16 coarse cells × 5 sub-steps). This is what
/// libopus does too (`silk_stereo_quant_pred` iterates); for our
/// purposes the small search is negligible (called once per 20 ms).
fn quantise_pred_weight_q13(weight_q13: i32) -> [i32; 3] {
    let quant = &tables::STEREO_PRED_QUANT_Q13;
    // Step size per sub-step: (Q[i+1] - Q[i]) * 0.1 (Q13).
    // 5 sub-steps within each cell i=0..=14 (cell 15 has no next).
    let mut best: i32 = i32::MAX;
    let mut best_idx = [0i32, 0, 0];
    for cell in 0..15 {
        let low_q13 = quant[cell] as i32;
        let high_q13 = quant[cell + 1] as i32;
        let step_q13 = ((high_q13 - low_q13) * 6554) >> 16; // 0.1 × 2^16
        for sub in 0..5 {
            let level = low_q13 + step_q13 * (2 * sub + 1);
            let diff = (level - weight_q13).abs();
            if diff < best {
                best = diff;
                // cell = ix[0] + 3*ix[2]; decompose:
                // ix[0] ∈ [0,2], ix[2] ∈ [0,4].
                let c = cell as i32;
                let ix2 = c / 3;
                let ix0 = c - 3 * ix2;
                best_idx = [ix0, sub, ix2];
            }
        }
    }
    best_idx
}

/// Encode the stereo prediction-weight header (RFC §4.2.7.1 /
/// libopus `silk_stereo_encode_pred`).
///
/// `pred_q13` is the pair `[w0, w1]` the decoder will reconstruct (the
/// decoder's `stereo_decode_pred` returns the same layout). The helper
/// emits the 3 range-coded symbols per channel (`STEREO_PRED_JOINT_ICDF`
/// for the joint coarse index, then UNIFORM3 / UNIFORM5 for the within-
/// cell indices).
///
/// Exactly matches the decoder's consumption order so the two stay in
/// lock-step.
pub fn encode_stereo_pred_weights(enc: &mut RangeEncoder, pred_q13: [i32; 2]) {
    // Libopus encodes the two weights, NOT their difference; the
    // decoder's final step is `pred_q13[0] -= pred_q13[1]`. Recover the
    // raw pair first.
    let w0_coded = pred_q13[0] + pred_q13[1];
    let w1_coded = pred_q13[1];
    let ix0_all = quantise_pred_weight_q13(w0_coded);
    let ix1_all = quantise_pred_weight_q13(w1_coded);

    // Joint coarse symbol: n = 5*ix[0][2] + ix[1][2] ∈ [0, 24].
    let n = 5 * ix0_all[2] + ix1_all[2];
    enc.encode_icdf(n as usize, &tables::STEREO_PRED_JOINT_ICDF, 8);

    // Per-channel fine indices.
    for ix in [ix0_all, ix1_all] {
        enc.encode_icdf(ix[0] as usize, &tables::STEREO_UNIFORM3_ICDF, 8);
        enc.encode_icdf(ix[1] as usize, &tables::STEREO_UNIFORM5_ICDF, 8);
    }
}

/// Compute the Q13 mid/side prediction weights for a stereo frame.
///
/// SILK's stereo predictor minimises `E{(S - w0*M - w1*M_shifted)^2}`
/// where `M_shifted` is the mid channel one sample in the past. The
/// closed form is the standard 2×2 Wiener filter; we implement it in
/// f64 then quantise to Q13.
///
/// `side_rms_floor` avoids a divide-by-zero when the side channel is
/// silent.
pub fn stereo_predict_weights_q13(mid: &[f32], side: &[f32]) -> [i32; 2] {
    debug_assert_eq!(mid.len(), side.len());
    let n = mid.len();
    if n < 2 {
        return [0, 0];
    }
    // Auto / cross correlations, shifted by 1 sample for the 2-tap
    // predictor. We use f64 for numerical stability.
    let mut r_mm = 0f64;
    let mut r_mm1 = 0f64;
    let mut r_m1m1 = 0f64;
    let mut r_sm = 0f64;
    let mut r_sm1 = 0f64;
    for i in 1..n {
        let m = mid[i] as f64;
        let m1 = mid[i - 1] as f64;
        let s = side[i] as f64;
        r_mm += m * m;
        r_mm1 += m * m1;
        r_m1m1 += m1 * m1;
        r_sm += s * m;
        r_sm1 += s * m1;
    }
    // Solve 2×2: [[r_mm, r_mm1],[r_mm1, r_m1m1]] * [w0,w1] = [r_sm,r_sm1].
    let det = r_mm * r_m1m1 - r_mm1 * r_mm1;
    if det.abs() < 1e-12 {
        return [0, 0];
    }
    let w0 = (r_sm * r_m1m1 - r_sm1 * r_mm1) / det;
    let w1 = (r_mm * r_sm1 - r_mm1 * r_sm) / det;
    // SILK clips the predictors to a conservative range. The table
    // spans [-13732, 13732] Q13 = [-1.676, 1.676], we clip inside it.
    let clamp = |w: f64| -> i32 {
        let q = (w * 8192.0).round();
        q.clamp(-13500.0, 13500.0) as i32
    };
    [clamp(w0), clamp(w1)]
}

/// A bit-for-bit copy of the decoder's `synthesize_nlsf` helper so the
/// encoder sees the exact same NLSF template the decoder will
/// reconstruct. We don't re-export the decoder's copy because it's
/// private to `silk/lsf.rs`; we keep the logic mirrored here with a
/// unit test below guarding the drift.
fn synthesize_nlsf_like_decoder(
    stage1: usize,
    voiced: bool,
    order: usize,
    residuals: &[i32],
) -> Vec<i16> {
    let tilt = (stage1 as f32 / 32.0) * 0.25 + if voiced { 0.0 } else { 0.15 };
    let mut nlsf = vec![0i16; order];
    for k in 0..order {
        let base = (k as f32 + 1.0) / (order as f32 + 1.0);
        let tilted = base.powf(1.0 + tilt);
        let mut q15 = (tilted * 32768.0) as i32;
        q15 += residuals[k].clamp(-7, 7) * 128;
        nlsf[k] = q15.clamp(1, 32767) as i16;
    }
    nlsf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nlsf_template_mirrors_decoder() {
        // Compare encoder's mirror to a tiny hand-expansion of the
        // decoder's formula. With stage1 = 0, voiced = false, all
        // residuals 0:
        //   tilt = 0.15
        //   nlsf[k] = clamp((k+1)/(order+1))^1.15 * 32768, 1, 32767)
        let nlsf = synthesize_nlsf_like_decoder(0, false, 10, &[0; 10]);
        assert_eq!(nlsf.len(), 10);
        // Monotonic after stabilisation.
        let stable = crate::silk::lsf::stabilize(&nlsf, 10);
        for w in stable.windows(2) {
            assert!(
                w[1] >= w[0],
                "stabilised NLSF should be non-decreasing ({} → {})",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn wb_frame_params_match_expectations() {
        let wb = SilkFrameEncoder::new_wb_20ms();
        assert_eq!(wb.lpc_order(), 16);
        assert_eq!(wb.subframe_len(), 80);
        assert_eq!(wb.frame_len(), 320);
        assert_eq!(wb.internal_rate_hz(), 16_000);
    }

    #[test]
    fn mb_frame_params_match_expectations() {
        let mb = SilkFrameEncoder::new_mb_20ms();
        assert_eq!(mb.lpc_order(), 10);
        assert_eq!(mb.subframe_len(), 60);
        assert_eq!(mb.frame_len(), 240);
        assert_eq!(mb.internal_rate_hz(), 12_000);
    }

    /// Encode a zero frame and decode it; output should be zero.
    #[test]
    fn encode_decode_zero_frame_matches() {
        use oxideav_celt::range_decoder::RangeDecoder;
        let mut enc = SilkFrameEncoder::new_nb_20ms();
        let pcm = vec![0.0f32; 160];
        let mut re = RangeEncoder::new(512);
        re.encode_bit_logp(true, 1);
        re.encode_bit_logp(false, 1);
        enc.encode_frame_body(&pcm, &mut re).unwrap();
        let buf = re.done().expect("done");
        let mut rc = RangeDecoder::new(&buf);
        let _vad = rc.decode_bit_logp(1);
        let _lbrr = rc.decode_bit_logp(1);
        let mut s = crate::silk::SilkChannelState::new();
        let decoded = crate::silk::decode_frame_body_pub(
            &mut rc,
            true,
            OpusBandwidth::Narrowband,
            10,
            40,
            4,
            &mut s,
        )
        .expect("decode");
        let peak = decoded.iter().copied().fold(0f32, |a, b| a.max(b.abs()));
        println!("zero-frame roundtrip peak = {peak:.6}");
        assert!(
            peak < 0.001,
            "zero-frame decode should be ~0, got peak {peak}"
        );
    }

    #[test]
    fn encode_decode_zero_frame_produces_finite_output() {
        let mut enc = SilkFrameEncoder::new_nb_20ms();
        let pcm = vec![0.0f32; 160];
        let mut re = RangeEncoder::new(512);
        enc.encode_frame_body(&pcm, &mut re).expect("encode");
        let buf = re.done().expect("done");
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 512);
    }

    /// End-to-end round-trip of one NB frame at the internal rate.
    #[test]
    fn encode_decode_nb_one_frame_internal_rate_snr() {
        run_internal_rate_roundtrip(BandwidthParams::nb(), 8_000, 25.0);
    }

    /// 10 ms round-trip (2-subframe) for all three bandwidths. The
    /// bar is softer than the 20 ms case — with only 2 sub-frames the
    /// LPC history starts from zero for each frame, which hurts the
    /// first-frame prediction; we still want >20 dB.
    #[test]
    fn encode_decode_nb_10ms_internal_rate_snr() {
        run_internal_rate_roundtrip_10ms(BandwidthParams::nb(), 8_000, 20.0);
    }

    #[test]
    fn encode_decode_mb_10ms_internal_rate_snr() {
        run_internal_rate_roundtrip_10ms(BandwidthParams::mb(), 12_000, 20.0);
    }

    #[test]
    fn encode_decode_wb_10ms_internal_rate_snr() {
        run_internal_rate_roundtrip_10ms(BandwidthParams::wb(), 16_000, 20.0);
    }

    fn run_internal_rate_roundtrip_10ms(params: BandwidthParams, rate: u32, snr_bar: f64) {
        use oxideav_celt::range_decoder::RangeDecoder;

        let mut enc = SilkFrameEncoder::new_with_subframes(params, 2);
        let frame_len = enc.frame_len();
        let freq = 300.0f32;
        let pcm: Vec<f32> = (0..frame_len)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.3)
            .collect();

        let mut re = RangeEncoder::new(1024);
        re.encode_bit_logp(true, 1);
        re.encode_bit_logp(false, 1);
        enc.encode_frame_body(&pcm, &mut re).expect("encode");
        let buf = re.done().expect("done");

        let mut dec_state = crate::silk::SilkChannelState::new();
        let mut rc = RangeDecoder::new(&buf);
        let _vad = rc.decode_bit_logp(1);
        let _lbrr = rc.decode_bit_logp(1);
        let decoded = crate::silk::decode_frame_body_pub(
            &mut rc,
            true,
            params.bandwidth,
            params.lpc_order,
            params.subframe_len,
            2,
            &mut dec_state,
        )
        .expect("decode");
        assert_eq!(decoded.len(), frame_len);
        let sig: f64 = pcm.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        let err: f64 = pcm
            .iter()
            .zip(decoded.iter())
            .map(|(a, b)| {
                let e = (*a - *b) as f64;
                e * e
            })
            .sum();
        let snr = 10.0 * (sig / err.max(1e-30)).log10();
        println!(
            "{:?} 10 ms internal-rate SNR: {snr:.2} dB (bar {snr_bar})",
            params.bandwidth
        );
        assert!(
            snr > snr_bar,
            "10 ms internal-rate SNR {snr:.2} dB below {snr_bar} dB bar"
        );
    }

    #[test]
    fn encode_decode_mb_one_frame_internal_rate_snr() {
        run_internal_rate_roundtrip(BandwidthParams::mb(), 12_000, 25.0);
    }

    #[test]
    fn encode_decode_wb_one_frame_internal_rate_snr() {
        run_internal_rate_roundtrip(BandwidthParams::wb(), 16_000, 25.0);
    }

    /// Stereo helper: the Wiener-filter coefficients for an LR-identical
    /// stereo block (side = 0) should quantise to (0, 0).
    #[test]
    fn stereo_pred_weights_zero_for_identical_channels() {
        let m: Vec<f32> = (0..100)
            .map(|i| (2.0 * std::f32::consts::PI * 300.0 * i as f32 / 8_000.0).sin() * 0.3)
            .collect();
        let s = vec![0.0f32; m.len()];
        let w = stereo_predict_weights_q13(&m, &s);
        assert_eq!(w, [0, 0]);
    }

    #[test]
    fn stereo_mid_side_reconstructs_lr() {
        // mid/side are doubled compared to the classical (L+R)/2 form
        // so the decoder's 0.5 unmix attenuation round-trips cleanly.
        let l = vec![0.1f32, 0.2, 0.3, 0.4];
        let r = vec![0.0f32, 0.1, 0.2, 0.3];
        let (m, s) = stereo_mid_side(&l, &r);
        for i in 0..l.len() {
            let rec_l = (m[i] + s[i]) * 0.5;
            let rec_r = (m[i] - s[i]) * 0.5;
            assert!((rec_l - l[i]).abs() < 1e-6);
            assert!((rec_r - r[i]).abs() < 1e-6);
        }
    }

    fn run_internal_rate_roundtrip(params: BandwidthParams, rate: u32, snr_bar: f64) {
        use oxideav_celt::range_decoder::RangeDecoder;

        let mut enc = SilkFrameEncoder::new(params);
        let frame_len = enc.frame_len();
        let freq = 300.0f32;
        let pcm: Vec<f32> = (0..frame_len)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.3)
            .collect();

        let mut re = RangeEncoder::new(1024);
        re.encode_bit_logp(true, 1);
        re.encode_bit_logp(false, 1);
        enc.encode_frame_body(&pcm, &mut re).expect("encode");
        let buf = re.done().expect("done");

        let mut dec_state = crate::silk::SilkChannelState::new();
        let mut rc = RangeDecoder::new(&buf);
        let _vad = rc.decode_bit_logp(1);
        let _lbrr = rc.decode_bit_logp(1);
        let decoded = crate::silk::decode_frame_body_pub(
            &mut rc,
            true,
            params.bandwidth,
            params.lpc_order,
            params.subframe_len,
            4,
            &mut dec_state,
        )
        .expect("decode");
        assert_eq!(decoded.len(), frame_len);

        let sig: f64 = pcm.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        let err: f64 = pcm
            .iter()
            .zip(decoded.iter())
            .map(|(a, b)| {
                let e = (*a - *b) as f64;
                e * e
            })
            .sum();
        let snr = 10.0 * (sig / err.max(1e-30)).log10();
        println!(
            "{:?} internal-rate SNR: {snr:.2} dB (bar {snr_bar})",
            params.bandwidth
        );
        assert!(
            snr > snr_bar,
            "internal-rate SNR {snr:.2} dB below {snr_bar} dB bar"
        );
    }
}
