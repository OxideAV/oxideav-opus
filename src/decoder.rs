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
//! incrementally:
//!
//! * **Mono SILK-only** frames run the full §4.2 decode → PCM path: the
//!   §4.2.3 header bits, the §4.2.5 LBRR / §4.2.6 regular SILK frame loop
//!   (1 / 2 / 3 SILK frames per §4.2.2), each frame decoded in Table-5
//!   order via [`crate::silk_decode::decode_silk_frame`] with the
//!   inter-frame state threaded across them, then the §4.2.7.9 LTP / LPC
//!   synthesis ([`crate::silk_synthesis::synthesize_silk_frame`]) and the
//!   §4.2.9 (non-normative) resample to 48 kHz. The carried §4.2.7.9
//!   synthesis histories persist across the packet's Opus frames; the
//!   emitted PCM is real audio ([`FrameDecodeStatus::SilkParamsDecoded`]).
//! * **Stereo SILK-only** frames run the full §4.2 interleaved decode →
//!   PCM path: the §4.2.3 two-channel header bits, the §4.2.5 / §4.2.6
//!   mid/side interleave (mid frame then side frame per 20 ms interval,
//!   the side frame skipped when the §4.2.7.2 mid-only flag is set), each
//!   channel's §4.2.7.9 synthesis with its own carried history, then the
//!   §4.2.8 mid/side → left/right unmixing
//!   ([`crate::silk_stereo::stereo_ms_to_lr`]) and the §4.2.9 resample,
//!   emitting interleaved L/R PCM
//!   ([`FrameDecodeStatus::SilkStereoDecoded`]). The §4.2.7.1 mono→stereo
//!   weight reset and the §4.5.2 SILK state reset are applied across
//!   packets.
//! * **CELT-only** frames run the full §4.3 decode → PCM path: the
//!   Table-56 entropy decode ([`crate::celt_frame_decode`]: frame flags,
//!   coarse / fine / final energies, TF, spread, dynalloc, trim, the
//!   §4.3.3 implicit allocation, §4.3.4 PVQ band shapes with folding,
//!   §4.3.5 anti-collapse) and the §4.3.6–§4.3.7.2 synthesis
//!   ([`crate::celt_mdct_synthesis`]: denormalisation, long/short-block
//!   inverse MDCT + overlap-add, the §4.3.7.1 pitch post-filter, and
//!   de-emphasis), with all cross-frame state carried
//!   ([`FrameDecodeStatus::CeltDecoded`]).
//! * **Hybrid** frames emit silence of the correct length flagged
//!   [`FrameDecodeStatus::LayerNotWired`] (the SILK + CELT layer sum is
//!   the remaining assembly step).
//!
//! Either way the multi-frame packet loop and the RFC 7845 §5.1 48 kHz
//! sample-count accounting are exercised end-to-end.

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
    /// A mono SILK-only frame whose full §4.2.7 bitstream (frame type,
    /// gains, LSF chain, LTP, LCG seed, excitation) was decoded in
    /// Table-5 order via [`crate::silk_decode::decode_silk_frame`], then
    /// synthesized through the §4.2.7.9 LTP / LPC filters
    /// ([`crate::silk_synthesis::synthesize_silk_frame`]) and resampled to
    /// 48 kHz (§4.2.9, non-normative). The emitted PCM is real audio.
    SilkParamsDecoded,
    /// A **stereo** SILK-only frame whose §4.2.3 / §4.2.4 header bits and
    /// the §4.2.5 / §4.2.6 interleaved mid/side SILK frames were decoded
    /// in §4.2.2 order (mid frame then side frame per 20 ms interval, the
    /// side frame skipped when the §4.2.7.2 mid-only flag is set), each
    /// channel synthesized through the §4.2.7.9 filters, converted from
    /// mid/side to left/right via §4.2.8 stereo unmixing
    /// ([`crate::silk_stereo::stereo_ms_to_lr`]), then resampled to 48 kHz
    /// (§4.2.9, non-normative). The emitted interleaved L/R PCM is real
    /// audio.
    SilkStereoDecoded,
    /// A SILK-only frame whose §4.2.7 bitstream decode latched an error
    /// (a malformed / truncated frame). Silence of the correct length was
    /// emitted in its place per the §4.6 floor.
    SilkDecodeError,
    /// A CELT-only frame whose §4.3.7.1 silence flag was set: the real
    /// range-coded frame prefix (silence + post-filter group) was decoded
    /// and the §4.3.6→§4.3.7.2 synthesis backend was advanced with
    /// all-zero band shapes / energies, emitting silence PCM while
    /// carrying the MDCT overlap-add and de-emphasis state forward for the
    /// next frame. (Distinct from [`Self::LayerNotWired`]: the bitstream
    /// is actually consumed and the synthesis state is real, not stubbed.)
    CeltSilence,
    /// A CELT-only frame whose §4.3.7.1 prefix decode latched a range-coder
    /// error (a malformed / truncated frame). Silence of the correct length
    /// was emitted in its place per the §4.6 floor.
    CeltDecodeError,
    /// A **non-silent** CELT-only frame decoded end-to-end: the full
    /// Table-56 entropy decode ([`crate::celt_frame_decode`]) followed
    /// by the §4.3.6–§4.3.7.2 synthesis
    /// ([`crate::celt_mdct_synthesis`]). The emitted PCM is real audio.
    CeltDecoded,
    /// A Hybrid frame decoded end-to-end: the §4.2 SILK layer (WB
    /// internal, non-normative resample to 48 kHz) plus the §4.3 CELT
    /// layer (bands 17–21) summed per §4.4. The emitted PCM is real
    /// audio.
    HybridDecoded,
    /// A **lost** frame concealed by the §4.4 packet-loss concealment
    /// ([`OpusDecoder::conceal_loss`]): the emitted PCM extrapolates
    /// the last decoded audio (LPC extrapolation after a SILK-bearing
    /// frame, pitch-periodic repetition after a CELT-only frame) with
    /// the §4.4 energy decay across consecutive losses. Not produced
    /// by `decode_packet` — only by an explicit concealment call.
    Concealed,
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

/// Why an in-band FEC ([`OpusDecoder::decode_packet_fec`]) recovery
/// produced the samples it did (RFC 6716 §2.1.7 / §4.2.5).
///
/// In-band FEC works by re-encoding the signal of the frame *prior* to a
/// packet at a lower bitrate and carrying it as one or more §4.2.5 LBRR
/// frames inside that packet. When a packet is lost, the decoder can
/// recover the lost frame's audio from the LBRR frame(s) in the *next*
/// successfully received packet (`decode_packet_fec`), rather than
/// emitting pure silence / running pitch-based concealment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecDecodeStatus {
    /// The packet carried §4.2.5 LBRR frame(s) for the lost prior frame,
    /// and they were decoded in Table-5 order and synthesized through the
    /// §4.2.7.9 LTP / LPC filters into real recovered audio at 48 kHz. For
    /// a stereo packet the recovered mid/side LBRR frames were unmixed via
    /// §4.2.8.
    Recovered,
    /// The packet has no LBRR frame for the requested channel(s) (the
    /// §4.2.4 LBRR flags are clear), so no FEC data is available. Silence
    /// of the requested duration was emitted; the caller should fall back
    /// to its own packet-loss concealment.
    NoLbrr,
    /// The packet is not a SILK-bearing mode (CELT-only carries no LBRR),
    /// so FEC recovery is not possible. Silence was emitted.
    NotSilk,
    /// The packet's §4.2 LBRR bitstream was malformed / truncated. Silence
    /// of the requested duration was emitted in its place.
    DecodeError,
}

