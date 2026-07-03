//! Analysed SILK encoding: PCM → SILK-only Opus packets — RFC 6716
//! §5.2 front half driving the §4.2.7 write-side wire mirrors.
//!
//! This module is the round-388 integration of the encoder
//! signal-analysis stack: unlike
//! [`crate::silk_packet_encode::encode_silk_only_packet_mono`], which
//! consumes caller-supplied symbol scripts, these encoders derive
//! every Table-5 symbol from the input audio itself. The per-channel
//! analysis chain ([`ChannelAnalyzer`]):
//!
//!  1. **Short-term prediction** (§5.2.3.4): Burg LPC on the frame
//!     ([`crate::silk_lpc_analysis`]), bandwidth-expanded, converted
//!     to NLSF ([`crate::silk_lpc_to_nlsf`]) and quantized to the
//!     §4.2.7.5 `(I1, I2[])` wire indices
//!     ([`crate::silk_nlsf_quantize`]). The frame always signals
//!     `w_Q2 = 4` (no interpolation split).
//!  2. **Pitch + voicing** (§5.2.3.2): whitened-domain open-loop lag
//!     search ([`crate::silk_pitch`]); voiced frames quantize the
//!     primary lag (absolute coding — every packet carries one SILK
//!     frame, which §4.2.7.6.1 makes absolute) and pitch contour, and
//!     run the §5.2.3.6 LTP codebook quantisation
//!     ([`crate::silk_ltp_analysis`]) against the DECODED lags.
//!  3. **Gains** (§5.2.3.3's role): per-subframe residual energy →
//!     the §4.2.7.4 log-gain index whose dequantised value scales the
//!     excitation pulses to a target RMS; the first subframe's index
//!     is floored at `previous_log_gain - 16` so the decoder-side
//!     §4.2.7.4 clamp (which carries ACROSS packets) can never bind
//!     and the fresh-decoder packet writer stays stream-exact.
//!  4. **Excitation** (§5.2.3.8's role): closed-loop pulse rounding
//!     against the carried decoder state
//!     ([`crate::silk_excitation_quantize`]).
//!
//! [`SilkEncoderMono`] wraps one analyzer into code-0 mono packets.
//! [`SilkEncoderStereo`] adds the §5.2.2 stereo mixing front end: the
//! least-squares §4.2.7.1 weight estimate
//! ([`crate::silk_stereo::estimate_stereo_weights`]) quantized through
//! [`StereoWeightSymbols::quantize`], the exact §4.2.8 algebraic
//! downmix ([`crate::silk_stereo::stereo_lr_to_ms`]) run with the
//! QUANTIZED weights, per-channel analysis of the mid and side
//! signals, and the §4.2.7.2 mid-only escape when the residual side
//! energy is negligible (mirroring the decoder's side-state reset on
//! an uncoded side frame).
//!
//! The produced packets are ordinary code-0 SILK-only packets that a
//! fresh or streaming [`crate::decoder::OpusDecoder`] decodes to
//! audio tracking the input.
//!
//! Input is PCM at the SILK **internal** rate (8 kHz NB / 12 kHz MB /
//! 16 kHz WB), nominal range `[-1.0, 1.0]`, one 20 ms frame per
//! packet.
//!
//! All truth is taken from RFC 6716 §4.2.7 / §4.2.8 / §5.2. No
//! external library source is consulted.

use crate::silk_decode::SilkFrameSymbols;
use crate::silk_excitation::{ExcitationSymbols, SilkFrameSize};
use crate::silk_excitation_quantize::{
    quantize_excitation_frame, ExcitationQuantized, LtpFrameParams,
};
use crate::silk_frame::{
    QuantizationOffsetType, SignalType, SilkHeaderSymbols, StereoPredictionWeights,
    StereoWeightSymbols,
};
use crate::silk_gains::{GainSymbol, SubframeGains, SubframeGainsConfig};
use crate::silk_log2lin::silk_gains_dequant;
use crate::silk_lpc_analysis::{bandwidth_expand, burg_lpc, lpc_residual};
use crate::silk_lpc_synth::{subframe_samples, LpcSynthState};
use crate::silk_lpc_to_nlsf::lpc_to_nlsf_q15;
use crate::silk_lsf_stage2::{D_LPC_NB_MB, D_LPC_WB};
use crate::silk_lsf_to_lpc::LpcQ17;
use crate::silk_ltp::{contour_offsets, lag_range, LtpSymbols, LTP_MAX_SUBFRAMES};
use crate::silk_ltp_analysis::ltp_analysis;
use crate::silk_ltp_synth::LtpSynthState;
use crate::silk_nlsf_quantize::quantize_nlsf;
use crate::silk_packet_encode::{
    encode_silk_only_packet_mono, encode_silk_only_packet_stereo, StereoIntervalScripts,
};
use crate::silk_pitch::{pitch_analysis, quantize_lag};
use crate::silk_stereo::{stereo_lr_to_ms, StereoDownmixState, StereoWeightsQ13};
use crate::toc::Bandwidth;
use crate::Error;

