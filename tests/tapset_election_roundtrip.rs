//! §5.3.1 tapset election integration gates (RFC 6716 §4.3.7.1).
//!
//! The CELT encoder's post-filter tapset is ELECTED by measured
//! rate/quality: on every pre-filter-firing frame, the frame is
//! trial-encoded per tapset at the same payload size, each trial is
//! decoded through a clone of a lockstep mirror decoder, and the
//! measured-SNR winner is committed. These gates pin the measured
//! outcome on strongly periodic content (short pitch period, where
//! the sharper tapsets demonstrably win):
//!
//! * the elected stream must beat the previous hardwired tapset-0
//!   encoder by a real margin at equal rate (measured ~+1.6 dB on
//!   this content at 40 B; gated at +0.8 dB),
//! * nonzero tapsets must actually be coded (the election engages),
//!   and
//! * with the election disabled the encoder stays bit-identical to
//!   the tapset-0 encoder.
//!
//! The reference-listing agreement of a tapset-elected stream is
//! validated out-of-tree (FB 20 ms mono, 103 dB / max 1 LSB against
//! the §A reference decoder — the float-noise corridor).

use oxideav_opus::celt_packet_encode::CeltEncoder;
use oxideav_opus::decoder::OpusDecoder;
use oxideav_opus::toc::Bandwidth;

/// Strongly periodic 48 kHz content: a 60-sample pulse train through
/// a low resonator (rich harmonic comb up to high frequencies).
fn periodic_48k(seconds: f64) -> Vec<i16> {
    let n = (48000.0 * seconds) as usize;
    let mut out = vec![0i16; n];
    let mut phase = 0.0f64;
    let (mut y1, mut y2) = (0.0f64, 0.0f64);
    for slot in out.iter_mut() {
        let f0 = 48000.0 / 60.0;
        phase += f0 / 48000.0;
        if phase >= 1.0 {
            phase -= 1.0;
        }
        let x = if phase < f0 / 48000.0 * 2.0 { 1.0 } else { 0.0 };
        let w = 2.0 * std::f64::consts::PI * 800.0 / 48000.0;
        let r = 0.92;
        let y = x + 2.0 * r * w.cos() * y1 - r * r * y2;
        y2 = y1;
        y1 = y;
        *slot = (2200.0 * y).clamp(-30000.0, 30000.0) as i16;
    }
    out
}

/// Stream one arm; returns (delay-aligned decode SNR dB, pf frames,
/// per-tapset coded counts, packets).
fn run_arm(pcm: &[i16], payload: usize, arm: &str) -> (f64, usize, [usize; 3], Vec<Vec<u8>>) {
    let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
    match arm {
        "elect" => enc.set_tapset_election(true),
        "t0" => enc.set_tapset(0),
        "t1" => enc.set_tapset(1),
        "t2" => enc.set_tapset(2),
        _ => unreachable!(),
    }
    let mut dec = OpusDecoder::new();
    let mut hist = vec![0i16; 120];
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    let mut pf_frames = 0usize;
    let mut tapsets = [0usize; 3];
    let mut packets = Vec::new();
    for chunk in pcm.chunks_exact(960) {
        let (packet, info) = enc.encode_packet(chunk, payload).unwrap();
        if info.postfilter_on {
            pf_frames += 1;
            tapsets[usize::from(info.postfilter_tapset)] += 1;
        }
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.samples_per_channel(), 960);
        let mut reference = hist.clone();
        reference.extend_from_slice(&chunk[..960 - 120]);
        for (&r, &t) in reference.iter().zip(out.pcm.iter()) {
            sig += f64::from(r) * f64::from(r);
            err += f64::from(r - t) * f64::from(r - t);
        }
        hist.copy_from_slice(&chunk[960 - 120..]);
        packets.push(packet);
    }
    (10.0 * (sig / err).log10(), pf_frames, tapsets, packets)
}

/// The election's measured win at equal rate over the previous
/// hardwired tapset-0 choice, with nonzero tapsets genuinely coded.
#[test]
fn tapset_election_beats_fixed_tapset0_at_equal_rate() {
    let pcm = periodic_48k(1.0);
    let (snr0, pf0, _, _) = run_arm(&pcm, 40, "t0");
    let (snr_e, pf_e, tapsets, packets) = run_arm(&pcm, 40, "elect");

    assert!(pf0 > 40, "pre-filter must fire on this content: {pf0}");
    assert_eq!(pf0, pf_e, "pf decision is tapset-independent");
    assert!(
        snr_e >= snr0 + 0.8,
        "no tapset-election win: elected {snr_e:.2} dB vs tapset-0 {snr0:.2} dB"
    );
    assert!(
        tapsets[1] + tapsets[2] > 0,
        "election never coded a nonzero tapset: {tapsets:?}"
    );
    // Same rate by construction (fixed payload); packets all sized.
    assert!(packets.iter().all(|p| p.len() == 41));
}

/// The election may only improve on the best FIXED tapset within a
/// small per-frame-vs-stream tolerance, and the disabled encoder is
/// bit-identical to the tapset-0 arm (the pre-election behaviour).
#[test]
fn tapset_election_tracks_best_fixed_arm_and_default_is_unchanged() {
    let pcm = periodic_48k(1.0);
    let (snr_e, _, _, _) = run_arm(&pcm, 40, "elect");
    let mut best_fixed = f64::NEG_INFINITY;
    for arm in ["t0", "t1", "t2"] {
        let (snr, _, _, _) = run_arm(&pcm, 40, arm);
        best_fixed = best_fixed.max(snr);
    }
    // Per-frame greedy election vs a whole-stream fixed arm: allow a
    // small deficit, require it stays in the best arm's neighborhood
    // (measured within ±0.1 dB here).
    assert!(
        snr_e >= best_fixed - 0.25,
        "election fell behind the best fixed arm: {snr_e:.2} vs {best_fixed:.2} dB"
    );

    // Default (no election, no forced tapset) == forced tapset 0.
    let (_, _, _, p_default) = {
        let mut enc = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
        let mut packets = Vec::new();
        for chunk in pcm.chunks_exact(960) {
            packets.push(enc.encode_packet(chunk, 40).unwrap().0);
        }
        (0.0, 0, [0usize; 3], packets)
    };
    let (_, _, _, p_t0) = run_arm(&pcm, 40, "t0");
    assert_eq!(
        p_default, p_t0,
        "default encoder must stay tapset-0 bit-identical"
    );
}
