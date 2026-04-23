//! Opus encoder — CELT-only full-band + the full SILK-only config
//! matrix (configs 0..=11), mono and stereo, 10 / 20 / 40 / 60 ms.
//!
//! # Mode selection
//!
//! * [`OpusEncoder::new_celt_only_full_band`] — CELT-only fullband
//!   20 ms (config 31). Accepts 48 kHz mono/stereo input. Stereo is
//!   downmixed to mono before the CELT mono core; see the original
//!   CELT section below for the honest caveat about the stereo TOC bit.
//!
//! * [`SilkEncoder`] exposes one named constructor per (bandwidth,
//!   channels, duration) tuple:
//!   - bandwidth ∈ {NB, MB, WB} (8 / 12 / 16 kHz internal rates),
//!   - channels ∈ {mono, stereo},
//!   - duration ∈ {10, 20, 40, 60} ms.
//!
//!   That gives 24 constructors, covering all 12 SILK-only `config`
//!   values × stereo bit. Each accepts either the SILK internal rate
//!   or 48 kHz; the latter is downsampled by a box-average pre-filter.
//!   40 / 60 ms packets carry 2 / 3 back-to-back 20 ms SILK frame
//!   bodies per RFC 6716 §4.2.4, so they produce a single Opus packet
//!   with framing code 0 (not code-1/2/3 — the SILK frames share one
//!   TOC byte and one range-coder bitstream).
//!
//! [`OpusEncoder::new`] routes by the `CodecParameters::sample_rate`:
//! 48 kHz mono/stereo → CELT-only FB; anything else → `Unsupported`,
//! because switching to SILK invisibly would be a nasty foot-gun for
//! callers that expected 48 kHz output parity. To emit SILK packets,
//! construct a [`SilkEncoder`] explicitly.
//!
//! Hybrid (SILK+CELT) is not implemented on either path.
//!
//! # Packet layout (RFC 6716 §3)
//!
//! ```text
//!   [ TOC byte ] [ CELT bitstream bytes ... ]
//! ```
//!
//! where the TOC byte is `(config << 3) | (stereo << 2) | code` with
//! `config = 31`, `stereo ∈ {0, 1}`, `code = 0` (single frame).
//!
//! # Supported inputs
//!
//! * S16 / S16P / F32 / F32P sample formats.
//! * 48 kHz sample rate only.
//! * Mono (channels = 1) — native path.
//! * Stereo (channels = 2) — **downmixed to mono** before being fed to
//!   the mono-only CELT encoder; the TOC is emitted with `stereo = 0`.
//!   A real CELT stereo path (coupled L/R PVQ with intensity /
//!   dual-stereo) would be needed to honestly advertise `stereo = 1`
//!   in the TOC, and the `oxideav-celt` encoder is mono-only today —
//!   see its module docs. The signal survives and decodes cleanly as
//!   duplicated-mono on both channels; per-channel detail is lost.
//!
//! # Unsupported
//!
//! * Framing codes 1/2/3 (multi-frame packets) — not emitted. 40 / 60
//!   ms SILK packets *are* emitted via RFC §4.2.4's multiple-SILK-
//!   frames-per-Opus-frame mechanism (still code = 0).
//! * CELT 2.5 / 5 / 10 ms frame sizes.
//! * Hybrid (SILK+CELT) mode.
//! * More than 2 channels.

use std::collections::VecDeque;

use oxideav_celt::encoder::{CeltEncoder, FRAME_SAMPLES, SAMPLE_RATE};
use oxideav_codec::Encoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

/// `config` field value for CELT-only, fullband, 20 ms frames.
const OPUS_CONFIG_CELT_FB_20MS: u8 = 31;

/// Build a TOC byte for config 31 (CELT-only FB 20 ms), code-0 (single
/// frame packet), with the given stereo bit.
///
/// Layout (RFC 6716 §3.1): `config(5) | stereo(1) | code(2)`.
pub fn build_toc_byte(stereo: bool) -> u8 {
    let stereo_bit: u8 = if stereo { 1 } else { 0 };
    (OPUS_CONFIG_CELT_FB_20MS << 3) | (stereo_bit << 2) // code = 0 (single frame)
}

/// Number of PCM samples per 20 ms Opus/CELT frame at 48 kHz.
pub const OPUS_FRAME_SAMPLES: usize = 960;

pub struct OpusEncoder {
    /// Output-stream parameters (after any channel-count adjustments).
    out_params: CodecParameters,
    /// Channel count on the *input* frames (1 or 2). Stereo inputs are
    /// downmixed to mono before hitting the CELT encoder.
    input_channels: u16,
    /// The underlying mono CELT encoder.
    celt: CeltEncoder,
    /// Output packet queue (one Opus packet per 20 ms of input).
    output: VecDeque<Packet>,
    /// PTS counter (in 48 kHz samples).
    pts_counter: i64,
}

impl OpusEncoder {
    /// Build a new Opus encoder. Mode selection is purely driven by the
    /// sample rate in `params`: 48 kHz → CELT-only full-band 20 ms. Any
    /// other rate returns `Error::Unsupported`.
    ///
    /// For an explicit, mode-named entry point that keeps the call-site
    /// intent obvious, see [`OpusEncoder::new_celt_only_full_band`].
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = params.channels.unwrap_or(1);
        if channels == 0 || channels > 2 {
            return Err(Error::unsupported(format!(
                "opus encoder: only mono/stereo supported, got {channels}-channel input"
            )));
        }
        let sr = params.sample_rate.unwrap_or(SAMPLE_RATE);
        if sr != SAMPLE_RATE {
            return Err(Error::unsupported(format!(
                "opus encoder: input must be 48 kHz (got {sr}); resample before encoding"
            )));
        }

