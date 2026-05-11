//! One-shot scanner: walk a directory of fuzz-corpus inputs (the
//! `(channels-byte, payload)` shape used by `opus_oracle_decode`),
//! decode each input via libopus and oxideav-opus, and print packets
//! where libopus says silence (max ≤ 64 LSB) but ours rails (max >
//! 8 000 LSB). Used to triage the silence-rail regressions reported
//! in commit a6ca9ea so the silk-side follow-up has reproducible
//! per-input samples to bisect against.
//!
//! Run via:
//!
//! ```text
//! cd fuzz && cargo run --example scan_silence -- corpus/opus_oracle_decode
//! ```

use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};
use oxideav_opus_fuzz::libopus;
use std::fs;
use std::path::PathBuf;

const SILENCE_LIBOPUS_MAX: i32 = 64;
const SILENCE_OXIDEAV_RAIL: i32 = 8_000;

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
    let mut total = 0usize;
    let mut silence_offenders = 0usize;
    for path in entries.iter() {
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
        total += 1;

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
        if our_pcm_bytes.len() < need {
            continue;
        }
        let mut libopus_max: i32 = 0;
        let mut ours_max: i32 = 0;
        for i in 0..n_samples_total {
            let off = i * 2;
            let ours = i16::from_le_bytes([our_pcm_bytes[off], our_pcm_bytes[off + 1]]) as i32;
            let theirs = oracle.pcm[i] as i32;
            if theirs.abs() > libopus_max {
                libopus_max = theirs.abs();
            }
            if ours.abs() > ours_max {
                ours_max = ours.abs();
            }
        }
        if libopus_max <= SILENCE_LIBOPUS_MAX && ours_max > SILENCE_OXIDEAV_RAIL {
            silence_offenders += 1;
            let cfg = (payload[0] >> 3) & 0x1f;
            let mode = packet_mode(payload);
            let stereo_bit = (payload[0] >> 2) & 1;
            let frame_packing = payload[0] & 0x3;
            println!(
                "{} mode={mode} cfg={cfg} stereo_bit={stereo_bit} fp={frame_packing} ch={channels} libopus_max={libopus_max} ours_max={ours_max} samples={} payload_len={}",
                path.file_name().unwrap().to_string_lossy(),
                oracle.samples_per_channel,
                payload.len(),
            );
        }
    }
    println!("scanned {total} packets, {silence_offenders} silence-rail offenders");
}
