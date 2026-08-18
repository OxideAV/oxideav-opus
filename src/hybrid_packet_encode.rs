//! Hybrid (SILK + CELT) Opus **packet** encoder — the §4.5 mode's
//! two-layer frame: a WB-internal §4.2 SILK layer for 0–8 kHz and a
//! §4.3 CELT layer for bands 17.. (8 kHz up), sharing one range coder,
//! with the §4.5.1 redundancy flag signalled off (RFC 6716 §2.1.2 /
//! §4.5 / §5.3).
//!
//! ## Layer time alignment
//!
//! The decoder sums the two layers sample for sample (§4.4), so the
//! encoder must present them on one timeline. The CELT analysis path
//! delays by the 120-sample §4.3.7 MDCT overlap; the SILK path delays
//! by this encoder's 48→16 kHz decimator (a linear-phase FIR whose
//! group delay is chosen as 82 samples at 48 kHz) plus the decoder's
//! §4.2.9 WB→48 kHz resampler (35 samples) and the §4.2.8 one-sample
//! internal-rate delay (3 samples): 82 + 35 + 3 = 120. Both layers
//! therefore land 120 samples late together, and the encoder feeds the
//! same input frame to both.
//!
//! ## Provenance
//!
//! RFC 6716 §2.1.2 / §4.5 / §5 + the normative Appendix A reference
//! listing (staged `docs/audio/opus/rfc6716-opus.txt`, hash-verified
//! per §A.1). No external library source was consulted.

use crate::celt_frame_encode::{encode_celt_frame, CeltEncoderState};
use crate::celt_redundancy::{
    HYBRID_REDUNDANCY_MIN_REMAINING_BITS, REDUNDANCY_FLAG_ICDF, REDUNDANCY_FLAG_ICDF_FTB,
};
use crate::range_encoder::RangeEncoder;
use crate::silk_decode::{encode_silk_frame, SilkFrameConfig, StereoHeaderContext};
use crate::silk_encoder::{
    interval_rms, lbrr_interval_is_onset, lbrr_ratio_for_loss, AnalyzedFrame, ChannelAnalyzer,
    DtxState, ACTIVITY_RMS, LBRR_ONSET_ONLY_MAX_LOSS_PERC, MID_ONLY_SIDE_RMS,
};
use crate::silk_excitation::SilkFrameSize;
use crate::silk_frame::{StereoPredictionWeights, StereoWeightSymbols};
use crate::silk_header::{PerFrameLbrr, SilkChannelHeader, SilkHeaderBits};
use crate::silk_stereo::{
    estimate_stereo_weights, stereo_lr_to_ms, StereoDownmixState, StereoWeightsQ13,
};
use crate::toc::{Bandwidth, FrameCountCode, Mode, OpusTocByte};
use crate::Error;

/// §3.2 maximum Opus frame payload.
const MAX_FRAME_BYTES: usize = 1275;

/// Minimum CELT-layer tail (bytes past the coded SILK layer) that
/// [`HybridEncoderMono::encode_packet_elected`] guarantees, so the
/// §4.3 layer always has a working budget for its gated symbols.
pub const HYBRID_MIN_CELT_TAIL_BYTES: usize = 12;

/// Decimator FIR half-width at 48 kHz: 165 taps → 82-sample group
/// delay (see the module docs' alignment budget).
const DECIM_TAPS: usize = 165;

/// Linear-phase windowed-sinc 48 kHz → 16 kHz decimator with carried
/// history (streaming; group delay (165-1)/2 = 82 input samples).
#[derive(Debug, Clone)]
struct Decimator48To16 {
    taps: Vec<f64>,
    hist: Vec<f64>,
}

impl Decimator48To16 {
    fn new() -> Self {
        // Kaiser-ish Hann-windowed sinc, cutoff 0.9 * 8 kHz.
        let fc = 0.9 * 8000.0 / 48000.0; // cycles per input sample
        let m = (DECIM_TAPS - 1) as f64;
        let mut taps = vec![0.0f64; DECIM_TAPS];
        let mut sum = 0.0f64;
        for (i, t) in taps.iter_mut().enumerate() {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos();
            *t = sinc * w;
            sum += *t;
        }
        for t in taps.iter_mut() {
            *t /= sum;
        }
        Self {
            taps,
            hist: vec![0.0; DECIM_TAPS - 1],
        }
    }

    fn reset(&mut self) {
        self.hist.fill(0.0);
    }

    /// Consume `3 * n` 48 kHz samples, produce `n` 16 kHz samples
    /// (f32 in [-1, 1] for the SILK analyzer).
    fn process(&mut self, input48: &[f64]) -> Vec<f32> {
        let n_out = input48.len() / 3;
        let mut ext = Vec::with_capacity(self.hist.len() + input48.len());
        ext.extend_from_slice(&self.hist);
        ext.extend_from_slice(input48);
        let mut out = Vec::with_capacity(n_out);
        for k in 0..n_out {
            // Output sample k corresponds to input index 3k (plus the
            // FIR delay carried by the history offset).
            let base = 3 * k;
            let mut acc = 0.0f64;
            for (j, &t) in self.taps.iter().enumerate() {
                acc += t * ext[base + j];
            }
            out.push((acc / 32768.0) as f32);
        }
        let keep = self.hist.len();
        self.hist.copy_from_slice(&ext[ext.len() - keep..]);
        out
    }
}

