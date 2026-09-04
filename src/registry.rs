//! Framework registry integration: the [`oxideav_core::Decoder`] /
//! [`oxideav_core::Encoder`] adapters over this crate's native Opus
//! machinery, and the dual-API `make_decoder` / `make_encoder`
//! factories that [`crate::register`] wires into the
//! [`oxideav_core::registry::CodecRegistry`].
//!
//! The adapters follow the crate's decode/encode conventions:
//!
//! * **Decode** ([`OpusStreamDecoder`]): each [`Packet`] is one Opus
//!   packet (RFC 6716 §3). The stream geometry comes from
//!   [`CodecParameters`] — when `extradata` carries an RFC 7845 §5.1
//!   identification header (`OpusHead`) it is authoritative for the
//!   output channel count, the §5.1.1 channel mapping (multichannel
//!   streams decode through [`crate::multistream::MultistreamDecoder`]),
//!   the pre-skip, and the §5.1 output gain; without extradata the
//!   parameter channel count (default: stereo) selects a plain 1-/2-
//!   channel [`crate::decoder::OpusDecoder`]. `sample_rate` selects
//!   any §4.2.9 supported output rate (8 / 12 / 16 / 24 / 48 kHz —
//!   the reduced-rate decode surface, pre-skip rescaled to the
//!   output-rate timeline). A zero-length packet payload is treated
//!   as a lost packet and concealed per RFC 6716 §4.4
//!   ([`crate::decoder::OpusDecoder::conceal_loss`]).
//! * **Encode** ([`OpusStreamEncoder`]): 48 kHz interleaved S16 input
//!   frames are re-blocked into 20 ms Opus frames and encoded through
//!   the unified [`crate::opus_encoder::OpusEncoder`] (bitrate-driven
//!   mode/bandwidth ladder, §4.5 transitions, every §2.1 knob)
//!   at the requested `bit_rate` (RFC 6716 §2.1.8), one packet per
//!   frame. `output_params` carries a composed `OpusHead` in
//!   `extradata` so container layers can encapsulate the stream per
//!   RFC 7845.
//!
//! Both `make_decoder` and `make_encoder` keep the historical direct
//! factory signatures (`fn(&CodecParameters) -> Result<Box<dyn …>>`)
//! alongside the `register!` path, so callers can construct codecs
//! without a registry and registry resolution constructs the same
//! implementations.

use std::collections::VecDeque;

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Frame, OptionField, OptionKind, OptionValue, Packet,
    Rational, SampleFormat, TimeBase,
};

use crate::decoder::{DecodedAudio, OpusDecoder, OUTPUT_SAMPLE_RATE_HZ};
use crate::multistream::MultistreamDecoder;
use crate::opus_head::{apply_output_gain, OpusHead, PreSkip};

/// The registry codec id this crate claims.
pub(crate) const CODEC_ID: &str = "opus";

fn codec_id() -> CodecId {
    CodecId::new(CODEC_ID)
}

/// Map a crate-level decode error onto the framework error space.
fn map_err(e: crate::Error) -> oxideav_core::Error {
    oxideav_core::Error::invalid(format!("{e}"))
}

// ─────────────────────────── decoder ───────────────────────────

/// The inner decode engine: a plain 1-/2-channel packet decoder, or
/// the RFC 7845 §5.1.1 multistream assembly for mapped multichannel
/// streams.
#[derive(Debug)]
enum DecodeEngine {
    Single(Box<OpusDecoder>),
    Multi(MultistreamDecoder),
}

/// [`oxideav_core::Decoder`] adapter over this crate's Opus decode
/// machinery. Build via [`make_decoder`] (directly or through the
/// registry).
#[derive(Debug)]
pub struct OpusStreamDecoder {
    id: CodecId,
    engine: DecodeEngine,
    /// Output channel count the adapter emits (every frame, regardless
    /// of the per-packet coded channel count).
    channels: u8,
    /// RFC 7845 §5.1 pre-skip accumulator (output-rate samples).
    pre_skip: PreSkip,
    /// RFC 7845 §5.1 output gain (Q7.8 dB); 0 = unity.
    gain_q7_8: i16,
    /// Decoded frames not yet pulled by `receive_frame`.
    queue: VecDeque<AudioFrame>,
    /// Set by `flush`: once the queue drains, `receive_frame` reports Eof.
    eof: bool,
}

