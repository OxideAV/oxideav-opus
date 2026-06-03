# Changelog

All notable changes to `oxideav-opus` are recorded here.

## [Unreleased]

### Added

* **Clean-room round 31 (2026-06-03):** §4.3.3 per-band
  maximum-allocation parameter surface — a new `celt_cache_caps50`
  module delivering the `CACHE_CAPS50` lookup piece of RFC 6716 §4.3.3
  (pp. 113–114): the 168-byte bits/sample table the §4.3.3 per-band
  bit cap `cap[band] = ((cache_caps50[i] + 64) * channels * N) / 4`
  consumes (named `init_caps()` in RFC 6716 §4.3.3 p. 114). Round 24
  noted the §4.3.3 allocator as blocked on `cache_caps50` +
  `LOG2_FRAC_TABLE`; round 30 landed `LOG2_FRAC_TABLE`, and this round
  closes that pair by landing `CACHE_CAPS50` plus the `init_caps()`
  convert-rule helpers. New public surface: `CACHE_CAPS50: [u8; 168]`
  (168 Q0 bytes; layout `[LM ∈ 0..4][stereo ∈ {0,1}][band ∈ 0..21]`
  flattened by the §4.3.3 `i = nbBands * (2*LM + stereo) + band` rule
  with `nbBands = 21`, matching
  `docs/audio/celt/tables/cache_caps50.csv` row-for-row); shape
  constants `CACHE_CAPS50_LM_COUNT = 4`,
  `CACHE_CAPS50_STEREO_COUNT = 2`, `CACHE_CAPS50_TOTAL_BYTES = 168`;
  stereo-axis index constants `CACHE_CAPS50_STEREO_MONO = 0`,
  `CACHE_CAPS50_STEREO_STEREO = 1`; convert-rule constants
  `INIT_CAPS_BIAS = 64`, `INIT_CAPS_DIVISOR = 4`,
  `INIT_CAPS_MAX_CHANNELS = 2`; typed stereo-axis selector
  `CacheCapsStereo::{Mono, Stereo}` with `axis_index()` /
  `channels()` / `from_is_stereo(bool)` helpers (the `channels()`
  helper turns `Mono → 1` / `Stereo → 2` to feed the `init_caps()`
  multiplier independently of the axis index); typed accessors
  `cache_caps_value(lm, stereo, band) -> Result<u8, CacheCaps50Error>`
  and `cache_caps_row(lm, stereo) -> Result<&'static [u8],
  CacheCaps50Error>` over the lookup; flat-offset helper
  `cache_caps_offset(lm, stereo, band) -> usize` covering the §4.3.3
  row-stride rule; `init_caps(caps_value, channels, n_bins) -> u32`
  computing the §4.3.3 `(value + 64) * channels * N / 4` convert
  rule on a single byte; composite
  `cap_for_band_bits(lm, stereo, band, channels, n_bins) ->
  Result<u32, CacheCaps50Error>` performing lookup-plus-convert in
  one typed call; error variants
  `CacheCaps50Error::{LmOutOfRange, BandOutOfRange,
  ChannelsOutOfRange}`. Twenty-nine new unit tests (560 lib tests
  total, up from 531 at round-30 close; 20 integration tests
  unchanged, grand total 580) pin the table shape, the
  `INIT_CAPS_BIAS = 64` / `INIT_CAPS_DIVISOR = 4` /
  `INIT_CAPS_MAX_CHANNELS = 2` convert-rule constants, the
  `CACHE_CAPS50_STEREO_MONO = 0` / `CACHE_CAPS50_STEREO_STEREO = 1`
  axis indices, the `CacheCapsStereo::channels()` `Mono → 1` /
  `Stereo → 2` helper mapping, the `from_is_stereo(bool)` round-trip,
  eight CSV-cell spot-checks at `(row 0, band 0)` / `(row 1, band 20)`
  / `(row 2, band 0)` / `(row 3, band 8)` / `(row 4, band 12)` /
  `(row 5, band 17)` / `(row 6, band 20)` / `(row 7, band 0)`, the
  §4.3.3 `cache_caps_offset()` rule against every `(LM, stereo, band)`
  triple (168 cells) plus its two endpoints, the `cache_caps_value()`
  total-function sweep, the `cache_caps_row()` per-cell mirror, the
  `LmOutOfRange` / `BandOutOfRange` / `ChannelsOutOfRange` error
  paths on both accessors, four §4.3.3 `init_caps()` formula pins
  (including the `(caps=255, channels=2, N=192) → 30624` upper-bound
  cell and the floor-division corner at `caps ∈ {1,2,3}`), a
  `cap_for_band_bits()` composite cross-check against the manual
  lookup-plus-`init_caps()` sequence, the §4.3.3 narrative invariant
  that 20 ms stereo caps fit in `i16` but at least one exceeds
  `i8::MAX`, and two §4.3.3-reachable-cell pins (CELT-only 20 ms
  stereo band 0 → `caps = 204` → `cap = 134 * n_bins`; Hybrid 20 ms
  mono band 17 → `caps = 173` → `cap = (237 * n_bins) / 4`). The
  §4.3.3 bit allocation orchestration that consumes the `cap[]`
  vector (boost / trim / anti-collapse / skip / dual-stereo
  reservations, the Table 57 static allocation search, the
  reallocation / fine-vs-shape split / band-priority computation) is
  out of scope for this round.

* **Clean-room round 30 (2026-06-02):** §4.3.3 intensity-stereo
  reservation parameter surface — a new `celt_log2_frac_table` module
  delivering the `LOG2_FRAC_TABLE` lookup piece of RFC 6716 §4.3.3
  (p. 113): the 24-byte conservative `log2` table (in Q3 / 1/8-bit
  units) the §4.3.3 `intensity_rsv = LOG2_FRAC_TABLE[end − start]`
  reservation consumes. Round 24 noted the §4.3.3 allocator as blocked
  on `cache_caps50` + `LOG2_FRAC_TABLE`; this round delivers the
  smaller of the two table dependencies. New public surface:
  `LOG2_FRAC_TABLE: [u8; 24]` (24 Q3 bytes; layout
  `LOG2_FRAC_TABLE[coded_bands] = conservative_log2(coded_bands)` in
  1/8-bit units, matching `docs/audio/celt/tables/log2_frac_table.csv`
  row-for-row); shape constant `LOG2_FRAC_TABLE_LEN = 24`;
  unit-denominator constant `Q3_BITS_PER_WHOLE_BIT = 8`; typed
  accessor `log2_frac(coded_bands) -> Result<u8, Log2FracError>`
  with the §4.3.3 `coded_bands = end − start` indexing rule and a
  bounds check covering the `coded_bands ≥ 24` case; full-row
  borrow `log2_frac_row() -> &'static [u8; 24]`; error variant
  `Log2FracError::CodedBandsOutOfRange { coded_bands }`. Seventeen
  new unit tests (531 lib tests total, up from 514 at round-29
  close) pin the table shape, the `Q3_BITS_PER_WHOLE_BIT = 8` unit
  constant, seven CSV-row spot-checks at indices 0 / 1 / 2 / 4 / 14
  / 15 / 21 / 23 (covering the §4.3.3 base case, the 1-bit floor,
  the upward-rounded conservative entry, the Hybrid reachable
  index, the 32-byte plateau pair, the CELT-only reachable index,
  and the final entry), a monotone-non-decreasing property across
  every adjacent pair, a conservative-bound property
  `LOG2_FRAC_TABLE[n] ≥ 8 × floor(log2(n))` for every `n ∈ 1..24`
  (leading-zero-count formulation, no floats), a total-function
  sweep over every in-range index, the `CodedBandsOutOfRange` error
  paths, a row-vs-pair cross-check, and two §4.3.3-reachable-index
  sanity pins (CELT-only `end − start = 21` → `36` Q3; Hybrid
  `end − start = 4` → `19` Q3). The rest of the §4.3.3 allocation
  algorithm (anti-collapse / skip / dual-stereo reservations, the
  Table 57 static-allocation search, boost / trim decoding, and the
  `cache_caps50` per-band maximum vector) is out of scope.

* **Clean-room round 29 (2026-06-01):** §4.3.2.1 CELT coarse-energy
  Laplace-model parameter surface — a new `celt_e_prob_model` module
  delivering the parameter-surface piece of RFC 6716 §4.3.2.1
  (pp. 108–109): the per-`(LM, mode, band)` Q8 `{prob, decay}` table
  the §4.3.2.1 `ec_laplace_decode` routine consumes. Round 20's CELT
  pre-band header noted the §4.3.2.1 coarse energy as blocked on this
  table; this round delivers it plus the surrounding selector /
  accessor surface so the Laplace decoder and 2-D `(time, frequency)`
  predictor can be wired up against it next. New public surface:
  `E_PROB_MODEL: [[[u8; 42]; 2]; 4]` (336 Q8 bytes; layout
  `[LM ∈ 0..4][mode ∈ {inter, intra}][band × 2 + {prob, decay}]`,
  matching `docs/audio/celt/tables/e_prob_model.csv` row-for-row);
  shape constants `E_PROB_MODEL_LM_COUNT = 4`,
  `E_PROB_MODEL_MODE_COUNT = 2`, `E_PROB_MODEL_BYTES_PER_BAND = 2`,
  `E_PROB_MODEL_BYTES_PER_ROW = 42`,
  `E_PROB_MODEL_TOTAL_BYTES = 336`; inner-axis index constants
  `E_PROB_MODEL_MODE_INTER = 0`, `E_PROB_MODEL_MODE_INTRA = 1`;
  typed selector `EnergyPredictionMode::{Inter, Intra}` with
  `from_intra_flag(bool)` decode helper and a `table_index()`
  accessor; `EProbPair { prob: u8, decay: u8 }`; typed accessors
  `e_prob_pair(lm, mode, band) -> Result<EProbPair, EProbModelError>`
  and `e_prob_row(lm, mode) -> Result<&'static [u8; 42],
  EProbModelError>`; intra-case prediction-coefficient constants
  `INTRA_PRED_ALPHA_Q15 = 0` and `INTRA_PRED_BETA_Q15 = 4915` against
  `Q15_ONE = 32768` per RFC 6716 §4.3.2.1 p. 108
  (`4915 / 32768 ≈ 0.15`). Twenty-two new unit tests (514 lib tests
  total, up from 492 at round-28 close) pin the table shape, the Q8
  byte values at seven CSV-row spot-checks, the `EnergyPredictionMode`
  mapping, the `LmOutOfRange` / `BandOutOfRange` error paths, a
  total-function sweep over every `(LM, mode, band)` triple
  (4 × 2 × 21 = 168 cells), a pair-vs-row cross-check on every cell,
  and the §4.3.2.1 prediction-effectiveness sanity property
  (`intra_band0_prob < inter_band0_prob` for every LM). The
  §4.3.2.1 Laplace decoder itself, the 2-D `(time, frequency)`
  predictor application, and the §4.3.2.2 fine-energy follow-up are
  out of scope for this module. The per-LM *inter*-mode
  `(alpha, beta)` pair is a §4.3.2.1 docs gap (the RFC names them as
  "depend on the frame size in use" without giving numeric values);
  deferred until the docs side delivers the gap fill.

