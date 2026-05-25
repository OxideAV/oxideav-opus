//! # oxideav-opus
//!
//! **Status:** orphan-rebuild scaffold (post 2026-05-20 audit).
//!
//! The prior implementation was retired under the workspace clean-room
//! policy. The crate is being re-implemented from scratch against
//! RFC 6716 + RFC 8251 + RFC 7587 + RFC 7845 using only material under
//! `docs/` and black-box validator binaries (`opusdec` / `opusenc`).
//!
//! ## Current surface
//!
//! * Round 1 lands the [`OpusTocByte`] parser per RFC 6716 §3.1
//!   (Table 2, Table 3, Table 4 — the 32-config × stereo-flag ×
//!   frame-count-code triple that prefixes every well-formed Opus
//!   packet).
//! * Round 2 lands the [`OpusPacket`] §3.2 frame-packing parser for
//!   all four `c` codes (code 0 single frame; code 1 two equal-size;
//!   code 2 two unequal with §3.2.1 length encoding; code 3 signalled
//!   frame count with optional VBR per-frame lengths and Opus
//!   padding). The returned slices borrow from the input packet, so
//!   the SILK / CELT decoders can be hooked up against them in a
//!   subsequent round without copying.
//! * Round 3 lands the [`RangeDecoder`] RFC 6716 §4.1 range coder —
//!   the shared entropy primitive consumed by both the SILK and CELT
//!   layers. The sibling `oxideav-celt` crate owns an independent
//!   clean-room copy of the same primitive; both crates carry their
//!   own copy until a shared low-level primitives crate exists.
//! * Round 4 lands the [`SilkFrameHeader`] decoder for RFC 6716
//!   §4.2.7.1 (stereo prediction weights), §4.2.7.2 (mid-only flag),
//!   §4.2.7.3 (frame type / quantization-offset type), and §4.2.7.5.1
//!   (normalized LSF stage-1 codebook index `I1`). These are the four
//!   structural decisions that gate every subsequent SILK stage
//!   (gains, LSF stage-2, LTP, excitation). Implemented as
//!   inverse-CDF reads against the range decoder, with the PDFs
//!   transcribed from Tables 6, 8, 9, and 14.
//! * Round 5 lands the [`SubframeGains`] decoder for RFC 6716
//!   §4.2.7.4 — per-subframe quantization gains for the two- or
//!   four-subframe SILK frame. The first subframe is **independently**
//!   coded (Table 11 signal-type-conditioned MSB PDF + Table 12
//!   uniform LSB PDF + the `max(gain_index, previous_log_gain - 16)`
//!   clamp from §4.2.7.4) when the §4.2.7.4 enumeration triggers;
//!   otherwise it's coded as a 41-symbol delta (Table 13) against
//!   the previous coded subframe gain via the `clamp(0,
//!   max(2*delta - 16, prev + delta - 4), 63)` rule. All subsequent
//!   subframes in the frame use the delta path. Output is integer
//!   `log_gain` in `0..=63`; the §4.2.7.4 tail-end `gain_Q16`
//!   conversion (`silk_log2lin`) is part of the excitation stage
//!   and not wired up yet.
//!
//! * Round 6 lands the [`LsfStage2`] decoder for RFC 6716 §4.2.7.5.2 —
//!   the per-coefficient stage-2 residual indices `I2[k] ∈ [-10, 10]`
//!   plus the backwards-prediction-undone `res_Q10[k]`. Tables 15
//!   (NB/MB) and 16 (WB) are the eight signal-shape codebooks; Tables
//!   17 (NB/MB) and 18 (WB) map `(I1, k)` → codebook letter; Table 19
//!   is the 7-cell extension PDF for the `|I2| == 4` saturation case;
//!   Table 20 holds the four prediction-weight lists (A/B for NB/MB,
//!   C/D for WB); Tables 21 (NB/MB) and 22 (WB) map `(I1, k)` →
//!   weight-list. Output stops at `res_Q10[]`.
//!
//! * Round 7 lands the [`NlsfReconstructed`] decoder for RFC 6716
//!   §4.2.7.5.3 — the stage-1 codebook lookup (Tables 23 NB/MB and
//!   24 WB carrying `cb1_Q8[]` for each `I1 ∈ 0..32`), the
//!   low-complexity Inverse Harmonic Mean Weighting (IHMW) derivation
//!   of `w_Q9[k]` from `cb1_Q8[]` via
//!   `w2_Q18[k] = (1024/(cb1_Q8[k]-cb1_Q8[k-1]) + 1024/(cb1_Q8[k+1]-cb1_Q8[k])) << 16`
//!   reduced through the spec's square-root approximation, and the
//!   final reconstructed
//!   `NLSF_Q15[k] = clamp(0, (cb1_Q8[k]<<7) + (res_Q10[k]<<14)/w_Q9[k], 32767)`.
//!   The §4.2.7.5.5 interpolation step that consumes the stabilized
//!   `NLSF_Q15[]` is deferred to a later round.
//!
//! * Round 8 lands the [`NlsfStabilized`] decoder for RFC 6716
//!   §4.2.7.5.4 — the normalized-LSF stabilization that enforces the
//!   Table 25 minimum spacing between consecutive `NLSF_Q15[]` entries.
//!   Up to 20 distortion-minimizing re-centring passes run first
//!   (finding the smallest-spacing pair, then the `min_center` /
//!   `max_center` / `center_freq` re-centring, with special handling
//!   for the implicit `NLSF_Q15[-1] = 0` and `NLSF_Q15[d_LPC] = 32768`
//!   edges), falling back after the 20th pass to a guaranteed sort +
//!   forward-`max` + backward-`min` sweep. The fallback's forward sweep
//!   uses 16-bit saturating addition per the RFC 8251 §7 erratum.
//!
//! * Round 9 lands the [`LsfInterpolated`] decoder for RFC 6716
//!   §4.2.7.5.5 — the normalized-LSF interpolation that produces the
//!   first-half coefficients of a 20 ms SILK frame. A Q2 factor
//!   `w_Q2 ∈ 0..=4` is decoded from the Table 26 PDF and
//!   `n1_Q15[k] = n0_Q15[k] + (w_Q2*(n2_Q15[k] - n0_Q15[k]) >> 2)` blends
//!   the prior coded frame's NLSF vector (`n0`) with the current
//!   stabilized one (`n2`). After a decoder reset or an uncoded regular
//!   side-channel SILK frame the factor is still decoded (to keep the
//!   range coder in sync) but discarded and `4` is used instead; for a
//!   10 ms SILK frame no factor is present at all.
//!
//! * Round 10 lands the [`LpcQ17`] core converter for RFC 6716
//!   §4.2.7.5.6 — the NLSF → LPC reconstruction (`silk_NLSF2A`). The
//!   Table 28 Q12 cosine table with linear interpolation produces the
//!   re-ordered Q17 cosine vector `c_Q17[]` per Table 27, the
//!   `silk_NLSF2A_find_poly` P/Q recurrence runs in i64 to absorb the
//!   "up to 48 bits of intermediate precision" the spec calls out, and
//!   the last-row sum/difference assembly produces the 32-bit
//!   `a32_Q17[]`.
//!
//! * Round 11 lands the §4.2.7.5.7 range-limiting bandwidth expansion
//!   ([`LpcQ17::range_limited`]) — up to 10 rounds of `silk_bwexpander_32`
//!   chirping (`maxabs_Q12 = min((maxabs_Q17 + 16) >> 5, 163838)`, chirp
//!   factor `sc_Q16[0] = 65470 - ((maxabs_Q12 - 32767) << 14) /
//!   ((maxabs_Q12 * (k+1)) >> 2)`) that shrink the raw `a32_Q17[]` until
//!   it fits a signed 16-bit Q12 value, followed by the documented
//!   post-loop Q12 saturation `clamp(-32768, (a + 16) >> 5, 32767) << 5`.
//!   The result is held in the Q17 domain for the §4.2.7.5.8
//!   prediction-gain limiting that follows.
//!
//! * Round 12 lands the §4.2.7.5.8 prediction-gain limiting
//!   ([`LpcQ17::prediction_gain_limited`] → [`LpcQ12`]) — the
//!   `silk_LPC_inverse_pred_gain_QA()` stability test (DC-response check
//!   plus the fixed-point Levinson recurrence on the Q24-widened Q12
//!   coefficients, with the `abs(a32_Q24[k][k]) > 16773022` and
//!   `inv_gain_Q30[k] < 107374` instability bounds) driving up to 16
//!   rounds of bandwidth expansion with `sc_Q16[0] = 65536 - (2<<i)`.
//!   The result is the final stable Q12 filter `a_Q12[k]` consumed by the
//!   §4.2.7.9.2 LPC synthesis.
//!
//! * Round 13 lands the §4.2.7.6 Long-Term Prediction parameters
//!   ([`LtpParameters`]) — the primary pitch lag (§4.2.7.6.1; absolute via
//!   Table 29 high part + Table 30 bandwidth-conditioned low part, or
//!   relative via the Table 31 delta with a zero-delta fallback to
//!   absolute), the pitch-contour VQ index (Table 32 PDF; Tables 33–36
//!   codebooks) that refines the primary lag into per-subframe pitch lags
//!   clamped to `[lag_min, lag_max]`, the §4.2.7.6.2 periodicity index
//!   (Table 37) and per-subframe 5-tap Q7 LTP filter taps (Table 38 PDFs;
//!   Tables 39–41 codebooks), and the §4.2.7.6.3 optional Q14 LTP scaling
//!   factor (Table 42 → `{15565, 12288, 8192}`; default `15565` when not
//!   coded). Non-voiced frames consume no LTP bits.
//!
//! * Round 14 lands the §4.2.7.7 LCG seed ([`decode_lcg_seed`]) and the
//!   §4.2.7.8 SILK excitation decoder ([`Excitation`] / [`ExcitationConfig`]).
//!   The excitation is decoded in six substeps: §4.2.7.8.1 rate level
//!   (Table 45 PDFs, one symbol per SILK frame), §4.2.7.8.2 per-shell-block
//!   pulse count (Table 46 PDFs at one of 11 rate levels; the "extra LSB"
//!   value 17 chains into rate level 9, then 10), §4.2.7.8.3 recursive
//!   pulse-location partition (16 → 8 → 4 → 2 → 1; Tables 47–50 select
//!   the split PDF by partition size + remaining pulse count),
//!   §4.2.7.8.4 per-coefficient LSB decoding (Table 51), §4.2.7.8.5
//!   sign decoding (Table 52, picked by signal type × quantization
//!   offset type × pulse count bin with 6+ saturating), and §4.2.7.8.6
//!   reconstruction with the LCG `seed' = 196314165*seed + 907633515
//!   mod 2^32` plus the Table 53 Q23 quantization offset. The result is
//!   the final Q23 excitation `e_Q23[]` consumed by the §4.2.7.9 LTP
//!   and LPC synthesis filters.
//!
//! * Round 15 lands the §4.2.7.9.2 SILK LPC synthesis filter
//!   ([`lpc_synthesis_subframe`] / [`lpc_synthesis_frame`] /
//!   [`LpcSynthState`]). The short-term predictor combines the §4.2.7.4
//!   Q16 gain, the §4.2.7.9.1 residual `res[i]`, and the §4.2.7.5.8 Q12
//!   stabilised filter `a_Q12[k]` into the unclamped `lpc[i]` and its
//!   clamped output `out[i] = clamp(-1.0, lpc[i], 1.0)`; the per-subframe
//!   `d_LPC` unclamped history is carried across subframes via the
//!   stateful [`LpcSynthState`] (cleared to zero on a decoder reset).
//!
//! * Round 16 lands the §4.2.7.9.1 SILK LTP synthesis filter
//!   ([`ltp_synthesis_subframe`] / [`ltp_synth_commit_subframe`] /
//!   [`LtpSynthState`]). Unvoiced subframes produce `res[i] = e_Q23[i] /
//!   2^23` (a normalised excitation copy). Voiced subframes go through the
//!   §4.2.7.6 5-tap Q7 LTP convolution `res[i] = e_Q23[i]/2^23 + Σ
//!   res[i - pitch_lag + 2 - k] * b_Q7[k]/128`, with the prior-subframe
//!   `out[]` history rewhitened via `4*LTP_scale_Q14/gain_Q16 *
//!   clamp(out[i] - Σ out[i-k-1] * a_Q12[k]/4096, -1, 1)` (region A) and
//!   the prior-subframe unclamped `lpc[]` rewhitened via `65536/gain_Q16 *
//!   (lpc[i] - Σ lpc[i-k-1] * a_Q12[k]/4096)` (region B). `out_end` and
//!   the effective `LTP_scale_Q14` (= 16384 fresh-LPC override) follow the
//!   §4.2.7.9.1 third/fourth-subframe LSF-interpolation-split branch. The
//!   stateful [`LtpSynthState`] carries 306 samples of out[] and 256
//!   samples of lpc[] history (the spec-stated WB worst cases) across
//!   subframes and across SILK frame boundaries, cleared to zero on a
//!   decoder reset per §4.5.2.
//!
//! * Round 17 lands the §4.2.8 SILK stereo unmixing
//!   ([`stereo_ms_to_lr`] / [`StereoUnmixState`] / [`StereoWeightsQ13`] /
//!   [`StereoFrame`]) — the `silk_stereo_MS_to_LR` conversion that turns
//!   the decoded mid/side `out[]` signals into left/right. The side
//!   channel is predicted from a low-passed mid term
//!   (`p0 = (mid[i-2] + 2*mid[i-1] + mid[i]) / 4`) and the unfiltered
//!   one-sample-delayed mid (`mid[i-1]`) via the §4.2.7.1 Q13 weights:
//!   `left[i] = clamp(-1, (1+w1)*mid[i-1] + side[i-1] + w0*p0, 1)` and
//!   `right[i] = clamp(-1, (1-w1)*mid[i-1] - side[i-1] - w0*p0, 1)`. The
//!   first `n1` samples (64 NB / 96 MB / 128 WB) interpolate the weights
//!   from the previous frame's `(prev_w0_Q13, prev_w1_Q13)` to the
//!   current frame's; the remainder use the current weights. An uncoded
//!   side channel (§4.2.7.2) is treated as all-zero. The two trailing
//!   mid samples, one trailing side sample, and previous-frame weights
//!   carry across the frame boundary via [`StereoUnmixState`], cleared
//!   to zero on a decoder reset per §4.2.8.
//!
//! The CELT layer is not yet wired up; the [`Decoder`] / [`Encoder`]
//! entry points still return [`Error::NotImplemented`].