/// Target RMS of the excitation pulses (`e_raw`) the gain selection
/// aims for — the bitrate/precision knob of this encoder.
const TARGET_PULSE_RMS: f64 = 2.0;

/// Initial bandwidth-expansion chirp applied to the Burg predictor
/// before LSF conversion.
const ANALYSIS_CHIRP: f64 = 0.996;

/// Side-channel RMS below which a stereo interval is coded mid-only
/// (§4.2.7.2).
const MID_ONLY_SIDE_RMS: f64 = 1.0e-4;

/// One channel's fully analysed SILK frame: every Table-5 symbol
/// (owned) plus the encoder's decode-mirror monitoring.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzedFrame {
    /// Steps 1-3 header symbols (stereo parts filled by the caller).
    pub header: SilkHeaderSymbols,
    /// §4.2.7.4 per-subframe gain symbols.
    pub gains: Vec<GainSymbol>,
    /// §4.2.7.5.1 stage-1 index.
    pub lsf_stage1: u8,
    /// §4.2.7.5.2 stage-2 indices.
    pub i2: Vec<i8>,
    /// §4.2.7.6 symbols (voiced frames only).
    pub ltp: Option<LtpSymbols>,
    /// §4.2.7.7 LCG seed.
    pub lcg_seed: u8,
    /// §4.2.7.8.1 rate level.
    pub rate_level: u8,
    /// §4.2.7.8.2 per-block extra-LSB counts.
    pub lsb_counts: Vec<u8>,
    /// §4.2.7.8 signed excitation values.
    pub e_raw: Vec<i32>,
    /// The internal-rate signal the decoder will reconstruct.
    pub reconstructed: Vec<f32>,
    /// Whether the frame was coded voiced.
    pub voiced: bool,
}

impl AnalyzedFrame {
    /// Borrow the owned symbols as the [`SilkFrameSymbols`] view the
    /// §4.2 packet writers consume.
    pub fn symbols(&self) -> SilkFrameSymbols<'_> {
        SilkFrameSymbols {
            header: self.header,
            gains: &self.gains,
            lsf_stage1: self.lsf_stage1,
            lsf_stage2_i2: &self.i2,
            lsf_interp_w_q2: Some(4),
            ltp: self.ltp,
            lcg_seed: self.lcg_seed,
            excitation: ExcitationSymbols {
                rate_level: self.rate_level,
                lsb_counts: &self.lsb_counts,
                e_raw: &self.e_raw,
            },
        }
    }
}

/// The per-channel §5.2.3 analysis chain with its carried state
/// (input history, §4.2.7.9 synthesis histories, cross-packet gain
/// clamp base). One instance per coded channel.
#[derive(Debug, Clone)]
pub struct ChannelAnalyzer {
    bandwidth: Bandwidth,
    d_lpc: usize,
    /// Input history (internal rate) for whitening + pitch lookback.
    hist: Vec<f64>,
    /// Carried §4.2.7.9 synthesis histories (decoder-authoritative).
    ltp_state: LtpSynthState,
    lpc_state: LpcSynthState,
    /// Cross-packet §4.2.7.4 clamp base.
    prev_log_gain: Option<u8>,
}

impl ChannelAnalyzer {
    /// Create an analyzer for one SILK internal bandwidth (NB / MB /
    /// WB; SWB / FB are rejected — SILK never codes them).
    pub fn new(bandwidth: Bandwidth) -> Result<Self, Error> {
        let d_lpc = match bandwidth {
            Bandwidth::Nb | Bandwidth::Mb => D_LPC_NB_MB,
            Bandwidth::Wb => D_LPC_WB,
            _ => return Err(Error::MalformedPacket),
        };
        let (_, lag_max, _) = lag_range(bandwidth)?;
        let hist_len = lag_max as usize + 2 + d_lpc;
        Ok(Self {
            bandwidth,
            d_lpc,
            hist: vec![0.0; hist_len],
            ltp_state: LtpSynthState::new(bandwidth)?,
            lpc_state: LpcSynthState::new(bandwidth)?,
            prev_log_gain: None,
        })
    }

    /// Reset all carried state (matches a §4.5.2 decoder reset, or
    /// the decoder's side-channel state clear after an uncoded side
    /// frame).
    pub fn reset(&mut self) {
        for v in self.hist.iter_mut() {
            *v = 0.0;
        }
        self.ltp_state.reset();
        self.lpc_state.reset();
        self.prev_log_gain = None;
    }

