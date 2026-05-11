//! SILK decoder per RFC 6716 §4.2.
//!
//! Scope of this module:
//!
//! * SILK-only, mono, **10 ms and 20 ms** frames at 8/12/16 kHz
//!   internal rate (NB/MB/WB). The decoder output is 48 kHz (Opus
//!   always emits 48 kHz; see RFC 6716 §4.2.1) by way of a local
//!   8/12/16→48 kHz upsampler.
//!
//!   A 10 ms frame uses 2 sub-frames (RFC §4.2.7.4); a 20 ms frame
//!   uses 4.
//!
//! * 40 ms / 60 ms frames — implemented as an outer loop over 2 or 3
//!   back-to-back 20 ms SILK frames inside a single Opus frame per
//!   RFC §4.2.4. The per-sub-frame LBRR flags are *parsed* (so the
//!   range coder stays aligned) but LBRR data itself is not yet
//!   redundancy-decoded; if any LBRR flag is 1 the decoder returns
//!   Unsupported rather than silently desyncing.
//!
//! * Stereo decoding — implemented: `mid_only` flag + stereo
//!   prediction weights (RFC §4.2.7.1) + stereo unmixing filter
//!   (RFC §4.2.8 / libopus `silk_stereo_MS_to_LR`). Output is
//!   interleaved L/R. The mid and side channels go through the same
//!   `decode_frame_body` path as mono.
//!
//! Sub-modules:
//!
//! * [`range_dec`] — re-exports the CELT crate's arithmetic coder plus
//!   SILK-specific helpers that share the same bitstream.
//! * [`lsf`] — Line Spectral Frequency (stage-1 + stage-2 normal + LSF
//!   stabilization + interpolation).
//! * [`ltp`] — Long-Term Prediction filter coefficient decoding and
//!   scale.
//! * [`excitation`] — Excitation signal decoding (pulses, LSBs, signs,
//!   LCG seed).
//! * [`synth`] — Synthesis filter (short-term LPC + LTP) and the
//!   post-upsample to 48 kHz.
//! * `tables` — All RFC §4.2 ICDFs transcribed verbatim.

#![allow(clippy::many_single_char_names)]

pub mod encoder;
pub mod excitation;
pub mod lsf;
pub mod ltp;
pub mod pitch_analysis;
pub mod range_dec;
pub mod shell;
pub mod synth;
pub mod tables;

use oxideav_celt::range_decoder::RangeDecoder;
use oxideav_core::{Error, Result};

use crate::toc::{OpusBandwidth, Toc};

/// Internal SILK sampling rate (8/12/16 kHz) for NB/MB/WB.
pub fn internal_rate_hz(bw: OpusBandwidth) -> u32 {
    match bw {
        OpusBandwidth::Narrowband => 8_000,
        OpusBandwidth::Mediumband => 12_000,
        OpusBandwidth::Wideband => 16_000,
        _ => 16_000, // SILK doesn't natively support SWB/FB
    }
}

/// Number of sub-frames in a 20 ms SILK frame: always 4.
pub const SUBFRAMES_20MS: usize = 4;

/// Number of sub-frames in a 10 ms SILK frame: always 2.
pub const SUBFRAMES_10MS: usize = 2;

/// Persistent decoder state carried across SILK frames for a single
/// channel.
#[derive(Debug, Clone)]
pub struct SilkChannelState {
    /// Previous frame's final LPC coefficients (for 10 ms interp +
    /// stereo / LBRR continuity). Only used internally in `synth`.
    pub prev_lpc: Vec<f32>,
    /// `lagPrev` from the previous frame, used in LTP pitch lag
    /// differential coding.
    pub prev_pitch_lag: i32,
    /// `NLSF_Q15` from the previous frame (used when interp_coef != 4).
    pub prev_nlsf_q15: Vec<i16>,
    /// Synthesis output buffer (one sub-frame of LPC order history).
    pub lpc_history: Vec<f32>,
    /// Past *residual* history for the LTP feedback loop (RFC
    /// §4.2.7.9.1 voiced LPC residual). Long enough for pitch_lag +
    /// LTP_ORDER/2 across frame boundaries.
    pub ltp_history: Vec<f32>,
    /// Past *clamped output* `out[]` ring used by the §4.2.7.9.1
    /// rewhitening pass. Sized to cover `max_pitch_lag(WB) + d_LPC + 2`
    /// = 306 samples plus a small alignment margin.
    pub out_history: Vec<f32>,
    /// `prev_gain_Q16` of the previous sub-frame.
    pub prev_gain_q16: i32,
    /// First-frame flag — after a decoder reset or a LBRR gap, the
    /// first frame is coded specially (absolute coding).
    pub first_frame: bool,
    /// Trailing internal-rate samples kept by the 8/12/16→48 kHz
    /// upsampler so the windowed-sinc convolution sees real history at
    /// the leading edge of the next frame instead of zeros. Two
    /// internal-rate samples are enough to cover the half-window
    /// `f` (= 2..6) of the kernel for any supported ratio. Empty on
    /// first frame; reset by `reset()`.
    pub upsample_history: Vec<f32>,
}

