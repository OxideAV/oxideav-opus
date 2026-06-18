//! Top-level Opus packet → PCM orchestration — RFC 6716 §3 / §4.
//!
//! This module is the keystone that turns a raw Opus packet (a TOC byte
//! plus one or more §3.2-packed Opus frames) into interleaved 48 kHz PCM
//! samples. It sits above every per-stage SILK / CELT decoder in the
//! crate and wires the §3.1 TOC parse, the §3.2 frame packing
//! ([`crate::frames::OpusPacket`]), and the §4.2 / §4.3 per-frame mode
//! dispatch ([`crate::framing::OpusFrameRouting`]) into one
//! [`OpusDecoder::decode_packet`] call.
//!
//! ## What this module owns
//!
//! * The packet → frame split (delegated to [`OpusPacket::parse`]).
//! * The §4.5 multi-frame loop: every Opus frame in a code-1 / code-2 /
//!   code-3 packet is decoded in order and its PCM appended to the
//!   output, so a 60 ms code-3 packet of three 20 ms frames yields one
//!   contiguous PCM buffer.
//! * The §3.2.1 DTX / lost-frame marker handling: a zero-length frame
//!   slice contributes one Opus-frame worth of silence (the §4.6 PLC
//!   "fill with silence" floor — a real concealment model is a separate
//!   milestone).
//! * The 48 kHz output sample-count accounting (RFC 7845 §5.1: the Opus
//!   decoder always emits 48 kHz regardless of the internal SILK / CELT
//!   sample rate).
//! * The per-frame routing seam: each Opus frame is dispatched to
//!   [`Self::decode_silk_only_frame`], [`Self::decode_celt_only_frame`],
//!   or [`Self::decode_hybrid_frame`] based on its [`OpusFrameRouting`].
//!
//! ## What this module does not own
//!
//! * The §4.1 range-coder primitive ([`crate::range_decoder`]).
//! * The per-stage SILK / CELT decode (the `silk_*` / `celt_*` modules).
//! * Any container parsing (Ogg / RTP framing live in their own crates;
//!   this module consumes a bare Opus packet).
//!
//! ## Status of the per-frame audio decode
//!
//! The packet-level orchestration (TOC → framing → routing → 48 kHz PCM
//! buffer layout) is complete and total over all 32 §3.1 configs and all
//! four §3.2 frame-count codes. The per-frame audio decode is wired
//! incrementally: a frame whose mode is not yet composed into a sample-
//! producing path emits silence of the correct length and flags the
//! reason in [`FrameDecodeStatus`], so the multi-frame packet loop and
//! the PCM sample-count accounting are exercised end-to-end regardless of
//! which layer's range-coded decode has landed.

use crate::frames::OpusPacket;
use crate::framing::{OperatingMode, OpusFrameRouting};
use crate::toc::ChannelMapping;
use crate::Error;

/// Output sample rate of the Opus decoder, in Hz. Per RFC 7845 §5.1 the
/// decoder always emits 48 kHz regardless of the internal SILK / CELT
/// sample rate; the per-layer resamplers upsample to this rate.
pub const OUTPUT_SAMPLE_RATE_HZ: u32 = 48_000;

/// Output samples per millisecond per channel at [`OUTPUT_SAMPLE_RATE_HZ`].
pub const OUTPUT_SAMPLES_PER_MS: u32 = OUTPUT_SAMPLE_RATE_HZ / 1000;

/// Number of 48 kHz output samples (per channel) an Opus frame of the
/// given duration produces.
///
/// `frame_size_tenths_ms` is the §3.1 Table 2 duration in tenths of a
/// millisecond (25, 50, 100, 200, 400, 600). The 2.5 ms CELT case
/// (`25` tenths) yields `25 * 48 / 10 = 120` samples per channel, which
/// is exact; all six durations divide evenly.
pub fn output_samples_per_channel(frame_size_tenths_ms: u16) -> usize {
    // tenths-ms * (48 samples / ms) / 10 = tenths-ms * 48 / 10.
    (frame_size_tenths_ms as usize * OUTPUT_SAMPLES_PER_MS as usize) / 10
}