impl OpusStreamDecoder {
    fn from_params(params: &CodecParameters) -> oxideav_core::Result<Self> {
        let rate = params.sample_rate.unwrap_or(OUTPUT_SAMPLE_RATE_HZ);
        if !crate::silk_resampler::is_supported_output_rate(rate) {
            return Err(oxideav_core::Error::unsupported(format!(
                "opus: unsupported output sample rate {rate} Hz                  (8000 / 12000 / 16000 / 24000 / 48000)"
            )));
        }
        match params.sample_format {
            None | Some(SampleFormat::S16) => {}
            Some(f) => {
                return Err(oxideav_core::Error::unsupported(format!(
                    "opus: unsupported sample format {f:?} (S16 interleaved only)"
                )));
            }
        }

        let (engine, channels, pre_skip, gain_q7_8);
        if !params.extradata.is_empty() {
            // RFC 7845 §5.1 identification header: authoritative for
            // channel geometry, pre-skip, and output gain.
            let head = OpusHead::parse(&params.extradata)
                .map_err(|e| oxideav_core::Error::invalid(format!("opus extradata: {e}")))?;
            if let Some(c) = params.channels {
                if c != u16::from(head.channel_count) {
                    return Err(oxideav_core::Error::invalid(format!(
                        "opus: parameter channel count {c} contradicts the OpusHead \
                         channel count {}",
                        head.channel_count
                    )));
                }
            }
            channels = head.channel_count;
            // RFC 7845 §5.1 pre-skip counts 48 kHz samples; at a
            // reduced output rate the discarded interval is the same
            // duration, rounded up to whole output samples.
            let pre_skip_scaled = (u64::from(head.pre_skip) * u64::from(rate))
                .div_ceil(u64::from(OUTPUT_SAMPLE_RATE_HZ));
            pre_skip = PreSkip::new(pre_skip_scaled as u16);
            gain_q7_8 = head.output_gain_q7_8;
            // A single family-0/1 stream with ≤ 2 channels decodes
            // through the plain packet decoder (identical output, no
            // §3 self-delimited split); anything mapped goes through
            // the multistream assembly.
            engine = if head.mapping.stream_count == 1 && head.channel_count <= 2 {
                DecodeEngine::Single(Box::new(
                    OpusDecoder::with_output_rate(rate).expect("rate validated above"),
                ))
            } else {
                DecodeEngine::Multi(
                    MultistreamDecoder::from_head_with_output_rate(&head, rate)
                        .expect("rate validated above"),
                )
            };
        } else {
            let c = params.channels.unwrap_or(2);
            if c == 0 || c > 2 {
                return Err(oxideav_core::Error::unsupported(format!(
                    "opus: {c} channels need an RFC 7845 §5.1.1 channel mapping in extradata"
                )));
            }
            channels = c as u8;
            pre_skip = PreSkip::new(0);
            gain_q7_8 = 0;
            engine = DecodeEngine::Single(Box::new(
                OpusDecoder::with_output_rate(rate).expect("rate validated above"),
            ));
        }

        Ok(Self {
            id: codec_id(),
            engine,
            channels,
            pre_skip,
            gain_q7_8,
            queue: VecDeque::new(),
            eof: false,
        })
    }