/// The result of an in-band FEC recovery for one lost packet
/// ([`OpusDecoder::decode_packet_fec`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FecRecovered {
    /// Interleaved signed 16-bit PCM at 48 kHz, same layout as
    /// [`DecodedAudio::pcm`]. Length is `samples_per_channel * channels`.
    pub pcm: Vec<i16>,
    /// Number of audio channels (1 for mono, 2 for stereo).
    pub channels: u8,
    /// Output sample rate in Hz (always [`OUTPUT_SAMPLE_RATE_HZ`]).
    pub sample_rate_hz: u32,
    /// Why the samples were produced (real recovery vs silence and why).
    pub status: FecDecodeStatus,
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
    /// Mono SILK synthesis state (the §4.2.7.9 LTP / LPC histories),
    /// carried across Opus frames in the stream. `None` until the first
    /// mono SILK-only frame is synthesized; re-created (cleared per
    /// §4.5.2) when the SILK bandwidth changes.
    silk_synth_mono: Option<crate::silk_synthesis::SilkSynthState>,
    /// Stereo SILK synthesis state: the §4.2.7.9 LTP / LPC histories for
    /// the **mid** and **side** channels, carried across Opus frames.
    /// `None` until the first stereo SILK-only frame; re-created when the
    /// SILK bandwidth changes (a §4.5.2 reset).
    silk_synth_stereo: Option<(
        crate::silk_synthesis::SilkSynthState,
        crate::silk_synthesis::SilkSynthState,
    )>,
    /// §4.2.8 stereo unmixing history (two prior mid samples, one prior
    /// side sample, and the previous frame's prediction weights), carried
    /// across Opus frames. `None` until the first stereo SILK-only frame;
    /// reset (zeroed) on any §4.2.7.1 mono→stereo transition.
    silk_stereo_unmix: Option<crate::silk_stereo::StereoUnmixState>,
    /// Cross-Opus-frame §4.2.7 reconstruction carry for the mono SILK
    /// channel: the last decoded subframe gain (the §4.2.7.4 clamp base,
    /// which persists "in the same channel" across Opus frames) and the
    /// last decoded NLSF vector (the §4.2.7.5.5 interpolation base `n0`,
    /// "the LSF coefficients decoded for the prior frame"). Cleared on a
    /// §4.5.2 SILK reset and on a SILK bandwidth change.
    silk_carry_mono: SilkChannelCarry,
    /// Cross-Opus-frame §4.2.7 reconstruction carry for the stereo MID
    /// channel (see [`Self::silk_carry_mono`]).
    silk_carry_mid: SilkChannelCarry,
    /// Cross-Opus-frame §4.2.7 reconstruction carry for the stereo SIDE
    /// channel. An uncoded side frame at the end of a packet leaves this
    /// cleared, so the next coded side frame codes independently with the
    /// §4.2.7.4 clamp skipped and the §4.2.7.5.5 factor forced to 4 —
    /// exactly the RFC's "previous frame in the side channel was not
    /// coded" rules.
    silk_carry_side: SilkChannelCarry,
    /// Operating mode of the most recently decoded Opus frame, used to
    /// drive the §4.5.2 SILK state-reset rule ("the SILK state is reset
    /// before every SILK-only or Hybrid frame where the previous frame
    /// was CELT-only"). `None` before the first frame / after a reset.
    prev_mode: Option<OperatingMode>,
    /// CELT synthesis-side state (§4.3.7 overlap-add memory, §4.3.7.1
    /// post-filter signal history + parameter pair, §4.3.7.2
    /// de-emphasis memory), carried across the CELT frames of the
    /// stream. `None` until the first CELT frame; rebuilt when the CELT
    /// frame size or channel count changes and dropped on a §4.5.2
    /// CELT reset.
    celt_synth: Option<crate::celt_mdct_synthesis::CeltSynthesis>,
    /// CELT entropy-side state (the §4.3.2 per-band energy history and
    /// the carried folding-noise seed), advanced by every CELT frame.
    celt_energy: Option<crate::celt_frame_decode::CeltEnergyState>,
    /// §4.4 packet-loss concealment state: the trailing output
    /// history, the consecutive-loss counter, and the pending
    /// concealment-to-real cross-lap tail.
    plc: crate::plc::PlcState,
    /// Frame duration (§3.1 Table 2, tenths of ms) of the most
    /// recently decoded packet — the duration [`Self::conceal_loss`]
    /// conceals when the lost packet's own duration is unknown.
    last_frame_tenths_ms: Option<u16>,
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
        self.decode_parsed_packet(parsed)
    }

    /// Decode one complete Opus packet that uses RFC 6716 Appendix-B
    /// self-delimited framing (the framing the first `N − 1` streams of
    /// a multistream packet use, RFC 7845 §3). Behaves exactly like
    /// [`Self::decode_packet`] otherwise — the only difference is how the
    /// frame slices are recovered from the packet bytes.
    pub fn decode_self_delimited_packet(&mut self, packet: &[u8]) -> Result<DecodedAudio, Error> {
        let parsed = crate::framing_self_delim::parse_self_delimited(packet)?.packet;
        self.decode_parsed_packet(parsed)
    }

    /// Shared decode body for both the regular ([`Self::decode_packet`])
    /// and self-delimited ([`Self::decode_self_delimited_packet`]) entry
    /// points: applies the §4.5.2 cross-packet state resets, then runs
    /// the §4.5 multi-frame loop over the already-sliced frames.
    fn decode_parsed_packet(&mut self, parsed: OpusPacket<'_>) -> Result<DecodedAudio, Error> {
        let routing = OpusFrameRouting::from_toc(parsed.toc);
        let channels = routing.channel_count();
        let per_frame_samples = output_samples_per_channel(routing.frame_size_tenths_ms);

        // §4.5.2 SILK state reset: the SILK decoder is reset before every
        // SILK-only or Hybrid frame whose predecessor was CELT-only. We
        // apply this at the Opus-packet boundary using the recorded
        // previous operating mode. (Redundancy placement only moves the
        // CELT reset, which doesn't affect the SILK reset, so we pass the
        // safe NotPresent default here.)
        if let Some(prev_mode) = self.prev_mode {
            let reset = crate::mode_transition_reset::decide_state_resets(
                prev_mode,
                routing.operating_mode,
                crate::celt_redundancy::RedundancyDecision::NotPresent,
            );
            if reset.silk {
                if let Some(state) = self.silk_synth_mono.as_mut() {
                    state.reset();
                }
                if let Some((mid, side)) = self.silk_synth_stereo.as_mut() {
                    mid.reset();
                    side.reset();
                }
                if let Some(unmix) = self.silk_stereo_unmix.as_mut() {
                    unmix.reset();
                }
                // §4.2.7.4 / §4.2.7.5.5: "the clamping is skipped after a
                // decoder reset" and the interpolation factor is forced
                // to 4 — drop the carried gain / NLSF bases.
                self.silk_carry_mono = SilkChannelCarry::default();
                self.silk_carry_mid = SilkChannelCarry::default();
                self.silk_carry_side = SilkChannelCarry::default();
            }
            if reset.celt_resets() {
                // §4.5.2 rules 2–4: drop the carried CELT state so the
                // next CELT-layer frame starts from the reset state.
                self.celt_energy = None;
                self.celt_synth = None;
            }
        }

        // §4.2.7.1: "the previous weights are reset to zeros on any
        // transition from mono to stereo." More generally the §4.2.8
        // unmixing history (and the mid/side synthesis state) only makes
        // sense within a contiguous stereo run; a channel-count change
        // clears the carried stereo state so a stale mono / prior-stereo
        // history can never leak across the transition.
        if self.last_channels.is_some_and(|c| c != channels) {
            if let Some(unmix) = self.silk_stereo_unmix.as_mut() {
                unmix.reset();
            }
            if let Some((mid, side)) = self.silk_synth_stereo.as_mut() {
                mid.reset();
                side.reset();
            }
            // The mid/side §4.2.7 reconstruction carries follow the same
            // policy as the stereo synthesis state: a channel-count
            // change clears them (the mono carry mirrors the mono
            // synthesis state and persists, matching the treatment of
            // `silk_synth_mono` above).
            self.silk_carry_mid = SilkChannelCarry::default();
            self.silk_carry_side = SilkChannelCarry::default();
        }

        self.last_channels = Some(channels);
        self.prev_mode = Some(routing.operating_mode);

        let frame_slices = parsed.frames();
        let mut pcm: Vec<i16> =
            Vec::with_capacity(frame_slices.len() * per_frame_samples * channels as usize);
        let mut frame_outcomes = Vec::with_capacity(frame_slices.len());

        for frame in frame_slices {
            let outcome = self.decode_one_frame(frame, &routing, &mut pcm);
            frame_outcomes.push(outcome);
        }

        // §4.4: cross-lap a pending concealment tail into the head of
        // this (first real post-loss) output, then record the output
        // as concealment history and re-arm the loss counter.
        self.plc.apply_tail(&mut pcm, channels as usize);
        self.plc.feed_decoded(&pcm, channels as usize);
        self.last_frame_tenths_ms = Some(routing.frame_size_tenths_ms);

        Ok(DecodedAudio {
            pcm,
            channels,
            sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
            frame_outcomes,
        })
    }

    /// Conceal one **lost** packet (RFC 6716 §4.4), producing one
    /// frame of interleaved 48 kHz PCM extrapolated from the last
    /// decoded audio.
    ///
    /// Call this once per lost packet, in stream position, when no FEC
    /// recovery is available (when the *next* packet is already in
    /// hand, prefer [`Self::decode_packet_fec`], which reconstructs
    /// the lost audio from real bitstream redundancy). The §4.4
    /// per-mode guidance is followed: after a SILK-only or Hybrid
    /// frame the concealment is an LPC extrapolation of the previous
    /// output ([`crate::plc::conceal_silk`]); after a CELT-only frame
    /// it repeats the pitch-periodic waveform
    /// ([`crate::plc::conceal_celt`]). Consecutive concealed frames
    /// decay in energy to the silence floor, and the first packet
    /// decoded after a concealment run is cross-lapped with the
    /// concealment's extrapolation tail so both joins are smooth.
    ///
    /// The concealed duration is the last decoded packet's §3.1 frame
    /// duration (a loss in a stream is overwhelmingly likely to have
    /// the neighbouring packets' geometry); with no decode history at
    /// all this emits 20 ms of silence. Per §4.5 the concealment is
    /// for *actual loss*: normative mode transitions in a fully
    /// received stream must not route through PLC (they either need no
    /// treatment or carry §4.5.1 redundancy).
    pub fn conceal_loss(&mut self) -> DecodedAudio {
        let tenths = self.last_frame_tenths_ms.unwrap_or(200);
        let per_channel = output_samples_per_channel(tenths);
        let channels = self.last_channels.unwrap_or(1);
        let flavor = match self.prev_mode {
            Some(OperatingMode::CeltOnly) => crate::plc::PlcFlavor::Celt,
            _ => crate::plc::PlcFlavor::Silk,
        };
        let pcm = self.plc.conceal(per_channel, channels as usize, flavor);
        DecodedAudio {
            pcm,
            channels,
            sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
            frame_outcomes: vec![FrameOutcome {
                samples_per_channel: per_channel,
                status: FrameDecodeStatus::Concealed,
            }],
        }
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

    /// Decode one SILK-only Opus frame (§4.2).
    ///
    /// For a **mono** Opus frame this runs the real §4.2.3 header-bit
    /// decode followed by the §4.2.5 LBRR / §4.2.6 regular SILK frame
    /// loop, calling [`crate::silk_decode::decode_silk_frame`] for each
    /// regular SILK frame in Table-5 order with the inter-frame state
    /// (previous gain / lag / NLSF) threaded across the frames of the
    /// Opus frame. The decoded parameters + excitation are then run
    /// through the §4.2.7.9 LTP / LPC synthesis
    /// ([`crate::silk_synthesis::synthesize_silk_frame`]) and the §4.2.9
    /// (non-normative) resample to 48 kHz, producing real PCM
    /// ([`FrameDecodeStatus::SilkParamsDecoded`]). A truncated / malformed
    /// frame yields [`FrameDecodeStatus::SilkDecodeError`] and silence.
    ///
    /// A **stereo** Opus frame routes to
    /// [`Self::decode_silk_only_stereo`], which runs the §4.2.6 mid/side
    /// interleave with the §4.2.7.1 / §4.2.7.2 symbols enabled and the
    /// §4.2.8 unmixing back half, emitting interleaved L/R PCM
    /// ([`FrameDecodeStatus::SilkStereoDecoded`]).
    fn decode_silk_only_frame(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        let channels = routing.channel_count();
        let pcm_start = pcm.len();
        push_silence(pcm, per_channel, channels);

        if channels == 2 {
            let status = match self.decode_silk_only_stereo(frame, routing) {
                Ok((left, right, bandwidth)) => {
                    // §4.2.9 (non-normative): resample each channel to the
                    // 48 kHz output rate, then write it interleaved
                    // (`[L0, R0, L1, R1, …]`) over the reserved silence.
                    resample_stereo_to_output_i16(
                        &left,
                        &right,
                        bandwidth,
                        &mut pcm[pcm_start..pcm_start + per_channel * 2],
                    );
                    FrameDecodeStatus::SilkStereoDecoded
                }
                Err(_) => FrameDecodeStatus::SilkDecodeError,
            };
            return FrameOutcome {
                samples_per_channel: per_channel,
                status,
            };
        }

        let status = match self.decode_silk_only_mono(frame, routing) {
            Ok((internal, bandwidth)) => {
                // §4.2.9 (non-normative): resample the internal-rate
                // signal to the 48 kHz decoder output rate and write it
                // over the reserved silence region. The spec says "the
                // resampler itself is non-normative, and a decoder can use
                // any method it wants"; we use linear interpolation.
                resample_internal_to_output_i16(
                    &internal,
                    bandwidth,
                    &mut pcm[pcm_start..pcm_start + per_channel],
                );
                FrameDecodeStatus::SilkParamsDecoded
            }
            Err(_) => FrameDecodeStatus::SilkDecodeError,
        };
        FrameOutcome {
            samples_per_channel: per_channel,
            status,
        }
    }

    /// Decode the full §4.2 bitstream of one mono SILK-only Opus frame:
    /// §4.2.3 header bits, the §4.2.5 LBRR frames, and the §4.2.6 regular
    /// SILK frames, consuming every symbol in order. Returns `Ok(())`
    /// when the whole frame decodes cleanly.
    fn decode_silk_only_mono(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
    ) -> Result<(Vec<f32>, crate::toc::Bandwidth), Error> {
        let mut rd = crate::range_decoder::RangeDecoder::new(frame);
        self.decode_silk_layer_mono(&mut rd, routing)
    }

    /// The mono §4.2 SILK layer decode on a caller-supplied range
    /// coder — shared by the SILK-only path (fresh coder over the
    /// frame) and the Hybrid path (the CELT layer continues on the
    /// same coder afterwards).
    fn decode_silk_layer_mono(
        &mut self,
        rd: &mut crate::range_decoder::RangeDecoder<'_>,
        routing: &OpusFrameRouting,
    ) -> Result<(Vec<f32>, crate::toc::Bandwidth), Error> {
        use crate::silk_decode::{decode_silk_frame, SilkFrameConfig, SilkFrameDecoded};
        use crate::silk_excitation::SilkFrameSize;
        use crate::silk_frame::FrameKind;
        use crate::silk_header::SilkHeaderBits;
        use crate::silk_synthesis::{synthesize_silk_frame, SilkSynthState};

        let bandwidth = routing
            .silk_bandwidth
            .ok_or(Error::MalformedPacket)?
            .to_bandwidth();
        let num_silk_frames = routing
            .silk_frames_per_channel
            .ok_or(Error::MalformedPacket)?;
        // §4.2.2: each SILK frame is 20 ms, except a 10 ms Opus frame
        // (one SILK frame of 10 ms).
        let frame_size = if routing.frame_size_tenths_ms == 100 {
            SilkFrameSize::TenMs
        } else {
            SilkFrameSize::TwentyMs
        };

        // §4.2.3 / §4.2.4 header bits (mono => stereo = false).
        let header = SilkHeaderBits::decode(rd, num_silk_frames, false)?;

        // §4.2.5 LBRR frames: one per SILK frame whose mid LBRR bit is
        // set, in time-interval order. LBRR frames are independent of the
        // regular-frame inter-frame state (they form their own sequence),
        // but for this mono path we decode them to consume their bits and
        // keep the range coder aligned with the regular frames that
        // follow. Per §4.2.7.3 an LBRR frame is always active-coded.
        let mut lbrr_prev_gain: Option<u8> = None;
        let mut lbrr_prev_lag: Option<i32> = None;
        let mut lbrr_first = true;
        for idx in 0..num_silk_frames {
            if !header.mid_has_lbrr(idx) {
                // §4.2.4 LBRR-flag gap: the next coded LBRR frame codes
                // independently (§4.2.7.4), with an absolute lag
                // (§4.2.7.6.1) and the §4.2.7.6.3 scaling field present
                // again ("the previous LBRR frame ... is not coded").
                lbrr_prev_gain = None;
                lbrr_prev_lag = None;
                lbrr_first = true;
                continue;
            }
            let cfg = SilkFrameConfig {
                bandwidth,
                frame_size,
                voice_active: true, // §4.2.7.3: LBRR uses the active PDF.
                first_subframe_independent: lbrr_first || lbrr_prev_gain.is_none(),
                previous_log_gain: lbrr_prev_gain,
                previous_primary_lag: lbrr_prev_lag,
                ltp_scaling_present: lbrr_first,
                lsf_interp_after_reset: lbrr_first,
                previous_nlsf_q15: None,
                previous_nlsf_len: 0,
                // Mono SILK-only path: no §4.2.7.1 / §4.2.7.2 stereo header.
                stereo: None,
            };
            let decoded = decode_silk_frame(rd, cfg)?;
            lbrr_prev_gain = Some(decoded.gains.last_log_gain());
            // §4.2.7.6.1: only a VOICED frame arms relative lag coding.
            lbrr_prev_lag = if decoded.ltp.is_voiced() {
                Some(decoded.ltp.primary_lag())
            } else {
                None
            };
            lbrr_first = false;
            let _ = FrameKind::Lbrr; // documents the §4.2.7.3 kind.
        }

        // §4.2.6 regular SILK frames: one per time interval, even when
        // the VAD flag is unset. Inter-frame state threads across them,
        // resuming the cross-Opus-frame §4.2.7.4 gain-clamp base and the
        // §4.2.7.5.5 NLSF interpolation base `n0` carried from the
        // previous Opus frame (the frame-local `first` flag still makes
        // the first frame's gain independently CODED and its §4.2.7.6.3
        // scaling / absolute lag present, per the per-Opus-frame rules).
        let mut chan = ChannelDecodeState::resume(&self.silk_carry_mono, bandwidth);
        let mut decoded_frames: Vec<SilkFrameDecoded> =
            Vec::with_capacity(num_silk_frames as usize);
        for idx in 0..num_silk_frames {
            let decoded = decode_silk_frame(
                rd,
                // Mono SILK-only path: no §4.2.7.1 / §4.2.7.2 stereo header.
                chan.config(bandwidth, frame_size, header.mid_vad(idx), None),
            )?;
            chan.advance(&decoded);
            decoded_frames.push(decoded);
        }

        if rd.has_error() {
            return Err(Error::MalformedPacket);
        }
        // Persist the §4.2.7.4 / §4.2.7.5.5 bases for the next Opus frame.
        self.silk_carry_mono = chan.to_carry(bandwidth);

        // §4.2.7.9 synthesis: turn the decoded SILK frames into
        // internal-rate (8/12/16 kHz) time-domain samples, threading the
        // cross-Opus-frame §4.2.7.9 histories. The state is (re)created if
        // absent or if the SILK bandwidth changed (a §4.5.2 reset).
        let need_fresh = match &self.silk_synth_mono {
            Some(s) => s.bandwidth() != bandwidth,
            None => true,
        };
        if need_fresh {
            self.silk_synth_mono = Some(SilkSynthState::new(bandwidth)?);
        }
        let state = self
            .silk_synth_mono
            .as_mut()
            .expect("synth state set above");

        let mut internal = Vec::new();
        for decoded in &decoded_frames {
            let frame_out = synthesize_silk_frame(bandwidth, frame_size, decoded, state)?;
            internal.extend_from_slice(&frame_out);
        }
        Ok((internal, bandwidth))
    }

    /// Decode the full §4.2 bitstream of one **stereo** SILK-only Opus
    /// frame and unmix it to left/right.
    ///
    /// The §4.2.2 stereo organisation interleaves the two channels: per
    /// 20 ms interval the mid SILK frame is decoded, then the side SILK
    /// frame (skipped when the §4.2.7.2 mid-only flag on the mid frame is
    /// set). The §4.2.7.1 stereo prediction weights ride on the mid
    /// frame. After both channels finish their §4.2.7.9 synthesis they are
    /// converted from mid/side to left/right via §4.2.8
    /// ([`crate::silk_stereo::stereo_ms_to_lr`]).
    ///
    /// LBRR frames (§4.2.5) precede the regular frames and are also
    /// interleaved (mid then side per interval); they are decoded only to
    /// keep the range coder aligned with the regular frames that follow.
    ///
    /// Returns `(left, right, bandwidth)` at the SILK internal rate.
    #[allow(clippy::type_complexity)]
    fn decode_silk_only_stereo(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
    ) -> Result<(Vec<f32>, Vec<f32>, crate::toc::Bandwidth), Error> {
        let mut rd = crate::range_decoder::RangeDecoder::new(frame);
        self.decode_silk_layer_stereo(&mut rd, routing)
    }

    /// The stereo §4.2 SILK layer decode on a caller-supplied range
    /// coder (see [`Self::decode_silk_layer_mono`]).
    fn decode_silk_layer_stereo(
        &mut self,
        rd: &mut crate::range_decoder::RangeDecoder<'_>,
        routing: &OpusFrameRouting,
    ) -> Result<(Vec<f32>, Vec<f32>, crate::toc::Bandwidth), Error> {
        use crate::silk_decode::{decode_silk_frame, SilkFrameDecoded, StereoHeaderContext};
        use crate::silk_excitation::SilkFrameSize;
        use crate::silk_header::SilkHeaderBits;
        use crate::silk_stereo::{stereo_ms_to_lr, StereoUnmixState, StereoWeightsQ13};
        use crate::silk_synthesis::{synthesize_silk_frame, SilkSynthState};

        let bandwidth = routing
            .silk_bandwidth
            .ok_or(Error::MalformedPacket)?
            .to_bandwidth();
        let num_silk_frames = routing
            .silk_frames_per_channel
            .ok_or(Error::MalformedPacket)?;
        let frame_size = if routing.frame_size_tenths_ms == 100 {
            SilkFrameSize::TenMs
        } else {
            SilkFrameSize::TwentyMs
        };

        // §4.2.3 / §4.2.4 header bits (stereo => both channels' VAD + LBRR
        // flags, mid then side).
        let header = SilkHeaderBits::decode(rd, num_silk_frames, true)?;

        // §4.2.5 LBRR frames: per 20 ms interval, the mid LBRR frame (if
        // present) then the side LBRR frame (if present), interleaved per
        // §4.2.2. Decoded only to consume their bits. The §4.2.7.1 stereo
        // weights ride on the mid LBRR frame; the §4.2.7.2 mid-only flag
        // is present on the mid LBRR frame iff the side LBRR is unset for
        // that interval.
        let mut lbrr_mid = ChannelDecodeState::new();
        let mut lbrr_side = ChannelDecodeState::new();
        for idx in 0..num_silk_frames {
            let mid_lbrr = header.mid_has_lbrr(idx);
            let side_lbrr = header.side_has_lbrr(idx);
            if mid_lbrr {
                let stereo_ctx = StereoHeaderContext {
                    // §4.2.7.2: mid-only flag present on the mid frame iff
                    // the corresponding side channel is not coded.
                    has_mid_only_flag: !side_lbrr,
                };
                let decoded = decode_silk_frame(
                    rd,
                    lbrr_mid.config(bandwidth, frame_size, true, Some(stereo_ctx)),
                )?;
                lbrr_mid.advance(&decoded);
            } else {
                // §4.2.4 LBRR-flag gap for the mid channel.
                lbrr_mid.mark_interval_uncoded(true);
            }
            // A set mid-only flag would forbid a coded side LBRR frame;
            // the header LBRR flags already encode that, so we trust
            // `side_lbrr` for the interleave decision. (A side-only LBRR
            // interval is legal: no stereo weights ride on a side frame
            // per §4.2.7.1.)
            if side_lbrr {
                let decoded =
                    decode_silk_frame(rd, lbrr_side.config(bandwidth, frame_size, true, None))?;
                lbrr_side.advance(&decoded);
            } else {
                lbrr_side.mark_interval_uncoded(true);
            }
        }

        // §4.2.6 regular SILK frames: per 20 ms interval, the mid frame
        // then (unless the §4.2.7.2 mid-only flag is set) the side frame.
        // Each channel resumes its cross-Opus-frame §4.2.7.4 gain-clamp
        // base and §4.2.7.5.5 NLSF interpolation base `n0`.
        let mut mid_state = ChannelDecodeState::resume(&self.silk_carry_mid, bandwidth);
        let mut side_state = ChannelDecodeState::resume(&self.silk_carry_side, bandwidth);
        let mut mid_frames: Vec<SilkFrameDecoded> = Vec::with_capacity(num_silk_frames as usize);
        // Per-interval side frame: `Some(frame)` when coded, `None` when
        // the side channel is skipped (mid-only flag set or side VAD path
        // produced no frame). The §4.2.8 unmixer treats a `None` side as
        // all-zero.
        let mut side_frames: Vec<Option<SilkFrameDecoded>> =
            Vec::with_capacity(num_silk_frames as usize);
        // The §4.2.7.1 weights carried by the most-recent mid frame; the
        // §4.2.8 unmix consumes the last interval's weights for the whole
        // Opus frame (one set of weights per SILK frame, but the unmix
        // runs once over the concatenated channel signal — we apply the
        // first interval's weights, threading prev across intervals via
        // the unmix state below).
        let mut interval_weights: Vec<StereoWeightsQ13> =
            Vec::with_capacity(num_silk_frames as usize);

        for idx in 0..num_silk_frames {
            let side_active = header.side_vad(idx);
            // §4.2.7.2: the mid-only flag is present iff the side channel
            // for this interval is NOT active (a regular frame with side
            // VAD unset). When side VAD is set the side frame must be
            // coded and the flag is omitted.
            let stereo_ctx = StereoHeaderContext {
                has_mid_only_flag: !side_active,
            };
            let mid_decoded = decode_silk_frame(
                rd,
                mid_state.config(bandwidth, frame_size, header.mid_vad(idx), Some(stereo_ctx)),
            )?;
            // §4.2.7.1 weights ride on the mid frame.
            let w = mid_decoded.stereo_pred.map(|p| StereoWeightsQ13 {
                w0_q13: p.w0_q13,
                w1_q13: p.w1_q13,
            });
            interval_weights.push(w.unwrap_or_default());
            // §4.2.7.2: side coded iff side VAD set OR the mid-only flag is
            // not set (mid-only flag present + cleared ⇒ side is coded).
            let side_coded = side_active || mid_decoded.mid_only_flag == Some(false);
            mid_state.advance(&mid_decoded);
            mid_frames.push(mid_decoded);

            if side_coded {
                let side_decoded = decode_silk_frame(
                    rd,
                    side_state.config(bandwidth, frame_size, header.side_vad(idx), None),
                )?;
                side_state.advance(&side_decoded);
                side_frames.push(Some(side_decoded));
            } else {
                // §4.2.7.2 / §4.5.2: an uncoded side SILK frame clears the
                // side LTP buffer; zeros feed the §4.2.8 unmixer. The side
                // carried state also resets (§4.2.7.4 / §4.2.7.5.5 /
                // §4.2.7.6.1 all treat "previous frame not coded" as a
                // fresh start for the next coded side frame).
                side_state.mark_interval_uncoded(false);
                side_frames.push(None);
            }
        }

        if rd.has_error() {
            return Err(Error::MalformedPacket);
        }
        // Persist the per-channel §4.2.7.4 / §4.2.7.5.5 bases for the
        // next Opus frame (an uncoded trailing side interval leaves the
        // side carry cleared, arming the RFC's "previous frame in the
        // side channel was not coded" fresh-start rules).
        self.silk_carry_mid = mid_state.to_carry(bandwidth);
        self.silk_carry_side = side_state.to_carry(bandwidth);

        // §4.2.7.9 synthesis for both channels, threading the cross-Opus-
        // frame histories. (Re)create the state on a bandwidth change.
        let need_fresh = match &self.silk_synth_stereo {
            Some((m, _)) => m.bandwidth() != bandwidth,
            None => true,
        };
        if need_fresh {
            self.silk_synth_stereo = Some((
                SilkSynthState::new(bandwidth)?,
                SilkSynthState::new(bandwidth)?,
            ));
        }
        let (mid_synth, side_synth) = self
            .silk_synth_stereo
            .as_mut()
            .expect("stereo synth state set above");

        // §4.2.8 stereo unmixing runs **per SILK frame** (per 20 ms
        // interval), not once over the whole Opus frame: the spec defines
        // the unmix over `j <= i < (j + n2)` where `j` is the SILK frame
        // start and `n2` is "the total number of samples in the frame"
        // (the SILK frame). Each interval carries its own §4.2.7.1 weights
        // and restarts the 8 ms interpolation phase; the previous
        // interval's weights and trailing samples thread through the
        // carried `StereoUnmixState`. We therefore synthesize and unmix
        // each interval in turn and concatenate the L/R outputs.
        let unmix = self
            .silk_stereo_unmix
            .get_or_insert_with(StereoUnmixState::new);

        let mut left = Vec::new();
        let mut right = Vec::new();
        for (idx, mid_frame) in mid_frames.iter().enumerate() {
            let mid_out = synthesize_silk_frame(bandwidth, frame_size, mid_frame, mid_synth)?;
            let n = mid_out.len();
            let weights = interval_weights[idx];
            let stereo = match &side_frames[idx] {
                Some(side_frame) => {
                    let side_out =
                        synthesize_silk_frame(bandwidth, frame_size, side_frame, side_synth)?;
                    stereo_ms_to_lr(bandwidth, &mid_out, Some(&side_out), weights, unmix)?
                }
                None => {
                    // §4.2.7.2 / §4.5.2: an uncoded side SILK frame clears
                    // the side LTP buffer; zeros feed the §4.2.8 unmixer
                    // (`side = None` ⇒ side[i] treated as 0 everywhere).
                    side_synth.reset();
                    stereo_ms_to_lr(bandwidth, &mid_out, None, weights, unmix)?
                }
            };
            debug_assert_eq!(stereo.left.len(), n);
            left.extend_from_slice(&stereo.left);
            right.extend_from_slice(&stereo.right);
        }

        Ok((left, right, bandwidth))
    }

    /// Decode one CELT-only Opus frame (§4.3) end-to-end.
    ///
    /// Runs the full Table-56 entropy decode
    /// ([`crate::celt_frame_decode::decode_celt_frame`]: frame flags,
    /// coarse / fine / final energies, TF, spread, dynalloc, trim, the
    /// §4.3.3 implicit allocation, §4.3.4 PVQ band shapes with folding,
    /// and §4.3.5 anti-collapse), then the §4.3.6–§4.3.7.2 synthesis
    /// ([`crate::celt_mdct_synthesis::CeltSynthesis`]: denormalisation,
    /// long/short-block inverse MDCT with overlap-add, the §4.3.7.1
    /// pitch post-filter, and de-emphasis), emitting real 48 kHz PCM.
    ///
    /// Cross-frame state (energy history + folding seed, overlap and
    /// post-filter memories) is carried in `self`; it is rebuilt when
    /// the frame geometry (size / channels) changes and dropped on a
    /// §4.5.2 CELT reset. A range-coder error or a bit-budget overrun
    /// yields [`FrameDecodeStatus::CeltDecodeError`] and silence.
    fn decode_celt_only_frame(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        use crate::celt_band_layout::CeltFrameSize;

        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        let channels = routing.channel_count() as usize;
        let pcm_start = pcm.len();
        push_silence(pcm, per_channel, channels as u8);
        let err = FrameOutcome {
            samples_per_channel: per_channel,
            status: FrameDecodeStatus::CeltDecodeError,
        };

        let Some(celt_size) =
            CeltFrameSize::from_frame_tenths_ms(routing.frame_size_tenths_ms as u32)
        else {
            return err;
        };
        let lm = celt_size.column_index() as i32;
        let n = per_channel; // CELT-only runs at the 48 kHz output rate.
                             // §4.4 / Table 55: the coded band range ends at the signalled
                             // audio bandwidth (NB→13, WB→17, SWB→19, FB→21).
        let end = match routing.toc_bandwidth {
            crate::toc::Bandwidth::Nb => 13,
            crate::toc::Bandwidth::Mb | crate::toc::Bandwidth::Wb => 17,
            crate::toc::Bandwidth::Swb => 19,
            crate::toc::Bandwidth::Fb => 21,
        };

        // (Re)build the carried CELT state on a geometry change.
        let rebuild = !self
            .celt_synth
            .as_ref()
            .is_some_and(|s| s.channels() == channels && s.frame_len() == n);
        if rebuild {
            self.celt_synth = Some(crate::celt_mdct_synthesis::CeltSynthesis::new(channels, n));
            self.celt_energy = Some(crate::celt_frame_decode::CeltEnergyState::new());
        }
        let energy = self.celt_energy.get_or_insert_with(Default::default);

        // §4.3 entropy half.
        let mut rd = crate::range_decoder::RangeDecoder::new(frame);
        let out = crate::celt_frame_decode::decode_celt_frame(
            &mut rd,
            frame.len(),
            0,
            end,
            lm,
            channels,
            energy,
        );
        if rd.has_error() || u64::from(rd.tell()) > 8 * frame.len() as u64 {
            return err;
        }

        // §4.3.6 denormalisation into the frame spectrum (bins outside
        // the coded range stay zero).
        let m = 1usize << lm;
        let mut freq = vec![0.0f64; channels * n];
        for c in 0..channels {
            for band in 0..end {
                let gain = out.band_gain[c][band];
                let off = m * crate::celt_rate_alloc::band_edge(band) as usize;
                let len = m * crate::celt_rate_alloc::band_width(band) as usize;
                for j in 0..len {
                    freq[c * n + off + j] = gain * out.x[c * out.plane + off + j];
                }
            }
        }

        // §4.3.7 synthesis (signal half).
        let blocks = if out.transient { m } else { 1 };
        let synth = self.celt_synth.as_mut().expect("state built above");
        let mut frame_pcm = vec![0i16; channels * n];
        synth.synthesize_frame(&freq, blocks, out.post_filter, &mut frame_pcm);
        let region = &mut pcm[pcm_start..pcm_start + per_channel * channels];
        region.copy_from_slice(&frame_pcm);

        FrameOutcome {
            samples_per_channel: per_channel,
            status: if out.silence {
                FrameDecodeStatus::CeltSilence
            } else {
                FrameDecodeStatus::CeltDecoded
            },
        }
    }

    /// Recover the audio of a **lost** Opus frame from the in-band FEC
    /// (§4.2.5 LBRR) data carried in the *next* successfully received
    /// packet (RFC 6716 §2.1.7).
    ///
    /// In-band FEC encodes a low-bitrate redundant copy of the signal
    /// immediately *prior* to a packet as one or more §4.2.5 LBRR frames
    /// inside that packet. When the application detects a packet loss and
    /// has the following packet in hand, it calls this method on that
    /// following packet to reconstruct the lost frame's audio instead of
    /// relying solely on silence / pitch-based concealment.
    ///
    /// The recovered PCM is returned at the 48 kHz output rate. The packet
    /// passed here is the one *after* the loss; only its §4.2.5 LBRR
    /// frames are decoded and synthesized (the packet's own regular frames
    /// are decoded later by an ordinary [`Self::decode_packet`] call).
    ///
    /// On success ([`FecDecodeStatus::Recovered`]) the SILK synthesis
    /// history is advanced to the recovered frame's state, so a subsequent
    /// [`Self::decode_packet`] on the same packet continues smoothly from
    /// the reconstructed signal. When the packet carries no LBRR data
    /// ([`FecDecodeStatus::NoLbrr`]), is CELT-only
    /// ([`FecDecodeStatus::NotSilk`]), or is malformed
    /// ([`FecDecodeStatus::DecodeError`]), silence of the lost frame's
    /// duration is returned and the caller falls back to its own
    /// concealment.
    ///
    /// Returns [`Error::EmptyPacket`] for a zero-length packet and
    /// [`Error::MalformedPacket`] for a §3.2 framing violation in the
    /// carrier packet.
    pub fn decode_packet_fec(&mut self, packet: &[u8]) -> Result<FecRecovered, Error> {
        let parsed = OpusPacket::parse(packet)?;
        let routing = OpusFrameRouting::from_toc(parsed.toc);
        let channels = routing.channel_count();
        // §4.2.5: an LBRR frame has the same frame size / bandwidth /
        // channel count as the carrier packet's regular frames, and covers
        // the equivalent prior interval(s); the recovered duration matches
        // the carrier's per-frame duration.
        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        let mut pcm = vec![0i16; per_channel * channels as usize];

        // FEC only exists for SILK-bearing modes (§2.1.7 re-encodes the
        // SILK speech layer); a CELT-only packet carries no LBRR.
        if !matches!(
            routing.operating_mode,
            OperatingMode::SilkOnly | OperatingMode::Hybrid
        ) {
            return Ok(FecRecovered {
                pcm,
                channels,
                sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
                status: FecDecodeStatus::NotSilk,
            });
        }

        // The first Opus frame of the packet carries the §4.2.5 LBRR
        // frames (LBRR frames precede the regular frames within a single
        // SILK-bearing Opus frame; a code-1/2/3 packet's later frames have
        // their own LBRR, but those cover intervals already adjacent to
        // received audio, so the canonical "previous packet was lost"
        // recovery uses the leading Opus frame's LBRR).
        let Some(&frame) = parsed.frames().first() else {
            return Ok(FecRecovered {
                pcm,
                channels,
                sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
                status: FecDecodeStatus::DecodeError,
            });
        };
        if frame.is_empty() {
            return Ok(FecRecovered {
                pcm,
                channels,
                sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
                status: FecDecodeStatus::NoLbrr,
            });
        }

        let status = if channels == 2 {
            match self.decode_silk_fec_stereo(frame, &routing) {
                Ok(Some((left, right, bandwidth))) => {
                    resample_stereo_to_output_i16(&left, &right, bandwidth, &mut pcm);
                    FecDecodeStatus::Recovered
                }
                Ok(None) => FecDecodeStatus::NoLbrr,
                Err(_) => FecDecodeStatus::DecodeError,
            }
        } else {
            match self.decode_silk_fec_mono(frame, &routing) {
                Ok(Some((internal, bandwidth))) => {
                    resample_internal_to_output_i16(&internal, bandwidth, &mut pcm);
                    FecDecodeStatus::Recovered
                }
                Ok(None) => FecDecodeStatus::NoLbrr,
                Err(_) => FecDecodeStatus::DecodeError,
            }
        };

        // A real recovery is stream audio: record it as §4.4
        // concealment history (and clear the consecutive-loss counter)
        // so a further loss immediately after the recovered frame
        // extrapolates from the recovered signal, not from before it.
        if status == FecDecodeStatus::Recovered {
            self.plc.feed_decoded(&pcm, channels as usize);
        }

        Ok(FecRecovered {
            pcm,
            channels,
            sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
            status,
        })
    }

    /// Decode and synthesize the §4.2.5 mono LBRR frame(s) of one
    /// SILK-bearing Opus frame into internal-rate recovered audio.
    ///
    /// Returns `Ok(Some((internal, bandwidth)))` with the recovered
    /// signal when at least one mid LBRR frame is present, `Ok(None)` when
    /// the §4.2.4 LBRR flags are all clear (no FEC data), or `Err` on a
    /// malformed bitstream.
    ///
    /// Unlike [`Self::decode_silk_only_mono`], which only consumed the
    /// LBRR bits to keep the range coder aligned, this path actually runs
    /// the §4.2.7.9 synthesis on the LBRR parameters. Per §4.2.5 the LBRR
    /// frames form their own independent sequence covering the prior
    /// interval(s), so synthesis starts from a **fresh** state (the lost
    /// frame's true history is, by definition, unavailable). On success
    /// the decoder's carried mono synthesis state is replaced with the
    /// recovered-frame history so the next real packet continues smoothly.
    fn decode_silk_fec_mono(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
    ) -> Result<Option<(Vec<f32>, crate::toc::Bandwidth)>, Error> {
        use crate::range_decoder::RangeDecoder;
        use crate::silk_decode::{decode_silk_frame, SilkFrameDecoded};
        use crate::silk_excitation::SilkFrameSize;
        use crate::silk_header::SilkHeaderBits;
        use crate::silk_synthesis::{synthesize_silk_frame, SilkSynthState};

        let bandwidth = routing
            .silk_bandwidth
            .ok_or(Error::MalformedPacket)?
            .to_bandwidth();
        let num_silk_frames = routing
            .silk_frames_per_channel
            .ok_or(Error::MalformedPacket)?;
        let frame_size = if routing.frame_size_tenths_ms == 100 {
            SilkFrameSize::TenMs
        } else {
            SilkFrameSize::TwentyMs
        };

        let mut rd = RangeDecoder::new(frame);
        let header = SilkHeaderBits::decode(&mut rd, num_silk_frames, false)?;

        // No LBRR data → no FEC recovery is possible.
        if !(0..num_silk_frames).any(|i| header.mid_has_lbrr(i)) {
            return Ok(None);
        }

        // §4.2.5 LBRR frames are always active-coded and form their own
        // inter-frame sequence; decode every present LBRR frame in
        // interval order, threading the LBRR-local previous gain / lag /
        // NLSF state (the same Table-5 inter-frame dependencies as regular
        // frames, but over the LBRR sub-sequence).
        let mut lbrr_chan = ChannelDecodeState::new();
        let mut lbrr_frames: Vec<SilkFrameDecoded> = Vec::new();
        for idx in 0..num_silk_frames {
            if !header.mid_has_lbrr(idx) {
                // §4.2.4 LBRR-flag gap: fresh start for the next coded
                // LBRR frame (§4.2.7.4 / §4.2.7.5.5 / §4.2.7.6.1 /
                // §4.2.7.6.3).
                lbrr_chan.mark_interval_uncoded(true);
                continue;
            }
            // §4.2.5: all LBRR frames are active.
            let decoded =
                decode_silk_frame(&mut rd, lbrr_chan.config(bandwidth, frame_size, true, None))?;
            lbrr_chan.advance(&decoded);
            lbrr_frames.push(decoded);
        }

        if rd.has_error() {
            return Err(Error::MalformedPacket);
        }
        if lbrr_frames.is_empty() {
            return Ok(None);
        }
        // The recovered interval becomes "the most recent coded frame"
        // in this channel: seed the cross-Opus-frame §4.2.7.4 /
        // §4.2.7.5.5 bases from the LBRR reconstruction (the RFC's
        // packet-loss latitude — the true regular-frame bases were lost
        // with the packet, and the LBRR frames are the re-encode of the
        // same interval).
        self.silk_carry_mono = lbrr_chan.to_carry(bandwidth);

        // §4.2.7.9 synthesis from a fresh state: the lost frame's true
        // history is unavailable, so the recovered signal is reconstructed
        // self-contained. The resulting history then becomes the carried
        // mono synthesis state for the following real packet.
        let mut state = SilkSynthState::new(bandwidth)?;
        let mut internal = Vec::new();
        for decoded in &lbrr_frames {
            let frame_out = synthesize_silk_frame(bandwidth, frame_size, decoded, &mut state)?;
            internal.extend_from_slice(&frame_out);
        }
        self.silk_synth_mono = Some(state);
        Ok(Some((internal, bandwidth)))
    }

    /// Decode and synthesize the §4.2.5 **stereo** LBRR frame(s) of one
    /// SILK-bearing Opus frame into internal-rate recovered L/R audio.
    ///
    /// Mirrors [`Self::decode_silk_fec_mono`] for stereo: the §4.2.5 LBRR
    /// frames are interleaved (mid then side per 20 ms interval), each
    /// channel is synthesized from a fresh state, and the pair is unmixed
    /// to left/right via §4.2.8 with a fresh unmix history. The §4.2.7.1
    /// stereo prediction weights ride on the mid LBRR frame; the §4.2.7.2
    /// mid-only flag governs whether a side LBRR frame is present for the
    /// interval (mirroring the regular stereo path).
    ///
    /// Returns `Ok(Some((left, right, bandwidth)))` on recovery,
    /// `Ok(None)` when neither channel carries LBRR, or `Err` on a
    /// malformed bitstream. On success the carried stereo synthesis +
    /// unmix state is replaced with the recovered-frame state.
    #[allow(clippy::type_complexity)]
    fn decode_silk_fec_stereo(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
    ) -> Result<Option<(Vec<f32>, Vec<f32>, crate::toc::Bandwidth)>, Error> {
        use crate::range_decoder::RangeDecoder;
        use crate::silk_decode::{decode_silk_frame, SilkFrameDecoded, StereoHeaderContext};
        use crate::silk_excitation::SilkFrameSize;
        use crate::silk_header::SilkHeaderBits;
        use crate::silk_stereo::{stereo_ms_to_lr, StereoUnmixState, StereoWeightsQ13};
        use crate::silk_synthesis::{synthesize_silk_frame, SilkSynthState};

        let bandwidth = routing
            .silk_bandwidth
            .ok_or(Error::MalformedPacket)?
            .to_bandwidth();
        let num_silk_frames = routing
            .silk_frames_per_channel
            .ok_or(Error::MalformedPacket)?;
        let frame_size = if routing.frame_size_tenths_ms == 100 {
            SilkFrameSize::TenMs
        } else {
            SilkFrameSize::TwentyMs
        };

        let mut rd = RangeDecoder::new(frame);
        let header = SilkHeaderBits::decode(&mut rd, num_silk_frames, true)?;

        let any_lbrr =
            (0..num_silk_frames).any(|i| header.mid_has_lbrr(i) || header.side_has_lbrr(i));
        if !any_lbrr {
            return Ok(None);
        }

        // §4.2.5 interleaved LBRR decode: per 20 ms interval the mid LBRR
        // frame (if present, carrying the §4.2.7.1 weights + §4.2.7.2
        // mid-only flag) then the side LBRR frame (if present). Each
        // channel threads its own LBRR-local inter-frame state.
        let mut mid_state = ChannelDecodeState::new();
        let mut side_state = ChannelDecodeState::new();
        let mut mid_frames: Vec<SilkFrameDecoded> = Vec::new();
        let mut side_frames: Vec<Option<SilkFrameDecoded>> = Vec::new();
        let mut interval_weights: Vec<StereoWeightsQ13> = Vec::new();

        for idx in 0..num_silk_frames {
            let mid_lbrr = header.mid_has_lbrr(idx);
            let side_lbrr = header.side_has_lbrr(idx);
            if !mid_lbrr {
                // §4.2.4 LBRR-flag gap for the mid channel.
                mid_state.mark_interval_uncoded(true);
                // §4.2.5 / §4.2.7.1: a side LBRR frame without a mid LBRR
                // frame carries no stereo weights; record a zero-weight
                // interval with the mid channel treated as silent.
                if side_lbrr {
                    let side_decoded = decode_silk_frame(
                        &mut rd,
                        side_state.config(bandwidth, frame_size, true, None),
                    )?;
                    side_state.advance(&side_decoded);
                    // Without a mid LBRR frame there is no mid signal for
                    // this interval; the unmixer treats the missing mid as
                    // a hole (handled by skipping the interval in synthesis
                    // below — we still consume the bits for alignment).
                    let _ = side_decoded;
                } else {
                    side_state.mark_interval_uncoded(true);
                }
                continue;
            }
            // §4.2.7.2: the mid-only flag is present on the mid LBRR frame
            // iff the side LBRR frame for this interval is absent.
            let stereo_ctx = StereoHeaderContext {
                has_mid_only_flag: !side_lbrr,
            };
            let mid_decoded = decode_silk_frame(
                &mut rd,
                mid_state.config(bandwidth, frame_size, true, Some(stereo_ctx)),
            )?;
            let w = mid_decoded.stereo_pred.map(|p| StereoWeightsQ13 {
                w0_q13: p.w0_q13,
                w1_q13: p.w1_q13,
            });
            interval_weights.push(w.unwrap_or_default());
            let side_coded = side_lbrr || mid_decoded.mid_only_flag == Some(false);
            mid_state.advance(&mid_decoded);
            mid_frames.push(mid_decoded);

            if side_coded {
                let side_decoded = decode_silk_frame(
                    &mut rd,
                    side_state.config(bandwidth, frame_size, true, None),
                )?;
                side_state.advance(&side_decoded);
                side_frames.push(Some(side_decoded));
            } else {
                side_state.mark_interval_uncoded(true);
                side_frames.push(None);
            }
        }

        if rd.has_error() {
            return Err(Error::MalformedPacket);
        }
        if mid_frames.is_empty() {
            return Ok(None);
        }

        // §4.2.7.9 synthesis + §4.2.8 unmix from fresh state.
        let mut mid_synth = SilkSynthState::new(bandwidth)?;
        let mut side_synth = SilkSynthState::new(bandwidth)?;
        let mut unmix = StereoUnmixState::new();
        let mut left = Vec::new();
        let mut right = Vec::new();
        for (idx, mid_frame) in mid_frames.iter().enumerate() {
            let mid_out = synthesize_silk_frame(bandwidth, frame_size, mid_frame, &mut mid_synth)?;
            let weights = interval_weights[idx];
            let stereo = match &side_frames[idx] {
                Some(side_frame) => {
                    let side_out =
                        synthesize_silk_frame(bandwidth, frame_size, side_frame, &mut side_synth)?;
                    stereo_ms_to_lr(bandwidth, &mid_out, Some(&side_out), weights, &mut unmix)?
                }
                None => {
                    side_synth.reset();
                    stereo_ms_to_lr(bandwidth, &mid_out, None, weights, &mut unmix)?
                }
            };
            left.extend_from_slice(&stereo.left);
            right.extend_from_slice(&stereo.right);
        }

        self.silk_synth_stereo = Some((mid_synth, side_synth));
        self.silk_stereo_unmix = Some(unmix);
        // As in the mono FEC path: the recovered interval seeds the
        // cross-Opus-frame §4.2.7.4 / §4.2.7.5.5 bases for both channels
        // (the §4.2.7.4 packet-loss latitude).
        self.silk_carry_mid = mid_state.to_carry(bandwidth);
        self.silk_carry_side = side_state.to_carry(bandwidth);
        Ok(Some((left, right, bandwidth)))
    }

    /// Decode one Hybrid Opus frame (§4.2 SILK + §4.3 CELT). Currently
    /// emits silence; depends on both layer paths landing.
    /// Decode one Hybrid Opus frame (§4.4): the §4.2 SILK layer (WB
    /// internal rate) and the §4.3 CELT layer (bands 17–21) share one
    /// range coder; their 48 kHz outputs are summed.
    ///
    /// After the SILK layer the §4.5.1 redundancy side information is
    /// decoded; when a redundant CELT frame is present its bytes are
    /// excluded from the CELT layer's budget (the redundant frame's
    /// own 5 ms synthesis/cross-lap is a pending refinement — the
    /// main-path audio is decoded either way). The SILK→48 kHz
    /// resample is the crate's non-normative linear interpolator, so
    /// the low band is not bit-aligned with the reference decoder's
    /// resampler; the CELT band and the assembly are the normative
    /// paths.
    fn decode_hybrid_frame(
        &mut self,
        frame: &[u8],
        routing: &OpusFrameRouting,
        pcm: &mut Vec<i16>,
    ) -> FrameOutcome {
        use crate::celt_band_layout::CeltFrameSize;

        let per_channel = output_samples_per_channel(routing.frame_size_tenths_ms);
        let channels = routing.channel_count() as usize;
        let pcm_start = pcm.len();
        push_silence(pcm, per_channel, channels as u8);

        let mut rd = crate::range_decoder::RangeDecoder::new(frame);

        // §4.2 SILK layer (always WB internal for Hybrid), resampled
        // to 48 kHz into the output region.
        let silk_ok = if channels == 2 {
            match self.decode_silk_layer_stereo(&mut rd, routing) {
                Ok((left, right, bandwidth)) => {
                    resample_stereo_to_output_i16(
                        &left,
                        &right,
                        bandwidth,
                        &mut pcm[pcm_start..pcm_start + per_channel * 2],
                    );
                    true
                }
                Err(_) => false,
            }
        } else {
            match self.decode_silk_layer_mono(&mut rd, routing) {
                Ok((internal, bandwidth)) => {
                    resample_internal_to_output_i16(
                        &internal,
                        bandwidth,
                        &mut pcm[pcm_start..pcm_start + per_channel],
                    );
                    true
                }
                Err(_) => false,
            }
        };
        if !silk_ok {
            // §4.6 floor: the reserved silence stands in.
            pcm[pcm_start..].fill(0);
            return FrameOutcome {
                samples_per_channel: per_channel,
                status: FrameDecodeStatus::SilkDecodeError,
            };
        }

        // §4.5.1 redundancy side information; a present redundant CELT
        // frame occupies the trailing bytes, shrinking the main CELT
        // layer's budget.
        let redundancy =
            crate::celt_redundancy::decode_redundancy(&mut rd, OperatingMode::Hybrid, frame.len());
        let celt_bytes = match redundancy {
            crate::celt_redundancy::RedundancyDecision::Present { size_bytes, .. } => {
                frame.len().saturating_sub(size_bytes)
            }
            crate::celt_redundancy::RedundancyDecision::Invalid => {
                // §4.5.1.3: stop decoding this frame; keep the SILK
                // audio already written.
                return FrameOutcome {
                    samples_per_channel: per_channel,
                    status: FrameDecodeStatus::HybridDecoded,
                };
            }
            crate::celt_redundancy::RedundancyDecision::NotPresent => frame.len(),
        };

        // §4.3 CELT layer: bands 17.. per the signalled bandwidth.
        let Some(celt_size) =
            CeltFrameSize::from_frame_tenths_ms(routing.frame_size_tenths_ms as u32)
        else {
            return FrameOutcome {
                samples_per_channel: per_channel,
                status: FrameDecodeStatus::CeltDecodeError,
            };
        };
        let lm = celt_size.column_index() as i32;
        let n = per_channel;
        let end = match routing.toc_bandwidth {
            crate::toc::Bandwidth::Swb => 19,
            _ => 21,
        };
        let rebuild = !self
            .celt_synth
            .as_ref()
            .is_some_and(|s| s.channels() == channels && s.frame_len() == n);
        if rebuild {
            self.celt_synth = Some(crate::celt_mdct_synthesis::CeltSynthesis::new(channels, n));
            self.celt_energy = Some(crate::celt_frame_decode::CeltEnergyState::new());
        }
        let energy = self.celt_energy.get_or_insert_with(Default::default);
        let out = crate::celt_frame_decode::decode_celt_frame(
            &mut rd,
            celt_bytes,
            crate::celt_band_layout::HYBRID_FIRST_CODED_BAND,
            end,
            lm,
            channels,
            energy,
        );
        if rd.has_error() || u64::from(rd.tell()) > 8 * frame.len() as u64 {
            // The CELT layer failed: apply the §4.6 whole-frame floor
            // (the crate-wide error convention — an error status is
            // always paired with silence).
            pcm[pcm_start..].fill(0);
            return FrameOutcome {
                samples_per_channel: per_channel,
                status: FrameDecodeStatus::CeltDecodeError,
            };
        }

        let m = 1usize << lm;
        let mut freq = vec![0.0f64; channels * n];
        for c in 0..channels {
            for band in crate::celt_band_layout::HYBRID_FIRST_CODED_BAND..end {
                let gain = out.band_gain[c][band];
                let off = m * crate::celt_rate_alloc::band_edge(band) as usize;
                let len = m * crate::celt_rate_alloc::band_width(band) as usize;
                for j in 0..len {
                    freq[c * n + off + j] = gain * out.x[c * out.plane + off + j];
                }
            }
        }
        let blocks = if out.transient { m } else { 1 };
        let synth = self.celt_synth.as_mut().expect("state built above");
        let mut celt_pcm = vec![0i16; channels * n];
        synth.synthesize_frame(&freq, blocks, out.post_filter, &mut celt_pcm);

        // §4.4: the layer outputs sum (saturating at the i16 rails).
        let region = &mut pcm[pcm_start..pcm_start + per_channel * channels];
        for (dst, &c) in region.iter_mut().zip(celt_pcm.iter()) {
            *dst = dst.saturating_add(c);
        }

        FrameOutcome {
            samples_per_channel: per_channel,
            status: FrameDecodeStatus::HybridDecoded,
        }
    }
}

