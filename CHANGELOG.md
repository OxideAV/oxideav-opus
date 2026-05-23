# Changelog

All notable changes to `oxideav-opus` are recorded here.

## [Unreleased]

### Added

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
