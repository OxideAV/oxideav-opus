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
    let _ = ltp_scale_q14;
    let _ = interp_coef;
    let frame_len = excitation.len();
    let mut out = vec![0f32; frame_len];

    // Ensure history buffers are large enough.
    if state.lpc_history.len() < lpc_order {
        state.lpc_history.resize(lpc_order, 0.0);
    }
    let ltp_hist_len = 480usize;
    if state.ltp_history.len() < ltp_hist_len {
        state.ltp_history.resize(ltp_hist_len, 0.0);
    }

    // Per RFC §4.2.7.9.2 the LPC ring keeps the *unclamped* lpc[i]
    // values for next-subframe feedback, while the *clamped* out[i] is
    // exported. Splitting them is needed to keep the IIR behaviour the
    // RFC specifies — feeding clamped values back into the filter
    // under-drives the synthesis whenever the unclamped values exceed ±1
    // (common when `g` is large or the NLSF template is mismatched to
    // the source signal).
    let mut lpc_ring = vec![0.0f32; frame_len];

    for sf in 0..n_subframes {
        let sf_start = sf * subframe_len;
        let sf_end = sf_start + subframe_len;
        let lpc = &lpc_per_sf[sf];
        // Overall short-term LPC gain for this sub-frame (Q16 → f32).
        // RFC §4.2.7.9.2: lpc[i] = (gain_Q16[s]/65536) * res[i] + …
        let g = gains_q16[sf].max(1) as f32 / 65536.0;
        let taps = &ltp_filter[sf];
        let lag = pitch_lags[sf];

        for n in sf_start..sf_end {
            // RFC §4.2.7.9.1 voiced residual: res[i] = e_Q23[i]/2^23 +
            // sum_{k=0..5} res[i - pitch_lag + 2 - k] * b_Q7[k]/128.
            // Apply the gain to the excitation first; the LTP feedback is
            // added at unit weight because our MVP reads `past` from the
            // post-LPC `out[]` buffer (which is approximately `g * res[]`),
            // so multiplying again by `g` would re-apply the gain and make
            // the LTP loop unstable. This approximation is audibility-correct
            // for an MVP (strict spec-bit-exactness requires a separate
            // res[] ring and rewhitening, deferred).
            let mut s = g * excitation[n];
            if voiced && lag > 0 {
                for k in 0..5 {
                    // RFC §4.2.7.9.1: res[i - pitch_lag + 2 - k].
                    let idx = n as i32 - lag + 2 - k as i32;
                    let past = if idx >= 0 && (idx as usize) < n {
                        out[idx as usize]
                    } else if idx >= 0 {
                        0.0
                    } else {
                        let hi = (ltp_hist_len as i32 + idx) as usize;
                        state.ltp_history.get(hi).copied().unwrap_or(0.0)
                    };
                    s += taps[k] * past;
                }
            }

            // Short-term all-pole LPC synthesis. RFC 6716 §4.2.7.9.2
            // feeds the *unclamped* lpc[i] (pre-clamp) into the sum
            // for next-sample prediction.
            for k in 1..=lpc_order {
                let idx = n as i32 - k as i32;
                let past = if idx >= 0 {
                    lpc_ring[idx as usize]
                } else {
                    let h_idx = (state.lpc_history.len() as i32 + idx) as usize;
                    state.lpc_history.get(h_idx).copied().unwrap_or(0.0)
                };
                s += lpc[k - 1] * past;
            }
            lpc_ring[n] = s;
            // RFC §4.2.7.9.2: out[i] = clamp(-1.0, lpc[i], 1.0).
            out[n] = s.clamp(-1.0, 1.0);
        }
    }

    // Update state history for next frame — RFC §4.2.7.9.2 carries the
    // *unclamped* lpc_ring values into the next subframe's LPC sum.
    let lpc_keep = lpc_order.min(lpc_ring.len());
    state.lpc_history = lpc_ring[lpc_ring.len() - lpc_keep..].to_vec();

    // Shift LTP history.
    let keep = ltp_hist_len.saturating_sub(frame_len);
    let mut new_ltp = Vec::with_capacity(ltp_hist_len);
    new_ltp.extend_from_slice(&state.ltp_history[ltp_hist_len - keep..]);
    new_ltp.extend_from_slice(&out);
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