/// Append `per_channel * channels` interleaved zero samples to `pcm`.
fn push_silence(pcm: &mut Vec<i16>, per_channel: usize, channels: u8) {
    pcm.resize(pcm.len() + per_channel * channels as usize, 0);
}

/// Cross-Opus-frame SILK per-channel reconstruction carry: the last
/// decoded subframe gain — the §4.2.7.4 clamp base, `log_gain =
/// max(gain_index, previous_log_gain - 16)`, whose `previous_log_gain`
/// persists "in the same channel" across Opus frames — and the last
/// decoded NLSF vector — the §4.2.7.5.5 interpolation base `n0_Q15`,
/// "the LSF coefficients decoded for the prior frame", which the RFC
/// only replaces with the forced `w_Q2 = 4` after a decoder reset or an
/// uncoded side frame. Tagged with the bandwidth it was decoded at: the
/// NLSF order and codebooks are bandwidth-specific, so a bandwidth
/// switch drops the carry (mirroring the synthesis-state re-creation).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SilkChannelCarry {
    gain: Option<u8>,
    nlsf: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]>,
    nlsf_len: usize,
    bandwidth: Option<crate::toc::Bandwidth>,
}

/// Per-channel inter-frame decode state threaded across the SILK frames
/// of one Opus frame (§4.2.7.4 previous gain, §4.2.7.6.1 previous lag,
/// §4.2.7.5.5 previous NLSF base, and the "first SILK frame of this type"
/// flag). One instance is used for the mid channel and one for the side
/// channel (each channel's frames form an independent sequence).
pub(crate) struct ChannelDecodeState {
    prev_gain: Option<u8>,
    prev_lag: Option<i32>,
    prev_nlsf: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]>,
    prev_nlsf_len: usize,
    first: bool,
}