        // Drive the underlying CELT encoder as mono — stereo input is
        // downmixed on the way in. The CELT-mono path is the only one
        // implemented today.
        let mut celt_params = params.clone();
        celt_params.channels = Some(1);
        celt_params.sample_rate = Some(SAMPLE_RATE);
        // CeltEncoder expects its own codec id; clone the whole parameter
        // block and override the id so the inner encoder doesn't reject
        // us for a mismatch.
        celt_params.codec_id = CodecId::new(oxideav_celt::CODEC_ID_STR);
        let celt = CeltEncoder::new(&celt_params)?;

        // Output params: we report the *input* channel count so that the
        // downstream muxer keeps the packet's implied channel layout in
        // sync with what callers asked for. The bitstream body is always
        // a mono CELT frame though — see module docs.
        let mut out_params = params.clone();
        out_params.sample_rate = Some(SAMPLE_RATE);
        out_params.channels = Some(channels);

        Ok(Self {
            out_params,
            input_channels: channels,
            celt,
            output: VecDeque::new(),
            pts_counter: 0,
        })
    }

    /// Explicit CELT-only full-band (48 kHz, 20 ms) constructor. Equivalent
    /// to [`OpusEncoder::new`] with `params.sample_rate = Some(48_000)`,
    /// but documents the intent at the call site. Returns `Unsupported`
    /// if the caller passed a non-48 kHz rate.
    ///
    /// Channels must be 1 or 2. Stereo input is downmixed to mono — see
    /// the module docs for why.
    pub fn new_celt_only_full_band(params: &CodecParameters) -> Result<Self> {
        let sr = params.sample_rate.unwrap_or(SAMPLE_RATE);
        if sr != SAMPLE_RATE {
            return Err(Error::unsupported(format!(
                "opus encoder (CELT-only FB): input must be 48 kHz, got {sr}"
            )));
        }
        Self::new(params)
    }

    /// Pull all pending CELT packets out of the underlying encoder, wrap
    /// each in an Opus TOC byte, and push the resulting Opus packets to
    /// the output queue.
    fn drain_celt(&mut self) -> Result<()> {
        // CeltEncoder is mono-only so stereo_bit is always 0 here.
        let toc = build_toc_byte(false);
        loop {
            match self.celt.receive_packet() {
                Ok(celt_pkt) => {
                    let mut data = Vec::with_capacity(1 + celt_pkt.data.len());
                    data.push(toc);
                    data.extend_from_slice(&celt_pkt.data);
                    let tb = TimeBase::new(1, SAMPLE_RATE as i64);
                    let pts = self.pts_counter;
                    self.pts_counter += OPUS_FRAME_SAMPLES as i64;
                    let pkt = Packet::new(0, tb, data)
                        .with_pts(pts)
                        .with_duration(OPUS_FRAME_SAMPLES as i64);
                    self.output.push_back(pkt);
                }
                Err(Error::NeedMore) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

impl Encoder for OpusEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.out_params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let audio = match frame {
            Frame::Audio(a) => a,
            _ => {
                return Err(Error::invalid(
                    "opus encoder: expected audio frame, got video",
                ))
            }
        };
        if audio.sample_rate != SAMPLE_RATE {
            return Err(Error::unsupported(format!(
                "opus encoder: input must be 48 kHz (got {}); resample before encoding",
                audio.sample_rate
            )));
        }
        if audio.channels != self.input_channels {
            return Err(Error::invalid(format!(
                "opus encoder: frame channels ({}) differ from configured input channels ({})",
                audio.channels, self.input_channels
            )));
        }

        // Flatten the input into a mono f32 buffer regardless of whether
        // the container was mono (passthrough) or stereo (downmix).
        let mono = extract_mono_f32(audio)?;

        // Feed the CELT encoder as a single mono F32 frame.
        let mut bytes = Vec::with_capacity(mono.len() * 4);
        for &s in &mono {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let celt_frame = Frame::Audio(AudioFrame {
            format: SampleFormat::F32,
            channels: 1,
            sample_rate: SAMPLE_RATE,
            samples: mono.len() as u32,
            pts: audio.pts,
            time_base: TimeBase::new(1, SAMPLE_RATE as i64),
            data: vec![bytes],
        });
        self.celt.send_frame(&celt_frame)?;
        self.drain_celt()
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.output.pop_front() {
            Ok(p)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.celt.flush()?;
        self.drain_celt()?;
        Ok(())
    }
}

/// Decode the `AudioFrame`'s sample bytes into a mono f32 buffer, applying
/// a stereo → mono downmix (simple mean) when needed. Supports S16 and
/// F32 (interleaved or planar).
fn extract_mono_f32(audio: &AudioFrame) -> Result<Vec<f32>> {
    let n = audio.samples as usize;
    let ch = audio.channels as usize;
    if ch == 0 {
        return Err(Error::invalid("opus encoder: 0-channel audio frame"));
    }
    let mut out = vec![0f32; n];
    match audio.format {
        SampleFormat::S16 => {
            // Interleaved S16.
            let bytes = &audio.data[0];
            let needed = n * ch * 2;
            if bytes.len() < needed {
                return Err(Error::invalid(
                    "opus encoder: S16 input shorter than declared sample count",
                ));
            }
            for i in 0..n {
                let mut acc = 0i32;
                for c in 0..ch {
                    let off = (i * ch + c) * 2;
                    let s = i16::from_le_bytes([bytes[off], bytes[off + 1]]);
                    acc += s as i32;
                }
                out[i] = (acc as f32) / (ch as f32 * 32768.0);
            }
        }
        SampleFormat::S16P => {
            // One plane per channel. Mono = plane 0, stereo = two planes.
            if audio.data.len() < ch {
                return Err(Error::invalid("opus encoder: S16P input missing planes"));
            }
            for i in 0..n {
                let mut acc = 0i32;
                for c in 0..ch {
                    let plane = &audio.data[c];
                    if plane.len() < n * 2 {
                        return Err(Error::invalid(
                            "opus encoder: S16P plane shorter than declared sample count",
                        ));
                    }
                    let off = i * 2;
                    let s = i16::from_le_bytes([plane[off], plane[off + 1]]);
                    acc += s as i32;
                }
                out[i] = (acc as f32) / (ch as f32 * 32768.0);
            }
        }
        SampleFormat::F32 => {
            let bytes = &audio.data[0];
            let needed = n * ch * 4;
            if bytes.len() < needed {
                return Err(Error::invalid(
                    "opus encoder: F32 input shorter than declared sample count",
                ));
            }
            for i in 0..n {
                let mut acc = 0f32;
                for c in 0..ch {
                    let off = (i * ch + c) * 4;
                    acc += f32::from_le_bytes([
                        bytes[off],
                        bytes[off + 1],
                        bytes[off + 2],
                        bytes[off + 3],
                    ]);
                }
                out[i] = acc / ch as f32;
            }
        }
        SampleFormat::F32P => {
            if audio.data.len() < ch {
                return Err(Error::invalid("opus encoder: F32P input missing planes"));
            }
            for i in 0..n {
                let mut acc = 0f32;
                for c in 0..ch {
                    let plane = &audio.data[c];
                    if plane.len() < n * 4 {
                        return Err(Error::invalid(
                            "opus encoder: F32P plane shorter than declared sample count",
                        ));
                    }
                    let off = i * 4;
                    acc += f32::from_le_bytes([
                        plane[off],
                        plane[off + 1],
                        plane[off + 2],
                        plane[off + 3],
                    ]);
                }
                out[i] = acc / ch as f32;
            }
        }
        other => {
            return Err(Error::unsupported(format!(
                "opus encoder: sample format {:?} not supported (use S16 / S16P / F32 / F32P)",
                other
            )));
        }
    }
    // Sanity: the CELT encoder always consumes `FRAME_SAMPLES` (960) per
    // frame. We don't enforce `n == FRAME_SAMPLES` here because the
    // underlying CELT encoder buffers up to a frame boundary internally
    // — but we do surface any non-20-ms chunking downstream as Unsupported
    // there. The caller is free to send any number of samples per frame
    // as long as the aggregate ends on a frame boundary before `flush()`.
    let _ = FRAME_SAMPLES;
    Ok(out)
}

pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(OpusEncoder::new(params)?))
}

// ---------------------------------------------------------------------
// SILK encoder — NB / MB / WB mono + NB stereo, 20 ms.
// ---------------------------------------------------------------------

/// `config` field value for SILK-only, narrowband, 20 ms frames (§3.1
/// Table 2).
pub const OPUS_CONFIG_SILK_NB_20MS: u8 = 1;
/// `config` field value for SILK-only, mediumband, 20 ms frames.
pub const OPUS_CONFIG_SILK_MB_20MS: u8 = 5;
/// `config` field value for SILK-only, wideband, 20 ms frames.
pub const OPUS_CONFIG_SILK_WB_20MS: u8 = 9;

/// Number of PCM samples per 20 ms SILK NB frame at the internal 8 kHz
/// rate.
pub const SILK_NB_FRAME_SAMPLES_INTERNAL: usize = 160;
/// Samples per 20 ms SILK MB frame at the internal 12 kHz rate.
pub const SILK_MB_FRAME_SAMPLES_INTERNAL: usize = 240;
/// Samples per 20 ms SILK WB frame at the internal 16 kHz rate.
pub const SILK_WB_FRAME_SAMPLES_INTERNAL: usize = 320;

/// Number of PCM samples per 20 ms frame at the Opus output rate of
/// 48 kHz (for PTS accounting).
pub const SILK_FRAME_SAMPLES_48K: usize = 960;

/// Internal (SILK) rate for NB.
pub const SILK_NB_RATE: u32 = 8_000;
/// Internal (SILK) rate for MB.
pub const SILK_MB_RATE: u32 = 12_000;
/// Internal (SILK) rate for WB.
pub const SILK_WB_RATE: u32 = 16_000;

/// Build a TOC byte for a SILK-only narrowband 20 ms (config 1) packet.
///
/// Layout (RFC 6716 §3.1): `config(5) | stereo(1) | code(2)`.
pub fn build_silk_nb_20ms_toc(stereo: bool) -> u8 {
    let stereo_bit: u8 = if stereo { 1 } else { 0 };
    (OPUS_CONFIG_SILK_NB_20MS << 3) | (stereo_bit << 2) // code = 0
}

/// Build a TOC byte for a SILK-only mediumband 20 ms (config 5) packet.
pub fn build_silk_mb_20ms_toc(stereo: bool) -> u8 {
    let stereo_bit: u8 = if stereo { 1 } else { 0 };
    (OPUS_CONFIG_SILK_MB_20MS << 3) | (stereo_bit << 2)
}

/// Build a TOC byte for a SILK-only wideband 20 ms (config 9) packet.
pub fn build_silk_wb_20ms_toc(stereo: bool) -> u8 {
    let stereo_bit: u8 = if stereo { 1 } else { 0 };
    (OPUS_CONFIG_SILK_WB_20MS << 3) | (stereo_bit << 2)
}

/// Opus audio bandwidth for a SILK-only mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SilkBw {
    Nb,
    Mb,
    Wb,
}