/// A mono Hybrid packet encoder (configs 12–15: SWB/FB × 10/20 ms).
#[derive(Debug, Clone)]
pub struct HybridEncoderMono {
    analyzer: ChannelAnalyzer,
    celt: CeltEncoderState,
    decim: Decimator48To16,
    bandwidth: Bandwidth,
    frame_tenths_ms: u16,
    silk_frame_size: SilkFrameSize,
    end_band: usize,
    lm: i32,
    n: usize,
    /// §4.2.5 LBRR (in-band FEC) emission switch.
    fec: bool,
    /// The previous packet's internal-rate PCM + pre-packet analyzer
    /// snapshot (see [`crate::silk_encoder::SilkEncoderMono`]).
    pending_fec: Option<PendingFecHybridMono>,
    /// §2.1.7 expected packet-loss percentage (0 = knob unset).
    loss_perc: u8,
    /// Previous LBRR-processed interval's RMS (§2.1.7 onset baseline).
    lbrr_prev_rms: f64,
    /// §2.1.9 DTX driver.
    dtx: DtxState,
}

/// The previous Hybrid packet's material a FEC-enabled encoder keeps
/// so the NEXT packet can carry its §4.2.5 LBRR re-encode of the WB
/// SILK layer.
#[derive(Debug, Clone)]
struct PendingFecHybridMono {
    pcm16: Vec<f32>,
    analyzer: ChannelAnalyzer,
}

impl HybridEncoderMono {
    /// New mono Hybrid encoder. `bandwidth` is SWB or FB;
    /// `frame_tenths_ms` is 100 or 200 (10 / 20 ms).
    pub fn new(bandwidth: Bandwidth, frame_tenths_ms: u16) -> Result<Self, Error> {
        let end_band = match bandwidth {
            Bandwidth::Swb => 19,
            Bandwidth::Fb => 21,
            _ => return Err(Error::MalformedPacket),
        };
        let (lm, silk_frame_size) = match frame_tenths_ms {
            100 => (2i32, SilkFrameSize::TenMs),
            200 => (3, SilkFrameSize::TwentyMs),
            _ => return Err(Error::MalformedPacket),
        };
        let _ = OpusTocByte::compose_byte(
            Mode::Hybrid,
            bandwidth,
            frame_tenths_ms,
            false,
            FrameCountCode::One,
        )?;
        let n = 120usize << lm;
        Ok(Self {
            // The Hybrid SILK layer always runs WB internal (§2.1.2).
            analyzer: ChannelAnalyzer::new(Bandwidth::Wb)?,
            celt: CeltEncoderState::new(1, n),
            decim: Decimator48To16::new(),
            bandwidth,
            frame_tenths_ms,
            silk_frame_size,
            end_band,
            lm,
            n,
            fec: false,
            pending_fec: None,
            loss_perc: 0,
            lbrr_prev_rms: 0.0,
            dtx: DtxState::default(),
        })
    }