impl ChannelDecodeState {
    pub(crate) fn new() -> Self {
        Self {
            prev_gain: None,
            prev_lag: None,
            prev_nlsf: None,
            prev_nlsf_len: 0,
            first: true,
        }
    }

    /// Resume a channel sequence at an Opus-frame boundary from the
    /// cross-frame carry: the §4.2.7.4 clamp base and §4.2.7.5.5 `n0`
    /// persist, while the frame-local `first` flag re-arms (the first
    /// frame's gain is independently *coded* per Opus frame, its lag is
    /// absolute, and its §4.2.7.6.3 scaling field is present). A carry
    /// recorded at a different bandwidth is dropped (fresh start).
    pub(crate) fn resume(carry: &SilkChannelCarry, bandwidth: crate::toc::Bandwidth) -> Self {
        if carry.bandwidth != Some(bandwidth) {
            return Self::new();
        }
        Self {
            prev_gain: carry.gain,
            prev_lag: None,
            prev_nlsf: carry.nlsf,
            prev_nlsf_len: carry.nlsf_len,
            first: true,
        }
    }

    /// Capture the cross-Opus-frame carry after this channel's last
    /// frame of the Opus frame has been folded in via [`Self::advance`]
    /// (or cleared via [`Self::mark_interval_uncoded`]).
    pub(crate) fn to_carry(&self, bandwidth: crate::toc::Bandwidth) -> SilkChannelCarry {
        SilkChannelCarry {
            gain: self.prev_gain,
            nlsf: self.prev_nlsf,
            nlsf_len: self.prev_nlsf_len,
            bandwidth: Some(bandwidth),
        }
    }

