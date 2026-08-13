//! Delayed-decision noise shaping quantisation for the SILK encoder —
//! the §5.2.3.8 multi-state trellis, RFC 6716.
//!
//! Where [`crate::silk_excitation_quantize::quantize_excitation_frame`]
//! walks the frame with a single greedy closed-loop state, this
//! quantiser follows the delayed-decision structure of the RFC 6716
//! §A embedded reference listing's noise shaping quantizer: `K`
//! parallel quantisation states, each seeded with a different
//! §4.2.7.7 LCG dither seed (`(k + seed) & 3` — the coded seed symbol
//! is *elected* by the winning state at no rate cost, since the seed
//! codes as two uniform bits either way). Every sample, each state
//! forks on its two best quantization-level candidates under the same
//! `(recon − want)² + λ·|q|` rate/distortion cost as the single-state
//! quantiser, and the `K` lowest-total-cost forks survive (the
//! listing's replace-worst-with-best-second pruning, generalized to a
//! full K-best selection). The decision horizon spans the whole SILK
//! frame: each state carries its own §4.2.7.9 synthesis mirrors, so —
//! unlike the listing's fixed-size shared buffers, which force
//! commitment after a bounded decision delay kept below the pitch lag
//! — no early commitment is needed; the frame-final winner's whole
//! trajectory is adopted. §5.2.3.8 is encoder freedom, so the bounded
//! delay is a latency/memory optimization this implementation does
//! not take.
//!
//! Per subframe, each surviving state advances its own cloned
//! [`LtpSynthState`] / [`LpcSynthState`] through the real §4.2.7.9
//! synthesis chain, so every state's predictions — and the final
//! carried state handed back to the caller — are bit-identical to a
//! decoder walking that state's would-be bitstream.
//!
//! All truth is taken from RFC 6716 §4.2.7.7 / §4.2.7.8 / §4.2.7.9 /
//! §5.2.3.8 and the §A embedded reference listing (staged
//! `docs/audio/opus/rfc6716-opus.txt`, hash-verified per §A.1). No
//! external library source is consulted.

use crate::silk_excitation::shell_block_count;
use crate::silk_excitation::{quantization_offset_q23, SilkFrameSize, SHELL_BLOCK_SAMPLES};
use crate::silk_excitation_quantize::{
    choose_pulse_pair, finish_excitation_symbols, ExcitationQuantized, LtpFrameParams,
    PulseRateControl,
};
use crate::silk_frame::{QuantizationOffsetType, SignalType};
use crate::silk_lpc_synth::{lpc_synthesis_subframe, subframe_samples, LpcSynthState};
use crate::silk_ltp::LTP_FILTER_TAPS;
use crate::silk_ltp_synth::{
    ltp_synth_commit_subframe, ltp_synthesis_subframe, LtpSynthState, LtpSynthSubframe,
};
use crate::toc::Bandwidth;
use crate::Error;

/// Maximum number of delayed-decision states (the listing's bound;
/// also the §4.2.7.7 seed alphabet size, so more states would only
/// duplicate dither sequences).
pub const MAX_DEL_DEC_STATES: usize = 4;

/// Result of the delayed-decision quantisation: the excitation plus
/// the elected §4.2.7.7 seed the caller must code for the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct DelDecQuantized {
    /// The winning state's excitation (same layout as the
    /// single-state quantiser's result, including the measured
    /// [`ExcitationQuantized::rd_q23`] cost).
    pub quantized: ExcitationQuantized,
    /// The §4.2.7.7 LCG seed the winning state started from — the
    /// frame's coded seed symbol.
    pub lcg_seed: u8,
}

/// One trellis state across subframe boundaries: its own §4.2.7.9
/// synthesis mirrors plus the frame trajectory decided so far.
#[derive(Clone)]
struct Heavy {
    ltp: LtpSynthState,
    lpc: LpcSynthState,
    seed_init: u8,
    seed: u32,
    rd: f64,
    e_raw: Vec<i32>,
    e_q23: Vec<i32>,
    recon: Vec<f32>,
}