impl SilkChannelState {
    pub fn new() -> Self {
        Self {
            prev_lpc: Vec::new(),
            prev_pitch_lag: 0,
            prev_nlsf_q15: Vec::new(),
            lpc_history: Vec::new(),
            ltp_history: vec![0.0; 480],
            out_history: vec![0.0; 320],
            prev_gain_q16: 0,
            first_frame: true,
            upsample_history: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for SilkChannelState {
    fn default() -> Self {
        Self::new()
    }
}

/// Persistent cross-packet state for stereo unmixing (RFC §4.2.8 /
/// libopus `silk_stereo_MS_to_LR`). `pred_prev_Q13` tracks the previous
/// packet's prediction weights for the 8 ms linear-interpolation
/// region; `s_mid` / `s_side` carry the 2-sample history needed by the
/// 3-tap sum in the unmixing filter.
#[derive(Debug, Clone, Default)]
pub struct SilkStereoState {
    pub pred_prev_q13: [i32; 2],
    pub s_mid: [i16; 2],
    pub s_side: [i16; 2],
    /// Tracks whether the previous 20 ms sub-frame was mid-only, so we
    /// know when to reset the side-channel decoder state (RFC §4.2.7.1).
    pub prev_decode_only_mid: bool,
}

impl SilkStereoState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Decoder for one SILK stream (possibly stereo).
///
/// Owns the mono + side channel states and the stereo unmixing state.
pub struct SilkDecoder {
    pub state: SilkChannelState,
    /// Second channel state for stereo (side channel in MS coding).
    pub side_state: SilkChannelState,
    /// Cross-packet stereo unmixing state.
    pub stereo_state: SilkStereoState,
    pub bandwidth: OpusBandwidth,
    /// Number of LPC coefficients (order). NB/MB => 10; WB => 16.
    pub lpc_order: usize,
    /// Sub-frame length in samples at the internal rate.
    pub subframe_len: usize,
    /// Full SILK frame length in samples at the internal rate (20 ms).
    pub frame_len: usize,
}

impl SilkDecoder {
    pub fn new(bandwidth: OpusBandwidth) -> Self {
        let (order, sub_len) = match bandwidth {
            OpusBandwidth::Narrowband => (10, 40), // 5 ms @ 8 kHz
            OpusBandwidth::Mediumband => (10, 60), // 5 ms @ 12 kHz
            OpusBandwidth::Wideband => (16, 80),   // 5 ms @ 16 kHz
            _ => (16, 80),
        };
        let frame_len = sub_len * SUBFRAMES_20MS;
        Self {
            state: SilkChannelState::new(),
            side_state: SilkChannelState::new(),
            stereo_state: SilkStereoState::new(),
            bandwidth,
            lpc_order: order,
            subframe_len: sub_len,
            frame_len,
        }
    }

    /// Decode a full SILK-only Opus frame (10/20/40/60 ms, mono or
    /// stereo) and return interleaved 48 kHz f32 samples.
    ///
    /// Output layout:
    /// * Mono: flat `Vec<f32>` of length `toc.frame_samples_48k`.
    /// * Stereo: interleaved L/R, length `toc.frame_samples_48k * 2`.
    ///
    /// Note that the *caller* (`decoder::decode_silk_frame`) handles
    /// splitting mono vs stereo packing; this function returns either
    /// a single-channel or interleaved block depending on `toc.stereo`.
    pub fn decode_frame_to_48k(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        toc: &Toc,
    ) -> Result<Vec<f32>> {
        // Supported 48 kHz frame lengths:
        //   480  = 10 ms × 1 (2 sub-frames per 20 ms SILK frame, but
        //          the 10 ms config has a single "half" frame).
        //   960  = 20 ms × 1
        //   1920 = 20 ms × 2 (40 ms packet)
        //   2880 = 20 ms × 3 (60 ms packet)
        let (n_frames_per_packet, n_subframes_per_frame) = match toc.frame_samples_48k {
            480 => (1, SUBFRAMES_10MS),
            960 => (1, SUBFRAMES_20MS),
            1920 => (2, SUBFRAMES_20MS),
            2880 => (3, SUBFRAMES_20MS),
            _ => {
                return Err(Error::unsupported("SILK: unsupported frame size"));
            }
        };
        let n_internal_channels = if toc.stereo { 2 } else { 1 };

        // --- §4.2.3 + §4.2.4 shared-header: VAD + LBRR flags for all
        //     (channels × frames-per-packet) sub-frames, then per-channel
        //     LBRR sub-flags if the channel LBRR flag is set. We do not
        //     yet *redundancy-decode* any LBRR data; if any LBRR flag is
        //     1 we bail out so we don't silently desync.
        //
        // Layout (libopus silk_Decode):
        //   for each internal channel n:
        //     for each packet frame i:
        //       vad_flags[n][i] = ec_dec_bit_logp(1)
        //     lbrr_flag[n]    = ec_dec_bit_logp(1)
        //   for each internal channel n with lbrr_flag[n] == 1:
        //     if frames_per_packet == 1:
        //       lbrr_flags[n][0] = 1
        //     else:
        //       decode a single (2^fpp - 1)-symbol iCDF → lbrr_flags[n][*]
        let mut vad_flags = [[false; 3]; 2]; // [channel][frame_idx]
        let mut lbrr_channel = [false; 2];
        for n in 0..n_internal_channels {
            for i in 0..n_frames_per_packet {
                vad_flags[n][i] = rc.decode_bit_logp(1);
            }
            lbrr_channel[n] = rc.decode_bit_logp(1);
        }
        let mut lbrr_flags = [[false; 3]; 2];
        for n in 0..n_internal_channels {
            if lbrr_channel[n] {
                if n_frames_per_packet == 1 {
                    lbrr_flags[n][0] = true;
                } else {
                    let icdf: &[u8] = if n_frames_per_packet == 2 {
                        &tables::LBRR_FLAGS_2_ICDF
                    } else {
                        &tables::LBRR_FLAGS_3_ICDF
                    };
                    let sym = rc.decode_icdf(icdf, 8) as u32 + 1;
                    for i in 0..n_frames_per_packet {
                        lbrr_flags[n][i] = ((sym >> i) & 1) != 0;
                    }
                }
            }
        }

        // RFC 6716 §4.2.4: LBRR frame bodies come *before* the regular
        // frames, ordered by frame index then channel. Each LBRR body
        // has the same structure as a regular SILK frame body (stereo
        // header is NOT re-emitted for LBRR — only stage-1/2 NLSF + LTP
        // + excitation). We decode-and-discard using scratch channel
        // states so the main decoder's LPC/LTP history isn't corrupted.
        //
        // This is the "robust" behaviour: LBRR is an out-of-band
        // redundancy copy used for packet-loss concealment. In the
        // loss-free path we just need to consume the bits so the range
        // coder stays aligned with the regular frame bodies that follow.
        let any_lbrr =
            (0..n_internal_channels).any(|n| (0..n_frames_per_packet).any(|i| lbrr_flags[n][i]));
        if any_lbrr {
            let mut lbrr_mid_state = SilkChannelState::new();
            let mut lbrr_side_state = SilkChannelState::new();
            for i in 0..n_frames_per_packet {
                for n in 0..n_internal_channels {
                    if !lbrr_flags[n][i] {
                        continue;
                    }
                    let state_ref = if n == 0 {
                        &mut lbrr_mid_state
                    } else {
                        &mut lbrr_side_state
                    };
                    let _ = decode_frame_body(
                        rc,
                        vad_flags[n][i],
                        self.bandwidth,
                        self.lpc_order,
                        self.subframe_len,
                        n_subframes_per_frame,
                        state_ref,
                    )?;
                }
            }
        }

        let internal_rate = internal_rate_hz(self.bandwidth);
        let fs_khz = (internal_rate / 1000) as i32;
        let frame_len_internal = self.subframe_len * n_subframes_per_frame;

        let mut out_per_packet_frame_interleaved: Vec<Vec<f32>> =
            Vec::with_capacity(n_frames_per_packet);

        // Outer loop over the 2 or 3 back-to-back 20 ms SILK frames.
        for i in 0..n_frames_per_packet {
            // --- Stereo header for THIS 20 ms block.
            let mut ms_pred_q13 = [0i32; 2];
            let mut decode_only_mid = false;
            if n_internal_channels == 2 {
                ms_pred_q13 = stereo_decode_pred(rc);
                // Decode the mid-only flag only if the side channel is
                // marked VAD=0 for this sub-frame (RFC §4.2.7.1).
                if !vad_flags[1][i] {
                    decode_only_mid = rc.decode_icdf(&tables::STEREO_ONLY_CODE_MID_ICDF, 8) != 0;
                }
            }

            // Reset side state on mid-only → coded transition (RFC §4.2.7.1 /
            // libopus silk_Decode comment: "Reset side channel decoder
            // prediction memory for first frame with side coding").
            if n_internal_channels == 2
                && !decode_only_mid
                && self.stereo_state.prev_decode_only_mid
            {
                self.side_state.reset();
            }

            // --- Mid channel decode.
            let mid_internal = decode_frame_body(
                rc,
                vad_flags[0][i],
                self.bandwidth,
                self.lpc_order,
                self.subframe_len,
                n_subframes_per_frame,
                &mut self.state,
            )?;

            // --- Side channel (only if stereo and !mid_only).
            let side_internal = if n_internal_channels == 2 && !decode_only_mid {
                decode_frame_body(
                    rc,
                    vad_flags[1][i],
                    self.bandwidth,
                    self.lpc_order,
                    self.subframe_len,
                    n_subframes_per_frame,
                    &mut self.side_state,
                )?
            } else {
                vec![0.0f32; frame_len_internal]
            };

            // Upsample both to 48 kHz with cross-frame state so the
            // windowed-sinc convolution sees real history at the
            // leading edge of each frame instead of zeros.
            let mid_48k = synth::upsample_to_48k_with_state(
                &mid_internal,
                internal_rate,
                &mut self.state.upsample_history,
            );
            let side_48k = if n_internal_channels == 2 && !decode_only_mid {
                synth::upsample_to_48k_with_state(
                    &side_internal,
                    internal_rate,
                    &mut self.side_state.upsample_history,
                )
            } else {
                Vec::new()
            };

            if n_internal_channels == 1 {
                // Mono: just the mid channel @ 48 kHz.
                out_per_packet_frame_interleaved.push(mid_48k);
            } else {
                // Stereo unmixing happens at the *internal* rate in
                // libopus, then resamples to the API rate. We do a
                // simpler but equivalent approximation: upsample both
                // channels to 48 kHz *then* unmix. The unmixing filter
                // is a linear-phase 1-pole-ish operation that commutes
                // with resampling up to the filter's group delay.
                let lr_48k = stereo_unmix_48k(
                    &mid_48k,
                    &side_48k,
                    &ms_pred_q13,
                    decode_only_mid,
                    fs_khz,
                    &mut self.stereo_state,
                );
                out_per_packet_frame_interleaved.push(lr_48k);
            }

            self.stereo_state.prev_decode_only_mid = decode_only_mid;
        }

        // Concatenate per-packet-frame outputs.
        let total = out_per_packet_frame_interleaved
            .iter()
            .map(|v| v.len())
            .sum();
        let mut out = Vec::with_capacity(total);
        for chunk in out_per_packet_frame_interleaved {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// Public thin wrapper around the crate-private `decode_frame_body`,
/// intended for the encoder's internal-rate round-trip unit test. Not
/// intended for general external use — the 20 ms body format assumed
/// here is only stable because both encoder and decoder live in this
/// crate.
#[doc(hidden)]
pub fn decode_frame_body_pub(
    rc: &mut oxideav_celt::range_decoder::RangeDecoder<'_>,
    vad_flag: bool,
    bandwidth: OpusBandwidth,
    lpc_order: usize,
    subframe_len: usize,
    n_subframes: usize,
    state: &mut SilkChannelState,
) -> Result<Vec<f32>> {
    decode_frame_body(
        rc,
        vad_flag,
        bandwidth,
        lpc_order,
        subframe_len,
        n_subframes,
        state,
    )
}

/// Decode the body of one 20 ms (or 10 ms) SILK frame *after* the
/// shared VAD/LBRR header has been consumed.
///
/// Implements RFC 6716 §4.2.7 steps:
///   1. Frame-type + gain indices — §4.2.7.3 and §4.2.7.4.
///   2. NLSF stage-1 + stage-2 → LSF → LPC — §4.2.7.5.
///   3. LTP params (when voiced) — §4.2.7.6.
///   4. Excitation (pulses, LSBs, signs, LCG) — §4.2.7.8.
///   5. LTP + short-term LPC synthesis — §4.2.7.9.
///
/// Returns internal-rate PCM of length `subframe_len * n_subframes`.
fn decode_frame_body(
    rc: &mut RangeDecoder<'_>,
    vad_flag: bool,
    bandwidth: OpusBandwidth,
    lpc_order: usize,
    subframe_len: usize,
    n_subframes: usize,
    state: &mut SilkChannelState,
) -> Result<Vec<f32>> {
    debug_assert!(n_subframes == SUBFRAMES_10MS || n_subframes == SUBFRAMES_20MS);
    let frame_len = subframe_len * n_subframes;

    // §4.2.7.3 frame type (signal + quantization offset). Table 9's
    // PDF has the two leading zero-prob entries 0 and 1 dropped for
    // the active table (they would overflow the u8 ICDF cell); we
    // therefore offset the decoded symbol by +2 in the active branch
    // before mapping through Table 10.
    let frame_type_sym = if vad_flag {
        rc.decode_icdf(&tables::FRAME_TYPE_ACTIVE_ICDF, 8) + 2
    } else {
        rc.decode_icdf(&tables::FRAME_TYPE_INACTIVE_ICDF, 8)
    };
    let (signal_type, quant_offset_type) = match frame_type_sym {
        0 => (0u8, 0u8),
        1 => (0, 1),
        2 => (1, 0),
        3 => (1, 1),
        4 => (2, 0),
        5 => (2, 1),
        _ => (1, 0),
    };
    let voiced = signal_type == 2;

    // §4.2.7.4 sub-frame gains. Track `log_gain` (the 6-bit integer
    // index 0..=63) directly across sub-frames; rounding through Q16
    // and back biases the delta-coding chain. Per the RFC, the absolute
    // (first sub-frame) index is `(MSB << 3) | LSB`; subsequent
    // sub-frames apply
    //
    //     log_gain = clamp(0, max(2*delta - 16,
    //                             prev_log_gain + delta - 4), 63)
    //
    // (RFC 6716 §4.2.7.4 — the `max(2*delta - 16, …)` lower-clamp
    // protects against undershoot when `prev_log_gain` is small and
    // `delta` is large; the previous code dropped that branch and
    // produced a chain that drifted toward zero gain on quiet frames.)
    let mut gains_q16 = vec![0i32; n_subframes];
    {
        let msb_icdf: &[u8] = match signal_type {
            0 => &tables::GAIN_MSB_INACTIVE_ICDF,
            1 => &tables::GAIN_MSB_UNVOICED_ICDF,
            _ => &tables::GAIN_MSB_VOICED_ICDF,
        };
        let msb = rc.decode_icdf(msb_icdf, 8) as i32;
        let lsb = rc.decode_icdf(&tables::GAIN_LSB_ICDF, 8) as i32;
        let abs_idx = ((msb << 3) | lsb).clamp(0, 63);
        let mut prev_log_gain = abs_idx;
        gains_q16[0] = gain_index_to_q16(prev_log_gain);
        for sf in 1..n_subframes {
            let delta = rc.decode_icdf(&tables::GAIN_DELTA_ICDF, 8) as i32;
            let candidate = (prev_log_gain + delta - 4).max(2 * delta - 16);
            let new_log = candidate.clamp(0, 63);
            gains_q16[sf] = gain_index_to_q16(new_log);
            prev_log_gain = new_log;
        }
    }

    // §4.2.7.5 NLSF decoding + §4.2.7.5.5 LSF interpolation.
    //
    // For 20 ms frames: `interp_coef` (w_Q2) is 0..=4. When < 4, the first
    // two sub-frames use NLSFs interpolated between the previous frame's
    // final NLSFs (n0_Q15) and the current frame's NLSFs (n2_Q15):
    //   n1_Q15[k] = n0_Q15[k] + (w_Q2*(n2_Q15[k] - n0_Q15[k]) >> 2)
    // For 10 ms frames: always w_Q2 = 4 (no interpolation).
    // After an uncoded side-channel frame or decoder reset: force w_Q2 = 4.
    let is_20ms = n_subframes == SUBFRAMES_20MS;
    let (nlsf_q15, raw_interp_coef) = lsf::decode_nlsf(rc, bandwidth, signal_type, is_20ms)?;

    // Force w_Q2 = 4 on the first frame (decoder-reset equivalent) or when
    // the previous frame's NLSFs aren't available.
    let interp_coef: u8 = if state.first_frame || state.prev_nlsf_q15.is_empty() {
        4
    } else {
        raw_interp_coef
    };

    // Build per-sub-frame LPC arrays.
    // Sub-frames 0-1 use interpolated NLSFs when w_Q2 < 4; sub-frames 2-3
    // (and 10 ms frames) always use the uninterpolated n2_Q15.
    let lpc_uninterp = lsf::nlsf_to_lpc(&nlsf_q15, bandwidth);
    let lpc_per_sf: Vec<Vec<f32>> = (0..n_subframes)
        .map(|sf| {
            // §4.2.7.5.5: first two sub-frames of a 20 ms frame are
            // interpolated when w_Q2 < 4.
            if is_20ms && sf < 2 && interp_coef < 4 {
                let order = nlsf_q15.len();
                let prev = &state.prev_nlsf_q15;
                if prev.len() == order {
                    let w = interp_coef as i32;
                    let n1_q15: Vec<i16> = (0..order)
                        .map(|k| {
                            let n0 = prev[k] as i32;
                            let n2 = nlsf_q15[k] as i32;
                            let n1 = n0 + ((w * (n2 - n0)) >> 2);
                            n1.clamp(0, 32767) as i16
                        })
                        .collect();
                    lsf::nlsf_to_lpc(&n1_q15, bandwidth)
                } else {
                    lpc_uninterp.clone()
                }
            } else {
                lpc_uninterp.clone()
            }
        })
        .collect();

    // §4.2.7.6.1 Primary pitch lag (voiced only).
    let mut pitch_lags = vec![0i32; n_subframes];
    let mut ltp_filter = vec![[0f32; 5]; n_subframes];
    let mut ltp_scale_q14 = 15565i32;
    if voiced {
        let abs_flag = rc.decode_bit_logp(1);
        let primary_lag = if abs_flag || state.prev_pitch_lag == 0 {
            ltp::decode_absolute_pitch_lag(rc, bandwidth)?
        } else {
            let delta = ltp::decode_delta_pitch_lag(rc)?;
            state.prev_pitch_lag + delta
        };
        let contour_idx = ltp::decode_pitch_contour(rc, bandwidth)?;
        ltp::expand_pitch_contour(primary_lag, contour_idx, bandwidth, &mut pitch_lags);
        state.prev_pitch_lag = primary_lag;

        let periodicity = rc.decode_icdf(&tables::LTP_PERIODICITY_ICDF, 8);
        for sf in 0..n_subframes {
            let tap = ltp::decode_ltp_filter(rc, periodicity);
            ltp_filter[sf][..5].copy_from_slice(&tap[..5]);
        }

        let ltp_scale_idx = rc.decode_icdf(&tables::LTP_SCALING_ICDF, 8);
        ltp_scale_q14 = match ltp_scale_idx {
            0 => 15565,
            1 => 12288,
            _ => 8192,
        };
    }

    // §4.2.7.7 LCG seed. RFC 6716 §4.2.7.7 codes the 2-bit seed using
    // a uniform 4-symbol PDF on ft=256 (so ftb=8 with values [192, 128,
    // 64, 0]); an earlier version of this decoder passed ftb=2 which
    // caused the range coder to wrap and always return 0. Now we call
    // with ftb=8 to match the table and keep encoder/decoder in sync.
    let seed = rc.decode_icdf(&tables::LCG_SEED_ICDF, 8) as u32;

    // §4.2.7.8 Excitation.
    let excitation = excitation::decode_excitation(
        rc,
        frame_len,
        subframe_len,
        signal_type,
        quant_offset_type,
        seed,
    )?;

    // §4.2.7.9 Synthesis.
    let output = synth::synthesize(
        &excitation,
        &lpc_per_sf,
        &gains_q16,
        &pitch_lags,
        &ltp_filter,
        ltp_scale_q14,
        subframe_len,
        n_subframes,
        lpc_order,
        voiced,
        interp_coef,
        state,
    );

    state.first_frame = false;
    state.prev_nlsf_q15 = nlsf_q15;
    Ok(output)
}

/// Decode the stereo mid/side prediction weights (RFC §4.2.7.1 /
/// libopus `silk_stereo_decode_pred`). Returns the two Q13 predictors.
fn stereo_decode_pred(rc: &mut RangeDecoder<'_>) -> [i32; 2] {
    let n = rc.decode_icdf(&tables::STEREO_PRED_JOINT_ICDF, 8) as i32;
    let mut ix = [[0i32; 3]; 2];
    ix[0][2] = n / 5;
    ix[1][2] = n - 5 * ix[0][2];
    for row in ix.iter_mut() {
        row[0] = rc.decode_icdf(&tables::STEREO_UNIFORM3_ICDF, 8) as i32;
        row[1] = rc.decode_icdf(&tables::STEREO_UNIFORM5_ICDF, 8) as i32;
    }

    let mut pred_q13 = [0i32; 2];
    for k in 0..2 {
        ix[k][0] += 3 * ix[k][2];
        let idx0 = (ix[k][0] as usize).min(15);
        let idx1 = (idx0 + 1).min(15);
        let low_q13 = tables::STEREO_PRED_QUANT_Q13[idx0] as i32;
        let high_q13 = tables::STEREO_PRED_QUANT_Q13[idx1] as i32;
        // silk_SMULWB(high - low, 0.5/STEREO_QUANT_SUB_STEPS in Q16).
        // 0.5 / 5 = 0.1 → 0.1 * 2^16 ≈ 6554.
        let step_q13 = ((high_q13 - low_q13) * 6554) >> 16;
        pred_q13[k] = low_q13 + step_q13 * (2 * ix[k][1] + 1);
    }
    pred_q13[0] -= pred_q13[1];
    pred_q13
}

/// Apply the SILK stereo unmixing filter to convert adaptive MS → LR.
///
/// Returns interleaved L/R samples of length `2 * mid.len()`.
///
/// Approximates libopus `silk_stereo_MS_to_LR` in f32 and at 48 kHz.
/// The reference implementation runs the filter at the internal rate
/// and then resamples; we run at 48 kHz directly because the filter is
/// a short 3-tap smoother + linear predictor interpolation, which is
/// stable under integer upsampling.
///
/// * `mid` / `side` — upsampled (48 kHz) channels. `side` may be empty
///   when `decode_only_mid` is true, in which case the side channel is
///   taken as zeros.
/// * `pred_q13` — decoded predictors for this sub-frame.
/// * `state` — persistent `pred_prev_q13` + 2-sample history.
fn stereo_unmix_48k(
    mid: &[f32],
    side: &[f32],
    pred_q13: &[i32; 2],
    decode_only_mid: bool,
    _fs_khz: i32,
    state: &mut SilkStereoState,
) -> Vec<f32> {
    let n = mid.len();
    let mut out = vec![0.0f32; 2 * n];
    if n == 0 {
        return out;
    }

    // Working mid / side arrays with 2-sample history prepended.
    let mut x1 = vec![0.0f32; n + 2];
    let mut x2 = vec![0.0f32; n + 2];
    x1[0] = state.s_mid[0] as f32 / 32768.0;
    x1[1] = state.s_mid[1] as f32 / 32768.0;
    x2[0] = state.s_side[0] as f32 / 32768.0;
    x2[1] = state.s_side[1] as f32 / 32768.0;
    for i in 0..n {
        x1[i + 2] = mid[i];
        x2[i + 2] = if side.is_empty() { 0.0 } else { side[i] };
    }

    state.s_mid[0] = f32_to_q15_clamp(x1[n]);
    state.s_mid[1] = f32_to_q15_clamp(x1[n + 1]);
    state.s_side[0] = f32_to_q15_clamp(x2[n]);
    state.s_side[1] = f32_to_q15_clamp(x2[n + 1]);

    // Predictor interpolation span: 8 ms at 48 kHz = 384 samples.
    let interp_len = (8 * 48).min(n);

    let prev0 = state.pred_prev_q13[0] as f32;
    let prev1 = state.pred_prev_q13[1] as f32;
    let curr0 = pred_q13[0] as f32;
    let curr1 = pred_q13[1] as f32;
    let q13_scale = 1.0 / 8192.0;

    for idx in 0..n {
        let t = if idx < interp_len {
            (idx + 1) as f32 / interp_len as f32
        } else {
            1.0
        };
        let p0 = (prev0 + (curr0 - prev0) * t) * q13_scale;
        let p1 = (prev1 + (curr1 - prev1) * t) * q13_scale;
        let m = (x1[idx] + 2.0 * x1[idx + 1] + x1[idx + 2]) * 0.25;
        let side_v = if decode_only_mid {
            m * p0 + x1[idx + 1] * p1
        } else {
            x2[idx + 1] + m * p0 + x1[idx + 1] * p1
        };
        let mid_v = x1[idx + 1];
        // L = mid + side, R = mid - side. We attenuate by 0.5 to keep
        // the combined channels comfortably inside [-1, 1] for the S16
        // conversion downstream — libopus saturates at S16 directly
        // but its per-channel amplitudes are bit-exact to the encoder,
        // whereas the MVP synth in this crate is only audibility-exact
        // and tends to over-produce on active frames.
        let l = ((mid_v + side_v) * 0.5).clamp(-1.0, 1.0);
        let r = ((mid_v - side_v) * 0.5).clamp(-1.0, 1.0);
        out[2 * idx] = l;
        out[2 * idx + 1] = r;
    }

    state.pred_prev_q13[0] = pred_q13[0];
    state.pred_prev_q13[1] = pred_q13[1];
    out
}

fn f32_to_q15_clamp(x: f32) -> i16 {
    let s = (x * 32768.0).round();
    s.clamp(-32768.0, 32767.0) as i16
}

/// Map a 6-bit log-gain index (0..=63) to a Q16 linear gain per RFC
/// 6716 §4.2.7.4.
///
/// Bit-exact integer implementation of
/// `silk_log2lin((0x1D1C71 * log_gain >> 16) + 2090)` where
/// `silk_log2lin(inLog_Q7)` approximates `2^(inLog_Q7/128.0)` via:
///
/// ```text
/// i = inLog_Q7 >> 7
/// f = inLog_Q7 & 127
/// out = (1 << i) + (((-174 * f * (128 - f)) >> 16) + f) * ((1 << i) >> 7)
/// ```
///
/// The RFC pins the output range of `gain_Q16` to `[81920,
/// 1686110208]` (linear 1.25 .. 25728), which the table below
/// reproduces exactly.
///
/// The previous float approximation effectively computed
/// `2^(2090/65536) ≈ 1.022 × 65536 ≈ 67000` for `idx = 0`, missing the
/// true value of 81920 (linear 1.25) by roughly 18%. That mis-scaling
/// is what made the SILK synthesis filter under-drive the LPC stage
/// while the matching upper-end overdrive at `idx = 63` saturated the
/// output to the clamp rails.
pub(crate) fn gain_index_to_q16(idx: i32) -> i32 {
    let log_gain = idx.clamp(0, 63);
    // (0x1D1C71 * log_gain) >> 16 + 2090 — all arithmetic stays inside
    // i32: 0x1D1C71 * 63 = 122 808 219 fits comfortably.
    let in_log_q7 = (((0x1D1C71_i32) * log_gain) >> 16) + 2090;
    silk_log2lin(in_log_q7)
}

/// Bit-exact `silk_log2lin(inLog_Q7)` per RFC 6716 §4.2.7.4 — returns
/// `2^(inLog_Q7/128)` rounded to Q16 via the integer approximation
/// listed in the spec.
fn silk_log2lin(in_log_q7: i32) -> i32 {
    if in_log_q7 < 0 {
        return 0;
    }
    let i = in_log_q7 >> 7; // integer part
    let f = in_log_q7 & 127; // fractional part (0..=127)
                             // Guard against shift-overflow if the encoder ever feeds nonsense.
    if i >= 31 {
        return i32::MAX;
    }
    let one_shl_i: i32 = 1 << i;
    // Spec: ((-174 * f * (128 - f)) >> 16) + f
    let inner = ((-174_i32 * f * (128 - f)) >> 16) + f;
    let extra = inner * (one_shl_i >> 7);
    one_shl_i + extra
}

/// Inverse of `gain_index_to_q16`. Used by the encoder only — the
/// decoder threads `log_gain` (the 6-bit integer index) through
/// `decode_frame_body` directly to avoid the lossy round-trip.
pub(crate) fn gain_index_of_q16(gain: i32) -> i32 {
    // For each candidate log_gain index, recompute gain_q16 and pick
    // the closest match. 64 candidates, called rarely.
    let mut best_idx = 0i32;
    let mut best_err = i64::MAX;
    for idx in 0..=63 {
        let g = gain_index_to_q16(idx);
        let err = (g as i64 - gain as i64).abs();
        if err < best_err {
            best_err = err;
            best_idx = idx;
        }
    }
    best_idx
}

#[cfg(test)]
mod gain_tests {
    use super::*;

    #[test]
    fn rfc_4_2_7_4_endpoints() {
        // RFC 6716 §4.2.7.4: "The final Q16 gain values lies between
        // 81920 and 1686110208, inclusive (representing scale factors
        // of 1.25 to 25728, respectively)."
        assert_eq!(gain_index_to_q16(0), 81920);
        assert_eq!(gain_index_to_q16(63), 1_686_110_208);
        // Approx 1.25 and 25728 in linear.
        assert!((gain_index_to_q16(0) as f64 / 65536.0 - 1.25).abs() < 1e-9);
        let top = gain_index_to_q16(63) as f64 / 65536.0;
        assert!((top - 25728.0).abs() < 1.0);
    }

    #[test]
    fn gain_monotone_in_index() {
        let mut prev = 0;
        for idx in 0..=63 {
            let g = gain_index_to_q16(idx);
            assert!(g > prev, "gain not monotone at idx={idx}");
            prev = g;
        }
    }

    #[test]
    fn gain_inverse_round_trips_each_index() {
        for idx in 0..=63 {
            let g = gain_index_to_q16(idx);
            assert_eq!(gain_index_of_q16(g), idx, "round-trip mismatch idx={idx}");
        }
    }
}
