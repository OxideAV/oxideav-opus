//! SILK synthesis filter — RFC 6716 §4.2.7.9.
//!
//! Applies LTP + short-term LPC synthesis to the excitation to
//! reconstruct the internal-rate output. The output is then upsampled
//! to 48 kHz to match Opus's fixed output rate.
//!
//! Excitation is fed in `e_Q23/2^23` form (the `silk::shell` decoder
//! emits the post-§4.2.7.8.6 reconstructed value already divided by
//! 2^23). Per RFC §4.2.7.9.1 the unvoiced residual is the excitation
//! verbatim; the voiced residual additionally folds in five rescaled
//! past LTP taps via the LTP filter. §4.2.7.9.2 then runs the short-term
//! all-pole LPC synthesis with the scalar gain factor `gain_Q16[s]/65536`,
//! and the final output is clamped to [-1, 1].
//!
//! LSF interpolation (§4.2.7.5.5): for 20 ms frames where the interpolation
//! factor w_Q2 < 4, sub-frames 0 and 1 use LPC coefficients derived from
//! the interpolated NLSFs. The caller passes `lpc_per_sf` with one LPC
//! array per sub-frame; this function selects the right one for each sf.
//!
//! §4.2.7.9.1 Rewhitening: for voiced frames the LTP residual `res[]` is
//! computed separately from the LPC synthesis output `out[]`. The LTP
//! feedback loop runs in residual space (pre-LPC-synthesis), not in output
//! space. This is the "rewhitening" pass referred to in the RFC and libopus
//! source (silk_LTP_analysis_filter_FIX.c). Keeping separate `res_ring[]`
//! and `out[]` histories is critical for correct voiced reconstruction.

use crate::silk::SilkChannelState;

/// Synthesize internal-rate output from excitation + filter parameters.
///
/// * `excitation` — Q0 excitation samples, length = frame_len.
/// * `lpc_per_sf` — per-sub-frame LPC coefficients (length `n_subframes`).
///   Each entry has length `lpc_order`. Sub-frames 0-1 may carry
///   interpolated NLSFs when `interp_coef < 4`; sub-frames 2-3 always
///   use the uninterpolated NLSFs.
/// * `gains_q16` — per sub-frame synthesis gain (Q16).
/// * `pitch_lags` / `ltp_filter` — per sub-frame LTP params.
/// * `ltp_scale_q14` — LTP scaling factor (Q14, decoded per §4.2.7.6.3).
/// * `subframe_len` — sub-frame length at internal rate (40/60/80).
/// * `n_subframes` — 2 for a 10 ms SILK frame, 4 for a 20 ms frame.
/// * `lpc_order` — 10 for NB/MB, 16 for WB.
/// * `voiced` — apply LTP only when true.
/// * `interp_coef` — w_Q2 from §4.2.7.5.5 (4 = no interpolation). Unused
///   in the synthesis itself; carried for callers that thread it through.
/// * `state` — persistent state (history buffers).
pub fn synthesize(
    excitation: &[f32],
    lpc_per_sf: &[Vec<f32>],
    gains_q16: &[i32],
    pitch_lags: &[i32],
    ltp_filter: &[[f32; 5]],
    ltp_scale_q14: i32,
    subframe_len: usize,
    n_subframes: usize,
    lpc_order: usize,
    voiced: bool,
    interp_coef: u8,
    state: &mut SilkChannelState,
) -> Vec<f32> {
    let _ = interp_coef;
    let frame_len = excitation.len();
    let mut out = vec![0f32; frame_len];

    // Ensure history buffers are large enough.
    if state.lpc_history.len() < lpc_order {
        state.lpc_history.resize(lpc_order, 0.0);
    }
    // The LTP history stores the *residual* values (res[]) from which
    // LTP predictions are drawn, per RFC §4.2.7.9.1. The history must
    // be long enough to hold the maximum pitch lag (288 samples for WB)
    // plus the LTP filter half-width (2 samples either side of centre).
    // 480 samples is sufficient for all bandwidths.
    let ltp_hist_len = 480usize;
    if state.ltp_history.len() < ltp_hist_len {
        state.ltp_history.resize(ltp_hist_len, 0.0);
    }

    // RFC §4.2.7.9.1 / §4.2.7.9.2 two-stage synthesis:
    //
    //   Stage 1 — LTP (voiced only): build residual res[] from excitation
    //   and past residuals. LTP feedback is in *residual* space so the
    //   LTP filter sees the pre-LPC-synthesis signal, not the all-pole
    //   output (this is the "rewhitening" distinction from §4.2.7.9.1).
    //
    //   Stage 2 — LPC synthesis: run the all-pole short-term IIR on res[]
    //   to get out[]. The feedback in stage 2 is the *unclamped* IIR
    //   output (pre-clamp) so the AR filter stays accurate even when
    //   individual samples clip.
    //
    // Keeping a separate res_ring[] is necessary because LTP taps index
    // res[i - lag + 2 - k], which crosses sub-frame boundaries and even
    // crosses into the previous frame (via state.ltp_history). Using
    // out[] for LTP would mix the LPC gain into the LTP loop twice.

    // LTP scale factor: Q14 → float. RFC §4.2.7.6.3 Table 43 values
    // {15565, 12288, 8192} divide by 16384.0.
    let ltp_scale = ltp_scale_q14 as f32 / 16384.0;

    // Per-frame residual buffer. Used for LTP indexing within this frame;
    // the sub-frame loop writes here before using it for LPC.
    let mut res_ring = vec![0.0f32; frame_len];

    // Per-frame LPC ring: unclamped IIR values for cross-subframe feedback.
    let mut lpc_ring = vec![0.0f32; frame_len];

    for sf in 0..n_subframes {
        let sf_start = sf * subframe_len;
        let sf_end = sf_start + subframe_len;
        let lpc = &lpc_per_sf[sf];
        // Overall gain: Q16 → f32.
        let g = gains_q16[sf].max(1) as f32 / 65536.0;
        let taps = &ltp_filter[sf];
        let lag = pitch_lags[sf];

        for n in sf_start..sf_end {
            // RFC §4.2.7.9.1 voiced residual.
            let mut res = g * excitation[n];
            if voiced && lag > 0 {
                for k in 0..5usize {
                    let idx = n as i32 - lag + 2 - k as i32;
                    let past_res = if idx >= 0 && (idx as usize) < n {
                        res_ring[idx as usize]
                    } else if idx >= 0 {
                        0.0
                    } else {
                        let abs_j = ltp_hist_len as i32 + idx;
                        if abs_j >= 0 {
                            state.ltp_history[abs_j as usize]
                        } else {
                            0.0
                        }
                    };
                    res += taps[k] * ltp_scale * past_res;
                }
            }
            res_ring[n] = res;

            // Stage 2: LPC synthesis.
            let mut s = res;
            for k in 1..=lpc_order {
                let idx = n as i32 - k as i32;
                let past_out = if idx >= 0 {
                    lpc_ring[idx as usize]
                } else {
                    let h_idx = (state.lpc_history.len() as i32 + idx) as usize;
                    state.lpc_history.get(h_idx).copied().unwrap_or(0.0)
                };
                s += lpc[k - 1] * past_out;
            }
            lpc_ring[n] = s;
            out[n] = s.clamp(-1.0, 1.0);
        }
    }

    // Persist LPC history (unclamped values for next frame's IIR).
    let lpc_keep = lpc_order.min(lpc_ring.len());
    state.lpc_history = lpc_ring[lpc_ring.len() - lpc_keep..].to_vec();

    // Persist LTP history: shift in the residuals from this frame so the
    // next frame's LTP lookup can reach them.
    let keep = ltp_hist_len.saturating_sub(frame_len);
    let mut new_ltp = Vec::with_capacity(ltp_hist_len);
    new_ltp.extend_from_slice(&state.ltp_history[ltp_hist_len - keep..]);
    new_ltp.extend_from_slice(&res_ring);
    if new_ltp.len() > ltp_hist_len {
        let drop = new_ltp.len() - ltp_hist_len;
        new_ltp.drain(0..drop);
    } else if new_ltp.len() < ltp_hist_len {
        let mut pad = vec![0f32; ltp_hist_len - new_ltp.len()];
        pad.extend(new_ltp);
        new_ltp = pad;
    }
    state.ltp_history = new_ltp;

    out
}

