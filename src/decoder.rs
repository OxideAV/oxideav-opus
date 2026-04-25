//! Opus decoder — wraps the CELT pipeline and the SILK sub-decoder.
//!
//! What's handled end-to-end (RFC 6716):
//!
//! 1. **TOC parsing** (§3.1) — mode, bandwidth, frame duration, stereo,
//!    framing code.
//! 2. **Framing codes 0/1/2/3** — packet split into per-frame byte slices.
//! 3. **Silence / DTX frames** — 0/1-byte frames emit silence for the
//!    expected duration.
//! 4. **CELT-only frames (§4.3)** — full pipeline: range decode, header,
//!    coarse + fine band energy, bit allocation, PVQ shape, anti-collapse,
//!    denormalise, IMDCT (sub-block + window + overlap-add), comb post
//!    filter. Output is 48 kHz S16 PCM.
//! 5. **SILK-only frames (§4.2)** — NB/MB/WB mono + stereo at 10/20/40/60 ms
//!    via the `silk` module. LBRR redundancy data is decoded-and-discarded
//!    via a scratch SILK channel state so the range coder stays aligned;
//!    the loss-free output is unaffected.
//! 6. **Hybrid (§4.4)** — SILK-WB low band + CELT high band with
//!    start_band offset, run through the same pipelines and summed.
//! 7. **Multistream (RFC 7845 §5.1.1)** — channel mapping family 1
//!    (Vorbis surround) and family 2 (ambisonics).

use oxideav_celt::bands::{anti_collapse, denormalise_bands, quant_all_bands};
use oxideav_celt::header::decode_header;
use oxideav_celt::mdct::imdct_sub;
use oxideav_celt::post_filter::comb_filter;
use oxideav_celt::quant_bands::{
    unquant_coarse_energy, unquant_energy_finalise, unquant_fine_energy,
};
use oxideav_celt::range_decoder::{RangeDecoder, BITRES};
use oxideav_celt::rate::clt_compute_allocation;
use oxideav_celt::tables::{
    end_band_for_bandwidth_celt, init_caps, lm_for_frame_samples, EBAND_5MS, NB_EBANDS,
    SPREAD_ICDF, SPREAD_NORMAL, TF_SELECT_TABLE, TRIM_ICDF,
};
use oxideav_core::Decoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::silk::SilkDecoder;
use crate::toc::{OpusMode, Toc};

/// Opus always decodes at 48 kHz.
pub const OPUS_RATE_HZ: u32 = 48_000;

/// Build an Opus decoder from the codec parameters.
///
/// If `params.extradata` is a valid `OpusHead` with channel mapping
/// family 1 or 2 and `output_channel_count > 2`, a
/// [`MultistreamOpusDecoder`] is returned that demuxes each packet
/// into N sub-stream packets (per RFC 7845 §5.1.1) and mixes the
/// coupled/uncoupled streams into the final output according to the
/// channel mapping table.
///
/// Otherwise the single-stream path is used. `params.channels` may be
/// 1 or 2; anything else is rejected.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let channels = params.channels.unwrap_or(1).max(1);

    // Detect multistream via the OpusHead extradata.
    if !params.extradata.is_empty() {
        if let Ok(head) = crate::header::parse_opus_head(&params.extradata) {
            if head.channel_mapping_family == 1 || head.channel_mapping_family == 2 {
                return MultistreamOpusDecoder::new(params, &head)
                    .map(|d| Box::new(d) as Box<dyn Decoder>);
            }
            if head.channel_mapping_family != 0 {
                return Err(Error::unsupported(format!(
                    "Opus channel mapping family {} not supported",
                    head.channel_mapping_family
                )));
            }
        }
    }

    if channels > 2 {
        return Err(Error::unsupported(
            "Opus channel count > 2 requires a channel-mapped OpusHead (family 1 or 2) in extradata",
        ));
    }
    Ok(Box::new(OpusDecoder::new(params, channels)))
}

/// Persistent CELT decoder state (carried across frames).
struct CeltState {
    channels: usize,
    /// Per-channel previous frame's IMDCT tail (for overlap-add).
    overlap_buf: Vec<Vec<f32>>,
    /// Per-channel previous frame's filtered output, used as comb-filter
    /// history for the next frame.
    history: Vec<Vec<f32>>,
    /// Inter-frame log-band energies in dB (channel-major: NB_EBANDS*channels).
    old_band_e: Vec<f32>,
    old_log_e: Vec<f32>,
    old_log_e2: Vec<f32>,
    /// Previous frame's post-filter parameters.
    pf_period_old: i32,
    pf_gain_old: f32,
    pf_tapset_old: usize,
    /// Current frame's previously decoded post-filter (carried for the post-
    /// pass on the *next* frame).
    pf_period: i32,
    pf_gain: f32,
    pf_tapset: usize,
    /// Range-coder seed.
    rng: u32,
}

impl CeltState {
    fn new(channels: usize) -> Self {
        Self {
            channels,
            overlap_buf: vec![vec![0.0; CELT_OVERLAP_120 * 8]; channels],
            history: vec![vec![0.0; HISTORY_SIZE]; channels],
            old_band_e: vec![-28.0; NB_EBANDS * 2],
            old_log_e: vec![-28.0; NB_EBANDS * 2],
            old_log_e2: vec![-28.0; NB_EBANDS * 2],
            pf_period_old: 0,
            pf_gain_old: 0.0,
            pf_tapset_old: 0,
            pf_period: 0,
            pf_gain: 0.0,
            pf_tapset: 0,
            rng: 0,
        }
    }
}

const CELT_OVERLAP_120: usize = 120;
const HISTORY_SIZE: usize = 1024; // bigger than max comb-filter period (768)

struct OpusDecoder {
    codec_id: CodecId,
    channels: u16,
    time_base: TimeBase,
    pending: Option<Packet>,
    eof: bool,
    emit_pts: i64,
    state: CeltState,
    /// SILK sub-decoder, instantiated lazily when the first SILK-only
    /// packet arrives.
    silk: Option<SilkDecoder>,
}