    /// Remix one decoded packet's interleaved PCM from its coded
    /// channel count onto the adapter's fixed output channel count:
    /// pass-through when equal, duplicate mono into both stereo
    /// channels, average a stereo pair down to mono.
    fn remix(&self, pcm: Vec<i16>, coded_channels: u8) -> Vec<i16> {
        let from = coded_channels.max(1) as usize;
        let to = self.channels.max(1) as usize;
        if from == to {
            return pcm;
        }
        let samples = pcm.len() / from;
        let mut out = Vec::with_capacity(samples * to);
        match (from, to) {
            (1, 2) => {
                for &s in &pcm {
                    out.push(s);
                    out.push(s);
                }
            }
            (2, 1) => {
                for pair in pcm.chunks_exact(2) {
                    out.push(((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16);
                }
            }
            _ => {
                // General fallback (multistream head vs adapter target
                // can never disagree, so this is unreachable in
                // practice): copy the overlapping channels.
                for s in 0..samples {
                    for c in 0..to {
                        out.push(if c < from { pcm[s * from + c] } else { 0 });
                    }
                }
            }
        }
        out
    }

    /// Post-process one decoded packet (remix → §5.1 output gain →
    /// pre-skip trim) and queue the surviving samples as one frame.
    fn queue_decoded(&mut self, pcm: Vec<i16>, coded_channels: u8, pts: Option<i64>) {
        let mut pcm = self.remix(pcm, coded_channels);
        if self.gain_q7_8 != 0 {
            apply_output_gain(&mut pcm, self.gain_q7_8);
        }
        let ch = self.channels.max(1) as usize;
        let samples = pcm.len() / ch;
        let dropped = self.pre_skip.consume(samples);
        if dropped > 0 {
            pcm.drain(..dropped * ch);
        }
        let kept = samples - dropped;
        if kept == 0 {
            return;
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in &pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.queue.push_back(AudioFrame {
            samples: kept as u32,
            // The pre-skip region is start-of-stream leading audio the
            // container timeline does not count; the surviving samples
            // keep the packet's timestamp.
            pts,
            data: vec![bytes],
        });
    }
}

impl oxideav_core::Decoder for OpusStreamDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        if packet.data.is_empty() {
            // A missing payload is a lost packet: run the §4.4
            // concealment instead of erroring (RFC 7845 §4.1 semantics
            // of "no audio to decode here").
            if let DecodeEngine::Single(dec) = &mut self.engine {
                let concealed = dec.conceal_loss();
                let ch = concealed.channels;
                self.queue_decoded(concealed.pcm, ch, packet.pts);
            }
            return Ok(());
        }
        match &mut self.engine {
            DecodeEngine::Single(dec) => {
                let DecodedAudio { pcm, channels, .. } =
                    dec.decode_packet(&packet.data).map_err(map_err)?;
                self.queue_decoded(pcm, channels, packet.pts);
            }
            DecodeEngine::Multi(dec) => {
                let audio = dec.decode_packet(&packet.data).map_err(map_err)?;
                let ch = audio.channels;
                self.queue_decoded(audio.pcm, ch, packet.pts);
            }
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        match self.queue.pop_front() {
            Some(frame) => Ok(Frame::Audio(frame)),
            None if self.eof => Err(oxideav_core::Error::Eof),
            None => Err(oxideav_core::Error::NeedMore),
        }
    }

    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> oxideav_core::Result<()> {
        match &mut self.engine {
            DecodeEngine::Single(dec) => dec.reset(),
            DecodeEngine::Multi(dec) => dec.reset(),
        }
        self.queue.clear();
        self.eof = false;
        // The RFC 7845 §5.1 pre-skip applies once at stream start; a
        // seek resumes mid-stream, so it is NOT re-armed here (the
        // player discards its §4.6 pre-roll instead).
        Ok(())
    }
}

/// Registry factory: build an [`OpusStreamDecoder`] honouring the
/// stream's [`CodecParameters`] (extradata `OpusHead`, channel count,
/// sample rate / format). This is both the direct-call construction
/// path and the function [`crate::register`] installs as the codec's
/// [`oxideav_core::DecoderFactory`].
pub fn make_decoder(
    params: &CodecParameters,
) -> oxideav_core::Result<Box<dyn oxideav_core::Decoder>> {
    Ok(Box::new(OpusStreamDecoder::from_params(params)?))
}

// ─────────────────────────── encoder ───────────────────────────

/// Typed encoder options (RFC 6716 encoder-side knobs), parsed from
/// [`CodecParameters::options`] and declared to the registry via
/// [`oxideav_core::registry::CodecInfo::encoder_options`].
#[derive(Debug, Clone)]
pub struct OpusEncoderOptions {
    /// §2.1 application profile steering the automatic mode /
    /// bandwidth decision: `"voip"`, `"audio"` (default), or
    /// `"lowdelay"` (CELT-only).
    pub application: String,
    /// §3.1 operating mode: `"auto"` (default — §2.1.1
    /// bitrate-driven, signal-adaptive unless `signal-adaptive=false`,
    /// with §4.5 transitions), `"silk"`, `"hybrid"`, or `"celt"`.
    pub mode: String,
    /// §2.1.3 audio bandwidth: `"auto"` (default — bitrate-driven),
    /// `"nb"`, `"mb"`, `"wb"`, `"swb"`, or `"fb"`.
    pub bandwidth: String,
    /// Packet duration in milliseconds: 2.5, 5, 10, 20 (default), 40,
    /// or 60 (40 / 60 ms are single SILK frames on the SILK-only mode
    /// and §3.2 code-3 packets of 20 ms frames on CELT / Hybrid).
    pub frame_ms: f32,
    /// §2.1.8 hard CBR (§3.2.5 code-3 padding to the exact
    /// per-packet byte target) instead of VBR.
    pub cbr: bool,
    /// §2.1.8 constrained VBR (bit-reservoir discipline) instead of
    /// unconstrained drift correction.
    pub constrained_vbr: bool,
    /// §2.1.9 discontinuous transmission.
    pub dtx: bool,
    /// §2.1.7 in-band FEC (§4.2.5 LBRR) on the SILK-bearing modes.
    pub fec: bool,
    /// §2.1.7 expected packet-loss percentage (0..=100; shapes LBRR).
    pub packet_loss: u32,
    /// §4.5.1 transition side information (redundant CELT frames at
    /// configuration switches; default on).
    pub redundancy: bool,
    /// §5.3.1 post-filter tapset election (CELT-only arm).
    pub tapset_election: bool,
    /// Complexity rung 0..=10 (`None` keeps the crate default, which
    /// is bit-identical to rung 4).
    pub complexity: Option<u32>,
    /// §5 signal-adaptive election under `mode=auto`: the encoder's
    /// own speech/music analyser and content-bandwidth estimate steer
    /// the mode / bandwidth decision (default on — measured to win on
    /// music, tones and mixed content at equal rate and to leave
    /// speech streams identical; see `tests/signal_adaptive_election.rs`).
    pub signal_adaptive: bool,
}

impl Default for OpusEncoderOptions {
    fn default() -> Self {
        Self {
            application: "audio".into(),
            mode: "auto".into(),
            bandwidth: "auto".into(),
            frame_ms: 20.0,
            cbr: false,
            constrained_vbr: false,
            dtx: false,
            fec: false,
            packet_loss: 0,
            redundancy: true,
            tapset_election: false,
            complexity: None,
            signal_adaptive: true,
        }
    }
}

impl oxideav_core::CodecOptionsStruct for OpusEncoderOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "application",
            kind: OptionKind::Enum(&["voip", "audio", "lowdelay"]),
            default: OptionValue::String(String::new()),
            help: "application profile: voip, audio (default), or lowdelay",
        },
        OptionField {
            name: "mode",
            kind: OptionKind::Enum(&["auto", "silk", "hybrid", "celt"]),
            default: OptionValue::String(String::new()),
            help: "operating mode: auto (default, bitrate-driven), silk, hybrid, or celt",
        },
        OptionField {
            name: "bandwidth",
            kind: OptionKind::Enum(&["auto", "nb", "mb", "wb", "swb", "fb"]),
            default: OptionValue::String(String::new()),
            help: "audio bandwidth: auto (default), nb, mb, wb, swb, or fb",
        },
        OptionField {
            name: "frame-ms",
            kind: OptionKind::F32,
            default: OptionValue::F32(20.0),
            help: "packet duration in ms: 2.5, 5, 10, 20, 40, or 60 (40/60 pack 20 ms CELT/Hybrid frames)",
        },
        OptionField {
            name: "cbr",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "hard CBR (code-3 padding to the exact packet size) instead of VBR",
        },
        OptionField {
            name: "constrained-vbr",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "constrained VBR (bit-reservoir bound) instead of unconstrained",
        },
        OptionField {
            name: "dtx",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "discontinuous transmission (1-byte markers over silence)",
        },
        OptionField {
            name: "fec",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "in-band FEC (LBRR redundancy) on the SILK-bearing modes",
        },
        OptionField {
            name: "packet-loss",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "expected packet-loss percentage 0..=100 (shapes the FEC)",
        },
        OptionField {
            name: "redundancy",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(true),
            help: "RFC 6716 §4.5.1 redundant CELT frames at configuration switches",
        },
        OptionField {
            name: "tapset-election",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "elect the post-filter tapset per frame by measured SNR",
        },
        OptionField {
            name: "complexity",
            kind: OptionKind::U32,
            default: OptionValue::U32(4),
            help: "complexity rung 0..=10 (default rung is bit-identical to 4)",
        },
        OptionField {
            name: "signal-adaptive",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(true),
            help: "under mode=auto, let the speech/music analyser steer the mode and bandwidth (default on)",
        },
    ];

    fn apply(&mut self, key: &str, value: &OptionValue) -> oxideav_core::Result<()> {
        match key {
            "application" => self.application = value.as_str()?.to_ascii_lowercase(),
            "mode" => self.mode = value.as_str()?.to_ascii_lowercase(),
            "bandwidth" => self.bandwidth = value.as_str()?.to_ascii_lowercase(),
            "frame-ms" => self.frame_ms = value.as_f32()?,
            "cbr" => self.cbr = value.as_bool()?,
            "constrained-vbr" => self.constrained_vbr = value.as_bool()?,
            "dtx" => self.dtx = value.as_bool()?,
            "fec" => self.fec = value.as_bool()?,
            "packet-loss" => self.packet_loss = value.as_u32()?,
            "redundancy" => self.redundancy = value.as_bool()?,
            "tapset-election" => self.tapset_election = value.as_bool()?,
            "complexity" => self.complexity = Some(value.as_u32()?),
            "signal-adaptive" => self.signal_adaptive = value.as_bool()?,
            _ => unreachable!("guarded by SCHEMA"),
        }
        Ok(())
    }
}