    /// Analyse one 20 ms internal-rate frame into a complete Table-5
    /// symbol script (stereo header parts left `None`; the stereo
    /// wrapper fills them). Advances every carried state.
    pub fn analyze_frame(&mut self, pcm: &[f32]) -> Result<AnalyzedFrame, Error> {
        let n = subframe_samples(self.bandwidth)?;
        let num_subframes = 4usize;
        let frame_len = n * num_subframes;
        if pcm.len() != frame_len {
            return Err(Error::MalformedPacket);
        }
        let (lag_min, lag_max, _) = lag_range(self.bandwidth)?;
        let hist_len = self.hist.len();

        // Contiguous analysis buffer: [history | frame], f64.
        let mut buf = Vec::with_capacity(hist_len + frame_len);
        buf.extend_from_slice(&self.hist);
        buf.extend(pcm.iter().map(|&v| v as f64));

        // ---- 1. Short-term prediction analysis (§5.2.3.4). ----
        // Burg over the frame plus one subframe of history for
        // conditioning; chirp; convert to NLSF with re-conditioning
        // retries; fall back to the uniform spectrum (A(z) = 1).
        let window = &buf[buf.len() - (frame_len + n)..];
        let mut a = burg_lpc(window, self.d_lpc)?;
        bandwidth_expand(&mut a, ANALYSIS_CHIRP);
        let nlsf_target = {
            let mut attempt = a.clone();
            let mut result = lpc_to_nlsf_q15(&attempt);
            let mut tries = 0;
            while result.is_err() && tries < 4 {
                bandwidth_expand(&mut attempt, 0.92);
                result = lpc_to_nlsf_q15(&attempt);
                tries += 1;
            }
            result.unwrap_or_else(|_| {
                (0..self.d_lpc)
                    .map(|k| (32768 * (k + 1) / (self.d_lpc + 1)) as i16)
                    .collect()
            })
        };

        // ---- 2. NLSF quantisation (§4.2.7.5) + decoder LPC. ----
        let nq = quantize_nlsf(self.bandwidth, &nlsf_target)?;
        let lpc_hat = LpcQ17::from_nlsf(self.bandwidth, nq.nlsf_q15())?
            .range_limited()
            .prediction_gain_limited();
        let a_q12 = lpc_hat.a_q12().to_vec();
        let a_hat: Vec<f64> = a_q12.iter().map(|&c| c as f64 / 4096.0).collect();

        // Whitened residual over the whole buffer with the QUANTIZED
        // filter (what the decoder's synthesis will invert).
        let r = lpc_residual(&buf, &[], &a_hat);

        // ---- 3. Pitch analysis + LTP (§5.2.3.2 / §5.2.3.6). ----
        let pa = pitch_analysis(self.bandwidth, &r, hist_len, num_subframes)?;
        let voiced = pa.voiced;
        let (ltp_symbols, ltp_params, decoded_lags) = if voiced {
            // Quantize the primary lag first; re-derive the DECODED
            // subframe lags from the decoded primary so the LTP
            // analysis and the excitation loop see exactly what the
            // decoder will.
            let (lag_sym, decoded_primary) = quantize_lag(self.bandwidth, pa.primary_lag, None)?;
            let offs = contour_offsets(self.bandwidth, num_subframes, pa.contour_index)?;
            let mut lags = [0i32; LTP_MAX_SUBFRAMES];
            for (s, slot) in lags.iter_mut().enumerate().take(num_subframes) {
                *slot = (decoded_primary + offs[s] as i32).clamp(lag_min, lag_max);
            }
            let lq = ltp_analysis(
                self.bandwidth,
                &r,
                hist_len,
                num_subframes,
                &lags[..num_subframes],
            )?;
            let symbols = LtpSymbols {
                lag: lag_sym,
                contour_index: pa.contour_index,
                periodicity_index: lq.periodicity_index,
                filter_indices: lq.filter_indices,
                // Index 0 → the §4.2.7.6.3 default 15565.
                ltp_scaling_index: Some(0),
            };
            let params = LtpFrameParams {
                pitch_lags: lags,
                taps_q7: lq.taps_q7,
                ltp_scaling_q14: 15565,
            };
            (Some(symbols), Some(params), lags)
        } else {
            (None, None, [0i32; LTP_MAX_SUBFRAMES])
        };

        // ---- 4. Gain selection (§4.2.7.4 quantize). ----
        // Per-subframe LTP-filtered residual energy → the log gain
        // whose dequantised value puts the pulses at TARGET_PULSE_RMS.
        let signal_type = if voiced {
            SignalType::Voiced
        } else {
            SignalType::Unvoiced
        };
        let mut desired = [0u8; LTP_MAX_SUBFRAMES];
        for s in 0..num_subframes {
            let base = hist_len + s * n;
            let mut energy = 0.0f64;
            for i in 0..n {
                let mut v = r[base + i];
                if let Some(p) = &ltp_params {
                    let lag = decoded_lags[s] as usize;
                    for (k, &t) in p.taps_q7[s].iter().enumerate() {
                        v -= t as f64 / 128.0 * r[base + i + 2 - lag - k];
                    }
                }
                energy += v * v;
            }
            let rms = (energy / n as f64).sqrt();
            // e_raw ≈ res * 2^31 / gain_Q16 (see the module docs of
            // silk_excitation_quantize): gain that lands the pulses on
            // the target RMS.
            let want_gain = rms * (1u64 << 31) as f64 / TARGET_PULSE_RMS;
            desired[s] = quantize_log_gain(want_gain);
        }
        // Cross-packet §4.2.7.4 clamp safety: the decoder computes
        // log_gain = max(gain_index, prev - 16) with prev carried
        // ACROSS packets; flooring our index keeps the clamp inert.
        if let Some(prev) = self.prev_log_gain {
            desired[0] = desired[0].max(prev.saturating_sub(16));
        }
        let gains_cfg = SubframeGainsConfig {
            signal_type,
            num_subframes: num_subframes as u8,
            first_subframe_is_independent: true,
            previous_log_gain: None,
        };
        let (gain_symbols, gains) = SubframeGains::quantize(gains_cfg, &desired[..num_subframes])?;
        let gains_q16_arr = gains.dequant_q16();
        // The fresh-decoder packet writer will reproduce these exact
        // gains; carry the last for the next packet's floor.
        self.prev_log_gain = Some(gains.last_log_gain());

        // ---- 5. Closed-loop excitation (§5.2.3.8 role). ----
        let lcg_seed = 0u8;
        let ExcitationQuantized {
            e_raw,
            lsb_counts,
            rate_level,
            reconstructed,
        } = quantize_excitation_frame(
            self.bandwidth,
            SilkFrameSize::TwentyMs,
            signal_type,
            QuantizationOffsetType::Low,
            lcg_seed,
            &gains_q16_arr[..num_subframes],
            &a_q12,
            ltp_params.as_ref(),
            pcm,
            &mut self.ltp_state,
            &mut self.lpc_state,
        )?;

        // Roll the input history.
        self.hist.extend(pcm.iter().map(|&v| v as f64));
        let cut = self.hist.len() - hist_len;
        self.hist.drain(..cut);

        Ok(AnalyzedFrame {
            header: SilkHeaderSymbols {
                stereo: None,
                mid_only_flag: None,
                // Table 10: Unvoiced/Low = 2, Voiced/Low = 4 (active).
                frame_type: if voiced { 4 } else { 2 },
            },
            gains: gain_symbols,
            lsf_stage1: nq.lsf_stage1,
            i2: nq.i2().to_vec(),
            ltp: ltp_symbols,
            lcg_seed,
            rate_level,
            lsb_counts,
            e_raw,
            reconstructed,
            voiced,
        })
    }
}

