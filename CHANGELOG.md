# Changelog

All notable changes to `oxideav-opus` are recorded here.

## [Unreleased]

### Added

* **Clean-room round 5 (2026-05-22):** RFC 6716 §4.2.7.4 SILK
  subframe quantization-gain decoder behind a new `SubframeGains` /
  `SubframeGainsConfig` API. Two coding paths:
  - Independent (Table 11 signal-type-conditioned 3-bit MSB +
    Table 12 uniform 3-bit LSB) joined into `gain_index ∈ 0..=63`
    and clamped with `log_gain = max(gain_index, previous_log_gain
    - 16)` per §4.2.7.4. The clamp is skipped when the caller
    passes no previous gain (decoder reset / side-channel
    previously uncoded / packet loss).
  - Delta (Table 13 41-symbol iCDF) folded into the previous gain
    via `log_gain = clamp(0, max(2*delta - 16, prev + delta - 4),
    63)`.
  The first subframe of a SILK frame uses the independent path
  iff the §4.2.7.4 enumeration triggers ("first SILK frame of its
  type for this channel in the current Opus frame, OR previous
  SILK frame of the same type was not coded"); every other
  subframe uses the delta path. Output is the integer `log_gain
  ∈ 0..=63` per subframe; the §4.2.7.4 tail-end conversion to
  `gain_Q16` via `silk_log2lin` is part of the excitation stage
  and not wired up here.
  20 new unit tests (88 total in the crate) covering PDF→iCDF
  transcription self-checks (Tables 11 / 12 / 13 each sum to
  256), all four `SignalType` → iCDF routings, the §4.2.7.4
  clamp behaviour across the four prev-value regimes (None, low,
  high, sub-16-saturate-to-zero), the delta path's dual-max +
  clamp formula reproduced against an independent range-decoder
  pass, end-to-end decode for mono-inactive 4-subframe,
  mono-unvoiced 2-subframe-with-prev, mono-voiced 4-subframe with
  high prev (asserting the floor clamp), the rejection of a
  pathological "first-subframe-delta without prev" config and
  num_subframes ∉ {2, 4}, and a four-subframe chain-consistency
  check that re-derives the gain chain from the raw PDF reads.

* **Clean-room round 4 (2026-05-21):** RFC 6716 §4.2.7.1 through
  §4.2.7.5.1 SILK frame-header decoder behind a new `SilkFrameHeader`
  type. The caller passes a `SilkFrameHeaderConfig` describing whether
  the current SILK frame is mid- or side-channel of a stereo Opus
  frame, whether the side channel is otherwise required (driving the
  presence of the mid-only flag), the frame kind (regular-inactive /
  regular-active / LBRR), and the SILK-layer audio bandwidth (NB / MB
  / WB). `SilkFrameHeader::decode` returns:
  - `stereo_pred: Option<StereoPredictionWeights>` per §4.2.7.1 with
    the three sub-symbols (Table 6 stage-1 25-cell, two stage-2
    3-cell, two stage-3 5-cell) composed into `(w0_Q13, w1_Q13)` per
    the spec formula and Table 7 weight table.
  - `mid_only_flag: Option<bool>` per §4.2.7.2 (Table 8 PDF
    `{192, 64}/256`).
  - `frame_type: u8` in `0..=5` per §4.2.7.3 (Table 9 inactive /
    active PDFs; active rows use a 4-entry tail-truncated iCDF with
    a +2 caller offset, since the §4.1.3.3 primitive cannot model
    leading-zero-mass cells).
  - `signal_type: SignalType`, `qoff_type: QuantizationOffsetType`
    decoded from `frame_type` via Table 10.
  - `lsf_stage1: u8` in `0..32` per §4.2.7.5.1 with the PDF chosen
    from Table 14 by `(bandwidth, signal_type)`.
  17 new unit tests (68 total in the crate) covering PDF→iCDF
  transcription self-checks (Tables 6 / 8 / 9 / 14 each sum to 256
  with monotone-decreasing iCDFs), the Table 7 weight-table symmetry
  (`w[15-k] == -w[k]`), the Table 10 frame-type → `(signal, qoff)`
  mapping, end-to-end decode against the range coder for mono
  inactive / mono active / stereo mid-channel / stereo side-channel
  / LBRR configurations, and a random-buffer sweep of
  `decode_stereo_pred` to confirm `wi0/wi1 ≤ 14` clamping keeps the
  Table 7 lookup in-bounds.
* **Clean-room round 3 (2026-05-21):** RFC 6716 §4.1 range decoder
  behind a new `RangeDecoder` API — the shared entropy primitive
  consumed by every SILK and CELT symbol. Implements §4.1.1
  initialization, §4.1.2 generic symbol decode, §4.1.2.1
  renormalization (with §4.1.4 zero-extension past EOF), §4.1.3.1
  `decode_bin` for power-of-two `ft`, §4.1.3.2 `dec_bit_logp` for
  `2^-logp` binaries, §4.1.3.3 `dec_icdf` for inverse-CDF tables,
  §4.1.4 `dec_bits` LSB-first raw bits from the END of the frame,
  §4.1.5 `dec_uint` (both small-ftb range-only and large-ftb
  range-plus-raw branches, with the corrupt-frame sticky error
  latch), §4.1.6.1 `tell()`, and §4.1.6.2 `tell_frac()` (with the
  `tell() == ceil(tell_frac() / 8.0)` identity holding across mixed
  operations). The sibling `oxideav-celt` crate carries an
  independent clean-room copy of the same primitive — both own
  their own copy until a shared low-level primitives crate exists.
  19 new unit tests (51 total in the crate).
* **Clean-room round 2 (2026-05-21):** RFC 6716 §3.2 frame-packing
  parser behind a new `OpusPacket::parse` entry point covering all
  four `c` codes:
  * Code 0 (§3.2.2) — one frame, remaining `N - 1` bytes.
  * Code 1 (§3.2.3) — two equal-size frames; R3 odd-payload rejection.
  * Code 2 (§3.2.4) — one- or two-byte §3.2.1 length sequence then
    `N1` + remainder; R4 length-exceeds-remaining rejection.
  * Code 3 (§3.2.5) — `M ∈ 1..=48` (R5) frame-count byte with the
    `v|p|M` bit layout; optional Opus padding with the §3.2.5
    255-byte-extension chain; CBR with R6 `R % M == 0` check; VBR
    with `M - 1` declared lengths and implicit final-frame size,
    R7 length-overrun rejection.
  * §3.2.1 length helper: `0` (DTX), `1..=251` single-byte,
    `252..=255` two-byte `(second * 4 + first)` up to 1275 (R2).
  Frame slices borrow from the input buffer via `OpusPacket::frames()
  -> &[&[u8]]`; padding length is exposed separately. Adds
  `Error::MalformedPacket` for §3.2 requirement violations. 27 new
  unit tests (32 total in the crate).
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
