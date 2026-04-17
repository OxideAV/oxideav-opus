#![allow(
    dead_code,
    clippy::needless_range_loop,
    clippy::excessive_precision,
    clippy::useless_vec,
    clippy::too_many_arguments,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::nonminimal_bool,
    clippy::manual_range_contains,
    clippy::needless_late_init,
    clippy::needless_return,
    clippy::let_unit_value,
    clippy::needless_borrow,
    unused_mut,
    unused_variables,
    unused_assignments,
    clippy::unnecessary_cast,
    clippy::manual_memcpy,
    clippy::neg_multiply,
    clippy::precedence
)]

//! Opus audio codec (RFC 6716 bitstream, RFC 7845 in-Ogg mapping).
//!
//! Decoder scope (see `decoder` module docs for the nitty-gritty):
//!
//! * `OpusHead` identification-packet parsing (RFC 7845 §5.1).
//! * Full TOC byte + framing code 0/1/2/3 packet parser (RFC 6716 §3).
//! * CELT-only frames at any bandwidth (NB/WB/SWB/FB), mono + stereo,
//!   2.5/5/10/20 ms — full §4.3 pipeline (range decode, coarse+fine+
//!   final band energy, bit allocation, PVQ shape, anti-collapse,
//!   IMDCT with overlap-add, comb post-filter).
//! * SILK-only frames at NB/MB/WB, mono + stereo, 10/20/40/60 ms —
//!   §4.2 pipeline (NLSF → LPC, LTP for voiced, excitation, synthesis,
//!   stereo MS→LR unmix, 8/12/16 → 48 kHz upsample).
//! * Silence / DTX frames (0 / 1-byte packets) produce correct-length
//!   silence output.
//!
//! Decoder gaps that still return `Unsupported`:
//!
//! * Hybrid (SILK+CELT) frames — §4.2 + §4.3 need to share a packet.
//! * SILK LBRR redundancy data — flags are parsed so the range coder
//!   stays aligned, but the redundancy frames themselves are not yet
//!   decoded; any packet that sets a LBRR flag is rejected rather than
//!   silently dropping samples.
//! * Channel mapping family 1/2 (Vorbis / ambisonic multistream).
//!
//! Encoder scope — see `encoder` module docs.

pub mod decoder;
pub mod encoder;
pub mod header;
pub mod silk;
pub mod toc;

use oxideav_codec::{CodecRegistry, Decoder, Encoder};
use oxideav_core::{CodecCapabilities, CodecId, CodecParameters, Result};

pub const CODEC_ID_STR: &str = "opus";

pub fn register(reg: &mut CodecRegistry) {
    let cid = CodecId::new(CODEC_ID_STR);
    let caps = CodecCapabilities::audio("opus_sw")
        .with_lossy(true)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);
    reg.register_both(cid, caps, make_decoder, make_encoder);
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    decoder::make_decoder(params)
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    encoder::make_encoder(params)
}

pub use header::{parse_opus_head, OpusHead};
pub use toc::{parse_packet, OpusBandwidth, OpusMode, OpusPacket, Toc};