#![warn(missing_debug_implementations)]

use oxideav_core::RuntimeContext;

/// Crate-local error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The caller passed a zero-length packet. RFC 6716 §3.1 requires
    /// every well-formed Opus packet to contain at least one byte (R1).
    EmptyPacket,
    /// The packet violates one of the §3.2 frame-packing
    /// requirements (R2..R7). Examples: a code-1 packet with an odd
    /// payload length; a code-2 packet whose declared first-frame
    /// length runs off the end of the buffer; a code-3 packet with
    /// `M = 0` or whose CBR per-frame size is not an integer divisor
    /// of the remaining payload.
    MalformedPacket,
    /// The clean-room rebuild has not yet wired up a working
    /// SILK / CELT pipeline; the higher-level decode / encode paths
    /// return this until that work lands.
    NotImplemented,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::EmptyPacket => write!(
                f,
                "oxideav-opus: packet is empty; RFC 6716 §3.1 R1 requires at least one byte"
            ),
            Error::MalformedPacket => write!(
                f,
                "oxideav-opus: packet violates an RFC 6716 §3.2 frame-packing requirement"
            ),
            Error::NotImplemented => write!(
                f,
                "oxideav-opus: orphan-rebuild scaffold — SILK/CELT pipeline not wired up yet"
            ),
        }
    }
}

