//! SILK-only Opus **packet** encoding — RFC 6716 §3.1 / §4.2.2-§4.2.6.
//!
//! Composes the encode-side mirrors into a complete, decoder-ready
//! Opus packet: the §3.1 TOC byte (code 0, one Opus frame), the
//! §4.2.3 / §4.2.4 header bits, and one regular SILK frame per 20 ms
//! time interval (or a single 10 ms frame) written in Table-5 order by
//! [`crate::silk_decode::encode_silk_frame`], finalized through the
//! §5.1.5 range-coder termination.
//!
//! The produced packets decode end-to-end through
//! [`crate::decoder::OpusDecoder::decode_packet`] on a fresh decoder:
//! the per-frame carried state (previous gain / lag / NLSF) is
//! threaded here exactly the way the packet decoder threads it, so
//! the per-frame `SilkFrameDecoded` predictions returned to the
//! caller equal what the decoder reconstructs.
//!
//! Scope: **mono**. LBRR (in-band FEC, §4.2.5) emission is supported
//! via [`encode_silk_only_packet_mono_with_lbrr`]: LBRR frames are
//! written ahead of the regular frames with their own independent
//! carried state, exactly mirroring the decode-side LBRR walk, and the
//! §4.2.3 / §4.2.4 LBRR flags are derived from which intervals carry a
//! redundancy script. The stereo mid/side interleave (§4.2.2) reuses
//! the same per-frame encoder and lands on top of this entry.

use crate::range_encoder::RangeEncoder;
use crate::silk_decode::{encode_silk_frame, SilkFrameConfig, SilkFrameDecoded, SilkFrameSymbols};
use crate::silk_excitation::SilkFrameSize;
use crate::silk_header::{silk_frame_count, PerFrameLbrr, SilkChannelHeader, SilkHeaderBits};
use crate::toc::{Bandwidth, FrameCountCode, Mode, OpusTocByte};
use crate::Error;

