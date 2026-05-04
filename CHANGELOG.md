# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- Trim crate-level `#![allow]` block from 20 lints to 4. The 16 dropped
  lints (`useless_vec`, `collapsible_if`, `collapsible_else_if`,
  `nonminimal_bool`, `manual_range_contains`, `needless_late_init`,
  `needless_return`, `let_unit_value`, `needless_borrow`, `unused_mut`,
  `unused_variables`, `unused_assignments`, `unnecessary_cast`,
  `manual_memcpy`, `neg_multiply`, `precedence`) no longer fire because
  the underlying call sites have been cleaned up. Resulting `cargo
  clippy --all-targets -- -D warnings` is green with the trimmed allow
  set, so any future regression in those categories surfaces in CI
  rather than being silently masked.

## [0.0.7](https://github.com/OxideAV/oxideav-opus/compare/v0.0.6...v0.0.7) - 2026-05-03

### Other

- revert celt 0.2 → 0.1
- bump oxideav-celt 0.1 -> 0.2 for new AudioFrame layout
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- Hybrid 10 ms (configs 12 + 14) mono and stereo
- Hybrid stereo 20 ms (configs 13 + 15) and broadened SILK MB/WB stereo
- add Hybrid (SILK + CELT) encoder for mono 20 ms SWB / FB
- SNR-validate MB/WB SILK stereo encode + fill NB stereo durations
- cargo fmt cleanup of decoder/encoder/roundtrip tests
- adopt slim VideoFrame/AudioFrame shape

### Added

- Hybrid (SILK + CELT) **10 ms** mono and stereo encoders for SWB
  (config 12) and FB (config 14) per RFC 6716 §4.4. New named
  constructors `HybridEncoder::new_swb_mono_10ms`,
  `new_fb_mono_10ms`, `new_swb_stereo_10ms`, `new_fb_stereo_10ms`
  mirror the existing 20 ms variants but pick the SILK 2-sub-frame
  WB body (160 internal samples) and the CELT LM=2 (480-sample / 10 ms)
  short-block path via the new
  `oxideav_celt::encoder::CeltEncoder::new_with_frame_samples(_, 480)`
  constructor. The shared range-coded layout, SILK VAD/LBRR header,
  stereo prediction header, and CELT
  `start_band = 17` plumbing are unchanged from the 20 ms variant —
  10 ms is otherwise the same hybrid body, just at half the duration.
  RFC 6716 §3.2.1 1275-byte per-frame cap still applies.
- Hybrid (SILK + CELT) mono 20 ms encoder for SWB (config 13) and FB
  (config 15) per RFC 6716 §4.4. Runs SILK-WB on the 0..8 kHz low band
  and CELT (`start_band = 17`) on the 8..12 kHz / 8..20 kHz high band,
  sharing a single range-coded bitstream — the CELT body is appended
  to the in-flight `RangeEncoder` after the SILK body, mirroring the
  decoder's `decode_hybrid_frame` which uses the same arithmetic
  stream end-to-end. New named constructors: `HybridEncoder::new_swb_
  mono_20ms` and `new_fb_mono_20ms`.
- New CELT-encoder helper `CeltEncoder::encode_hybrid_body_mono(pcm,
  enc, start_band, end_band, budget_bytes)` — encodes a CELT body
  into a caller-supplied `RangeEncoder` with the given band range,
  threading all the §4.3 stages (coarse + fine energy, tf_decode,
  spread, dynalloc, allocator, PVQ, fine-finalise) through the
  `start_band..end_band` window. Mirrors `decode_celt_body` on the
  encoder side. Mono only for now.

### Tests

- Promote MB / WB stereo SILK 20 ms tests from "energy + smoke" to full
  SNR + channel-separation. Both clear the 20 dB bar on a 300 Hz
  L-sin / R-cos tone pair (MB ~36 / 31 dB, WB ~43 / 33 dB).
- Add NB stereo round-trip SNR + channel-separation tests at 10 / 40 /
  60 ms (configs 0 / 2 / 3 + stereo bit), filling out the duration
  matrix to match the existing 20 ms NB stereo coverage.
