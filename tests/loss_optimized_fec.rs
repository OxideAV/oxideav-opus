//! §2.1.7 loss-optimised LBRR integration gates (RFC 6716).
//!
//! `set_packet_loss_perc` shapes the §4.2.5 redundancy from the
//! expected packet-loss percentage: at low loss (1..=10%) LBRR rides
//! only on "perceptually important" intervals (§2.1.7 names "onsets
//! or transients" as the FEC candidates), freeing elected budget for
//! the primary encoding; at heavy loss the redundancy rate ratio
//! ramps from the 0.5 default toward 0.9, shifting bits toward the
//! copy that will actually be heard. The knob at 0 (default) is
//! bit-identical to the legacy FEC behaviour.
//!
//! Every measured claim below is gated: the onset gate genuinely
//! thins the redundancy and buys primary-path quality at equal
//! elected rate; the high-loss ramp genuinely buys recovery quality;
//! and the expected quality under the declared loss model orders the
//! arms the way the knob promises.

use oxideav_opus::decoder::{FecDecodeStatus, OpusDecoder};
use oxideav_opus::silk_encoder::{SilkEncoderMono, SilkEncoderStereo};
use oxideav_opus::toc::Bandwidth;
use oxideav_opus::vbr::SilkVbrEncoderMono;

/// Speech-like deterministic content: a pitch-swept pulse train
/// through a low resonator (voiced) alternating with noise bursts
/// (unvoiced), shaped by a dip envelope (near-silence for ~0.2 s
/// every 0.9 s) so every post-dip re-entry is a §2.1.7 onset.
fn speech_like(rate_hz: usize, seconds: f64) -> Vec<f32> {
    let n = (rate_hz as f64 * seconds) as usize;
    let mut out = vec![0.0f32; n];
    let mut lcg = 0x1234_5678u32;
    let mut phase = 0.0f64;
    let (mut y1, mut y2) = (0.0f64, 0.0f64);
    for (i, slot) in out.iter_mut().enumerate() {
        let t = i as f64 / rate_hz as f64;
        let voiced = (t / 0.75).fract() < 0.6667;
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = ((lcg >> 16) as f64 - 32768.0) / 32768.0;
        let x = if voiced {
            let f0 = 90.0 + 50.0 * (t * 0.9).sin().abs();
            phase += f0 / rate_hz as f64;
            if phase >= 1.0 {
                phase -= 1.0;
            }
            let pulse = if phase < f0 / rate_hz as f64 * 1.5 {
                1.0
            } else {
                0.0
            };
            pulse + 0.02 * noise
        } else {
            0.25 * noise
        };
        let w = 2.0 * std::f64::consts::PI * 500.0 / rate_hz as f64;
        let r = 0.95;
        let y = x + 2.0 * r * w.cos() * y1 - r * r * y2;
        y2 = y1;
        y1 = y;
        // The onset envelope: a deep dip (below the activity floor)
        // for the last ~22% of each 0.9 s cycle.
        let env = if (t / 0.9).fract() < 0.78 { 1.0 } else { 0.004 };
        *slot = (0.18 * env * y) as f32;
    }
    out
}

