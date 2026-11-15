//! Encode / decode throughput of the unified `OpusEncoder` and the
//! streaming `OpusDecoder`, in multiples of real time.
//!
//! `cargo bench --bench codec_speed` prints one row per channel count
//! × target bitrate (16 kb/s speech profile → the LP layer, 32 kb/s →
//! Hybrid, 64 / 128 kb/s audio profile → the MDCT layer): the packet
//! mode mix, the realized bitrate, and the encode and decode speeds as
//! the median of `OPUS_BENCH_RUNS` (default 5) timed passes over an
//! `OPUS_BENCH_SECONDS` (default 10) second synthetic clip.
//! `OPUS_BENCH_FILTER=<substring>` restricts the rows (matched against
//! e.g. `1ch-64k`), `OPUS_BENCH_ONLY=encode|decode` the timed half.

use oxideav_opus::{Application, OpusDecoder, OpusEncoder};
use std::time::Instant;

/// Voiced harmonics under a syllabic envelope, a few sustained
/// partials and a little noise: something for every layer to chew on.
fn clip(seconds: f64, channels: usize) -> Vec<i16> {
    let n = (seconds * 48_000.0) as usize;
    let mut state = 0x2545_f491u32;
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        let t = i as f64 / 48_000.0;
        let tau = std::f64::consts::TAU;
        let envelope = (tau * 2.7 * t).sin().max(0.0).powi(2);
        let f0 = 110.0 + 25.0 * (tau * 0.45 * t).sin();
        let mut voiced = 0.0;
        for h in 1..=10 {
            voiced += (tau * f0 * f64::from(h) * t).sin() / f64::from(h);
        }
        let partials = (tau * 523.25 * t).sin()
            + 0.6 * (tau * 1567.98 * t).sin()
            + 0.3 * (tau * 4186.0 * t).sin()
            + 0.15 * (tau * 9_400.0 * t).sin();
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = f64::from(state >> 8) / f64::from(1u32 << 24) - 0.5;
        let left = 6000.0 * envelope * voiced + 2200.0 * partials + 500.0 * noise;
        let right = 3500.0 * envelope * voiced + 3000.0 * partials - 400.0 * noise;
        out.push(left as i16);
        if channels == 2 {
            out.push(right as i16);
        }
    }
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn env_num(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let seconds = env_num("OPUS_BENCH_SECONDS", 10.0);
    let runs = env_num("OPUS_BENCH_RUNS", 5.0) as usize;
    let filter = std::env::var("OPUS_BENCH_FILTER").ok();
    let only = std::env::var("OPUS_BENCH_ONLY").ok();
    println!(
        "| channels | target kb/s | packets SILK/Hybrid/CELT | realized kb/s | encode x real time | decode x real time |"
    );
    println!("| --- | --- | --- | --- | --- | --- |");
    for channels in [1usize, 2] {
        let pcm = clip(seconds, channels);
        for (rate, application) in [
            (16_000u32, Application::Voip),
            (32_000, Application::Voip),
            (64_000, Application::Audio),
            (128_000, Application::Audio),
        ] {
            let tag = format!("{channels}ch-{}k", rate / 1000);
            if filter.as_ref().is_some_and(|f| !tag.contains(f.as_str())) {
                continue;
            }
            let mut packets: Vec<Vec<u8>> = Vec::new();
            let mut encode_speed = Vec::new();
            let encode_runs = if only.as_deref() == Some("decode") {
                1
            } else {
                runs
            };
            for _ in 0..encode_runs {
                let mut enc = OpusEncoder::new(channels, application, rate).expect("encoder");
                let frame = enc.frame_samples() * channels;
                packets.clear();
                let start = Instant::now();
                for chunk in pcm.chunks_exact(frame) {
                    packets.push(enc.encode_frame(chunk).expect("encode"));
                }
                encode_speed.push(seconds / start.elapsed().as_secs_f64());
            }
            let mut decode_speed = Vec::new();
            let decode_runs = if only.as_deref() == Some("encode") {
                1
            } else {
                runs
            };
            let mut checksum = 0i64;
            for _ in 0..decode_runs {
                let mut dec = OpusDecoder::new();
                let start = Instant::now();
                for p in &packets {
                    let audio = dec.decode_packet(p).expect("decode");
                    checksum += i64::from(audio.pcm[audio.pcm.len() / 2]);
                }
                decode_speed.push(seconds / start.elapsed().as_secs_f64());
            }
            std::hint::black_box(checksum);
            let mut modes = [0usize; 3];
            for p in &packets {
                modes[match p[0] >> 3 {
                    0..=11 => 0,
                    12..=15 => 1,
                    _ => 2,
                }] += 1;
            }
            let bytes: usize = packets.iter().map(Vec::len).sum();
            println!(
                "| {channels} | {} | {}/{}/{} | {:.1} | {:.1} | {:.1} |",
                rate / 1000,
                modes[0],
                modes[1],
                modes[2],
                bytes as f64 * 8.0 / seconds / 1000.0,
                median(encode_speed),
                median(decode_speed),
            );
        }
    }
}