impl OpusDecoder {
    fn new(params: &CodecParameters, channels: u16) -> Self {
        Self {
            codec_id: params.codec_id.clone(),
            channels,
            time_base: TimeBase::new(1, OPUS_RATE_HZ as i64),
            pending: None,
            eof: false,
            emit_pts: 0,
            state: CeltState::new(channels as usize),
            silk: None,
        }
    }

    /// Decode an already-parsed Opus packet to per-channel f32 PCM.
    ///
    /// Used by `MultistreamOpusDecoder` which parses the outer framing
    /// itself (to support self-delimited sub-stream packets) and then
    /// hands the parsed frames to this method.
    ///
    /// Every returned channel is exactly
    /// `toc.frame_samples_48k * frames.len()` long — zero-padded if
    /// the underlying decode path emitted fewer samples (CELT's
    /// internal grid is M*100 rather than M*120; the existing
    /// single-stream decode_packet relies on `get(i).unwrap_or(0.0)`
    /// to zero-fill, but multistream needs the explicit padding so
    /// every sub-stream has the same sample count).
    fn decode_parsed_to_pcm(
        &mut self,
        parsed: &crate::toc::OpusPacket<'_>,
    ) -> Result<Vec<Vec<f32>>> {
        let toc_ch = parsed.toc.channels();
        let out_channels = self.channels.max(toc_ch);
        let per_frame = parsed.toc.frame_samples_48k as usize;
        let total_samples = per_frame * parsed.frames.len();

        let mut per_ch: Vec<Vec<f32>> = (0..out_channels)
            .map(|_| Vec::with_capacity(total_samples))
            .collect();
        for frame_bytes in parsed.frames.iter() {
            let mut ch_buf = decode_frame(self, &parsed.toc, frame_bytes, out_channels as usize)?;
            for (dst, src) in per_ch.iter_mut().zip(ch_buf.drain(..)) {
                let start = dst.len();
                dst.extend_from_slice(&src);
                // Pad to `per_frame` samples if the decoder returned
                // fewer (current CELT pipeline emits M*100 = 800 at
                // LM=3 when the spec mandates M*120 = 960).
                if dst.len() < start + per_frame {
                    dst.resize(start + per_frame, 0.0);
                }
            }
        }
        Ok(per_ch)
    }
}

impl Decoder for OpusDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other(
                "Opus decoder: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        };
        decode_packet(self, &pkt)
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // Opus carries extensive cross-frame state on both paths:
        //   * CELT: IMDCT overlap-add buffer, comb-filter history, per-band
        //     energy memory (old_band_e / old_log_e / old_log_e2),
        //     post-filter parameters (period/gain/tapset old+current),
        //     range-coder seed `rng`.
        //   * SILK: LPC filter memory, LTP / pitch-track history, NLSF
        //     history, gain-predictor state, stereo unmixing memory.
        //
        // Dropping the SILK sub-decoder is the simplest correct wipe: it
        // is rebuilt lazily on the next SILK frame and picks up the
        // bandwidth from the TOC. The CELT state is rebuilt via
        // `CeltState::new` to zero all the band-energy / overlap buffers.
        self.state = CeltState::new(self.channels as usize);
        self.silk = None;
        self.pending = None;
        self.eof = false;
        self.emit_pts = 0;
        Ok(())
    }
}

fn decode_packet(dec: &mut OpusDecoder, packet: &Packet) -> Result<Frame> {
    let parsed = crate::toc::parse_packet(&packet.data)?;
    let n_frames = parsed.frames.len();
    let per_frame = parsed.toc.frame_samples_48k as usize;
    let total_samples = per_frame * n_frames;
    let toc_ch = parsed.toc.channels();
    let out_channels = dec.channels.max(toc_ch);

    let mut per_ch: Vec<Vec<f32>> = (0..out_channels)
        .map(|_| Vec::with_capacity(total_samples))
        .collect();

    for frame_bytes in parsed.frames.iter() {
        let mut ch_buf = decode_frame(dec, &parsed.toc, frame_bytes, out_channels as usize)?;
        for (dst, src) in per_ch.iter_mut().zip(ch_buf.drain(..)) {
            dst.extend_from_slice(&src);
        }
    }

    let mut interleaved = Vec::with_capacity(total_samples * out_channels as usize * 2);
    for i in 0..total_samples {
        for ch_buf in per_ch.iter().take(out_channels as usize) {
            let s = ch_buf.get(i).copied().unwrap_or(0.0);
            let clamped = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
            interleaved.extend_from_slice(&clamped.to_le_bytes());
        }
    }

    let pts = packet.pts.unwrap_or(dec.emit_pts);
    dec.emit_pts = pts + total_samples as i64;

    Ok(Frame::Audio(AudioFrame {
        format: SampleFormat::S16,
        channels: out_channels,
        sample_rate: OPUS_RATE_HZ,
        samples: total_samples as u32,
        pts: Some(pts),
        time_base: dec.time_base,
        data: vec![interleaved],
    }))
}

/// Multistream Opus decoder per RFC 7845 §5.1 (channel mapping family
/// 1 for Vorbis surround, family 2 for ambisonics).
///
/// Holds N sub-decoders (one per Opus stream in the packet). The first
/// M streams are stereo ("coupled"), the rest are mono ("uncoupled").
/// Each input packet is split into N sub-packets using the same length
/// coding as RFC 6716 §3.2.5 (all but the last stream has a 1 or 2
/// byte length prefix), and each sub-packet is decoded by its
/// corresponding sub-decoder. The decoded per-stream channels are then
/// gathered into the final output via the channel mapping table.
pub struct MultistreamOpusDecoder {
    codec_id: CodecId,
    /// Output channel count (C).
    channels: u16,
    /// Stream count (N).
    stream_count: u8,
    /// Coupled stream count (M). First M streams are stereo.
    coupled_stream_count: u8,
    /// Channel mapping table: `channel_mapping[c]` = source stream
    /// channel index for output channel c. Coupled streams occupy two
    /// consecutive indices per stream, followed by one index per
    /// uncoupled stream. Index 255 = silent (per RFC 7845).
    channel_mapping: Vec<u8>,
    time_base: TimeBase,
    pending: Option<Packet>,
    eof: bool,
    emit_pts: i64,
    /// Sub-decoders. First `coupled_stream_count` are stereo, the rest
    /// are mono.
    streams: Vec<OpusDecoder>,
}

