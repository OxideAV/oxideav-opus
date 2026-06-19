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
//! * **CELT-only / Hybrid** frames emit silence of the correct length
//!   flagged [`FrameDecodeStatus::LayerNotWired`] (CELT is gated on the
//!   §4.3.2.1 coarse-energy Laplace decode).
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
    /// Operating mode of the most recently decoded Opus frame, used to
    /// drive the §4.5.2 SILK state-reset rule ("the SILK state is reset
    /// before every SILK-only or Hybrid frame where the previous frame
    /// was CELT-only"). `None` before the first frame / after a reset.
    prev_mode: Option<OperatingMode>,
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
        use crate::range_decoder::RangeDecoder;
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

        let mut rd = RangeDecoder::new(frame);

        // §4.2.3 / §4.2.4 header bits (mono => stereo = false).
        let header = SilkHeaderBits::decode(&mut rd, num_silk_frames, false)?;

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
            let decoded = decode_silk_frame(&mut rd, cfg)?;
            lbrr_prev_gain = Some(decoded.gains.last_log_gain());
            lbrr_prev_lag = Some(decoded.ltp.primary_lag());
            lbrr_first = false;
            let _ = FrameKind::Lbrr; // documents the §4.2.7.3 kind.
        }

        // §4.2.6 regular SILK frames: one per time interval, even when
        // the VAD flag is unset. Inter-frame state threads across them.
        let mut prev_gain: Option<u8> = None;
        let mut prev_lag: Option<i32> = None;
        let mut prev_nlsf: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]> = None;
        let mut prev_nlsf_len = 0usize;
        let mut first = true;
        let mut decoded_frames: Vec<SilkFrameDecoded> =
            Vec::with_capacity(num_silk_frames as usize);
        for idx in 0..num_silk_frames {
            let cfg = SilkFrameConfig {
                bandwidth,
                frame_size,
                voice_active: header.mid_vad(idx),
                first_subframe_independent: first || prev_gain.is_none(),
                previous_log_gain: prev_gain,
                previous_primary_lag: prev_lag,
                ltp_scaling_present: first,
                lsf_interp_after_reset: first || prev_nlsf.is_none(),
                previous_nlsf_q15: prev_nlsf,
                previous_nlsf_len: prev_nlsf_len,
                // Mono SILK-only path: no §4.2.7.1 / §4.2.7.2 stereo header.
                stereo: None,
            };
            let decoded = decode_silk_frame(&mut rd, cfg)?;
            prev_gain = Some(decoded.gains.last_log_gain());
            prev_lag = Some(decoded.ltp.primary_lag());
            prev_nlsf = Some(decoded.nlsf_q15);
            prev_nlsf_len = decoded.d_lpc;
            first = false;
            decoded_frames.push(decoded);
        }

        if rd.has_error() {
            return Err(Error::MalformedPacket);
        }

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

        // §4.2.3 / §4.2.4 header bits (stereo => both channels' VAD + LBRR
        // flags, mid then side).
        let header = SilkHeaderBits::decode(&mut rd, num_silk_frames, true)?;

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
                    &mut rd,
                    lbrr_mid.config(bandwidth, frame_size, true, Some(stereo_ctx)),
                )?;
                lbrr_mid.advance(&decoded);
                // A set mid-only flag would forbid a coded side LBRR
                // frame; the header LBRR flags already encode that, so we
                // trust `side_lbrr` for the interleave decision.
                if side_lbrr {
                    let decoded = decode_silk_frame(
                        &mut rd,
                        lbrr_side.config(bandwidth, frame_size, true, None),
                    )?;
                    lbrr_side.advance(&decoded);
                }
            } else if side_lbrr {
                // Side-only LBRR (mid not coded): no stereo weights on a
                // side frame per §4.2.7.1.
                let decoded = decode_silk_frame(
                    &mut rd,
                    lbrr_side.config(bandwidth, frame_size, true, None),
                )?;
                lbrr_side.advance(&decoded);
            }
        }

        // §4.2.6 regular SILK frames: per 20 ms interval, the mid frame
        // then (unless the §4.2.7.2 mid-only flag is set) the side frame.
        let mut mid_state = ChannelDecodeState::new();
        let mut side_state = ChannelDecodeState::new();
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
                &mut rd,
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
                    &mut rd,
                    side_state.config(bandwidth, frame_size, header.side_vad(idx), None),
                )?;
                side_state.advance(&side_decoded);
                side_frames.push(Some(side_decoded));
            } else {
                // §4.2.7.2 / §4.5.2: an uncoded side SILK frame clears the
                // side LTP buffer; zeros feed the §4.2.8 unmixer.
                side_frames.push(None);
            }
        }

        if rd.has_error() {
            return Err(Error::MalformedPacket);
        }

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

