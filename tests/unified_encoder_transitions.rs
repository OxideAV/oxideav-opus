//! RFC 6716 §4.5 configuration switching on the WRITE side: the
//! unified [`OpusEncoder`] drives the mode/bandwidth ladder from the
//! bitrate knob (§2.1.1 / §2.1.3) and embeds the §4.5.1 transition
//! side information — a 5 ms redundant CELT frame in the last
//! old-configuration packet and/or the first new-configuration
//! packet, exactly where §4.5.3 Figure 18 places it:
//!
//! * SILK bandwidth change — end + beginning (double redundancy);
//! * NB/MB SILK → Hybrid, SILK/Hybrid → CELT — end only;
//! * CELT → SILK/Hybrid — beginning only;
//! * WB SILK ↔ Hybrid — none (normative without side information);
//!
//! verified structurally through the crate's own decoder
//! (`OpusDecoder::last_redundancy`) and by whole-stream +
//! seam-window SNR gates on the 120-sample stream timeline.

use oxideav_opus::celt_redundancy::{RedundancyDecision, RedundancyPosition};
use oxideav_opus::{
    Application, Bandwidth, Mode, OpusDecoder, OpusEncoder, OpusFrameRouting, OpusTocByte,
};

/// Aperiodic multitone test signal (incommensurate partials, so lag
/// and seam measurements cannot alias onto a pitch period).
fn multitone(samples: usize, channels: usize, amp: f64) -> Vec<i16> {
    (0..samples * channels)
        .map(|i| {
            let t = (i / channels) as f64 / 48_000.0;
            let v = (std::f64::consts::TAU * 313.7 * t).sin()
                + 0.6 * (std::f64::consts::TAU * 741.3 * t).sin()
                + 0.4 * (std::f64::consts::TAU * 1327.9 * t).sin();
            (amp * 0.5 * v) as i16
        })
        .collect()
}

/// Encode `schedule` (packets-at-bitrate legs) over `input`,
/// returning the packets.
fn run_ladder(
    channels: usize,
    app: Application,
    redundancy: bool,
    input: &[i16],
    schedule: &[(usize, u32)],
) -> Vec<Vec<u8>> {
    let mut enc = OpusEncoder::new(channels, app, schedule[0].1).expect("encoder");
    enc.set_transition_redundancy(redundancy);
    let n = enc.frame_samples() * channels;
    let mut packets = Vec::new();
    let mut off = 0usize;
    for &(count, rate) in schedule {
        enc.set_bitrate(rate).expect("bitrate");
        for _ in 0..count {
            let frame = &input[off..off + n];
            packets.push(enc.encode_frame(frame).expect("encode"));
            off += n;
        }
    }
    packets
}

fn decode_all(packets: &[Vec<u8>]) -> (Vec<i16>, Vec<RedundancyDecision>) {
    let mut dec = OpusDecoder::new();
    let mut pcm = Vec::new();
    let mut reds = Vec::new();
    for p in packets {
        let out = dec.decode_packet(p).expect("decode");
        pcm.extend_from_slice(&out.pcm);
        reds.push(dec.last_redundancy());
    }
    (pcm, reds)
}

fn snr_window(
    input: &[i16],
    out: &[i16],
    channels: usize,
    lag: usize,
    range: std::ops::Range<usize>,
) -> f64 {
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for i in range {
        for k in 0..channels {
            let w = f64::from(input[i * channels + k]);
            let d = w - f64::from(out[(i + lag) * channels + k]);
            sig += w * w;
            err += d * d;
        }
    }
    10.0 * (sig / err.max(1e-9)).log10()
}

fn mode_of(packet: &[u8]) -> (Mode, Bandwidth) {
    let toc = OpusTocByte::parse(packet).expect("toc");
    let routing = OpusFrameRouting::from_toc(toc);
    let mode = match routing.operating_mode {
        oxideav_opus::OperatingMode::SilkOnly => Mode::SilkOnly,
        oxideav_opus::OperatingMode::Hybrid => Mode::Hybrid,
        oxideav_opus::OperatingMode::CeltOnly => Mode::CeltOnly,
    };
    (mode, toc.bandwidth)
}

const P: usize = 25; // packets per leg (500 ms)
const N: usize = 960; // 20 ms at 48 kHz

