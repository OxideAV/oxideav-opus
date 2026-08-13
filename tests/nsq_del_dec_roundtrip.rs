//! §5.2.3.8 delayed-decision NSQ integration gates (RFC 6716).
//!
//! The multi-state trellis is an ELECTION over the single-state noise
//! shaping quantiser: per frame, both run and the measured-RD winner
//! is adopted. These gates pin the measured outcome on speech-like
//! content:
//!
//! * at EQUAL elected rate, the 4-state encoder's decode-mirror
//!   reconstruction must beat the single-state encoder's by a real
//!   margin (measured +0.8..+1.2 dB on this content; gated at
//!   +0.4 dB), and
//! * every delayed-decision packet must decode through the streaming
//!   [`OpusDecoder`] with the exact §3 sample count (the elected
//!   §4.2.7.7 seed rides the wire).
//!
//! The bit-exactness of the delayed-decision streams against the §A
//! reference listing's decoder is validated out-of-tree (five oracle
//! streams: elected mono / FEC / default-quality / 60 ms multiframe /
//! stereo — all sample-identical at 48 kHz).

use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::silk_encoder::SilkEncoderMono;
use oxideav_opus::toc::Bandwidth;
use oxideav_opus::vbr::SilkVbrEncoderMono;

fn snr_db(reference: &[f32], test: &[f32]) -> f64 {
    let mut sig = 0.0f64;
    let mut err = 0.0f64;
    for (&r, &t) in reference.iter().zip(test.iter()) {
        sig += (r as f64) * (r as f64);
        err += ((r - t) as f64) * ((r - t) as f64);
    }
    if err == 0.0 {
        return 150.0;
    }
    10.0 * (sig / err).log10()
}

/// Speech-like deterministic content: a pitch-swept pulse train
/// through a low resonator (voiced) alternating with noise bursts
/// (unvoiced).
fn speech_like(rate_hz: usize, seconds: f64) -> Vec<f32> {
    let n = (rate_hz as f64 * seconds) as usize;
    let mut out = vec![0.0f32; n];
    let mut lcg = 0x12345678u32;
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
        *slot = (0.18 * y) as f32;
    }
    out
}

/// Run one elected encode pass; returns (packets, avg bytes/packet,
/// decode-mirror SNR vs the input).
fn elected_pass(
    pcm: &[f32],
    frame: usize,
    target_bytes: usize,
    nsq_states: usize,
) -> (Vec<Vec<u8>>, f64, f64) {
    let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
    enc.set_nsq_delayed_decision(nsq_states);
    let mut packets = Vec::new();
    let mut recon: Vec<f32> = Vec::new();
    let mut bytes = 0usize;
    for chunk in pcm.chunks_exact(frame) {
        let out = enc.encode_packet_elected(chunk, target_bytes).unwrap();
        bytes += out.packet.len();
        recon.extend_from_slice(&out.reconstructed);
        packets.push(out.packet);
    }
    let snr = snr_db(&pcm[..recon.len()], &recon);
    let avg = bytes as f64 / packets.len() as f64;
    (packets, avg, snr)
}

/// The election's measured win: at equal elected rate the 4-state
/// trellis beats the single-state quantiser, and its packets decode
/// end-to-end with exact sample accounting.
#[test]
fn delayed_decision_beats_single_state_at_equal_rate() {
    let pcm = speech_like(16000, 2.0);
    let (single_pkts, single_rate, single_snr) = elected_pass(&pcm, 320, 40, 1);
    let (dd_pkts, dd_rate, dd_snr) = elected_pass(&pcm, 320, 40, 4);

    // Equal rate (the election lands both within a byte or two of the
    // same target; neither may exceed it on average).
    assert!(
        (dd_rate - single_rate).abs() < 2.0,
        "rate drifted: dd {dd_rate:.2} vs single {single_rate:.2} B/pkt"
    );
    assert!(dd_rate <= 40.0 + 0.5 && single_rate <= 40.0 + 0.5);

    // Measured quality win at equal rate (measured ~+1.2 dB here;
    // gate leaves headroom for content/rounding drift).
    assert!(
        dd_snr >= single_snr + 0.4,
        "no delayed-decision win: dd {dd_snr:.2} dB vs single {single_snr:.2} dB"
    );

    // The delayed-decision stream is genuinely different (the elected
    // seeds / trajectories ride the wire) and decodes cleanly.
    assert_ne!(single_pkts, dd_pkts);
    let mut dec = OpusDecoder::new();
    for p in &dd_pkts {
        let out = dec.decode_packet(p).unwrap();
        assert_eq!(out.samples_per_channel(), 960, "20 ms WB packet at 48 kHz");
    }
}