/// Decoder-side start-up samples the encoder's processing chain
/// introduces (the CELT MDCT overlap latency), declared as the RFC
/// 7845 §5.1 pre-skip in the composed `OpusHead`.
const ENCODE_PRE_SKIP: u16 = 120;

/// [`oxideav_core::Encoder`] adapter over the unified
/// [`crate::opus_encoder::OpusEncoder`]: 48 kHz interleaved S16
/// frames in, one Opus packet per frame out, with the §2.1.1
/// bitrate-driven mode/bandwidth ladder and the §4.5 transition
/// machinery behind the `mode`/`bandwidth`/`application` options.
/// Build via [`make_encoder`].
#[derive(Debug)]
pub struct OpusStreamEncoder {
    id: CodecId,
    enc: crate::opus_encoder::OpusEncoder,
    out_params: CodecParameters,
    channels: usize,
    /// Samples per channel in one Opus frame (options-selected
    /// duration at 48 kHz).
    frame_samples: usize,
    /// Interleaved samples awaiting a full 20 ms frame.
    pending: Vec<i16>,
    /// Encoded packets not yet pulled by `receive_packet`.
    queue: VecDeque<Packet>,
    /// Running output timestamp in 1/48000 units.
    next_pts: i64,
    flushed: bool,
}

impl OpusStreamEncoder {
    fn from_params(params: &CodecParameters) -> oxideav_core::Result<Self> {
        if let Some(rate) = params.sample_rate {
            if rate != OUTPUT_SAMPLE_RATE_HZ {
                return Err(oxideav_core::Error::unsupported(format!(
                    "opus encode: unsupported input sample rate {rate} Hz (48000 required)"
                )));
            }
        }
        match params.sample_format {
            None | Some(SampleFormat::S16) => {}
            Some(f) => {
                return Err(oxideav_core::Error::unsupported(format!(
                    "opus encode: unsupported sample format {f:?} (S16 interleaved only)"
                )));
            }
        }
        let channels = params.channels.unwrap_or(2);
        if channels == 0 || channels > 2 {
            return Err(oxideav_core::Error::unsupported(format!(
                "opus encode: {channels} channels unsupported (mono or stereo)"
            )));
        }
        let stereo = channels == 2;
        let opts: OpusEncoderOptions = oxideav_core::parse_options(&params.options)?;
        let application = match opts.application.as_str() {
            "voip" => crate::opus_encoder::Application::Voip,
            "audio" => crate::opus_encoder::Application::Audio,
            "lowdelay" => crate::opus_encoder::Application::RestrictedLowDelay,
            other => {
                return Err(oxideav_core::Error::invalid(format!(
                    "opus encode: application '{other}' (voip / audio / lowdelay)"
                )))
            }
        };
        let forced_mode = match opts.mode.as_str() {
            "auto" => None,
            "silk" => Some(crate::toc::Mode::SilkOnly),
            "hybrid" => Some(crate::toc::Mode::Hybrid),
            "celt" => Some(crate::toc::Mode::CeltOnly),
            other => {
                return Err(oxideav_core::Error::invalid(format!(
                    "opus encode: mode '{other}' (auto / silk / hybrid / celt)"
                )))
            }
        };
        let forced_bandwidth = match opts.bandwidth.as_str() {
            "auto" => None,
            "nb" => Some(crate::toc::Bandwidth::Nb),
            "mb" => Some(crate::toc::Bandwidth::Mb),
            "wb" => Some(crate::toc::Bandwidth::Wb),
            "swb" => Some(crate::toc::Bandwidth::Swb),
            "fb" => Some(crate::toc::Bandwidth::Fb),
            other => {
                return Err(oxideav_core::Error::invalid(format!(
                    "opus encode: bandwidth '{other}' (auto / nb / mb / wb / swb / fb)"
                )))
            }
        };
        let frame_tenths = (opts.frame_ms * 10.0).round() as u16;
        if !matches!(frame_tenths, 25 | 50 | 100 | 200 | 400 | 600) {
            return Err(oxideav_core::Error::invalid(format!(
                "opus encode: frame-ms {} (2.5 / 5 / 10 / 20 / 40 / 60)",
                opts.frame_ms
            )));
        }
        // Default rate: 64 kb/s per channel, a comfortable fullband
        // music operating point for the §2.1.8 VBR controller.
        let bit_rate = params
            .bit_rate
            .unwrap_or(64_000 * channels as u64)
            .clamp(6_000, 510_000) as u32;
        let mut enc =
            crate::opus_encoder::OpusEncoder::new(channels as usize, application, bit_rate)
                .map_err(map_err)?;
        enc.set_frame_tenths_ms(frame_tenths).map_err(|_| {
            oxideav_core::Error::invalid(format!(
                "opus encode: frame-ms {} unsupported at this configuration",
                opts.frame_ms
            ))
        })?;
        enc.set_mode(forced_mode).map_err(|_| {
            oxideav_core::Error::invalid(format!(
                "opus encode: mode '{}' incompatible with frame-ms {}",
                opts.mode, opts.frame_ms
            ))
        })?;
        enc.set_bandwidth(forced_bandwidth);
        enc.set_vbr(!opts.cbr);
        enc.set_constrained_vbr(opts.constrained_vbr)
            .map_err(map_err)?;
        enc.set_dtx(opts.dtx);
        enc.set_fec(opts.fec);
        enc.set_packet_loss_perc(opts.packet_loss.min(100) as u8);
        enc.set_transition_redundancy(opts.redundancy);
        enc.set_tapset_election(opts.tapset_election);
        if let Some(c) = opts.complexity {
            enc.set_complexity(c.min(10) as u8);
        }
        enc.set_signal_adaptive(opts.signal_adaptive);
        let frame_samples = enc.frame_samples();

        let head = OpusHead {
            version: 1,
            channel_count: channels as u8,
            pre_skip: ENCODE_PRE_SKIP,
            input_sample_rate: OUTPUT_SAMPLE_RATE_HZ,
            output_gain_q7_8: 0,
            mapping_family: 0,
            mapping: crate::opus_head::ChannelMappingTable {
                stream_count: 1,
                coupled_count: if stereo { 1 } else { 0 },
                mapping: if stereo { vec![0, 1] } else { vec![0] },
            },
        };
        // Family 0 synthesizes the mapping on parse; compose validates.
        let extradata = head
            .compose()
            .map_err(|e| oxideav_core::Error::invalid(format!("opus encode: {e}")))?;

        let mut out_params = CodecParameters::audio(codec_id());
        out_params.sample_rate = Some(OUTPUT_SAMPLE_RATE_HZ);
        out_params.channels = Some(channels);
        out_params.sample_format = Some(SampleFormat::S16);
        out_params.bit_rate = Some(u64::from(bit_rate));
        out_params.extradata = extradata;

        Ok(Self {
            id: codec_id(),
            enc,
            out_params,
            channels: channels as usize,
            frame_samples,
            pending: Vec::new(),
            queue: VecDeque::new(),
            next_pts: 0,
            flushed: false,
        })
    }

