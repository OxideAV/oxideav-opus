//! SILK §4.2.8 stereo unmixing (mid/side → left/right) — RFC 6716.
//!
//! For stereo SILK streams, the two channels are decoded as a
//! *mid* (M) channel and a *side* (S) channel. After both channels
//! finish their §4.2.7.9 reconstruction (LTP + LPC synthesis,
//! producing the per-channel `out[]` signal), the decoder converts the
//! mid/side representation back into the left/right (LR) representation
//! the application expects. RFC 6716 calls this the
//! `silk_stereo_MS_to_LR` step.
//!
//! The side channel is predicted from two things:
//!
//!   * a simple low-passed version of the mid channel
//!     (`p0 = (mid[i-2] + 2*mid[i-1] + mid[i]) / 4`), and
//!   * the unfiltered mid channel,
//!
//! using the two Q13 prediction weights `(w0_Q13, w1_Q13)` decoded for
//! the *mid* channel in §4.2.7.1. The low-pass filter imposes a
//! one-sample delay, and the unfiltered mid term is also delayed by one
//! sample (it reads `mid[i-1]`, not `mid[i]`), so the reconstruction
//! reaches back two samples into the mid channel and one sample into the
//! side channel relative to the first frame sample.
//!
//! The unmixing runs in two phases (§4.2.8):
//!
//!   1. **Interpolation phase** — for the first `n1` samples
//!      (`64` NB, `96` MB, `128` WB ≈ 8 ms) the weights ramp linearly
//!      from the *previous* frame's weights `(prev_w0_Q13, prev_w1_Q13)`
//!      to the current frame's `(w0_Q13, w1_Q13)`:
//!
//!      ```text
//!            prev_w0_Q13                    (w0_Q13 - prev_w0_Q13)
//!      w0 =  ----------- + min(i - j, n1) * ----------------------
//!              8192.0                             8192.0*n1
//!      ```
//!
//!      (and likewise for `w1`).
//!   2. **Steady phase** — for the remaining `n2 - n1` samples,
//!      `min(i - j, n1) == n1`, so the weights are simply the current
//!      frame's values.
//!
//! The per-sample reconstruction is then:
//!
//! ```text
//!              p0 = (mid[i-2] + 2*mid[i-1] + mid[i]) / 4.0
//!         left[i] = clamp(-1.0, (1 + w1)*mid[i-1] + side[i-1] + w0*p0, 1.0)
//!        right[i] = clamp(-1.0, (1 - w1)*mid[i-1] - side[i-1] - w0*p0, 1.0)
//! ```
//!
//! When the side channel is not coded for this frame (§4.2.7.2 mid-only
//! flag), `side[i]` is taken to be zero everywhere — including the
//! `side[i-1]` history term.
//!
//! The two prior mid samples and one prior side sample carried across
//! the frame boundary live in [`StereoUnmixState`]; on a decoder reset
//! (or for the first frame) they are zero, per the §4.2.8 closing
//! paragraph.
//!
//! Per the §4.2.7.9 preamble, this stage "does not need to be
//! bit-exact"; we follow the spec's floating-point formulation in `f32`.
//!
//! All truth is taken from RFC 6716 §4.2.8 (and §4.2.7.1 for the weight
//! decode). No external library source is consulted.

use crate::toc::Bandwidth;
use crate::Error;

/// Number of samples in the §4.2.8 interpolation phase (`n1`): 64 for
/// NB, 96 for MB, 128 for WB. Roughly 8 ms at each SILK internal rate.
///
/// SWB / FB are SILK-illegal at this stage (the §4.2.2 hybrid split
/// hands SILK only NB/MB/WB), so they are rejected.
pub fn interp_phase_samples(bandwidth: Bandwidth) -> Result<usize, Error> {
    Ok(match bandwidth {
        Bandwidth::Nb => 64,
        Bandwidth::Mb => 96,
        Bandwidth::Wb => 128,
        _ => return Err(Error::MalformedPacket),
    })
}

/// One channel-pair's stereo prediction weights, in Q13 fixed-point, as
/// produced by §4.2.7.1 ([`crate::StereoPredictionWeights`]).
///
/// `silk_stereo_MS_to_LR` consumes the *current* frame's weights and the
/// *previous* frame's weights together (the first `n1` samples
/// interpolate between them). The previous-frame weights are zero on a
/// decoder reset / first frame, mirroring the cleared sample history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StereoWeightsQ13 {
    /// Low-pass prediction weight `w0_Q13`.
    pub w0_q13: i32,
    /// Direct (unfiltered) prediction weight `w1_Q13`.
    pub w1_q13: i32,
}

