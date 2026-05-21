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
//!   weight-list. Output stops at `res_Q10[]`; §4.2.7.5.3 codebook
//!   reconstruction, IHMW weighting, §4.2.7.5.4 stabilization, and
//!   §4.2.7.5.5 interpolation are deferred to round 7+.
//!
//! Subsequent SILK stages (LSF stabilization §4.2.7.5.4, LSF
//! interpolation §4.2.7.5.5, LTP §4.2.7.6, LCG seed §4.2.7.7,
//! excitation §4.2.7.8) and the CELT layer are not yet wired up; the
//! [`Decoder`] / [`Encoder`] entry points still return
//! [`Error::NotImplemented`].

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
pub mod silk_frame;
pub mod silk_gains;
pub mod silk_lsf_stage2;
pub mod toc;

pub use frames::{OpusPacket, MAX_FRAMES_PER_PACKET, MAX_FRAME_BYTES};
pub use range_decoder::RangeDecoder;
pub use silk_frame::{
    FrameKind, QuantizationOffsetType, SignalType, SilkFrameHeader, SilkFrameHeaderConfig,
    StereoPredictionWeights,
};
pub use silk_gains::{SubframeGain, SubframeGains, SubframeGainsConfig, SILK_MAX_SUBFRAMES};
pub use silk_lsf_stage2::{
    LsfStage2, D_LPC_MAX, D_LPC_NB_MB, D_LPC_WB, QSTEP_NB_MB_Q16, QSTEP_WB_Q16,
};
pub use toc::{Bandwidth, ChannelMapping, FrameCountCode, Mode, OpusTocByte};

/// No-op codec registration — the orphan-rebuild scaffold registers
/// nothing into the runtime context until decode / encode paths are
/// wired up.
pub fn register(_ctx: &mut RuntimeContext) {}

oxideav_core::register!("opus", register);