/// Why a given Opus frame produced the samples it did.
///
/// The packet-level orchestration is complete, but the per-frame audio
/// decode lands incrementally. This status lets a caller (and the
/// crate's own tests) distinguish "decoded real audio" from "emitted
/// silence because the layer's range-coded decode is not wired yet" or
/// "emitted silence for a DTX / lost frame".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecodeStatus {
    /// A §3.2.1 zero-length frame: DTX or a lost/packet-loss marker.
    /// Per §4.6 the floor behaviour is to emit silence; a real PLC model
    /// is a separate milestone.
    DtxOrLost,
    /// The frame's operating mode does not yet have a composed
    /// sample-producing decode path in this crate, so silence of the
    /// correct length was emitted. The variant carries the mode so the
    /// caller knows which layer is pending.
    LayerNotWired(OperatingMode),
}

/// The result of decoding one Opus frame: how many per-channel samples
/// it contributed and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameOutcome {
    /// Per-channel 48 kHz sample count this frame contributed.
    pub samples_per_channel: usize,
    /// Provenance of the samples (real audio vs silence and why).
    pub status: FrameDecodeStatus,
}

/// Decoded audio for one Opus packet: interleaved 48 kHz PCM plus the
/// per-frame outcomes.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    /// Interleaved signed 16-bit PCM at 48 kHz. For stereo the layout is
    /// `[L0, R0, L1, R1, …]`; for mono it is `[S0, S1, …]`. Length is
    /// `total_samples_per_channel * channels`.
    pub pcm: Vec<i16>,
    /// Number of audio channels (1 for mono, 2 for stereo).
    pub channels: u8,
    /// Output sample rate in Hz (always [`OUTPUT_SAMPLE_RATE_HZ`]).
    pub sample_rate_hz: u32,
    /// Per-Opus-frame outcomes, in packet order. `outcomes.len()` equals
    /// the packet's §3.2 frame count.
    pub frame_outcomes: Vec<FrameOutcome>,
}

impl DecodedAudio {
    /// Total per-channel 48 kHz sample count across every Opus frame in
    /// the packet.
    pub fn samples_per_channel(&self) -> usize {
        self.pcm.len() / self.channels.max(1) as usize
    }
}

/// Stateful Opus packet → PCM decoder.
///
/// One [`OpusDecoder`] is fed Opus packets in stream order via
/// [`Self::decode_packet`]. The decoder is stateful because the SILK and
/// CELT layers carry inter-frame state (LPC / LTP history, MDCT overlap,
/// stereo unmixing memory, the §4.5.2 reset policy); the state lives here
/// and is threaded into the per-frame decode as those paths land. Today
/// the carried state is minimal (it grows as each layer is wired), but
/// the type is the stable home for it.
#[derive(Debug, Default)]
pub struct OpusDecoder {
    /// Channel count of the most recently decoded packet, if any. Used
    /// only for the §4.5.2 mono↔stereo transition reset bookkeeping the
    /// per-layer decoders will consult once wired.
    last_channels: Option<u8>,
}

impl OpusDecoder {
    /// Construct a fresh decoder with no carried state (equivalent to the
    /// post-`reset` state of §4.5.2).
    pub fn new() -> Self {
        Self::default()
    }

    /// Discard all inter-frame state, as after a container seek (the
    /// §4.5.2 decoder reset). Leaves the decoder ready to decode a new
    /// bitstream position as if it were the first packet.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Decode one complete Opus packet into interleaved 48 kHz PCM.
    ///
    /// Performs the §3.1 TOC parse, the §3.2 frame split, and the §4.5
    /// multi-frame loop, dispatching each Opus frame through its
    /// [`OpusFrameRouting`] to the matching per-mode decode. Returns
    /// [`Error::EmptyPacket`] for a zero-length packet (§3.1 R1) and
    /// [`Error::MalformedPacket`] for any §3.2 framing violation.
    pub fn decode_packet(&mut self, packet: &[u8]) -> Result<DecodedAudio, Error> {
        let parsed = OpusPacket::parse(packet)?;
        let routing = OpusFrameRouting::from_toc(parsed.toc);
        let channels = routing.channel_count();
        let per_frame_samples = output_samples_per_channel(routing.frame_size_tenths_ms);

        self.last_channels = Some(channels);

        let frame_slices = parsed.frames();
        let mut pcm: Vec<i16> =
            Vec::with_capacity(frame_slices.len() * per_frame_samples * channels as usize);
        let mut frame_outcomes = Vec::with_capacity(frame_slices.len());

        for frame in frame_slices {
            let outcome = self.decode_one_frame(frame, &routing, &mut pcm);
            frame_outcomes.push(outcome);
        }

        Ok(DecodedAudio {
            pcm,
            channels,
            sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
            frame_outcomes,
        })
    }

