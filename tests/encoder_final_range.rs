//! Encoder ↔ decoder final-range agreement (RFC 6716 §6: "the
//! decoder's … final range coder state MUST match the encoder's").
//!
//! Every packet the unified encoder writes — each operating mode ×
//! audio bandwidth × packet duration × channel count, under VBR,
//! constrained VBR and hard CBR, with in-band FEC and DTX, and across
//! the §4.5 configuration switches with their redundant CELT frames —
//! must decode through the crate's own decoder to the identical final
//! range the encoder reports, and decode without error.
//!
//! With `OPUS_FINAL_RANGE_CAPTURE=<dir>` each run is also written in
//! the reference demo program's capture format (big-endian u32 length,
//! big-endian u32 final range, payload) so a black-box decoder can
//! verify the encoder's reported values independently.

use oxideav_opus::toc::{Bandwidth, Mode};
use oxideav_opus::{Application, FrameDecodeStatus, OpusDecoder, OpusEncoder};

/// Speech-like + music-like deterministic test signal (interleaved).
fn signal(samples: usize, channels: usize, seed: u32) -> Vec<i16> {
    let mut state = seed | 1;
    let mut noise = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f64 / f64::from(1u32 << 24) - 0.5
    };
    let mut out = Vec::with_capacity(samples * channels);
    for i in 0..samples {
        let t = i as f64 / 48_000.0;
        // Syllabic envelope with pauses.
        let syl = (2.0 * std::f64::consts::PI * 3.1 * t).sin().max(0.0);
        let f0 = 120.0 + 30.0 * (2.0 * std::f64::consts::PI * 0.7 * t).sin();
        let mut voiced = 0.0;
        for h in 1..=12 {
            voiced += (2.0 * std::f64::consts::PI * f0 * f64::from(h) * t).sin() / f64::from(h);
        }
        let music = (2.0 * std::f64::consts::PI * 440.0 * t).sin()
            + 0.5 * (2.0 * std::f64::consts::PI * 1318.5 * t).sin()
            + 0.25 * (2.0 * std::f64::consts::PI * 5274.0 * t).sin();
        let n = noise();
        let l = 5000.0 * syl * voiced + 2500.0 * music + 600.0 * n;
        let r = 3000.0 * syl * voiced + 3500.0 * music - 500.0 * n;
        out.push(l as i16);
        if channels == 2 {
            out.push(r as i16);
        }
    }
    out
}

fn dump(name: &str, packets: &[(Vec<u8>, u32)]) {
    let Some(dir) = std::env::var_os("OPUS_FINAL_RANGE_CAPTURE") else {
        return;
    };
    let mut bytes = Vec::new();
    for (p, rng) in packets {
        bytes.extend_from_slice(&(p.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&rng.to_be_bytes());
        bytes.extend_from_slice(p);
    }
    let mut path = std::path::PathBuf::from(dir);
    path.push(format!("{name}.bit"));
    std::fs::write(path, bytes).unwrap();
}

/// Run `frames` packets through `enc`, calling `tweak(i, enc)` before
/// each, and check every packet against a fresh decoder. Returns the
/// number of packets whose final range was non-zero (i.e. verified
/// against real entropy-coded content).
fn run(
    name: &str,
    enc: &mut OpusEncoder,
    frames: usize,
    mut tweak: impl FnMut(usize, &mut OpusEncoder),
    quiet_from: Option<usize>,
) -> usize {
    let channels = enc.channels();
    let mut dec = OpusDecoder::new();
    let mut verified = 0usize;
    let mut captured = Vec::new();
    let mut pos = 0usize;
    let total: Vec<i16> = signal(48_000 * 4, channels, 0x5eed);
    for i in 0..frames {
        tweak(i, enc);
        let n = enc.frame_samples() * channels;
        let mut pcm: Vec<i16> = (0..n).map(|k| total[(pos + k) % total.len()]).collect();
        pos += n;
        if quiet_from.is_some_and(|q| i >= q) {
            pcm.fill(0);
        }
        let packet = enc
            .encode_frame(&pcm)
            .unwrap_or_else(|e| panic!("{name}: encode failed at packet {i}: {e:?}"));
        let enc_range = enc.final_range();
        let audio = dec
            .decode_packet(&packet)
            .unwrap_or_else(|e| panic!("{name}: decode failed at packet {i}: {e:?}"));
        for o in &audio.frame_outcomes {
            assert!(
                !matches!(
                    o.status,
                    FrameDecodeStatus::SilkDecodeError | FrameDecodeStatus::CeltDecodeError
                ),
                "{name}: packet {i} decoded with an error status {:?}",
                o.status
            );
        }
        assert_eq!(
            dec.final_range(),
            enc_range,
            "{name}: final range mismatch at packet {i} (toc {:#04x}, {} bytes)",
            packet[0],
            packet.len()
        );
        if enc_range != 0 {
            verified += 1;
        }
        captured.push((packet, enc_range));
    }
    dump(name, &captured);
    verified
}

const SILK_BW: [Bandwidth; 3] = [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb];
const HYBRID_BW: [Bandwidth; 2] = [Bandwidth::Swb, Bandwidth::Fb];
const CELT_BW: [Bandwidth; 4] = [Bandwidth::Nb, Bandwidth::Wb, Bandwidth::Swb, Bandwidth::Fb];

fn bw_name(b: Bandwidth) -> &'static str {
    match b {
        Bandwidth::Nb => "nb",
        Bandwidth::Mb => "mb",
        Bandwidth::Wb => "wb",
        Bandwidth::Swb => "swb",
        Bandwidth::Fb => "fb",
    }
}