/// Encode one **mono, SILK-only, code-0** Opus packet from per-frame
/// symbol scripts.
///
/// * `bandwidth` — NB / MB / WB (the SILK internal bandwidths).
/// * `frame_size_tenths_ms` — the §3.1 Opus frame duration: 100, 200,
///   400, or 600 (10/20/40/60 ms). A 10 ms packet carries one 10 ms
///   SILK frame; the others carry one 20 ms SILK frame per interval
///   (1, 2, or 3), and `frames.len()` must match.
/// * `frames` — one Table-5 symbol script per SILK frame. Each frame's
///   §4.2.3 VAD bit is derived from its §4.2.7.3 frame type (types
///   `0..=1` are inactive, `2..=5` active). Frames after the first
///   must delta-code their first gain (the carried state makes the
///   first subframe non-independent), exactly as the decoder expects.
///
/// Returns the packet bytes plus the per-frame [`SilkFrameDecoded`]
/// predictions (what a fresh decoder will reconstruct).
pub fn encode_silk_only_packet_mono(
    bandwidth: Bandwidth,
    frame_size_tenths_ms: u16,
    frames: &[SilkFrameSymbols<'_>],
) -> Result<(Vec<u8>, Vec<SilkFrameDecoded>), Error> {
    let n = silk_frame_count(frame_size_tenths_ms).ok_or(Error::MalformedPacket)? as usize;
    let no_lbrr = vec![None; n];
    let (packet, regular, _) =
        encode_silk_only_packet_mono_with_lbrr(bandwidth, frame_size_tenths_ms, frames, &no_lbrr)?;
    Ok((packet, regular))
}

/// [`encode_silk_only_packet_mono`] with §4.2.5 LBRR (in-band FEC)
/// emission: `lbrr[idx]` optionally carries a redundancy script for
/// SILK frame `idx`'s time interval (a re-encode of the interval the
/// *previous* packet covered, per §2.1.7). LBRR frames are written
/// ahead of the regular frames with their own independent carried
/// state, exactly the way the packet decoder walks them; each LBRR
/// script's frame type must be active (`2..=5`, §4.2.7.3), its first
/// gain independent only on the first coded LBRR frame, and its LTP
/// scaling present only on the first coded LBRR frame.
///
/// Returns the packet bytes, the regular per-frame predictions, and
/// the LBRR per-frame predictions.
#[allow(clippy::type_complexity)]
pub fn encode_silk_only_packet_mono_with_lbrr(
    bandwidth: Bandwidth,
    frame_size_tenths_ms: u16,
    frames: &[SilkFrameSymbols<'_>],
    lbrr: &[Option<SilkFrameSymbols<'_>>],
) -> Result<
    (
        Vec<u8>,
        Vec<SilkFrameDecoded>,
        Vec<Option<SilkFrameDecoded>>,
    ),
    Error,
> {
    let num_silk_frames = silk_frame_count(frame_size_tenths_ms).ok_or(Error::MalformedPacket)?;
    if frames.len() != num_silk_frames as usize || lbrr.len() != num_silk_frames as usize {
        return Err(Error::MalformedPacket);
    }
    let frame_size = if frame_size_tenths_ms == 100 {
        SilkFrameSize::TenMs
    } else {
        SilkFrameSize::TwentyMs
    };

    // §3.1 TOC byte: SILK-only, mono, code 0 (one Opus frame).
    let toc = OpusTocByte::compose_byte(
        Mode::SilkOnly,
        bandwidth,
        frame_size_tenths_ms,
        false,
        FrameCountCode::One,
    )?;

    let mut re = RangeEncoder::new();

    // §4.2.3 / §4.2.4 header bits: VAD per frame from the frame type,
    // LBRR flags from which intervals carry a redundancy script.
    let mut vad_flags = 0u8;
    for (idx, f) in frames.iter().enumerate() {
        if f.header.frame_type >= 2 {
            vad_flags |= 1 << idx;
        }
    }
    let mut lbrr_bits = 0u8;
    for (idx, l) in lbrr.iter().enumerate() {
        if l.is_some() {
            lbrr_bits |= 1 << idx;
        }
    }
    let header = SilkHeaderBits {
        num_silk_frames,
        mid: SilkChannelHeader {
            vad_flags,
            lbrr_flag: lbrr_bits != 0,
        },
        side: None,
        per_frame_lbrr: PerFrameLbrr {
            mid: lbrr_bits,
            side: 0,
        },
    };
    header.encode(&mut re)?;

    // §4.2.5 LBRR frames: written ahead of the regular frames, with
    // their own independent carried state (they form their own
    // sequence), mirroring the decoder's LBRR walk. Per §4.2.7.3 every
    // LBRR frame is active-coded.
    let mut lbrr_prev_gain: Option<u8> = None;
    let mut lbrr_prev_lag: Option<i32> = None;
    let mut lbrr_first = true;
    let mut lbrr_predictions: Vec<Option<SilkFrameDecoded>> = Vec::with_capacity(lbrr.len());
    for entry in lbrr.iter() {
        let Some(symbols) = entry else {
            lbrr_predictions.push(None);
            continue;
        };
        if symbols.header.frame_type < 2 {
            // §4.2.7.3: LBRR frames use the active PDF.
            return Err(Error::MalformedPacket);
        }
        let cfg = SilkFrameConfig {
            bandwidth,
            frame_size,
            voice_active: true,
            first_subframe_independent: lbrr_first || lbrr_prev_gain.is_none(),
            previous_log_gain: lbrr_prev_gain,
            previous_primary_lag: lbrr_prev_lag,
            ltp_scaling_present: lbrr_first,
            lsf_interp_after_reset: lbrr_first,
            previous_nlsf_q15: None,
            previous_nlsf_len: 0,
            stereo: None,
        };
        let decoded = encode_silk_frame(&mut re, cfg, symbols)?;
        lbrr_prev_gain = Some(decoded.gains.last_log_gain());
        lbrr_prev_lag = Some(decoded.ltp.primary_lag());
        lbrr_first = false;
        lbrr_predictions.push(Some(decoded));
    }

    // §4.2.6 regular SILK frames with the carried state threaded the
    // same way the packet decoder threads it (fresh-decoder start).
    let mut prev_gain: Option<u8> = None;
    let mut prev_lag: Option<i32> = None;
    let mut prev_nlsf: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]> = None;
    let mut prev_nlsf_len = 0usize;
    let mut first = true;
    let mut predictions = Vec::with_capacity(frames.len());
    for (idx, symbols) in frames.iter().enumerate() {
        let cfg = SilkFrameConfig {
            bandwidth,
            frame_size,
            voice_active: (vad_flags >> idx) & 1 == 1,
            first_subframe_independent: first || prev_gain.is_none(),
            previous_log_gain: prev_gain,
            previous_primary_lag: prev_lag,
            ltp_scaling_present: first,
            lsf_interp_after_reset: first || prev_nlsf.is_none(),
            previous_nlsf_q15: prev_nlsf,
            previous_nlsf_len: prev_nlsf_len,
            stereo: None,
        };
        let decoded = encode_silk_frame(&mut re, cfg, symbols)?;
        prev_gain = Some(decoded.gains.last_log_gain());
        prev_lag = Some(decoded.ltp.primary_lag());
        prev_nlsf = Some(decoded.nlsf_q15);
        prev_nlsf_len = decoded.d_lpc;
        first = false;
        predictions.push(decoded);
    }

    // §5.1.5 finalize; §3.2 code-0 framing = TOC byte + the single
    // compressed frame.
    let body = re.finish();
    let mut packet = Vec::with_capacity(1 + body.len());
    packet.push(toc);
    packet.extend_from_slice(&body);
    Ok((packet, predictions, lbrr_predictions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::{FrameDecodeStatus, OpusDecoder};
    use crate::range_decoder::RangeDecoder;
    use crate::silk_decode::decode_silk_frame;
    use crate::silk_excitation::{shell_block_count, ExcitationSymbols, SHELL_BLOCK_SAMPLES};
    use crate::silk_frame::SilkHeaderSymbols;
    use crate::silk_gains::GainSymbol;
    use crate::silk_ltp::{LagSymbols, LtpSymbols, LTP_MAX_SUBFRAMES};

    /// A tiny deterministic LCG for the packet-level sweeps.
    struct Lcg(u64);
    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
        fn below(&mut self, n: u32) -> u32 {
            self.next_u32() % n
        }
    }

    /// Owns the per-frame script buffers (the `SilkFrameSymbols` borrow
    /// slices).
    struct ScriptBufs {
        gains: Vec<GainSymbol>,
        i2: Vec<i8>,
        lsb_counts: Vec<u8>,
        e_raw: Vec<i32>,
        header: SilkHeaderSymbols,
        lsf_stage1: u8,
        lsf_interp_w_q2: Option<u8>,
        ltp: Option<LtpSymbols>,
        lcg_seed: u8,
        rate_level: u8,
    }

    fn random_frame_script(
        rng: &mut Lcg,
        bandwidth: Bandwidth,
        frame_size: SilkFrameSize,
        first: bool,
        has_prev_lag: bool,
    ) -> ScriptBufs {
        let num_subframes = if frame_size == SilkFrameSize::TenMs {
            2usize
        } else {
            4
        };
        let frame_type = rng.below(6) as u8;
        let voiced = frame_type >= 4;
        let gains: Vec<GainSymbol> = (0..num_subframes)
            .map(|k| {
                if k == 0 && first {
                    GainSymbol::Independent(rng.below(64) as u8)
                } else {
                    GainSymbol::Delta(rng.below(41) as u8)
                }
            })
            .collect();
        let d_lpc = if bandwidth == Bandwidth::Wb { 16 } else { 10 };
        let i2: Vec<i8> = (0..d_lpc).map(|_| rng.below(21) as i8 - 10).collect();
        let ltp = voiced.then(|| {
            let lag_low_count = match bandwidth {
                Bandwidth::Nb => 4u32,
                Bandwidth::Mb => 6,
                _ => 8,
            };
            let lag = if has_prev_lag {
                if rng.below(2) == 0 {
                    LagSymbols::RelativeDelta {
                        delta_index: 1 + rng.below(20) as u8,
                    }
                } else {
                    LagSymbols::RelativeFallback {
                        lag_high: rng.below(32) as u8,
                        lag_low: rng.below(lag_low_count) as u8,
                    }
                }
            } else {
                LagSymbols::Absolute {
                    lag_high: rng.below(32) as u8,
                    lag_low: rng.below(lag_low_count) as u8,
                }
            };
            let contour_cells = match (bandwidth, num_subframes) {
                (Bandwidth::Nb, 2) => 3u32,
                (Bandwidth::Nb, 4) => 11,
                (_, 2) => 12,
                _ => 34,
            };
            let periodicity_index = rng.below(3) as u8;
            let filter_cells = [8u32, 16, 32][periodicity_index as usize];
            let mut filter_indices = [0u8; LTP_MAX_SUBFRAMES];
            for f in filter_indices.iter_mut().take(num_subframes) {
                *f = rng.below(filter_cells) as u8;
            }
            LtpSymbols {
                lag,
                contour_index: rng.below(contour_cells) as u8,
                periodicity_index,
                filter_indices,
                // ltp_scaling_present == `first` in the packet path.
                ltp_scaling_index: first.then(|| rng.below(3) as u8),
            }
        });
        let blocks = shell_block_count(bandwidth, frame_size).unwrap();
        let total = blocks * SHELL_BLOCK_SAMPLES;
        let mut lsb_counts = vec![0u8; blocks];
        let mut e_raw = vec![0i32; total];
        for (b, lc) in lsb_counts.iter_mut().enumerate() {
            let lsbs = if rng.below(4) == 0 { 1 } else { 0 };
            *lc = lsbs as u8;
            let budget = rng.below(17);
            let base = b * SHELL_BLOCK_SAMPLES;
            let mut spent = 0u32;
            while spent < budget {
                let i = base + rng.below(16) as usize;
                let add = 1 + rng.below(budget - spent);
                e_raw[i] += (add << lsbs) as i32;
                spent += add;
            }
            for slot in e_raw[base..base + SHELL_BLOCK_SAMPLES].iter_mut() {
                if lsbs > 0 {
                    *slot += (rng.next_u32() & 1) as i32;
                }
                if *slot != 0 && rng.below(2) == 0 {
                    *slot = -*slot;
                }
            }
        }
        ScriptBufs {
            gains,
            i2,
            lsb_counts,
            e_raw,
            header: SilkHeaderSymbols {
                stereo: None,
                mid_only_flag: None,
                frame_type,
            },
            lsf_stage1: rng.below(32) as u8,
            lsf_interp_w_q2: (frame_size == SilkFrameSize::TwentyMs).then(|| rng.below(5) as u8),
            ltp,
            lcg_seed: rng.below(4) as u8,
            rate_level: rng.below(9) as u8,
        }
    }

    fn symbols_of(bufs: &ScriptBufs) -> SilkFrameSymbols<'_> {
        SilkFrameSymbols {
            header: bufs.header,
            gains: &bufs.gains,
            lsf_stage1: bufs.lsf_stage1,
            lsf_stage2_i2: &bufs.i2,
            lsf_interp_w_q2: bufs.lsf_interp_w_q2,
            ltp: bufs.ltp,
            lcg_seed: bufs.lcg_seed,
            excitation: ExcitationSymbols {
                rate_level: bufs.rate_level,
                lsb_counts: &bufs.lsb_counts,
                e_raw: &bufs.e_raw,
            },
        }
    }

    /// End-to-end: random mono SILK-only packets (10/20/40/60 ms, all
    /// bandwidths) produced by the packet encoder decode through a
    /// fresh `OpusDecoder::decode_packet` with a real-SILK-PCM status
    /// and the exact §3 sample count, and the frame-level symbols
    /// decode back (via a parallel `decode_silk_frame` walk) equal to
    /// the encoder's predictions.
    #[test]
    fn packet_encode_decodes_end_to_end() {
        let mut rng = Lcg(0x0AC4_E701);
        for round in 0..120 {
            let bandwidth = match rng.below(3) {
                0 => Bandwidth::Nb,
                1 => Bandwidth::Mb,
                _ => Bandwidth::Wb,
            };
            let fs_tenths: u16 = [100u16, 200, 400, 600][rng.below(4) as usize];
            let frame_size = if fs_tenths == 100 {
                SilkFrameSize::TenMs
            } else {
                SilkFrameSize::TwentyMs
            };
            let n = silk_frame_count(fs_tenths).unwrap() as usize;
            let bufs: Vec<ScriptBufs> = (0..n)
                .map(|idx| random_frame_script(&mut rng, bandwidth, frame_size, idx == 0, idx > 0))
                .collect();
            let scripts: Vec<SilkFrameSymbols<'_>> = bufs.iter().map(symbols_of).collect();

            let (packet, predictions) =
                encode_silk_only_packet_mono(bandwidth, fs_tenths, &scripts)
                    .expect("packet encode");
            assert_eq!(predictions.len(), n);

            // The packet decodes end-to-end on a fresh decoder.
            let mut dec = OpusDecoder::new();
            let out = dec.decode_packet(&packet).expect("packet decode");
            assert_eq!(out.channels, 1, "round {round}");
            assert_eq!(out.frame_outcomes.len(), 1, "round {round}");
            assert_eq!(
                out.frame_outcomes[0].status,
                FrameDecodeStatus::SilkParamsDecoded,
                "round {round} bw={bandwidth:?} fs={fs_tenths}"
            );
            // §3: 48 kHz output samples for the Opus frame duration.
            assert_eq!(
                out.samples_per_channel() as u32,
                48_000 * fs_tenths as u32 / 10_000,
                "round {round}"
            );

            // Frame-level: a parallel Table-5 walk over the packet body
            // reconstructs exactly the encoder's predictions.
            let mut rd = RangeDecoder::new(&packet[1..]);
            let header = SilkHeaderBits::decode(&mut rd, n as u8, false).expect("header bits");
            assert!(!header.mid.lbrr_flag);
            let mut prev_gain: Option<u8> = None;
            let mut prev_lag: Option<i32> = None;
            let mut prev_nlsf: Option<[i16; crate::silk_lsf_stage2::D_LPC_MAX]> = None;
            let mut prev_nlsf_len = 0usize;
            let mut first = true;
            for (idx, expected) in predictions.iter().enumerate() {
                let cfg = SilkFrameConfig {
                    bandwidth,
                    frame_size,
                    voice_active: header.mid_vad(idx as u8),
                    first_subframe_independent: first || prev_gain.is_none(),
                    previous_log_gain: prev_gain,
                    previous_primary_lag: prev_lag,
                    ltp_scaling_present: first,
                    lsf_interp_after_reset: first || prev_nlsf.is_none(),
                    previous_nlsf_q15: prev_nlsf,
                    previous_nlsf_len: prev_nlsf_len,
                    stereo: None,
                };
                let decoded = decode_silk_frame(&mut rd, cfg).expect("frame decode");
                assert_eq!(&decoded, expected, "round {round} frame {idx}");
                prev_gain = Some(decoded.gains.last_log_gain());
                prev_lag = Some(decoded.ltp.primary_lag());
                prev_nlsf = Some(decoded.nlsf_q15);
                prev_nlsf_len = decoded.d_lpc;
                first = false;
            }
            assert!(!rd.has_error());
        }
    }

    /// LBRR emission closes the FEC loop: a packet carrying §4.2.5
    /// redundancy scripts still decodes its regular frames end-to-end
    /// (the LBRR bits shift every later symbol, so this pins the
    /// range-coder alignment), `decode_packet_fec` recovers real audio
    /// (`Recovered`), and a no-LBRR packet reports `NoLbrr`.
    #[test]
    fn packet_encode_with_lbrr_fec_roundtrip() {
        use crate::decoder::FecDecodeStatus;
        let mut rng = Lcg(0xFEC0_0382);
        for round in 0..60 {
            let bandwidth = match rng.below(3) {
                0 => Bandwidth::Nb,
                1 => Bandwidth::Mb,
                _ => Bandwidth::Wb,
            };
            let fs_tenths: u16 = [100u16, 200, 400, 600][rng.below(4) as usize];
            let frame_size = if fs_tenths == 100 {
                SilkFrameSize::TenMs
            } else {
                SilkFrameSize::TwentyMs
            };
            let n = silk_frame_count(fs_tenths).unwrap() as usize;
            let bufs: Vec<ScriptBufs> = (0..n)
                .map(|idx| random_frame_script(&mut rng, bandwidth, frame_size, idx == 0, idx > 0))
                .collect();
            let scripts: Vec<SilkFrameSymbols<'_>> = bufs.iter().map(symbols_of).collect();

            // Random non-empty LBRR subset. LBRR scripts must be
            // active-coded and follow the LBRR carried-state rules, so
            // build them with the same generator but force an active
            // frame type and first/subsequent shape per coded order.
            let mut which = vec![false; n];
            which[rng.below(n as u32) as usize] = true;
            for w in which.iter_mut() {
                if rng.below(2) == 0 {
                    *w = true;
                }
            }
            let mut lbrr_bufs: Vec<Option<ScriptBufs>> = Vec::with_capacity(n);
            let mut coded_first = true;
            for &w in &which {
                if !w {
                    lbrr_bufs.push(None);
                    continue;
                }
                let mut b =
                    random_frame_script(&mut rng, bandwidth, frame_size, coded_first, !coded_first);
                // Force an active frame type (§4.2.7.3: LBRR is
                // active-coded) while keeping the generator's LTP shape
                // consistent with it.
                if b.header.frame_type < 2 {
                    b.header.frame_type += 2; // 0/1 -> 2/3 (unvoiced, no LTP)
                }
                lbrr_bufs.push(Some(b));
                coded_first = false;
            }
            let lbrr_scripts: Vec<Option<SilkFrameSymbols<'_>>> = lbrr_bufs
                .iter()
                .map(|b| b.as_ref().map(symbols_of))
                .collect();

            let (packet, regular, lbrr_pred) = encode_silk_only_packet_mono_with_lbrr(
                bandwidth,
                fs_tenths,
                &scripts,
                &lbrr_scripts,
            )
            .expect("packet encode with lbrr");
            assert_eq!(regular.len(), n);
            assert_eq!(
                lbrr_pred.iter().filter(|p| p.is_some()).count(),
                which.iter().filter(|&&w| w).count()
            );

            // Regular decode still lands (range-coder alignment past
            // the LBRR frames).
            let mut dec = OpusDecoder::new();
            let out = dec.decode_packet(&packet).expect("packet decode");
            assert_eq!(
                out.frame_outcomes[0].status,
                FrameDecodeStatus::SilkParamsDecoded,
                "round {round} bw={bandwidth:?} fs={fs_tenths}"
            );

            // FEC recovery from the LBRR we emitted.
            let mut fec_dec = OpusDecoder::new();
            let rec = fec_dec.decode_packet_fec(&packet).expect("fec decode");
            assert_eq!(
                rec.status,
                FecDecodeStatus::Recovered,
                "round {round} bw={bandwidth:?} fs={fs_tenths}"
            );
            assert_eq!(
                rec.pcm.len() as u32,
                48_000 * fs_tenths as u32 / 10_000,
                "round {round}"
            );
        }

        // A no-LBRR packet reports NoLbrr.
        let bufs = random_frame_script(
            &mut rng,
            Bandwidth::Nb,
            SilkFrameSize::TwentyMs,
            true,
            false,
        );
        let script = symbols_of(&bufs);
        let (packet, _) =
            encode_silk_only_packet_mono(Bandwidth::Nb, 200, &[script]).expect("encode");
        let mut dec = OpusDecoder::new();
        let rec = dec.decode_packet_fec(&packet).expect("fec decode");
        assert_eq!(rec.status, FecDecodeStatus::NoLbrr);
    }

    /// Frame-count / duration mismatches are rejected.
    #[test]
    fn packet_encode_rejects_bad_shape() {
        let mut rng = Lcg(7);
        let bufs = random_frame_script(
            &mut rng,
            Bandwidth::Nb,
            SilkFrameSize::TwentyMs,
            true,
            false,
        );
        let script = symbols_of(&bufs);
        // 40 ms needs 2 frames.
        assert!(encode_silk_only_packet_mono(Bandwidth::Nb, 400, &[script]).is_err());
        // 2.5 ms is not a SILK duration.
        let script = symbols_of(&bufs);
        assert!(encode_silk_only_packet_mono(Bandwidth::Nb, 25, &[script]).is_err());
        // SWB is not a SILK bandwidth.
        let script = symbols_of(&bufs);
        assert!(encode_silk_only_packet_mono(Bandwidth::Swb, 200, &[script]).is_err());
    }
}
