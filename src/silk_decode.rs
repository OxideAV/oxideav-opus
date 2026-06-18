//! In-order SILK frame decode — RFC 6716 §4.2.6 / §4.2.7 (Table 5).
//!
//! This module composes the individually-tested per-stage SILK decoders
//! (`silk_frame`, `silk_gains`, `silk_lsf_*`, `silk_ltp`,
//! `silk_lcg_seed`, `silk_excitation`) into a single
//! [`decode_silk_frame`] call that reads one regular SILK frame's
//! bitstream in the **exact Table-5 symbol order**:
//!
//! 1. §4.2.7.1 stereo prediction weights (mid channel of a stereo Opus
//!    frame only),
//! 2. §4.2.7.2 mid-only flag (conditional),
//! 3. §4.2.7.3 frame type,
//! 4. §4.2.7.4 subframe gains,
//! 5. §4.2.7.5.1 normalized LSF stage-1 index,
//! 6. §4.2.7.5.2 normalized LSF stage-2 residual,
//! 7. §4.2.7.5.5 LSF interpolation weight (20 ms frame only),
//! 8. §4.2.7.6 LTP lags + gains + scaling (voiced frame only),
//! 9. §4.2.7.7 LCG seed,
//! 10. §4.2.7.8 quantized excitation.
//!
//! The critical correctness property is that the §4.2.7.4 gains are read
//! *between* the frame type (step 3) and the LSF stage-1 index (step 5),
//! exactly as Table 5 places them. The convenience
//! [`crate::silk_frame::SilkFrameHeader::decode`] reads steps 1–3 and 5
//! back-to-back (no gains in between) and is therefore unsuitable for a
//! full-frame decode; this module uses the composable
//! [`crate::silk_frame::SilkFrameHeader::decode_pre_gains`] (steps 1–3)
//! and [`crate::silk_frame::SilkFrameHeader::decode_lsf_stage1`] (step 5)
//! entries with the gains read in between.
//!
//! After the bitstream is consumed, the module runs the *non-bitstream*
//! §4.2.7.5.3–§4.2.7.5.8 LSF → LPC reconstruction chain (codebook lookup
//! → stabilization → interpolation → NLSF→LPC → bandwidth expansion →
//! prediction-gain limiting) so the returned [`SilkFrameDecoded`] carries
//! the final stable Q12 LPC coefficients ready for the §4.2.7.9 synthesis
//! filters, alongside the LTP parameters and the Q23 excitation.
//!
//! ## Scope of this round
//!
//! This module produces a fully decoded *parameter set + excitation* for
//! one regular SILK frame: every symbol of the frame's bitstream is
//! consumed in Table-5 order, and the LSF → LPC transform is run. The
//! §4.2.7.9 LTP / LPC synthesis filters (which turn the excitation +
//! filters into time-domain samples) and the §4.2.9 resample to 48 kHz
//! are composed in a follow-up; [`SilkFrameDecoded`] is the stable
//! hand-off point between the bitstream-consuming front half and the
//! signal-reconstructing back half.
//!
//! The current entry decodes a **mono** regular SILK frame (no stereo
//! prediction weights / mid-only flag). The stereo mid/side interleave
//! (§4.2.6) reuses the same per-frame decode with the §4.2.7.1 / §4.2.7.2
//! symbols enabled and is wired once the stereo unmixing back half lands.

use crate::range_decoder::RangeDecoder;
use crate::silk_excitation::{Excitation, ExcitationConfig, SilkFrameSize};
use crate::silk_frame::{
    FrameKind, QuantizationOffsetType, SignalType, SilkFrameHeader, SilkFrameHeaderConfig,
};
use crate::silk_gains::{SubframeGains, SubframeGainsConfig};
use crate::silk_lcg_seed::decode_lcg_seed;
use crate::silk_lsf_interp::{LsfInterpContext, LsfInterpolated};
use crate::silk_lsf_recon::NlsfReconstructed;
use crate::silk_lsf_stabilize::NlsfStabilized;
use crate::silk_lsf_stage2::LsfStage2;
use crate::silk_lsf_to_lpc::LpcQ12;
use crate::silk_ltp::{LagCoding, LtpConfig, LtpParameters};
use crate::toc::Bandwidth;
use crate::Error;