    /// 48 kHz samples per packet.
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        self.n
    }

    /// Enable / disable §2.1.9 discontinuous transmission (see
    /// [`crate::silk_encoder::SilkEncoderMono::set_dtx`]): an
    /// inactive packet is (after the transmitted hangover) replaced
    /// by the 1-byte Hybrid TOC marker, with one coded refresh per
    /// 400 ms of suppression. The refresh (and any resume) codes its
    /// CELT energies INTRA and its SILK frame without LTP, so its
    /// reconstruction never depends on what a decoder's own
    /// non-normative concealment left behind.
    pub fn set_dtx(&mut self, enabled: bool) {
        self.dtx.enabled = enabled;
        self.dtx.reset();
    }

    /// Enable / disable §4.2.5 LBRR (in-band FEC) emission on the WB
    /// SILK layer: when on, every packet after the first carries a
    /// reduced-rate re-encode of the previous packet's SILK band
    /// (recoverable with
    /// [`crate::decoder::OpusDecoder::decode_packet_fec`] — the
    /// recovery is the 0–8 kHz LP layer, §2.1.7's re-encoded speech
    /// information). Disabling drops any pending redundancy.
    pub fn set_fec(&mut self, enabled: bool) {
        self.fec = enabled;
        if !enabled {
            self.pending_fec = None;
        }
    }

    /// §2.1.7 loss-optimised LBRR (see
    /// [`crate::silk_encoder::SilkEncoderMono::set_packet_loss_perc`]):
    /// the onset gate and the redundancy rate-ratio ramp on the
    /// Hybrid SILK layer's LBRR.
    pub fn set_packet_loss_perc(&mut self, loss_perc: u8) {
        self.loss_perc = loss_perc.min(100);
        self.analyzer
            .set_lbrr_rate_ratio(lbrr_ratio_for_loss(self.loss_perc));
    }

    /// Arm the §5.2.3.8 delayed-decision noise shaping quantiser on
    /// the Hybrid SILK layer (see
    /// [`ChannelAnalyzer::set_nsq_delayed_decision`]; the CELT bands
    /// 17.. are unaffected).
    pub fn set_nsq_delayed_decision(&mut self, n_states: usize) {
        self.analyzer.set_nsq_delayed_decision(n_states);
    }

    /// Complexity ladder (0..=10; see
    /// [`crate::silk_encoder::SilkEncoderMono::set_complexity`]) —
    /// maps to the SILK layer's delayed-decision state count. A
    /// Hybrid frame never runs the §5.3.1 pre-filter (the decoder's
    /// `start == 0` gate), so the CELT rungs do not apply.
    pub fn set_complexity(&mut self, complexity: u8) {
        self.set_nsq_delayed_decision(crate::silk_encoder::silk_states_for_complexity(complexity));
    }

    /// Reset all carried state (§4.5.2).
    pub fn reset(&mut self) {
        self.analyzer.reset();
        self.celt.reset();
        self.decim.reset();
        self.pending_fec = None;
        self.lbrr_prev_rms = 0.0;
        self.dtx.reset();
    }

    /// Encode one packet: `pcm` holds `frame_samples()` mono 48 kHz
    /// samples; the packet is `1 + payload_bytes` bytes (code 0).
    pub fn encode_packet(&mut self, pcm: &[i16], payload_bytes: usize) -> Result<Vec<u8>, Error> {
        if pcm.len() != self.n {
            return Err(Error::MalformedPacket);
        }
        if !(2..=MAX_FRAME_BYTES).contains(&payload_bytes) {
            return Err(Error::MalformedPacket);
        }
        let pcm16 = self.decimate(pcm);
        if let Some(marker) = self.dtx_gate(&pcm16)? {
            return Ok(marker);
        }
        let (toc, re) = self.encode_silk_layer(&pcm16)?;
        // The SILK layer has no rate control (its quantizer targets a
        // fixed quality); when it alone exceeds the payload budget the
        // packet cannot be emitted. The analysis state has already
        // advanced (as with the SILK CBR helper), so pick payloads
        // with headroom for the configured content.
        if re.tell() > payload_bytes as u32 * 8 {
            return Err(Error::MalformedPacket);
        }
        self.finish_with_celt(pcm, toc, re, payload_bytes)
    }

    /// Encode one packet with a VBR-elected payload size, raising the
    /// election to the SILK-layer floor when the elected size cannot
    /// carry the coded SILK frame plus a working CELT tail.
    ///
    /// The §4.2 SILK layer is coded first at its natural (quality-
    /// driven) size; `elected_payload_bytes` is then honoured when it
    /// leaves room, and otherwise raised to
    /// `ceil(silk_bits / 8) + HYBRID_MIN_CELT_TAIL_BYTES` (capped at
    /// the §3.2.1 1275-byte frame limit — a SILK frame that busts even
    /// that is rejected). Returns the finished packet; its length is
    /// `1 + actual_payload` where `actual_payload >= 2`.
    pub fn encode_packet_elected(
        &mut self,
        pcm: &[i16],
        elected_payload_bytes: usize,
    ) -> Result<Vec<u8>, Error> {
        if pcm.len() != self.n {
            return Err(Error::MalformedPacket);
        }
        let pcm16 = self.decimate(pcm);
        if let Some(marker) = self.dtx_gate(&pcm16)? {
            return Ok(marker);
        }
        let (toc, re) = self.encode_silk_layer(&pcm16)?;
        let silk_bytes = (re.tell() as usize).div_ceil(8);
        let floor = silk_bytes + HYBRID_MIN_CELT_TAIL_BYTES;
        if floor > MAX_FRAME_BYTES {
            return Err(Error::MalformedPacket);
        }
        let payload_bytes = elected_payload_bytes.clamp(floor, MAX_FRAME_BYTES);
        self.finish_with_celt(pcm, toc, re, payload_bytes)
    }

    /// 48 kHz → WB internal rate (advances the decimator history —
    /// input-side state, rolled for suppressed packets too).
    fn decimate(&mut self, pcm: &[i16]) -> Vec<f32> {
        let pcm48: Vec<f64> = pcm.iter().map(|&v| f64::from(v)).collect();
        self.decim.process(&pcm48)
    }

    /// §2.1.9 DTX gate: run the driver on this packet's activity;
    /// when it suppresses, roll the analysis lookback, freeze every
    /// decoder-authoritative mirror (SILK synthesis state and the
    /// CELT energy/synthesis carries alike — the decoder decodes
    /// nothing for a zero-length frame), drop the (inactive) pending
    /// redundancy, and return the 1-byte TOC-only marker. When the
    /// packet is the first CODED one after a run, arm the resume
    /// treatment: CELT energies intra + SILK frame without LTP.
    fn dtx_gate(&mut self, pcm16: &[f32]) -> Result<Option<Vec<u8>>, Error> {
        let all_inactive = interval_rms(pcm16) < ACTIVITY_RMS;
        let lbrr_pending_active = self
            .pending_fec
            .as_ref()
            .is_some_and(|p| interval_rms(&p.pcm16) >= ACTIVITY_RMS);
        if self.dtx.step(
            all_inactive,
            lbrr_pending_active,
            u32::from(self.frame_tenths_ms) / 10,
        ) {
            self.analyzer.skip_frame_untransmitted(pcm16);
            self.lbrr_prev_rms = interval_rms(pcm16);
            self.pending_fec = None;
            let toc = OpusTocByte::compose_byte(
                Mode::Hybrid,
                self.bandwidth,
                self.frame_tenths_ms,
                false,
                FrameCountCode::One,
            )?;
            return Ok(Some(vec![toc]));
        }
        if self.dtx.take_resume() {
            self.celt.force_intra = true;
            self.analyzer.set_force_unvoiced(true);
        }
        Ok(None)
    }

    /// Phase 1: the §4.2 SILK layer (decimate to the WB internal rate
    /// and encode one SILK frame — a Hybrid frame carries exactly one)
    /// on a fresh range coder. Independent of the final payload size.
    fn encode_silk_layer(&mut self, pcm16: &[f32]) -> Result<(u8, RangeEncoder), Error> {
        let toc = OpusTocByte::compose_byte(
            Mode::Hybrid,
            self.bandwidth,
            self.frame_tenths_ms,
            false,
            FrameCountCode::One,
        )?;
        let mut re = RangeEncoder::new();

        // §4.2.5 / §2.1.7: this packet's LBRR frame is a reduced-rate
        // re-encode of the PREVIOUS packet's SILK band, from the
        // pre-packet analyzer snapshot (exactly the SILK-only path,
        // one frame per Hybrid packet). The §2.1.7 importance gate
        // applies at low declared loss.
        let lbrr_frame: Option<AnalyzedFrame> = match self.pending_fec.take() {
            Some(pending) => {
                let mut la = pending.analyzer;
                la.rearm_for_lbrr();
                let rms = interval_rms(&pending.pcm16);
                let important = !(1..=LBRR_ONSET_ONLY_MAX_LOSS_PERC).contains(&self.loss_perc)
                    || lbrr_interval_is_onset(rms, self.lbrr_prev_rms);
                self.lbrr_prev_rms = rms;
                let f = la.analyze_frame_sized(&pending.pcm16, true, self.silk_frame_size)?;
                (f.header.frame_type >= 2 && important).then_some(f)
            }
            None => None,
        };
        if self.fec {
            self.pending_fec = Some(PendingFecHybridMono {
                pcm16: pcm16.to_vec(),
                analyzer: self.analyzer.clone(),
            });
        }

        let analyzed = self
            .analyzer
            .analyze_frame_sized(pcm16, true, self.silk_frame_size)?;
        // Clear a one-shot §2.1.9 resume treatment (see `dtx_gate`).
        self.analyzer.set_force_unvoiced(false);
        let vad = analyzed.header.frame_type >= 2;
        let header = SilkHeaderBits {
            num_silk_frames: 1,
            mid: SilkChannelHeader {
                vad_flags: u8::from(vad),
                lbrr_flag: lbrr_frame.is_some(),
            },
            side: None,
            // n = 1: the global LBRR flag IS the per-frame flag.
            per_frame_lbrr: PerFrameLbrr {
                mid: u8::from(lbrr_frame.is_some()),
                side: 0,
            },
        };
        header.encode(&mut re)?;
        // §4.2.5: the LBRR frame precedes the regular frame, coded
        // like a first-in-sequence active frame (independent gain,
        // absolute lag, §4.2.7.6.3 scaling present).
        if let Some(lf) = &lbrr_frame {
            let cfg = SilkFrameConfig {
                bandwidth: Bandwidth::Wb,
                frame_size: self.silk_frame_size,
                voice_active: true,
                first_subframe_independent: true,
                previous_log_gain: None,
                previous_primary_lag: None,
                ltp_scaling_present: true,
                lsf_interp_after_reset: true,
                previous_nlsf_q15: None,
                previous_nlsf_len: 0,
                stereo: None,
            };
            let _ = encode_silk_frame(&mut re, cfg, &lf.symbols())?;
        }
        let cfg = SilkFrameConfig {
            bandwidth: Bandwidth::Wb,
            frame_size: self.silk_frame_size,
            voice_active: vad,
            first_subframe_independent: true,
            previous_log_gain: None,
            previous_primary_lag: None,
            ltp_scaling_present: true,
            lsf_interp_after_reset: true,
            previous_nlsf_q15: None,
            previous_nlsf_len: 0,
            stereo: None,
        };
        let _decoded = encode_silk_frame(&mut re, cfg, &analyzed.symbols())?;
        Ok((toc, re))
    }

    /// Phase 2: the §4.5.1.1 redundancy flag (signalled off — only
    /// coded when the 37-bit window is open, mirroring the decoder's
    /// gate) and the §4.3 CELT layer (bands 17..) on the same coder,
    /// finalized to exactly `payload_bytes` bytes.
    fn finish_with_celt(
        &mut self,
        pcm: &[i16],
        toc: u8,
        mut re: RangeEncoder,
        payload_bytes: usize,
    ) -> Result<Vec<u8>, Error> {
        let total_bits = payload_bytes as u32 * 8;
        debug_assert!(re.tell() <= total_bits, "SILK layer over budget");
        if total_bits.saturating_sub(re.tell()) >= HYBRID_REDUNDANCY_MIN_REMAINING_BITS {
            re.enc_icdf(0, &REDUNDANCY_FLAG_ICDF, REDUNDANCY_FLAG_ICDF_FTB);
        }
        let _info = encode_celt_frame(
            &mut self.celt,
            &mut re,
            pcm,
            payload_bytes,
            crate::celt_band_layout::HYBRID_FIRST_CODED_BAND,
            self.end_band,
            self.lm,
        );
        debug_assert!(re.tell() <= total_bits, "hybrid CELT layer bust");
        let body = re
            .finish_fixed(payload_bytes)
            .ok_or(Error::MalformedPacket)?;
        let mut packet = Vec::with_capacity(1 + payload_bytes);
        packet.push(toc);
        packet.extend_from_slice(&body);
        Ok(packet)
    }
}