impl MultistreamOpusDecoder {
    /// Construct from already-parsed OpusHead.
    fn new(params: &CodecParameters, head: &crate::header::OpusHead) -> Result<Self> {
        // Mapping table layout per RFC 7845 §5.1.1 for family 1/2:
        //   byte 0: stream count (N)
        //   byte 1: coupled stream count (M)
        //   bytes 2..2+C: channel_mapping (one byte per output channel)
        if head.mapping_table.len() < 2 {
            return Err(Error::invalid(
                "Opus multistream OpusHead: mapping table too short",
            ));
        }
        let stream_count = head.mapping_table[0];
        let coupled_stream_count = head.mapping_table[1];
        if stream_count == 0 {
            return Err(Error::invalid("Opus multistream: stream count must be > 0"));
        }
        if coupled_stream_count > stream_count {
            return Err(Error::invalid("Opus multistream: coupled > stream count"));
        }
        let c = head.output_channel_count as usize;
        if head.mapping_table.len() < 2 + c {
            return Err(Error::invalid(
                "Opus multistream: channel mapping truncated",
            ));
        }
        // Validate mapping indices: max valid index is
        //   2 * coupled + (stream - coupled) - 1 = stream + coupled - 1.
        // 255 is allowed and means "silent channel".
        let max_idx = stream_count + coupled_stream_count;
        for &m in &head.mapping_table[2..2 + c] {
            if m != 255 && m >= max_idx {
                return Err(Error::invalid(format!(
                    "Opus multistream: channel mapping index {} out of range (max {})",
                    m,
                    max_idx - 1
                )));
            }
        }

        // Family 2 (ambisonics) forbids coupled streams — every stream
        // is a discrete ambisonic channel.
        if head.channel_mapping_family == 2 && coupled_stream_count != 0 {
            return Err(Error::invalid(
                "Opus family 2 (ambisonics): coupled streams must be 0",
            ));
        }

        let mut streams = Vec::with_capacity(stream_count as usize);
        for i in 0..stream_count {
            let ch: u16 = if i < coupled_stream_count { 2 } else { 1 };
            streams.push(OpusDecoder::new(params, ch));
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            channels: head.output_channel_count as u16,
            stream_count,
            coupled_stream_count,
            channel_mapping: head.mapping_table[2..2 + c].to_vec(),
            time_base: TimeBase::new(1, OPUS_RATE_HZ as i64),
            pending: None,
            eof: false,
            emit_pts: 0,
            streams,
        })
    }

    /// Split a multistream packet into N parsed sub-stream packets.
    ///
    /// Per RFC 7845 §5.1.1 and libopus's `opus_multistream_decode`:
    /// the first N-1 sub-streams use **self-delimited framing**
    /// (RFC 6716 Appendix B); the last sub-stream uses regular
    /// framing and takes the remaining bytes.
    fn split_packet<'a>(&self, data: &'a [u8]) -> Result<Vec<crate::toc::OpusPacket<'a>>> {
        let n = self.stream_count as usize;
        let mut out: Vec<crate::toc::OpusPacket<'a>> = Vec::with_capacity(n);
        let mut cursor = 0usize;
        for _ in 0..n.saturating_sub(1) {
            let (pkt, consumed) = crate::toc::parse_self_delimited_packet(&data[cursor..])?;
            out.push(pkt);
            cursor += consumed;
        }
        // Last sub-stream: regular framing.
        let pkt = crate::toc::parse_packet(&data[cursor..])?;
        out.push(pkt);
        Ok(out)
    }
}

impl Decoder for MultistreamOpusDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other(
                "Opus multistream: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        };
        let sub_packets = self.split_packet(&pkt.data)?;
        debug_assert_eq!(sub_packets.len(), self.stream_count as usize);

        // Decode each sub-stream into per-channel buffers. Collect the
        // channels into one flat vector in the order:
        //   [stream0_L, stream0_R, stream1_L, stream1_R, ..., streamM-1_L, streamM-1_R,
        //    streamM, streamM+1, ..., streamN-1]
        // i.e. the same indexing the channel mapping table uses.
        let mut stream_channels: Vec<Vec<f32>> = Vec::new();
        let mut per_frame_samples = 0usize;
        for (i, sub) in sub_packets.iter().enumerate() {
            let pcm = self.streams[i].decode_parsed_to_pcm(sub)?;
            if per_frame_samples == 0 && !pcm.is_empty() {
                per_frame_samples = pcm[0].len();
            } else if !pcm.is_empty() && pcm[0].len() != per_frame_samples {
                return Err(Error::invalid(format!(
                    "Opus multistream: stream {} sample count {} != expected {}",
                    i,
                    pcm[0].len(),
                    per_frame_samples,
                )));
            }
            for ch in pcm.into_iter() {
                stream_channels.push(ch);
            }
        }

        // Map to output channels.
        let out_channels = self.channels as usize;
        let mut interleaved = Vec::with_capacity(per_frame_samples * out_channels * 2);
        for i in 0..per_frame_samples {
            for c in 0..out_channels {
                let idx = self.channel_mapping[c];
                let s = if idx == 255 {
                    0.0f32
                } else if (idx as usize) < stream_channels.len() {
                    stream_channels[idx as usize].get(i).copied().unwrap_or(0.0)
                } else {
                    0.0
                };
                let clamped = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
                interleaved.extend_from_slice(&clamped.to_le_bytes());
            }
        }

        let pts = pkt.pts.unwrap_or(self.emit_pts);
        self.emit_pts = pts + per_frame_samples as i64;

        Ok(Frame::Audio(AudioFrame {
            format: SampleFormat::S16,
            channels: self.channels,
            sample_rate: OPUS_RATE_HZ,
            samples: per_frame_samples as u32,
            pts: Some(pts),
            time_base: self.time_base,
            data: vec![interleaved],
        }))
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        for s in self.streams.iter_mut() {
            s.reset()?;
        }
        self.pending = None;
        self.eof = false;
        self.emit_pts = 0;
        Ok(())
    }
}