/// Configuration for one regular SILK frame decode, supplying the
/// per-frame conditions that the §4.2 packet organisation determines
/// outside the SILK frame itself (the §4.2.4 VAD flag, the §4.2.7.4
/// independent-gain enumeration, the §4.2.7.6.1 relative-lag base, and
/// the §4.2.7.6.3 LTP-scaling-present enumeration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilkFrameConfig {
    /// Internal SILK bandwidth (NB / MB / WB). SWB / FB are rejected.
    pub bandwidth: Bandwidth,
    /// SILK frame duration: 10 ms (2 subframes) or 20 ms (4 subframes).
    pub frame_size: SilkFrameSize,
    /// §4.2.4 voice-activity flag for this frame's time interval. Selects
    /// the §4.2.7.3 frame-type PDF (active vs inactive).
    pub voice_active: bool,
    /// §4.2.7.4: whether the first subframe gain is coded independently
    /// (first SILK frame of its type for this channel in the Opus frame,
    /// or the previous SILK frame of the same type was not coded).
    pub first_subframe_independent: bool,
    /// §4.2.7.4 clamp base: the previous SILK frame's last subframe
    /// `log_gain` for this channel, or `None` after a reset / uncoded
    /// previous frame (the clamp is then skipped).
    pub previous_log_gain: Option<u8>,
    /// §4.2.7.6.1: how the primary pitch lag is coded. `None` defaults to
    /// absolute coding; `Some(prev)` enables relative coding against the
    /// previous frame's primary lag.
    pub previous_primary_lag: Option<i32>,
    /// §4.2.7.6.3: whether the LTP scaling factor is present in the
    /// bitstream (first time interval of the Opus frame for its type, or
    /// an LBRR frame whose prior LBRR frame is not coded).
    pub ltp_scaling_present: bool,
    /// §4.2.7.5.5 interpolation context for a 20 ms frame: `true` after a
    /// decoder reset / uncoded previous frame (the decoded factor is
    /// discarded and `4` is used). Ignored for a 10 ms frame.
    pub lsf_interp_after_reset: bool,
    /// §4.2.7.5.5: the previous coded frame's stabilized NLSF vector
    /// (`n0_Q15[]`), used as the interpolation base for a 20 ms frame.
    /// `None` after a reset (the `4` factor is used so `n1 == n2`).
    pub previous_nlsf_q15: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]>,
    /// Length of the populated prefix of [`Self::previous_nlsf_q15`]
    /// (the `d_LPC` of the previous frame: 10 for NB/MB, 16 for WB).
    pub previous_nlsf_len: usize,
}