    fn encode_ready_frames(&mut self) -> oxideav_core::Result<()> {
        let frame_len = self.frame_samples * self.channels;
        while self.pending.len() >= frame_len {
            let frame: Vec<i16> = self.pending.drain(..frame_len).collect();
            let bytes = self.enc.encode_frame(&frame).map_err(map_err)?;
            let mut packet = Packet::new(
                0,
                TimeBase(Rational::new(1, OUTPUT_SAMPLE_RATE_HZ as i64)),
                bytes,
            );
            packet.pts = Some(self.next_pts);
            packet.dts = Some(self.next_pts);
            packet.duration = Some(self.frame_samples as i64);
            self.next_pts += self.frame_samples as i64;
            self.queue.push_back(packet);
        }
        Ok(())
    }
}

impl oxideav_core::Encoder for OpusStreamEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> oxideav_core::Result<()> {
        if self.flushed {
            return Err(oxideav_core::Error::invalid(
                "opus encode: send_frame after flush",
            ));
        }
        let audio = match frame {
            Frame::Audio(a) => a,
            _ => {
                return Err(oxideav_core::Error::invalid(
                    "opus encode: expected an audio frame",
                ))
            }
        };
        let plane = audio
            .data
            .first()
            .ok_or_else(|| oxideav_core::Error::invalid("opus encode: empty audio frame"))?;
        if audio.data.len() != 1 || plane.len() % 2 != 0 {
            return Err(oxideav_core::Error::invalid(
                "opus encode: expected one interleaved S16 plane",
            ));
        }
        let expected = audio.samples as usize * self.channels * 2;
        if plane.len() != expected {
            return Err(oxideav_core::Error::invalid(format!(
                "opus encode: plane holds {} bytes, expected {expected} \
                 ({} samples × {} channels × 2)",
                plane.len(),
                audio.samples,
                self.channels
            )));
        }
        self.pending.extend(
            plane
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        );
        self.encode_ready_frames()
    }

    fn receive_packet(&mut self) -> oxideav_core::Result<Packet> {
        match self.queue.pop_front() {
            Some(p) => Ok(p),
            None if self.flushed => Err(oxideav_core::Error::Eof),
            None => Err(oxideav_core::Error::NeedMore),
        }
    }

    fn flush(&mut self) -> oxideav_core::Result<()> {
        if !self.flushed {
            self.flushed = true;
            if !self.pending.is_empty() {
                // Zero-pad the final partial frame to a whole 20 ms
                // packet; the container's end-trimming (RFC 7845 §4.4
                // granule arithmetic) drops the padding on playback.
                let frame_len = self.frame_samples * self.channels;
                self.pending.resize(frame_len, 0);
                self.encode_ready_frames()?;
            }
        }
        Ok(())
    }
}