impl SilkBw {
    fn internal_rate(self) -> u32 {
        match self {
            SilkBw::Nb => SILK_NB_RATE,
            SilkBw::Mb => SILK_MB_RATE,
            SilkBw::Wb => SILK_WB_RATE,
        }
    }
    /// Samples per 20 ms SILK frame at the internal rate.
    fn frame_samples_20ms(self) -> usize {
        match self {
            SilkBw::Nb => SILK_NB_FRAME_SAMPLES_INTERNAL,
            SilkBw::Mb => SILK_MB_FRAME_SAMPLES_INTERNAL,
            SilkBw::Wb => SILK_WB_FRAME_SAMPLES_INTERNAL,
        }
    }
}

/// Concrete SILK mode this `SilkEncoder` instance emits.
///
/// A SILK mode is the tuple of (bandwidth, channels, duration). Together
/// they fix the TOC `config` field (0..=11) plus the stereo bit. The
/// decoder's frame-size mapping is in RFC 6716 Table 2.
#[derive(Copy, Clone, Debug)]
struct SilkMode {
    bw: SilkBw,
    stereo: bool,
    /// Opus frame duration in milliseconds: 10, 20, 40, or 60.
    duration_ms: u32,
}

impl SilkMode {
    fn new(bw: SilkBw, stereo: bool, duration_ms: u32) -> Self {
        debug_assert!(matches!(duration_ms, 10 | 20 | 40 | 60));
        Self {
            bw,
            stereo,
            duration_ms,
        }
    }
    /// RFC 6716 Table 2 `config` field for this mode.
    fn config(self) -> u8 {
        // Each bandwidth occupies a block of 4 configs: NB=0..=3,
        // MB=4..=7, WB=8..=11. Within each block the order is
        // 10/20/40/60 ms.
        let base = match self.bw {
            SilkBw::Nb => 0u8,
            SilkBw::Mb => 4,
            SilkBw::Wb => 8,
        };
        let offset = match self.duration_ms {
            10 => 0,
            20 => 1,
            40 => 2,
            60 => 3,
            _ => unreachable!(),
        };
        base + offset
    }
    fn toc_byte(self) -> u8 {
        let stereo_bit: u8 = if self.stereo { 1 } else { 0 };
        (self.config() << 3) | (stereo_bit << 2)
    }
    fn internal_rate(self) -> u32 {
        self.bw.internal_rate()
    }
    /// Number of 20 ms SILK frames packed into this Opus frame per
    /// RFC §4.2.4. 10 ms = 1 (half-length), 20 ms = 1, 40 ms = 2,
    /// 60 ms = 3.
    fn silk_frames_per_packet(self) -> usize {
        match self.duration_ms {
            10 | 20 => 1,
            40 => 2,
            60 => 3,
            _ => unreachable!(),
        }
    }
    /// Number of sub-frames in each embedded SILK frame (2 for 10 ms,
    /// 4 for 20/40/60 ms).
    fn subframes_per_silk_frame(self) -> usize {
        if self.duration_ms == 10 {
            2
        } else {
            4
        }
    }
    /// Total internal-rate samples carried by this Opus frame.
    fn frame_samples_internal(self) -> usize {
        let per_silk = match self.duration_ms {
            10 => self.bw.frame_samples_20ms() / 2,
            _ => self.bw.frame_samples_20ms(),
        };
        per_silk * self.silk_frames_per_packet()
    }
    /// Samples per *embedded* SILK frame at the internal rate (one of
    /// the 1/2/3 blocks that make up a 10/20/40/60 ms Opus frame).
    fn samples_per_silk_frame(self) -> usize {
        self.frame_samples_internal() / self.silk_frames_per_packet()
    }
    /// PCM samples per Opus frame at the 48 kHz output rate (for PTS
    /// accounting).
    fn frame_samples_48k(self) -> usize {
        match self.duration_ms {
            10 => 480,
            20 => 960,
            40 => 1920,
            60 => 2880,
            _ => unreachable!(),
        }
    }
    fn input_channels(self) -> u16 {
        if self.stereo {
            2
        } else {
            1
        }
    }
    fn is_stereo(self) -> bool {
        self.stereo
    }
    /// Bytes of range-encoder storage to allocate per packet. Sized so
    /// the MVP per-sample nibble carrier fits with headroom; stereo
    /// doubles the mono budget. 40 / 60 ms packets carry 2 / 3 back-to-
    /// back SILK frame bodies, so the budget scales linearly.
    fn buffer_bytes(self) -> u32 {
        let samples = self.frame_samples_internal();
        // ~17 bits per sample worst-case (nibble+nibble + sign), plus
        // headers. Round up to a 64-byte multiple with 2× headroom.
        let base = (samples * 17) / 8 + 128;
        let doubled = if self.is_stereo() { base * 2 } else { base };
        doubled.next_multiple_of(64).max(384) as u32
    }
    /// 48 kHz → internal-rate downsample ratio (integer).
    fn downsample_ratio(self) -> usize {
        (SAMPLE_RATE / self.internal_rate()) as usize
    }
}