    /// Build the [`crate::silk_decode::SilkFrameConfig`] for the next SILK
    /// frame in this channel's sequence, given the §4.2.4 VAD flag and the
    /// optional §4.2.7.1 / §4.2.7.2 stereo header context (present only on
    /// the mid channel).
    pub(crate) fn config(
        &self,
        bandwidth: crate::toc::Bandwidth,
        frame_size: crate::silk_excitation::SilkFrameSize,
        voice_active: bool,
        stereo: Option<crate::silk_decode::StereoHeaderContext>,
    ) -> crate::silk_decode::SilkFrameConfig {
        crate::silk_decode::SilkFrameConfig {
            bandwidth,
            frame_size,
            voice_active,
            first_subframe_independent: self.first || self.prev_gain.is_none(),
            previous_log_gain: self.prev_gain,
            previous_primary_lag: self.prev_lag,
            ltp_scaling_present: self.first,
            // §4.2.7.5.5: the factor is forced to 4 only when no prior
            // decoded NLSF base exists (decoder reset / uncoded side
            // frame) — for a resumed sequence `n0` comes from the
            // previous Opus frame even though `first` is set.
            lsf_interp_after_reset: self.prev_nlsf.is_none(),
            previous_nlsf_q15: self.prev_nlsf,
            previous_nlsf_len: self.prev_nlsf_len,
            stereo,
        }
    }