/// One fully decoded regular SILK frame: every Table-5 bitstream symbol
/// consumed and the §4.2.7.5 LSF → LPC chain run.
#[derive(Debug, Clone)]
pub struct SilkFrameDecoded {
    /// §4.2.7.3 signal type.
    pub signal_type: SignalType,
    /// §4.2.7.3 quantization-offset type.
    pub qoff_type: QuantizationOffsetType,
    /// §4.2.7.4 per-subframe quantization gains (`log_gain ∈ 0..=63`).
    pub gains: SubframeGains,
    /// §4.2.7.5.1 normalized LSF stage-1 index `I1 ∈ 0..32`.
    pub lsf_stage1: u8,
    /// The §4.2.7.5.4 stabilized normalized-LSF vector for the *current*
    /// frame (`n2_Q15[]`), carried forward as the next frame's §4.2.7.5.5
    /// interpolation base. Only `0..d_lpc` entries are valid.
    pub nlsf_q15: [i16; crate::silk_lsf_stage2::D_LPC_MAX],
    /// `d_LPC` (length of [`Self::nlsf_q15`]): 10 for NB/MB, 16 for WB.
    pub d_lpc: usize,
    /// §4.2.7.5.5 interpolation factor `w_Q2 ∈ 0..=4` for a 20 ms frame;
    /// `None` for a 10 ms frame (no first-half split).
    pub lsf_interp_q2: Option<u8>,
    /// The final stable §4.2.7.5.8 Q12 LPC filter for the *second half*
    /// of the frame (derived from the stabilized current-frame NLSF).
    pub lpc_second_half: LpcQ12,
    /// The final stable §4.2.7.5.8 Q12 LPC filter for the *first half* of
    /// a 20 ms frame (derived from the interpolated `n1_Q15[]`); `None`
    /// for a 10 ms frame, which uses [`Self::lpc_second_half`] throughout.
    pub lpc_first_half: Option<LpcQ12>,
    /// §4.2.7.6 LTP parameters (voiced frames only; empty otherwise).
    pub ltp: LtpParameters,
    /// §4.2.7.7 LCG seed `0..=3`.
    pub lcg_seed: u8,
    /// §4.2.7.8 quantized excitation `e_Q23[]`.
    pub excitation: Excitation,
}