* **Clean-room round 28 (2026-06-01):** §4.5.1.4 redundant-CELT-frame
  decode parameters and cross-lap placement — a new
  `redundancy_decode_params` module encoding the two normative halves
  of RFC 6716 §4.5.1.4 (pp. 126–127). Half 1: the parameter-derivation
  rule (no TOC byte, 5 ms fixed duration via
  `REDUNDANT_FRAME_TENTHS_MS = 50`, inherited channel count,
  inherited bandwidth with the §4.5.1.4 "MB SILK → WB" exception via
  `apply_mb_to_wb_override`) bundled into
  `RedundantFrameParams { duration_tenths_ms, channels, bandwidth,
  position, size_bytes, cross_lap }`. Half 2: the cross-lap placement
  rule (`CrossLapPlacement::FirstHalfAsIs` for
  `RedundancyPosition::Beginning` — CELT → SILK/Hybrid carriers,
  where the redundant CELT frame's first 2.5 ms replace the carrier's
  leading 2.5 ms and the second 2.5 ms cross-lap;
  `CrossLapPlacement::SecondHalfAsIs` for `RedundancyPosition::End`
  — SILK/Hybrid → CELT carriers, where only the second 2.5 ms is
  used and it cross-laps with the SILK/Hybrid trailing edge).
  `redundant_frame_params(routing, decision)` is the pure-function
  driver entry; `RedundancyDecision::Invalid` (the §4.5.1.3 overflow
  outcome) and `RedundancyDecision::NotPresent` both route to `None`.
  Twenty-five new unit tests (492 lib tests total, up from 467 at
  round-27 close) pin every rule, cross-check four §4.5.3 Figure 18
  transition rows, and sweep the total-function output for the
  MB-bandwidth invariant.

* **Clean-room round 27 (2026-05-31):** §4.5.2 SILK + CELT decoder
  state-reset policy across mode transitions — a new
  `mode_transition_reset` module encoding the four normative rules
  of RFC 6716 §4.5.2 (p. 127) as a pure decision function
  `decide_state_resets(prev_mode, next_mode, redundancy) ->
  StateReset { silk, celt: CeltResetPlacement }`. Rule 1 resets
  SILK on every CELT-only → SILK-only/Hybrid transition; rule 2
  resets CELT on every mode change into Hybrid or CELT-only except
  when redundancy is used; rule 3 places the CELT reset *before
  the redundant CELT frame* on SILK/Hybrid → CELT-only with
  redundancy and skips it before the following CELT-only frame;
  rule 4 suppresses the CELT reset on CELT-only → SILK/Hybrid
  with redundancy. `RedundancyDecision::Invalid` (the §4.5.1.3
  overflow outcome) is treated as no usable redundancy.
  `CeltResetPlacement::{None, BeforeFrame, BeforeRedundantOnly}`
  plus `StateReset::{celt_resets, is_noop}` accessors round out
  the public surface. Twenty-seven new unit tests (467 lib tests
  total) pin every cell of the 3×3 mode-pair × redundancy
  cross-product and cross-check four §4.5.3 Figure 18 transition
  rows.

## [0.0.11](https://github.com/OxideAV/oxideav-opus/compare/v0.0.10...v0.0.11) - 2026-05-30

### Other

- §4.3.4.5 CELT TF-resolution adjustment lookup (round 25)

### Added