/// One arm's whole-story measurement: elected 40 B WB FEC stream at
/// the given loss knob. Returns (clean mirror SNR dB, recovery
/// tracking SNR dB, packets carrying LBRR, avg bytes/packet,
/// packets).
fn run_arm(pcm: &[f32], loss_perc: u8) -> (f64, f64, usize, f64, Vec<Vec<u8>>) {
    let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    enc.set_fec(true);
    enc.set_packet_loss_perc(loss_perc);
    let mut packets = Vec::new();
    let mut recon: Vec<f32> = Vec::new();
    let mut bytes = 0usize;
    for chunk in pcm.chunks_exact(320) {
        let out = enc.encode_packet_elected(chunk, 40).unwrap();
        bytes += out.packet.len();
        recon.extend_from_slice(&out.reconstructed);
        packets.push(out.packet);
    }
    // Steady-state clean measure: skip the two frames straddling
    // each envelope re-entry (attack frames whose gain/LTP
    // misprediction dominates whole-stream error regardless of the
    // redundancy budget under test).
    let (mut csig, mut cerr) = (0.0f64, 0.0f64);
    for (k, chunk) in pcm.chunks_exact(320).enumerate() {
        let ph = ((k * 320) as f64 / 16000.0 / 0.9).fract();
        if !(0.06..0.74).contains(&ph) {
            continue;
        }
        for (&r, &t) in chunk.iter().zip(recon[k * 320..(k + 1) * 320].iter()) {
            csig += f64::from(r) * f64::from(r);
            cerr += (f64::from(r) - f64::from(t)).powi(2);
        }
    }
    let clean_snr = 10.0 * (csig / cerr).log10();
    let avg = bytes as f64 / packets.len() as f64;

    // Clean whole-stream decode for the recovery reference.
    let mut clean = Vec::new();
    let mut dec = OpusDecoder::new();
    for p in &packets {
        clean.push(dec.decode_packet(p).unwrap().pcm);
    }

    // Count LBRR carriers and measure recovery tracking: every packet
    // whose successor carries LBRR is dropped once (independent fresh
    // replays would be O(n^2); the running decoder matches how a
    // receiver rides a lossy channel).
    let mut dec = OpusDecoder::new();
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    let mut carriers = 0usize;
    let mut k = 0usize;
    while k < packets.len() {
        if k % 7 == 6 && k + 1 < packets.len() {
            let fec = dec.decode_packet_fec(&packets[k + 1]).unwrap();
            if fec.status == FecDecodeStatus::Recovered {
                carriers += 1;
                for (&c, &r) in clean[k][240..].iter().zip(fec.pcm[240..].iter()) {
                    sig += f64::from(c) * f64::from(c);
                    err += (f64::from(c) - f64::from(r)).powi(2);
                }
            }
            let _ = dec.decode_packet(&packets[k + 1]).unwrap();
            k += 2;
        } else {
            let _ = dec.decode_packet(&packets[k]).unwrap();
            k += 1;
        }
    }
    let rec_snr = if err > 0.0 {
        10.0 * (sig / err).log10()
    } else {
        f64::NEG_INFINITY
    };
    (clean_snr, rec_snr, carriers, avg, packets)
}

/// Count how many packets in a stream carry LBRR at all (via the FEC
/// decode status on a throwaway decoder).
fn lbrr_carriers(packets: &[Vec<u8>]) -> usize {
    packets
        .iter()
        .filter(|p| {
            OpusDecoder::new().decode_packet_fec(p).unwrap().status == FecDecodeStatus::Recovered
        })
        .count()
}

/// Knob at 0 = legacy FEC, bit-identical.
#[test]
fn loss_knob_at_zero_is_bit_identical_to_legacy_fec() {
    let pcm = speech_like(16000, 1.5);
    let run = |touch: bool| {
        let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
        enc.set_fec(true);
        if touch {
            enc.set_packet_loss_perc(0);
        }
        pcm.chunks_exact(320)
            .map(|c| enc.encode_packet_elected(c, 40).unwrap().packet)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(false), run(true));
}

/// The low-loss onset gate: fewer intervals carry redundancy, the
/// freed elected budget improves the primary path, and the packets
/// that DO carry LBRR still recover.
#[test]
fn onset_gate_thins_redundancy_and_buys_primary_quality() {
    let pcm = speech_like(16000, 3.0);
    let (clean_0, _, _, rate_0, pkts_0) = run_arm(&pcm, 0);
    let (clean_5, _, _, rate_5, pkts_5) = run_arm(&pcm, 5);

    let carriers_0 = lbrr_carriers(&pkts_0);
    let carriers_5 = lbrr_carriers(&pkts_5);
    assert!(
        carriers_5 * 2 <= carriers_0,
        "onset gate did not thin the redundancy: {carriers_5} vs {carriers_0} carriers"
    );
    assert!(carriers_5 > 0, "onsets must still carry LBRR");

    // Equal elected rate; the freed redundancy bits go to the primary
    // encoding (measured on this content: 143 -> 10 carriers and
    // +2.0 dB steady-state clean quality at a slightly LOWER average
    // rate; gated at +0.15 dB).
    assert!(
        (rate_5 - rate_0).abs() < 2.0,
        "rate drifted: {rate_5:.2} vs {rate_0:.2} B/pkt"
    );
    assert!(
        clean_5 >= clean_0 + 0.15,
        "onset gate bought no primary quality: {clean_5:.2} vs {clean_0:.2} dB"
    );

    // A dropped packet whose successor carries onset LBRR recovers.
    let onset_carrier = pkts_5
        .iter()
        .position(|p| {
            OpusDecoder::new().decode_packet_fec(p).unwrap().status == FecDecodeStatus::Recovered
        })
        .expect("an onset carrier exists");
    let mut dec = OpusDecoder::new();
    for p in &pkts_5[..onset_carrier] {
        // Real replay up to the loss (the carrier's predecessor is
        // the "lost" packet).
        let _ = dec.decode_packet(p).unwrap();
    }
    let fec = dec.decode_packet_fec(&pkts_5[onset_carrier]).unwrap();
    assert_eq!(fec.status, FecDecodeStatus::Recovered);
    assert_eq!(fec.pcm.len(), 960);
}

