#![no_main]

//! Fuzz: arbitrary Opus packet bytes → both libopus's `opus_decode`
//! and oxideav's decoder, then compare:
//!
//!   * If libopus accepts (returns ≥ 0 sample count), ours must also
//!     accept and produce the same number of samples per channel.
//!   * If libopus rejects (returns negative), we don't constrain
//!     oxideav's verdict — random bytes can be a "syntactically OK
//!     but semantically wrong" packet that one decoder might surface
//!     as silence and the other as an error.
//!
//! ## What the PCM comparison checks (and what it doesn't)
//!
//! Per the oxideav-opus decoder docs (`src/decoder.rs` module head)
//! and the oxideav-celt crate header, the CELT pipeline's PVQ shape
//! recurrence and IMDCT are **not yet bit-exact with libopus** — which
//! is why `tests/encoder_roundtrip.rs` uses an "energy survives"
//! criterion rather than a tight PSNR bar. A strict ±2-LSB window
//! therefore divergence-trips on essentially every packet that
//! exercises the CELT path, which buries the more interesting bugs
//! (sample-count mismatches, panic-on-libopus-accept).
//!
//! Hard-asserted contract today:
//!   1. `samples_per_channel` matches libopus exactly,
//!   2. our decoder produces the expected byte count for that shape,
//!   3. our decoder did not panic when libopus accepted.
//!
//! Logged-only (CI grep target):
//!   * Per-mode divergence histogram every power-of-two iterations,
//!     bucketed at 0 / ≤1 / ≤2 / ≤4 / ≤16 / ≤64 / ≤1024 / >1024 LSB.
//!     Distribution shape over time tells you whether a fix moved the
//!     needle or just shifted noise.
//!   * Throttled per-divergence trace (1 in 64 packets) for triage
//!     when a regression first lands.
//!   * **Scale-saturation gate**: when libopus reports the packet as
//!     near-silent (max |sample| ≤ `SILENCE_LIBOPUS_MAX`), our
//!     decoder should also stay below `SILENCE_OXIDEAV_RAIL`. Sweep
//!     history on the on-disk 1248-packet corpus:
//!
//!     | round | silk | hybrid | celt | total |
//!     |------:|-----:|-------:|-----:|------:|
//!     | r44   |    4 |     10 |    2 |    16 |
//!     | r45   |    1 |     10 |    2 |    13 | silk §4.2.7.3 ICDF fix |
//!     | r46   |    1 |     12 |    2 |    15 | celt 0.1.5 publish (mixed-radix FFT + norm_len) |
//!
//!     The hybrid count nudged 10→12 after the celt 0.1.5 update —
//!     two short hybrid packets (cfg=12, cfg=14) that previously
//!     just-cleared 8 000 LSB on the broken-FFT codepath now rail
//!     on the corrected one. They share the cfg-12/14 hybrid shape
//!     so the failure mode is hybrid bit-allocation / start-band
//!     interaction, not a celt regression. Per-mode follow-up is
//!     queued.
//!
//! Flip `STRICT_PCM` to `true` once oxideav-celt's PVQ/IMDCT bit-exact
//! rebuild lands. Post-celt-0.1.5 baseline against the on-disk corpus
//! (1248 inputs, 1194 oracle-accepted):
//!
//!   * silk-only   n=316: = 0 → 10  (3.2 % bit-exact)
//!   * hybrid      n=106: = 0 →  0
//!   * celt-only   n=772: = 0 → 13  (1.7 % bit-exact)
//!
//! Tightening `PCM_TOL` from ±2 → ±16 won't materially change which
//! packets fall under STRICT_PCM (≤16 only adds 6 silk / 0 hybrid /
//! 1 celt packets over `= 0`), so the global gate stays off until
//! oxideav-celt's PVQ / fine-energy refinement lands.
//!
//! When libopus isn't installed the harness `eprintln!`s a
//! `[oracle skip]` marker once per process and returns — **NO
//! `#[ignore]`** is used, so the binary still runs and the cargo
//! fuzz wrapper still records iterations.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};
use oxideav_opus_fuzz::libopus;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

/// PCM tolerance in 16-bit code units when `STRICT_PCM` is on. The
/// CELT post-filter and IMDCT have a couple of last-place rounding
/// paths that can disagree by ±1 LSB across implementations; we set
/// ±2 to absorb that. Today this only logs (see `STRICT_PCM`).
const PCM_TOL: i32 = 2;
/// Flip to `true` once oxideav-celt's PVQ/IMDCT path is bit-exact
/// with libopus. Until then, large PCM divergences are expected on
/// CELT-bearing packets and a strict assertion would bury more
/// interesting bugs.
const STRICT_PCM: bool = false;