/// A stereo Hybrid packet encoder (configs 12–15 with the stereo
/// flag): the §4.2.2 WB SILK stereo layer (one mid frame with the
/// §4.2.7.1 weights + §4.2.7.2 mid-only escape, then the side frame)
/// and the stereo §4.3 CELT layer for bands 17.., sharing one range
/// coder on the mono encoder's delay-matched timeline (the §4.2.8
/// stereo unmix carries the same one-sample internal-rate delay as
/// the mono path).
#[derive(Debug, Clone)]
pub struct HybridEncoderStereo {
    mid: ChannelAnalyzer,
    side: ChannelAnalyzer,
    downmix: StereoDownmixState,
    /// Previous packet's trailing raw-mid sample (the §4.2.8 `p0`
    /// boundary term of the weight estimate).
    prev_mid: f32,
    celt: CeltEncoderState,
    decim_l: Decimator48To16,
    decim_r: Decimator48To16,
    bandwidth: Bandwidth,
    frame_tenths_ms: u16,
    silk_frame_size: SilkFrameSize,
    end_band: usize,
    lm: i32,
    n: usize,
    /// §4.2.5 LBRR (in-band FEC) emission switch.
    fec: bool,
    /// The previous packet's mix products + pre-packet analyzer
    /// snapshots (see [`crate::silk_encoder::SilkEncoderStereo`]).
    pending_fec: Option<PendingFecHybridStereo>,
    /// §2.1.7 expected packet-loss percentage (0 = knob unset).
    loss_perc: u8,
    /// Previous LBRR-processed interval's MID RMS (§2.1.7 onset
    /// baseline).
    lbrr_prev_rms: f64,
    /// §2.1.9 DTX driver.
    dtx: DtxState,
}