/// The high-loss ramp: a 50% knob spends more of the same elected
/// budget on redundancy — recovery tracking improves by a real
/// margin, and under the declared loss model the expected quality
/// beats the default ratio's.
#[test]
fn high_loss_ramp_buys_recovery_quality() {
    let pcm = speech_like(16000, 3.0);
    let (clean_20, rec_20, car_20, rate_20, _) = run_arm(&pcm, 20);
    let (clean_50, rec_50, car_50, rate_50, _) = run_arm(&pcm, 50);

    // Same coverage (both protect every active interval), same
    // elected rate.
    assert_eq!(car_20, car_50, "coverage must not change on the ramp");
    assert!(
        (rate_50 - rate_20).abs() < 2.0,
        "rate drifted: {rate_50:.2} vs {rate_20:.2} B/pkt"
    );

    // The richer redundancy tracks the clean decode measurably better
    // (measured +1.6 dB on this content, 2.2 vs 0.6; gated at +0.4).
    assert!(
        rec_50 >= rec_20 + 0.4,
        "high-loss ramp bought no recovery quality: {rec_50:.2} vs {rec_20:.2} dB"
    );

    // The ramp must not cost clean quality at the same election (on
    // this content the richer redundancy also fills the election's
    // acceptance window better, so it measures as a small clean WIN;
    // the gate is no-regression), and the expected quality under the
    // declared 50% loss model must favour the knob's own operating
    // point.
    assert!(
        clean_50 >= clean_20 - 0.5,
        "high-loss ramp cost clean quality: {clean_50:.2} vs {clean_20:.2} dB"
    );
    let expected = |clean: f64, rec: f64, p: f64| (1.0 - p) * clean + p * rec;
    assert!(
        expected(clean_50, rec_50, 0.5) > expected(clean_20, rec_20, 0.5),
        "50% knob loses at 50% loss: {:.2} vs {:.2}",
        expected(clean_50, rec_50, 0.5),
        expected(clean_20, rec_20, 0.5)
    );
}