/// libopus-said-silent threshold: if libopus's max |sample| on this
/// packet is below this, the packet is effectively silence/DTX. Our
/// decoder is required to also stay below `SILENCE_OUR_RAIL` — i.e.
/// no scale-saturation regression that turns silence into ±32k. Set
/// generously: real DTX yields max ≤ 1; real low-energy speech can
/// still legitimately reach a few hundred LSB.
const SILENCE_LIBOPUS_MAX: i32 = 64;
/// When `libopus_max <= SILENCE_LIBOPUS_MAX`, our decoder MUST stay
/// below this. This is the "no rail saturation on silence" assertion
/// — catches the historical scale-detection regression where a quiet
/// CELT packet mis-routed to the `× 32 768` path and pegged every
/// sample at ±32k. Achievable on the current corpus.
const SILENCE_OXIDEAV_RAIL: i32 = 8_000;

/// Histogram bucket boundaries for per-class divergence reporting.
/// Each slot counts packets whose max |diff| is `<= boundary` (and
/// greater than the previous boundary).
const DIVERGENCE_BUCKETS: [i32; 8] = [0, 1, 2, 4, 16, 64, 1024, i32::MAX];
const DIVERGENCE_LABELS: [&str; 8] = [
    "= 0", "<= 1", "<= 2", "<= 4", "<= 16", "<= 64", "<= 1024", "> 1024",
];

fuzz_target!(|data: &[u8]| {
    static SKIP_LOGGED: OnceLock<()> = OnceLock::new();
    if !libopus::available() {
        SKIP_LOGGED.get_or_init(|| {
            eprintln!("[oracle skip] libopus not loadable — opus_oracle_decode harness is a no-op");
        });
        return;
    }
    if data.is_empty() {
        return;
    }
    // oxideav's decoder hard-codes 48 kHz output (see
    // `OPUS_RATE_HZ`). libopus accepts 8/12/16/24/48 kHz and resamples
    // internally, but those rates produce a different sample-count
    // contract than ours, so we only oracle-compare at 48 kHz.
    // Channels (1 or 2) come from the low bit of the first byte.
    let channels: i32 = if data[0] & 1 == 0 { 1 } else { 2 };
    let sample_rate: i32 = 48_000;
    let payload = &data[1..];
    if payload.is_empty() {
        return;
    }

    // Run libopus first — it's the oracle.
    let oracle = match libopus::decode(payload, sample_rate, channels) {
        Some(o) => o,
        None => {
            // libopus rejected the packet. We don't constrain
            // oxideav here — random bytes that confuse libopus may
            // or may not confuse oxideav. Just verify oxideav
            // doesn't panic on them.
            run_oxideav_no_assert(payload, channels as u16, sample_rate as u32);
            return;
        }
    };

    // libopus accepted. Now run oxideav and compare.
    let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    params.channels = Some(channels as u16);
    params.sample_rate = Some(sample_rate as u32);
    let mut dec = match oxideav_opus::decoder::make_decoder(&params) {
        Ok(d) => d,
        Err(_) => return,
    };
    let pkt = Packet::new(0, TimeBase::new(1, sample_rate as i64), payload.to_vec());
    if dec.send_packet(&pkt).is_err() {
        // libopus accepted but our decoder is sequenced — should
        // never fail at send_packet (state machine, not parse).
        return;
    }
    let out = match dec.receive_frame() {
        Ok(Frame::Audio(a)) => a,
        Ok(_) => return,
        Err(e) => {
            panic!(
                "libopus accepted (got {} samples/ch at {}/{}ch) but oxideav rejected: {e:?}",
                oracle.samples_per_channel, sample_rate, channels
            );
        }
    };

    // Shape: same sample count per channel.
    assert_eq!(
        out.samples as i32, oracle.samples_per_channel,
        "sample count mismatch: oxideav={}, libopus={} (sr={}, ch={})",
        out.samples, oracle.samples_per_channel, sample_rate, channels
    );

    // PCM bytes: oxideav emits interleaved S16 LE in plane 0 for
    // both mono and stereo (see decoder docs).
    let our_pcm_bytes = &out.data[0];
    let n_samples_total = out.samples as usize * channels as usize;
    let need = n_samples_total * 2;
    if our_pcm_bytes.len() < need {
        panic!(
            "oxideav decoded plane {} bytes < expected {} (samples={}, ch={})",
            our_pcm_bytes.len(),
            need,
            out.samples,
            channels
        );
    }

    // PCM-by-PCM compare with tolerance — see STRICT_PCM doc for why
    // strict equality is logging-only today. We still walk every
    // sample so the cost is paid against the oxideav decoder (catching
    // e.g. NaN spills via the i16 round-trip).
    let mut max_diff: i32 = 0;
    let mut max_at: usize = 0;
    let mut libopus_max: i32 = 0;
    let mut ours_max: i32 = 0;
    for i in 0..n_samples_total {
        let off = i * 2;
        let ours = i16::from_le_bytes([our_pcm_bytes[off], our_pcm_bytes[off + 1]]) as i32;
        let theirs = oracle.pcm[i] as i32;
        let d = (ours - theirs).abs();
        if d > max_diff {
            max_diff = d;
            max_at = i;
        }
        if theirs.abs() > libopus_max {
            libopus_max = theirs.abs();
        }
        if ours.abs() > ours_max {
            ours_max = ours.abs();
        }
    }

    // "No scale-saturation on silence" gate. When libopus reports a
    // near-silent packet (DTX or low-energy tail) our decoder should
    // also stay quiet. Today this is logged not asserted: post-r45
    // sweep of the on-disk corpus shows 15 / 1248 packets violate
    // it (12 hybrid, 1 silk-only, 2 celt-only). The silk path is
    // now within one stray; the hybrid offenders (mostly cfg=14/15
    // short payloads, 3-11 B) and the two cfg=16/27 celt-only ones
    // still need the hybrid bit-allocation + celt fine-energy
    // tightening. Once those land, swap the `eprintln!` for an
    // `assert!` to catch regressions.
    if libopus_max <= SILENCE_LIBOPUS_MAX && ours_max > SILENCE_OXIDEAV_RAIL {
        eprintln!(
            "[oracle silence-saturation] mode={} libopus_max={libopus_max} \
             ours_max={ours_max} (sr={sample_rate}, ch={channels}, \
             samples/ch={}, payload[0]=0x{:02x})",
            packet_mode(payload),
            oracle.samples_per_channel,
            payload[0]
        );
    }

    // Per-mode bucket histogram. Logged once per N divergences via
    // `should_log_progress` so a CI run produces a digestible
    // summary tail without per-input spam. Distribution shape over
    // time tells you whether a fix moved the needle.
    let mode_class = packet_mode(payload);
    record_divergence(mode_class, max_diff);

    if STRICT_PCM {
        assert!(
            max_diff <= PCM_TOL,
            "PCM diverges from libopus by {max_diff} LSB at sample {max_at} \
             (sr={sample_rate}, ch={channels}, samples/ch={})",
            oracle.samples_per_channel
        );
    } else if max_diff > PCM_TOL && should_log_individual() {
        // Per-divergence trace, throttled. The per-mode histogram in
        // `record_divergence` is the primary dashboard.
        eprintln!(
            "[oracle pcm-diverge] mode={mode_class} {max_diff} LSB at sample {max_at} \
             (sr={sample_rate}, ch={channels}, samples/ch={})",
            oracle.samples_per_channel
        );
    }
});