    /// Decode one Opus frame, appending its interleaved 48 kHz PCM to
    /// `pcm` and returning the per-frame outcome.
    fn decode_one_frame(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        let channels = routing.channel_count();

        // §3.2.1 zero-length frame: DTX / lost. §4.6 floor = silence.
        if frame.is_empty() {
            push_silence(pcm, per_channel, channels);
            return FrameOutcome {
                samples_per_channel: per_channel,
                status: FrameDecodeStatus::DtxOrLost,
            };
        }

        match routing.operating_mode {
            OperatingMode::SilkOnly => self.decode_silk_only_frame(frame, routing, pcm),
            OperatingMode::CeltOnly => self.decode_celt_only_frame(frame, routing, pcm),
            OperatingMode::Hybrid => self.decode_hybrid_frame(frame, routing, pcm),
        }
    }

    /// Decode one SILK-only Opus frame (§4.2). Currently emits silence of
    /// the correct length and flags [`OperatingMode::SilkOnly`] as not yet
    /// composed; the per-stage SILK decoders exist but the Table-5 in-order
    /// range-coded composition is wired in a follow-up.
    fn decode_silk_only_frame(
        &mut self,
        _frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        push_silence(pcm, per_channel, routing.channel_count());
        FrameOutcome {
            samples_per_channel: per_channel,
            status: FrameDecodeStatus::LayerNotWired(OperatingMode::SilkOnly),
        }
    }

    /// Decode one CELT-only Opus frame (§4.3). Currently emits silence;
    /// the §4.3.2.1 coarse-energy Laplace decode (the first range-coded
    /// CELT field) is the documented gating gap.
    fn decode_celt_only_frame(
        &mut self,
        _frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        push_silence(pcm, per_channel, routing.channel_count());
        FrameOutcome {
            samples_per_channel: per_channel,
            status: FrameDecodeStatus::LayerNotWired(OperatingMode::CeltOnly),
        }
    }

    /// Decode one Hybrid Opus frame (§4.2 SILK + §4.3 CELT). Currently
    /// emits silence; depends on both layer paths landing.
    fn decode_hybrid_frame(
        &mut self,
        _frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        push_silence(pcm, per_channel, routing.channel_count());
        FrameOutcome {
            samples_per_channel: per_channel,
            status: FrameDecodeStatus::LayerNotWired(OperatingMode::Hybrid),
        }
    }
}

/// Append `per_channel * channels` interleaved zero samples to `pcm`.
fn push_silence(pcm: &mut Vec<i16>, per_channel: usize, channels: u8) {
    pcm.resize(pcm.len() + per_channel * channels as usize, 0);
}