/// One encoded packet plus the encoder's decode-mirror monitoring.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedSilkPacket {
    /// The complete code-0 SILK-only Opus packet (TOC + payload).
    pub packet: Vec<u8>,
    /// The internal-rate signal the decoder will reconstruct for this
    /// packet (mono: the channel; stereo: the MID channel).
    pub reconstructed: Vec<f32>,
    /// Whether the (mid) frame was coded voiced.
    pub voiced: bool,
}

/// Streaming mono SILK encoder: 20 ms of internal-rate PCM in, one
/// SILK-only Opus packet out.
#[derive(Debug, Clone)]
pub struct SilkEncoderMono {
    channel: ChannelAnalyzer,
}

impl SilkEncoderMono {
    /// Create an encoder for one SILK internal bandwidth (NB / MB /
    /// WB; SWB / FB are rejected — SILK never codes them).
    pub fn new(bandwidth: Bandwidth) -> Result<Self, Error> {
        Ok(Self {
            channel: ChannelAnalyzer::new(bandwidth)?,
        })
    }

    /// Per-packet input length: 20 ms at the internal rate
    /// (160 NB / 240 MB / 320 WB samples).
    pub fn frame_samples(&self) -> usize {
        subframe_samples(self.channel.bandwidth).unwrap_or(0) * 4
    }

    /// Reset all carried state (matches a §4.5.2 decoder reset).
    pub fn reset(&mut self) {
        self.channel.reset();
    }