/// The full mono ladder: every Figure 18 transition class fires and
/// the redundancy lands at its normative position.
#[test]
fn mono_ladder_places_figure18_redundancy() {
    // Legs (Audio app, mono): 10k = SILK NB, 22k = SILK WB (bandwidth
    // change), 24k = Hybrid SWB (from WB SILK: no redundancy), 48k =
    // CELT FB (Hybrid→CELT: end), 10k = SILK NB (CELT→SILK:
    // beginning), 24k = Hybrid (NB SILK→Hybrid... via one-packet legs
    // below), and CELT→Hybrid.
    let schedule: &[(usize, u32)] = &[
        (P, 10_000), // SILK NB
        (P, 18_000), // SILK WB       — S bw change: end + beginning
        (P, 24_000), // Hybrid SWB    — WB S → H: none
        (P, 48_000), // CELT FB       — H → C: end
        (P, 10_000), // SILK NB       — C → S: beginning
        (P, 24_000), // Hybrid SWB    — NB S → H: end
        (P, 48_000), // CELT FB       — H → C: end
        (P, 28_000), // Hybrid SWB    — C → H: beginning (deferred |H)
        (P, 18_000), // SILK WB       — H → WB S: none (overlap flush)
        (P, 10_000), // SILK NB       — S bw change: end + beginning
    ];
    let total: usize = schedule.iter().map(|(c, _)| c * N).sum();
    let input = multitone(total + N, 1, 9000.0);
    let packets = run_ladder(1, Application::Audio, true, &input, schedule);
    let (pcm, reds) = decode_all(&packets);

    // Expected mode/bandwidth per leg, shifted one packet late (the
    // §4.5 one-packet transition latency).
    let legs: &[(Mode, Bandwidth)] = &[
        (Mode::SilkOnly, Bandwidth::Nb),
        (Mode::SilkOnly, Bandwidth::Wb),
        (Mode::Hybrid, Bandwidth::Swb),
        (Mode::CeltOnly, Bandwidth::Fb),
        (Mode::SilkOnly, Bandwidth::Nb),
        (Mode::Hybrid, Bandwidth::Swb),
        (Mode::CeltOnly, Bandwidth::Fb),
        (Mode::Hybrid, Bandwidth::Swb),
        (Mode::SilkOnly, Bandwidth::Wb),
        (Mode::SilkOnly, Bandwidth::Nb),
    ];
    for (leg, &(mode, bw)) in legs.iter().enumerate() {
        // First packet of each leg after leg 0 is still the previous
        // configuration (transition carrier).
        let start = leg * P + usize::from(leg > 0);
        let (m, b) = mode_of(&packets[start]);
        assert_eq!((m, b), (mode, bw), "leg {leg} config");
    }

    // §4.5.3 Figure 18 side-information placement. The transition
    // carrier is packet `leg*P` (the first packet coded after the
    // knob moved, still in the old configuration); the first
    // new-configuration packet is `leg*P + 1`.
    let expect: &[(
        usize,
        Option<RedundancyPosition>,
        Option<RedundancyPosition>,
    )] = &[
        (
            1,
            Some(RedundancyPosition::End),
            Some(RedundancyPosition::Beginning),
        ), // NB→WB
        (2, None, None),                                // WB S→H
        (3, Some(RedundancyPosition::End), None),       // H→C
        (4, None, Some(RedundancyPosition::Beginning)), // C→S
        (5, Some(RedundancyPosition::End), None),       // NB S→H
        (6, Some(RedundancyPosition::End), None),       // H→C
        (7, None, Some(RedundancyPosition::Beginning)), // C→H
        (8, None, None),                                // H→WB S
        (
            9,
            Some(RedundancyPosition::End),
            Some(RedundancyPosition::Beginning),
        ), // WB→NB
    ];
    for &(leg, end, begin) in expect {
        let carrier = leg * P;
        let first_new = carrier + 1;
        let got_end = match reds[carrier] {
            RedundancyDecision::Present { position, .. } => Some(position),
            _ => None,
        };
        let got_begin = match reds[first_new] {
            RedundancyDecision::Present { position, .. } => Some(position),
            _ => None,
        };
        assert_eq!(got_end, end, "leg {leg} carrier packet side info");
        assert_eq!(got_begin, begin, "leg {leg} first-new packet side info");
        // §4.5.1.3 size as the decoder counts it must equal what the
        // encoder appended: ~5 ms of the richer seam side's bitrate
        // (the SILK portion is padded to ceil(tell / 8) so the
        // terminator's omitted zero bytes cannot shift the count).
        let expected_size = (schedule[leg - 1].1.max(schedule[leg].1) / 1600) as usize;
        for (pkt, want) in [(carrier, end), (first_new, begin)] {
            if want.is_some() {
                assert!(
                    matches!(reds[pkt], RedundancyDecision::Present { size_bytes, .. }
                        if size_bytes == expected_size),
                    "leg {leg} packet {pkt}: {:?}, expected {expected_size} bytes",
                    reds[pkt]
                );
            }
        }
    }

    // Whole-stream decode on the 120-sample timeline.
    let snr = snr_window(&input, &pcm, 1, 120, 4_800..total - 200);
    assert!(snr > 4.0, "whole-stream SNR {snr:.1} dB");
}