/// The stereo and VBR arms ride the knob: packets decode with exact
/// accounting, the stereo onset gate thins carriers, and the VBR arm
/// stays on target with the reshaped redundancy inside the election.
#[test]
fn stereo_and_vbr_arms_ride_the_loss_knob() {
    let pcm = speech_like(16000, 2.0);

    // Stereo: amplitude-panned copy, onset-gated.
    let run_stereo = |loss: u8| -> (usize, usize) {
        let mut enc = SilkEncoderStereo::new(Bandwidth::Wb).unwrap();
        enc.set_fec(true);
        enc.set_packet_loss_perc(loss);
        let left: Vec<f32> = pcm.iter().map(|&v| v * 0.9).collect();
        let right: Vec<f32> = pcm.iter().map(|&v| v * 0.5).collect();
        let mut dec = OpusDecoder::new();
        let mut carriers = 0usize;
        let mut n = 0usize;
        let chunks: Vec<_> = pcm.chunks_exact(320).collect();
        for (k, _) in chunks.iter().enumerate() {
            let l = &left[k * 320..(k + 1) * 320];
            let r = &right[k * 320..(k + 1) * 320];
            let next = if (k + 1) * 320 < left.len() {
                Some((left[(k + 1) * 320], right[(k + 1) * 320]))
            } else {
                None
            };
            let out = enc.encode_packet(l, r, next).unwrap();
            if OpusDecoder::new()
                .decode_packet_fec(&out.packet)
                .unwrap()
                .status
                == FecDecodeStatus::Recovered
            {
                carriers += 1;
            }
            let d = dec.decode_packet(&out.packet).unwrap();
            assert_eq!(d.samples_per_channel(), 960);
            assert_eq!(d.channels, 2);
            n += 1;
        }
        (carriers, n)
    };
    let (car_all, _) = run_stereo(0);
    let (car_onset, n) = run_stereo(5);
    assert!(n > 90);
    assert!(
        car_onset * 2 <= car_all && car_onset > 0,
        "stereo onset gate: {car_onset} vs {car_all} carriers"
    );

    // VBR arm: on-target average with the knob at 30%.
    let mut enc = SilkVbrEncoderMono::new(Bandwidth::Wb, 200, 20000, true).unwrap();
    enc.set_fec(true);
    enc.set_packet_loss_perc(30);
    let mut dec = OpusDecoder::new();
    let mut total = 0usize;
    let mut count = 0usize;
    for chunk in pcm.chunks_exact(320) {
        let packet = enc.encode_frame(chunk).unwrap();
        total += packet.len();
        count += 1;
        assert_eq!(
            dec.decode_packet(&packet).unwrap().samples_per_channel(),
            960
        );
    }
    // The dip envelope's inactive stretches collapse below target
    // (constrained VBR never spends unbanked bits), so the average
    // sits under the 50 B/pkt target but must stay in its
    // neighborhood and never above it.
    let avg = total as f64 / count as f64;
    assert!(
        avg <= 50.0 + 2.0 && avg > 38.0,
        "VBR + loss knob average {avg:.1} B/pkt out of range"
    );
}

/// r445: Hybrid packets carry LBRR too — the WB SILK layer's §4.2.5
/// redundancy rides in front of the regular SILK frame on the shared
/// range coder, the stream still decodes end-to-end, and a dropped
/// packet's SILK band recovers through `decode_packet_fec` (the
/// recovery is the 0–8 kHz LP layer per §2.1.7). The loss knob's
/// onset gate thins hybrid carriers exactly like the SILK-only path.
#[test]
fn hybrid_fec_emits_recovers_and_takes_the_loss_knob() {
    use oxideav_opus::hybrid_packet_encode::HybridEncoderMono;

    // 48 kHz speech-like content (dip envelope for onsets).
    let f32pcm = speech_like(48000, 3.0);
    let pcm: Vec<i16> = f32pcm
        .iter()
        .map(|&v| (v * 24000.0).clamp(-30000.0, 30000.0) as i16)
        .collect();

    let run = |fec: bool, loss: u8| -> (usize, Vec<Vec<u8>>) {
        let mut enc = HybridEncoderMono::new(Bandwidth::Fb, 200).unwrap();
        enc.set_fec(fec);
        enc.set_packet_loss_perc(loss);
        let mut dec = OpusDecoder::new();
        let mut carriers = 0usize;
        let mut packets = Vec::new();
        for (ki, chunk) in pcm.chunks_exact(960).enumerate() {
            let packet = enc
                .encode_packet_elected(chunk, 120)
                .unwrap_or_else(|e| panic!("packet {ki} (fec {fec} loss {loss}): {e:?}"));
            if OpusDecoder::new()
                .decode_packet_fec(&packet)
                .unwrap()
                .status
                == FecDecodeStatus::Recovered
            {
                carriers += 1;
            }
            // The whole stream (LBRR included) must decode normally.
            let out = dec.decode_packet(&packet).unwrap();
            assert_eq!(out.samples_per_channel(), 960);
            packets.push(packet);
        }
        (carriers, packets)
    };

    // FEC off: no packet carries LBRR (and the encoder is unchanged).
    let (car_off, _) = run(false, 0);
    assert_eq!(car_off, 0);

    // FEC on: active intervals carry LBRR; the onset gate thins them.
    let (car_on, pkts_on) = run(true, 0);
    assert!(car_on > 50, "hybrid LBRR must ride: {car_on} carriers");
    let (car_onset, _) = run(true, 5);
    assert!(
        car_onset * 2 <= car_on && car_onset > 0,
        "hybrid onset gate: {car_onset} vs {car_on} carriers"
    );

    // A real loss: drop packet k, recover its SILK band from packet
    // k+1, and compare the recovery against the clean decode's low
    // band energy-wise (the recovery is only the LP layer, so exact
    // waveform SNR does not apply — gate that it is real audio, not
    // silence, with the exact sample count).
    let k = 30usize;
    let mut dec = OpusDecoder::new();
    for p in &pkts_on[..k] {
        let _ = dec.decode_packet(p).unwrap();
    }
    let fec = dec.decode_packet_fec(&pkts_on[k + 1]).unwrap();
    assert_eq!(fec.status, FecDecodeStatus::Recovered);
    assert_eq!(fec.pcm.len(), 960);
    let energy: f64 = fec.pcm.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    assert!(energy > 1.0e6, "recovered SILK band is silence: {energy}");
    // And the stream continues after the recovery.
    let out = dec.decode_packet(&pkts_on[k + 1]).unwrap();
    assert_eq!(out.samples_per_channel(), 960);
}