/// One trellis state inside a subframe's sample walk (cheap to fork).
#[derive(Clone)]
struct Light {
    /// Index of the subframe-start [`Heavy`] this walk grew from.
    heavy: usize,
    seed: u32,
    rd: f64,
    /// Quantized local LPC history (seeded from the heavy state's
    /// mirror history, extended per sample).
    lpc_local: Vec<f32>,
    /// Excitation-delta history for the LTP ringing (subframe-local).
    delta: Vec<f32>,
    e_raw: Vec<i32>,
    e_q23: Vec<i32>,
}

/// Quantize one SILK frame's excitation with the §5.2.3.8
/// delayed-decision trellis over `n_states` dither-diverse states.
///
/// Arguments mirror
/// [`crate::silk_excitation_quantize::quantize_excitation_frame`];
/// `base_seed` replaces the fixed `lcg_seed` (state `k` starts from
/// `(base_seed + k) & 3`, exactly the listing's seeding), and the
/// elected seed comes back in [`DelDecQuantized::lcg_seed`].
/// `n_states` must be in `1..=`[`MAX_DEL_DEC_STATES`].
///
/// `ltp_state` / `lpc_state` end the frame carrying the WINNING
/// state's histories (decoder-identical for the produced excitation
/// + elected seed).
#[allow(clippy::too_many_arguments)]
pub fn quantize_excitation_frame_del_dec(
    bandwidth: Bandwidth,
    frame_size: SilkFrameSize,
    signal_type: SignalType,
    qoff_type: QuantizationOffsetType,
    base_seed: u8,
    n_states: usize,
    gains_q16: &[u32],
    a_q12: &[i32],
    ltp: Option<&LtpFrameParams>,
    target: &[f32],
    rate: &PulseRateControl,
    ltp_state: &mut LtpSynthState,
    lpc_state: &mut LpcSynthState,
) -> Result<DelDecQuantized, Error> {
    let n = subframe_samples(bandwidth)?;
    let num_subframes = match frame_size {
        SilkFrameSize::TenMs => 2usize,
        SilkFrameSize::TwentyMs => 4usize,
    };
    let frame_len = n * num_subframes;
    if base_seed > 3
        || !(1..=MAX_DEL_DEC_STATES).contains(&n_states)
        || gains_q16.len() != num_subframes
        || target.len() != frame_len
        || (signal_type == SignalType::Voiced) != ltp.is_some()
    {
        return Err(Error::MalformedPacket);
    }
    let d_lpc = lpc_state.d_lpc();
    if a_q12.len() != d_lpc {
        return Err(Error::MalformedPacket);
    }
    if rate
        .a_syn
        .as_ref()
        .is_some_and(|a_syn| a_syn.len() != d_lpc)
    {
        return Err(Error::MalformedPacket);
    }
    let a_i16: Vec<i16> = a_q12
        .iter()
        .map(|&c| c.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
        .collect();
    let a_f: Vec<f32> = a_i16.iter().map(|&c| c as f32 / 4096.0).collect();
    let offset_q23 = quantization_offset_q23(signal_type, qoff_type);

    // Initialize the trellis: each state clones the carried mirrors
    // and takes its own §4.2.7.7 seed (the listing's `(k + Seed) & 3`).
    let mut heavies: Vec<Heavy> = (0..n_states)
        .map(|k| {
            let mut ltp_mirror = ltp_state.clone();
            ltp_mirror.start_frame();
            Heavy {
                ltp: ltp_mirror,
                lpc: lpc_state.clone(),
                seed_init: ((base_seed as usize + k) & 3) as u8,
                seed: ((base_seed as usize + k) & 3) as u32,
                rd: 0.0,
                e_raw: Vec::with_capacity(frame_len),
                e_q23: Vec::with_capacity(frame_len),
                recon: Vec::with_capacity(frame_len),
            }
        })
        .collect();

    for s in 0..num_subframes {
        let gain_q16 = gains_q16[s];
        let gain_f = gain_q16 as f32 / 65536.0;
        let inv_gain = 65536.0 / gain_q16 as f32;
        let (pitch_lag, b_q7) = match ltp {
            Some(p) => (p.pitch_lags[s], p.taps_q7[s]),
            None => (1i32, [0i8; LTP_FILTER_TAPS]),
        };
        let cfg = LtpSynthSubframe {
            bandwidth,
            signal_type,
            frame_size,
            subframe_index: s as u8,
            gain_q16,
            pitch_lag,
            b_q7,
            ltp_scaling_q14: ltp.map(|p| p.ltp_scaling_q14).unwrap_or(0),
            a_q12: &a_i16,
            lsf_interp_used: false,
        };
        let b_f: [f32; LTP_FILTER_TAPS] = core::array::from_fn(|k| b_q7[k] as f32 / 128.0);
        let voiced = signal_type == SignalType::Voiced;

        // Zero-input LTP response per heavy state (each state's own
        // §4.2.7.9.1 lookback + ringing).
        let zeros = vec![0i32; n];
        let mut res_zero: Vec<Vec<f32>> = Vec::with_capacity(heavies.len());
        for h in &heavies {
            let mut rz = vec![0.0f32; n];
            ltp_synthesis_subframe(&h.ltp, cfg, &zeros, &mut rz)?;
            res_zero.push(rz);
        }

        // Per-sample trellis walk.
        let mut lights: Vec<Light> = heavies
            .iter()
            .enumerate()
            .map(|(idx, h)| Light {
                heavy: idx,
                seed: h.seed,
                rd: h.rd,
                lpc_local: h.lpc.history().to_vec(),
                delta: vec![0.0f32; n],
                e_raw: Vec::with_capacity(n),
                e_q23: Vec::with_capacity(n),
            })
            .collect();
        let hist_len = lights[0].lpc_local.len();

        for i in 0..n {
            // Fork every light state on its two candidates.
            // (light index, candidate e_raw, seed after LCG advance,
            // flip, want, accumulated rd)
            struct Fork {
                light: usize,
                e_raw: i32,
                seed_pre: u32,
                flip: bool,
                rd: f64,
                res_base: f32,
                lpc_pred: f32,
            }
            let mut forks: Vec<Fork> = Vec::with_capacity(2 * lights.len());
            for (li, light) in lights.iter().enumerate() {
                // LTP part: res_base = res_zero + delta ringing.
                let mut res_base = res_zero[light.heavy][i];
                if voiced {
                    for (k, &bf) in b_f.iter().enumerate() {
                        let src = i as i32 - pitch_lag + 2 - k as i32;
                        if src >= 0 {
                            res_base += light.delta[src as usize] * bf;
                        }
                    }
                }
                // LPC prediction (and the shaped-mode n_AR feedback)
                // from the state's own quantized history.
                let mut lpc_pred = 0.0f32;
                let mut n_ar = 0.0f32;
                for (k, &af) in a_f.iter().enumerate() {
                    let idx = hist_len + i - k - 1;
                    lpc_pred += light.lpc_local[idx] * af;
                    if let Some(a_syn) = &rate.a_syn {
                        n_ar += light.lpc_local[idx] * a_syn[k];
                    }
                }
                let desired_res = (target[s * n + i] + n_ar - lpc_pred) * inv_gain;
                let e_target_q23 = (desired_res - res_base) * 8_388_608.0;

                // §4.2.7.8.6 LCG advance + flip for THIS state.
                let seed_pre = light
                    .seed
                    .wrapping_mul(196_314_165)
                    .wrapping_add(907_633_515);
                let flip = (seed_pre & 0x8000_0000) != 0;
                let want = if flip { -e_target_q23 } else { e_target_q23 };
                for (cand, cost) in choose_pulse_pair(want, offset_q23, rate.lambda_pulses) {
                    forks.push(Fork {
                        light: li,
                        e_raw: cand,
                        seed_pre,
                        flip,
                        rd: light.rd + f64::from(cost),
                        res_base,
                        lpc_pred,
                    });
                }
            }
            // K-best survivor selection (the listing's per-sample
            // winner + replace-worst pruning, as a full sort).
            forks.sort_by(|a, b| a.rd.total_cmp(&b.rd));
            forks.truncate(n_states);

            let mut next: Vec<Light> = Vec::with_capacity(forks.len());
            for f in &forks {
                let mut light = lights[f.light].clone();
                light.seed = f.seed_pre.wrapping_add(f.e_raw as u32);
                light.rd = f.rd;
                // Decoder-identical e_Q23 for the candidate.
                let sign_e = match f.e_raw.cmp(&0) {
                    core::cmp::Ordering::Less => -1,
                    core::cmp::Ordering::Greater => 1,
                    core::cmp::Ordering::Equal => 0,
                };
                let mut e_q23 = (f.e_raw << 8) - sign_e * 20 + offset_q23;
                if f.flip {
                    e_q23 = -e_q23;
                }
                let e_f = e_q23 as f32 / 8_388_608.0;
                light.delta[i] = e_f + (f.res_base - res_zero[light.heavy][i]);
                let res_i = f.res_base + e_f;
                light.lpc_local.push(gain_f * res_i + f.lpc_pred);
                light.e_raw.push(f.e_raw);
                light.e_q23.push(e_q23);
                next.push(light);
            }
            lights = next;
        }

        // Subframe boundary: advance each survivor's own mirrors
        // through the real §4.2.7.9 chain with its trajectory.
        let mut next_heavies: Vec<Heavy> = Vec::with_capacity(lights.len());
        for light in &lights {
            let mut h = heavies[light.heavy].clone();
            let mut res_actual = vec![0.0f32; n];
            ltp_synthesis_subframe(&h.ltp, cfg, &light.e_q23, &mut res_actual)?;
            let mut out_sub = vec![0.0f32; n];
            let lpc_unclamped = lpc_synthesis_subframe(
                bandwidth,
                &mut h.lpc,
                &res_actual,
                gain_q16,
                &a_i16,
                &mut out_sub,
            )?;
            ltp_synth_commit_subframe(&mut h.ltp, &out_sub, &lpc_unclamped)?;
            h.recon.extend_from_slice(&out_sub);
            h.e_raw.extend_from_slice(&light.e_raw);
            h.e_q23.extend_from_slice(&light.e_q23);
            h.seed = light.seed;
            h.rd = light.rd;
            next_heavies.push(h);
        }
        heavies = next_heavies;
    }

    // Frame-final winner.
    let winner = heavies
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.rd.total_cmp(&b.rd))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let win = heavies.swap_remove(winner);
    *ltp_state = win.ltp;
    *lpc_state = win.lpc;

    // Pad to whole shell blocks and derive the wire-side symbols
    // exactly like the single-state quantiser.
    let shell_blocks = shell_block_count(bandwidth, frame_size)?;
    let mut e_raw_all = win.e_raw;
    e_raw_all.resize(shell_blocks * SHELL_BLOCK_SAMPLES, 0);
    let (lsb_counts, rate_level) = finish_excitation_symbols(
        bandwidth,
        frame_size,
        signal_type,
        qoff_type,
        win.seed_init,
        &e_raw_all,
    )?;

    Ok(DelDecQuantized {
        quantized: ExcitationQuantized {
            e_raw: e_raw_all,
            lsb_counts,
            rate_level,
            reconstructed: win.recon,
            rd_q23: win.rd,
        },
        lcg_seed: win.seed_init,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range_decoder::RangeDecoder;
    use crate::range_encoder::RangeEncoder;
    use crate::silk_excitation::{Excitation, ExcitationConfig, ExcitationSymbols};
    use crate::silk_excitation_quantize::{quantize_excitation_frame, MAX_PULSE_MAGNITUDE};
    use crate::silk_lsf_recon::cb1_q8;
    use crate::silk_lsf_to_lpc::LpcQ17;
    use crate::silk_ltp::{ltp_filter_taps_q7, LTP_MAX_SUBFRAMES};

    fn wb_codebook_lpc(i1: u8) -> Vec<i32> {
        let cb = cb1_q8(Bandwidth::Wb, i1).unwrap();
        let nlsf: Vec<i16> = cb.iter().map(|&v| (v as i16) << 7).collect();
        LpcQ17::from_nlsf(Bandwidth::Wb, &nlsf)
            .unwrap()
            .range_limited()
            .prediction_gain_limited()
            .a_q12()
            .to_vec()
    }

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

    /// The trellis never measures worse than the single-state
    /// quantiser on the same frame (same λ / shaping), across voiced
    /// and unvoiced content and all base seeds — the election's
    /// guarantee is measured, not assumed.
    #[test]
    fn trellis_rd_never_worse_than_single_state() {
        let bw = Bandwidth::Wb;
        let n = subframe_samples(bw).unwrap();
        let a_q12 = wb_codebook_lpc(12);
        let taps = ltp_filter_taps_q7(1, 3).unwrap();
        let ltp = LtpFrameParams {
            pitch_lags: [80; LTP_MAX_SUBFRAMES],
            taps_q7: [taps; LTP_MAX_SUBFRAMES],
            ltp_scaling_q14: 15565,
        };
        let voiced_target: Vec<f32> = (0..4 * n)
            .map(|i| {
                let t = i as f32;
                0.25 * (t * core::f32::consts::TAU / 80.0).sin() + 0.05 * (t * 0.031).sin()
            })
            .collect();
        let unvoiced_target: Vec<f32> = (0..4 * n)
            .map(|i| {
                let t = i as f32;
                0.12 * (t * 0.05).sin() + 0.06 * (t * 0.013).sin()
            })
            .collect();
        let gains = [30_000_000u32; 4];

        for &(vt, sig_type) in &[(true, SignalType::Voiced), (false, SignalType::Unvoiced)] {
            let target = if vt { &voiced_target } else { &unvoiced_target };
            let ltp_ref = vt.then_some(&ltp);
            for base_seed in 0..=3u8 {
                let mut ltp_a = LtpSynthState::new(bw).unwrap();
                let mut lpc_a = LpcSynthState::new(bw).unwrap();
                let single = quantize_excitation_frame(
                    bw,
                    SilkFrameSize::TwentyMs,
                    sig_type,
                    QuantizationOffsetType::Low,
                    base_seed,
                    &gains,
                    &a_q12,
                    ltp_ref,
                    target,
                    &PulseRateControl::default(),
                    &mut ltp_a,
                    &mut lpc_a,
                )
                .unwrap();
                let mut ltp_b = LtpSynthState::new(bw).unwrap();
                let mut lpc_b = LpcSynthState::new(bw).unwrap();
                let dd = quantize_excitation_frame_del_dec(
                    bw,
                    SilkFrameSize::TwentyMs,
                    sig_type,
                    QuantizationOffsetType::Low,
                    base_seed,
                    4,
                    &gains,
                    &a_q12,
                    ltp_ref,
                    target,
                    &PulseRateControl::default(),
                    &mut ltp_b,
                    &mut lpc_b,
                )
                .unwrap();
                assert!(
                    dd.quantized.rd_q23 <= single.rd_q23 * 1.000001,
                    "seed {base_seed} voiced={vt}: dd RD {} > single RD {}",
                    dd.quantized.rd_q23,
                    single.rd_q23
                );
                assert!(dd.lcg_seed <= 3);
                assert!(dd
                    .quantized
                    .e_raw
                    .iter()
                    .all(|&v| v.abs() <= MAX_PULSE_MAGNITUDE));
            }
        }
    }

    /// With one state and the same seed, the trellis walks the exact
    /// single-state trajectory apart from candidate ordering — the
    /// fork's best candidate must match `choose_pulse`, so the wire
    /// symbols and reconstruction are identical.
    #[test]
    fn one_state_matches_single_state_quantiser() {
        let bw = Bandwidth::Nb;
        let n = subframe_samples(bw).unwrap();
        let a_q12 = vec![0i32; 10];
        let target: Vec<f32> = (0..4 * n).map(|i| 0.1 * (i as f32 * 0.07).sin()).collect();
        let gains = [20_000_000u32; 4];

        let mut ltp_a = LtpSynthState::new(bw).unwrap();
        let mut lpc_a = LpcSynthState::new(bw).unwrap();
        let single = quantize_excitation_frame(
            bw,
            SilkFrameSize::TwentyMs,
            SignalType::Unvoiced,
            QuantizationOffsetType::Low,
            2,
            &gains,
            &a_q12,
            None,
            &target,
            &PulseRateControl::default(),
            &mut ltp_a,
            &mut lpc_a,
        )
        .unwrap();
        let mut ltp_b = LtpSynthState::new(bw).unwrap();
        let mut lpc_b = LpcSynthState::new(bw).unwrap();
        let dd = quantize_excitation_frame_del_dec(
            bw,
            SilkFrameSize::TwentyMs,
            SignalType::Unvoiced,
            QuantizationOffsetType::Low,
            2,
            1,
            &gains,
            &a_q12,
            None,
            &target,
            &PulseRateControl::default(),
            &mut ltp_b,
            &mut lpc_b,
        )
        .unwrap();
        assert_eq!(dd.lcg_seed, 2);
        assert_eq!(dd.quantized.e_raw, single.e_raw);
        assert_eq!(dd.quantized.lsb_counts, single.lsb_counts);
        assert_eq!(dd.quantized.rate_level, single.rate_level);
        for (a, b) in dd
            .quantized
            .reconstructed
            .iter()
            .zip(single.reconstructed.iter())
        {
            assert!((a - b).abs() < 1e-7);
        }
    }

    /// The winning trajectory must be decoder-consistent: encode the
    /// excitation on the wire with the ELECTED seed, decode it, and
    /// re-synthesize on fresh states — the result must equal the
    /// trellis's predicted reconstruction, and the carried states
    /// handed back must equal the re-synthesis states.
    #[test]
    fn winner_roundtrips_wire_and_states() {
        let bw = Bandwidth::Wb;
        let n = subframe_samples(bw).unwrap();
        let a_q12 = wb_codebook_lpc(7);
        let taps = ltp_filter_taps_q7(2, 10).unwrap();
        let ltp = LtpFrameParams {
            pitch_lags: [120; LTP_MAX_SUBFRAMES],
            taps_q7: [taps; LTP_MAX_SUBFRAMES],
            ltp_scaling_q14: 15565,
        };
        let target: Vec<f32> = (0..4 * n)
            .map(|i| {
                let t = i as f32;
                0.3 * (t * core::f32::consts::TAU / 120.0).sin() + 0.04 * (t * 0.017).cos()
            })
            .collect();
        let gains = [25_000_000u32; 4];

        let mut ltp_state = LtpSynthState::new(bw).unwrap();
        let mut lpc_state = LpcSynthState::new(bw).unwrap();
        let dd = quantize_excitation_frame_del_dec(
            bw,
            SilkFrameSize::TwentyMs,
            SignalType::Voiced,
            QuantizationOffsetType::Low,
            0,
            4,
            &gains,
            &a_q12,
            Some(&ltp),
            &target,
            &PulseRateControl::default(),
            &mut ltp_state,
            &mut lpc_state,
        )
        .unwrap();
        let snr = snr_db(&target, &dd.quantized.reconstructed);
        assert!(snr > 12.0, "voiced dd SNR too low: {snr} dB");

        // Wire roundtrip with the elected seed.
        let ex_cfg = ExcitationConfig {
            bandwidth: bw,
            frame_size: SilkFrameSize::TwentyMs,
            signal_type: SignalType::Voiced,
            qoff_type: QuantizationOffsetType::Low,
            lcg_seed: dd.lcg_seed,
        };
        let mut re = RangeEncoder::new();
        let symbols = ExcitationSymbols {
            rate_level: dd.quantized.rate_level,
            lsb_counts: &dd.quantized.lsb_counts,
            e_raw: &dd.quantized.e_raw,
        };
        let enc = Excitation::encode(&mut re, ex_cfg, &symbols).unwrap();
        let bytes = re.finish();
        let mut rd = RangeDecoder::new(&bytes);
        let dec = Excitation::decode(&mut rd, ex_cfg).unwrap();
        assert_eq!(enc.e_q23(), dec.e_q23());

        // Fresh-state re-synthesis must match the predicted
        // reconstruction AND the carried states.
        let mut ltp2 = LtpSynthState::new(bw).unwrap();
        let mut lpc2 = LpcSynthState::new(bw).unwrap();
        let a_i16: Vec<i16> = a_q12.iter().map(|&c| c as i16).collect();
        ltp2.start_frame();
        let mut out_all = Vec::new();
        for (s, &gain) in gains.iter().enumerate() {
            let cfg = LtpSynthSubframe {
                bandwidth: bw,
                signal_type: SignalType::Voiced,
                frame_size: SilkFrameSize::TwentyMs,
                subframe_index: s as u8,
                gain_q16: gain,
                pitch_lag: 120,
                b_q7: taps,
                ltp_scaling_q14: 15565,
                a_q12: &a_i16,
                lsf_interp_used: false,
            };
            let mut res = vec![0.0f32; n];
            ltp_synthesis_subframe(&ltp2, cfg, &dec.e_q23()[s * n..(s + 1) * n], &mut res).unwrap();
            let mut out = vec![0.0f32; n];
            let lpc_unclamped =
                lpc_synthesis_subframe(bw, &mut lpc2, &res, gain, &a_i16, &mut out).unwrap();
            ltp_synth_commit_subframe(&mut ltp2, &out, &lpc_unclamped).unwrap();
            out_all.extend_from_slice(&out);
        }
        for (a, b) in out_all.iter().zip(dd.quantized.reconstructed.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        for (a, b) in lpc_state.history().iter().zip(lpc2.history().iter()) {
            assert!((a - b).abs() < 1e-6, "carried LPC state mismatch");
        }
        for (a, b) in ltp_state
            .out_history()
            .iter()
            .zip(ltp2.out_history().iter())
        {
            assert!((a - b).abs() < 1e-6, "carried LTP out state mismatch");
        }
    }

    #[test]
    fn rejects_bad_args() {
        let bw = Bandwidth::Nb;
        let n = subframe_samples(bw).unwrap();
        let a_q12 = vec![0i32; 10];
        let target = vec![0.0f32; 4 * n];
        let mut ltp_state = LtpSynthState::new(bw).unwrap();
        let mut lpc_state = LpcSynthState::new(bw).unwrap();
        // Too many states.
        assert!(quantize_excitation_frame_del_dec(
            bw,
            SilkFrameSize::TwentyMs,
            SignalType::Unvoiced,
            QuantizationOffsetType::Low,
            0,
            5,
            &[100_000; 4],
            &a_q12,
            None,
            &target,
            &PulseRateControl::default(),
            &mut ltp_state,
            &mut lpc_state,
        )
        .is_err());
        // Zero states.
        assert!(quantize_excitation_frame_del_dec(
            bw,
            SilkFrameSize::TwentyMs,
            SignalType::Unvoiced,
            QuantizationOffsetType::Low,
            0,
            0,
            &[100_000; 4],
            &a_q12,
            None,
            &target,
            &PulseRateControl::default(),
            &mut ltp_state,
            &mut lpc_state,
        )
        .is_err());
        // Bad base seed.
        assert!(quantize_excitation_frame_del_dec(
            bw,
            SilkFrameSize::TwentyMs,
            SignalType::Unvoiced,
            QuantizationOffsetType::Low,
            4,
            2,
            &[100_000; 4],
            &a_q12,
            None,
            &target,
            &PulseRateControl::default(),
            &mut ltp_state,
            &mut lpc_state,
        )
        .is_err());
    }
}