/// Cross-frame history needed by §4.2.8: the two trailing mid samples
/// and the one trailing side sample from the previous frame, plus the
/// previous frame's prediction weights for the phase-1 interpolation.
///
/// All fields are zero after a decoder reset or for the first frame
/// after one (RFC 6716 §4.2.8: "For the first frame after a decoder
/// reset, zeros are used instead.").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoUnmixState {
    /// `mid[j-2]` and `mid[j-1]` for the upcoming frame (oldest first).
    mid_hist: [f32; 2],
    /// `side[j-1]` for the upcoming frame.
    side_hist: f32,
    /// The previous frame's `(w0_Q13, w1_Q13)` — `prev_w0_Q13` /
    /// `prev_w1_Q13` in the §4.2.8 formulas.
    prev_weights: StereoWeightsQ13,
}

impl Default for StereoUnmixState {
    fn default() -> Self {
        Self::new()
    }
}

impl StereoUnmixState {
    /// A freshly-reset state: cleared mid/side history and zero previous
    /// weights, as required for the first frame after a decoder reset.
    pub fn new() -> Self {
        StereoUnmixState {
            mid_hist: [0.0; 2],
            side_hist: 0.0,
            prev_weights: StereoWeightsQ13::default(),
        }
    }

    /// Reset the state to its post-decoder-reset values (RFC 6716
    /// §4.2.8 / §4.5.2): all sample history and previous weights zeroed.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The previous-frame weights currently held (exposed for tests /
    /// introspection).
    pub fn prev_weights(&self) -> StereoWeightsQ13 {
        self.prev_weights
    }
}

/// One frame of stereo output, two equal-length channel signals nominally
/// in `[-1.0, 1.0]`.
#[derive(Debug, Clone, PartialEq)]
pub struct StereoFrame {
    /// Left channel, length `n2` (the frame sample count).
    pub left: Vec<f32>,
    /// Right channel, same length as [`StereoFrame::left`].
    pub right: Vec<f32>,
}

