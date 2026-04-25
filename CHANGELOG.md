# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