/// SILK-mode Opus encoder — covers the full SILK-only config matrix
/// (configs 0..=11), mono and stereo, 10 / 20 / 40 / 60 ms frames.
///
/// Emits a TOC byte matching the configured mode followed by the SILK
/// bitstream described in [`crate::silk::encoder`]. Accepts either the
/// SILK internal rate (8 kHz / 12 kHz / 16 kHz for NB / MB / WB) or the
/// 48 kHz Opus output rate; non-internal input is downsampled by a
/// simple box-average pre-filter.
///
/// Named entry points (one per (bandwidth, channels, duration) tuple):
///
/// * 20 ms mono: [`SilkEncoder::new_nb_mono_20ms`], `new_mb_mono_20ms`,
///   `new_wb_mono_20ms` (configs 1, 5, 9).
/// * 20 ms stereo: [`SilkEncoder::new_nb_stereo_20ms`],
///   `new_mb_stereo_20ms`, `new_wb_stereo_20ms` (configs 1/5/9 + stereo
///   bit). Runs a mid/side pair of [`SilkFrameEncoder`]s and emits the
///   RFC §4.2.7.1 prediction header.
/// * 10 ms mono + stereo: configs 0, 4, 8 (half the sub-frame count
///   per embedded SILK frame).
/// * 40 ms mono + stereo: configs 2, 6, 10 — packet carries 2 back-to-
///   back 20 ms SILK frame bodies per RFC §4.2.4.
/// * 60 ms mono + stereo: configs 3, 7, 11 — 3 back-to-back 20 ms SILK
///   frame bodies.
///
/// Round-trip SNR > 20 dB on speech-like input through the crate's own
/// SILK decoder for every 20 ms mode (see `encoder_roundtrip.rs`).
/// 10 ms frames lose a bit of first-frame SNR because the LPC history
/// starts cold; 40/60 ms frames match the 20 ms bar because each
/// embedded SILK frame carries its own LPC history.
///
/// Out of scope for this pass: voiced/LTP path, LBRR (redundancy),
/// Hybrid.
pub struct SilkEncoder {
    out_params: CodecParameters,
    mode: SilkMode,
    /// Per-frame SILK encoder for the mid (or mono) channel.
    silk_mid: crate::silk::encoder::SilkFrameEncoder,
    /// Side-channel encoder (stereo only).
    silk_side: Option<crate::silk::encoder::SilkFrameEncoder>,
    /// Pending internal-rate samples. For stereo, interleaved L/R.
    pending_internal: VecDeque<f32>,
    /// Expected input sample rate (internal or 48 kHz).
    input_sample_rate: u32,
    /// Output packet queue.
    output: VecDeque<Packet>,
    /// PTS counter in 48 kHz samples.
    pts_counter: i64,
}