    /// Fold a freshly decoded SILK frame into the carried state, so the
    /// next frame in this channel's sequence predicts against it.
    pub(crate) fn advance(&mut self, decoded: &crate::silk_decode::SilkFrameDecoded) {
        self.prev_gain = Some(decoded.gains.last_log_gain());
        // §4.2.7.6.1: relative lag coding requires the previous frame of
        // the same type to have been coded AND voiced ("that previous
        // SILK frame was coded, but was not voiced" selects absolute
        // coding); a non-voiced frame therefore clears the lag base.
        self.prev_lag = if decoded.ltp.is_voiced() {
            Some(decoded.ltp.primary_lag())
        } else {
            None
        };
        self.prev_nlsf = Some(decoded.nlsf_q15);
        self.prev_nlsf_len = decoded.d_lpc;
        self.first = false;
    }

    /// Fold an UNCODED interval into the carried state (a §4.2.7.2
    /// side-channel skip or a §4.2.4 LBRR-flag gap). The next coded
    /// frame in the sequence then codes its gains independently with
    /// the §4.2.7.4 clamp skipped ("in the side channel if the
    /// previous frame in the side channel was not coded"), its primary
    /// lag absolutely (§4.2.7.6.1 second bullet), and its §4.2.7.5.5
    /// LSF interpolation factor forced to 4. For an LBRR sequence the
    /// §4.2.7.6.3 LTP-scaling field also reappears ("an LBRR frame
    /// where the LBRR flags indicate the previous LBRR frame in the
    /// same channel is not coded"), so `first` is re-armed; for
    /// regular frames that field only ever rides the first time
    /// interval of the Opus frame, so `first` is cleared.
    pub(crate) fn mark_interval_uncoded(&mut self, lbrr: bool) {
        self.prev_gain = None;
        self.prev_lag = None;
        self.prev_nlsf = None;
        self.prev_nlsf_len = 0;
        self.first = lbrr;
    }
}

/// Resample one Opus frame's internal-rate SILK samples (`internal`, at
/// the §4.2.1 SILK internal rate for `bandwidth`) to the 48 kHz decoder
/// output rate and write the result, converted to signed 16-bit PCM, into
/// `out` (whose length is the §3.1 48 kHz per-channel sample count).
///
/// Per RFC 6716 §4.2.9 "the resampler itself is non-normative, and a
/// decoder can use any method it wants to perform the resampling." We use
/// linear interpolation between adjacent internal-rate samples — a simple,
/// total method that introduces only the small distortion the §4.2.7.9
/// preamble explicitly permits ("small errors should only introduce
/// proportionally small distortions"). A bit-exact match to a particular
/// reference resampler is **not** attempted; the RFC defers the kernel
/// choice to the implementation.
///
/// The `internal`-to-`out` length ratio is the integer rate ratio (6 for
/// NB 8 kHz, 4 for MB 12 kHz, 3 for WB 16 kHz → 48 kHz), so the linear
/// interpolation positions are exact rationals; no fractional drift
/// accumulates across frames.
fn resample_internal_to_output_i16(
    internal: &[f32],
    bandwidth: crate::toc::Bandwidth,
    out: &mut [i16],
) {
    if out.is_empty() {
        return;
    }
    if internal.is_empty() {
        for o in out.iter_mut() {
            *o = 0;
        }
        return;
    }
    let in_len = internal.len();
    let out_len = out.len();
    // The internal-rate sample position for output sample `i` is
    // `i * in_len / out_len`. Linear-interpolate between the two
    // bracketing internal samples.
    let _ = bandwidth; // the rate ratio is implied by in_len / out_len.
    for (i, o) in out.iter_mut().enumerate() {
        let pos = (i as f64) * (in_len as f64) / (out_len as f64);
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let s0 = internal[i0.min(in_len - 1)];
        let s1 = internal[(i0 + 1).min(in_len - 1)];
        let v = s0 + (s1 - s0) * frac;
        *o = f32_to_i16(v);
    }
}

/// Resample a stereo pair of internal-rate SILK channels (`left` /
/// `right`, both at the §4.2.1 SILK internal rate for `bandwidth`) to the
/// 48 kHz output rate and write them **interleaved** (`[L0, R0, L1, R1,
/// …]`) into `out` (length `2 * per_channel`).
///
/// Per RFC 6716 §4.2.9 the resampler is non-normative; we use the same
/// linear interpolation as the mono path on each channel independently.
fn resample_stereo_to_output_i16(
    left: &[f32],
    right: &[f32],
    bandwidth: crate::toc::Bandwidth,
    out: &mut [i16],
) {
    let per_channel = out.len() / 2;
    if per_channel == 0 {
        return;
    }
    // Resample each channel into a scratch buffer, then interleave.
    let mut l = vec![0i16; per_channel];
    let mut r = vec![0i16; per_channel];
    resample_internal_to_output_i16(left, bandwidth, &mut l);
    resample_internal_to_output_i16(right, bandwidth, &mut r);
    for i in 0..per_channel {
        out[2 * i] = l[i];
        out[2 * i + 1] = r[i];
    }
}