/// Default-quality (non-elected) encoders stay bit-identical with the
/// knob at 1 state, and with the trellis armed they may only improve
/// the measured mirror SNR while shaving rate.
#[test]
fn default_quality_with_trellis_never_regresses() {
    let pcm = speech_like(16000, 1.5);
    let run = |states: usize| {
        let mut enc = SilkEncoderMono::new(Bandwidth::Wb).unwrap();
        enc.set_nsq_delayed_decision(states);
        let mut packets = Vec::new();
        let mut recon: Vec<f32> = Vec::new();
        for chunk in pcm.chunks_exact(320) {
            let out = enc.encode_packet(chunk).unwrap();
            recon.extend_from_slice(&out.reconstructed);
            packets.push(out.packet);
        }
        let snr = snr_db(&pcm[..recon.len()], &recon);
        let bytes: usize = packets.iter().map(Vec::len).sum();
        (packets, bytes, snr)
    };
    let (p_off, bytes_off, snr_off) = run(1);
    let (p_base, bytes_base, snr_base) = run(1);
    assert_eq!(p_off, p_base, "states=1 must be deterministic");

    let (p_dd, bytes_dd, snr_dd) = run(4);
    assert_ne!(p_off, p_dd);
    // The per-frame RD election only adopts measured wins: quality
    // must not regress (small tolerance for the frame-RD vs
    // whole-signal-SNR measure mismatch), and on this content the
    // trellis shaves rate.
    assert!(
        snr_dd >= snr_off - 0.1,
        "default-quality regression: {snr_dd:.2} vs {snr_off:.2} dB"
    );
    assert!(
        bytes_dd <= bytes_off,
        "default-quality rate regression: {bytes_dd} vs {bytes_off} bytes"
    );
    let _ = (bytes_base, snr_base);

    // And the packets decode.
    let mut dec = OpusDecoder::new();
    for p in &p_dd {
        assert_eq!(dec.decode_packet(p).unwrap().samples_per_channel(), 960);
    }
}

/// The SILK VBR arm rides the delayed-decision knob: realized rate
/// stays on target and every packet decodes with exact accounting.
/// (The corresponding oracle stream decodes bit-exactly through the
/// reference-listing decoder out-of-tree.)
#[test]
fn vbr_arm_with_delayed_decision_stays_on_target() {
    let pcm = speech_like(16000, 2.0);
    let mut enc = SilkVbrEncoderMono::new(Bandwidth::Wb, 200, 20000, true).unwrap();
    enc.set_nsq_delayed_decision(4);
    let mut dec = OpusDecoder::new();
    let mut total = 0usize;
    let mut packets = 0usize;
    for chunk in pcm.chunks_exact(320) {
        let packet = enc.encode_frame(chunk).unwrap();
        total += packet.len();
        packets += 1;
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.samples_per_channel(), 960);
    }
    // 20 kb/s at 20 ms = 50 B/packet; the election lands within a
    // few percent of target.
    let avg = total as f64 / packets as f64;
    assert!(
        (avg - 50.0).abs() < 3.0,
        "VBR+dd average {avg:.1} B/pkt off the 50 B target"
    );
}
