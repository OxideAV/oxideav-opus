# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