* **Clean-room round 26 (2026-05-30):** §4.5.1 CELT redundancy /
  mode-transition side-information decoder — a new
  `celt_redundancy` module owning RFC 6716 §4.5.1.1–§4.5.1.3
  (Tables 64 and 65). Exposes
  `decode_redundancy(rd, mode, opus_frame_bytes) -> RedundancyDecision`
  routing CELT-only Opus frames to a no-op bypass, SILK-only Opus
  frames through the §4.5.1.1 implicit 17-bit remaining-budget
  gate + §4.5.1.2 Table 65 `{1,1}/2` position symbol + §4.5.1.3
  "remaining whole bytes" size, and Hybrid Opus frames through
  the §4.5.1.1 explicit 37-bit gate + Table 64 `{4095,1}/4096`
  flag + Table 65 position symbol + §4.5.1.3 `2 + dec_uint(256)`
  size with the "claimed > whole bytes remaining" overflow
  routed to `RedundancyDecision::Invalid`. Also lands
  `RedundancyPosition::{End, Beginning}` for the §4.5.1.2 placement
  decision, `RedundancyDecision::{NotPresent, Present {position,
  size_bytes}, Invalid}` for the three legal outcomes, named
  constants `SILK_ONLY_REDUNDANCY_MIN_REMAINING_BITS = 17` /
  `HYBRID_REDUNDANCY_MIN_REMAINING_BITS = 37` /
  `REDUNDANCY_FLAG_ICDF = [1, 0]` / `REDUNDANCY_FLAG_ICDF_FTB = 12`
  / `REDUNDANCY_POSITION_ICDF = [1, 0]` /
  `REDUNDANCY_POSITION_ICDF_FTB = 1` /
  `HYBRID_REDUNDANCY_SIZE_BASELINE_BYTES = 2` /
  `HYBRID_REDUNDANCY_SIZE_DEC_UINT_FT = 256` /
  `REDUNDANCY_MIN_SIZE_BYTES = 2`, and helper accounting
  functions `remaining_bits(rd, opus_frame_bytes)` /
  `whole_bytes_remaining(rd, opus_frame_bytes)` per §4.1.6 +
  §4.5.1.3.

  Round 26 stops at the §4.5.1 boundary metadata (where the
  redundant CELT bytes start, how many of them there are); the
  decode of the redundant CELT frame itself needs §4.3.2.1
  coarse energy (#936, Laplace decoder + `e_prob_model`) and
  §4.3.3 bit allocation (#943, `cache_caps50` + `LOG2_FRAC_TABLE`).

  Twelve new unit tests cover the SILK-only implicit-flag boundary
  (below 17 bits → not present; full buffer → present with
  remaining-whole-bytes size; size-equals-whole-bytes-remaining
  invariant), the Hybrid 37-bit gate (below → not present; full
  buffer → Table 64 read advances `tell()`), the CELT-only bypass
  invariant, the Table 64 / Table 65 ICDF derivations from the
  RFC PDFs, the `RedundancyPosition::from_symbol` Table 65 mapping
  (including the defensive non-binary fall-through), the
  `RedundancyDecision` accessor helpers, the §4.1.6 + §4.5.1.3
  `remaining_bits` / `whole_bytes_remaining` helper accounting,
  and a sanity check on every named constant against the RFC text.
  Decoder runs to 440 unit tests + 20 integration tests, all passing.

* **Clean-room round 25 (2026-05-30):** §4.3.4.5 CELT TF-resolution
  adjustment lookup — a new `celt_tf_adjust` module owning RFC 6716
  Tables 60–63 (the four `(frame_size, choice) -> i8` adjustment
  tables keyed by `(transient, tf_select)`). Exposes
  `celt_tf_adjustment(frame_size, transient, tf_select, tf_change)
  -> i8` (the routed lookup, return value `∈ [-3, 3]`),
  `celt_tf_select_can_affect(frame_size, transient, &[bool]) -> bool`
  (the §4.3.1 "tf_select is only decoded if it can have an impact on
  the result knowing the value of all per-band tf_change flags"
  redundancy gate that the §4.3.4.5 band loop calls AFTER decoding
  every per-band `tf_change[b]`), `TfDirection::{Unchanged,
  IncreaseTime(N), IncreaseFrequency(N)}` carrying the §4.3.4.5
  Hadamard-transform branch and level count, `TfAdjustment` (= `i8`)
  storage type, and named constants `TF_ADJUSTMENT_MAX = 3`,
  `TF_ADJUSTMENT_ABS_MAX = 3`, plus the four tables themselves
  (`TF_ADJ_NONTRANSIENT_SELECT0`, `TF_ADJ_NONTRANSIENT_SELECT1`,
  `TF_ADJ_TRANSIENT_SELECT0`, `TF_ADJ_TRANSIENT_SELECT1`) for direct
  inspection.

  This is the lookup the §4.3.4.5 band loop downstream consumes once
  it's wired up (gated on §4.3.2.1 coarse energy and §4.3.3 bit
  allocation, both still deferred); it sits between Table 55's
  per-band MDCT-bin count and the §4.3.4.2 PVQ shape decoder.

  Twenty-seven new module tests (428 lib tests total, up from 401 at
  round-24 close; 20 integration tests unchanged, grand total 448).
  Coverage: every table's `4 × 2` shape; every cell `∈ [-3, 3]`; all
  32 cells across Tables 60 / 61 / 62 / 63 hand-pinned to RFC 6716
  §4.3.4.5; the "non-transient `choice = 0` is always 0" invariant;
  the "non-transient `choice = 1` is always ≤ 0" invariant; the
  "positive adjustments only on transient frames" §4.3.4.5
  asymmetry pinned at both the table and `TfDirection` layer; the
  Table 62 `choice = 0` monotone `0, 1, 2, 3` scale across frame
  sizes; the universal 2.5 ms `[0, -1]` row across all four tables;
  `TF_ADJUSTMENT_MAX` and `TF_ADJUSTMENT_ABS_MAX` matching the
  observed max over every cell; `celt_tf_adjustment` entry-point
  routing each `(transient, tf_select)` corner; `celt_tf_select_can_affect`
  returning `false` on empty band sets and on the redundant 2.5 ms
  rows; returning `false` on 10 ms non-transient with all-`choice 0`
  bands and `true` as soon as any band picks `choice = 1`;
  returning `true` on 20 ms transient for any non-empty band set;
  `TfDirection::from_adjustment` classification + `levels()` value
  matching `adj.unsigned_abs()` over `[-3, 3]`;
  `IncreaseFrequency` never reachable on non-transient frames;
  `IncreaseTime` always reached for non-transient `choice = 1`.

## [0.0.10](https://github.com/OxideAV/oxideav-opus/compare/v0.0.9...v0.0.10) - 2026-05-29

### Other

- §4.3 Table 55 CELT MDCT-band layout (round 24)
- §4.2.7.4 SILK gain dequant tail (silk_log2lin) — round 23
- §3.4 R1..R7 malformed-input rejection audit (round 22)
- §3.1 / §4.2 framing dispatch (round 21)
- land §4.3 Table 56 CELT pre-band header symbols
- Round 19: §4.2.9 SILK resampler delay budget + sample-rate accounting
- round 18: §4.2.3 SILK header bits + §4.2.4 per-frame LBRR flags
- §4.2.8 stereo unmixing — silk_stereo_MS_to_LR (round 17)
- fix the round-16 test count phrasing
- §4.2.7.9.1 LTP synthesis filter (round 16)
- round 15 — RFC 6716 §4.2.7.9.2 LPC synthesis filter
- §4.2.7.7 LCG seed + §4.2.7.8 excitation reconstruction
- §4.2.7.6 LTP parameters (pitch lags + LTP filter + LTP scaling)
- §4.2.7.5.8 LPC prediction-gain stability limiting
- §4.2.7.5.7 LPC range-limiting bandwidth expansion
- round 10: RFC 6716 §4.2.7.5.6 SILK NLSF→LPC core conversion
- clean-room round 9 — SILK §4.2.7.5.5 NLSF interpolation
- round 8: RFC 6716 §4.2.7.5.4 SILK NLSF stabilization
- round 7: RFC 6716 §4.2.7.5.3 SILK NLSF reconstruction
- round 6: RFC 6716 §4.2.7.5.2 SILK LSF Stage-2
- round 5 fix: compare iCDF slices by value, not pointer identity
- round 5: RFC 6716 §4.2.7.4 SILK subframe gains
- round 4: RFC 6716 §4.2.7.1–§4.2.7.5.1 SILK frame header
- round 3: RFC 6716 §4.1 range decoder
- round 2: RFC 6716 §3.2 frame-packing parser
- round 1: RFC 6716 §3.1 packet TOC byte parser
- orphan rebuild: clean-room scaffold post 2026-05-20 audit

### Added

* **Clean-room round 24 (2026-05-29):** §4.3 CELT MDCT-band layout —
  a new `celt_band_layout` module owning RFC 6716 Table 55 (the
  21-band partition with per-band MDCT bin counts at 2.5 / 5 / 10 /
  20 ms and the `0..=20000 Hz` band-edge frequencies) and the §4.3
  "first 17 bands (up to 8 kHz) are not coded in Hybrid mode" rule.
  Exposes `CeltFrameSize` (the four CELT frame-size variants whose
  `repr(u8)` discriminants double as Table 55 column index `0..=3`,
  plus `from_frame_tenths_ms` / `to_frame_tenths_ms`),
  `celt_band_bins_per_channel(band, fs) -> Option<u16>`,
  `celt_band_start_hz(b)` / `celt_band_stop_hz(b)` band edges,
  `celt_band_at_hz(hz) -> Option<usize>` reverse lookup,
  `celt_first_coded_band(is_hybrid)` / `celt_end_coded_band()`
  iterator bounds, `celt_total_bins_per_channel(fs, is_hybrid)`
  column-sum helper, and named constants `CELT_NUM_BANDS = 21`,
  `HYBRID_FIRST_CODED_BAND = 17`, `CELT_MAX_BINS_PER_BAND = 176`.
  The "Custom" mode of §6.2 is explicitly out of scope.

  This is the layout the deferred §4.3.2 coarse-energy decoder,
  §4.3.3 bit allocator, §4.3.4 PVQ shape decoder, §4.3.6
  denormalisation, and §4.3.7 inverse MDCT all need before any band
  loop can iterate.

  Twenty new module tests (401 lib tests total, up from 381 at
  round-23 close; 20 integration tests unchanged) cover: full-band
  start / stop pin (band 0 starts at 0 Hz, band 20 stops at 20 kHz);
  adjacent bands tile without gaps (`stop(b) == start(b + 1)` for
  every `b ∈ 0..=19`); positive band widths everywhere; the
  power-of-two column-scaling invariant (`column(c) == 1 << c *
  column(0)` per band); every cell `∈ [1, 176]` per the §4.3 prose;
  hand-pinned Table 55 cells at bands 0 / 8 / 12 / 15 / 17 / 20;
  band-edge spot pins at start, the §4.3 Hybrid boundary
  (`stop(16) = 8000` = `start(17)`), and tail (`stop(20) = 20000`);
  out-of-range band index returns `None`;
  `CeltFrameSize::from_frame_tenths_ms` round-trip with explicit
  SILK-only rejection (400 / 600 ms); discriminant-vs-column-index
  agreement; the Hybrid-vs-CELT-only first-coded-band split with the
  8 kHz boundary pin; `celt_total_bins_per_channel` column-sum
  agreement against an independent iterator sum for each mode; strict
  `hybrid_total < celt_only_total` invariant; pinned CELT-only column
  sums (100 / 200 / 400 / 800) and Hybrid column sums (60 / 120 /
  240 / 480); `celt_band_at_hz` round-trip against the band-edge
  triple (start, midpoint, `stop - 1` all land on the same band);
  `>= 20 kHz` rejection of `celt_band_at_hz`;
  `celt_band_at_hz(8000) == 17` lining up with
  `HYBRID_FIRST_CODED_BAND`; multiple-of-200-Hz band-width
  invariant with three pinned widths (200 Hz for band 0, 400 Hz for
  band 8, 4400 Hz for band 20); `CELT_MAX_BINS_PER_BAND == max(every
  cell)`.

* **Clean-room round 23 (2026-05-29):** §4.2.7.4 SILK gain
  dequantization tail — a new `silk_log2lin` module exposing
  `silk_log2lin` (the spec's piecewise-linear approximation of
  `2^(inLog_Q7/128)`) and `silk_gains_dequant` (the composed
  `log_gain ∈ 0..=63 → gain_Q16` pipeline
  `silk_log2lin((0x1D1C71*log_gain >> 16) + 2090)`), plus the named
  constants `SILK_LOG_GAIN_MULTIPLIER`, `SILK_LOG_GAIN_BIAS`,
  `SILK_GAIN_Q16_MIN = 81_920`, `SILK_GAIN_Q16_MAX = 1_686_110_208`.
  Also adds a `SubframeGains::dequant_q16` convenience that maps an
  entire decoded frame's `log_gain[]` into the fixed-size `[u32;
  SILK_MAX_SUBFRAMES]` array consumed by the §4.2.7.9.1 LTP and
  §4.2.7.9.2 LPC synthesis filters (with trailing unused slots left
  at zero for two-subframe frames).

  This closes the §4.2.7.4 tail-end conversion that was previously
  noted as deferred since round 5; the §4.2.7.9 synthesis filters
  already accept a `gain_q16` input but were missing the official
  RFC-spec mapping from the decoded `log_gain` integer to the linear
  Q16 gain.

  Nineteen new module tests (381 lib tests total, up from 362 at
  round-22 close, plus the 20 integration tests unchanged) cover:

  * The §4.2.7.4 documented endpoints — `log_gain = 0` returns
    `81920` (= 1.25 in linear scale) and `log_gain = 63` returns
    `1_686_110_208` (≈ 25 728 in linear).
  * Strict monotonicity: `silk_gains_dequant(g+1) > silk_gains_dequant(g)`
    for every `g ∈ 0..=62`.
  * Spec-range invariant across the full `0..=63` sweep.
  * Pure-power-of-two collapse: `silk_log2lin(128*i) == 1<<i` for
    `i ∈ 0..=30`.
  * `silk_log2lin(0) == 1` and `silk_log2lin(1) == 1` (the
    approximation can't resolve sub-128 Q7 below `i = 7`).
  * Pinned `silk_log2lin(7*128 | 64) = 181` — exact match of the
    §4.2.7.4 approximation against the true `2^7.5 ≈ 181.019…`
    halfway between `2^7` and `2^8`, exercising both the `bowed`
    correction and the linear term.
  * Independent i64 oracle of the §4.2.7.4 formula matched bit-for-bit
    by the production i32 implementation across (a) every reachable
    `inLog_Q7` from the `log_gain` dequant pipeline and (b) the full
    `i ∈ 0..=30 × f ∈ 0..=127` Q7 domain.
  * Endpoint algebra pinned independently of `silk_gains_dequant`:
    `log_gain = 0 → in_log_q7 = 2090`; `log_gain = 63 → in_log_q7 =
    3923` (= `30*128 + 83`), `silk_log2lin(2090) = 81_920`,
    `silk_log2lin(3923) = 1_686_110_208`.
  * `SubframeGains::dequant_q16` leaves trailing slots at zero for a
    two-subframe frame and matches per-subframe `silk_gains_dequant`
    calls across the four-subframe frame.

* **Clean-room round 22 (2026-05-27):** §3.4 R1..R7 malformed-input
  rejection audit — a dedicated integration-level test file
  (`tests/malformed_input.rs`, 20 tests) that pins every concrete
  failure mode RFC 6716 §3.4 enumerates for a malformed packet, plus
  property-style sweeps proving the §4.2.3 / §4.2.4 SILK header
  decoder is panic-free on any truncation of a previously-valid
  bitstream (§4.1.4 zero-extension contract). Covers R1 (empty
  packet), R2 (frame > 1275 B for codes 0 and 1, plus the §3.2.1
  boundary at 1275 B), R3 (code-1 odd body length), R4 (code-2
  length-byte truncations + length > remaining + DTX boundary), R5
  (`M = 0` + `M > 48` rejection), R6 (CBR `R % M != 0`), R7 (VBR
  declared length overrun), §3.2.5 padding-chain pathologies, TOC
  total-function self-consistency, §4.2.3 / §4.2.4 truncation
  panic-freeness across `(num_silk_frames, stereo)` × prefix-length
  1..=32, §4.2.4 LBRR-bitmap-never-zero invariant for 40 / 60 ms,
  mono channel never emits side state, parsed-frame slice
  lifetimes, and a code × body-len short-packet panic sweep. Test
  totals: 362 lib + 20 integration = 382 (was 362 lib + 0
  integration after round 21).

* **Clean-room round 21 (2026-05-27):** §3.1 / §4.2 framing dispatch —
  a new `framing` module exposing `OpusFrameRouting`, `OperatingMode`,
  and `SilkBandwidth`. `OpusFrameRouting::from_toc` turns the parsed
  `OpusTocByte` into the per-Opus-frame dispatch decision a §4 decoder
  needs *before* it touches the range coder:

  * `operating_mode` — SilkOnly / Hybrid / CeltOnly from §3.1 Table 2.
  * `silk_layer` / `celt_layer` — which layers are present.
  * `silk_bandwidth` — internal SILK bandwidth (NB / MB / WB), pinned
    to WB for every Hybrid frame regardless of the TOC's SWB / FB per
    RFC 6716 §4.2 first paragraph ("the LP layer itself still only runs
    in WB").
  * `silk_frames_per_channel` — §4.2.2 LP-layer organisation (1 for
    10 / 20 ms Opus frames, 2 for 40 ms, 3 for 60 ms; `None` for
    CELT-only).
  * `total_silk_frames` — channel-count × per-channel SILK frames.
  * `has_per_frame_lbrr_bits` — §4.2.4 duration gate (true for SILK-
    bearing frames longer than 20 ms).

  Thirteen new unit tests cover the SILK-only Table 2 row-by-row
  expectations (12 cells), the Hybrid WB-pin, the CELT-only frames
  sweep across mono/stereo, the §4.2.4 per-frame LBRR gate against
  every Table 2 cell, the `total_silk_frames` formula across all 32
  configs × {mono, stereo}, a 60 ms stereo SILK-only worked example,
  the `c`-bit independence of the routing, the channel-mapping
  pass-through, and the `silk_layer ⇔ silk_bandwidth.is_some() ⇔
  silk_frames_per_channel.is_some()` invariants across the entire
  Table 2 grid.

* **Clean-room round 20 (2026-05-26):** first CELT-layer fragment —
  the RFC 6716 §4.3 / Table 56 pre-band header symbols behind a new
  `celt_header` module (`CeltHeaderPrefix` / `CeltPostFilter`).

  * `silence` — `dec_icdf` against the 2-entry `{32767, 1}/32768`
    iCDF `[1, 0]` at ftb=15. When the flag fires the rest of the
    CELT prefix is force-defaulted per the §4.3 shortcut (no
    post-filter, no transient, no intra).
  * §4.3.7.1 pitch post-filter parameter group: one `dec_bit_logp(1)`
    enable bit, then on the enabled branch `octave` via
    `dec_uint(6)` (uniform on `0..=5`), the `4 + octave` raw-bit
    `fine_pitch` field through `dec_bits`, the §4.3.7.1 pitch-period
    reconstruction `T = (16 << octave) + fine_pitch - 1` (bounded
    `15..=1022`), the 3-bit `gain_index` raw field whose downstream
    gain is `G = 3 * (gain_index + 1) / 32`, and the §4.3.7.1
    `tapset` `{2, 1, 1}/4` iCDF `[2, 1, 0]` at ftb=2.
  * §4.3.1 `transient` and §4.3.2.1 `intra` flags via
    `dec_bit_logp(3)` (PDF `{7, 1}/8`).
  * This is the only Table-56 segment that fits between the SILK
    pipeline already wired up and the §4.3.2.1 coarse-energy
    (Laplace decoder + `e_prob_model` table, gated on a docs gap)
    / §4.3.3 bit allocation (`cache_caps50` + `LOG2_FRAC_TABLE`,
    also gated on a docs gap) sub-pieces; the per-band `tf_change`
    symbols (§4.3.1) live in the band loop and are decoded after
    `coarse energy` per Table 56, so they're deferred as well.
  * Ten new unit tests cover the iCDF transcription self-checks
    (silence PDF sums to 32768, tapset PDF sums to 4), the pitch
    period formula at the global minimum (15), maximum (1022), and
    per-octave boundaries, an all-zero buffer (most-likely symbol
    on every branch ⇒ no silence / no post-filter / no transient /
    no intra), an all-ones buffer (every produced field stays in
    its declared range), a `tell()`-advance proof, a 256-buffer
    fuzz-style range sweep over the post-filter fields, and the
    silence-shortcut post-condition.

* **Clean-room round 19 (2026-05-26):** RFC 6716 §4.2.9 SILK resampler
  delay budget and the internal-vs-output sample-rate accounting
  behind a new `silk_resampler` module
  (`silk_resampler_delay_ms` / `silk_resampler_delay_samples_at` /
  `silk_internal_rate_hz` / `silk_frame_samples_internal` /
  `silk_frame_samples_at_output` / `is_supported_output_rate` /
  `SUPPORTED_OUTPUT_RATES_HZ` / `REFERENCE_RATE_HZ` plus the
  `SILK_RESAMPLER_DELAY_MS_{NB,MB,WB}` constants).

  * Table 54 normative delay allocation: NB = 0.538 ms, MB = 0.692 ms,
    WB = 0.706 ms. The §4.2.9 resampler itself is non-normative ("a
    decoder can use any method it wants to perform the resampling"),
    but the delay budget is normative so the encoder can apply a
    matching pre-delay and keep SILK/CELT aligned across a §4.5 mode
    switch. SWB and FB never reach the §4.2.9 SILK stage and return
    `None`.
  * Internal SILK sample rates per bandwidth (NB = 8 kHz, MB = 12 kHz,
    WB = 16 kHz) and per-frame sample-count accounting at both the
    internal rate and any output rate. NB 20 ms = 160 internal samples
    or 960 output samples at 48 kHz; MB 20 ms = 240 / 960; WB 20 ms =
    320 / 960.
  * The five §4.2.9 supported output rates (8 / 12 / 16 / 24 / 48 kHz),
    the rates "the reference implementation is able to resample to …
    within or near this delay constraint".
  * Delay-samples helper rounds half away from zero per the §4.2.9
    caveat that exact whole-sample delays may be unachievable at
    arbitrary output rates.

  18 new module tests (339 lib tests total, up from 321): Table 54
  transcription + SWB/FB exclusion + strict NB < MB < WB monotonicity;
  Table 54 expansion to 48 kHz samples (26 / 33 / 34) and the
  internal-rate / 24 kHz intermediate-rate expansions; SWB / FB /
  zero-rate rejections; the §4.2.9 supported-output-rate set plus a
  sweep of unsupported rates; the SILK internal rate per bandwidth
  and its membership in the supported-output set; canonical per-frame
  sample counts at internal + output rates plus rejection of
  non-SILK durations; and a cross-check that the Table 54 delay is
  strictly less than one 10 ms SILK frame at every supported output
  rate × every SILK bandwidth.

* **Clean-room round 18 (2026-05-26):** RFC 6716 §4.2.3 SILK
  packet-level header bits and §4.2.4 per-frame LBRR flags behind a
  new `SilkHeaderBits` / `SilkChannelHeader` / `PerFrameLbrr` /
  `silk_frame_count` API (`silk_header` module). The decoder reads
  the §4.2.2 Figures 15/16 prefix:

  * Per channel (mono: 1; stereo: 2): `N` uniform-binary VAD bits
    plus one global LBRR flag via `RangeDecoder::dec_bit_logp(1)`,
    where `N` is the SILK-frame count from §4.2.2 (1 for 10/20 ms
    Opus frames, 2 for 40 ms, 3 for 60 ms).
  * For Opus frames > 20 ms with the channel's global LBRR flag set,
    one §4.2.4 per-frame LBRR symbol from Table 4
    (`{0, 53, 53, 150}/256` for 40 ms or
    `{0, 41, 20, 29, 41, 15, 28, 82}/256` for 60 ms). Both PDFs have
    a leading zero entry per §4.1.3.3, so the iCDF tables
    (`PER_FRAME_LBRR_{40MS,60MS}_ICDF`) drop the leading zero and the
    helper adds offset 1, producing a 2- or 3-bit LBRR bitmap packed
    LSB-to-MSB (bit `i` ↔ SILK frame `i`).
  * For Opus frames ≤ 20 ms the per-frame LBRR bitmap mirrors the
    global LBRR flag without consuming any extra bits, per §4.2.4.

  Output is a `SilkHeaderBits` with per-channel VAD bitmaps, global
  LBRR flags, and a fully-expanded `PerFrameLbrr` bitmap for the
  downstream §4.2.5 / §4.2.6 LBRR + regular SILK loop.

  14 new module tests (321 lib tests total, up from 307): Table 4
  PDF/iCDF transcription self-checks (40 ms + 60 ms, with
  strictly-decreasing + terminator-zero invariants); `per_frame_lbrr_pdf`
  dispatch fallback; `silk_frame_count` §4.2.2 dispatch including the
  2.5/5 ms CELT-only `None` arm; mono 10 ms decode consumes exactly
  2 bits; stereo 60 ms decode populates 3-bit bitmaps within range;
  rejection of `num_silk_frames ∉ {1, 2, 3}`; the §4.2.3-implied
  per-frame mirror on 10 ms with the global flag set (verifying no
  extra symbol consumed); the §4.2.4 skip path on 60 ms with both
  global flags unset (verifying exactly 8 bits consumed); VAD / LBRR
  accessors for present-side and missing-side cases; exhaustive 40 ms
  and 60 ms `decode_per_frame_lbrr` symbol-range sweeps plus a 60 ms
  full-coverage sweep over `{1..=7}`.

* **Clean-room round 17 (2026-05-25):** RFC 6716 §4.2.8 SILK stereo
  unmixing (`silk_stereo_MS_to_LR`) behind a new `stereo_ms_to_lr` /
  `StereoUnmixState` / `StereoWeightsQ13` / `StereoFrame` API
  (`silk_stereo` module). Converts the decoded mid/side `out[]` signals
  into left/right:

  * `p0 = (mid[i-2] + 2*mid[i-1] + mid[i]) / 4.0` is the low-passed,
    one-sample-delayed mid term; the unfiltered mid is also delayed one
    sample (`mid[i-1]`).
  * `left[i] = clamp(-1.0, (1+w1)*mid[i-1] + side[i-1] + w0*p0, 1.0)`,
    `right[i] = clamp(-1.0, (1-w1)*mid[i-1] - side[i-1] - w0*p0, 1.0)`.
  * Phase 1 (first `n1` = 64 NB / 96 MB / 128 WB samples) interpolates
    the §4.2.7.1 Q13 weights from the previous frame's
    `(prev_w0_Q13, prev_w1_Q13)` to the current frame's; phase 2 uses
    the current weights.
  * An uncoded side channel (§4.2.7.2 mid-only flag) is treated as
    all-zero.
  * `StereoUnmixState` carries the two trailing mid samples, one
    trailing side sample, and the previous-frame weights across the
    frame boundary, cleared to zero on a decoder reset per §4.2.8.

  9 module tests: the `interp_phase_samples` table, fresh/reset state,
  empty/length validation, zero-weight delayed-mono collapse, a
  hand-computed constant-weight mid/side reconstruction, phase-1 ramp
  endpoints, mid- and side-history carry across frame boundaries, and
  output clamping.

* **Clean-room round 16 (2026-05-25):** RFC 6716 §4.2.7.9.1 SILK LTP
  synthesis filter behind a new `ltp_synthesis_subframe` /
  `ltp_synth_commit_subframe` / `LtpSynthState` / `LtpSynthSubframe`
  API (`silk_ltp_synth` module). Two regimes:

  * **Unvoiced** (`signal_type != Voiced`): `res[i] = e_Q23[i] / 2^23`
    (a normalised copy of the §4.2.7.8 excitation).
  * **Voiced**: 5-tap Q7 LTP convolution
    `res[i] = e_Q23[i]/2^23 + Σ_{k=0..4} res[i - pitch_lag + 2 - k] *
    b_Q7[k]/128`, where the "prior res[]" values come from rewhitening
    the previous subframes' outputs through the current subframe's LPC
    coefficients. Two rewhitening regions:
    * Region A (out[] rewhiten, `(j - pitch_lag - 2) <= i < out_end`):
      `res[i] = 4*LTP_scale_Q14/gain_Q16 * clamp(out[i] - Σ
      out[i-k-1] * a_Q12[k]/4096, -1, 1)`.
    * Region B (lpc[] rewhiten, `out_end <= i < j`):
      `res[i] = 65536/gain_Q16 * (lpc[i] - Σ lpc[i-k-1] *
      a_Q12[k]/4096)`.

  `out_end` and the effective `LTP_scale_Q14` follow the §4.2.7.9.1
  LSF-interpolation-split branch: third/fourth subframe of a 20 ms
  SILK frame with `w_Q2 < 4` ⇒ `out_end = j - (s-2)*n` and
  `LTP_scale_Q14 = 16384`; otherwise `out_end = j - s*n` and the
  §4.2.7.6.3 decoded scaling factor is used.

  `LtpSynthState` carries 306 samples of `out[]` history (`lag_max
  288 + d_LPC 16 + 2`) and 256 samples of `lpc[]` history (`3 prior
  WB subframes 240 + d_LPC 16`) — the buffer sizes called out in the
  §4.2.7.9.1 paragraphs. `reset()` clears both for the §4.5.2
  decoder-reset / uncoded-side-channel-frame paths;
  `ltp_synth_commit_subframe` pushes the §4.2.7.9.2 outputs back into
  the state for the next subframe's rewhitening.

  Twenty-one new unit tests (298 lib tests total) cover the
  spec-stated buffer-size constants, `LtpSynthState` d_LPC routing +
  zero-init + reset + start_frame + push_subframe ordering, the
  unvoiced normalised-excitation identity (NB / Wb sweeps with
  Inactive and Unvoiced both routed to the unvoiced path), four
  input-validation rejections (mismatched lengths, bandwidth, subframe
  index, non-positive pitch lag), the voiced zero-history /
  zero-excitation / zero-b identity, the voiced `b == 0` pass-through
  identity, the voiced `b_Q7[0]` region-A pitch-lookback algebra
  (`0.5 * 4*LTP_scale_Q14/gain_Q16 * out[j-14]`), the voiced `b_Q7[2]`
  region-B (lpc[]) rewhiten algebra, the LSF-interpolation-split
  override (effective scale becomes `4*16384/65536 = 1.0` exactly),
  voiced-decode determinism, and a no-panic finite-output sweep across
  3 buffers × {NB, MB, WB} × {10 ms, 20 ms} × 4 subframes with state
  carried via `ltp_synth_commit_subframe`.

* **Clean-room round 15 (2026-05-25):** RFC 6716 §4.2.7.9.2 SILK LPC
  synthesis filter behind a new `lpc_synthesis_subframe` /
  `lpc_synthesis_frame` / `LpcSynthState` API (`silk_lpc_synth` module).
  Given the §4.2.7.9.1 LPC residual `res[]` for the current subframe, the
  §4.2.7.4 Q16 quantization gain `gain_Q16[s]`, and the §4.2.7.5.8
  stabilised Q12 short-term predictor `a_Q12[k]`, the filter runs:

  ```
                                  d_LPC-1
                 gain_Q16[s]         __              a_Q12[k]
        lpc[i] = ----------- * res[i] + \  lpc[i-k-1] * --------
                   65536.0              /_               4096.0
                                        k=0

        out[i] = clamp(-1.0, lpc[i], 1.0)
  ```

  Each subframe carries d_LPC unclamped `lpc[i]` history samples forward
  into the next subframe through `LpcSynthState`, which is cleared to
  zero on a decoder reset (RFC 6716 §4.5.2) or after an uncoded regular
  SILK frame for the channel. The §4.2.7.9 preamble explicitly licenses
  a floating-point implementation here ("the remainder of the
  reconstruction process for the frame does not need to be bit-exact"),
  so the filter accumulates in `f32` with the spec's left-to-right
  formula. The §4.2.7.9.2 wording that "the decoder saves the unclamped
  values lpc[i] to feed into the LPC filter for the next subframe, but
  saves the clamped values out[i] for rewhitening in voiced frames" is
  implemented exactly: state holds unclamped values; the rendered output
  is the clamped vector. d_LPC routing follows §4.2.7.5: 10 for NB / MB
  and 16 for WB; SWB / FB rejected at the SILK layer.

  Eighteen new unit tests (277 lib tests total, up from 259 at round-14
  close) cover `subframe_samples` (40 / 60 / 80 for NB / MB / WB + SWB /
  FB rejection); `LpcSynthState` d_LPC routing + zero initialisation +
  reset; the three input-validation rejections (mismatched `res` /
  `out_clamped` / `a_q12` lengths); the algebraic identities (a_Q12 = 0
  gives `lpc = gain_Q16/65536 * res`; res = 0 with zero history gives
  identically zero output regardless of a_Q12 / gain); a hand-pinned
  single-tap unity-gain NB filter (impulse response is the constant
  `1.0`); a hand-pinned single-tap half-gain WB filter (impulse response
  is the geometric series `0.5^(i+1)` and the history holds the final 16
  unclamped samples to 1e-9 precision); a hand-traced two-tap NB filter
  with non-trivial `res[]` `[1, 2, 3, 0, ...]` yielding the exact
  sequence `[1.0, 2.5, 4.5, 2.875, 2.5625, ...]` plus the per-sample
  clamp; the cross-subframe history carry-over (an impulse decays into a
  unit-feedback subframe and the next subframe of zero residual still
  emits `1.0` everywhere); the decoder-reset path zeroes history; the
  `out[]` ∈ `[-1.0, 1.0]` clamp post-condition under deliberately
  over-driven input; the spec wording that `state.history` stores the
  unclamped `lpc[i]` and not the saturated `out[i]`; the
  `lpc_synthesis_frame` wrapper matches an explicit per-subframe loop
  bit-for-bit (state included) and rejects bad input lengths; and a
  sweep over {NB, MB, WB} × {10, 20 ms} that asserts no panics, the
  correct output length, the clamp post-condition, and the correct
  history length. The §4.2.7.9.1 LTP synthesis filter that produces the
  voiced-frame `res[]` is deferred to a later round; this stage can
  already be driven directly off `e_Q23[i] / 2^23` for unvoiced
  subframes per the §4.2.7.9.1 wording.

* **Clean-room round 14 (2026-05-25):** RFC 6716 §4.2.7.7 SILK LCG seed
  (`silk_lcg_seed` module) and §4.2.7.8 SILK excitation decoder behind a
  new `Excitation` / `ExcitationConfig` API (`silk_excitation` module).

  The §4.2.7.7 LCG seed is a single uniform 4-entry symbol from Table
  43 (`{64, 64, 64, 64}/256`) producing a value in `0..=3` that
  initialises the LCG used by §4.2.7.8.6 reconstruction.

  The §4.2.7.8 excitation runs in six substeps: §4.2.7.8.1 rate level
  (one symbol per SILK frame from one of two Table 45 PDFs chosen by
  signal type, producing `0..=8`); §4.2.7.8.2 per-shell-block pulse
  count (Table 46 PDFs at the chosen rate level, with the special
  value 17 chaining into rate level 9, then to rate level 10 on the
  10th occurrence — capping extra LSBs at 10 per block per the
  §4.2.7.8.2 spec note); §4.2.7.8.3 recursive pulse-location decoding
  (partition halves 16 → 8 → 4 → 2 → 1 with Tables 47/48/49/50 split
  PDFs selected by partition size + remaining pulse count); §4.2.7.8.4
  per-coefficient LSB decoding (Table 51 `{136, 120}/256`, doubling
  the magnitude and adding each bit MSB-first); §4.2.7.8.5 sign
  decoding (Table 52, picked by `(signal_type, qoff_type,
  min(pulses_in_block, 6))` — 42 PDFs in all); and §4.2.7.8.6
  reconstruction with `e_Q23[i] = (e_raw << 8) - sign(e_raw)*20 +
  offset_Q23` (Table 53 offsets `{25, 60, 25, 60, 8, 25}`) plus the
  32-bit LCG step `seed = (196314165*seed + 907633515) & 0xFFFFFFFF`
  whose MSB drives a per-sample sign flip, followed by
  `seed = (seed + e_raw[i]) & 0xFFFFFFFF` for the next iteration.

  Table 44 routes `(bandwidth, frame_size)` to the shell-block count
  (5/8/10/10/15/20 for the six NB/MB/WB × 10ms/20ms cells; SWB/FB
  rejected as not paired with the SILK layer). The 10 ms MB special
  case decodes 8 shell blocks (128 samples) of which the trailing 8
  are discarded by the caller per the §4.2.7.8 preamble.

  Thirty new unit tests (259 lib tests total, up from 229 at round-13
  close) cover the Table 43 transcription + the 0..=3 + 2-bits-per-
  symbol invariants; Table 44 (all six cells + SWB/FB rejection); both
  Table 45 PDFs; all eleven Table 46 PDFs including the L10 cell-17 =
  0 boundary; spot-checks on Tables 47/48/49/50 (1- and ≥7-pulse
  cells); Table 51; six Table 52 spot-checks across each
  `(signal_type, qoff_type)` quadrant + the "6 or more" saturation;
  all six Table 53 offsets; the LCG recurrence pinned algebraically
  for the first two steps from seed=0; `Excitation::decode` rejections
  (invalid LCG seed, SWB/FB bandwidth); per-cell sample count; the
  §4.2.7.8 "fits in 24 bits including sign" invariant across three
  buffers × all (NB/MB/WB × 10/20 ms) cells; per-block pulse-count ≤
  16 and LSB-count ≤ 10 invariants; a hand-pinned reconstruction of
  an isolated mag=5 sign=-1 sample producing ±1235; the
  zero-magnitude `|e_Q23[i]| == offset_Q23` identity; cross-pass
  reproducibility; LCG-seed divergence; and a no-panic sweep over
  three buffers × {NB, MB, WB} × {10, 20 ms} × 3 signal types × 2
  qoff types × 4 seeds. The §4.2.7.9 LTP / LPC synthesis filters that
  consume `e_Q23[]` are deferred to a later round.

* **Clean-room round 13 (2026-05-24):** RFC 6716 §4.2.7.6 SILK Long-Term
  Prediction parameters behind a new `LtpParameters` / `LtpConfig` API
  (`silk_ltp` module). Decodes the §4.2.7.6.1 primary pitch lag (either
  absolute, via Table 29 high-part + Table 30 bandwidth-conditioned
  low-part / scale / lag-range, or relative, via the Table 31 21-entry
  delta PDF with a zero-delta fallback to absolute coding), the
  pitch-contour VQ index (Table 32 PDF; Tables 33–36 codebooks) that
  refines the primary lag into per-subframe pitch lags clamped to the
  bandwidth's `[lag_min, lag_max]`, the §4.2.7.6.2 periodicity index
  (Table 37) and per-subframe 5-tap Q7 LTP filter taps (Table 38 PDFs;
  Tables 39–41 codebooks of sizes 8 / 16 / 32), and the optional
  §4.2.7.6.3 Q14 LTP scaling factor (Table 42 → `{15565, 12288, 8192}`;
  default `15565` ≈ 0.95 when not coded or for non-voiced frames).
  Non-voiced frames consume no LTP bits. The §4.2.7.9 LTP synthesis
  filter that consumes these parameters is deferred to a later round.

  Nineteen new unit tests (229 lib tests total in the crate, up from
  210 at round-12 close) cover the eleven PDF → iCDF transcriptions
  (Tables 29 / 30 NB-MB-WB / 31 / 32 four PDFs / 37 / 38 three PDFs /
  42), Table 30 scale + lag-range values, contour codebook
  size-matches-PDF self-checks + index-0 all-zero rows + four
  interior-row spot-checks against the spec, LTP filter codebook sizes
  (8 / 16 / 32) + four boundary-row spot-checks against Tables 39–41,
  the non-voiced no-bits-consumed property (both Inactive and Unvoiced),
  rejection of non-2-non-4 `num_subframes` and SWB / FB bandwidths,
  in-range absolute-coding lags + production / independent formula
  agreement across {NB, MB, WB} × {2, 4} subframes, relative-coding
  non-zero-delta + zero-delta-fallback paths, LTP-scaling-present output
  ∈ `{15565, 12288, 8192}` and absent-uses-default-without-reading bit
  positioning, and a sweep over {NB, MB, WB} × {2, 4} × {absent,
  present} × {Absolute, Relative} × three buffers asserting no panics,
  the `[lag_min, lag_max]` post-condition, and periodicity ≤ 2.

* **Clean-room round 12 (2026-05-24):** RFC 6716 §4.2.7.5.8 SILK LPC
  prediction-gain limiting behind a new `LpcQ17::prediction_gain_limited`
  method returning a new `LpcQ12` type (`silk_lsf_to_lpc` module).
  Consumes the (range-limited) §4.2.7.5.7 `a32_Q17[]` and produces the
  final stable Q12 filter `a_Q12[k]` for the §4.2.7.9.2 LPC synthesis.

  - **Up to 16 rounds of stability-driven bandwidth expansion.** Each
    round converts to the real Q12 coefficients
    `a32_Q12[n] = (a32_Q17[n] + 16) >> 5` and runs the
    `silk_LPC_inverse_pred_gain_QA()` stability test. If the filter is
    stable the Q12 coefficients are returned; otherwise a chirp round with
    `sc_Q16[0] = 65536 - (2<<i)` is applied via the same
    `silk_bwexpander_32` as §4.2.7.5.7. On round 15 `sc_Q16[0] = 0`,
    zeroing every coefficient so an all-zero (trivially stable) filter is
    the worst-case outcome.
  - **`silk_LPC_inverse_pred_gain_QA()` stability test (`is_lpc_stable`).**
    The DC-response check (`DC_resp = Σ a32_Q12[n] > 4096` ⇒ unstable)
    followed by the fixed-point Levinson recurrence on the Q24-widened
    coefficients: initialize `inv_gain_Q30[d_LPC] = 1<<30` and
    `a32_Q24[d_LPC-1][n] = a32_Q12[n] << 12`, then for each `k` from
    `d_LPC-1` down to `0` reject on `abs(a32_Q24[k][k]) > 16773022`
    (≈ 0.99975 in Q24) or `inv_gain_Q30[k] < 107374` (≈ 1/10000 in Q30)
    via `rc_Q31 = -a32_Q24[k][k] << 7`,
    `div_Q30 = (1<<30) - (rc_Q31*rc_Q31 >> 32)`,
    `inv_gain_Q30[k] = (inv_gain_Q30[k+1]*div_Q30 >> 32) << 2`. Each
    surviving step (for `k > 0`) computes row `k-1` via the spec's
    `b1 = ilog(div_Q30)`, `inv_Qb2`, `err_Q29`, `gain_Qb1`, `num_Q24[n]`,
    `a32_Q24[k-1][n]` formulas. Every multiply the spec marks as needing
    more than 32 bits is performed in `i64`.

  `LpcQ12` exposes `a_q12()`, `len()`, `is_empty()`, and `rounds()` (the
  number of chirp rounds that ran before the filter was deemed stable).

  9 new unit tests (210 lib tests total in the crate; up from 201 in the
  round-11 close) covering:

  - `is_lpc_stable` agrees with an independent 2D-matrix spec
    transcription oracle on hand-built filters (all-zero, gentle decay,
    near-unit single tap at the DC=4096 boundary, DC over the ceiling,
    mixed-sign moderate).
  - The all-zero filter is stable for both NB/MB and WB widths.
  - DC response `> 4096` is rejected before the Levinson recurrence; the
    DC=4096 boundary is not rejected by the DC check alone.
  - A real §4.2.7.5.7 → §4.2.7.5.8 conversion of a typical decoded NLSF
    vector returns on round 0 with `a_Q12 == (a32_Q17 + 16) >> 5`.
  - Deliberately unstable Q17 inputs (near-unit tap, high-gain resonant
    alternating taps, DC over the ceiling) always converge to a stable
    Q12 filter within ≤ 16 rounds.
  - A persistently-unstable input zeroes every coefficient if it reaches
    the forced round-15 (`sc_Q16[0] = 0`) step.
  - The emitted Q12 filter fits a signed 16-bit value.
  - A real §4.2.7.5.2 → … → §4.2.7.5.7 → §4.2.7.5.8 pipeline sweep across
    all 32 `I1` values × {NB, MB, WB} on three buffers: the emitted Q12
    filter is always stable (cross-checked vs the oracle) and the round
    count is ≤ 16.
  - `ilog64` (the i64 variant used by §4.2.7.5.8) matches the §1.1.10
    definition for the spec examples plus the `2^30` / `2^30 - 1`
    `div_Q30`-domain boundaries.

* **Clean-room round 11 (2026-05-24):** RFC 6716 §4.2.7.5.7 SILK LPC
  range-limiting bandwidth expansion behind a new
  `LpcQ17::range_limited` method (`silk_lsf_to_lpc` module). Consumes the
  raw §4.2.7.5.6 `a32_Q17[]` and reduces it so it fits a signed 16-bit
  Q12 value:

  - **Up to 10 rounds of `silk_bwexpander_32` chirping.** Each round
    finds the index `k` of the largest `abs(a32_Q17[k])` (ties to the
    lowest `k`), computes `maxabs_Q12 = min((maxabs_Q17 + 16) >> 5,
    163838)`, and stops once `maxabs_Q12 <= 32767`. Otherwise it derives
    the chirp factor `sc_Q16[0] = 65470 - ((maxabs_Q12 - 32767) << 14) /
    ((maxabs_Q12 * (k+1)) >> 2)` (integer division) and runs the
    `silk_bwexpander_32` recurrence `a32_Q17[k] = (a32_Q17[k]*sc_Q16[k])
    >> 16`, `sc_Q16[k+1] = (sc_Q16[0]*sc_Q16[k] + 32768) >> 16`. The
    first multiply runs in i64 ("up to 48 bits of precision"); the second
    is performed unsigned per the spec to avoid 32-bit overflow.
  - **Post-loop Q12 saturation.** If `maxabs_Q12` is still greater than
    32767 after the 10th round, each coefficient is saturated in the Q12
    domain and converted back to Q17:
    `a32_Q17[k] = clamp(-32768, (a32_Q17[k] + 16) >> 5, 32767) << 5`.
    The result is returned in the Q17 domain (the §4.2.7.5.8
    prediction-gain limiting that follows consumes Q17 coefficients), so
    it shares the `LpcQ17` representation. The §4.2.7.5.8 stability check
    is deferred to a subsequent round.

  `maxabs_Q17` is taken via `i32::unsigned_abs()` so an `i32::MIN`
  coefficient from an adversarial §4.2.7.5.6 output does not panic.

  6 new unit tests (201 lib tests total in the crate; up from 195 in the
  round-10 close) covering:

  - Small coefficients already fitting Q12 pass through unchanged.
  - Production agrees bit-for-bit with an independent i128 transcription
    of the §4.2.7.5.7 loop on synthetic overflow vectors (single peak,
    peak at a non-zero index, mixed-sign large coefficients, a moderate
    overshoot) and on an extreme input pinned to the 163838 cap.
  - Every range-limited output fits a signed 16-bit Q12 value.
  - The `i32::MIN` coefficient no-panic edge.
  - The post-loop Q12 saturation formula pinned in isolation (the
    adaptive chirp converges every realistic input within 10 rounds, so
    the engaged branch is effectively unreachable; the formula is pinned
    directly to catch a transcription typo).
  - A real §4.2.7.5.2 → §4.2.7.5.3 → §4.2.7.5.4 → §4.2.7.5.6 →
    §4.2.7.5.7 pipeline sweep across all 32 `I1` values × {NB, MB, WB}
    asserting the Q12 fit and production/oracle agreement.

* **Clean-room round 10 (2026-05-24):** RFC 6716 §4.2.7.5.6 SILK
  Normalized LSF → LPC core conversion behind a new `LpcQ17` API
  (`silk_lsf_to_lpc` module). Consumes a stabilized / interpolated
  `nlsf_q15[]` (the §4.2.7.5.4 / §4.2.7.5.5 output) and runs the
  `silk_NLSF2A` procedure in three steps:

  - **Table 27 ordering + Table 28 cosine table (`silk_NLSF2A_cos`).**
    The 129-entry Q12 cosine table (`cos_Q12[0]=4096`, `cos_Q12[64]=0`,
    `cos_Q12[128]=-4096`, anti-symmetric about i=64) is transcribed
    verbatim. For each coefficient `i = nlsf >> 8`, `f = nlsf & 255`
    and the §4.2.7.5.6 piecewise-linear interpolation
    `c_Q17[ordering[k]] = (cos_Q12[i]*256 + (cos_Q12[i+1]-cos_Q12[i])*f
    + 4) >> 3` lands the re-ordered Q17 cosine vector. The Table 27
    `ordering[]` vectors are NB/MB `[0,9,6,3,4,5,8,1,2,7]` and WB
    `[0,15,8,7,4,11,12,3,2,13,10,5,6,9,14,1]`.
  - **`silk_NLSF2A_find_poly` P/Q recurrence.** Two rolling-row passes
    on the even-indexed (P) and odd-indexed (Q) `c_Q17[]` cells run
    `p[k][j] = p[k-1][j] + p[k-1][j-2] - ((c*p[k-1][j-1] + 32768)>>16)`
    in i64 to absorb the spec's noted "up to 48 bits of intermediate
    precision" requirement, with the §4.2.7.5.6 boundary conditions
    `p[k][j<0] = 0` and `p[k][k+2] = p[k][k]`.
  - **`silk_NLSF2A` last-row assembly.** The final i64 rows are folded
    into the 32-bit Q17 LPC coefficients via the §4.2.7.5.6 sum/diff
    pair: `a32_Q17[k] = -((q_diff) + (p_sum))` and
    `a32_Q17[d_LPC-k-1] = (q_diff) - (p_sum)`, where
    `q_diff = q[d2-1][k+1] - q[d2-1][k]` and
    `p_sum  = p[d2-1][k+1] + p[d2-1][k]`.

  The §4.2.7.5.7 range-limiting bandwidth-expansion loop (up to 10
  rounds shrinking `a32_Q17[]` so it fits Q12) and the §4.2.7.5.8
  prediction-gain stability check (up to 16 chirp rounds + the
  `silk_LPC_inverse_pred_gain_QA` test) are deferred to subsequent
  rounds.

  22 new unit tests (195 lib tests total in the crate; up from 173 in
  the round-9 close) covering:

  - Table 27 row-widths, permutation-of-0..d_LPC self-checks, and
    bandwidth routing (`ordering()` rejects SWB / FB).
  - Table 28 length (129), the three anchors (0 → 4096; 64 → 0;
    128 → -4096), the strict-monotone-decreasing pairwise check, the
    anti-symmetric-about-64 invariant, the Q12-range bound, and four
    row spot-checks (rows 0, 16, 60, 64, 124).
  - `nlsf_to_c_q17` at the table anchor points (`f == 0` round-trip
    against `cos_Q12[8*k]`) and at the linear-interpolation midpoint
    (`f == 128` matching the `16*(a+b)` algebraic identity).
  - `nlsf_to_c_q17` rejects SWB / FB and `nlsf_q15.len() != d_LPC`.
  - `LpcQ17` length, SWB / FB and length-mismatch rejection.
  - Production `LpcQ17::from_nlsf` agrees bit-for-bit with an
    independent 2D-matrix spec-transcription oracle of the §4.2.7.5.6
    recurrence on synthetic ascending NLSF vectors for both NB and WB.
  - Production `LpcQ17::from_nlsf` agrees with the same oracle when
    driven by the full §4.2.7.5.2 → §4.2.7.5.3 → §4.2.7.5.4 decoder
    pipeline across all 32 `I1` values × {NB, MB, WB}.
  - A no-panic sweep over three buffers × all 32 `I1` × {NB, MB, WB}
    confirming the full §4.2.7.5.2..§4.2.7.5.6 path is panic-free.

* **Clean-room round 9 (2026-05-24):** RFC 6716 §4.2.7.5.5 SILK
  Normalized LSF interpolation behind a new `LsfInterpolated` /
  `LsfInterpContext` API (`silk_lsf_interp` module). For a 20 ms SILK
  frame the first half (the first two subframes) may use NLSF
  coefficients interpolated between the most recent coded frame's
  vector `n0_Q15[]` and the current §4.2.7.5.4-stabilized vector
  `n2_Q15[]`:

  - **`TwentyMs`** decodes the Q2 factor `w_Q2 ∈ 0..=4` from the
    Table 26 PDF (`{13, 22, 29, 11, 181}/256`; iCDF
    `[243, 221, 192, 181, 0]`) and computes
    `n1_Q15[k] = n0_Q15[k] + (w_Q2*(n2_Q15[k] - n0_Q15[k]) >> 2)`.
  - **`TwentyMsAfterResetOrUncoded`** still decodes the factor (to keep
    the range coder in sync) but discards it and uses `4` instead, so
    `n1_Q15[] == n2_Q15[]`. The same forced-4 behaviour applies when
    `n0_Q15[]` is `None` (no prior-frame history).
  - **`TenMs`** reads no factor (it is not present in the bitstream)
    and produces no first-half vector.

  The result exposes the decoded `w_q2()` (`None` for 10 ms) and the
  first-half `n1_q15()` (`None` for 10 ms). The second half of a 20 ms
  frame and the whole of a 10 ms frame always use `n2_Q15[]` directly.

  10 new unit tests (173 lib tests total in the crate; up from 163 in
  the round-8 close) covering:

  - Table 26 PDF→iCDF transcription (sum-to-256 and strict
    monotone-decreasing iCDF self-checks; exactly five factors).
  - 10 ms path reads nothing (`tell()` unchanged) and has no
    first-half vector.
  - End-to-end 20 ms interpolation matching an independent
    re-derivation of the §4.2.7.5.5 formula.
  - The `w_Q2 == 0 → n1 == n0` and `w_Q2 == 4 → n1 == n2` algebraic
    identities.
  - The reset/uncoded context decodes the factor then forces `4`
    (asserting `tell()` parity with the normal context).
  - The no-history `n0 = None` path forces `n1 == n2` even in the
    normal context while still decoding the factor.
  - `n0`-length-mismatch rejection (`Error::MalformedPacket`).
  - A sweep asserting every interpolated value stays in `[0, 32767]`
    across {NB, MB, WB} × all 32 `I1` × `w_Q2 ∈ 0..=4`.

* **Clean-room round 8 (2026-05-23):** RFC 6716 §4.2.7.5.4 SILK
  Normalized LSF stabilization behind a new `NlsfStabilized` API
  (`silk_lsf_stabilize` module). Consumes the §4.2.7.5.3
  `NlsfReconstructed` output and enforces the Table 25 minimum spacing
  between consecutive `NLSF_Q15[]` coefficients, with the boundary
  conventions `NLSF_Q15[-1] = 0` / `NLSF_Q15[d_LPC] = 32768` and a
  Table 25 column of `d_LPC + 1` entries.

  - **Up to 20 distortion-minimizing passes.** Each pass finds the
    smallest `NLSF_Q15[i] - NLSF_Q15[i-1] - NDeltaMin_Q15[i]` over
    `i ∈ 0..=d_LPC` (ties to lower `i`); stops if non-negative.
    Otherwise `i == 0` → `NLSF_Q15[0] = NDeltaMin_Q15[0]`,
    `i == d_LPC` → `NLSF_Q15[d_LPC-1] = 32768 - NDeltaMin_Q15[d_LPC]`,
    and any interior `i` re-centres the pair via the
    `min_center`/`max_center` running sums and
    `center_freq = clamp(min_center, (NLSF[i-1]+NLSF[i]+1)>>1,
    max_center)`, then `NLSF_Q15[i-1] = center_freq -
    (NDeltaMin_Q15[i]>>1)` and `NLSF_Q15[i] = NLSF_Q15[i-1] +
    NDeltaMin_Q15[i]`.
  - **Fallback (once after the 20th pass).** Sort ascending, then a
    forward `max(NLSF[k], NLSF[k-1] + NDeltaMin[k])` sweep and a
    backward `min(NLSF[k], NLSF[k+1] - NDeltaMin[k+1])` sweep.
  - **RFC 8251 §7 erratum.** The fallback forward sweep's addition
    uses 16-bit saturating addition (`silk_ADD_SAT16`) to avoid the
    integer wrap-around the erratum documents on adversarial inputs
    with extremely large high-LSF parameters.

  Table 25 is transcribed verbatim: NB/MB column
  `{250, 3, 6, 3, 3, 3, 4, 3, 3, 3, 461}`, WB column
  `{100, 3, 40, 3, 3, 3, 5, 14, 14, 10, 11, 3, 8, 9, 7, 3, 347}`.

  19 new unit tests (163 lib tests total in the crate; up from 144 in
  the round-7 close) covering:

  - Table 25 lengths (`d_LPC + 1` for NB/MB and WB) and spot-checks.
  - `ndelta_min_q15` rejects SWB / FB.
  - `add_sat16` saturates at both `i16` bounds.
  - An already-stable comfortably-spaced input is left bit-identical
    (NB and WB).
  - First-coefficient-too-low pushed up to `NDeltaMin[0]`;
    last-coefficient-too-high pulled down to `32768 - NDeltaMin[d_LPC]`.
  - Interior re-centring with hand-computed exact `NLSF_Q15[i-1]` /
    `NLSF_Q15[i]` values for an isolated tight pair.
  - The fallback sort + sweeps fix a fully-reversed input; all-zero
    and all-32767 inputs are spread to valid spacing.
  - The RFC 8251 §7 no-wrap guard: an all-`i16::MAX` input stays in
    `[0, 32767]` (a wrap-around would produce a negative value).
  - End-to-end sweep across all 32 `I1` values × {NB, MB, WB} wired
    through the §4.2.7.5.2 / §4.2.7.5.3 decoders, asserting the
    spacing post-condition, the `[0, 32767]` bound, and strict
    monotonicity of every stabilized vector.
  - `from_reconstructed` rejects SWB / FB and a bandwidth ↔ recon
    length mismatch.

* **Clean-room round 7 (2026-05-22):** RFC 6716 §4.2.7.5.3 SILK
  Normalized LSF reconstruction behind a new `NlsfReconstructed` API
  (`silk_lsf_recon` module). Lifts the stage-2 residual `res_Q10[]`
  (returned by round 6's `LsfStage2`) to the final `NLSF_Q15[]`
  coefficient vector in three steps:

  - **Tables 23 / 24 lookup.** The 32 × 10 NB/MB and 32 × 16 WB
    stage-1 codebook vectors `cb1_Q8[]` are transcribed verbatim from
    the RFC text. The `(bandwidth, I1)` lookup yields a slice of
    `d_LPC` Q8 cells.
  - **IHMW weights `w_Q9[k]`.** The low-complexity Inverse Harmonic
    Mean Weighting derivation
    `w2_Q18[k] = (1024/(cb1_Q8[k]-cb1_Q8[k-1])
                + 1024/(cb1_Q8[k+1]-cb1_Q8[k])) << 16`
    (with boundary `cb1_Q8[-1] = 0` and `cb1_Q8[d_LPC] = 256` and
    integer division) is reduced through the spec's square-root
    approximation: `i = ilog(w2_Q18[k])`,
    `f = (w2_Q18[k] >> (i-8)) & 127`,
    `y = ((i & 1) ? 32768 : 46214) >> ((32-i) >> 1)`,
    `w_Q9[k] = y + ((213 * f * y) >> 16)`. Every weight across the
    full 32 × {NB/MB d_LPC=10, WB d_LPC=16} sweep falls inside the
    spec's documented 13-bit `[1819, 5227]` range.
  - **Final NLSF.**
    `NLSF_Q15[k] = clamp(0, (cb1_Q8[k]<<7) + (res_Q10[k]<<14)/w_Q9[k], 32767)`,
    integer division throughout. The §4.2.7.5.4 stabilization and
    §4.2.7.5.5 interpolation passes that consume `NLSF_Q15[]` are
    deferred to a later round.

  26 new unit tests (144 lib tests total in the crate; up from 118 in
  the round-6 close) covering:

  - `ilog(n)` matches the RFC §1.1.10 examples for `n ∈ {-1, 0, 1,
    2, 3, 4, 7}`, plus 8 / 255 / 256 / 2^24.
  - Tables 23 and 24 rows are strictly monotone increasing (a
    pre-condition of the IHMW divisor being positive).
  - Tables 23 / 24 row widths equal `D_LPC_NB_MB` (10) and
    `D_LPC_WB` (16).
  - Table 23 row 0 (`12 35 60 83 108 132 157 180 206 228`),
    Table 23 row 31, Table 24 row 0
    (`7 23 38 54 69 85 100 116 131 147 162 178 193 208 223 239`),
    Table 24 row 31 spot-checks.
  - `cb1_q8()` routes Nb / Mb to Table 23 and Wb to Table 24, and
    rejects `I1 >= 32` and Swb / Fb (SILK never sees the latter
    after the §4.2.2 hybrid split).
  - All 32 × NB IHMW weights and all 32 × WB IHMW weights are in
    `[1819, 5227]` (the spec's own documented range for the 13-bit
    tabulated form).
  - Concrete hand-computed IHMW match: NB I1=0 k=0 → 2897; WB I1=0
    k=0 → 3657 — both derived from `1024/diff` integer arithmetic
    against the transcribed `cb1_Q8` cells.
  - With `res_Q10[k] == 0`, every reconstructed `NLSF_Q15[k]` equals
    `cb1_Q8[k] << 7` (bounded by `242 << 7 = 30976`, within the
    `32767` clamp).
  - Sweep across all 32 `I1` values × {NB, MB, WB} via a synthetic
    range-decoder buffer: every reconstructed `NLSF_Q15[k]` is in
    `[0, 32767]` and exactly reproduces the §4.2.7.5.3 formula
    re-applied to the decoded `res_Q10[k]` and `w_Q9[k]`.
  - `from_stage1_and_stage2` rejects mismatched bandwidth ↔ stage-2
    length (e.g. WB-decoded stage-2 with NB reconstruction),
    out-of-range `I1`, and Swb / Fb bandwidths.

* **Clean-room round 6 (2026-05-22):** RFC 6716 §4.2.7.5.2 Normalized
  LSF Stage-2 decoder behind a new `LsfStage2` API. The caller passes
  the SILK-layer bandwidth (`Nb` / `Mb` / `Wb`) and the stage-1 codebook
  index `I1 ∈ 0..32` (returned by the §4.2.7.5.1 decoder). The decoder:
  - Reads `d_LPC` stage-2 residual indices `I2[k]` (10 cells for
    NB / MB, 16 for WB) using one of 16 signal-shape codebook PDFs
    (Tables 15 a..h for NB/MB, Table 16 i..p for WB). The
    `(bandwidth, I1) → codebook` mapping comes from Table 17 (NB/MB)
    or Table 18 (WB). Each raw symbol is `0..=8`; after the `-4`
    subtraction the index is `[-4, 4]`. If `|idx| == 4`, the Table 19
    extension PDF (`{156, 60, 24, 9, 4, 2, 1}/256`) supplies an
    additional `0..=6` magnitude with the same sign, giving
    `I2[k] ∈ [-10, 10]`.
  - Undoes the backwards-prediction step with the §4.2.7.5.2 formula
    `res_Q10[k] = (k+1 < d_LPC ? (res_Q10[k+1]*pred_Q8[k])>>8 : 0)
    + ((((I2[k]<<10) - sign(I2[k])*102) * qstep) >> 16)` running
    `k = d_LPC-1` down to `0`. `qstep = 11796 (Q16)` for NB / MB and
    `9830` for WB. The Q8 prediction weight is chosen per-coefficient
    from Table 20 lists A / B (NB/MB) or C / D (WB) via Table 21
    (NB/MB) or Table 22 (WB).
  - Returns `i2[]` and `res_Q10[]`; the §4.2.7.5.3 reconstruction
    (stage-1 codebook lookup + IHMW weights + final `NLSF_Q15[k]`),
    §4.2.7.5.4 stabilization, and §4.2.7.5.5 interpolation are
    deferred to round 7+.

  The RFC 6716 Table 17 row at `I1 = 6` is mislabelled `g` in the
  source PDF; the row's cells (`a c c c c c c c c b`) are valid
  codebook letters and the table is transcribed with the I1 row-label
  restored, matching the table's documented intent.

  30 new unit tests (148 total in the crate) covering:
  - All 16 stage-2 PDFs sum to 256 and their transcribed iCDFs are
    monotone non-increasing with a trailing zero (Tables 15, 16).
  - The Table 19 extension PDF sums to 256 and the iCDF cells match
    `256 - fh[k]`.
  - Tables 17, 18, 21, 22 row widths match `d_LPC` (NB/MB) and
    `d_LPC` / `d_LPC - 1` (WB pred-weight); all entries fall in
    `0..=7` (codebook selection) or `0..=1` (prediction-weight
    selection).
  - Table 17 `I1 = 0` (all-`a`), `I1 = 2`, and the `I1 = 6` typo-row
    spot-checks; Table 18 `I1 = 0` (all-`i`), `I1 = 6` (all-`i`),
    `I1 = 9` (`k j i ...`) spot-checks.
  - Table 20 A[0] = 179, B[0] = 116, A[8] = 163, B[8] = 92,
    C[0] = 175, D[0] = 68, C[14] = 182, D[14] = 155 spot-checks;
    Table 21 / 22 `I1 = 0` rows.
  - `pred_weight` resolves the right A/B and C/D list cells per
    coefficient against the Table 21 / 22 selection rows.
  - End-to-end decode for `(Nb, I1=0)`, `(Mb, I1=5)`, `(Wb, I1=0)`,
    `(Wb, I1=9)` with `i2[k] ∈ [-10, 10]` for every populated
    coefficient.
  - Independent rejection of `I1 = 32` (out of range), `Swb`, and
    `Fb` (SILK never sees SWB / FB after the §4.2.2 hybrid split).
  - `res_Q10[]` from `LsfStage2::decode` exactly reproduces the
    §4.2.7.5.2 formula re-applied to the decoded `i2[]` for both
    NB/MB and WB.
  - Sweep across all 32 I1 values × {NB, MB, WB} confirming every
    decode succeeds and `i2[k] ∈ [-10, 10]` for every coefficient.
  - `RangeDecoder::tell()` is monotone non-decreasing across a
    stage-2 decode (the decoder consumes ≥ 1 bit).

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