/// Apply RFC 6716 §4.2.8 stereo unmixing to one frame, converting the
/// decoded mid/side channels into left/right.
///
/// * `bandwidth` selects the §4.2.8 interpolation length `n1`.
/// * `mid` is the mid channel's §4.2.7.9.2 `out[]` for this frame
///   (length `n2`, the total frame sample count).
/// * `side` is the side channel's `out[]`, or `None` when the side
///   channel is not coded for this frame (§4.2.7.2 mid-only flag), in
///   which case `side[i]` is treated as zero everywhere.
/// * `weights` are this frame's §4.2.7.1 weights (`w0_Q13`, `w1_Q13`).
/// * `state` carries the two prior mid samples, one prior side sample,
///   and the previous frame's weights across the frame boundary; it is
///   updated in place ready for the next frame.
///
/// Returns the reconstructed [`StereoFrame`]. Errors if `side` is
/// present but its length differs from `mid`, if `mid` is empty, or if
/// `bandwidth` is SILK-illegal.
pub fn stereo_ms_to_lr(
    bandwidth: Bandwidth,
    mid: &[f32],
    side: Option<&[f32]>,
    weights: StereoWeightsQ13,
    state: &mut StereoUnmixState,
) -> Result<StereoFrame, Error> {
    let n2 = mid.len();
    if n2 == 0 {
        return Err(Error::MalformedPacket);
    }
    if let Some(s) = side {
        if s.len() != n2 {
            return Err(Error::MalformedPacket);
        }
    }
    // n1 is the interpolation-phase length; it must not exceed n2 (the
    // §4.2.8 `min(i - j, n1)` term clamps the ramp regardless, but a
    // very short frame would otherwise never reach the steady phase —
    // which is still correct, just fully-interpolated).
    let n1 = interp_phase_samples(bandwidth)?;

    let prev = state.prev_weights;
    let w0_q13 = weights.w0_q13 as f32;
    let w1_q13 = weights.w1_q13 as f32;
    let prev_w0_q13 = prev.w0_q13 as f32;
    let prev_w1_q13 = prev.w1_q13 as f32;

    // Precompute the per-sample interpolation increment denominators.
    // w0 = prev_w0_Q13/8192 + min(i-j,n1) * (w0_Q13 - prev_w0_Q13)/(8192*n1)
    let n1_f = n1 as f32;
    let w0_base = prev_w0_q13 / 8192.0;
    let w1_base = prev_w1_q13 / 8192.0;
    let w0_step = (w0_q13 - prev_w0_q13) / (8192.0 * n1_f);
    let w1_step = (w1_q13 - prev_w1_q13) / (8192.0 * n1_f);

    let mut left = vec![0.0f32; n2];
    let mut right = vec![0.0f32; n2];

    // History accessors. mid[j-2], mid[j-1] come from the carried state;
    // side[j-1] likewise. With j == 0 (each frame's local index space),
    // "i-2" / "i-1" for i in 0..2 reach into the history.
    let mid_m2 = state.mid_hist[0]; // mid[j-2]
    let mid_m1 = state.mid_hist[1]; // mid[j-1]
    let side_m1 = if side.is_some() {
        state.side_hist
    } else {
        // Side not coded → side[i] = 0 everywhere, including history.
        0.0
    };

    for i in 0..n2 {
        // Interpolated weights for this sample. min(i - j, n1) with j=0.
        let ramp = (i.min(n1)) as f32;
        let w0 = w0_base + ramp * w0_step;
        let w1 = w1_base + ramp * w1_step;

        // mid[i], mid[i-1], mid[i-2] with i-1 / i-2 dipping into history.
        let m_i = mid[i];
        let m_i1 = if i >= 1 { mid[i - 1] } else { mid_m1 };
        let m_i2 = match i {
            0 => mid_m2,
            1 => mid_m1,
            _ => mid[i - 2],
        };

        // side[i-1] with i-1 dipping into history (or 0 if uncoded).
        let s_i1 = match side {
            Some(s) if i >= 1 => s[i - 1],
            Some(_) => side_m1,
            None => 0.0,
        };

        // p0 = (mid[i-2] + 2*mid[i-1] + mid[i]) / 4.0
        let p0 = (m_i2 + 2.0 * m_i1 + m_i) / 4.0;

        // left[i]  = clamp(-1, (1 + w1)*mid[i-1] + side[i-1] + w0*p0, 1)
        // right[i] = clamp(-1, (1 - w1)*mid[i-1] - side[i-1] - w0*p0, 1)
        let l = (1.0 + w1) * m_i1 + s_i1 + w0 * p0;
        let r = (1.0 - w1) * m_i1 - s_i1 - w0 * p0;
        left[i] = l.clamp(-1.0, 1.0);
        right[i] = r.clamp(-1.0, 1.0);
    }

    // Carry the trailing samples + current weights into the next frame.
    // mid history: the last two samples of this frame.
    state.mid_hist = if n2 >= 2 {
        [mid[n2 - 2], mid[n2 - 1]]
    } else {
        // n2 == 1: shift the old most-recent into the older slot.
        [mid_m1, mid[n2 - 1]]
    };
    // side history: last sample of this frame, or 0 if uncoded.
    state.side_hist = match side {
        Some(s) => s[n2 - 1],
        None => 0.0,
    };
    state.prev_weights = weights;

    Ok(StereoFrame { left, right })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!(
            (a - b).abs() < 1e-5,
            "expected {b}, got {a} (delta {})",
            (a - b).abs()
        );
    }

    #[test]
    fn interp_phase_table() {
        assert_eq!(interp_phase_samples(Bandwidth::Nb).unwrap(), 64);
        assert_eq!(interp_phase_samples(Bandwidth::Mb).unwrap(), 96);
        assert_eq!(interp_phase_samples(Bandwidth::Wb).unwrap(), 128);
        assert!(interp_phase_samples(Bandwidth::Swb).is_err());
        assert!(interp_phase_samples(Bandwidth::Fb).is_err());
    }

    #[test]
    fn state_starts_and_resets_zero() {
        let mut s = StereoUnmixState::new();
        assert_eq!(s.mid_hist, [0.0, 0.0]);
        assert_eq!(s.side_hist, 0.0);
        assert_eq!(s.prev_weights, StereoWeightsQ13::default());
        s.mid_hist = [0.3, -0.2];
        s.side_hist = 0.1;
        s.prev_weights = StereoWeightsQ13 {
            w0_q13: 5,
            w1_q13: 7,
        };
        s.reset();
        assert_eq!(s, StereoUnmixState::new());
    }

    #[test]
    fn rejects_empty_and_mismatched() {
        let mut s = StereoUnmixState::new();
        assert!(stereo_ms_to_lr(
            Bandwidth::Wb,
            &[],
            None,
            StereoWeightsQ13::default(),
            &mut s
        )
        .is_err());
        let mid = vec![0.0f32; 80];
        let side = vec![0.0f32; 79];
        assert!(stereo_ms_to_lr(
            Bandwidth::Wb,
            &mid,
            Some(&side),
            StereoWeightsQ13::default(),
            &mut s
        )
        .is_err());
    }

    /// With zero weights and no side channel, the unmixer collapses to
    /// `left[i] = right[i] = mid[i-1]` (a one-sample delay), and the
    /// first sample reads the zeroed mid history → 0.
    #[test]
    fn zero_weights_no_side_is_delayed_mono() {
        let mut s = StereoUnmixState::new();
        let mid: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4];
        let out = stereo_ms_to_lr(
            Bandwidth::Wb,
            &mid,
            None,
            StereoWeightsQ13::default(),
            &mut s,
        )
        .unwrap();
        // left[i] = right[i] = mid[i-1]; mid[-1] = 0.
        let expect = [0.0, 0.1, 0.2, 0.3];
        for (i, &e) in expect.iter().enumerate() {
            approx(out.left[i], e);
            approx(out.right[i], e);
        }
        // L == R when there's no side / weights.
        assert_eq!(out.left, out.right);
    }

    /// Hand-computed reference: WB frame, constant non-zero weights so
    /// the phase-1 ramp is flat (prev == current), a coded side channel,
    /// and a handful of mid samples. We reproduce the §4.2.8 formulas by
    /// hand and check the unmixer matches.
    #[test]
    fn known_midside_reconstruction_constant_weights() {
        // Use prev == current so min(i,n1)*step term vanishes and the
        // weights are constant w0 = w0_Q13/8192, w1 = w1_Q13/8192.
        let w = StereoWeightsQ13 {
            w0_q13: 4096, // -> w0 = 0.5
            w1_q13: 8192, // -> w1 = 1.0
        };
        let mut s = StereoUnmixState::new();
        s.prev_weights = w; // flat ramp

        let mid = vec![0.4f32, -0.2, 0.1, 0.3, -0.1];
        let side = vec![0.05f32, 0.0, -0.1, 0.2, 0.1];

        let out = stereo_ms_to_lr(Bandwidth::Wb, &mid, Some(&side), w, &mut s).unwrap();

        let w0 = 0.5f32;
        let w1 = 1.0f32;
        // History is all zero (fresh state for mid[-1], mid[-2], side[-1]).
        let mut mhist = [0.0f32, 0.0]; // [mid[i-2], mid[i-1]] sliding
        let mut shist = 0.0f32; // side[i-1]
        for i in 0..mid.len() {
            let m_i = mid[i];
            let m_i1 = mhist[1];
            let m_i2 = mhist[0];
            let s_i1 = shist;
            let p0 = (m_i2 + 2.0 * m_i1 + m_i) / 4.0;
            let l = ((1.0 + w1) * m_i1 + s_i1 + w0 * p0).clamp(-1.0, 1.0);
            let r = ((1.0 - w1) * m_i1 - s_i1 - w0 * p0).clamp(-1.0, 1.0);
            approx(out.left[i], l);
            approx(out.right[i], r);
            // slide
            mhist = [m_i1, m_i];
            shist = side[i];
        }
    }

    /// The phase-1 weight ramp: with prev != current weights, the first
    /// `n1` samples interpolate linearly. We verify the effective weight
    /// at sample 0 equals prev/8192 and at sample >= n1 equals cur/8192.
    /// We isolate w1 by using a unit mid impulse delayed one sample and
    /// zero w0 / side.
    #[test]
    fn phase1_ramp_endpoints() {
        // NB so n1 = 64; make a frame long enough to reach the steady
        // phase. w0 = 0 to drop the p0 term. Side uncoded.
        let n1 = 64usize;
        let n2 = n1 + 4;
        let w_cur = StereoWeightsQ13 {
            w0_q13: 0,
            w1_q13: 8192,
        }; // cur w1 = 1.0
        let mut s = StereoUnmixState::new();
        s.prev_weights = StereoWeightsQ13 {
            w0_q13: 0,
            w1_q13: 0,
        }; // prev w1 = 0.0

        // mid constant 0.4 so mid[i-1] = 0.4 for i >= 1 (small enough
        // that the ramped left term stays inside the clamp range).
        let m = 0.4f32;
        let mid = vec![m; n2];
        let out = stereo_ms_to_lr(Bandwidth::Nb, &mid, None, w_cur, &mut s).unwrap();

        // left[i] = (1 + w1(i)) * mid[i-1]; with mid[i-1]=0.4 for i>=1.
        // w1(i) = 0 + min(i, n1) * (1.0 - 0)/n1 = min(i,n1)/n1.
        // i = 1 → w1 = 1/64 → left = (1 + 1/64)*0.4.
        approx(out.left[1], (1.0 + 1.0 / 64.0) * m);
        // i = n1 → w1 = 1.0 → left = 2.0*0.4 = 0.8 (still in range).
        approx(out.left[n1], 2.0 * m);
        // steady region (i = n1 + 1) → w1 = 1.0 → 0.8.
        approx(out.left[n1 + 1], 2.0 * m);
        // right[i] = (1 - w1) * mid[i-1]; at i=1 → (1 - 1/64)*0.4.
        approx(out.right[1], (1.0 - 1.0 / 64.0) * m);
        // right at steady → (1 - 1.0)*0.4 = 0.
        approx(out.right[n1 + 1], 0.0);
    }

    /// History carries across frame boundaries: the second frame's
    /// first sample must read the previous frame's trailing mid / side
    /// samples and weights, not zero.
    #[test]
    fn history_carries_across_frames() {
        let w = StereoWeightsQ13 {
            w0_q13: 0,
            w1_q13: 0,
        }; // pure delay, L == R == mid[i-1]
        let mut s = StereoUnmixState::new();
        s.prev_weights = w;

        let frame1 = vec![0.1f32, 0.2, 0.3, 0.4];
        let _ = stereo_ms_to_lr(Bandwidth::Wb, &frame1, None, w, &mut s).unwrap();
        // After frame1 the mid history holds [0.3, 0.4].
        assert_eq!(s.mid_hist, [0.3, 0.4]);

        let frame2 = vec![0.5f32, 0.6, 0.7, 0.8];
        let out2 = stereo_ms_to_lr(Bandwidth::Wb, &frame2, None, w, &mut s).unwrap();
        // frame2 left[0] = mid[-1] = last sample of frame1 = 0.4.
        approx(out2.left[0], 0.4);
        approx(out2.left[1], 0.5);
    }

    /// Side history carries across frames too (for a coded side channel),
    /// and adds/subtracts symmetrically into L/R.
    #[test]
    fn side_history_carries_across_frames() {
        let w = StereoWeightsQ13 {
            w0_q13: 0,
            w1_q13: 0,
        };
        let mut s = StereoUnmixState::new();
        s.prev_weights = w;

        let mid1 = vec![0.0f32; 4];
        let side1 = vec![0.1f32, 0.2, 0.3, 0.4];
        let _ = stereo_ms_to_lr(Bandwidth::Wb, &mid1, Some(&side1), w, &mut s).unwrap();
        assert_eq!(s.side_hist, 0.4);

        let mid2 = vec![0.0f32; 4];
        let side2 = vec![0.5f32, 0.6, 0.7, 0.8];
        let out2 = stereo_ms_to_lr(Bandwidth::Wb, &mid2, Some(&side2), w, &mut s).unwrap();
        // mid all zero, w=0 → left[i] = side[i-1], right[i] = -side[i-1].
        // left[0] = side[-1] = last of side1 = 0.4.
        approx(out2.left[0], 0.4);
        approx(out2.right[0], -0.4);
        approx(out2.left[1], 0.5);
        approx(out2.right[1], -0.5);
    }

    /// Clamping: drive both L and R out of range and confirm the
    /// `[-1.0, 1.0]` clamp from §4.2.8 is applied.
    #[test]
    fn output_is_clamped() {
        let w = StereoWeightsQ13 {
            w0_q13: 8192 * 4, // huge w0
            w1_q13: 8192 * 4, // huge w1
        };
        let mut s = StereoUnmixState::new();
        s.prev_weights = w;
        let mid = vec![1.0f32; 8];
        let side = vec![1.0f32; 8];
        let out = stereo_ms_to_lr(Bandwidth::Wb, &mid, Some(&side), w, &mut s).unwrap();
        for i in 0..8 {
            assert!((-1.0..=1.0).contains(&out.left[i]), "left {}", out.left[i]);
            assert!(
                (-1.0..=1.0).contains(&out.right[i]),
                "right {}",
                out.right[i]
            );
        }
    }
}