    /// Encode one 20 ms frame of mono internal-rate PCM into a
    /// SILK-only Opus packet.
    ///
    /// `pcm.len()` must equal [`Self::frame_samples`]; samples are
    /// nominally in `[-1.0, 1.0]`.
    pub fn encode_packet(&mut self, pcm: &[f32]) -> Result<EncodedSilkPacket, Error> {
        let bandwidth = self.channel.bandwidth;
        let frame = self.channel.analyze_frame(pcm)?;
        let (packet, _) = encode_silk_only_packet_mono(bandwidth, 200, &[frame.symbols()])?;
        Ok(EncodedSilkPacket {
            packet,
            reconstructed: frame.reconstructed,
            voiced: frame.voiced,
        })
    }
}

/// Streaming stereo SILK encoder: 20 ms of L/R internal-rate PCM in,
/// one stereo SILK-only Opus packet out (§5.2.2 stereo mixing +
/// §4.2.7.1 weight coding + §4.2.7.2 mid-only escape).
#[derive(Debug, Clone)]
pub struct SilkEncoderStereo {
    bandwidth: Bandwidth,
    mid: ChannelAnalyzer,
    side: ChannelAnalyzer,
    downmix: StereoDownmixState,
    /// Previous frame's trailing raw-mid sample (the §4.2.8 `p0`
    /// boundary term for the weight estimate).
    prev_mid: f32,
}

impl SilkEncoderStereo {
    /// Create a stereo encoder for one SILK internal bandwidth.
    pub fn new(bandwidth: Bandwidth) -> Result<Self, Error> {
        Ok(Self {
            bandwidth,
            mid: ChannelAnalyzer::new(bandwidth)?,
            side: ChannelAnalyzer::new(bandwidth)?,
            downmix: StereoDownmixState::new(),
            prev_mid: 0.0,
        })
    }

    /// Per-packet input length per channel (20 ms at the internal
    /// rate).
    pub fn frame_samples(&self) -> usize {
        subframe_samples(self.bandwidth).unwrap_or(0) * 4
    }

    /// Reset all carried state.
    pub fn reset(&mut self) {
        self.mid.reset();
        self.side.reset();
        self.downmix.reset();
        self.prev_mid = 0.0;
    }

    /// Encode one 20 ms frame of stereo internal-rate PCM.
    ///
    /// `left.len()` and `right.len()` must equal
    /// [`Self::frame_samples`]. `next_lr` is the NEXT frame's first
    /// left/right sample pair when known (the §4.2.8 one-sample
    /// lookahead of the exact downmix); pass `None` at stream end.
    pub fn encode_packet(
        &mut self,
        left: &[f32],
        right: &[f32],
        next_lr: Option<(f32, f32)>,
    ) -> Result<EncodedSilkPacket, Error> {
        let frame_len = self.frame_samples();
        if left.len() != frame_len || right.len() != frame_len {
            return Err(Error::MalformedPacket);
        }

        // ---- §5.2.3.4 stereo mixing: estimate + quantize weights,
        // then run the exact §4.2.8 inverse with the QUANTIZED pair
        // (what the decoder will apply). ----
        let mid_raw: Vec<f32> = left
            .iter()
            .zip(right)
            .map(|(&l, &r)| (l + r) / 2.0)
            .collect();
        let side_raw: Vec<f32> = left
            .iter()
            .zip(right)
            .map(|(&l, &r)| (l - r) / 2.0)
            .collect();
        let mid_next = match next_lr {
            Some((l, r)) => (l + r) / 2.0,
            None => mid_raw[frame_len - 1],
        };
        let target = crate::silk_stereo::estimate_stereo_weights(
            &mid_raw,
            &side_raw,
            self.prev_mid,
            mid_next,
        )?;
        let weight_symbols = StereoWeightSymbols::quantize(StereoPredictionWeights {
            w0_q13: target.w0_q13,
            w1_q13: target.w1_q13,
        });
        let decoded_w = weight_symbols.weights();
        let ms = stereo_lr_to_ms(
            self.bandwidth,
            left,
            right,
            StereoWeightsQ13 {
                w0_q13: decoded_w.w0_q13,
                w1_q13: decoded_w.w1_q13,
            },
            next_lr,
            &mut self.downmix,
        )?;
        self.prev_mid = mid_raw[frame_len - 1];

        // ---- Mid-only decision (§4.2.7.2). ----
        let side_energy: f64 = ms.side.iter().map(|&v| (v as f64) * (v as f64)).sum();
        let side_rms = (side_energy / frame_len as f64).sqrt();
        let code_side = side_rms > MID_ONLY_SIDE_RMS;

        // ---- Per-channel analysis. ----
        let mut mid_frame = self.mid.analyze_frame(&ms.mid)?;
        mid_frame.header.stereo = Some(weight_symbols);

        let (packet, mid_reconstructed, voiced) = if code_side {
            let side_frame = self.side.analyze_frame(&ms.side)?;
            // Side coded with an active type → side VAD set → the
            // §4.2.7.2 mid-only flag is absent.
            mid_frame.header.mid_only_flag = None;
            let iv = StereoIntervalScripts {
                mid: mid_frame.symbols(),
                side: Some(side_frame.symbols()),
            };
            let (packet, _) = encode_silk_only_packet_stereo(self.bandwidth, 200, &[iv])?;
            (packet, mid_frame.reconstructed.clone(), mid_frame.voiced)
        } else {
            // Mid-only: flag present and set; the decoder clears the
            // side channel's synthesis state and gain history after
            // the uncoded side frame — mirror it.
            mid_frame.header.mid_only_flag = Some(true);
            self.side.reset();
            let iv = StereoIntervalScripts {
                mid: mid_frame.symbols(),
                side: None,
            };
            let (packet, _) = encode_silk_only_packet_stereo(self.bandwidth, 200, &[iv])?;
            (packet, mid_frame.reconstructed.clone(), mid_frame.voiced)
        };

        Ok(EncodedSilkPacket {
            packet,
            reconstructed: mid_reconstructed,
            voiced,
        })
    }
}