/// Decode one regular **mono** SILK frame from `rd`, reading every
/// Table-5 bitstream symbol in order and running the §4.2.7.5 LSF → LPC
/// reconstruction.
///
/// Returns [`Error::MalformedPacket`] if any stage rejects (an
/// out-of-range symbol, an SWB / FB bandwidth, a mismatched length, or a
/// latched range-coder error).
pub fn decode_silk_frame(
    rd: &mut RangeDecoder<'_>,
    cfg: SilkFrameConfig,
) -> Result<SilkFrameDecoded, Error> {
    let num_subframes: u8 = match cfg.frame_size {
        SilkFrameSize::TenMs => 2,
        SilkFrameSize::TwentyMs => 4,
    };

    // ---- Steps 1-3: §4.2.7.1 / §4.2.7.2 / §4.2.7.3 (mono: no stereo
    // weights, no mid-only flag). ----
    let header_cfg = SilkFrameHeaderConfig {
        stereo_mid_channel: false,
        stereo: false,
        has_mid_only_flag: false,
        kind: if cfg.voice_active {
            FrameKind::RegularActive
        } else {
            FrameKind::RegularInactive
        },
        bandwidth: cfg.bandwidth,
    };
    let pre = SilkFrameHeader::decode_pre_gains(rd, header_cfg)?;

    // ---- Step 4: §4.2.7.4 subframe gains. ----
    let gains = SubframeGains::decode(
        rd,
        SubframeGainsConfig {
            signal_type: pre.signal_type,
            num_subframes,
            first_subframe_is_independent: cfg.first_subframe_independent,
            previous_log_gain: cfg.previous_log_gain,
        },
    )?;

    // ---- Step 5: §4.2.7.5.1 LSF stage-1 index. ----
    let lsf_stage1 = SilkFrameHeader::decode_lsf_stage1(rd, cfg.bandwidth, pre.signal_type)?;

    // ---- Step 6: §4.2.7.5.2 LSF stage-2 residual. ----
    let stage2 = LsfStage2::decode(rd, cfg.bandwidth, lsf_stage1)?;

    // §4.2.7.5.3 / §4.2.7.5.4 (non-bitstream): reconstruct + stabilize
    // the current-frame normalized LSF vector.
    let recon = NlsfReconstructed::from_stage1_and_stage2(cfg.bandwidth, lsf_stage1, &stage2)?;
    let stabilized = NlsfStabilized::from_reconstructed(cfg.bandwidth, &recon)?;
    let d_lpc = stabilized.len();
    let mut nlsf_q15 = [0i16; crate::silk_lsf_stage2::D_LPC_MAX];
    nlsf_q15[..d_lpc].copy_from_slice(stabilized.nlsf_q15());

    // ---- Step 7: §4.2.7.5.5 LSF interpolation weight (20 ms only). ----
    let interp_context = match cfg.frame_size {
        SilkFrameSize::TenMs => LsfInterpContext::TenMs,
        SilkFrameSize::TwentyMs => {
            if cfg.lsf_interp_after_reset || cfg.previous_nlsf_q15.is_none() {
                LsfInterpContext::TwentyMsAfterResetOrUncoded
            } else {
                LsfInterpContext::TwentyMs
            }
        }
    };
    let n0_slice: Option<&[i16]> = match (&cfg.previous_nlsf_q15, cfg.frame_size) {
        (Some(prev), SilkFrameSize::TwentyMs) if cfg.previous_nlsf_len == d_lpc => {
            Some(&prev[..d_lpc])
        }
        _ => None,
    };
    let interp = LsfInterpolated::decode(rd, &stabilized, n0_slice, interp_context)?;
    let lsf_interp_q2 = interp.w_q2();

    // §4.2.7.5.6-§4.2.7.5.8 (non-bitstream): NLSF → stable Q12 LPC.
    let lpc_second_half = nlsf_to_stable_lpc(cfg.bandwidth, &nlsf_q15[..d_lpc])?;
    let lpc_first_half = match interp.n1_q15() {
        Some(n1) => Some(nlsf_to_stable_lpc(cfg.bandwidth, n1)?),
        None => None,
    };

    // ---- Step 8: §4.2.7.6 LTP lags + gains + scaling (voiced only). ----
    let lag_coding = match cfg.previous_primary_lag {
        Some(previous_lag) => LagCoding::Relative { previous_lag },
        None => LagCoding::Absolute,
    };
    let ltp = LtpParameters::decode(
        rd,
        LtpConfig {
            bandwidth: cfg.bandwidth,
            signal_type: pre.signal_type,
            num_subframes,
            lag_coding,
            ltp_scaling_present: cfg.ltp_scaling_present,
        },
    )?;

    // ---- Step 9: §4.2.7.7 LCG seed. ----
    let lcg_seed = decode_lcg_seed(rd);

    // ---- Step 10: §4.2.7.8 quantized excitation. ----
    let excitation = Excitation::decode(
        rd,
        ExcitationConfig {
            bandwidth: cfg.bandwidth,
            frame_size: cfg.frame_size,
            signal_type: pre.signal_type,
            qoff_type: pre.qoff_type,
            lcg_seed,
        },
    )?;

    if rd.has_error() {
        return Err(Error::MalformedPacket);
    }

    Ok(SilkFrameDecoded {
        signal_type: pre.signal_type,
        qoff_type: pre.qoff_type,
        gains,
        lsf_stage1,
        nlsf_q15,
        d_lpc,
        lsf_interp_q2,
        lpc_second_half,
        lpc_first_half,
        ltp,
        lcg_seed,
        excitation,
    })
}