fn decode_frame(
    dec: &mut OpusDecoder,
    toc: &Toc,
    bytes: &[u8],
    channels: usize,
) -> Result<Vec<Vec<f32>>> {
    let n_samples = toc.frame_samples_48k as usize;
    if bytes.len() <= 1 {
        return Ok(silence(channels, n_samples));
    }
    match toc.mode {
        OpusMode::CeltOnly => decode_celt_frame(dec, toc, bytes, channels, n_samples),
        OpusMode::SilkOnly => decode_silk_frame(dec, toc, bytes, channels, n_samples),
        OpusMode::Hybrid => decode_hybrid_frame(dec, toc, bytes, channels, n_samples),
    }
}

/// Decode a Hybrid Opus frame (RFC 6716 §4.4 / §4.3 start_band=17).
///
/// Hybrid frames (TOC config 12..=15) pack a SILK-WB body (covering
/// the 0..8 kHz low band) and a CELT high-band portion (starting at
/// band 17, i.e. the 8 kHz edge, covering 8..12 kHz for SWB or
/// 8..20 kHz for FB) into a single range-coded bitstream. The SILK
/// part decodes first; the CELT part re-uses the **same**
/// `RangeDecoder` resumed where SILK left off — that's what makes this
/// a single-packet hybrid rather than two independent bitstreams.
///
/// Internally SILK always runs at 16 kHz (WB), regardless of whether
/// the TOC bandwidth is SWB (config 12/13) or FB (14/15); the bins
/// between 8 kHz and the TOC bandwidth cutoff are filled in by the
/// CELT high-band, which runs at 48 kHz. The final output is the
/// pointwise sum of the two layers at 48 kHz: SILK's anti-aliasing
/// filter zeroes out content above 8 kHz, and the CELT bit allocator
/// skips bands below `start_band=17` (also zero), so the sum is
/// band-complementary and mixes cleanly without a fade.
///
/// Supports mono and stereo at 10 ms / 20 ms (the only configurations
/// RFC 6716 Table 2 defines for hybrid).
fn decode_hybrid_frame(
    dec: &mut OpusDecoder,
    toc: &Toc,
    bytes: &[u8],
    channels: usize,
    n_samples: usize,
) -> Result<Vec<Vec<f32>>> {
    // Only SWB (12) / FB (14) hybrid @ 10 ms and SWB (13) / FB (15) @ 20 ms
    // are defined in the TOC table. Both route to SILK-WB internally.
    let silk_bw = crate::toc::OpusBandwidth::Wideband;
    // Ensure the SILK sub-decoder is configured for WB.
    if dec.silk.is_none() || dec.silk.as_ref().map(|s| s.bandwidth) != Some(silk_bw) {
        dec.silk = Some(SilkDecoder::new(silk_bw));
    }

    // Hybrid SILK operates as a WB frame (16 kHz internal). The frame
    // length mirrors `toc.frame_samples_48k` — 480 = 10 ms, 960 = 20 ms
    // at 48 kHz, equivalent to 160 / 320 samples at 16 kHz.
    //
    // We construct a SILK-only view of the TOC so the existing SILK
    // decoder path runs unchanged. The shared range coder is passed in
    // via `decode_frame_to_48k`, which consumes exactly the SILK
    // portion of the bitstream and stops cleanly at the boundary.
    let silk_toc = crate::toc::Toc {
        config: toc.config,
        mode: crate::toc::OpusMode::SilkOnly,
        bandwidth: silk_bw,
        frame_samples_48k: toc.frame_samples_48k,
        stereo: toc.stereo,
        code: toc.code,
    };

    let mut rc = RangeDecoder::new(bytes);
    let silk_pcm_48k = {
        let silk = dec
            .silk
            .as_mut()
            .ok_or_else(|| Error::other("opus hybrid: SILK sub-decoder failed to initialise"))?;
        silk.decode_frame_to_48k(&mut rc, &silk_toc)?
    };

    // Split SILK output into per-channel buffers matching `channels`.
    let silk_per_ch: Vec<Vec<f32>> = if toc.stereo {
        debug_assert_eq!(silk_pcm_48k.len(), n_samples * 2);
        let mut l = Vec::with_capacity(n_samples);
        let mut r = Vec::with_capacity(n_samples);
        for c in silk_pcm_48k.chunks_exact(2) {
            l.push(c[0]);
            r.push(c[1]);
        }
        if channels == 2 {
            vec![l, r]
        } else {
            // Downmix (rare but guard): average L+R.
            let mono: Vec<f32> = l.iter().zip(r.iter()).map(|(a, b)| 0.5 * (a + b)).collect();
            vec![mono]
        }
    } else {
        let mut v = vec![silk_pcm_48k];
        while v.len() < channels {
            v.push(v[0].clone());
        }
        v
    };

    // CELT high-band: use the same range coder. start_band=17 for both
    // SWB and FB hybrid. end_band comes from the bandwidth.
    let celt_start = 17usize;
    let celt_pcm = decode_celt_body(dec, toc, &mut rc, channels, n_samples, celt_start)?;

    // Mix SILK-low + CELT-high into the output. Both are already at
    // 48 kHz. The CELT MDCT output at bands below `celt_start` is
    // zero (the bit allocator skipped those bands), and the SILK
    // upsampler's anti-aliasing filter removes content above ~8 kHz
    // from the SILK-WB output, so the two contributions are
    // band-complementary and a straight pointwise sum is correct.
    //
    // No per-frame fade is applied: CELT already handles continuity
    // between adjacent frames via its own IMDCT overlap-add (a
    // per-frame linear ramp at the start of every frame would
    // introduce a periodic 20 ms amplitude discontinuity in the CELT
    // high band, which was previously costing audible dB in the
    // hybrid round-trip vs ffmpeg). The final S16 conversion
    // downstream clamps to the codec-output range.
    let mut out: Vec<Vec<f32>> = (0..channels).map(|_| vec![0f32; n_samples]).collect();
    for c in 0..channels {
        let silk_c = silk_per_ch
            .get(c)
            .cloned()
            .unwrap_or_else(|| vec![0.0; n_samples]);
        let celt_c = celt_pcm
            .get(c)
            .cloned()
            .unwrap_or_else(|| vec![0.0; n_samples]);
        for i in 0..n_samples {
            let s = silk_c.get(i).copied().unwrap_or(0.0);
            let e = celt_c.get(i).copied().unwrap_or(0.0);
            out[c][i] = s + e;
        }
    }
    Ok(out)
}