impl SilkEncoder {
    // ---- 20 ms mono + stereo, all 3 bandwidths ---------------------

    /// Build a SILK NB mono 20 ms encoder. Input: 8 kHz or 48 kHz mono.
    pub fn new_nb_mono_20ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, false, 20))
    }

    /// Build a SILK MB mono 20 ms encoder. Input: 12 kHz or 48 kHz mono.
    pub fn new_mb_mono_20ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, false, 20))
    }

    /// Build a SILK WB mono 20 ms encoder. Input: 16 kHz or 48 kHz mono.
    pub fn new_wb_mono_20ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, false, 20))
    }

    /// Build a SILK NB stereo 20 ms encoder. Input: 8 kHz or 48 kHz
    /// stereo (interleaved L/R). Emits a mid/side-coded packet with the
    /// stereo prediction header from RFC §4.2.7.1.
    pub fn new_nb_stereo_20ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, true, 20))
    }

    /// MB stereo 20 ms (config 5 + stereo bit).
    pub fn new_mb_stereo_20ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, true, 20))
    }

    /// WB stereo 20 ms (config 9 + stereo bit).
    pub fn new_wb_stereo_20ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, true, 20))
    }

    // ---- 10 ms mono + stereo, all 3 bandwidths ---------------------

    /// NB mono 10 ms (config 0). Input: 8 kHz or 48 kHz mono.
    pub fn new_nb_mono_10ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, false, 10))
    }

    /// MB mono 10 ms (config 4). Input: 12 kHz or 48 kHz mono.
    pub fn new_mb_mono_10ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, false, 10))
    }

    /// WB mono 10 ms (config 8). Input: 16 kHz or 48 kHz mono.
    pub fn new_wb_mono_10ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, false, 10))
    }

    /// NB stereo 10 ms (config 0 + stereo bit).
    pub fn new_nb_stereo_10ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, true, 10))
    }

    /// MB stereo 10 ms (config 4 + stereo bit).
    pub fn new_mb_stereo_10ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, true, 10))
    }

    /// WB stereo 10 ms (config 8 + stereo bit).
    pub fn new_wb_stereo_10ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, true, 10))
    }

    // ---- 40 ms mono + stereo, all 3 bandwidths ---------------------
    //
    // 40 ms Opus frames contain 2 back-to-back 20 ms SILK frames per
    // RFC §4.2.4. The header VAD/LBRR flags span both SILK frames.

    /// NB mono 40 ms (config 2).
    pub fn new_nb_mono_40ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, false, 40))
    }

    /// MB mono 40 ms (config 6).
    pub fn new_mb_mono_40ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, false, 40))
    }

    /// WB mono 40 ms (config 10).
    pub fn new_wb_mono_40ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, false, 40))
    }

    /// NB stereo 40 ms.
    pub fn new_nb_stereo_40ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, true, 40))
    }

    /// MB stereo 40 ms.
    pub fn new_mb_stereo_40ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, true, 40))
    }

    /// WB stereo 40 ms.
    pub fn new_wb_stereo_40ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, true, 40))
    }

    // ---- 60 ms mono + stereo, all 3 bandwidths ---------------------

    /// NB mono 60 ms (config 3).
    pub fn new_nb_mono_60ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, false, 60))
    }

    /// MB mono 60 ms (config 7).
    pub fn new_mb_mono_60ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, false, 60))
    }

    /// WB mono 60 ms (config 11).
    pub fn new_wb_mono_60ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, false, 60))
    }

    /// NB stereo 60 ms.
    pub fn new_nb_stereo_60ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Nb, true, 60))
    }

    /// MB stereo 60 ms.
    pub fn new_mb_stereo_60ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Mb, true, 60))
    }

    /// WB stereo 60 ms.
    pub fn new_wb_stereo_60ms(params: &CodecParameters) -> Result<Self> {
        Self::new_mode(params, SilkMode::new(SilkBw::Wb, true, 60))
    }

    fn build_frame_encoder(mode: SilkMode) -> crate::silk::encoder::SilkFrameEncoder {
        let bw_params = match mode.bw {
            SilkBw::Nb => crate::silk::encoder::BandwidthParams::nb(),
            SilkBw::Mb => crate::silk::encoder::BandwidthParams::mb(),
            SilkBw::Wb => crate::silk::encoder::BandwidthParams::wb(),
        };
        let subframes = mode.subframes_per_silk_frame();
        crate::silk::encoder::SilkFrameEncoder::new_with_subframes(bw_params, subframes)
    }

    fn new_mode(params: &CodecParameters, mode: SilkMode) -> Result<Self> {
        let channels = params.channels.unwrap_or(mode.input_channels());
        if channels != mode.input_channels() {
            return Err(Error::unsupported(format!(
                "SILK encoder: {:?} expects {}-channel input, got {channels} channels",
                mode,
                mode.input_channels()
            )));
        }
        let sr = params.sample_rate.unwrap_or(SAMPLE_RATE);
        let internal = mode.internal_rate();
        if sr != internal && sr != SAMPLE_RATE {
            return Err(Error::unsupported(format!(
                "SILK encoder: {mode:?} expects {internal} Hz or 48 kHz input, got {sr} Hz"
            )));
        }
        let silk_mid = Self::build_frame_encoder(mode);
        let silk_side = if mode.is_stereo() {
            Some(Self::build_frame_encoder(mode))
        } else {
            None
        };

        let mut out_params = params.clone();
        out_params.sample_rate = Some(SAMPLE_RATE);
        out_params.channels = Some(mode.input_channels());
        let per_frame_items =
            mode.frame_samples_internal() * (if mode.is_stereo() { 2 } else { 1 });

        Ok(Self {
            out_params,
            mode,
            silk_mid,
            silk_side,
            pending_internal: VecDeque::with_capacity(per_frame_items * 2),
            input_sample_rate: sr,
            output: VecDeque::new(),
            pts_counter: 0,
        })
    }

    fn drain_frames(&mut self) -> Result<()> {
        let samples_per_frame = self.mode.frame_samples_internal();
        let per_frame_items = samples_per_frame * (if self.mode.is_stereo() { 2 } else { 1 });
        while self.pending_internal.len() >= per_frame_items {
            if self.mode.is_stereo() {
                let mut left = Vec::with_capacity(samples_per_frame);
                let mut right = Vec::with_capacity(samples_per_frame);
                for _ in 0..samples_per_frame {
                    left.push(self.pending_internal.pop_front().unwrap_or(0.0));
                    right.push(self.pending_internal.pop_front().unwrap_or(0.0));
                }
                let pkt = self.encode_one_stereo_frame(&left, &right)?;
                self.output.push_back(pkt);
            } else {
                let mut frame = Vec::with_capacity(samples_per_frame);
                for _ in 0..samples_per_frame {
                    frame.push(self.pending_internal.pop_front().unwrap_or(0.0));
                }
                let pkt = self.encode_one_mono_frame(&frame)?;
                self.output.push_back(pkt);
            }
        }
        Ok(())
    }

    /// Emit the shared VAD + LBRR header at the top of an Opus frame
    /// per RFC §4.2.3 / §4.2.4. Layout (from libopus `silk_Decode`):
    ///
    /// ```text
    ///   for each internal channel n:
    ///     for each packet frame i:
    ///       vad_flags[n][i] = ec_dec_bit_logp(1)
    ///     lbrr_flag[n]      = ec_dec_bit_logp(1)
    /// ```
    ///
    /// We always emit `vad = 1` (active frame) and `lbrr = 0` (no
    /// redundancy). For stereo the mid channel's bits come first,
    /// then the side channel's. For multi-frame packets (40 / 60 ms)
    /// the VAD bits for all internal frames are emitted before the
    /// LBRR flag (one LBRR flag per channel, regardless of frame
    /// count).
    fn emit_shared_header(&self, enc: &mut oxideav_celt::range_encoder::RangeEncoder) {
        let n_silk_frames = self.mode.silk_frames_per_packet();
        let n_channels = if self.mode.is_stereo() { 2 } else { 1 };
        for _ch in 0..n_channels {
            for _i in 0..n_silk_frames {
                enc.encode_bit_logp(true, 1); // VAD = 1
            }
            enc.encode_bit_logp(false, 1); // LBRR = 0
        }
    }

    fn encode_one_mono_frame(&mut self, pcm_internal: &[f32]) -> Result<Packet> {
        debug_assert_eq!(pcm_internal.len(), self.mode.frame_samples_internal());
        let mut re = oxideav_celt::range_encoder::RangeEncoder::new(self.mode.buffer_bytes());

        self.emit_shared_header(&mut re);

        // Emit 1 / 2 / 3 back-to-back SILK frame bodies. For 10 and
        // 20 ms Opus frames this is just one body.
        let per_silk = self.mode.samples_per_silk_frame();
        let n = self.mode.silk_frames_per_packet();
        for i in 0..n {
            let start = i * per_silk;
            self.silk_mid
                .encode_frame_body(&pcm_internal[start..start + per_silk], &mut re)?;
        }

        let body = re
            .done()
            .map_err(|e| Error::other(format!("SILK encoder: {e}")))?;
        let body = strip_trailing_zeros(body);
        self.finish_packet(body)
    }

    fn encode_one_stereo_frame(&mut self, left: &[f32], right: &[f32]) -> Result<Packet> {
        debug_assert_eq!(left.len(), right.len());
        debug_assert_eq!(left.len(), self.mode.frame_samples_internal());
        let mut re = oxideav_celt::range_encoder::RangeEncoder::new(self.mode.buffer_bytes());

        self.emit_shared_header(&mut re);

        // For stereo, the mid/side split is done *per 20 ms SILK sub-
        // frame* because the decoder emits one stereo prediction header
        // + one mid body + one side body per sub-frame (RFC §4.2.4).
        let per_silk = self.mode.samples_per_silk_frame();
        let n = self.mode.silk_frames_per_packet();
        for i in 0..n {
            let start = i * per_silk;
            let lc = &left[start..start + per_silk];
            let rc = &right[start..start + per_silk];

            // Stereo prediction weights (0, 0) for the MVP — see the
            // comment in the 20 ms path.
            let (mid, side) = crate::silk::encoder::stereo_mid_side(lc, rc);
            crate::silk::encoder::encode_stereo_pred_weights(&mut re, [0, 0]);

            // Mid then side body. The decoder reads the mid-only flag
            // only when the side VAD is 0; we emit VAD=1 for the side
            // channel (see `emit_shared_header`), so no extra bit.
            self.silk_mid.encode_frame_body(&mid, &mut re)?;
            let side_enc = self
                .silk_side
                .as_mut()
                .ok_or_else(|| Error::other("SILK stereo encoder: missing side state"))?;
            side_enc.encode_frame_body(&side, &mut re)?;
        }

        let body = re
            .done()
            .map_err(|e| Error::other(format!("SILK encoder: {e}")))?;
        let body = strip_trailing_zeros(body);
        self.finish_packet(body)
    }

    fn finish_packet(&mut self, body: Vec<u8>) -> Result<Packet> {
        let toc = self.mode.toc_byte();
        let mut data = Vec::with_capacity(1 + body.len());
        data.push(toc);
        data.extend_from_slice(&body);

        let tb = TimeBase::new(1, SAMPLE_RATE as i64);
        let pts = self.pts_counter;
        let samples_48k = self.mode.frame_samples_48k() as i64;
        self.pts_counter += samples_48k;
        Ok(Packet::new(0, tb, data)
            .with_pts(pts)
            .with_duration(samples_48k))
    }
}