- Add hybrid_roundtrip integration tests covering Hybrid SWB + FB
  20 ms TOC sanity, low-band tone SNR (~24 dB), swept-sine
  band-energy survival in both the < 4 kHz and > 8 kHz regions, and
  silence-stays-quiet bounds.
- Extend hybrid_roundtrip with **10 ms** Hybrid coverage (configs
  12 / 14 SWB / FB, mono + stereo): TOC config sanity, swept-sine
  band-energy survival in both < 4 kHz and > 8 kHz regions, silence
  bounds, and libopus + ffmpeg cross-decode checks for every
  10 ms variant. 32 hybrid_roundtrip tests total (was 20).

## [0.0.6](https://github.com/OxideAV/oxideav-opus/compare/v0.0.5...v0.0.6) - 2026-04-25

### Other

- revert celt pin to 0.1
- pin release-plz to patch-only bumps

## [0.0.5](https://github.com/OxideAV/oxideav-opus/compare/v0.0.4...v0.0.5) - 2026-04-25

### Other

- bump oxideav-celt to 0.2
- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- promote hybrid decode from hidden to first-class + interop tests
- implement real RFC 4.2.7.8 shell-pulse coder
- add SILK encoder voiced-path round-trip tests
- wire SILK encoder voiced/LTP path end-to-end
- add SILK encoder pitch lag + LTP filter encode helpers
- add SILK encoder pitch analysis (autocorrelation)
- README + Cargo description for full SILK encode matrix
- full SILK-only config matrix (0-11) encoder
- clamp CELT post-filter head to `n.min(120)` for short frames
- add SILK 10 ms NB/MB/WB mono frame encoder
- add RFC 6716 Appendix A test-vector smoke test (ignored)
- multistream decoder (channel mapping family 1 + 2)
- decode Hybrid (SILK+CELT) frames per RFC 6716 §4.4
- decode-and-discard SILK LBRR bodies to keep range coder aligned
- add BSD-3-Clause attribution for libopus-derived code
- document SILK MB/WB/stereo encoder modes in README
- round-trip tests + stereo M/S scaling fix for SILK MB/WB/stereo
- add SILK MB/WB mono + NB stereo encoder modes
- factor SILK frame encoder into BandwidthParams descriptor (NB/MB/WB)

## [0.0.4](https://github.com/OxideAV/oxideav-opus/compare/v0.0.3...v0.0.4) - 2026-04-19

### Other

- restore integration tests + bump oxideav-ogg/celt deps to 0.1
- gate ogg-dep integration tests behind `ogg-tests` feature
- add SILK NB mono 20 ms encoder + fix LCG-seed ftb
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
- release v0.0.3

## [0.0.3](https://github.com/OxideAV/oxideav-opus/releases/tag/v0.0.3) - 2026-04-19

### Other

- claim WAVEFORMATEX tags via oxideav-codec CodecTag registry
- rewrite README + refresh crate-level doc to match reality
- exclude local /.cargo dev-only path-patch dir
- make crate standalone (pin deps, add CI + release-plz + LICENSE)
- add Decoder::reset overrides for Vorbis + Opus (+ FLAC note)
- move repo to OxideAV/oxideav-workspace
- add publish metadata (readme/homepage/keywords/categories)
- add CELT-only full-band encoder path
- address workspace-wide lints to unblock CI
- cargo fmt across the workspace
- SRT + WebVTT + ASS/SSA parsing + cross-format transforms
- CELT-only encoder wrapping oxideav-celt's mono long-block encoder
- SILK stereo + 40/60 ms decode
- SILK 10 ms frames + stereo CELT pin + scoped follow-ups
- add SILK decoder skeleton — NB/MB/WB mono 20ms routes to audio
- land full §4.3.3-§4.3.8 pipeline (allocator + PVQ + IMDCT + post-filter)
- bit-exact range decoder + §4.3.2 coarse energy + IFFT scaffold
- land CELT frame-header decode + name the next gap
- TOC byte + framing parser, silence/DTX/CELT-silence-flag decode
- multi-impl registry with capabilities, priority, and fallback
- add FLAC encoder, Opus crate, faithful Ogg page preservation