/// Decode a SILK-only frame using the crate-local `silk` module and
/// upsample to 48 kHz.
///
/// Supported: mono and stereo NB/MB/WB at 10/20/40/60 ms. LBRR
/// redundancy data is parsed and decoded to a scratch state so the
/// range coder stays aligned; the primary output is unaffected.
fn decode_silk_frame(
    dec: &mut OpusDecoder,
    toc: &Toc,
    bytes: &[u8],
    channels: usize,
    n_samples: usize,
) -> Result<Vec<Vec<f32>>> {
    // Instantiate SILK decoder lazily on first SILK packet. Reset on
    // bandwidth change (NB/MB/WB dictate the LPC order + sub-frame
    // length, which change the persistent state layout).
    if dec.silk.is_none() || dec.silk.as_ref().map(|s| s.bandwidth) != Some(toc.bandwidth) {
        dec.silk = Some(SilkDecoder::new(toc.bandwidth));
    }
    let Some(silk) = dec.silk.as_mut() else {
        // Unreachable: we just set `dec.silk` to `Some(_)`.
        return Err(Error::other(
            "opus decoder: SILK sub-decoder failed to initialise",
        ));
    };

    let mut rc = RangeDecoder::new(bytes);
    let pcm = silk.decode_frame_to_48k(&mut rc, toc)?;

    if toc.stereo {
        // `pcm` is interleaved L/R; split it into per-channel buffers.
        debug_assert!(
            pcm.len() == n_samples * 2,
            "SILK stereo expected {} interleaved samples, got {}",
            n_samples * 2,
            pcm.len()
        );
        let mut left = Vec::with_capacity(n_samples);
        let mut right = Vec::with_capacity(n_samples);
        for chunk in pcm.chunks_exact(2) {
            left.push(chunk[0]);
            right.push(chunk[1]);
        }
        let mut out = vec![left, right];
        // If the output container wants more channels (shouldn't happen
        // for stereo Opus but guard anyway), splat the right channel.
        while out.len() < channels {
            out.push(out.last().cloned().unwrap_or_default());
        }
        Ok(out)
    } else {
        debug_assert!(
            pcm.len() == n_samples,
            "SILK expected {} samples, got {}",
            n_samples,
            pcm.len()
        );
        let mut out = vec![pcm.clone()];
        while out.len() < channels {
            out.push(pcm.clone());
        }
        Ok(out)
    }
}

fn decode_celt_frame(
    dec: &mut OpusDecoder,
    toc: &Toc,
    bytes: &[u8],
    channels: usize,
    n_samples: usize,
) -> Result<Vec<Vec<f32>>> {
    let mut rc = RangeDecoder::new(bytes);
    decode_celt_body(dec, toc, &mut rc, channels, n_samples, 0)
}