/// Trim trailing zero bytes from a range-encoded buffer. The last
/// non-zero byte of the main bitstream + the optional back-buffer
/// bits fully determine the decoded symbols; any trailing zeros are
/// padding the CELT range encoder writes to its allocated storage.
fn strip_trailing_zeros(mut v: Vec<u8>) -> Vec<u8> {
    while v.len() > 1 && v.last() == Some(&0) {
        v.pop();
    }
    v
}

impl Encoder for SilkEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.out_params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let audio = match frame {
            Frame::Audio(a) => a,
            _ => {
                return Err(Error::invalid(
                    "SILK encoder: expected audio frame, got video",
                ))
            }
        };
        if audio.channels != self.mode.input_channels() {
            return Err(Error::invalid(format!(
                "SILK encoder: frame channels ({}) differ from configured input channels ({})",
                audio.channels,
                self.mode.input_channels()
            )));
        }
        if audio.sample_rate != self.input_sample_rate {
            return Err(Error::unsupported(format!(
                "SILK encoder: input sample rate ({}) differs from configured rate ({}); reconfigure or resample first",
                audio.sample_rate, self.input_sample_rate
            )));
        }

        // Extract f32 samples. Mono modes feed `extract_mono_f32`; the
        // stereo mode keeps per-channel planes interleaved so the
        // caller-side mid/side split stays bit-exact.
        let internal_items_per_sample = if self.mode.is_stereo() { 2 } else { 1 };
        let internal_samples: Vec<f32> = if self.mode.is_stereo() {
            let stereo = extract_stereo_f32(audio)?;
            if audio.sample_rate == self.mode.internal_rate() {
                stereo
            } else {
                downsample_box_interleaved(&stereo, self.mode.downsample_ratio(), 2)
            }
        } else {
            let mono = extract_mono_f32(audio)?;
            if audio.sample_rate == self.mode.internal_rate() {
                mono
            } else {
                downsample_box(&mono, self.mode.downsample_ratio())
            }
        };

        debug_assert_eq!(
            internal_items_per_sample * (internal_samples.len() / internal_items_per_sample),
            internal_samples.len()
        );
        self.pending_internal.extend(&internal_samples);
        self.drain_frames()
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.output.pop_front() {
            Ok(p)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        if !self.pending_internal.is_empty() {
            let per_frame_items =
                self.mode.frame_samples_internal() * (if self.mode.is_stereo() { 2 } else { 1 });
            while self.pending_internal.len() % per_frame_items != 0 {
                self.pending_internal.push_back(0.0);
            }
            self.drain_frames()?;
        }
        Ok(())
    }
}

