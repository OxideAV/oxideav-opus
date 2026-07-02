//! SILK frame synthesis composition — RFC 6716 §4.2.7.9.
//!
//! This module is the composition seam between the bitstream-consuming
//! front half of the SILK decode (everything up to and including the
//! [`crate::silk_decode::SilkFrameDecoded`] parameter set) and the
//! signal-reconstructing back half. It takes one fully decoded regular
//! SILK frame plus the cross-frame synthesis state and produces the
//! frame's time-domain output samples at the **internal SILK rate**
//! (8 kHz NB / 12 kHz MB / 16 kHz WB), running, subframe by subframe in
//! source order:
//!
//! 1. §4.2.7.9.1 LTP synthesis ([`crate::silk_ltp_synth`]) — for voiced
//!    frames the excitation is passed through the rewhiten + LTP
//!    convolution; for unvoiced frames it is the normalized excitation
//!    copy.
//! 2. §4.2.7.9.2 LPC synthesis ([`crate::silk_lpc_synth`]) — the LPC
//!    residual `res[]` is run through the short-term predictor producing
//!    the clamped `out[]` samples and the unclamped `lpc[]` values.
//! 3. The §4.2.7.9.2 state carry: the unclamped `lpc[]` feeds the next
//!    subframe's LPC filter and the LTP rewhitening's region-B path; the
//!    clamped `out[]` feeds the LTP rewhitening's region-A path.
//!
//! ## Per-subframe LPC selection (§4.2.7.9)
//!
//! The RFC §4.2.7.9 preamble says: "If this is the first or second
//! subframe of a 20 ms SILK frame and the LSF interpolation factor,
//! w_Q2 …, is less than 4, then [a_Q12] correspond to the final LPC
//! coefficients produced … from the interpolated LSF coefficients,
//! n1_Q15[k] …. Otherwise, they correspond to the final LPC
//! coefficients produced from the uninterpolated LSF coefficients for
//! the current frame, n2_Q15[k]."
//!
//! [`SilkFrameDecoded`] carries both: `lpc_first_half` is the n1-derived
//! filter (present only for a 20 ms frame with an interpolation split),
//! and `lpc_second_half` is the n2-derived filter (always present). This
//! module maps subframes 0 / 1 of an interpolation-split 20 ms frame to
//! `lpc_first_half` and every other subframe to `lpc_second_half`.
//!
//! The companion §4.2.7.9.1 LSF-interpolation-split flag for subframes
//! 2 / 3 (`lsf_interp_used`, which switches the LTP rewhitening to the
//! `out_end = j - (s-2)*n`, `LTP_scale_Q14 = 16384` branch) fires under
//! the same condition — a 20 ms frame whose decoded `w_Q2 < 4`.
//!
//! ## Cross-frame state
//!
//! The §4.2.7.9.1 LTP `out[]` / `lpc[]` histories and the §4.2.7.9.2 LPC
//! history persist across SILK frames (and across Opus frames within a
//! packet); they are cleared only on a §4.5.2 decoder reset or after an
//! uncoded regular SILK frame for the side channel. [`SilkSynthState`]
//! holds both and is threaded by the caller across the SILK frames of an
//! Opus frame.
//!
//! All truth is taken from RFC 6716 §4.2.7.9. No external library source
//! is consulted.

use crate::silk_decode::SilkFrameDecoded;
use crate::silk_excitation::SilkFrameSize;
use crate::silk_frame::SignalType;
use crate::silk_gains::SILK_MAX_SUBFRAMES;
use crate::silk_lpc_synth::{lpc_synthesis_subframe, subframe_samples, LpcSynthState};
use crate::silk_ltp::LTP_FILTER_TAPS;
use crate::silk_ltp_synth::{
    ltp_synth_commit_subframe, ltp_synthesis_subframe, LtpSynthState, LtpSynthSubframe,
};
use crate::toc::Bandwidth;
use crate::Error;

/// Cross-frame SILK synthesis state for one channel: the §4.2.7.9.1 LTP
/// histories (`out[]` clamped + `lpc[]` unclamped) and the §4.2.7.9.2 LPC
/// synthesis history.
///
/// Both are zero on construction and after [`Self::reset`] (the §4.5.2
/// decoder-reset path / uncoded side-channel frame). They persist across
/// SILK frames within an Opus frame and across Opus frames within a
/// stream.
#[derive(Debug, Clone)]
pub struct SilkSynthState {
    bandwidth: Bandwidth,
    ltp: LtpSynthState,
    lpc: LpcSynthState,
}