/// Run the §4.2.7.5.6–§4.2.7.5.8 NLSF → stable Q12 LPC chain for one
/// normalized-LSF vector: NLSF→LPC (`silk_NLSF2A`), the §4.2.7.5.7
/// range-limiting bandwidth expansion, and the §4.2.7.5.8
/// prediction-gain limiting.
fn nlsf_to_stable_lpc(bandwidth: Bandwidth, nlsf_q15: &[i16]) -> Result<LpcQ12, Error> {
    let lpc_q17 = crate::silk_lsf_to_lpc::LpcQ17::from_nlsf(bandwidth, nlsf_q15)?;
    let range_limited = lpc_q17.range_limited();
    Ok(range_limited.prediction_gain_limited())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SILK frame config for a fresh (post-reset) regular mono frame.
    fn fresh_cfg(bandwidth: Bandwidth, frame_size: SilkFrameSize, voiced: bool) -> SilkFrameConfig {
        SilkFrameConfig {
            bandwidth,
            frame_size,
            voice_active: voiced,
            first_subframe_independent: true,
            previous_log_gain: None,
            previous_primary_lag: None,
            ltp_scaling_present: true,
            lsf_interp_after_reset: true,
            previous_nlsf_q15: None,
            previous_nlsf_len: 0,
        }
    }

    /// Decoding from an all-zero buffer is total (never panics) and
    /// either succeeds or reports MalformedPacket. The all-zero buffer is
    /// a valid range-coder input; this pins that every stage threads
    /// through without an index-out-of-bounds or arithmetic panic.
    #[test]
    fn decode_from_zero_buffer_is_total() {
        for &bw in &[Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            for &fs in &[SilkFrameSize::TenMs, SilkFrameSize::TwentyMs] {
                for voiced in [false, true] {
                    let buf = [0u8; 64];
                    let mut rd = RangeDecoder::new(&buf);
                    let cfg = fresh_cfg(bw, fs, voiced);
                    let _ = decode_silk_frame(&mut rd, cfg);
                }
            }
        }
    }

    /// Decoding consumes bits in Table-5 order: `tell()` after a
    /// successful decode is strictly greater than at the start (the frame
    /// always has at least the frame-type + gains + LSF symbols), and the
    /// decoded `d_lpc` matches the bandwidth.
    #[test]
    fn decode_consumes_bits_and_sets_d_lpc() {
        // A non-trivial buffer so the range coder produces varied
        // symbols. The exact decoded values are not asserted (no
        // bit-exact fixture at the codec level yet); the structural
        // invariants are.
        let buf: Vec<u8> = (0..96u16)
            .map(|i| (i.wrapping_mul(37) & 0xff) as u8)
            .collect();
        for (&bw, expected_d) in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb]
            .iter()
            .zip([10usize, 10, 16])
        {
            let mut rd = RangeDecoder::new(&buf);
            let start = rd.tell();
            let cfg = fresh_cfg(bw, SilkFrameSize::TwentyMs, false);
            if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
                assert!(rd.tell() > start, "bw={bw:?} must consume bits");
                assert_eq!(decoded.d_lpc, expected_d, "bw={bw:?}");
                // A 20 ms frame carries an interpolation factor and a
                // first-half LPC filter.
                assert!(decoded.lsf_interp_q2.is_some());
                assert!(decoded.lpc_first_half.is_some());
                // The stable Q12 LPC has d_lpc taps.
                assert_eq!(decoded.lpc_second_half.a_q12().len(), expected_d);
            }
        }
    }

    /// A 10 ms frame carries no interpolation factor and reuses the
    /// second-half LPC throughout (no first-half split).
    #[test]
    fn ten_ms_frame_has_no_interpolation_split() {
        let buf: Vec<u8> = (0..96u16)
            .map(|i| (i.wrapping_mul(91) & 0xff) as u8)
            .collect();
        let mut rd = RangeDecoder::new(&buf);
        let cfg = fresh_cfg(Bandwidth::Wb, SilkFrameSize::TenMs, false);
        if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
            assert!(decoded.lsf_interp_q2.is_none());
            assert!(decoded.lpc_first_half.is_none());
        }
    }

    /// SWB / FB are rejected: SILK never sees them after the §4.2.2
    /// hybrid split. (The public Bandwidth enum carries them, so the
    /// decode must reject rather than mis-index a table.)
    #[test]
    fn swb_fb_rejected() {
        let buf = [0x42u8; 32];
        for &bw in &[Bandwidth::Swb, Bandwidth::Fb] {
            let mut rd = RangeDecoder::new(&buf);
            let cfg = fresh_cfg(bw, SilkFrameSize::TwentyMs, false);
            assert!(matches!(
                decode_silk_frame(&mut rd, cfg),
                Err(Error::MalformedPacket)
            ));
        }
    }
}