/// The previous stereo Hybrid packet's material a FEC-enabled encoder
/// keeps: the §4.2.8 downmix products and coded weights from the
/// regular pass (so the redundant copy codes the identical mix),
/// whether an ACTIVE side frame was coded, and the pre-packet
/// analyzer snapshots.
#[derive(Debug, Clone)]
struct PendingFecHybridStereo {
    mid_pcm: Vec<f32>,
    side_pcm: Vec<f32>,
    side_active: bool,
    weights: StereoWeightSymbols,
    mid_analyzer: ChannelAnalyzer,
    side_analyzer: ChannelAnalyzer,
}

impl HybridEncoderStereo {
    /// New stereo Hybrid encoder. `bandwidth` is SWB or FB;
    /// `frame_tenths_ms` is 100 or 200 (10 / 20 ms).
    pub fn new(bandwidth: Bandwidth, frame_tenths_ms: u16) -> Result<Self, Error> {
        let end_band = match bandwidth {
            Bandwidth::Swb => 19,
            Bandwidth::Fb => 21,
            _ => return Err(Error::MalformedPacket),
        };
        let (lm, silk_frame_size) = match frame_tenths_ms {
            100 => (2i32, SilkFrameSize::TenMs),
            200 => (3, SilkFrameSize::TwentyMs),
            _ => return Err(Error::MalformedPacket),
        };
        let _ = OpusTocByte::compose_byte(
            Mode::Hybrid,
            bandwidth,
            frame_tenths_ms,
            true,
            FrameCountCode::One,
        )?;
        let n = 120usize << lm;
        Ok(Self {
            mid: ChannelAnalyzer::new(Bandwidth::Wb)?,
            side: ChannelAnalyzer::new(Bandwidth::Wb)?,
            downmix: StereoDownmixState::new(),
            prev_mid: 0.0,
            celt: CeltEncoderState::new(2, n),
            decim_l: Decimator48To16::new(),
            decim_r: Decimator48To16::new(),
            bandwidth,
            frame_tenths_ms,
            silk_frame_size,
            end_band,
            lm,
            n,
            fec: false,
            pending_fec: None,
            loss_perc: 0,
            lbrr_prev_rms: 0.0,
            dtx: DtxState::default(),
        })
    }

    /// Enable / disable §2.1.9 discontinuous transmission (see
    /// [`HybridEncoderMono::set_dtx`]): the activity gate runs on the
    /// decimated mid/side pair; the marker carries the stereo Hybrid
    /// TOC.
    pub fn set_dtx(&mut self, enabled: bool) {
        self.dtx.enabled = enabled;
        self.dtx.reset();
    }

    /// Enable / disable §4.2.5 LBRR (in-band FEC) emission on the WB
    /// SILK stereo layer (see [`HybridEncoderMono::set_fec`]).
    pub fn set_fec(&mut self, enabled: bool) {
        self.fec = enabled;
        if !enabled {
            self.pending_fec = None;
        }
    }

    /// §2.1.7 loss-optimised LBRR (see
    /// [`HybridEncoderMono::set_packet_loss_perc`]): the onset gate
    /// is decided on the mid channel, the rate ratio applies to both
    /// channels' re-encoders.
    pub fn set_packet_loss_perc(&mut self, loss_perc: u8) {
        self.loss_perc = loss_perc.min(100);
        let ratio = lbrr_ratio_for_loss(self.loss_perc);
        self.mid.set_lbrr_rate_ratio(ratio);
        self.side.set_lbrr_rate_ratio(ratio);
    }