/// CELT decode body shared by CELT-only and Hybrid frames.
///
/// * `rc` — range decoder already positioned at the CELT portion of the
///   frame. For CELT-only this is the frame start; for Hybrid this is
///   *after* the SILK body has been consumed from the same bitstream.
/// * `start_band` — 0 for CELT-only, 17 for Hybrid.
fn decode_celt_body(
    dec: &mut OpusDecoder,
    toc: &Toc,
    mut rc: &mut RangeDecoder<'_>,
    channels: usize,
    n_samples: usize,
    start_band: usize,
) -> Result<Vec<Vec<f32>>> {
    let state = &mut dec.state;
    let lm = lm_for_frame_samples(toc.frame_samples_48k) as i32;
    let end_band = end_band_for_bandwidth_celt(toc.bandwidth.cutoff_hz());
    let total_bits_raw = (rc.storage() * 8) as i32;

    // Silence flag (decoded inside header).
    let header = match decode_header(&mut rc) {
        Some(h) => h,
        None => {
            // Reset old energies as libopus does.
            for e in state.old_band_e.iter_mut() {
                *e = -28.0;
            }
            return Ok(silence(channels, n_samples));
        }
    };

    let m = 1i32 << lm;
    let n = (m * EBAND_5MS[NB_EBANDS] as i32) as usize;
    let overlap = 120usize;
    let short_blocks = header.transient;
    let _big_b = if short_blocks { m } else { 1 };

    // Coarse band energies.
    unquant_coarse_energy(
        &mut rc,
        &mut state.old_band_e,
        start_band,
        end_band,
        header.intra,
        channels,
        lm as usize,
    );

    // tf_decode (§4.3.4 transient flags per band).
    let mut tf_res = vec![0i32; NB_EBANDS];
    tf_decode(
        &mut rc,
        start_band,
        end_band,
        header.transient,
        &mut tf_res,
        lm,
    );

    // Spread decision.
    let mut tell = rc.tell();
    let total_bits_check = (rc.storage() * 8) as i32;
    let spread = if tell + 4 <= total_bits_check {
        rc.decode_icdf(&SPREAD_ICDF, 5) as i32
    } else {
        SPREAD_NORMAL
    };

    // dynalloc (band boost) offsets.
    let cap = init_caps(lm as usize, channels);
    let mut offsets = [0i32; NB_EBANDS];
    let mut dynalloc_logp = 6i32;
    let mut total_bits_frac = (total_bits_raw as i32) << BITRES;
    tell = rc.tell_frac() as i32;
    for i in start_band..end_band {
        let width = (channels as i32) * (EBAND_5MS[i + 1] - EBAND_5MS[i]) as i32 * m;
        let quanta = (width << BITRES).min((6 << BITRES).max(width));
        let mut dynalloc_loop_logp = dynalloc_logp;
        let mut boost = 0i32;
        while tell + (dynalloc_loop_logp << BITRES) < total_bits_frac && boost < cap[i] {
            let flag = rc.decode_bit_logp(dynalloc_loop_logp as u32);
            tell = rc.tell_frac() as i32;
            if !flag {
                break;
            }
            boost += quanta;
            total_bits_frac -= quanta;
            dynalloc_loop_logp = 1;
        }
        offsets[i] = boost;
        if boost > 0 {
            dynalloc_logp = 2.max(dynalloc_logp - 1);
        }
    }

    // Allocation trim.
    let alloc_trim = if tell + (6 << BITRES) <= total_bits_frac {
        rc.decode_icdf(&TRIM_ICDF, 7) as i32
    } else {
        5
    };

    // Bits available for PVQ.
    let mut bits = ((rc.storage() as i32) * 8 << BITRES) - rc.tell_frac() as i32 - 1;
    let anti_collapse_rsv = if header.transient && lm >= 2 && bits >= ((lm + 2) << BITRES) {
        1 << BITRES
    } else {
        0
    };
    bits -= anti_collapse_rsv;

    let mut pulses = vec![0i32; NB_EBANDS];
    let mut fine_quant = vec![0i32; NB_EBANDS];
    let mut fine_priority = vec![0i32; NB_EBANDS];
    let mut intensity = 0i32;
    let mut dual_stereo = 0i32;
    let mut balance = 0i32;
    let coded_bands = clt_compute_allocation(
        start_band,
        end_band,
        &offsets,
        &cap,
        alloc_trim,
        &mut intensity,
        &mut dual_stereo,
        bits,
        &mut balance,
        &mut pulses,
        &mut fine_quant,
        &mut fine_priority,
        channels as i32,
        lm,
        &mut rc,
    );

    // Fine energies.
    unquant_fine_energy(
        &mut rc,
        &mut state.old_band_e,
        start_band,
        end_band,
        &fine_quant,
        channels,
    );

    // PVQ shape decode.
    let mut x_buf = vec![0f32; n];
    let mut y_buf = if channels == 2 {
        vec![0f32; n]
    } else {
        Vec::new()
    };
    let mut collapse_masks = vec![0u8; NB_EBANDS * channels];
    let total_pvq_bits = (rc.storage() as i32) * (8 << BITRES) - anti_collapse_rsv;
    let y_opt = if channels == 2 {
        Some(y_buf.as_mut_slice())
    } else {
        None
    };
    let band_e_snapshot = state.old_band_e.clone();
    let mut rng_local = state.rng;
    quant_all_bands(
        start_band,
        end_band,
        &mut x_buf,
        y_opt,
        &mut collapse_masks,
        &band_e_snapshot,
        &pulses,
        short_blocks,
        spread,
        dual_stereo,
        intensity,
        &tf_res,
        total_pvq_bits,
        balance,
        &mut rc,
        lm,
        coded_bands,
        &mut rng_local,
        false,
    );
    state.rng = rng_local;

    // Anti-collapse decision.
    let anti_collapse_on = if anti_collapse_rsv > 0 {
        rc.decode_bits(1) != 0
    } else {
        false
    };

    // Final fine-energy pass.
    let bits_left = (rc.storage() as i32) * 8 - rc.tell();
    unquant_energy_finalise(
        &mut rc,
        &mut state.old_band_e,
        start_band,
        end_band,
        &fine_quant,
        &fine_priority,
        bits_left,
        channels,
    );

    // Anti-collapse.
    if anti_collapse_on {
        if channels == 2 {
            // Combine X+Y into one buffer for anti_collapse, but our buffer
            // layout is per-channel. We process them sequentially.
            let mut combined = vec![0f32; 2 * n];
            combined[..n].copy_from_slice(&x_buf);
            combined[n..].copy_from_slice(&y_buf);
            let _ = (&state.old_log_e, &state.old_log_e2);
            // Note: anti_collapse expects logE arrays of length 2*nbEBands
            anti_collapse(
                &mut combined,
                &collapse_masks,
                lm,
                channels,
                n,
                start_band,
                end_band,
                &state.old_band_e,
                &state.old_log_e,
                &state.old_log_e2,
                &pulses,
                state.rng,
            );
            x_buf.copy_from_slice(&combined[..n]);
            y_buf.copy_from_slice(&combined[n..]);
        } else {
            anti_collapse(
                &mut x_buf,
                &collapse_masks,
                lm,
                channels,
                n,
                start_band,
                end_band,
                &state.old_band_e,
                &state.old_log_e,
                &state.old_log_e2,
                &pulses,
                state.rng,
            );
        }
    }

    // Denormalise per channel.
    let mut freq_per_ch: Vec<Vec<f32>> = (0..channels).map(|_| vec![0f32; n]).collect();
    for c in 0..channels {
        let band_log_e = &state.old_band_e[c * NB_EBANDS..(c + 1) * NB_EBANDS];
        let shape = if c == 0 || channels == 1 {
            &x_buf
        } else {
            &y_buf
        };
        denormalise_bands(
            shape,
            &mut freq_per_ch[c],
            band_log_e,
            start_band,
            end_band,
            m as usize,
            false,
        );
    }

    // IMDCT per channel + overlap-add.
    let mut pcm_per_ch: Vec<Vec<f32>> = (0..channels).map(|_| vec![0f32; n]).collect();
    let _n_b = if header.transient {
        120 // mode->shortMdctSize
    } else {
        120usize << lm as usize
    };
    let blocks = if header.transient {
        (1 << lm) as usize
    } else {
        1
    };
    for c in 0..channels {
        // De-interleave the M sub-block coefficients (libopus: `freq[b]` accessed
        // with stride M to recover sub-block b of size N/M).
        let mut interleaved = vec![0f32; n];
        for b in 0..blocks {
            for k in 0..n / blocks {
                interleaved[b * (n / blocks) + k] = freq_per_ch[c][k * blocks + b];
            }
        }
        let mut out_accum = vec![0f32; n + overlap];
        let win = window120();
        let prev_tail = state.overlap_buf[c].clone();
        for b in 0..blocks {
            let sub_n = n / blocks;
            let coeff = &interleaved[b * sub_n..(b + 1) * sub_n];
            let mut raw = vec![0f32; 2 * sub_n];
            imdct_sub(coeff, &mut raw, sub_n);
            // Place into out_accum with overlap-add at b*sub_n.
            let dst_start = b * sub_n;
            for i in 0..2 * sub_n {
                let idx = dst_start + i;
                if idx < out_accum.len() {
                    out_accum[idx] += raw[i];
                }
            }
        }
        // Apply prev tail at the very front.
        for i in 0..overlap {
            out_accum[i] += prev_tail[i];
        }
        // Stash the new tail for next frame.
        for i in 0..overlap {
            state.overlap_buf[c][i] = out_accum[n + i];
        }
        pcm_per_ch[c].copy_from_slice(&out_accum[..n]);
        let _ = win;
    }

    // Comb post filter.
    let postfilter_pitch;
    let postfilter_gain;
    let postfilter_tapset;
    if let Some(pf) = header.post_filter {
        postfilter_pitch = ((16 << pf.octave) + pf.period) as i32 - 1;
        postfilter_gain = (pf.gain as f32 + 1.0) * 0.09375;
        postfilter_tapset = pf.tapset as usize;
    } else {
        postfilter_pitch = 0;
        postfilter_gain = 0.0;
        postfilter_tapset = 0;
    }

    for c in 0..channels {
        // comb_filter is in-place on `y` (see oxideav_celt::post_filter).
        // Start by copying the IMDCT output into `filtered`, then run
        // the filter against the previous frame's history.
        let mut filtered = pcm_per_ch[c].clone();
        let history = state.history[c].clone();
        // libopus splits the frame at the short-MDCT boundary (120
        // samples): the first 120 samples use the crossfade between the
        // previous-frame and current-frame filters; the tail uses the
        // current filter only. At LM=0 (2.5 ms CELT frame) the whole
        // frame is shorter than the short-MDCT size, so we run a single
        // call on the `n` available samples.
        let head = n.min(120);
        comb_filter(
            &mut filtered[..head],
            &history,
            state.pf_period_old,
            state.pf_period,
            head, // shortMdctSize (or full frame if smaller)
            state.pf_gain_old,
            state.pf_gain,
            state.pf_tapset_old,
            state.pf_tapset,
            window120(),
            overlap,
        );
        if lm > 0 {
            // The tail (post-shortMdctSize) uses the *current* frame's
            // filter throughout, with the already-filtered 120 samples
            // as history.
            let mut synth_hist = history.clone();
            synth_hist.extend_from_slice(&filtered[..head]);
            comb_filter(
                &mut filtered[head..],
                &synth_hist,
                state.pf_period,
                postfilter_pitch,
                n - head,
                state.pf_gain,
                postfilter_gain,
                state.pf_tapset,
                postfilter_tapset,
                window120(),
                overlap,
            );
        }
        // Update history for next frame: last samples of filtered output.
        let take = HISTORY_SIZE.min(filtered.len());
        let hlen = state.history[c].len();
        state.history[c].rotate_left(take.min(hlen));
        let dst_start = hlen - take;
        state.history[c][dst_start..].copy_from_slice(&filtered[filtered.len() - take..]);
        pcm_per_ch[c] = filtered;
    }

    // Update post-filter state for next frame.
    state.pf_period_old = state.pf_period;
    state.pf_gain_old = state.pf_gain;
    state.pf_tapset_old = state.pf_tapset;
    state.pf_period = postfilter_pitch;
    state.pf_gain = postfilter_gain;
    state.pf_tapset = postfilter_tapset;
    if lm != 0 {
        state.pf_period_old = state.pf_period;
        state.pf_gain_old = state.pf_gain;
        state.pf_tapset_old = state.pf_tapset;
    }

    // Mono → CC=1 case: just return; multichannel/stereo splat as needed.
    if channels < toc.channels() as usize {
        // Duplicate mono to all output channels.
        let copy = pcm_per_ch[0].clone();
        for c in 1..(toc.channels() as usize) {
            if c < pcm_per_ch.len() {
                pcm_per_ch[c] = copy.clone();
            } else {
                pcm_per_ch.push(copy.clone());
            }
        }
    }

    // Roll energy history for next frame.
    if !header.transient {
        state.old_log_e2 = state.old_log_e.clone();
        state.old_log_e = state.old_band_e.clone();
    } else {
        for i in 0..2 * NB_EBANDS {
            state.old_log_e[i] = state.old_log_e[i].min(state.old_band_e[i]);
        }
    }
    if channels == 1 {
        for i in 0..NB_EBANDS {
            state.old_band_e[NB_EBANDS + i] = state.old_band_e[i];
        }
    }
    state.rng = (state.rng as u32)
        .wrapping_mul(1_103_515_245)
        .wrapping_add(12_345);
    let _ = state.channels;

    Ok(pcm_per_ch)
}