/// Upsample the internal-rate signal to 48 kHz.
///
/// Uses a simple 2× zero-stuff + 2-tap FIR for 8→16, repeated for
/// 16→48 (×3 zero-stuff + FIR). Not a perfect filter but adequate for
/// an audibility test.
pub fn upsample_to_48k(samples: &[f32], src_rate: u32) -> Vec<f32> {
    match src_rate {
        8_000 => upsample(samples, 6),
        12_000 => upsample(samples, 4),
        16_000 => upsample(samples, 3),
        24_000 => upsample(samples, 2),
        48_000 => samples.to_vec(),
        _ => upsample(samples, 48_000 / src_rate),
    }
}

/// Integer-ratio upsample by `factor`, followed by a short low-pass
/// FIR to smear the zero-inserted samples.
fn upsample(samples: &[f32], factor: u32) -> Vec<f32> {
    let f = factor as usize;
    if f <= 1 {
        return samples.to_vec();
    }
    let mut upsampled = vec![0f32; samples.len() * f];
    for (i, &s) in samples.iter().enumerate() {
        upsampled[i * f] = s * (f as f32);
    }
    // Simple symmetric low-pass (hann window, length = 2*f+1).
    let win_len = 2 * f + 1;
    let mut win = vec![0f32; win_len];
    for k in 0..win_len {
        let phase = (k as f32 - f as f32) * core::f32::consts::PI / (f as f32);
        let sinc = if phase.abs() < 1e-6 {
            1.0
        } else {
            phase.sin() / phase
        };
        let hann =
            0.5 - 0.5 * (2.0 * core::f32::consts::PI * k as f32 / (win_len as f32 - 1.0)).cos();
        win[k] = sinc * hann;
    }
    let gain: f32 = win.iter().sum();
    for w in win.iter_mut() {
        *w /= gain;
    }

    let mut out = vec![0f32; upsampled.len()];
    for n in 0..upsampled.len() {
        let mut acc = 0f32;
        for k in 0..win_len {
            let idx = n as i32 + k as i32 - f as i32;
            if idx >= 0 && (idx as usize) < upsampled.len() {
                acc += win[k] * upsampled[idx as usize];
            }
        }
        out[n] = acc;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsample_length() {
        let input = vec![0.0; 160];
        let out = upsample_to_48k(&input, 8_000);
        assert_eq!(out.len(), 960);
    }

    #[test]
    fn upsample_factor_1() {
        let input = vec![1.0, 2.0, 3.0];
        let out = upsample_to_48k(&input, 48_000);
        assert_eq!(out, input);
    }
}