/// Extract an interleaved L/R f32 buffer from an `AudioFrame`. Supports
/// the same sample formats as [`extract_mono_f32`] but returns
/// `samples * 2` floats, preserving per-channel detail.
fn extract_stereo_f32(audio: &AudioFrame) -> Result<Vec<f32>> {
    let n = audio.samples as usize;
    let ch = audio.channels as usize;
    if ch != 2 {
        return Err(Error::invalid(format!(
            "SILK stereo encoder: expected 2-channel input, got {ch}"
        )));
    }
    let mut out = vec![0f32; n * 2];
    match audio.format {
        SampleFormat::S16 => {
            let bytes = &audio.data[0];
            let needed = n * 2 * 2;
            if bytes.len() < needed {
                return Err(Error::invalid(
                    "SILK stereo encoder: S16 input shorter than declared sample count",
                ));
            }
            for i in 0..n {
                for c in 0..2 {
                    let off = (i * 2 + c) * 2;
                    let s = i16::from_le_bytes([bytes[off], bytes[off + 1]]);
                    out[i * 2 + c] = s as f32 / 32768.0;
                }
            }
        }
        SampleFormat::S16P => {
            if audio.data.len() < 2 {
                return Err(Error::invalid("SILK stereo encoder: S16P missing planes"));
            }
            for i in 0..n {
                for c in 0..2 {
                    let plane = &audio.data[c];
                    if plane.len() < n * 2 {
                        return Err(Error::invalid(
                            "SILK stereo encoder: S16P plane shorter than declared sample count",
                        ));
                    }
                    let off = i * 2;
                    let s = i16::from_le_bytes([plane[off], plane[off + 1]]);
                    out[i * 2 + c] = s as f32 / 32768.0;
                }
            }
        }
        SampleFormat::F32 => {
            let bytes = &audio.data[0];
            let needed = n * 2 * 4;
            if bytes.len() < needed {
                return Err(Error::invalid(
                    "SILK stereo encoder: F32 input shorter than declared sample count",
                ));
            }
            for i in 0..n {
                for c in 0..2 {
                    let off = (i * 2 + c) * 4;
                    out[i * 2 + c] = f32::from_le_bytes([
                        bytes[off],
                        bytes[off + 1],
                        bytes[off + 2],
                        bytes[off + 3],
                    ]);
                }
            }
        }
        SampleFormat::F32P => {
            if audio.data.len() < 2 {
                return Err(Error::invalid("SILK stereo encoder: F32P missing planes"));
            }
            for i in 0..n {
                for c in 0..2 {
                    let plane = &audio.data[c];
                    if plane.len() < n * 4 {
                        return Err(Error::invalid(
                            "SILK stereo encoder: F32P plane shorter than declared sample count",
                        ));
                    }
                    let off = i * 4;
                    out[i * 2 + c] = f32::from_le_bytes([
                        plane[off],
                        plane[off + 1],
                        plane[off + 2],
                        plane[off + 3],
                    ]);
                }
            }
        }
        other => {
            return Err(Error::unsupported(format!(
                "SILK stereo encoder: sample format {other:?} not supported (use S16 / S16P / F32 / F32P)"
            )));
        }
    }
    Ok(out)
}