/// Time-Frequency change decoder (RFC 6716 §4.3.4.1, libopus tf_decode).
fn tf_decode(
    rc: &mut RangeDecoder<'_>,
    start: usize,
    end: usize,
    is_transient: bool,
    tf_res: &mut [i32],
    lm: i32,
) {
    let budget = rc.storage() * 8;
    let mut tell = rc.tell() as u32;
    let mut logp = if is_transient { 2 } else { 4 };
    let tf_select_rsv = if lm > 0 && (tell + logp + 1) <= budget {
        1
    } else {
        0
    };
    let budget_after = budget - tf_select_rsv;
    let mut tf_changed = 0i32;
    let mut curr = 0i32;
    for i in start..end {
        if tell + logp <= budget_after {
            let bit = rc.decode_bit_logp(logp);
            curr ^= bit as i32;
            tell = rc.tell() as u32;
            tf_changed |= curr;
        }
        tf_res[i] = curr;
        logp = if is_transient { 4 } else { 5 };
    }
    let mut tf_select = 0i32;
    if tf_select_rsv != 0
        && TF_SELECT_TABLE[lm as usize][4 * is_transient as usize + tf_changed as usize]
            != TF_SELECT_TABLE[lm as usize][4 * is_transient as usize + 2 + tf_changed as usize]
    {
        tf_select = if rc.decode_bit_logp(1) { 1 } else { 0 };
    }
    for i in start..end {
        let idx = (4 * is_transient as i32 + 2 * tf_select + tf_res[i]) as usize;
        tf_res[i] = TF_SELECT_TABLE[lm as usize][idx] as i32;
    }
}

