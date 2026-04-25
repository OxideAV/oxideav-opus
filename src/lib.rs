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
//! Additional decoder modes:
//!
//! * Hybrid (SILK+CELT) frames (§4.4) — SILK WB body + CELT high band
//!   with a start_band offset (17 for FB, 14 for SWB), summed.
//! * SILK LBRR redundancy — flags are parsed and the redundancy
//!   bodies decode-and-discard via a scratch SILK state so the range
//!   coder stays aligned; loss-free output is unaffected.
//! * Channel mapping family 1 (Vorbis surround) and family 2
//!   (ambisonics) via the `MultistreamDecoder`.
//!
//! Encoder scope — see `encoder` module docs. Today the module exposes:
//!
//! * `OpusEncoder` — CELT-only Fullband mono/stereo 20 ms at 48 kHz.
//! * `SilkEncoder` — SILK-only Narrowband mono 20 ms (config 1),
//!   accepting 8 kHz or 48 kHz input. Round-trips through our own
//!   decoder at ≥ 20 dB SNR on speech-like signals.

pub mod decoder;
pub mod encoder;
pub mod header;
pub mod silk;
pub mod toc;

use oxideav_core::{CodecCapabilities, CodecId, CodecParameters, CodecTag, Result};
use oxideav_core::{CodecInfo, CodecRegistry, Decoder, Encoder};

pub const CODEC_ID_STR: &str = "opus";

pub fn register(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::audio("opus_sw")
        .with_lossy(true)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);
    // AVI / WAVEFORMATEX tags — Opus in AVI is non-standard but a few
    // third-party muxers have stamped these wFormatTag values on Opus
    // streams.
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .tags([
                CodecTag::wave_format(0x4F70),
                CodecTag::wave_format(0x704F),
                CodecTag::wave_format(0x7075),
            ]),
    );
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    decoder::make_decoder(params)
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    encoder::make_encoder(params)
}

pub use decoder::MultistreamOpusDecoder;
pub use header::{parse_opus_head, OpusHead};
pub use toc::{
    parse_packet, parse_self_delimited_packet, OpusBandwidth, OpusMode, OpusPacket, Toc,
};