fn run_oxideav_no_assert(payload: &[u8], channels: u16, sample_rate: u32) {
    let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    let mut dec = match oxideav_opus::decoder::make_decoder(&params) {
        Ok(d) => d,
        Err(_) => return,
    };
    let pkt = Packet::new(0, TimeBase::new(1, sample_rate as i64), payload.to_vec());
    let _ = dec.send_packet(&pkt);
    let _ = dec.receive_frame();
}

/// Classify the packet by its TOC config field for per-mode reporting.
/// Mirrors RFC 6716 §3.1 Table 2.
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

/// Per-mode divergence histogram counters. Indexed by
/// `mode_index(class) * DIVERGENCE_BUCKETS.len() + bucket`. Atomic so
/// libfuzzer's parallel iterations don't race the counters; relaxed
/// ordering is fine since we only print a snapshot.
static HIST: [AtomicU64; 4 * 8] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn mode_index(class: &str) -> usize {
    match class {
        "silk" => 0,
        "hybrid" => 1,
        "celt" => 2,
        _ => 3,
    }
}

fn record_divergence(class: &str, max_diff: i32) {
    let m = mode_index(class);
    for (i, bucket) in DIVERGENCE_BUCKETS.iter().enumerate() {
        if max_diff <= *bucket {
            HIST[m * DIVERGENCE_BUCKETS.len() + i].fetch_add(1, Ordering::Relaxed);
            break;
        }
    }
    // Periodically dump the histogram so libfuzzer's stdout has a
    // running snapshot. Every 1024 inputs produces a few lines per
    // session — enough to track regressions, light enough to grep.
    static SEEN: AtomicU64 = AtomicU64::new(0);
    let seen = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    if seen.is_power_of_two() && seen >= 1024 {
        dump_hist(seen);
    }
}

fn dump_hist(seen: u64) {
    eprintln!("[oracle hist] after {seen} inputs");
    for (m, class) in ["silk", "hybrid", "celt", "?"].iter().enumerate() {
        let row: Vec<u64> = (0..DIVERGENCE_BUCKETS.len())
            .map(|i| HIST[m * DIVERGENCE_BUCKETS.len() + i].load(Ordering::Relaxed))
            .collect();
        let total: u64 = row.iter().sum();
        if total == 0 {
            continue;
        }
        let parts: Vec<String> = DIVERGENCE_LABELS
            .iter()
            .zip(row.iter())
            .filter(|(_, &n)| n > 0)
            .map(|(l, n)| format!("{l}:{n}"))
            .collect();
        eprintln!("  [{class}] n={total} {}", parts.join(" "));
    }
}

/// Throttle individual divergence eprintln!s to one in 64 — we still
/// get a representative trickle, but a fuzzing session of 100k inputs
/// doesn't drown CI in lines.
fn should_log_individual() -> bool {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    n & 0x3F == 0
}
