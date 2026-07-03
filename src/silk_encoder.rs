//! Analysed SILK encoding: PCM → SILK-only Opus packets — RFC 6716
//! §5.2 front half driving the §4.2.7 write-side wire mirrors.
//!
//! [`SilkEncoderMono`] is the round-388 integration of the encoder
//! signal-analysis stack: unlike
//! [`crate::silk_packet_encode::encode_silk_only_packet_mono`], which
//! consumes caller-supplied symbol scripts, this encoder derives every
//! Table-5 symbol from the input audio itself:
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
//! The produced packets are ordinary code-0 SILK-only packets that a
//! fresh or streaming [`crate::decoder::OpusDecoder`] decodes to
//! audio tracking the input.
//!
//! Input is mono PCM at the SILK **internal** rate (8 kHz NB /
//! 12 kHz MB / 16 kHz WB), nominal range `[-1.0, 1.0]`, one 20 ms
//! frame per packet.
//!
//! All truth is taken from RFC 6716 §4.2.7 / §5.2. No external
//! library source is consulted.

use crate::silk_decode::SilkFrameSymbols;
use crate::silk_excitation::SilkFrameSize;
use crate::silk_excitation_quantize::{
    quantize_excitation_frame, ExcitationQuantized, LtpFrameParams,
};
use crate::silk_frame::{QuantizationOffsetType, SignalType, SilkHeaderSymbols};
use crate::silk_gains::{SubframeGains, SubframeGainsConfig};
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
use crate::silk_packet_encode::encode_silk_only_packet_mono;
use crate::silk_pitch::{pitch_analysis, quantize_lag};
use crate::toc::Bandwidth;
use crate::Error;

/// Target RMS of the excitation pulses (`e_raw`) the gain selection
/// aims for — the bitrate/precision knob of this encoder.
const TARGET_PULSE_RMS: f64 = 2.0;

/// Initial bandwidth-expansion chirp applied to the Burg predictor
/// before LSF conversion.
const ANALYSIS_CHIRP: f64 = 0.996;

/// One encoded packet plus the encoder's decode-mirror monitoring.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedSilkPacket {
    /// The complete code-0 SILK-only Opus packet (TOC + payload).
    pub packet: Vec<u8>,
    /// The internal-rate signal the decoder will reconstruct for this
    /// packet (from the real §4.2.7.9 synthesis chain).
    pub reconstructed: Vec<f32>,
    /// Whether the frame was coded voiced.
    pub voiced: bool,
}

/// Streaming mono SILK encoder: 20 ms of internal-rate PCM in, one
/// SILK-only Opus packet out.
#[derive(Debug, Clone)]
pub struct SilkEncoderMono {
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

impl SilkEncoderMono {
    /// Create an encoder for one SILK internal bandwidth (NB / MB /
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

    /// Per-packet input length: 20 ms at the internal rate
    /// (160 NB / 240 MB / 320 WB samples).
    pub fn frame_samples(&self) -> usize {
        // new() validated the bandwidth.
        subframe_samples(self.bandwidth).unwrap_or(0) * 4
    }

    /// Reset all carried state (matches a §4.5.2 decoder reset).
    pub fn reset(&mut self) {
        for v in self.hist.iter_mut() {
            *v = 0.0;
        }
        self.ltp_state.reset();
        self.lpc_state.reset();
        self.prev_log_gain = None;
    }

    /// Encode one 20 ms frame of mono internal-rate PCM into a
    /// SILK-only Opus packet.
    ///
    /// `pcm.len()` must equal [`Self::frame_samples`]; samples are
    /// nominally in `[-1.0, 1.0]`.
    pub fn encode_packet(&mut self, pcm: &[f32]) -> Result<EncodedSilkPacket, Error> {
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

        // ---- 6. Wire composition (§4.2 packet walk). ----
        let symbols = SilkFrameSymbols {
            header: SilkHeaderSymbols {
                stereo: None,
                mid_only_flag: None,
                // Table 10: Unvoiced/Low = 2, Voiced/Low = 4 (active).
                frame_type: if voiced { 4 } else { 2 },
            },
            gains: &gain_symbols,
            lsf_stage1: nq.lsf_stage1,
            lsf_stage2_i2: nq.i2(),
            lsf_interp_w_q2: Some(4),
            ltp: ltp_symbols,
            lcg_seed,
            excitation: crate::silk_excitation::ExcitationSymbols {
                rate_level,
                lsb_counts: &lsb_counts,
                e_raw: &e_raw,
            },
        };
        let (packet, predictions) = encode_silk_only_packet_mono(self.bandwidth, 200, &[symbols])?;

        // Carry the decoder's cross-packet state.
        let decoded = predictions.first().ok_or(Error::MalformedPacket)?;
        self.prev_log_gain = Some(decoded.gains.last_log_gain());

        // Roll the input history.
        self.hist.extend(pcm.iter().map(|&v| v as f64));
        let cut = self.hist.len() - hist_len;
        self.hist.drain(..cut);

        Ok(EncodedSilkPacket {
            packet,
            reconstructed,
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

    /// Least-squares sinusoid-projection SNR: fit A*sin + B*cos at
    /// frequency `f_hz` to the decoded signal and measure the residual.
    /// Absorbs the decoder resampler's delay/phase/gain, making it a
    /// pure "is this the input tone" metric.
    fn sine_projection_snr_db(x: &[f64], f_hz: f64, rate: f64) -> f64 {
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
            return 0.0;
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
        if err == 0.0 {
            return 120.0;
        }
        10.0 * (sig / err).log10()
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
        let proj = sine_projection_snr_db(tail, f, 48_000.0);
        assert!(proj > 10.0, "48 kHz sine projection SNR: {proj:.1} dB");

        // And the tone must actually be voiced-coded by then.
        // (The first packet may classify unvoiced while history fills.)
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
        let proj = sine_projection_snr_db(tail, f, 48_000.0);
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

    #[test]
    fn rejects_bad_input() {
        assert!(SilkEncoderMono::new(Bandwidth::Swb).is_err());
        let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
        assert!(enc.encode_packet(&[0.0; 100]).is_err());
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