/// r445: the stereo Hybrid arm's LBRR — mid/side redundancy frames
/// ride in front of the regular stereo walk (mid LBRR carries the
/// §4.2.7.1 weights and the gated §4.2.7.2 flag), the stream decodes
/// end-to-end, and a dropped packet recovers two-channel SILK-band
/// audio through `decode_packet_fec`.
#[test]
fn stereo_hybrid_fec_emits_and_recovers() {
    use oxideav_opus::hybrid_packet_encode::HybridEncoderStereo;

    let f32pcm = speech_like(48000, 2.0);
    let pcm: Vec<i16> = f32pcm
        .iter()
        .flat_map(|&v| {
            let l = (v * 24000.0).clamp(-30000.0, 30000.0) as i16;
            let r = (v * 12000.0).clamp(-30000.0, 30000.0) as i16;
            [l, r]
        })
        .collect();

    let mut enc = HybridEncoderStereo::new(Bandwidth::Fb, 200).unwrap();
    enc.set_fec(true);
    let mut dec = OpusDecoder::new();
    let mut carriers = 0usize;
    let mut packets = Vec::new();
    for chunk in pcm.chunks_exact(2 * 960) {
        let packet = enc.encode_packet_elected(chunk, 200).unwrap();
        if OpusDecoder::new()
            .decode_packet_fec(&packet)
            .unwrap()
            .status
            == FecDecodeStatus::Recovered
        {
            carriers += 1;
        }
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.samples_per_channel(), 960);
        assert_eq!(out.channels, 2);
        packets.push(packet);
    }
    assert!(carriers > 30, "stereo hybrid LBRR must ride: {carriers}");

    // A real two-channel recovery.
    let k = 25usize;
    let mut dec = OpusDecoder::new();
    for p in &packets[..k] {
        let _ = dec.decode_packet(p).unwrap();
    }
    let fec = dec.decode_packet_fec(&packets[k + 1]).unwrap();
    assert_eq!(fec.status, FecDecodeStatus::Recovered);
    assert_eq!(fec.channels, 2);
    assert_eq!(fec.pcm.len(), 2 * 960);
    // Both channels carry real audio with the panned imbalance
    // preserved (L was encoded twice as loud as R).
    let (mut el, mut er) = (0.0f64, 0.0f64);
    for pair in fec.pcm.chunks_exact(2) {
        el += f64::from(pair[0]) * f64::from(pair[0]);
        er += f64::from(pair[1]) * f64::from(pair[1]);
    }
    assert!(el > 1.0e6 && er > 1.0e5, "recovered channels: {el} / {er}");
    assert!(el > er, "panning must survive the recovery");
    let out = dec.decode_packet(&packets[k + 1]).unwrap();
    assert_eq!(out.samples_per_channel(), 960);
}