    /// 48 kHz samples per channel per packet (`pcm` is interleaved
    /// L/R, so `encode_packet` consumes `2 * frame_samples()`).
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        self.n
    }

    /// Reset all carried state (§4.5.2).
    /// Arm the §5.2.3.8 delayed-decision noise shaping quantiser on
    /// both channels of the Hybrid SILK layer (see
    /// [`ChannelAnalyzer::set_nsq_delayed_decision`]).
    pub fn set_nsq_delayed_decision(&mut self, n_states: usize) {
        self.mid.set_nsq_delayed_decision(n_states);
        self.side.set_nsq_delayed_decision(n_states);
    }

    /// Complexity ladder (0..=10; see
    /// [`HybridEncoderMono::set_complexity`]) on both channels.
    pub fn set_complexity(&mut self, complexity: u8) {
        self.set_nsq_delayed_decision(crate::silk_encoder::silk_states_for_complexity(complexity));
    }

    pub fn reset(&mut self) {
        self.mid.reset();
        self.side.reset();
        self.downmix.reset();
        self.prev_mid = 0.0;
        self.celt.reset();
        self.decim_l.reset();
        self.decim_r.reset();
        self.pending_fec = None;
        self.lbrr_prev_rms = 0.0;
        self.dtx.reset();
    }

    /// Encode one packet: `pcm` holds `2 * frame_samples()` interleaved
    /// L/R 48 kHz samples; the packet is `1 + payload_bytes` bytes
    /// (code 0). As with the mono encoder, a payload the SILK layer
    /// alone overflows is rejected (the analysis state has already
    /// advanced).
    pub fn encode_packet(&mut self, pcm: &[i16], payload_bytes: usize) -> Result<Vec<u8>, Error> {
        if pcm.len() != 2 * self.n {
            return Err(Error::MalformedPacket);
        }
        if !(2..=MAX_FRAME_BYTES).contains(&payload_bytes) {
            return Err(Error::MalformedPacket);
        }
        let (l16, r16) = self.decimate(pcm);
        if let Some(marker) = self.dtx_gate(&l16, &r16)? {
            return Ok(marker);
        }
        let (toc, re) = self.encode_silk_layer(&l16, &r16)?;
        if re.tell() > payload_bytes as u32 * 8 {
            return Err(Error::MalformedPacket);
        }
        self.finish_with_celt(pcm, toc, re, payload_bytes)
    }

    /// Encode one packet with a VBR-elected payload size (see
    /// [`HybridEncoderMono::encode_packet_elected`]).
    pub fn encode_packet_elected(
        &mut self,
        pcm: &[i16],
        elected_payload_bytes: usize,
    ) -> Result<Vec<u8>, Error> {
        if pcm.len() != 2 * self.n {
            return Err(Error::MalformedPacket);
        }
        let (l16, r16) = self.decimate(pcm);
        if let Some(marker) = self.dtx_gate(&l16, &r16)? {
            return Ok(marker);
        }
        let (toc, re) = self.encode_silk_layer(&l16, &r16)?;
        let silk_bytes = (re.tell() as usize).div_ceil(8);
        let floor = silk_bytes + HYBRID_MIN_CELT_TAIL_BYTES;
        if floor > MAX_FRAME_BYTES {
            return Err(Error::MalformedPacket);
        }
        let payload_bytes = elected_payload_bytes.clamp(floor, MAX_FRAME_BYTES);
        self.finish_with_celt(pcm, toc, re, payload_bytes)
    }

    /// 48 kHz interleaved L/R → WB internal-rate pair (advances the
    /// decimator histories — input-side state, rolled for suppressed
    /// packets too).
    fn decimate(&mut self, pcm: &[i16]) -> (Vec<f32>, Vec<f32>) {
        let mut l48 = Vec::with_capacity(self.n);
        let mut r48 = Vec::with_capacity(self.n);
        for pair in pcm.chunks_exact(2) {
            l48.push(f64::from(pair[0]));
            r48.push(f64::from(pair[1]));
        }
        (self.decim_l.process(&l48), self.decim_r.process(&r48))
    }

    /// §2.1.9 DTX gate on the decimated pair (see the mono
    /// [`HybridEncoderMono::encode_packet`] path): suppression rolls
    /// both analysis lookbacks with the raw mid/side signals, freezes
    /// every decoder-authoritative mirror (both SILK channels, the
    /// §4.2.7.1 unmix-interp history, the CELT carries), and returns
    /// the stereo Hybrid marker; a resume arms intra CELT energies +
    /// LTP-free SILK frames.
    fn dtx_gate(&mut self, l16: &[f32], r16: &[f32]) -> Result<Option<Vec<u8>>, Error> {
        let flen = l16.len();
        let mid_raw: Vec<f32> = l16.iter().zip(r16).map(|(&a, &b)| (a + b) / 2.0).collect();
        let side_raw: Vec<f32> = l16.iter().zip(r16).map(|(&a, &b)| (a - b) / 2.0).collect();
        let all_inactive =
            interval_rms(&mid_raw) < ACTIVITY_RMS && interval_rms(&side_raw) < ACTIVITY_RMS;
        let lbrr_pending_active = self
            .pending_fec
            .as_ref()
            .is_some_and(|p| p.side_active || interval_rms(&p.mid_pcm) >= ACTIVITY_RMS);
        if self.dtx.step(
            all_inactive,
            lbrr_pending_active,
            u32::from(self.frame_tenths_ms) / 10,
        ) {
            self.mid.skip_frame_untransmitted(&mid_raw);
            self.side.skip_frame_untransmitted(&side_raw);
            self.prev_mid = mid_raw[flen - 1];
            self.lbrr_prev_rms = interval_rms(&mid_raw);
            self.pending_fec = None;
            let toc = OpusTocByte::compose_byte(
                Mode::Hybrid,
                self.bandwidth,
                self.frame_tenths_ms,
                true,
                FrameCountCode::One,
            )?;
            return Ok(Some(vec![toc]));
        }
        if self.dtx.take_resume() {
            self.celt.force_intra = true;
            self.mid.set_force_unvoiced(true);
            self.side.set_force_unvoiced(true);
        }
        Ok(None)
    }

    /// Phase 1: the §4.2.2 stereo SILK layer on a fresh range coder —
    /// run the §5.2.2 stereo mixing front end on the decimated pair
    /// (weight estimate → quantize → exact §4.2.8 downmix with the
    /// QUANTIZED pair), analyse mid and side, and write header bits +
    /// mid frame (+ side frame unless mid-only), mirroring the
    /// decoder's single-interval stereo walk.
    fn encode_silk_layer(&mut self, l16: &[f32], r16: &[f32]) -> Result<(u8, RangeEncoder), Error> {
        let toc = OpusTocByte::compose_byte(
            Mode::Hybrid,
            self.bandwidth,
            self.frame_tenths_ms,
            true,
            FrameCountCode::One,
        )?;
        let mut re = RangeEncoder::new();
        let flen = l16.len();

        // §5.2.2 stereo mixing (one §4.2.2 interval per Hybrid frame).
        let mid_raw: Vec<f32> = l16.iter().zip(r16).map(|(&a, &b)| (a + b) / 2.0).collect();
        let side_raw: Vec<f32> = l16.iter().zip(r16).map(|(&a, &b)| (a - b) / 2.0).collect();
        let mid_next = mid_raw[flen - 1];
        let target = estimate_stereo_weights(&mid_raw, &side_raw, self.prev_mid, mid_next)?;
        let weight_symbols = StereoWeightSymbols::quantize(StereoPredictionWeights {
            w0_q13: target.w0_q13,
            w1_q13: target.w1_q13,
        });
        let decoded_w = weight_symbols.weights();
        let ms = stereo_lr_to_ms(
            Bandwidth::Wb,
            l16,
            r16,
            StereoWeightsQ13 {
                w0_q13: decoded_w.w0_q13,
                w1_q13: decoded_w.w1_q13,
            },
            None,
            &mut self.downmix,
        )?;
        self.prev_mid = mid_raw[flen - 1];

        // §4.2.7.2 mid-only decision.
        let side_energy: f64 = ms.side.iter().map(|&v| (v as f64) * (v as f64)).sum();
        let code_side = (side_energy / flen as f64).sqrt() > MID_ONLY_SIDE_RMS;

        // §4.2.5 / §2.1.7: this packet's LBRR frames re-encode the
        // PREVIOUS packet's mid/side products from the pre-packet
        // analyzer snapshots (exactly the SILK-only stereo path, one
        // §4.2.2 interval per Hybrid packet). Side first — the mid
        // LBRR frame's §4.2.7.2 flag depends on whether a side LBRR
        // frame actually rides.
        let (lbrr_mid, lbrr_side): (Option<AnalyzedFrame>, Option<AnalyzedFrame>) =
            match self.pending_fec.take() {
                Some(pending) => {
                    let mut lm = pending.mid_analyzer;
                    let mut ls = pending.side_analyzer;
                    lm.rearm_for_lbrr();
                    ls.rearm_for_lbrr();
                    let rms = interval_rms(&pending.mid_pcm);
                    let important = !(1..=LBRR_ONSET_ONLY_MAX_LOSS_PERC).contains(&self.loss_perc)
                        || lbrr_interval_is_onset(rms, self.lbrr_prev_rms);
                    self.lbrr_prev_rms = rms;
                    let side_frame = if pending.side_active && important {
                        let sf =
                            ls.analyze_frame_sized(&pending.side_pcm, true, self.silk_frame_size)?;
                        (sf.header.frame_type >= 2).then_some(sf)
                    } else {
                        None
                    };
                    let mf =
                        lm.analyze_frame_sized(&pending.mid_pcm, true, self.silk_frame_size)?;
                    let mid_frame = if mf.header.frame_type >= 2 && important {
                        let mut mf = mf;
                        mf.header.stereo = Some(pending.weights);
                        // §4.2.7.2 on an LBRR mid frame: present (and
                        // SET) iff no side LBRR frame rides.
                        mf.header.mid_only_flag = if side_frame.is_some() {
                            None
                        } else {
                            Some(true)
                        };
                        Some(mf)
                    } else {
                        None
                    };
                    (mid_frame, side_frame)
                }
                None => (None, None),
            };
        let fec_snapshot = self.fec.then(|| (self.mid.clone(), self.side.clone()));

        // Per-channel analysis (each Hybrid frame is a fresh Opus
        // frame: first-in-packet threading, decoder carry kept inert
        // by the analyzers' gain floors).
        let mut mid_frame = self
            .mid
            .analyze_frame_sized(&ms.mid, true, self.silk_frame_size)?;
        mid_frame.header.stereo = Some(weight_symbols);
        let side_frame = if code_side {
            Some(
                self.side
                    .analyze_frame_sized(&ms.side, true, self.silk_frame_size)?,
            )
        } else {
            // Mid-only: the decoder clears the side carried state
            // after the uncoded side frame — mirror it.
            self.side.reset();
            None
        };
        // Clear a one-shot §2.1.9 resume treatment (see `dtx_gate`).
        self.mid.set_force_unvoiced(false);
        self.side.set_force_unvoiced(false);
        let side_active = side_frame
            .as_ref()
            .is_some_and(|f| f.header.frame_type >= 2);
        // §4.2.7.2: flag present iff the side VAD is clear for the
        // interval; set iff no side frame is coded at all.
        mid_frame.header.mid_only_flag = if side_active {
            None
        } else {
            Some(side_frame.is_none())
        };
        // A coded-but-inactive side frame keeps the flag present and
        // cleared; the frame itself still rides the bitstream.

        // Arm the NEXT packet's redundancy from THIS packet's mix
        // products and pre-packet snapshots.
        if let Some((mid_analyzer, side_analyzer)) = fec_snapshot {
            self.pending_fec = Some(PendingFecHybridStereo {
                mid_pcm: ms.mid.clone(),
                side_pcm: ms.side.clone(),
                side_active,
                weights: weight_symbols,
                mid_analyzer,
                side_analyzer,
            });
        }

        let header = SilkHeaderBits {
            num_silk_frames: 1,
            mid: SilkChannelHeader {
                vad_flags: u8::from(mid_frame.header.frame_type >= 2),
                lbrr_flag: lbrr_mid.is_some(),
            },
            side: Some(SilkChannelHeader {
                vad_flags: u8::from(side_active),
                lbrr_flag: lbrr_side.is_some(),
            }),
            // n = 1: the global LBRR flags ARE the per-frame flags.
            per_frame_lbrr: PerFrameLbrr {
                mid: u8::from(lbrr_mid.is_some()),
                side: u8::from(lbrr_side.is_some()),
            },
        };
        header.encode(&mut re)?;

        // §4.2.5: the LBRR frames precede the regular frames, mid
        // then side, each coded like a first-in-sequence active frame.
        if let Some(lf) = &lbrr_mid {
            let cfg = SilkFrameConfig {
                bandwidth: Bandwidth::Wb,
                frame_size: self.silk_frame_size,
                voice_active: true,
                first_subframe_independent: true,
                previous_log_gain: None,
                previous_primary_lag: None,
                ltp_scaling_present: true,
                lsf_interp_after_reset: true,
                previous_nlsf_q15: None,
                previous_nlsf_len: 0,
                stereo: Some(StereoHeaderContext {
                    has_mid_only_flag: lbrr_side.is_none(),
                }),
            };
            let _ = encode_silk_frame(&mut re, cfg, &lf.symbols())?;
        }
        if let Some(sf) = &lbrr_side {
            let cfg = SilkFrameConfig {
                bandwidth: Bandwidth::Wb,
                frame_size: self.silk_frame_size,
                voice_active: true,
                first_subframe_independent: true,
                previous_log_gain: None,
                previous_primary_lag: None,
                ltp_scaling_present: true,
                lsf_interp_after_reset: true,
                previous_nlsf_q15: None,
                previous_nlsf_len: 0,
                stereo: None,
            };
            let _ = encode_silk_frame(&mut re, cfg, &sf.symbols())?;
        }

        let stereo_ctx = StereoHeaderContext {
            has_mid_only_flag: !side_active,
        };
        let mid_cfg = SilkFrameConfig {
            bandwidth: Bandwidth::Wb,
            frame_size: self.silk_frame_size,
            voice_active: mid_frame.header.frame_type >= 2,
            first_subframe_independent: true,
            previous_log_gain: None,
            previous_primary_lag: None,
            ltp_scaling_present: true,
            lsf_interp_after_reset: true,
            previous_nlsf_q15: None,
            previous_nlsf_len: 0,
            stereo: Some(stereo_ctx),
        };
        let _ = encode_silk_frame(&mut re, mid_cfg, &mid_frame.symbols())?;
        if let Some(side) = &side_frame {
            let side_cfg = SilkFrameConfig {
                bandwidth: Bandwidth::Wb,
                frame_size: self.silk_frame_size,
                voice_active: side_active,
                first_subframe_independent: true,
                previous_log_gain: None,
                previous_primary_lag: None,
                ltp_scaling_present: true,
                lsf_interp_after_reset: true,
                previous_nlsf_q15: None,
                previous_nlsf_len: 0,
                stereo: None,
            };
            let _ = encode_silk_frame(&mut re, side_cfg, &side.symbols())?;
        }
        Ok((toc, re))
    }

    /// Phase 2: identical to the mono encoder's (§4.5.1.1 redundancy
    /// flag off + stereo §4.3 CELT layer on the same coder).
    fn finish_with_celt(
        &mut self,
        pcm: &[i16],
        toc: u8,
        mut re: RangeEncoder,
        payload_bytes: usize,
    ) -> Result<Vec<u8>, Error> {
        let total_bits = payload_bytes as u32 * 8;
        debug_assert!(re.tell() <= total_bits, "SILK layer over budget");
        if total_bits.saturating_sub(re.tell()) >= HYBRID_REDUNDANCY_MIN_REMAINING_BITS {
            re.enc_icdf(0, &REDUNDANCY_FLAG_ICDF, REDUNDANCY_FLAG_ICDF_FTB);
        }
        let _info = encode_celt_frame(
            &mut self.celt,
            &mut re,
            pcm,
            payload_bytes,
            crate::celt_band_layout::HYBRID_FIRST_CODED_BAND,
            self.end_band,
            self.lm,
        );
        debug_assert!(re.tell() <= total_bits, "hybrid CELT layer bust");
        let body = re
            .finish_fixed(payload_bytes)
            .ok_or(Error::MalformedPacket)?;
        let mut packet = Vec::with_capacity(1 + payload_bytes);
        packet.push(toc);
        packet.extend_from_slice(&body);
        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimator_is_linear_phase_with_82_sample_delay() {
        let mut d = Decimator48To16::new();
        // Impulse at 48 kHz position 300 → output peak at 16 kHz
        // position (300 + 82) / 3.
        let mut input = vec![0.0f64; 3 * 400];
        input[300] = 32768.0;
        let out = d.process(&input);
        let peak = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        let expect = (300 + 82) / 3;
        assert!(
            (peak as i64 - expect as i64).abs() <= 1,
            "peak {peak} expect {expect}"
        );
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(HybridEncoderMono::new(Bandwidth::Nb, 200).is_err());
        assert!(HybridEncoderMono::new(Bandwidth::Fb, 400).is_err());
        assert!(HybridEncoderMono::new(Bandwidth::Fb, 200).is_ok());
    }
}
