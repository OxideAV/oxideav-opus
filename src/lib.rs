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
//!
//! Actual SILK / CELT frame decoding is not yet wired up; the
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
pub mod toc;

pub use frames::{OpusPacket, MAX_FRAMES_PER_PACKET, MAX_FRAME_BYTES};
pub use range_decoder::RangeDecoder;
pub use toc::{Bandwidth, ChannelMapping, FrameCountCode, Mode, OpusTocByte};

/// No-op codec registration — the orphan-rebuild scaffold registers
/// nothing into the runtime context until decode / encode paths are
/// wired up.
pub fn register(_ctx: &mut RuntimeContext) {}

oxideav_core::register!("opus", register);