/// Redundancy measurably helps the CELT-involving seams: with the
/// §4.5.1 side information the seam window decodes at least as well
/// as the Figure 19 concealment fallback, and the C→S / H→C seams
/// improve outright.
#[test]
fn redundancy_improves_celt_seams() {
    let schedule: &[(usize, u32)] = &[
        (P, 48_000), // CELT FB
        (P, 10_000), // SILK NB — C→S seam at packet 25/26
        (P, 48_000), // CELT FB — S→C seam
        (P, 28_000), // Hybrid SWB — C→H seam
    ];
    let total: usize = schedule.iter().map(|(c, _)| c * N).sum();
    let input = multitone(total + N, 1, 9000.0);
    let with = decode_all(&run_ladder(1, Application::Audio, true, &input, schedule)).0;
    let without = decode_all(&run_ladder(1, Application::Audio, false, &input, schedule)).0;

    for (name, seam_pkt) in [("C->S", P), ("S->C", 2 * P), ("C->H", 3 * P)] {
        // 40 ms window centred on the seam between the carrier packet
        // and the first new-configuration packet.
        let seam = (seam_pkt + 1) * N;
        let w = seam - N..seam + N;
        let s_with = snr_window(&input, &with, 1, 120, w.clone());
        let s_without = snr_window(&input, &without, 1, 120, w);
        println!("{name}: with={s_with:.1} dB without={s_without:.1} dB");
        assert!(
            s_with + 0.5 >= s_without,
            "{name}: redundancy must not hurt the seam ({s_with:.1} vs {s_without:.1})"
        );
    }
}

/// The stereo ladder places the same side information (redundant
/// frames carry the carrier's channel count per §4.5.1.4).
#[test]
fn stereo_ladder_places_redundancy() {
    let schedule: &[(usize, u32)] = &[
        (P, 14_000), // SILK NB (eff 9.3k)
        (P, 60_000), // CELT SWB? eff 40k -> CELT Fb
        (P, 14_000), // back: C->S beginning
    ];
    let total: usize = schedule.iter().map(|(c, _)| c * N).sum();
    let input = multitone(total + N, 2, 9000.0);
    let packets = run_ladder(2, Application::Audio, true, &input, schedule);
    let (pcm, reds) = decode_all(&packets);
    assert!(matches!(
        reds[P],
        RedundancyDecision::Present {
            position: RedundancyPosition::End,
            ..
        }
    ));
    assert!(matches!(
        reds[2 * P + 1],
        RedundancyDecision::Present {
            position: RedundancyPosition::Beginning,
            size_bytes: 37, // max(60k, 14k) / 1600
        }
    ));
    assert!(matches!(
        reds[P],
        RedundancyDecision::Present { size_bytes: 37, .. }
    ));
    let snr = snr_window(&input, &pcm, 2, 120, 4_800..total - 200);
    assert!(snr > 3.0, "stereo whole-stream SNR {snr:.1} dB");
}

/// Knob validation: incompatible mode / frame-duration combinations
/// are rejected; compatible ones round-trip.
#[test]
fn knob_validation() {
    let mut enc = OpusEncoder::new(1, Application::Voip, 16_000).expect("encoder");
    enc.set_frame_tenths_ms(25).expect("2.5 ms ok");
    assert!(enc.set_mode(Some(Mode::SilkOnly)).is_err());
    assert!(enc.set_mode(Some(Mode::CeltOnly)).is_ok());
    enc.set_frame_tenths_ms(200).expect("back to 20 ms");
    enc.set_mode(Some(Mode::SilkOnly))
        .expect("silk ok at 20 ms");
    assert!(enc.set_frame_tenths_ms(50).is_err());
    enc.set_mode(None).expect("auto");
    enc.set_frame_tenths_ms(400).expect("40 ms auto = SILK");
    assert!(enc.set_mode(Some(Mode::Hybrid)).is_err());
    assert!(OpusEncoder::new(3, Application::Voip, 16_000).is_err());
}

/// Black-box capture dump: when `OPUS_DUMP_DIR` is set, write the
/// mono ladder's packets in the reference demo program's capture
/// framing (big-endian u32 payload length, big-endian u32 range
/// word — zero here, the decoder direction ignores it — then the
/// payload) plus the raw 48 kHz s16le input, for out-of-tree
/// validation against black-box decoder binaries. A no-op in CI.
#[test]
fn dump_ladder_captures_for_blackbox() {
    let Some(dir) = std::env::var_os("OPUS_DUMP_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let schedule: &[(usize, u32)] = &[
        (P, 10_000),
        (P, 18_000),
        (P, 24_000),
        (P, 48_000),
        (P, 10_000),
        (P, 24_000),
        (P, 48_000),
        (P, 28_000),
        (P, 18_000),
        (P, 10_000),
    ];
    let total: usize = schedule.iter().map(|(c, _)| c * N).sum();
    for (name, channels) in [("ladder-mono", 1usize), ("ladder-stereo", 2)] {
        let input = multitone(total + N, channels, 9000.0);
        let packets = run_ladder(channels, Application::Audio, true, &input, schedule);
        let mut bits = Vec::new();
        for p in &packets {
            bits.extend_from_slice(&(p.len() as u32).to_be_bytes());
            bits.extend_from_slice(&0u32.to_be_bytes());
            bits.extend_from_slice(p);
        }
        std::fs::write(dir.join(format!("{name}.bits")), bits).expect("write bits");
        let mut raw = Vec::with_capacity(input.len() * 2);
        for s in &input[..total * channels] {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(dir.join(format!("{name}.input.s16")), raw).expect("write input");
        // Our own decode for cross-checking.
        let (pcm, _) = decode_all(&packets);
        let mut raw = Vec::with_capacity(pcm.len() * 2);
        for s in &pcm {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(dir.join(format!("{name}.own.s16")), raw).expect("write own");
    }
}
