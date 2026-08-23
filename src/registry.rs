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
//!   the CELT-only fullband VBR arm ([`crate::vbr::CeltVbrEncoder`])
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
    AudioFrame, CodecId, CodecParameters, Frame, Packet, Rational, SampleFormat, TimeBase,
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

/// Samples per channel in one 20 ms Opus frame at 48 kHz.
const ENCODE_FRAME_SAMPLES: usize = 960;

/// Decoder-side start-up samples the encoder's processing chain
/// introduces (the CELT MDCT overlap latency), declared as the RFC
/// 7845 §5.1 pre-skip in the composed `OpusHead`.
const ENCODE_PRE_SKIP: u16 = 120;

/// [`oxideav_core::Encoder`] adapter: 48 kHz interleaved S16 frames in,
/// one Opus packet per 20 ms frame out (CELT-only fullband VBR at the
/// requested bit rate). Build via [`make_encoder`].
#[derive(Debug)]
pub struct OpusStreamEncoder {
    id: CodecId,
    enc: crate::vbr::CeltVbrEncoder,
    out_params: CodecParameters,
    channels: usize,
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
        // Default rate: 64 kb/s per channel, a comfortable fullband
        // music operating point for the §2.1.8 VBR controller.
        let bit_rate = params
            .bit_rate
            .unwrap_or(64_000 * channels as u64)
            .clamp(6_000, 512_000) as u32;
        let enc = crate::vbr::CeltVbrEncoder::new(
            crate::toc::Bandwidth::Fb,
            200, // 20 ms frames
            stereo,
            bit_rate,
            false,
        )
        .map_err(map_err)?;

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
            pending: Vec::new(),
            queue: VecDeque::new(),
            next_pts: 0,
            flushed: false,
        })
    }

    fn encode_ready_frames(&mut self) -> oxideav_core::Result<()> {
        let frame_len = ENCODE_FRAME_SAMPLES * self.channels;
        while self.pending.len() >= frame_len {
            let frame: Vec<i16> = self.pending.drain(..frame_len).collect();
            let (bytes, _info) = self.enc.encode_frame(&frame).map_err(map_err)?;
            let mut packet = Packet::new(
                0,
                TimeBase(Rational::new(1, OUTPUT_SAMPLE_RATE_HZ as i64)),
                bytes,
            );
            packet.pts = Some(self.next_pts);
            packet.dts = Some(self.next_pts);
            packet.duration = Some(ENCODE_FRAME_SAMPLES as i64);
            self.next_pts += ENCODE_FRAME_SAMPLES as i64;
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
                let frame_len = ENCODE_FRAME_SAMPLES * self.channels;
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