/// Convenience: the channel count for a [`ChannelMapping`].
pub fn channel_count(mapping: ChannelMapping) -> u8 {
    match mapping {
        ChannelMapping::Mono => 1,
        ChannelMapping::Stereo => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toc::OpusTocByte;

    /// Build a minimal code-0 packet: TOC byte + a non-empty single
    /// frame body. `config` is the 5-bit §3.1 config, `stereo` the s bit.
    fn code0_packet(config: u8, stereo: bool, body: &[u8]) -> Vec<u8> {
        let toc = (config << 3) | (if stereo { 1 << 2 } else { 0 });
        let mut p = vec![toc];
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn output_samples_per_channel_matches_table2_durations() {
        // (tenths-ms, expected 48 kHz samples/channel)
        let cases = [
            (25u16, 120usize), // 2.5 ms CELT
            (50, 240),         // 5 ms
            (100, 480),        // 10 ms
            (200, 960),        // 20 ms
            (400, 1920),       // 40 ms
            (600, 2880),       // 60 ms
        ];
        for (tenths, expected) in cases {
            assert_eq!(
                output_samples_per_channel(tenths),
                expected,
                "tenths={tenths}"
            );
        }
    }

    #[test]
    fn empty_packet_rejected() {
        let mut dec = OpusDecoder::new();
        assert_eq!(dec.decode_packet(&[]), Err(Error::EmptyPacket));
    }

    #[test]
    fn silk_nb_mono_20ms_single_frame_pcm_length() {
        // config 1 = SILK NB 20 ms (200 tenths-ms), mono, code 0.
        let pkt = code0_packet(1, false, &[0x12, 0x34, 0x56]);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.channels, 1);
        assert_eq!(out.sample_rate_hz, OUTPUT_SAMPLE_RATE_HZ);
        assert_eq!(out.samples_per_channel(), 960);
        assert_eq!(out.pcm.len(), 960);
        assert_eq!(out.frame_outcomes.len(), 1);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::LayerNotWired(OperatingMode::SilkOnly)
        );
    }

    #[test]
    fn celt_only_stereo_pcm_is_interleaved_length() {
        // config 20 = CELT-only, second size in the NB/WB group; stereo.
        let pkt = code0_packet(20, true, &[0xaa, 0xbb]);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.channels, 2);
        // 2 channels interleaved => pcm len = 2 * samples_per_channel.
        assert_eq!(out.pcm.len(), 2 * out.samples_per_channel());
        let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::LayerNotWired(OperatingMode::CeltOnly)
        );
        assert_eq!(routing.operating_mode, OperatingMode::CeltOnly);
    }

    #[test]
    fn code1_two_equal_frames_concatenate_pcm() {
        // config 0 = SILK NB 10 ms (100 tenths => 480 samples/ch), mono.
        // Code 1 = two equal frames; body must be even length.
        // config 0 (<< 3 = 0), mono, code 1 (0b01).
        let toc = 0b01u8;
        let mut pkt = vec![toc];
        pkt.extend_from_slice(&[1, 2, 3, 4]); // two 2-byte frames
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.frame_outcomes.len(), 2);
        // Two 10 ms frames => 2 * 480 = 960 samples/channel.
        assert_eq!(out.samples_per_channel(), 960);
        assert_eq!(out.pcm.len(), 960);
    }

    #[test]
    fn dtx_zero_length_frame_emits_silence_with_status() {
        // Code 3 VBR with a zero-length (DTX) frame. Build a code-3
        // packet by hand: TOC, frame-count byte, then VBR lengths.
        // Simpler: rely on code-2 unequal where the first frame length 0
        // is a valid DTX marker per §3.2.1.
        // config 0 (<< 3 = 0) SILK NB 10 ms mono, code 2 (0b10).
        let toc = 0b10u8;
        // code 2 body: a length prefix for frame 1, then frame1, then
        // frame2 is the remainder. Length 0 => frame1 is DTX.
        let pkt = vec![toc, 0x00, 0x07];
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.frame_outcomes.len(), 2);
        assert_eq!(out.frame_outcomes[0].status, FrameDecodeStatus::DtxOrLost);
        // Both frames are 10 ms => 480 samples/channel each.
        assert_eq!(out.samples_per_channel(), 960);
    }

    #[test]
    fn reset_clears_carried_channel_state() {
        let mut dec = OpusDecoder::new();
        let stereo = code0_packet(20, true, &[1, 2]);
        dec.decode_packet(&stereo).expect("decode");
        assert_eq!(dec.last_channels, Some(2));
        dec.reset();
        assert_eq!(dec.last_channels, None);
    }

    #[test]
    fn all_silence_pcm_for_unwired_layers() {
        // Every Table-2 config currently emits all-zero PCM; verify the
        // buffer is silence and the length matches the routing.
        let mut dec = OpusDecoder::new();
        for config in 0u8..32 {
            for stereo in [false, true] {
                let pkt = code0_packet(config, stereo, &[0x55, 0x66, 0x77]);
                let out = dec.decode_packet(&pkt).expect("decode");
                assert!(out.pcm.iter().all(|&s| s == 0), "config {config}");
                let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
                let expected = output_samples_per_channel(routing.frame_size_tenths_ms)
                    * out.channels as usize;
                assert_eq!(out.pcm.len(), expected, "config {config} stereo {stereo}");
            }
        }
    }
}