impl SilkSynthState {
    /// Construct a fresh zero-initialised synthesis state for `bandwidth`.
    ///
    /// Rejects SWB / FB (the SILK layer never sees them after the §4.2.2
    /// hybrid split).
    pub fn new(bandwidth: Bandwidth) -> Result<Self, Error> {
        Ok(Self {
            bandwidth,
            ltp: LtpSynthState::new(bandwidth)?,
            lpc: LpcSynthState::new(bandwidth)?,
        })
    }

    /// Bandwidth this state was created for.
    pub fn bandwidth(&self) -> Bandwidth {
        self.bandwidth
    }

    /// Clear all histories (the §4.5.2 decoder reset / uncoded
    /// side-channel-frame path).
    pub fn reset(&mut self) {
        self.ltp.reset();
        self.lpc.reset();
    }

    /// Read-only access to the LTP synthesis state (for tests / callers
    /// that want to inspect history).
    pub fn ltp(&self) -> &LtpSynthState {
        &self.ltp
    }

    /// Read-only access to the LPC synthesis state.
    pub fn lpc(&self) -> &LpcSynthState {
        &self.lpc
    }
}

/// Convert a Q12 LPC coefficient slice (`&[i32]`, as produced by
/// [`crate::silk_lsf_to_lpc::LpcQ12::a_q12`]) into the `i16` slice the
/// §4.2.7.9 synthesis filters expect. The §4.2.7.5.8 prediction-gain
/// limiter guarantees the coefficients fit in `i16`; values are clamped
/// defensively so a malformed upstream value can never wrap.
fn a_q12_i16(a_q12: &[i32]) -> Vec<i16> {
    a_q12
        .iter()
        .map(|&c| c.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
        .collect()
}

/// Synthesize one fully decoded regular SILK frame into time-domain
/// output samples at the internal SILK rate, threading the cross-frame
/// [`SilkSynthState`].
///
/// Returns the clamped `out[]` signal for the whole SILK frame
/// (`subframe_samples(bandwidth) * num_subframes` samples, nominal range
/// `[-1.0, 1.0]`). The caller resamples this internal-rate signal to the
/// decoder output rate (§4.2.9, non-normative).
///
/// `decoded` is the parameter set from
/// [`crate::silk_decode::decode_silk_frame`]; `state` carries the
/// §4.2.7.9 histories across SILK frames.
///
/// Errors:
///
/// * [`Error::MalformedPacket`] if `state.bandwidth()` disagrees with
///   `bandwidth`, if a per-subframe LPC filter has the wrong order, or if
///   any inner synthesis stage rejects.
pub fn synthesize_silk_frame(
    bandwidth: Bandwidth,
    frame_size: SilkFrameSize,
    decoded: &SilkFrameDecoded,
    state: &mut SilkSynthState,
) -> Result<Vec<f32>, Error> {
    if state.bandwidth != bandwidth {
        return Err(Error::MalformedPacket);
    }
    let n = subframe_samples(bandwidth)?;
    let num_subframes = match frame_size {
        SilkFrameSize::TenMs => 2usize,
        SilkFrameSize::TwentyMs => 4usize,
    };

    // §4.2.7.4: dequantise the per-subframe log-gain to Q16.
    let gains_q16 = decoded.gains.dequant_q16();
    if decoded.gains.len() != num_subframes {
        return Err(Error::MalformedPacket);
    }

    // §4.2.7.9: the LSF-interpolation split fires for a 20 ms frame whose
    // decoded w_Q2 < 4 (then `lpc_first_half` is Some). When it does,
    // subframes 0/1 use the interpolated (n1) LPC and subframes 2/3 use
    // the uninterpolated (n2) LPC with the §4.2.7.9.1 split rewhitening.
    let interp_split =
        matches!(frame_size, SilkFrameSize::TwentyMs) && decoded.lpc_first_half.is_some();

    // The excitation for the whole SILK frame (Q23), partitioned into
    // per-subframe windows below. §4.2.7.8: a 10 ms MB frame codes 8
    // shell blocks (128 samples) of which only the first 120 are used
    // — the parsed excitation may therefore be LONGER than the frame's
    // sample count, and the tail is discarded (a round-382 find: this
    // was previously rejected, making every 10 ms MB SILK packet fail
    // to synthesize).
    let e_q23 = decoded.excitation.e_q23();
    if e_q23.len() < n * num_subframes {
        return Err(Error::MalformedPacket);
    }
    let e_q23 = &e_q23[..n * num_subframes];

    // The voiced LTP parameters (pitch lags + 5-tap filters). Empty for
    // unvoiced frames; the LTP-synth path consults them only when voiced.
    let pitch_lags = decoded.ltp.pitch_lags();
    let filter_taps = decoded.ltp.filter_taps_q7();
    let ltp_scaling_q14 = decoded.ltp.ltp_scaling_q14();
    let is_voiced = decoded.signal_type == SignalType::Voiced;
    if is_voiced && (pitch_lags.len() != num_subframes || filter_taps.len() != num_subframes) {
        return Err(Error::MalformedPacket);
    }

    state.ltp.start_frame();

    let mut out = vec![0.0f32; n * num_subframes];

    for s in 0..num_subframes {
        // §4.2.7.9 per-subframe LPC selection.
        let a_q12 = if interp_split && s < 2 {
            // first / second subframe of an interpolation-split 20 ms
            // frame → interpolated (n1) filter.
            decoded
                .lpc_first_half
                .as_ref()
                .ok_or(Error::MalformedPacket)?
                .a_q12()
        } else {
            decoded.lpc_second_half.a_q12()
        };
        let a_i16 = a_q12_i16(a_q12);

        // §4.2.7.9.1 LTP synthesis → LPC residual `res[]`.
        let (pitch_lag, b_q7) = if is_voiced {
            (pitch_lags[s], filter_taps[s])
        } else {
            // Unvoiced: pitch_lag / b_q7 are unused but must be a valid
            // (positive) placeholder so the subframe config validates.
            (1i32, [0i8; LTP_FILTER_TAPS])
        };
        // §4.2.7.9.1: the "third/fourth subframe of a 20 ms frame with
        // w_Q2 < 4" split branch fires when interp_split && s >= 2.
        let lsf_interp_used = interp_split;

        let j = s * n;
        let e_sub = &e_q23[j..j + n];
        let mut res = vec![0.0f32; n];
        let cfg = LtpSynthSubframe {
            bandwidth,
            signal_type: decoded.signal_type,
            frame_size,
            subframe_index: s as u8,
            gain_q16: gains_q16[s],
            pitch_lag,
            b_q7,
            ltp_scaling_q14,
            a_q12: &a_i16,
            lsf_interp_used,
        };
        ltp_synthesis_subframe(&state.ltp, cfg, e_sub, &mut res)?;

        // §4.2.7.9.2 LPC synthesis → clamped out[] + unclamped lpc[].
        let mut out_sub = vec![0.0f32; n];
        let lpc_unclamped = lpc_synthesis_subframe(
            bandwidth,
            &mut state.lpc,
            &res,
            gains_q16[s],
            &a_i16,
            &mut out_sub,
        )?;

        // §4.2.7.9.2 state carry: commit the clamped out[] and unclamped
        // lpc[] into the LTP histories for the next subframe's rewhiten.
        ltp_synth_commit_subframe(&mut state.ltp, &out_sub, &lpc_unclamped)?;

        out[j..j + n].copy_from_slice(&out_sub);
    }

    Ok(out)
}

/// Synthesize a sequence of decoded SILK frames into one contiguous
/// internal-rate output buffer, threading `state` across the frames.
///
/// Convenience for an Opus frame that carries 2 or 3 SILK frames (40 / 60
/// ms). Each frame's samples are appended in order; `state` carries the
/// §4.2.7.9 histories across them.
pub fn synthesize_silk_frames(
    bandwidth: Bandwidth,
    frame_size: SilkFrameSize,
    decoded: &[SilkFrameDecoded],
    state: &mut SilkSynthState,
) -> Result<Vec<f32>, Error> {
    let n = subframe_samples(bandwidth)?;
    let per_frame = n * match frame_size {
        SilkFrameSize::TenMs => 2usize,
        SilkFrameSize::TwentyMs => 4usize,
    };
    let mut out = Vec::with_capacity(per_frame * decoded.len());
    for frame in decoded {
        let frame_out = synthesize_silk_frame(bandwidth, frame_size, frame, state)?;
        out.extend_from_slice(&frame_out);
    }
    Ok(out)
}

/// The internal-rate sample count one SILK frame of the given bandwidth ×
/// duration produces (a convenience mirror of
/// [`crate::silk_resampler::silk_frame_samples_internal`] expressed via
/// the synthesis subframe geometry).
pub fn silk_frame_internal_samples(
    bandwidth: Bandwidth,
    frame_size: SilkFrameSize,
) -> Result<usize, Error> {
    let n = subframe_samples(bandwidth)?;
    let num_subframes = match frame_size {
        SilkFrameSize::TenMs => 2usize,
        SilkFrameSize::TwentyMs => 4usize,
    };
    debug_assert!(num_subframes <= SILK_MAX_SUBFRAMES);
    Ok(n * num_subframes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range_decoder::RangeDecoder;
    use crate::silk_decode::{decode_silk_frame, SilkFrameConfig};

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
            stereo: None,
        }
    }

    /// Synthesis state constructor routes d_LPC and rejects SWB / FB.
    #[test]
    fn state_new_routes_and_rejects() {
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            let s = SilkSynthState::new(bw).unwrap();
            assert_eq!(s.bandwidth(), bw);
        }
        assert!(SilkSynthState::new(Bandwidth::Swb).is_err());
        assert!(SilkSynthState::new(Bandwidth::Fb).is_err());
    }

    /// `silk_frame_internal_samples` matches the §4.2.9 geometry.
    #[test]
    fn internal_sample_counts() {
        assert_eq!(
            silk_frame_internal_samples(Bandwidth::Nb, SilkFrameSize::TenMs).unwrap(),
            80
        );
        assert_eq!(
            silk_frame_internal_samples(Bandwidth::Nb, SilkFrameSize::TwentyMs).unwrap(),
            160
        );
        assert_eq!(
            silk_frame_internal_samples(Bandwidth::Mb, SilkFrameSize::TwentyMs).unwrap(),
            240
        );
        assert_eq!(
            silk_frame_internal_samples(Bandwidth::Wb, SilkFrameSize::TwentyMs).unwrap(),
            320
        );
        assert_eq!(
            silk_frame_internal_samples(Bandwidth::Wb, SilkFrameSize::TenMs).unwrap(),
            160
        );
    }

    /// Bandwidth mismatch between the synthesis state and the requested
    /// frame is rejected.
    #[test]
    fn rejects_bandwidth_mismatch() {
        let buf = [0x33u8; 96];
        let mut rd = RangeDecoder::new(&buf);
        let cfg = fresh_cfg(Bandwidth::Nb, SilkFrameSize::TwentyMs, false);
        if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
            let mut state = SilkSynthState::new(Bandwidth::Wb).unwrap();
            assert!(matches!(
                synthesize_silk_frame(Bandwidth::Nb, SilkFrameSize::TwentyMs, &decoded, &mut state),
                Err(Error::MalformedPacket)
            ));
        }
    }

    /// A real decoded frame synthesizes to the right number of samples and
    /// every sample is in nominal range / finite.
    #[test]
    fn synthesize_produces_in_range_samples() {
        let buf: Vec<u8> = (0..160u16)
            .map(|i| (i.wrapping_mul(67).wrapping_add(5) & 0xff) as u8)
            .collect();
        for (bw, expected) in [
            (Bandwidth::Nb, 160usize),
            (Bandwidth::Mb, 240),
            (Bandwidth::Wb, 320),
        ] {
            for voiced in [false, true] {
                let mut rd = RangeDecoder::new(&buf);
                let cfg = fresh_cfg(bw, SilkFrameSize::TwentyMs, voiced);
                if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
                    let mut state = SilkSynthState::new(bw).unwrap();
                    let out =
                        synthesize_silk_frame(bw, SilkFrameSize::TwentyMs, &decoded, &mut state)
                            .unwrap();
                    assert_eq!(out.len(), expected, "bw={bw:?} voiced={voiced}");
                    for (i, &v) in out.iter().enumerate() {
                        assert!(v.is_finite(), "non-finite at {i}: {v}");
                        assert!(
                            (-1.0..=1.0).contains(&v),
                            "out[{i}]={v} outside nominal range (bw={bw:?})"
                        );
                    }
                }
            }
        }
    }

    /// A 10 ms frame synthesizes to two subframes; no interpolation split.
    #[test]
    fn ten_ms_two_subframes() {
        let buf: Vec<u8> = (0..96u16)
            .map(|i| (i.wrapping_mul(43).wrapping_add(9) & 0xff) as u8)
            .collect();
        let mut rd = RangeDecoder::new(&buf);
        let cfg = fresh_cfg(Bandwidth::Wb, SilkFrameSize::TenMs, false);
        if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
            assert!(decoded.lpc_first_half.is_none());
            let mut state = SilkSynthState::new(Bandwidth::Wb).unwrap();
            let out =
                synthesize_silk_frame(Bandwidth::Wb, SilkFrameSize::TenMs, &decoded, &mut state)
                    .unwrap();
            assert_eq!(out.len(), 160); // 80 * 2
        }
    }

    /// Synthesis is deterministic: the same decoded frame + fresh state
    /// yields identical output.
    #[test]
    fn synthesis_is_deterministic() {
        let buf: Vec<u8> = (0..160u16)
            .map(|i| (i.wrapping_mul(89).wrapping_add(1) & 0xff) as u8)
            .collect();
        let mut rd = RangeDecoder::new(&buf);
        let cfg = fresh_cfg(Bandwidth::Nb, SilkFrameSize::TwentyMs, true);
        if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
            let mut s1 = SilkSynthState::new(Bandwidth::Nb).unwrap();
            let mut s2 = SilkSynthState::new(Bandwidth::Nb).unwrap();
            let o1 =
                synthesize_silk_frame(Bandwidth::Nb, SilkFrameSize::TwentyMs, &decoded, &mut s1)
                    .unwrap();
            let o2 =
                synthesize_silk_frame(Bandwidth::Nb, SilkFrameSize::TwentyMs, &decoded, &mut s2)
                    .unwrap();
            assert_eq!(o1, o2);
        }
    }

    /// The multi-frame helper concatenates per-frame outputs and threads
    /// state (the second frame's history is non-zero going in).
    #[test]
    fn multi_frame_concatenates() {
        let buf: Vec<u8> = (0..160u16)
            .map(|i| (i.wrapping_mul(31).wrapping_add(7) & 0xff) as u8)
            .collect();
        let mut rd = RangeDecoder::new(&buf);
        let cfg = fresh_cfg(Bandwidth::Nb, SilkFrameSize::TwentyMs, false);
        if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
            let frames = [decoded.clone(), decoded.clone()];
            let mut state = SilkSynthState::new(Bandwidth::Nb).unwrap();
            let out =
                synthesize_silk_frames(Bandwidth::Nb, SilkFrameSize::TwentyMs, &frames, &mut state)
                    .unwrap();
            assert_eq!(out.len(), 320); // 160 * 2
        }
    }

    /// Reset clears the carried histories.
    #[test]
    fn reset_clears_histories() {
        let buf: Vec<u8> = (0..160u16)
            .map(|i| (i.wrapping_mul(53).wrapping_add(2) & 0xff) as u8)
            .collect();
        let mut rd = RangeDecoder::new(&buf);
        let cfg = fresh_cfg(Bandwidth::Wb, SilkFrameSize::TwentyMs, true);
        if let Ok(decoded) = decode_silk_frame(&mut rd, cfg) {
            let mut state = SilkSynthState::new(Bandwidth::Wb).unwrap();
            synthesize_silk_frame(Bandwidth::Wb, SilkFrameSize::TwentyMs, &decoded, &mut state)
                .unwrap();
            // After a frame the LPC history may be non-zero; reset clears it.
            state.reset();
            assert!(state.lpc().history().iter().all(|&x| x == 0.0));
            assert!(state.ltp().out_history().iter().all(|&x| x == 0.0));
            assert!(state.ltp().lpc_history().iter().all(|&x| x == 0.0));
        }
    }
}