#[test]
fn every_forced_configuration_matches() {
    let mut verified = 0usize;
    let mut configs = 0usize;
    for channels in [1usize, 2] {
        let plan: [(Mode, &[Bandwidth], &[u16], u32); 3] = [
            (Mode::SilkOnly, &SILK_BW, &[100, 200, 400, 600], 20_000),
            (Mode::Hybrid, &HYBRID_BW, &[100, 200, 400, 600], 36_000),
            (
                Mode::CeltOnly,
                &CELT_BW,
                &[25, 50, 100, 200, 400, 600],
                64_000,
            ),
        ];
        for (mode, bws, durations, rate) in plan {
            for &bw in bws {
                for &tenths in durations {
                    let bitrate = rate * channels as u32;
                    let mut enc = OpusEncoder::new(channels, Application::Audio, bitrate).unwrap();
                    enc.set_mode(Some(mode)).unwrap();
                    enc.set_bandwidth(Some(bw));
                    enc.set_frame_tenths_ms(tenths).unwrap();
                    let name = format!("{mode:?}-{}-{tenths}-{channels}ch", bw_name(bw));
                    let frames = (4000 / usize::from(tenths)).clamp(6, 40);
                    verified += run(&name, &mut enc, frames, |_, _| {}, None);
                    configs += 1;
                }
            }
        }
    }
    eprintln!("{configs} forced configurations, {verified} packets verified");
    assert!(verified > configs * 4, "too few packets carried a range");
}

#[test]
fn rate_control_fec_and_dtx_variants_match() {
    let mut verified = 0usize;
    for channels in [1usize, 2] {
        for (label, mode, bitrate) in [
            ("silk", Mode::SilkOnly, 16_000u32),
            ("hybrid", Mode::Hybrid, 32_000),
            ("celt", Mode::CeltOnly, 48_000),
        ] {
            for variant in ["vbr", "cvbr", "cbr", "fec", "dtx"] {
                let mut enc = OpusEncoder::new(channels, Application::Voip, bitrate).unwrap();
                enc.set_mode(Some(mode)).unwrap();
                match variant {
                    "cvbr" => enc.set_constrained_vbr(true).unwrap(),
                    "cbr" => enc.set_vbr(false),
                    "fec" => {
                        enc.set_fec(true);
                        enc.set_packet_loss_perc(15);
                    }
                    "dtx" => enc.set_dtx(true),
                    _ => {}
                }
                let quiet = (variant == "dtx").then_some(20);
                let name = format!("{label}-{variant}-{channels}ch");
                verified += run(&name, &mut enc, 60, |_, _| {}, quiet);
            }
        }
    }
    eprintln!("{verified} packets verified");
    assert!(verified > 1000);
}

#[test]
fn configuration_switches_with_redundancy_match() {
    // Walk mode and bandwidth changes so the §4.5.1 redundant frames
    // (both positions) take part in the reported value.
    let ladder: [(Mode, Bandwidth); 9] = [
        (Mode::SilkOnly, Bandwidth::Nb),
        (Mode::SilkOnly, Bandwidth::Wb),
        (Mode::Hybrid, Bandwidth::Fb),
        (Mode::CeltOnly, Bandwidth::Fb),
        (Mode::Hybrid, Bandwidth::Swb),
        (Mode::SilkOnly, Bandwidth::Mb),
        (Mode::CeltOnly, Bandwidth::Wb),
        (Mode::SilkOnly, Bandwidth::Wb),
        (Mode::Hybrid, Bandwidth::Fb),
    ];
    let mut verified = 0usize;
    for channels in [1usize, 2] {
        for redundancy in [true, false] {
            let mut enc = OpusEncoder::new(channels, Application::Audio, 40_000).unwrap();
            enc.set_transition_redundancy(redundancy);
            let name = format!("switches-{channels}ch-red{}", u8::from(redundancy));
            verified += run(
                &name,
                &mut enc,
                ladder.len() * 8,
                |i, enc| {
                    let (mode, bw) = ladder[i / 8];
                    enc.set_mode(Some(mode)).unwrap();
                    enc.set_bandwidth(Some(bw));
                },
                None,
            );
        }
        // The automatic, signal-adaptive election with a bitrate sweep.
        let mut enc = OpusEncoder::new(channels, Application::Voip, 8_000).unwrap();
        verified += run(
            &format!("auto-sweep-{channels}ch"),
            &mut enc,
            120,
            |i, enc| {
                let rate = 8_000 + 1_500 * (i as u32 % 60);
                enc.set_bitrate(rate).unwrap();
            },
            None,
        );
    }
    eprintln!("{verified} packets verified");
    assert!(verified > 500);
}