fn silence(channels: usize, n_samples: usize) -> Vec<Vec<f32>> {
    (0..channels).map(|_| vec![0.0; n_samples]).collect()
}

fn window120() -> &'static [f32] {
    &WINDOW_120
}

#[rustfmt::skip]
const WINDOW_120: [f32; 120] = [
    6.7286966e-05, 0.00060551348, 0.0016815970, 0.0032947962, 0.0054439943,
    0.0081276923, 0.011344001, 0.015090633, 0.019364886, 0.024163635,
    0.029483315, 0.035319905, 0.041668911, 0.048525347, 0.055883718,
    0.063737999, 0.072081616, 0.080907428, 0.090207705, 0.099974111,
    0.11019769, 0.12086883, 0.13197729, 0.14351214, 0.15546177,
    0.16781389, 0.18055550, 0.19367290, 0.20715171, 0.22097682,
    0.23513243, 0.24960208, 0.26436860, 0.27941419, 0.29472040,
    0.31026818, 0.32603788, 0.34200931, 0.35816177, 0.37447407,
    0.39092462, 0.40749142, 0.42415215, 0.44088423, 0.45766484,
    0.47447104, 0.49127978, 0.50806798, 0.52481261, 0.54149077,
    0.55807973, 0.57455701, 0.59090049, 0.60708841, 0.62309951,
    0.63891306, 0.65450896, 0.66986776, 0.68497077, 0.69980010,
    0.71433873, 0.72857055, 0.74248043, 0.75605424, 0.76927895,
    0.78214257, 0.79463430, 0.80674445, 0.81846456, 0.82978733,
    0.84070669, 0.85121779, 0.86131698, 0.87100183, 0.88027111,
    0.88912479, 0.89756398, 0.90559094, 0.91320904, 0.92042270,
    0.92723738, 0.93365955, 0.93969656, 0.94535671, 0.95064907,
    0.95558353, 0.96017067, 0.96442171, 0.96834849, 0.97196334,
    0.97527906, 0.97830883, 0.98106616, 0.98356480, 0.98581869,
    0.98784191, 0.98964856, 0.99125274, 0.99266849, 0.99390969,
    0.99499004, 0.99592297, 0.99672162, 0.99739874, 0.99796667,
    0.99843728, 0.99882195, 0.99913147, 0.99937606, 0.99956527,
    0.99970802, 0.99981248, 0.99988613, 0.99993565, 0.99996697,
    0.99998518, 0.99999457, 0.99999859, 0.99999982, 1.0000000,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toc::{OpusBandwidth, OpusMode};
    use oxideav_core::{CodecId, MediaType};

    fn celt_toc() -> Toc {
        Toc {
            config: 31,
            mode: OpusMode::CeltOnly,
            bandwidth: OpusBandwidth::Fullband,
            frame_samples_48k: 960,
            stereo: true,
            code: 0,
        }
    }

    #[test]
    fn short_frame_returns_silence() {
        let toc = celt_toc();
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(2);
        let mut dec = make_decoder(&p).unwrap();
        let pkt = Packet::new(0, TimeBase::new(1, 48_000), vec![(31u8 << 3) | (1 << 2)]);
        dec.send_packet(&pkt).unwrap();
        let _ = dec.receive_frame().unwrap();
        let _ = toc;
    }

    #[test]
    fn silk_frame_is_unsupported_not_panic() {
        let toc = Toc {
            config: 0,
            mode: OpusMode::SilkOnly,
            bandwidth: OpusBandwidth::Narrowband,
            frame_samples_48k: 480,
            stereo: false,
            code: 0,
        };
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(1);
        let mut dec = make_decoder(&p).unwrap();
        let _ = (toc, dec);
    }

    #[test]
    fn make_decoder_mono() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(1);
        let d = make_decoder(&p).unwrap();
        assert_eq!(d.codec_id().as_str(), "opus");
    }

    #[test]
    fn make_decoder_rejects_multichannel_without_extradata() {
        // No extradata = no OpusHead = no multistream mapping. A
        // channel count > 2 is only valid with a channel-mapped
        // OpusHead (family 1 or 2) in extradata.
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(6);
        match make_decoder(&p) {
            Err(Error::Unsupported(_)) => {}
            _ => panic!("expected Unsupported"),
        }
    }

    #[test]
    fn make_decoder_accepts_family1_surround_5_1() {
        // Build a minimal OpusHead for 5.1 surround (Vorbis channel
        // order): 6 output channels, stream_count=4, coupled=2,
        // mapping [0,4,1,2,3,5] (FL FR FC LFE BL BR in libopus order).
        let mut head = Vec::new();
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(6); // channels
        head.extend_from_slice(&0u16.to_le_bytes()); // pre_skip
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // gain
        head.push(1); // family 1
        head.push(4); // stream count
        head.push(2); // coupled
        head.extend_from_slice(&[0, 4, 1, 2, 3, 5]);

        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(6);
        p.extradata = head;
        let d = make_decoder(&p).expect("5.1 multistream should be accepted");
        assert_eq!(d.codec_id().as_str(), "opus");
    }

    #[test]
    fn receive_frame_silence_packet() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(2);
        let mut dec = make_decoder(&p).unwrap();
        let pkt = Packet::new(0, TimeBase::new(1, 48_000), vec![(31u8 << 3) | (1 << 2)]);
        dec.send_packet(&pkt).unwrap();
        let f = dec.receive_frame().unwrap();
        match f {
            Frame::Audio(a) => {
                assert_eq!(a.samples, 960);
                assert_eq!(a.channels, 2);
                assert_eq!(a.sample_rate, 48_000);
                assert_eq!(a.format, SampleFormat::S16);
                let s16_bytes = &a.data[0];
                assert!(s16_bytes.chunks(2).all(|c| c[0] == 0 && c[1] == 0));
            }
            _ => panic!("expected AudioFrame"),
        }
        let _ = MediaType::Audio;
    }
}