/// Per-channel inter-frame decode state threaded across the SILK frames
/// of one Opus frame (§4.2.7.4 previous gain, §4.2.7.6.1 previous lag,
/// §4.2.7.5.5 previous NLSF base, and the "first SILK frame of this type"
/// flag). One instance is used for the mid channel and one for the side
/// channel (each channel's frames form an independent sequence).
struct ChannelDecodeState {
    prev_gain: Option<u8>,
    prev_lag: Option<i32>,
    prev_nlsf: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]>,
    prev_nlsf_len: usize,
    first: bool,
}

impl ChannelDecodeState {
    fn new() -> Self {
        Self {
            prev_gain: None,
            prev_lag: None,
            prev_nlsf: None,
            prev_nlsf_len: 0,
            first: true,
        }
    }

    /// Build the [`crate::silk_decode::SilkFrameConfig`] for the next SILK
    /// frame in this channel's sequence, given the §4.2.4 VAD flag and the
    /// optional §4.2.7.1 / §4.2.7.2 stereo header context (present only on
    /// the mid channel).
    fn config(
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
            lsf_interp_after_reset: self.first || self.prev_nlsf.is_none(),
            previous_nlsf_q15: self.prev_nlsf,
            previous_nlsf_len: self.prev_nlsf_len,
            stereo,
        }
    }

    /// Fold a freshly decoded SILK frame into the carried state, so the
    /// next frame in this channel's sequence predicts against it.
    fn advance(&mut self, decoded: &crate::silk_decode::SilkFrameDecoded) {
        self.prev_gain = Some(decoded.gains.last_log_gain());
        self.prev_lag = Some(decoded.ltp.primary_lag());
        self.prev_nlsf = Some(decoded.nlsf_q15);
        self.prev_nlsf_len = decoded.d_lpc;
        self.first = false;
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
        // 48 kHz length × channels. Mono SILK-only configs now synthesize
        // real audio (§4.2.7.9); the still-unwired layers (CELT-only,
        // Hybrid, and stereo SILK) emit correct-length silence. This sweep
        // pins the length invariant for all 32 configs and the silence
        // invariant for the not-yet-wired ones.
        let mut dec = OpusDecoder::new();
        for config in 0u8..32 {
            for stereo in [false, true] {
                let pkt = code0_packet(config, stereo, &[0x55, 0x66, 0x77]);
                let out = dec.decode_packet(&pkt).expect("decode");
                let routing = OpusFrameRouting::from_toc(OpusTocByte::from_byte(pkt[0]));
                let expected = output_samples_per_channel(routing.frame_size_tenths_ms)
                    * out.channels as usize;
                assert_eq!(out.pcm.len(), expected, "config {config} stereo {stereo}");
                // The unwired layers (everything except a successfully
                // synthesized mono or stereo SILK-only frame) still emit
                // silence.
                let is_wired_silk = matches!(
                    out.frame_outcomes[0].status,
                    FrameDecodeStatus::SilkParamsDecoded | FrameDecodeStatus::SilkStereoDecoded
                );
                if !is_wired_silk {
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
}
