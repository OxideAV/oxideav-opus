# Changelog

All notable changes to `oxideav-opus` are recorded here.

## [Unreleased]

### Added

* **Clean-room round 1 (2026-05-20):** RFC 6716 §3.1 packet TOC byte
  parser. `OpusTocByte::parse` / `OpusTocByte::from_byte` decode the
  five-bit `config` against Table 2 (32 mode × bandwidth × frame-size
  tuples), the `s` stereo bit against the Table 3 prose, and the `c`
  frame-count code against the Table 4 prose (one frame /
  two-equal / two-unequal / arbitrary). `frame_count_range()` gives
  the implied `(min, max)` frame count without consulting further
  bytes (code 3 reports the legal `(1, 48)` range derived from
  §3.2.5's "no more than 120 ms total"). Five unit tests sweep the
  full enumeration plus the R1 empty-packet rejection.

### Changed

* **Orphan rebuild (2026-05-20).** The crate was reset to a clean-room
  scaffold. The prior implementation contained module-level docstrings
  and inline comments whose provenance could not be defended against
  the workspace clean-room rule. Orphan-master rebuild per workspace
  policy; no `old` branch retained. License also reset to clean MIT.

  Higher-level decode / encode paths still return
  `Error::NotImplemented`. A clean-room re-implementation of the
  SILK / CELT inner decoders, the §3.2 frame-packing layer, the §4
  range coder, and the §5 encoder pipeline against RFC 6716 +
  RFC 8251 + RFC 7587 + RFC 7845 is planned for subsequent rounds.