impl std::error::Error for Error {}

pub mod frames;
pub mod range_decoder;
pub mod silk_excitation;
pub mod silk_frame;
pub mod silk_gains;
pub mod silk_lcg_seed;
pub mod silk_lpc_synth;
pub mod silk_lsf_interp;
pub mod silk_lsf_recon;
pub mod silk_lsf_stabilize;
pub mod silk_lsf_stage2;
pub mod silk_lsf_to_lpc;
pub mod silk_ltp;
pub mod silk_ltp_synth;
pub mod silk_stereo;
pub mod toc;

pub use frames::{OpusPacket, MAX_FRAMES_PER_PACKET, MAX_FRAME_BYTES};
pub use range_decoder::RangeDecoder;
pub use silk_excitation::{
    quantization_offset_q23, shell_block_count, Excitation, ExcitationConfig, SilkFrameSize,
    MAX_EXCITATION_SAMPLES, MAX_SHELL_BLOCKS, SHELL_BLOCK_SAMPLES,
};
pub use silk_frame::{
    FrameKind, QuantizationOffsetType, SignalType, SilkFrameHeader, SilkFrameHeaderConfig,
    StereoPredictionWeights,
};
pub use silk_gains::{SubframeGain, SubframeGains, SubframeGainsConfig, SILK_MAX_SUBFRAMES};
pub use silk_lcg_seed::decode_lcg_seed;
pub use silk_lpc_synth::{
    lpc_synthesis_frame, lpc_synthesis_subframe, subframe_samples, LpcSynthState,
    LPC_SYNTH_MAX_ORDER, LPC_SYNTH_MAX_SUBFRAME_SAMPLES,
};
pub use silk_lsf_interp::{LsfInterpContext, LsfInterpolated};
pub use silk_lsf_recon::{cb1_q8, NlsfReconstructed};
pub use silk_lsf_stabilize::NlsfStabilized;
pub use silk_lsf_stage2::{
    LsfStage2, D_LPC_MAX, D_LPC_NB_MB, D_LPC_WB, QSTEP_NB_MB_Q16, QSTEP_WB_Q16,
};
pub use silk_lsf_to_lpc::{nlsf_to_c_q17, ordering, LpcQ12, LpcQ17};
pub use silk_ltp::{
    LagCoding, LtpConfig, LtpParameters, LTP_FILTER_TAPS, LTP_MAX_SUBFRAMES,
    LTP_SCALING_DEFAULT_Q14,
};
pub use silk_ltp_synth::{
    ltp_synth_commit_subframe, ltp_synthesis_subframe, LtpSynthState, LtpSynthSubframe,
    LTP_LPC_HISTORY_MAX, LTP_MAX_PITCH_LAG, LTP_OUT_HISTORY_MAX, LTP_SCALE_FRESH_Q14,
};
pub use silk_stereo::{
    interp_phase_samples, stereo_ms_to_lr, StereoFrame, StereoUnmixState, StereoWeightsQ13,
};
pub use toc::{Bandwidth, ChannelMapping, FrameCountCode, Mode, OpusTocByte};

/// No-op codec registration — the orphan-rebuild scaffold registers
/// nothing into the runtime context until decode / encode paths are
/// wired up.
pub fn register(_ctx: &mut RuntimeContext) {}

oxideav_core::register!("opus", register);