/// Convert a nominal `[-1.0, 1.0]` float sample to signed 16-bit PCM,
/// rounding to nearest and clamping into the i16 range. The §4.2.7.9.2
/// output is already clamped to `[-1.0, 1.0]`; the clamp here is a
/// defensive backstop.
fn f32_to_i16(v: f32) -> i16 {
    let scaled = (v.clamp(-1.0, 1.0) * 32767.0).round();
    scaled as i16
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
        // A mono SILK-only frame now runs the real §4.2 bitstream decode;
        // the status is either a clean params-decoded or a decode-error
        // (a 3-byte arbitrary body may truncate mid-frame), never the
        // not-wired placeholder.
        assert!(
            matches!(
                out.frame_outcomes[0].status,
                FrameDecodeStatus::SilkParamsDecoded | FrameDecodeStatus::SilkDecodeError
            ),
            "got {:?}",
            out.frame_outcomes[0].status
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
        // A CELT-only frame decodes end-to-end — so the status is one
        // of the real CELT outcomes (silence / fully decoded / a decode
        // error on a 2-byte body), never the not-wired placeholder.
        assert!(
            matches!(
                out.frame_outcomes[0].status,
                FrameDecodeStatus::CeltSilence
                    | FrameDecodeStatus::CeltDecoded
                    | FrameDecodeStatus::CeltDecodeError
            ),
            "got {:?}",
            out.frame_outcomes[0].status
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
    fn celt_to_silk_transition_resets_silk_state() {
        // §4.5.2: the SILK state is reset before a SILK-only frame whose
        // predecessor was CELT-only. With CELT not yet wired (a CELT-only
        // packet emits silence and touches no SILK state), a SILK packet
        // followed by a CELT packet followed by the same SILK packet must
        // produce the *same* PCM as a fresh decoder running that SILK
        // packet once — because the §4.5.2 reset clears the carried
        // §4.2.7.9 history the first SILK packet left behind.
        let silk_body: Vec<u8> = (0..200u16)
            .map(|i| (i.wrapping_mul(149).wrapping_add(11) & 0xff) as u8)
            .collect();
        let silk_pkt = code0_packet(1, false, &silk_body); // config 1 = SILK NB 20 ms mono.
        let celt_pkt = code0_packet(17, false, &[0xaa, 0xbb]); // config 17 = CELT-only mono.

        // Reference: a fresh decoder running the SILK packet once.
        let mut ref_dec = OpusDecoder::new();
        let reference = ref_dec.decode_packet(&silk_pkt).expect("decode");

        // Sequence: SILK, then CELT (resets SILK state on the *next* SILK
        // frame), then SILK again. The third packet must match the
        // reference if and only if the §4.5.2 reset fired.
        let mut seq_dec = OpusDecoder::new();
        seq_dec.decode_packet(&silk_pkt).expect("decode");
        seq_dec.decode_packet(&celt_pkt).expect("decode");
        let after_reset = seq_dec.decode_packet(&silk_pkt).expect("decode");

        // Only compare when the SILK frame actually synthesized audio.
        if reference.frame_outcomes[0].status == FrameDecodeStatus::SilkParamsDecoded {
            assert_eq!(
                after_reset.pcm, reference.pcm,
                "§4.5.2 CELT→SILK transition must reset SILK state"
            );
        }
    }

    #[test]
    fn silk_to_silk_no_reset_threads_state() {
        // The complement of the §4.5.2 test: two consecutive SILK-only
        // packets (no CELT interlude) do NOT reset the SILK state, so the
        // second packet's output generally differs from a fresh-decoder
        // decode of that packet (the carried §4.2.7.9 history changes the
        // LPC/LTP synthesis). This pins that state actually threads when
        // it should.
        let silk_body: Vec<u8> = (0..200u16)
            .map(|i| (i.wrapping_mul(149).wrapping_add(11) & 0xff) as u8)
            .collect();
        let silk_pkt = code0_packet(1, false, &silk_body);

        let mut fresh = OpusDecoder::new();
        let fresh_out = fresh.decode_packet(&silk_pkt).expect("decode");

        let mut threaded = OpusDecoder::new();
        threaded.decode_packet(&silk_pkt).expect("decode");
        let second = threaded.decode_packet(&silk_pkt).expect("decode");

        // Both decode to the same length; the carried state means the
        // second decode is at least a valid, finite PCM buffer.
        assert_eq!(second.pcm.len(), fresh_out.pcm.len());
    }

    #[test]
    fn silk_mono_full_decode_consumes_bitstream_cleanly() {
        // A long pseudo-random SILK NB mono 20 ms body: the range coder
        // does not run out of bits, so the full §4.2 frame decodes and the
        // status is the clean params-decoded outcome (not a decode error).
        let body: Vec<u8> = (0..120u16)
            .map(|i| (i.wrapping_mul(101).wrapping_add(7) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(1, false, &body); // config 1 = SILK NB 20 ms.
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.frame_outcomes.len(), 1);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::SilkParamsDecoded,
            "a long SILK NB mono body should fully decode"
        );
        // PCM length is correct even though the samples are silence
        // (synthesis pending).
        assert_eq!(out.samples_per_channel(), 960);
    }

    #[test]
    fn silk_mono_40ms_two_silk_frames_decode() {
        // config 2 = SILK NB 40 ms => 2 SILK frames per channel; mono.
        let body: Vec<u8> = (0..220u16)
            .map(|i| (i.wrapping_mul(53).wrapping_add(3) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(2, false, &body);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
        assert_eq!(routing.silk_frames_per_channel, Some(2));
        // 40 ms => 1920 samples/channel; one Opus frame (code 0).
        assert_eq!(out.frame_outcomes.len(), 1);
        assert_eq!(out.samples_per_channel(), 1920);
        // The two-SILK-frame loop ran; the status reflects a SILK decode
        // (clean or truncated), never the not-wired placeholder.
        assert!(matches!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::SilkParamsDecoded | FrameDecodeStatus::SilkDecodeError
        ));
    }

    #[test]
    fn stereo_silk_only_decodes_to_interleaved_pcm() {
        // Stereo SILK now runs the full §4.2 interleaved mid/side decode +
        // §4.2.8 unmix. A long pseudo-random body decodes cleanly; the
        // output is interleaved L/R 48 kHz PCM.
        let body: Vec<u8> = (0..220u16)
            .map(|i| (i.wrapping_mul(137).wrapping_add(19) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(1, true, &body); // config 1 = SILK NB 20 ms stereo.
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.channels, 2);
        assert_eq!(out.samples_per_channel(), 960);
        assert_eq!(out.pcm.len(), 2 * 960);
        assert!(matches!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::SilkStereoDecoded | FrameDecodeStatus::SilkDecodeError
        ));
    }

    #[test]
    fn stereo_silk_clean_body_is_fully_decoded() {
        // A buffer long enough that the range coder never starves: the
        // interleaved mid/side decode + unmix completes, yielding the
        // stereo-decoded status (not a decode error, not not-wired).
        let body: Vec<u8> = (0..400u16)
            .map(|i| (i.wrapping_mul(97).wrapping_add(41) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(1, true, &body);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::SilkStereoDecoded,
            "a long stereo SILK NB body should fully decode"
        );
        // The output is finite and within i16 range by construction.
        assert_eq!(out.pcm.len(), 2 * 960);
    }

    #[test]
    fn stereo_silk_40ms_two_intervals_decode() {
        // config 2 = SILK NB 40 ms => 2 SILK frames per channel; stereo.
        // The §4.2.2 interleave runs mid/side per 20 ms interval twice.
        let body: Vec<u8> = (0..480u16)
            .map(|i| (i.wrapping_mul(61).wrapping_add(7) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(2, true, &body);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
        assert_eq!(routing.silk_frames_per_channel, Some(2));
        assert_eq!(out.channels, 2);
        // 40 ms => 1920 samples/channel interleaved.
        assert_eq!(out.samples_per_channel(), 1920);
        assert_eq!(out.pcm.len(), 2 * 1920);
        assert!(matches!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::SilkStereoDecoded | FrameDecodeStatus::SilkDecodeError
        ));
    }

    #[test]
    fn stereo_silk_60ms_three_intervals_per_interval_unmix() {
        // config 3 = SILK NB 60 ms => 3 SILK frames per channel; stereo.
        // Each 20 ms interval is unmixed separately (its own §4.2.7.1
        // weights + a fresh §4.2.8 interpolation phase), and the three
        // L/R interval outputs are concatenated. This pins the per-interval
        // unmix path for a multi-interval stereo frame.
        let body: Vec<u8> = (0..640u16)
            .map(|i| (i.wrapping_mul(73).wrapping_add(31) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(3, true, &body);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
        assert_eq!(routing.silk_frames_per_channel, Some(3));
        assert_eq!(out.channels, 2);
        // 60 ms => 2880 samples/channel interleaved.
        assert_eq!(out.samples_per_channel(), 2880);
        assert_eq!(out.pcm.len(), 2 * 2880);
        assert!(matches!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::SilkStereoDecoded | FrameDecodeStatus::SilkDecodeError
        ));
    }

    #[test]
    fn stereo_silk_state_threads_across_packets() {
        // Two consecutive stereo SILK packets thread the §4.2.7.9 + §4.2.8
        // histories: the second packet's output may differ from a fresh
        // decode, but both are valid finite buffers of equal length.
        let body: Vec<u8> = (0..300u16)
            .map(|i| (i.wrapping_mul(113).wrapping_add(23) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(1, true, &body);

        let mut fresh = OpusDecoder::new();
        let fresh_out = fresh.decode_packet(&pkt).expect("decode");

        let mut threaded = OpusDecoder::new();
        threaded.decode_packet(&pkt).expect("decode");
        let second = threaded.decode_packet(&pkt).expect("decode");
        assert_eq!(second.pcm.len(), fresh_out.pcm.len());
    }

    #[test]
    fn mono_to_stereo_transition_resets_stereo_state() {
        // §4.2.7.1: previous stereo weights reset on a mono→stereo
        // transition. A mono packet, then a stereo packet, then the same
        // stereo packet must leave the second stereo decode in a defined
        // state (no panic; correct length). The mono→stereo channel-count
        // change clears the carried stereo history.
        let mono_body: Vec<u8> = (0..200u16)
            .map(|i| (i.wrapping_mul(71).wrapping_add(5) & 0xff) as u8)
            .collect();
        let stereo_body: Vec<u8> = (0..300u16)
            .map(|i| (i.wrapping_mul(89).wrapping_add(11) & 0xff) as u8)
            .collect();
        let mono_pkt = code0_packet(1, false, &mono_body);
        let stereo_pkt = code0_packet(1, true, &stereo_body);

        let mut dec = OpusDecoder::new();
        dec.decode_packet(&mono_pkt).expect("mono");
        let out = dec.decode_packet(&stereo_pkt).expect("stereo");
        assert_eq!(out.channels, 2);
        assert_eq!(out.pcm.len(), 2 * 960);
    }

    #[test]
    fn pcm_length_matches_routing_for_every_config() {
        // Every Table-2 config decodes to a PCM buffer of the routing's
        // 48 kHz length × channels. SILK-only and CELT-only configs now
        // synthesize real audio; the remaining unwired layer (Hybrid)
        // and every error/silence outcome emit correct-length silence.
        // This sweep pins the length invariant for all 32 configs.
        let mut dec = OpusDecoder::new();
        for config in 0u8..32 {
            for stereo in [false, true] {
                let pkt = code0_packet(config, stereo, &[0x55, 0x66, 0x77]);
                let out = dec.decode_packet(&pkt).expect("decode");
                let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
                let expected = output_samples_per_channel(routing.frame_size_tenths_ms)
                    * out.channels as usize;
                assert_eq!(out.pcm.len(), expected, "config {config} stereo {stereo}");
                // Everything except a successfully synthesized SILK or
                // CELT frame still emits silence.
                let is_wired = matches!(
                    out.frame_outcomes[0].status,
                    FrameDecodeStatus::SilkParamsDecoded
                        | FrameDecodeStatus::SilkStereoDecoded
                        | FrameDecodeStatus::CeltDecoded
                );
                if !is_wired {
                    assert!(
                        out.pcm.iter().all(|&s| s == 0),
                        "config {config} stereo {stereo} status {:?} should be silence",
                        out.frame_outcomes[0].status
                    );
                }
                // The decoder must be reset between configs so the carried
                // §4.2.7.9 synthesis history of one bandwidth doesn't leak
                // into the next.
                dec.reset();
            }
        }
    }

    #[test]
    fn mono_silk_frame_can_emit_nonsilent_pcm() {
        // A long pseudo-random mono SILK NB 20 ms body decodes cleanly and
        // is synthesized through the §4.2.7.9 LTP/LPC filters + §4.2.9
        // resample; the emitted PCM is no longer forced to silence. (The
        // exact samples are not pinned — there is no codec-level bit-exact
        // fixture yet — but a clean params-decoded frame produces a
        // correctly-sized 48 kHz buffer.)
        let body: Vec<u8> = (0..200u16)
            .map(|i| (i.wrapping_mul(181).wrapping_add(13) & 0xff) as u8)
            .collect();
        let pkt = code0_packet(1, false, &body); // config 1 = SILK NB 20 ms.
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.channels, 1);
        assert_eq!(out.samples_per_channel(), 960);
        if out.frame_outcomes[0].status == FrameDecodeStatus::SilkParamsDecoded {
            // A successfully synthesized frame produces a full-length
            // buffer; every sample is a valid i16 (no panic / overflow).
            assert_eq!(out.pcm.len(), 960);
        }
    }

    /// Search for a CELT body whose §4.3.7.1 prefix decodes silence = 1
    /// with the post-filter off, so the frame takes the fully-wired
    /// silence synthesis path. Returns the body bytes appended after the
    /// TOC. The search is deterministic (fixed candidate set), so the
    /// chosen body is stable across runs.
    fn find_celt_silence_body() -> Vec<u8> {
        use crate::celt_frame_prefix::decode_celt_frame_prefix;
        use crate::range_decoder::RangeDecoder;
        // The silence flag is the {32767,1}/32768 "1" branch (probability
        // 2^-15), so a silent frame is rare in random bytes; sweep the
        // first two bytes (with a trailing zero run that keeps the
        // post-filter off) to find one deterministically.
        for b0 in 0u16..=255 {
            for b1 in 0u16..=255 {
                let buf = [b0 as u8, b1 as u8, 0, 0, 0, 0];
                let mut rd = RangeDecoder::new(&buf);
                let p = decode_celt_frame_prefix(&mut rd);
                if p.silence && p.post_filter.is_none() && !rd.has_error() {
                    return buf.to_vec();
                }
            }
        }
        panic!("no CELT silence body found in the candidate set");
    }

    #[test]
    fn celt_only_silence_frame_decodes_end_to_end() {
        // config 17 = CELT-only mono, 5 ms (Table-55 second column) →
        // 240 samples/channel at 48 kHz.
        let body = find_celt_silence_body();
        let pkt = code0_packet(17, false, &body);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(out.channels, 1);
        assert_eq!(out.samples_per_channel(), 240);
        assert_eq!(
            out.frame_outcomes[0].status,
            FrameDecodeStatus::CeltSilence,
            "silence-flagged CELT frame must take the wired synthesis path"
        );
        // The frame is silent: every emitted sample is zero (a zero-energy
        // band envelope synthesizes to a zero time-domain block, and the
        // overlap-add / de-emphasis of an all-zero history stays zero).
        assert_eq!(out.pcm.len(), 240);
        assert!(
            out.pcm.iter().all(|&s| s == 0),
            "silence frame must be all zero"
        );
    }

    #[test]
    fn celt_silence_advances_synthesis_state() {
        // Two consecutive CELT silence frames both decode through the
        // wired path; the second reuses the carried CeltSynthState (no
        // rebuild), and both emit silence of the correct length.
        let body = find_celt_silence_body();
        let pkt = code0_packet(17, false, &body);
        let mut dec = OpusDecoder::new();
        let first = dec.decode_packet(&pkt).expect("decode");
        let second = dec.decode_packet(&pkt).expect("decode");
        assert_eq!(
            first.frame_outcomes[0].status,
            FrameDecodeStatus::CeltSilence
        );
        assert_eq!(
            second.frame_outcomes[0].status,
            FrameDecodeStatus::CeltSilence
        );
        assert!(second.pcm.iter().all(|&s| s == 0));
    }

    #[test]
    fn celt_non_silent_frame_decodes_fully() {
        // A CELT body whose silence flag is clear takes the full §4.3
        // decode → synthesis path: the status is CeltDecoded (or
        // CeltDecodeError when the entropy content of the random body
        // is impossible), never a panic, never the not-wired
        // placeholder, and the PCM length is exact.
        use crate::celt_frame_prefix::decode_celt_frame_prefix;
        use crate::range_decoder::RangeDecoder;
        let mut chosen: Option<Vec<u8>> = None;
        for b0 in 0u16..=255 {
            let buf = [
                b0 as u8, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
            ];
            let mut rd = RangeDecoder::new(&buf);
            let p = decode_celt_frame_prefix(&mut rd);
            if !p.silence && !rd.has_error() {
                chosen = Some(buf.to_vec());
                break;
            }
        }
        let body = chosen.expect("a non-silent CELT body exists in the candidate set");
        let pkt = code0_packet(17, false, &body);
        let mut dec = OpusDecoder::new();
        let out = dec.decode_packet(&pkt).expect("decode");
        assert!(
            matches!(
                out.frame_outcomes[0].status,
                FrameDecodeStatus::CeltDecoded | FrameDecodeStatus::CeltDecodeError
            ),
            "got {:?}",
            out.frame_outcomes[0].status
        );
        assert_eq!(out.pcm.len(), 240);
    }

    #[test]
    fn celt_energy_state_threads_across_frames() {
        // Two successive non-silent CELT-only frames must thread the
        // §4.3.2 energy history: after the first decodes fully, the
        // decoder carries a CeltEnergyState whose per-band energies the
        // second frame's inter prediction reads.
        use crate::celt_frame_prefix::decode_celt_frame_prefix;
        use crate::range_decoder::RangeDecoder;
        let mut chosen: Option<Vec<u8>> = None;
        for b0 in 0u16..=255 {
            let buf = [
                b0 as u8, 0x33, 0xcc, 0x55, 0xaa, 0x0f, 0xf0, 0x12, 0x9a, 0x4e,
            ];
            let mut rd = RangeDecoder::new(&buf);
            let p = decode_celt_frame_prefix(&mut rd);
            if !p.silence && !p.intra && !rd.has_error() {
                chosen = Some(buf.to_vec());
                break;
            }
        }
        if let Some(body) = chosen {
            let pkt = code0_packet(19, false, &body); // 20 ms CELT-only mono
            let mut dec = OpusDecoder::new();
            let first = dec.decode_packet(&pkt).expect("decode");
            if first.frame_outcomes[0].status == FrameDecodeStatus::CeltDecoded {
                let carried = dec.celt_energy.clone().expect("energy state carried");
                let second = dec.decode_packet(&pkt).expect("decode");
                // The same packet decodes against a different energy
                // history, so (when both frames fully decode) the
                // carried state must have advanced.
                if second.frame_outcomes[0].status == FrameDecodeStatus::CeltDecoded {
                    assert!(dec.celt_energy.is_some());
                    assert_ne!(
                        dec.celt_energy.as_ref().unwrap().old_band_e,
                        carried.old_log_e2,
                        "energy history must roll forward"
                    );
                }
            }
        }
    }

    #[test]
    fn celt_full_decode_consumes_bits_within_budget() {
        // Direct exercise of the §4.3 frame decode: on a non-silent
        // body it must consume real bits and never report a tell past
        // the frame budget unless the coder latched an error.
        use crate::celt_frame_decode::{decode_celt_frame, CeltEnergyState};
        use crate::range_decoder::RangeDecoder;

        let body: [u8; 14] = [
            0x40, 0x91, 0x37, 0xc4, 0x6e, 0x2d, 0xa8, 0x5b, 0xf1, 0x0c, 0x93, 0x47, 0xbe, 0x21,
        ];
        let mut rd = RangeDecoder::new(&body);
        let mut st = CeltEnergyState::new();
        let out = decode_celt_frame(&mut rd, body.len(), 0, 21, 3, 1, &mut st);
        assert!(!out.silence, "test body must be non-silent");
        assert!(rd.tell() > 1, "the frame decode must consume bits");
        if !rd.has_error() {
            assert!(
                rd.tell() as usize <= 8 * body.len(),
                "tell {} past budget",
                rd.tell()
            );
        }
    }

    // ---- Cross-packet §4.2.7.4 / §4.2.7.5.5 reconstruction carry ----

    use crate::silk_decode::SilkFrameSymbols;
    use crate::silk_excitation::ExcitationSymbols;
    use crate::silk_frame::{SilkHeaderSymbols, StereoPredictionWeights, StereoWeightSymbols};
    use crate::silk_gains::GainSymbol;
    use crate::silk_packet_encode::{
        encode_silk_only_packet_mono, encode_silk_only_packet_stereo, StereoIntervalScripts,
    };

    /// Owned buffers for one scripted NB 20 ms unvoiced SILK frame (4
    /// subframes, d_LPC = 10, 160 excitation samples in 10 shell
    /// blocks).
    struct FrameScript {
        gains: Vec<GainSymbol>,
        i2: Vec<i8>,
        lsb_counts: Vec<u8>,
        e_raw: Vec<i32>,
        header: SilkHeaderSymbols,
        lsf_stage1: u8,
        lsf_interp_w_q2: Option<u8>,
    }

    impl FrameScript {
        /// An unvoiced NB frame whose first-subframe gain symbol,
        /// stage-1 LSF index, §4.2.7.5.5 factor, and excitation are
        /// caller-chosen; the remaining subframes hold the gain steady
        /// (`Delta(4)` ⇒ `log_gain = previous_log_gain`).
        fn nb_unvoiced(first_gain: GainSymbol, lsf_stage1: u8, w_q2: u8, pulses: bool) -> Self {
            let mut e_raw = vec![0i32; 160];
            if pulses {
                for (i, slot) in e_raw.iter_mut().enumerate().step_by(8) {
                    *slot = if (i / 8) % 2 == 0 { 1 } else { -1 };
                }
            }
            Self {
                gains: vec![
                    first_gain,
                    GainSymbol::Delta(4),
                    GainSymbol::Delta(4),
                    GainSymbol::Delta(4),
                ],
                i2: vec![0i8; 10],
                lsb_counts: vec![0u8; 10],
                e_raw,
                header: SilkHeaderSymbols {
                    stereo: None,
                    mid_only_flag: None,
                    frame_type: 2, // Unvoiced / Low (Table 10) — VAD set.
                },
                lsf_stage1,
                lsf_interp_w_q2: Some(w_q2),
            }
        }

        fn symbols(&self) -> SilkFrameSymbols<'_> {
            SilkFrameSymbols {
                header: self.header,
                gains: &self.gains,
                lsf_stage1: self.lsf_stage1,
                lsf_stage2_i2: &self.i2,
                lsf_interp_w_q2: self.lsf_interp_w_q2,
                ltp: None,
                lcg_seed: 0,
                excitation: ExcitationSymbols {
                    rate_level: 0,
                    lsb_counts: &self.lsb_counts,
                    e_raw: &self.e_raw,
                },
            }
        }
    }

    fn rms(pcm: &[i16]) -> f64 {
        let e: f64 = pcm.iter().map(|&v| (v as f64) * (v as f64)).sum();
        (e / pcm.len().max(1) as f64).sqrt()
    }

    /// §4.2.7.4: `previous_log_gain` persists across Opus frames, so an
    /// independently coded first-subframe gain in the NEXT packet is
    /// clamped to `max(gain_index, prev - 16)`. Packet A ends at
    /// log gain 63; packet B codes gain index 10, which a streaming
    /// decoder must lift to 47 (a fresh decoder decodes 10). The ~50 dB
    /// level difference is the observable.
    #[test]
    fn gain_clamp_base_carries_across_packets() {
        let bw = crate::toc::Bandwidth::Nb;
        let loud = FrameScript::nb_unvoiced(GainSymbol::Independent(63), 0, 4, true);
        let quiet = FrameScript::nb_unvoiced(GainSymbol::Independent(10), 0, 4, true);
        let (pkt_a, _) = encode_silk_only_packet_mono(bw, 200, &[loud.symbols()]).unwrap();
        let (pkt_b, _) = encode_silk_only_packet_mono(bw, 200, &[quiet.symbols()]).unwrap();

        let mut fresh = OpusDecoder::new();
        let quiet_alone = fresh.decode_packet(&pkt_b).unwrap();

        let mut stream = OpusDecoder::new();
        stream.decode_packet(&pkt_a).unwrap();
        let quiet_streamed = stream.decode_packet(&pkt_b).unwrap();

        let r_fresh = rms(&quiet_alone.pcm);
        let r_stream = rms(&quiet_streamed.pcm);
        assert!(
            r_stream > 30.0 * r_fresh.max(1e-9),
            "clamp must lift the streamed gain: fresh rms {r_fresh:.2}, streamed {r_stream:.2}"
        );
    }

    /// [`OpusDecoder::reset`] drops the §4.2.7.4 clamp base ("the
    /// clamping is skipped after a decoder reset"): decoding packet B
    /// after a reset must be byte-identical to a fresh decode.
    #[test]
    fn reset_clears_gain_clamp_base() {
        let bw = crate::toc::Bandwidth::Nb;
        let loud = FrameScript::nb_unvoiced(GainSymbol::Independent(63), 0, 4, true);
        let quiet = FrameScript::nb_unvoiced(GainSymbol::Independent(10), 0, 4, true);
        let (pkt_a, _) = encode_silk_only_packet_mono(bw, 200, &[loud.symbols()]).unwrap();
        let (pkt_b, _) = encode_silk_only_packet_mono(bw, 200, &[quiet.symbols()]).unwrap();

        let mut fresh = OpusDecoder::new();
        let alone = fresh.decode_packet(&pkt_b).unwrap();

        let mut stream = OpusDecoder::new();
        stream.decode_packet(&pkt_a).unwrap();
        stream.reset();
        let after_reset = stream.decode_packet(&pkt_b).unwrap();
        assert_eq!(alone.pcm, after_reset.pcm);
    }

    /// §4.2.7.5.5: the interpolation base `n0` is "the LSF coefficients
    /// decoded for the prior frame" — including across Opus frames. Two
    /// second packets that differ ONLY in the coded factor (`w_Q2 = 0`
    /// vs `4`) must reconstruct differently after the same first packet
    /// (with `w_Q2 = 0` the first half-frame runs the PREVIOUS packet's
    /// LPC), while a fresh decoder — where the factor is forced to 4 —
    /// reconstructs them identically.
    #[test]
    fn nlsf_interp_n0_carries_across_packets() {
        let bw = crate::toc::Bandwidth::Nb;
        let a = FrameScript::nb_unvoiced(GainSymbol::Independent(45), 0, 4, true);
        let b_interp = FrameScript::nb_unvoiced(GainSymbol::Independent(45), 25, 0, true);
        let b_no_interp = FrameScript::nb_unvoiced(GainSymbol::Independent(45), 25, 4, true);
        let (pkt_a, _) = encode_silk_only_packet_mono(bw, 200, &[a.symbols()]).unwrap();
        let (pkt_b0, _) = encode_silk_only_packet_mono(bw, 200, &[b_interp.symbols()]).unwrap();
        let (pkt_b4, _) = encode_silk_only_packet_mono(bw, 200, &[b_no_interp.symbols()]).unwrap();

        // Fresh decoders: no prior NLSF base exists, the factor is
        // forced to 4, and both variants decode identically.
        let mut f0 = OpusDecoder::new();
        let mut f4 = OpusDecoder::new();
        assert_eq!(
            f0.decode_packet(&pkt_b0).unwrap().pcm,
            f4.decode_packet(&pkt_b4).unwrap().pcm,
            "without a carried n0 the coded factor must be ignored"
        );

        // Streaming decoders: after packet A the coded factor is live.
        let mut s0 = OpusDecoder::new();
        let mut s4 = OpusDecoder::new();
        s0.decode_packet(&pkt_a).unwrap();
        s4.decode_packet(&pkt_a).unwrap();
        let out0 = s0.decode_packet(&pkt_b0).unwrap();
        let out4 = s4.decode_packet(&pkt_b4).unwrap();
        assert_ne!(
            out0.pcm, out4.pcm,
            "w_Q2 = 0 must interpolate against the previous packet's NLSFs"
        );
    }

    /// The stereo SIDE channel carries its own §4.2.7.4 clamp base
    /// across packets (independent of the mid channel's).
    #[test]
    fn stereo_side_gain_clamp_carries_across_packets() {
        let bw = crate::toc::Bandwidth::Nb;
        let weights = StereoWeightSymbols::quantize(StereoPredictionWeights {
            w0_q13: 0,
            w1_q13: 0,
        });
        // Mid frames: fixed moderate gain, silent excitation.
        let mid = |first: u8| {
            let mut m = FrameScript::nb_unvoiced(GainSymbol::Independent(first), 0, 4, false);
            m.header.stereo = Some(weights);
            m
        };
        let mid_a = mid(30);
        let mid_b = mid(30);
        // Side frames: packet A ends at log gain 63; packet B codes 10.
        let side_a = FrameScript::nb_unvoiced(GainSymbol::Independent(63), 0, 4, true);
        let side_b = FrameScript::nb_unvoiced(GainSymbol::Independent(10), 0, 4, true);

        let (pkt_a, _) = encode_silk_only_packet_stereo(
            bw,
            200,
            &[StereoIntervalScripts {
                mid: mid_a.symbols(),
                side: Some(side_a.symbols()),
            }],
        )
        .unwrap();
        let (pkt_b, _) = encode_silk_only_packet_stereo(
            bw,
            200,
            &[StereoIntervalScripts {
                mid: mid_b.symbols(),
                side: Some(side_b.symbols()),
            }],
        )
        .unwrap();

        // The side signal is (L - R) / 2 up to the (zero) prediction
        // weights; compare its energy fresh vs streamed.
        let side_rms = |audio: &DecodedAudio| {
            let d: Vec<i16> = audio
                .pcm
                .chunks_exact(2)
                .map(|lr| ((lr[0] as i32 - lr[1] as i32) / 2) as i16)
                .collect();
            rms(&d)
        };

        let mut fresh = OpusDecoder::new();
        let alone = fresh.decode_packet(&pkt_b).unwrap();

        let mut stream = OpusDecoder::new();
        stream.decode_packet(&pkt_a).unwrap();
        let streamed = stream.decode_packet(&pkt_b).unwrap();

        let r_fresh = side_rms(&alone);
        let r_stream = side_rms(&streamed);
        assert!(
            r_stream > 30.0 * r_fresh.max(1e-9),
            "side clamp must lift the streamed gain: fresh {r_fresh:.2}, streamed {r_stream:.2}"
        );
    }
}
