//! One-shot scanner: walk a directory of fuzz-corpus inputs (the
//! `(channels-byte, payload)` shape used by `opus_oracle_decode`),
//! decode each input via libopus and oxideav-opus, and print a
//! per-mode divergence histogram bucketed by max |PCM-diff|.
//!
//! Used to gauge bit-exactness progress after a celt or silk
//! reference update; cheaper than spinning up cargo-fuzz when we
//! already have a representative corpus on disk.
//!
//! Run via:
//!
//! ```text
//! cd fuzz && cargo run --example scan_histogram -- corpus/opus_oracle_decode
//! ```

use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};
use oxideav_opus_fuzz::libopus;
use std::fs;
use std::path::PathBuf;

const DIVERGENCE_BUCKETS: [i32; 8] = [0, 1, 2, 4, 16, 64, 1024, i32::MAX];
const DIVERGENCE_LABELS: [&str; 8] = [
    "= 0", "<= 1", "<= 2", "<= 4", "<= 16", "<= 64", "<= 1024", "> 1024",
];

fn packet_mode(payload: &[u8]) -> &'static str {
    if payload.is_empty() {
        return "empty";
    }
    let config = (payload[0] >> 3) & 0x1f;
    match config {
        0..=11 => "silk",
        12..=15 => "hybrid",
        16..=31 => "celt",
        _ => "?",
    }
}

fn bucket_index(max_diff: i32) -> usize {
    for (i, b) in DIVERGENCE_BUCKETS.iter().enumerate() {
        if max_diff <= *b {
            return i;
        }
    }
    DIVERGENCE_BUCKETS.len() - 1
}

fn main() {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("corpus/opus_oracle_decode"));
    let entries: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    if !libopus::available() {
        eprintln!("libopus not available");
        return;
    }
    // hist[mode_idx][bucket]
    let mut hist = [[0u64; 8]; 4];
    let mode_names = ["silk", "hybrid", "celt", "?"];
    let mut total = 0usize;
    let mut compared = 0usize;
    for path in entries.iter() {
        total += 1;
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.is_empty() {
            continue;
        }
        let channels: i32 = if data[0] & 1 == 0 { 1 } else { 2 };
        let sample_rate: i32 = 48_000;
        let payload = &data[1..];
        if payload.is_empty() {
            continue;
        }
        let oracle = match libopus::decode(payload, sample_rate, channels) {
            Some(o) => o,
            None => continue,
        };

        let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
        params.channels = Some(channels as u16);
        params.sample_rate = Some(sample_rate as u32);
        let mut dec = match oxideav_opus::decoder::make_decoder(&params) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let pkt = Packet::new(0, TimeBase::new(1, sample_rate as i64), payload.to_vec());
        if dec.send_packet(&pkt).is_err() {
            continue;
        }
        let out = match dec.receive_frame() {
            Ok(Frame::Audio(a)) => a,
            _ => continue,
        };
        let our_pcm_bytes = &out.data[0];
        let n_samples_total = out.samples as usize * channels as usize;
        let need = n_samples_total * 2;
        if our_pcm_bytes.len() < need || oracle.pcm.len() < n_samples_total {
            continue;
        }
        let mut max_diff: i32 = 0;
        for i in 0..n_samples_total {
            let off = i * 2;
            let ours = i16::from_le_bytes([our_pcm_bytes[off], our_pcm_bytes[off + 1]]) as i32;
            let theirs = oracle.pcm[i] as i32;
            let d = (ours - theirs).abs();
            if d > max_diff {
                max_diff = d;
            }
        }
        let m = match packet_mode(payload) {
            "silk" => 0,
            "hybrid" => 1,
            "celt" => 2,
            _ => 3,
        };
        let b = bucket_index(max_diff);
        hist[m][b] += 1;
        compared += 1;
    }
    println!("scanned {total} files, compared {compared} packets");
    for (m, name) in mode_names.iter().enumerate() {
        let row_sum: u64 = hist[m].iter().sum();
        if row_sum == 0 {
            continue;
        }
        let parts: Vec<String> = DIVERGENCE_LABELS
            .iter()
            .zip(hist[m].iter())
            .map(|(l, n)| format!("{l}:{n}"))
            .collect();
        println!("[{name}] n={row_sum} {}", parts.join(" "));
    }
}