/// Interleaved (stride-aware) box-average downsampler. `stride` is the
/// number of interleaved channels (1 = mono, 2 = stereo). Identical
/// ratio averaging as [`downsample_box`], but keeps per-channel samples
/// aligned through the decimation.
fn downsample_box_interleaved(input: &[f32], ratio: usize, stride: usize) -> Vec<f32> {
    if ratio <= 1 {
        return input.to_vec();
    }
    debug_assert_eq!(input.len() % stride, 0);
    let n_out_frames = (input.len() / stride) / ratio;
    let mut out = vec![0f32; n_out_frames * stride];
    for i in 0..n_out_frames {
        for c in 0..stride {
            let mut sum = 0f32;
            for k in 0..ratio {
                sum += input[(i * ratio + k) * stride + c];
            }
            out[i * stride + c] = sum / ratio as f32;
        }
    }
    out
}

/// Average every `ratio` consecutive input samples into one output
/// sample. Cheap & cheerful anti-alias for speech-band content. The
/// output length is `input.len() / ratio` (any trailing partial group
/// is dropped — callers that need strict sample accounting should
/// pass multiples of `ratio`).
fn downsample_box(input: &[f32], ratio: usize) -> Vec<f32> {
    if ratio <= 1 {
        return input.to_vec();
    }
    let n_out = input.len() / ratio;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let mut sum = 0f32;
        for k in 0..ratio {
            sum += input[i * ratio + k];
        }
        out.push(sum / ratio as f32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_byte_mono() {
        let b = build_toc_byte(false);
        assert_eq!(b >> 3, 31, "config should be 31");
        assert_eq!((b >> 2) & 1, 0, "stereo bit should be 0");
        assert_eq!(b & 0x3, 0, "framing code should be 0");
    }

    #[test]
    fn toc_byte_stereo() {
        let b = build_toc_byte(true);
        assert_eq!(b >> 3, 31, "config should be 31");
        assert_eq!((b >> 2) & 1, 1, "stereo bit should be 1");
        assert_eq!(b & 0x3, 0, "framing code should be 0");
    }

    #[test]
    fn rejects_non_48k() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(1);
        p.sample_rate = Some(44_100);
        match OpusEncoder::new(&p) {
            Err(Error::Unsupported(_)) => {}
            Err(e) => panic!("expected Unsupported, got {e:?}"),
            Ok(_) => panic!("expected Unsupported, got Ok"),
        }
    }

    #[test]
    fn rejects_more_than_stereo() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(6);
        p.sample_rate = Some(SAMPLE_RATE);
        match OpusEncoder::new(&p) {
            Err(Error::Unsupported(_)) => {}
            Err(e) => panic!("expected Unsupported, got {e:?}"),
            Ok(_) => panic!("expected Unsupported, got Ok"),
        }
    }

    #[test]
    fn new_celt_only_fb_accepts_48k_mono() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(1);
        p.sample_rate = Some(SAMPLE_RATE);
        assert!(OpusEncoder::new_celt_only_full_band(&p).is_ok());
    }

    #[test]
    fn new_celt_only_fb_rejects_non_48k() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(1);
        p.sample_rate = Some(16_000);
        match OpusEncoder::new_celt_only_full_band(&p) {
            Err(Error::Unsupported(_)) => {}
            Err(e) => panic!("expected Unsupported, got {e:?}"),
            Ok(_) => panic!("expected Unsupported, got Ok"),
        }
    }

    #[test]
    fn mono_encoder_produces_toc_byte() {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.channels = Some(1);
        p.sample_rate = Some(SAMPLE_RATE);
        let mut enc = OpusEncoder::new(&p).unwrap();
        // Feed one frame of silence.
        let bytes = vec![0u8; OPUS_FRAME_SAMPLES * 2];
        let frame = Frame::Audio(AudioFrame {
            format: SampleFormat::S16,
            channels: 1,
            sample_rate: SAMPLE_RATE,
            samples: OPUS_FRAME_SAMPLES as u32,
            pts: None,
            time_base: TimeBase::new(1, SAMPLE_RATE as i64),
            data: vec![bytes],
        });
        enc.send_frame(&frame).unwrap();
        let pkt = enc.receive_packet().unwrap();
        assert!(!pkt.data.is_empty(), "packet must contain TOC + bitstream");
        let toc = pkt.data[0];
        assert_eq!(toc >> 3, 31, "config should be 31");
        assert_eq!((toc >> 2) & 1, 0, "mono → stereo bit = 0");
        assert_eq!(toc & 0x3, 0, "single-frame packet → code 0");
    }
}