/// Registry factory: build an [`OpusStreamEncoder`] honouring the
/// stream's [`CodecParameters`] (channel count, bit rate, sample rate /
/// format). This is both the direct-call construction path and the
/// function [`crate::register`] installs as the codec's
/// [`oxideav_core::EncoderFactory`].
pub fn make_encoder(
    params: &CodecParameters,
) -> oxideav_core::Result<Box<dyn oxideav_core::Encoder>> {
    Ok(Box::new(OpusStreamEncoder::from_params(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{Decoder as _, Encoder as _};

    fn audio_params(channels: u16) -> CodecParameters {
        let mut p = CodecParameters::audio(codec_id());
        p.channels = Some(channels);
        p
    }

    #[test]
    fn decoder_rejects_wrong_rate_and_format() {
        let mut p = audio_params(1);
        p.sample_rate = Some(44_100);
        assert!(make_decoder(&p).is_err());
        let mut p = audio_params(1);
        p.sample_format = Some(SampleFormat::F32);
        assert!(make_decoder(&p).is_err());
    }

    #[test]
    fn decoder_requires_mapping_for_many_channels() {
        assert!(make_decoder(&audio_params(6)).is_err());
    }

    #[test]
    fn extradata_channel_conflict_is_rejected() {
        // Minimal family-0 stereo OpusHead vs params claiming mono.
        let mut head = Vec::new();
        head.extend_from_slice(b"OpusHead");
        head.extend_from_slice(&[1, 2]);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, 0]);
        let mut p = audio_params(1);
        p.extradata = head;
        assert!(make_decoder(&p).is_err());
    }

    #[test]
    fn encoder_decoder_roundtrip_tone() {
        // 100 ms of a 440 Hz tone through the registry-facing encoder
        // and back through the registry-facing decoder.
        let params = audio_params(1);
        let mut enc = OpusStreamEncoder::from_params(&params).expect("encoder");
        let samples = 4_800usize;
        let pcm: Vec<i16> = (0..samples)
            .map(|i| (8_000.0 * (std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin()) as i16)
            .collect();
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in &pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: samples as u32,
            pts: Some(0),
            data: vec![bytes],
        }))
        .expect("send");
        enc.flush().expect("flush");

        let mut dec_params = audio_params(1);
        dec_params.extradata = enc.output_params().extradata.clone();
        let mut dec = OpusStreamDecoder::from_params(&dec_params).expect("decoder");
        let mut decoded = 0usize;
        let mut energy = 0f64;
        loop {
            let packet = match enc.receive_packet() {
                Ok(p) => p,
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("receive_packet: {e}"),
            };
            assert_eq!(packet.duration, Some(960));
            dec.send_packet(&packet).expect("decode");
            while let Ok(Frame::Audio(f)) = dec.receive_frame() {
                decoded += f.samples as usize;
                for b in f.data[0].chunks_exact(2) {
                    let v = f64::from(i16::from_le_bytes([b[0], b[1]]));
                    energy += v * v;
                }
            }
        }
        // 5 packets × 960 samples, minus the 120-sample pre-skip the
        // composed OpusHead declares.
        assert_eq!(decoded, 5 * 960 - usize::from(ENCODE_PRE_SKIP));
        assert!(energy > 0.0, "decoded audio must carry signal");
    }
}