/// Map a desired linear Q16 gain to the §4.2.7.4 `log_gain` index
/// whose dequantised value is nearest in the log domain.
fn quantize_log_gain(want_gain_q16: f64) -> u8 {
    let want = want_gain_q16.max(1.0).ln();
    let mut best = (0u8, f64::MAX);
    for lg in 0..=63u8 {
        let g = silk_gains_dequant(lg) as f64;
        let err = (g.ln() - want).abs();
        if err < best.1 {
            best = (lg, err);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::OpusDecoder;

    fn snr_db(reference: &[f32], test: &[f32]) -> f64 {
        let mut sig = 0.0f64;
        let mut err = 0.0f64;
        for (&r, &t) in reference.iter().zip(test.iter()) {
            sig += (r as f64) * (r as f64);
            err += ((r - t) as f64) * ((r - t) as f64);
        }
        if err == 0.0 {
            return 120.0;
        }
        10.0 * (sig / err).log10()
    }

    /// Least-squares sinusoid projection at frequency `f_hz`: returns
    /// (amplitude, SNR dB of the fit). Absorbs the decoder
    /// resampler's delay/phase/gain, making it a pure "is this the
    /// input tone" metric.
    fn sine_projection(x: &[f64], f_hz: f64, rate: f64) -> (f64, f64) {
        let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let w = core::f64::consts::TAU * f_hz * i as f64 / rate;
            let (s, c) = w.sin_cos();
            ss += s * s;
            cc += c * c;
            sc += s * c;
            xs += v * s;
            xc += v * c;
        }
        let det = ss * cc - sc * sc;
        if det.abs() < 1e-9 {
            return (0.0, 0.0);
        }
        let a = (xs * cc - xc * sc) / det;
        let b = (xc * ss - xs * sc) / det;
        let mut sig = 0.0;
        let mut err = 0.0;
        for (i, &v) in x.iter().enumerate() {
            let w = core::f64::consts::TAU * f_hz * i as f64 / rate;
            let fit = a * w.sin() + b * w.cos();
            sig += fit * fit;
            err += (v - fit) * (v - fit);
        }
        let amp = (a * a + b * b).sqrt();
        if err == 0.0 {
            return (amp, 120.0);
        }
        (amp, 10.0 * (sig / err).log10())
    }

    /// End-to-end: encode a 400 Hz sine at WB from PCM alone, decode
    /// the packets with the real streaming OpusDecoder, and require
    /// the 48 kHz output to BE that sine (projection SNR), plus a
    /// healthy internal-rate closed-loop SNR.
    #[test]
    fn wb_sine_roundtrips_through_real_decoder() {
        let bw = Bandwidth::Wb;
        let mut enc = SilkEncoderMono::new(bw).unwrap();
        let flen = enc.frame_samples();
        assert_eq!(flen, 320);
        let fs = 16_000.0f64;
        let f = 400.0f64;

        let mut dec = OpusDecoder::new();
        let mut decoded_48k: Vec<f64> = Vec::new();
        let mut internal_snrs = Vec::new();
        for pkt_idx in 0..12 {
            let pcm: Vec<f32> = (0..flen)
                .map(|i| {
                    let t = (pkt_idx * flen + i) as f64 / fs;
                    (0.3 * (core::f64::consts::TAU * f * t).sin()) as f32
                })
                .collect();
            let out = enc.encode_packet(&pcm).unwrap();
            internal_snrs.push(snr_db(&pcm, &out.reconstructed));

            let audio = dec.decode_packet(&out.packet).unwrap();
            assert_eq!(audio.channels, 1);
            assert_eq!(audio.sample_rate_hz, 48_000);
            decoded_48k.extend(audio.pcm.iter().map(|&v| v as f64 / 32768.0));
        }

        // Steady-state packets (skip 2 warmup frames) must track the
        // input closely at the internal rate.
        let steady = &internal_snrs[2..];
        let avg = steady.iter().sum::<f64>() / steady.len() as f64;
        assert!(avg > 12.0, "internal-rate SNR too low: {avg:.1} dB");

        // The 48 kHz decode must BE the 400 Hz tone.
        let tail = &decoded_48k[decoded_48k.len() / 3..];
        let (_, proj) = sine_projection(tail, f, 48_000.0);
        assert!(proj > 10.0, "48 kHz sine projection SNR: {proj:.1} dB");
    }

    /// NB sine: same end-to-end path at 8 kHz internal rate.
    #[test]
    fn nb_sine_roundtrips_through_real_decoder() {
        let bw = Bandwidth::Nb;
        let mut enc = SilkEncoderMono::new(bw).unwrap();
        let flen = enc.frame_samples();
        assert_eq!(flen, 160);
        let fs = 8_000.0f64;
        let f = 220.0f64;

        let mut dec = OpusDecoder::new();
        let mut decoded_48k: Vec<f64> = Vec::new();
        for pkt_idx in 0..10 {
            let pcm: Vec<f32> = (0..flen)
                .map(|i| {
                    let t = (pkt_idx * flen + i) as f64 / fs;
                    (0.25 * (core::f64::consts::TAU * f * t).sin()) as f32
                })
                .collect();
            let out = enc.encode_packet(&pcm).unwrap();
            let audio = dec.decode_packet(&out.packet).unwrap();
            decoded_48k.extend(audio.pcm.iter().map(|&v| v as f64 / 32768.0));
        }
        let tail = &decoded_48k[decoded_48k.len() / 3..];
        let (_, proj) = sine_projection(tail, f, 48_000.0);
        assert!(proj > 10.0, "48 kHz sine projection SNR: {proj:.1} dB");
    }

    /// Speech-like signal — a glottal-style pulse train through a
    /// one-pole resonator (rich harmonics an order-16 LPC cannot
    /// fully whiten, unlike a few pure sinusoids) — must classify
    /// voiced, track well, and decode cleanly.
    #[test]
    fn pulse_train_encodes_voiced_and_decodes() {
        let bw = Bandwidth::Wb;
        let mut enc = SilkEncoderMono::new(bw).unwrap();
        let flen = enc.frame_samples();
        let period = 100usize; // 160 Hz at 16 kHz

        let mut dec = OpusDecoder::new();
        let mut any_voiced = false;
        let mut snrs = Vec::new();
        let mut lp = 0.0f64; // one-pole resonator state
        let mut sample_idx = 0usize;
        for _pkt in 0..8 {
            let pcm: Vec<f32> = (0..flen)
                .map(|_| {
                    let pulse = if sample_idx % period == 0 { 1.0 } else { 0.0 };
                    lp = 0.75 * lp + 0.25 * pulse;
                    sample_idx += 1;
                    (0.5 * lp) as f32
                })
                .collect();
            let out = enc.encode_packet(&pcm).unwrap();
            any_voiced |= out.voiced;
            snrs.push(snr_db(&pcm, &out.reconstructed));
            dec.decode_packet(&out.packet).unwrap();
        }
        assert!(any_voiced, "pulse train never classified voiced");
        let steady = &snrs[2..];
        let avg = steady.iter().sum::<f64>() / steady.len() as f64;
        assert!(avg > 10.0, "pulse-train tracking SNR too low: {avg:.1} dB");
    }

    /// Loud → silence → loud transitions: the cross-packet §4.2.7.4
    /// gain-clamp floor must keep every packet decodable with no
    /// encoder/decoder divergence blowup afterwards.
    #[test]
    fn gain_transitions_stay_stream_consistent() {
        let bw = Bandwidth::Mb;
        let mut enc = SilkEncoderMono::new(bw).unwrap();
        let flen = enc.frame_samples();
        let fs = 12_000.0f64;

        let mut dec = OpusDecoder::new();
        let mut last_snr = 0.0f64;
        for pkt_idx in 0..12 {
            let amp = match pkt_idx % 4 {
                0 | 1 => 0.4f64,
                2 => 0.0,
                _ => 0.4,
            };
            let pcm: Vec<f32> = (0..flen)
                .map(|i| {
                    let t = (pkt_idx * flen + i) as f64 / fs;
                    (amp * (core::f64::consts::TAU * 300.0 * t).sin()) as f32
                })
                .collect();
            let out = enc.encode_packet(&pcm).unwrap();
            dec.decode_packet(&out.packet).unwrap();
            if pkt_idx == 11 {
                last_snr = snr_db(&pcm, &out.reconstructed);
            }
        }
        assert!(
            last_snr > 8.0,
            "post-transition tracking degraded: {last_snr:.1} dB"
        );
    }

    /// Stereo end-to-end: an amplitude-panned 350 Hz sine (L = 2R)
    /// through SilkEncoderStereo decodes on the real OpusDecoder to a
    /// stereo 48 kHz signal that is that tone on both channels with
    /// the panning preserved.
    #[test]
    fn stereo_panned_sine_roundtrips_through_real_decoder() {
        let bw = Bandwidth::Wb;
        let mut enc = SilkEncoderStereo::new(bw).unwrap();
        let flen = enc.frame_samples();
        let fs = 16_000.0f64;
        let f = 350.0f64;

        let gen = |i: usize| -> (f32, f32) {
            let t = i as f64 / fs;
            let s = (core::f64::consts::TAU * f * t).sin();
            ((0.4 * s) as f32, (0.2 * s) as f32)
        };

        let mut dec = OpusDecoder::new();
        let mut l48: Vec<f64> = Vec::new();
        let mut r48: Vec<f64> = Vec::new();
        for pkt_idx in 0..12 {
            let mut left = Vec::with_capacity(flen);
            let mut right = Vec::with_capacity(flen);
            for i in 0..flen {
                let (l, r) = gen(pkt_idx * flen + i);
                left.push(l);
                right.push(r);
            }
            let next = gen((pkt_idx + 1) * flen);
            let out = enc.encode_packet(&left, &right, Some(next)).unwrap();
            let audio = dec.decode_packet(&out.packet).unwrap();
            assert_eq!(audio.channels, 2);
            for ch in audio.pcm.chunks_exact(2) {
                l48.push(ch[0] as f64 / 32768.0);
                r48.push(ch[1] as f64 / 32768.0);
            }
        }

        let tail_l = &l48[l48.len() / 3..];
        let tail_r = &r48[r48.len() / 3..];
        let (amp_l, proj_l) = sine_projection(tail_l, f, 48_000.0);
        let (amp_r, proj_r) = sine_projection(tail_r, f, 48_000.0);
        assert!(proj_l > 8.0, "left projection SNR: {proj_l:.1} dB");
        assert!(proj_r > 8.0, "right projection SNR: {proj_r:.1} dB");
        // Panning preserved: L ≈ 2R within 25%.
        let ratio = amp_l / amp_r.max(1e-9);
        assert!(
            (ratio - 2.0).abs() < 0.5,
            "panning ratio {ratio:.2} (amps {amp_l:.3}/{amp_r:.3})"
        );
    }

    /// Identical L and R (zero side signal): the encoder must take the
    /// §4.2.7.2 mid-only path and still decode to both channels.
    #[test]
    fn stereo_identical_channels_use_mid_only() {
        let bw = Bandwidth::Nb;
        let mut enc = SilkEncoderStereo::new(bw).unwrap();
        let flen = enc.frame_samples();

        let mut dec = OpusDecoder::new();
        for pkt_idx in 0..4 {
            let pcm: Vec<f32> = (0..flen)
                .map(|i| 0.3 * (((pkt_idx * flen + i) as f32) * 0.2).sin())
                .collect();
            let out = enc.encode_packet(&pcm, &pcm, None).unwrap();
            let audio = dec.decode_packet(&out.packet).unwrap();
            assert_eq!(audio.channels, 2);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(SilkEncoderMono::new(Bandwidth::Swb).is_err());
        assert!(SilkEncoderStereo::new(Bandwidth::Fb).is_err());
        let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
        assert!(enc.encode_packet(&[0.0; 100]).is_err());
        let mut senc = SilkEncoderStereo::new(Bandwidth::Wb).unwrap();
        assert!(senc.encode_packet(&[0.0; 100], &[0.0; 100], None).is_err());
    }

    /// reset() returns the encoder to a decoder-reset-compatible
    /// state: a fresh decoder accepts the first packet after reset.
    #[test]
    fn reset_realigns_with_fresh_decoder() {
        let bw = Bandwidth::Nb;
        let mut enc = SilkEncoderMono::new(bw).unwrap();
        let flen = enc.frame_samples();
        let pcm: Vec<f32> = (0..flen).map(|i| 0.2 * (i as f32 * 0.3).sin()).collect();
        for _ in 0..3 {
            enc.encode_packet(&pcm).unwrap();
        }
        enc.reset();
        let out = enc.encode_packet(&pcm).unwrap();
        let mut dec = OpusDecoder::new();
        assert!(dec.decode_packet(&out.packet).is_ok());
    }
}
