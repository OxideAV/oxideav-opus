# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT).

## Status — 2026-05-29 (clean-room round 23)

**Packet header + §3.2 frame-packing parser + §3.1 / §4.2 framing
dispatch (`OpusFrameRouting`: SILK-only / Hybrid / CELT-only mode +
SILK internal bandwidth pinned to WB for Hybrid + §4.2.2 SILK-frame
count + §4.2.4 per-frame LBRR-flag presence gate + channel-count
multiplier) + §3.4 R1..R7 malformed-input rejection audit
(`tests/malformed_input.rs`: 20 integration tests sweeping every
R1..R7 violation shape + TOC-byte total-function determinism +
§4.2.3 / §4.2.4 SILK-header truncation safety property) + §4.1
range decoder +
§4.2.3 SILK header bits (VAD + LBRR flag per channel) + §4.2.4
per-frame LBRR flags (Table 4 PDFs at 40/60 ms) +
SILK §4.2.7.1–§4.2.7.5.1 frame-header decoder + §4.2.7.4 subframe
gains (log_gain decode + the §4.2.7.4 tail-end
`gain_Q16 = silk_log2lin((0x1D1C71*log_gain >> 16) + 2090)` dequant
mapping `0..=63 → [81920, 1_686_110_208]`) + §4.2.7.5.2 LSF Stage-2 residual + §4.2.7.5.3 NLSF
reconstruction + §4.2.7.5.4 NLSF stabilization + §4.2.7.5.5 NLSF
interpolation + §4.2.7.5.6 NLSF→LPC core conversion (`silk_NLSF2A`) +
§4.2.7.5.7 LPC range-limiting bandwidth expansion + §4.2.7.5.8 LPC
prediction-gain stability limiting (`silk_LPC_inverse_pred_gain_QA`) +
§4.2.7.6 LTP parameters (pitch lags + LTP filter coefficients +
LTP scaling) + §4.2.7.7 LCG seed + §4.2.7.8 excitation (rate level +
pulses per shell block + recursive pulse-location split + LSBs + signs
+ §4.2.7.8.6 LCG-driven reconstruction) + §4.2.7.9.1 LTP synthesis
filter (voiced 5-tap Q7 LTP convolution + out[]/lpc[] rewhitening
with the §4.2.7.9.1 LSF-interpolation-split branch; unvoiced `res[i]
= e_Q23[i]/2^23` normalised copy) + §4.2.7.9.2 LPC synthesis filter
(per-subframe short-term predictor with `d_LPC` history carry-over
and `out[i] = clamp(-1, lpc[i], 1)`) + §4.2.8 stereo unmixing
(`silk_stereo_MS_to_LR`: low-passed `p0` + delayed mid + §4.2.7.1 Q13
weights → clamped L/R, with 8 ms weight interpolation across frames) +
§4.2.9 resampler delay budget (Table 54: NB = 0.538 ms, MB = 0.692 ms,
WB = 0.706 ms; internal SILK rates 8/12/16 kHz; supported output rates
8/12/16/24/48 kHz) + first CELT-layer fragment: §4.3 Table 56
pre-band header symbols (`silence` `{32767, 1}/32768`, §4.3.7.1
post-filter parameter group: logp=1 enable + `octave` uniform[0,6)
+ `period = (16<<octave) + fine_pitch - 1` from `4+octave` raw bits
∈ `15..=1022` + `gain` 3 raw bits → `G = 3*(gain_index+1)/32` +
`tapset` `{2,1,1}/4`, §4.3.1 `transient` `{7,1}/8`, §4.3.2.1 `intra`
`{7,1}/8`); coarse energy / bit allocation / band loop still
deferred.**

## Round 23 — §4.2.7.4 SILK gain dequantization tail (2026-05-29)

Round 23 lands the §4.2.7.4 tail-end mapping from the integer
`log_gain ∈ 0..=63` (decoded since round 5) to the linear Q16
gain `gain_Q16 ∈ [81_920, 1_686_110_208]` consumed by the
§4.2.7.9.1 LTP and §4.2.7.9.2 LPC synthesis filters. A new
`silk_log2lin` module owns:

* `silk_log2lin(in_log_q7)` — the §4.2.7.4 piecewise-linear
  approximation of `2^(inLog_Q7/128)`:
  `(1 << i) + ((-174*f*(128-f) >> 16) + f) * ((1 << i) >> 7)`
  with `i = inLog_Q7 >> 7` and `f = inLog_Q7 & 127`.
* `silk_gains_dequant(log_gain)` — the composed
  `silk_log2lin((0x1D1C71*log_gain >> 16) + 2090)` mapping. Both
  documented endpoints (`81920` at `log_gain = 0` representing
  linear gain 1.25, and `1_686_110_208` at `log_gain = 63`
  representing linear gain ≈ 25 728) are pinned exactly.
* Named constants `SILK_LOG_GAIN_MULTIPLIER = 0x1D1C71`,
  `SILK_LOG_GAIN_BIAS = 2090`, `SILK_GAIN_Q16_MIN = 81_920`,
  `SILK_GAIN_Q16_MAX = 1_686_110_208`.
* `SubframeGains::dequant_q16()` convenience that maps an entire
  decoded frame's `log_gain[]` into a fixed-size `[u32;
  SILK_MAX_SUBFRAMES]` array (trailing unused slots stay zero for
  two-subframe frames).

19 new module tests (381 lib tests total, up from 362; 20
integration tests unchanged): the two §4.2.7.4 endpoints pinned to
the RFC text; strict-monotone-in-`log_gain` property across the
full domain; spec-range invariant across the full sweep;
pure-power-of-two collapse `silk_log2lin(128*i) == 1 << i` for
`i ∈ 0..=30`; an independent i64 oracle of the §4.2.7.4 formula
matched bit-for-bit by the production i32 implementation across
the entire `i ∈ 0..=30 × f ∈ 0..=127` Q7 domain plus the
log-gain dequant sub-domain; pinned endpoint algebra
(`log_gain = 63 → in_log_q7 = 30*128 + 83 = 3923`); the halfway
pin `silk_log2lin(7*128 | 64) = 181` matching the true `2^7.5 ≈
181.019`; the `SubframeGains::dequant_q16` trailing-slot zeroing
and per-subframe agreement properties.

## Round 22 — §3.4 R1..R7 malformed-input rejection audit (2026-05-27)

Round 22 lands a dedicated integration-level malformed-input audit
(`tests/malformed_input.rs`, 20 tests) that pins the §3.4
requirements R1..R7 rejection behaviour to a per-requirement set of
property-style sweeps. This is the audit-grade evidence — for both
fuzz tooling and a future Auditor pass — that the §3.2 frame-packing
parser rejects every concrete malformed shape RFC 6716 §3.4
enumerates, and that the §4.2.3 / §4.2.4 SILK header decoder is
panic-free on any truncation of a previously-valid bitstream.

Coverage:

* **R1** — empty-packet rejection (`OpusPacket::parse(&[]) =>
  EmptyPacket`).
* **R2** — implicit frame length capped at `MAX_FRAME_BYTES = 1275`:
  code 0 with 1276 B body rejects; 1275 B accepts (boundary); code 1
  with 2552 B body (two 1276 B halves) rejects; 2550 B accepts; code
  3 VBR boundary at 1275 B accepts.
* **R3** — code-1 packets with odd body length (i.e. even `N`)
  rejected, sweeping body_len ∈ 0..=8.
* **R4** — code-2 packets across three failure shapes: missing
  length byte, missing second length byte for first ∈ 252..=255, and
  declared length > remaining; plus the §3.2.1 DTX boundary where
  declared length equals remaining (second frame is zero-length).
* **R5** — code-3 `M=0` rejected; `M ∈ 1..=48` with zero R/M
  accepted; `M > 48` rejected by the high-bit constraint
  (`MAX_FRAMES_PER_PACKET = 48`).
* **R6** — code-3 CBR where R is not a multiple of M (R=7, M=3)
  rejected; R=6, M=3 (R/M=2) accepted (boundary).
* **R7** — code-3 VBR declared lengths overrunning remaining
  rejected; declared=5, M=2 with 15 body bytes accepted (boundary,
  final frame = 10 B).
* **§3.2.5 padding chain** — missing padding-length byte rejected;
  padding > remaining rejected; unterminated 255-chain rejected.
* **TOC determinism** — every `u8` parses to a self-consistent TOC
  byte; `frame_size_tenths_ms` is always in `{25, 50, 100, 200, 400,
  600}` (Table 2).
* **§4.2.3 / §4.2.4 truncation safety** — for every
  `(num_silk_frames, stereo) ∈ {1, 2, 3} × {false, true}`,
  truncating a 32-byte buffer to every prefix length 1..=32 never
  panics; the returned `SilkHeaderBits` always has zero high-order
  bits beyond `num_silk_frames`. The §4.1.4 RangeDecoder
  zero-extension rule makes this provably safe — the test pins the
  contract.
* **§4.2.4 PDF bounds** — `decode_per_frame_lbrr` always returns a
  value in `{1..=2^N - 1}` for any input, never `0`, by way of the
  §4.1.3.3 leading-zero offset.
* **Mono-only safety** — `SilkHeaderBits::decode(..., stereo=false)`
  never emits `Some(side)` or a non-zero `side` LBRR bitmap (swept
  across all 256 byte-0 starts × 3 frame counts).
* **Slice lifetimes** — frames returned by a successful parse all
  point inside the input buffer's bounds.
* **Pathological short-packet sweep** — every `(c, body_len)` shape
  from 0..=12 bytes × five different filler patterns runs without
  panicking.

Total test count: 362 lib tests + 20 integration tests = 382 tests
(was 362 lib + 0 integration after round 21).

The audit caught one real shape that would otherwise have been
unspecified in the test suite: `M ∈ 49..=63` (reachable from the
6-bit `M` field but disallowed by R5's "120 ms / 2.5 ms = 48" cap)
must be rejected — the existing parser already does so via
`MAX_FRAMES_PER_PACKET`, but the test now pins the behaviour
explicitly.

## Round 21 — §3.1 / §4.2 framing dispatch (2026-05-27)

Round 21 lands the framing dispatch (`framing` module:
`OpusFrameRouting` / `OperatingMode` / `SilkBandwidth`) — the single
pure-function lookup that turns an `OpusTocByte` into the
per-Opus-frame routing decision a §4 decoder needs *before* it
touches the range coder. This codifies the SILK / Hybrid / CELT-only
dispatch logic, the §4.2 "Hybrid → SILK runs in WB regardless of TOC
bandwidth" pin, the §4.2.2 SILK-frame count per channel (1 for
10/20 ms, 2 for 40 ms, 3 for 60 ms), the §4.2.4 per-frame LBRR-flag
presence gate (duration > 20 ms), and the channel-count multiplier
for stereo — fields that were previously open-coded by every caller
that constructed a SILK or CELT context.

Concretely, `OpusFrameRouting::from_toc` is the dispatch entry point.
For a 60 ms stereo SILK-only WB frame (config 11, s=1) it produces:
`operating_mode = SilkOnly`, `silk_bandwidth = Some(Wb)`,
`silk_frames_per_channel = Some(3)`, `channel_count() = 2`,
`total_silk_frames() = 6`, `has_per_frame_lbrr_bits() = true`. For a
20 ms stereo Hybrid SWB frame (config 13, s=1) it produces:
`operating_mode = Hybrid`, `silk_bandwidth = Some(Wb)` (the §4.2 pin
even though the TOC bandwidth is `Swb`), `silk_frames_per_channel =
Some(1)`, `total_silk_frames() = 2`, `has_per_frame_lbrr_bits() =
false`. For a 5 ms mono CELT-only NB frame (config 17, s=0):
`operating_mode = CeltOnly`, `silk_bandwidth = None`,
`silk_frames_per_channel = None`, `total_silk_frames() = 0`,
`has_per_frame_lbrr_bits() = false`.

Thirteen new unit tests cover the SILK-only Table 2 row-by-row
expectations (12 cells × `(toc_bandwidth, frame_size, silk_bandwidth,
silk_frames_per_channel)`), the Hybrid WB-pin (4 Hybrid configs × the
SWB→WB / FB→WB downgrade), CELT-only frames sweep across mono / stereo
(16 × 2 configs), the §4.2.4 per-frame LBRR gate against every Table 2
cell (32 configs), the `total_silk_frames` formula across all 32 ×
{mono, stereo}, a 60 ms stereo SILK-only worked example, the `c`-bit
independence of the routing (the §3.2 frame-count code never affects
the §4 dispatch), the channel-mapping pass-through for CELT-only, the
`OperatingMode::from(Mode)` bijection, the `SilkBandwidth::to_bandwidth`
lift, and the `silk_layer ⇔ silk_bandwidth.is_some() ⇔
silk_frames_per_channel.is_some()` invariants across the entire Table 2
grid.

Total test count: 362 lib tests (was 349 after round 20).

## Round 20 — first CELT-layer fragment (2026-05-26)

Round 20 lands the §4.3 / Table 56 pre-band header symbols every
CELT-bearing Opus frame opens with, behind a new `celt_header`
module exposing `CeltHeaderPrefix` / `CeltPostFilter`. These are
the only Table-56 entries that fit between the SILK pipeline now
wired up and the two known-blocked CELT sub-pieces (§4.3.2.1
coarse energy, gated on the Laplace decoder + `e_prob_model`
table; §4.3.3 bit allocation, gated on `cache_caps50` +
`LOG2_FRAC_TABLE`). The per-band `tf_change` flags (§4.3.1) live
in the band loop after coarse energy per Table 56, so they're
deferred as well.

The decode order encoded by `CeltHeaderPrefix::decode` mirrors
Table 56: `silence` via the 2-entry `{32767, 1}/32768` iCDF
(short-circuits the rest of the prefix when set); `post-filter`
via `dec_bit_logp(1)` (logp=1, PDF `{1, 1}/2`); if post-filter is
enabled, the §4.3.7.1 four-parameter group — `octave` via
`dec_uint(6)` (uniform on `0..=5`), `fine_pitch` via
`dec_bits(4 + octave)` (at most 9 raw bits), the §4.3.7.1 pitch
period reconstruction `T = (16 << octave) + fine_pitch - 1`
(global bounds `15..=1022`; per-octave lower bounds
`{15, 31, 63, 127, 255, 511}` and per-octave upper bounds
`{30, 62, 126, 254, 510, 1022}`), `gain_index` via `dec_bits(3)`
(downstream gain `G = 3 * (gain_index + 1) / 32`), and `tapset`
via the §4.3.7.1 `{2, 1, 1}/4` iCDF — and finally `transient`
(§4.3.1) and `intra` (§4.3.2.1), both as `dec_bit_logp(3)` (PDF
`{7, 1}/8`).

Ten new unit tests cover the iCDF transcription self-checks
(silence PDF sums to 32768, tapset PDF sums to 4, both iCDF
arrays terminate at zero and decrease monotonically), the pitch
period formula at the global minimum (15), the global maximum
(1022), the lower bound of each octave (`fine_pitch = 0`), and
the upper bound of each octave (`fine_pitch = (1 << (4+k)) - 1`),
an all-zero buffer where every most-likely-symbol branch fires
(no silence / no post-filter / no transient / no intra), an
all-ones buffer where every produced field still stays in its
declared range, a `tell()`-advance proof, a 256-buffer
fuzz-style range sweep over the post-filter fields, and the
silence-shortcut post-condition.

Total test count: 349 lib tests across SILK + CELT-header (was
339 after round 19).

The §4.3.4 PVQ shape decoder, §4.3.5 anti-collapse, §4.3.6
denormalization, and the §4.3.7 inverse MDCT plus its
post-filter application all remain ahead, sitting behind the
two §4.3.2.1 / §4.3.3 blockers above.

The prior implementation was retired under the workspace clean-room
policy: provenance for several core modules could not be defended
against the "no external library source as reference" rule that
governs every crate in this workspace. Per workspace policy, the only
acceptable response is a full clean-room re-implementation against the
Opus standards documents and black-box validator binaries.

Round 1 (2026-05-20) landed the RFC 6716 §3.1 packet TOC byte parser:
the 32-config × stereo-flag × frame-count-code triple plus the
implied `(min, max)` frame-count range. Five unit tests sweep Table 2,
Table 3, Table 4 and the R1 empty-packet rejection.

Round 2 (2026-05-21) lands the RFC 6716 §3.2 frame-packing parser
behind a new `OpusPacket::parse` entry point:

* **Code 0** (§3.2.2) — one frame, the remaining `N - 1` bytes.
* **Code 1** (§3.2.3) — two equal-size frames; rejects odd `(N - 1)`
  per requirement R3.
* **Code 2** (§3.2.4) — two frames with a one- or two-byte §3.2.1
  length prefix for the first; rejects R4 violations
  (length-exceeds-remaining, length-byte missing, etc.).
* **Code 3** (§3.2.5) — signalled frame count `M ∈ 1..=48` (R5) in
  the frame-count byte, optional Opus padding (with the §3.2.5
  "value 255 chains another length byte" extension), then either CBR
  (every frame is `R / M` bytes; R6 enforces `R % M == 0`) or VBR
  (`M − 1` §3.2.1 length sequences with the final frame implicit;
  R7 enforces no length overrun).

The §3.2.1 helper decodes the one- and two-byte length sequence
(`0`, `1..=251`, `252..=255 → (second*4 + first)`) and treats length
zero as a valid DTX / lost-frame marker (zero-byte slice in the
returned list).

`OpusPacket::frames()` returns `&[&[u8]]` borrowed from the input
buffer; the slices are ready to feed into the SILK / CELT decoders
once those land. Padding length is exposed separately so the caller
can sanity-check against the §3.2.5 budget.

Twenty-seven new unit tests cover each `c` code (round-trip plus
under-length and over-length rejections), the §3.2.1 length encoding
end-to-end (including the 252/255 extension boundaries), the
padding-chain 255-extension behaviour, the R5 cap at 48 frames, and
the R6/R7 boundary conditions.

Round 3 (2026-05-21) lands the RFC 6716 §4.1 range decoder behind a
new `RangeDecoder` API. This is the shared entropy primitive that
every SILK and CELT symbol passes through. The implementation covers:

* §4.1.1 initialization (`b0 >> 1` into `val`, leftover bit into the
  renorm buffer, immediate renormalization to the `rng > 2^23`
  invariant).
* §4.1.2 generic symbol decode (`ec_decode` / `ec_dec_update`) and
  §4.1.2.1 renormalization (MSB-first byte intake with the
  zero-extension past end-of-frame).
* §4.1.3.1 `decode_bin` for power-of-two `ft`.
* §4.1.3.2 `dec_bit_logp` for `2^-logp` binary symbols.
* §4.1.3.3 `dec_icdf` for inverse-CDF table decoding.
* §4.1.4 `dec_bits` for raw bits packed LSB-first from the end of
  the frame, with §4.1.4 zero-extension.
* §4.1.5 `dec_uint` covering both the small (`ftb <= 8`) range-coded
  branch and the large (`ftb > 8`) range-plus-raw-bits branch, with
  the §4.1.5 corrupt-frame error-flag latch.
* §4.1.6.1 `tell()` and §4.1.6.2 `tell_frac()` accounting, satisfying
  the `tell() == ceil(tell_frac() / 8.0)` identity.

The sibling `oxideav-celt` crate carries an independent clean-room
copy of the same primitive — both crates own their own copy until a
shared low-level primitives crate is introduced.

Nineteen new unit tests cover: initialization on empty + non-empty
buffers, `dec_bit_logp` bias under extreme inputs, raw-bit LSB-first
ordering, zero-extension past EOF, `dec_uint` degenerate (`ft=0`,
`ft=1`) and both ftb regimes, `decode_bin` matching the generic
`decode(1<<ftb)` path bit-for-bit, `dec_icdf` agreement with
`dec_bit_logp` on binary distributions plus uniform and
single-symbol coverage, `tell()` and `tell_frac()` monotonicity, the
§4.1.6.1 ceiling identity, and the `dec_bits` zero-width and
over-large-width guards.

Round 4 (2026-05-21) lands the SILK per-frame header decoder for
RFC 6716 §4.2.7.1 through §4.2.7.5.1 behind a new `SilkFrameHeader`
type. The caller passes a `SilkFrameHeaderConfig` describing whether
the current SILK frame is mid- or side-channel of a stereo Opus
frame, the side-channel-required flag (driving §4.2.7.2), the frame
kind (regular-inactive / regular-active / LBRR), and the SILK-layer
bandwidth (NB / MB / WB). `decode` returns:

* `stereo_pred: Option<StereoPredictionWeights>` per §4.2.7.1 — the
  three sub-symbols (Table 6 stage-1 25-cell PDF, two stage-2 3-cell
  PDFs, two stage-3 5-cell PDFs) composed via the §4.2.7.1 formula
  into `(w0_Q13, w1_Q13)` against Table 7 (16-entry Q13 weight
  table).
* `mid_only_flag: Option<bool>` per §4.2.7.2 (Table 8 PDF
  `{192, 64}/256`).
* `frame_type: u8` ∈ `0..=5` per §4.2.7.3 (Table 9 inactive / active
  PDFs; active rows are transcribed as 4-entry iCDFs with a +2
  caller offset since the §4.1.3.3 primitive cannot model
  leading-zero-mass cells).
* `signal_type: SignalType`, `qoff_type: QuantizationOffsetType`
  decoded from `frame_type` via Table 10.
* `lsf_stage1: u8` ∈ `0..32` per §4.2.7.5.1 with PDF chosen from
  Table 14 by `(bandwidth, signal_type)`.

Seventeen new unit tests cover PDF→iCDF transcription self-checks
(Tables 6 / 8 / 9 / 14 each sum to 256), the Table 7 weight-table
symmetry (`w[15-k] == -w[k]`), the Table 10 frame-type → signal /
qoff mapping, end-to-end decode against the range coder for the
mono-inactive, mono-active, stereo-mid (with both stereo prediction
weights and mid-only flag), stereo-side, and LBRR configurations,
plus a random-buffer sweep of the stereo-prediction decoder to
confirm `wi*` clamping keeps the Table 7 lookup in-bounds.

Round 5 (2026-05-22) lands the SILK subframe quantization-gain
decoder for RFC 6716 §4.2.7.4 behind a new `SubframeGains` /
`SubframeGainsConfig` API. The caller passes the signal type
(`SignalType` from the §4.2.7.3 frame-type symbol), the subframe
count (2 for 10 ms SILK frames, 4 for 20 ms / Hybrid), whether the
first subframe is independently coded per the §4.2.7.4 enumeration
("first SILK frame of its type for this channel in the current Opus
frame, OR previous SILK frame of the same type was not coded"), and
the previous SILK frame's last-subframe `log_gain` if available.
`decode` returns:

* An array of up to 4 `SubframeGain { log_gain: u8 }` values in
  `0..=63`.
* The independent path decodes the 3-bit MSB from one of three
  signal-type-conditioned PDFs (Table 11: Inactive `{32, 112, 68,
  29, 12, 1, 1, 1}/256`; Unvoiced `{2, 17, 45, 60, 62, 47, 19,
  4}/256`; Voiced `{1, 3, 26, 71, 94, 50, 9, 2}/256`), then a
  uniform 3-bit LSB from Table 12 `{32, …, 32}/256`. The two are
  joined into `gain_index = (msb << 3) | lsb` and clamped with
  `log_gain = max(gain_index, previous_log_gain - 16)` (the clamp
  is skipped after a decoder reset / on a side channel whose
  predecessor was not coded — caller passes `None`).
* The delta path decodes a 41-symbol `delta_gain_index` from Table
  13 `{6, 5, 11, 31, 132, 21, 8, 4, 3, 2, 2, 2, 1, 1, …, 1}/256`
  then folds it into the previous coded gain via
  `log_gain = clamp(0, max(2*delta - 16, prev + delta - 4), 63)`.

The §4.2.7.4 tail-end `silk_log2lin` conversion to `gain_Q16` lives
in the excitation stage and is intentionally left to a later round.

Twenty new unit tests cover PDF→iCDF transcription self-checks
(Tables 11 / 12 / 13 each sum to 256), the four signal-type → iCDF
routings, the §4.2.7.4 clamp behaviour (no prev / low prev no-op /
high prev raises floor / sub-16 prev saturates at 0), the delta
path's dual-max + clamp formula reproduced against an independent
range-decoder pass, end-to-end decode for mono-inactive 4-subframe,
mono-unvoiced 2-subframe with prev, mono-voiced 4-subframe with
prev (asserting the clamp floor), the rejection of a
"first-subframe delta without prev" / non-{2,4} num_subframes
malformed input, and a four-subframe chain-consistency check that
re-derives the gain chain from the raw PDF reads.

Round 6 (2026-05-22) lands the SILK Normalized LSF Stage-2 decoder
for RFC 6716 §4.2.7.5.2 behind a new `LsfStage2` API. The caller
passes the SILK-layer bandwidth (NB / MB / WB) and the stage-1 index
`I1 ∈ 0..32` (returned by the §4.2.7.5.1 decoder). `decode` returns:

* `i2: &[i8]` of length `d_LPC` (10 for NB / MB, 16 for WB) — the
  signed stage-2 residual indices `I2[k] ∈ [-10, 10]`. Each
  coefficient reads one symbol from one of the 16 Table 15 (NB / MB
  `a..h`) or Table 16 (WB `i..p`) PDFs, indexed by
  Table 17 / Table 18 against `(I1, k)`. The raw symbol `0..=8` is
  shifted by `-4`; if the resulting `|idx| == 4`, a second symbol
  is drawn from the Table 19 extension PDF (7-cell
  `{156, 60, 24, 9, 4, 2, 1}/256`) and added to the magnitude with
  the same sign.
* `res_q10: &[i32]` of length `d_LPC` — the Q10 stage-2 residual
  after the §4.2.7.5.2 backwards-prediction inverse. The recursion
  runs `k = d_LPC-1` down to `0` per
  `res_Q10[k] = (k+1 < d_LPC ? (res_Q10[k+1]*pred_Q8[k])>>8 : 0)
  + ((((I2[k]<<10) - sign(I2[k])*102) * qstep) >> 16)`. `qstep` is
  `11796` (Q16, ≈0.18) for NB / MB and `9830` (≈0.15) for WB. The
  Q8 prediction weight `pred_Q8[k]` is one of A/B (NB/MB) or C/D
  (WB) from Table 20, selected per-coefficient by Table 21 / 22.

The RFC's Table 17 row label at `I1 = 6` is mistyped as "g" in the
source PDF; the row's cells (`a c c c c c c c c b`) are valid
codebook letters and the table is transcribed with the I1 row-label
restored. A unit test pins the exact row contents.

Thirty new unit tests cover the 16 Table 15 / Table 16 PDF→iCDF
transcriptions (each sums to 256 with monotone-decreasing iCDFs),
the Table 19 extension PDF, the four Table 17 / 18 / 21 / 22 table
row-widths and value ranges, the `pred_weight` A↔B and C↔D
resolution, end-to-end decode for NB/MB/WB at several `I1` values
(asserting every `i2[k] ∈ [-10, 10]`), rejection of `I1 ≥ 32` /
SWB / FB, the `res_Q10[]` formula re-derivation against the decoded
`i2[]` for both NB/MB and WB, a sweep of all 32 `I1` values across
{NB, MB, WB}, and a `tell()` monotonicity check.

Round 7 (2026-05-22) lifts `res_Q10[]` to the final normalized LSF
vector `NLSF_Q15[]` per RFC 6716 §4.2.7.5.3 behind a new
`NlsfReconstructed::from_stage1_and_stage2(bandwidth, lsf_stage1,
&stage2)` API. Three steps run inline:

* **Table 23 / 24 stage-1 codebook lookup.** 32 × 10 NB/MB and
  32 × 16 WB rows of `cb1_Q8[]` are transcribed verbatim. The
  `(bandwidth, I1) → cb1_Q8[..d_LPC]` mapping is the `cb1_q8()`
  helper.
* **IHMW weights `w_Q9[k]`.** Closed-form derivation from
  `cb1_Q8[]` with boundary `cb1_Q8[-1] = 0` /
  `cb1_Q8[d_LPC] = 256`:
  `w2_Q18[k] = (1024 / d_left + 1024 / d_right) << 16`
  (integer division), reduced through `i = ilog(w2_Q18)`,
  `f = (w2_Q18 >> (i-8)) & 127`,
  `y = ((i & 1) ? 32768 : 46214) >> ((32-i) >> 1)`,
  `w_Q9[k] = y + ((213 * f * y) >> 16)`. The spec asserts the
  resulting 13-bit weights tabulate to `1819..=5227` — a property
  the test sweep verifies across all 32 × {NB, MB, WB} codebook
  rows.
* **Final reconstruction.**
  `NLSF_Q15[k] = clamp(0, (cb1_Q8[k]<<7)
                       + (res_Q10[k]<<14) / w_Q9[k], 32767)`
  with integer division. Each `NLSF_Q15[k]` is held as `i16` in
  `[0, 32767]`.

26 new unit tests (144 lib tests total in the crate, up from 118 at
round-6 close) cover Table 23 / 24 transcription (strict monotone +
row widths + spot-checks of rows 0 and 31), the `cb1_q8()` routing
table (Nb/Mb → 23, Wb → 24, plus Swb/Fb and out-of-range I1
rejection), `ilog()` against the seven RFC §1.1.10 examples,
concrete hand-computed IHMW matches (NB I1=0 k=0 → 2897; WB I1=0
k=0 → 3657), the IHMW 13-bit-range assertion across every cell,
the zero-residual identity `NLSF_Q15[k] == cb1_Q8[k] << 7`, and
all-`I1` round-trips on a synthetic range-decoder buffer for NB /
MB / WB confirming the final `NLSF_Q15[]` exactly matches the
formula re-applied to `res_Q10[k]` and `w_Q9[k]`.

Round 8 (2026-05-23) stabilizes the reconstructed `NLSF_Q15[]` per
RFC 6716 §4.2.7.5.4 behind a new
`NlsfStabilized::from_reconstructed(bandwidth, &recon)` API, ensuring
consecutive coefficients stay at least the Table 25 minimum spacing
apart (the 0.01-percentile spacing of the SILK training set). The
boundary conventions are `NLSF_Q15[-1] = 0` and `NLSF_Q15[d_LPC] =
32768`; Table 25's `NDeltaMin_Q15[]` carries `d_LPC + 1` entries (one
trailing entry for the spacing against the implicit upper edge).

* **Up to 20 distortion-minimizing passes.** Each pass scans
  `i ∈ 0..=d_LPC` for the smallest `NLSF_Q15[i] - NLSF_Q15[i-1] -
  NDeltaMin_Q15[i]` (ties to lower `i`). If non-negative, the
  coefficients already satisfy every constraint and the procedure
  stops. Otherwise: `i == 0` sets `NLSF_Q15[0] = NDeltaMin_Q15[0]`;
  `i == d_LPC` sets `NLSF_Q15[d_LPC-1] = 32768 - NDeltaMin_Q15[d_LPC]`;
  any interior `i` re-centres the pair via the `min_center` /
  `max_center` running-sum band and the
  `center_freq = clamp(min_center, (NLSF[i-1]+NLSF[i]+1)>>1,
  max_center)` midpoint, then writes
  `NLSF_Q15[i-1] = center_freq - (NDeltaMin_Q15[i]>>1)` and
  `NLSF_Q15[i] = NLSF_Q15[i-1] + NDeltaMin_Q15[i]`.
* **Fallback (once, after the 20th pass).** Sort ascending, then a
  forward `max(NLSF[k], NLSF[k-1] + NDeltaMin[k])` sweep and a
  backward `min(NLSF[k], NLSF[k+1] - NDeltaMin[k+1])` sweep that
  mechanically guarantee the spacing. Per the **RFC 8251 §7**
  erratum the forward sweep's addition is performed with 16-bit
  saturating addition (`silk_ADD_SAT16`) so an adversarial input near
  `i16::MAX` cannot wrap around into a negative value.

19 new unit tests cover Table 25 lengths and spot-checks (NB/MB index
0 = 250 / index 10 = 461; WB index 0 = 100 / index 2 = 40 / index 16
= 347), the SWB/FB column rejection, `add_sat16` saturation, an
"already-stable input is left untouched" identity for NB and WB, the
two boundary branches (first coefficient pushed up to `NDeltaMin[0]`,
last coefficient pulled down to `32768 - NDeltaMin[d_LPC]`), an
interior re-centring with hand-computed exact `NLSF_Q15[i-1]` /
`NLSF_Q15[i]` values, the fallback path on a fully reversed input,
all-zero and all-32767 inputs spread to valid spacing, the RFC 8251
no-wrap guard near `i16::MAX`, an all-`I1` × {NB, MB, WB} end-to-end
sweep wired through the §4.2.7.5.2 / §4.2.7.5.3 decoders (asserting
the spacing post-condition, the `[0, 32767]` bound, and strict
monotonicity), length-matches-bandwidth checks, and the SWB/FB +
length-mismatch rejections.

Round 9 (2026-05-24) lands the SILK Normalized LSF interpolation for
RFC 6716 §4.2.7.5.5 behind a new `LsfInterpolated` /
`LsfInterpContext` API. For a 20 ms SILK frame the first half (the
first two subframes) may use NLSF coefficients interpolated between
the most recent coded frame's vector `n0_Q15[]` and the current
stabilized vector `n2_Q15[]`. `decode` takes the range decoder, the
§4.2.7.5.4 `NlsfStabilized` (`n2`), the prior frame's `n0_Q15[]`
(or `None`), and an `LsfInterpContext`:

* **`TwentyMs`** — decode the Q2 factor `w_Q2 ∈ 0..=4` from the
  Table 26 PDF (`{13, 22, 29, 11, 181}/256`, iCDF `[243, 221, 192,
  181, 0]`) and compute
  `n1_Q15[k] = n0_Q15[k] + (w_Q2*(n2_Q15[k] - n0_Q15[k]) >> 2)`.
* **`TwentyMsAfterResetOrUncoded`** — the factor is still decoded
  (the range coder must stay in sync) but its value is discarded and
  `4` is substituted, so `n1_Q15[] == n2_Q15[]` (no interpolation).
  This is also the behaviour whenever `n0_Q15[]` is `None`
  (no prior-frame history).
* **`TenMs`** — the factor is not present in the bitstream; nothing is
  decoded and there is no first-half vector.

The result exposes the decoded `w_q2()` (`None` for 10 ms) and the
first-half `n1_q15()` (`None` for 10 ms). The second half of a 20 ms
frame and the whole of a 10 ms frame always use `n2_Q15[]` directly —
that is the caller's responsibility.

Ten new unit tests cover the Table 26 PDF→iCDF transcription
(sum-to-256 and monotone-decreasing self-checks), the 10 ms
no-read / no-first-half path (range coder untouched), the
end-to-end 20 ms interpolation against an independent formula
re-derivation, the `w_Q2 == 0 → n0` and `w_Q2 == 4 → n2` algebraic
identities, the reset/uncoded context decoding-then-forcing-4
behaviour (with a `tell()` parity check against the normal context),
the no-history `n0 = None` forced-`n2` path, the `n0`-length-mismatch
rejection, and a sweep asserting every interpolated value stays in
`[0, 32767]` across {NB, MB, WB} × all 32 `I1` × `w_Q2 ∈ 0..=4`.

Round 10 (2026-05-24) lands the SILK Normalized LSF → LPC core
conversion for RFC 6716 §4.2.7.5.6 behind a new `LpcQ17` API. Given a
stabilized / interpolated `nlsf_q15[]` (the §4.2.7.5.4 / §4.2.7.5.5
output) and the SILK-layer bandwidth (NB / MB / WB), the three-step
`silk_NLSF2A` procedure runs:

* **`silk_NLSF2A_cos` (Table 27 + Table 28).** The 129-entry Q12
  cosine table (`cos_Q12[0]=4096`, `cos_Q12[64]=0`,
  `cos_Q12[128]=-4096`, anti-symmetric about i=64) is transcribed
  verbatim. Each coefficient splits into top-7-bits `i = nlsf >> 8`
  and next-8-bits `f = nlsf & 255`; the §4.2.7.5.6 piecewise-linear
  interpolation `c_Q17[ordering[k]] = (cos_Q12[i]*256 +
  (cos_Q12[i+1]-cos_Q12[i])*f + 4) >> 3` populates the re-ordered Q17
  cosine vector. Table 27's `ordering[]` is `[0,9,6,3,4,5,8,1,2,7]`
  for NB/MB and `[0,15,8,7,4,11,12,3,2,13,10,5,6,9,14,1]` for WB.
* **`silk_NLSF2A_find_poly` recurrence.** Two rolling-row passes on
  the even-indexed (P) and odd-indexed (Q) `c_Q17[]` cells run
  `p[k][j] = p[k-1][j] + p[k-1][j-2] - ((c*p[k-1][j-1] + 32768)>>16)`
  with the §4.2.7.5.6 boundary conditions `p[k][j<0] = 0` and
  `p[k][k+2] = p[k][k]`. Intermediates are computed in i64 to absorb
  the spec's noted "up to 48 bits of intermediate precision".
* **`silk_NLSF2A` last-row assembly.** The final P / Q rows are
  folded into the 32-bit Q17 LPC coefficients via the §4.2.7.5.6
  sum / difference pair `a32_Q17[k] = -((q_diff) + (p_sum))` and
  `a32_Q17[d_LPC-k-1] = (q_diff) - (p_sum)`, where
  `q_diff = q[d2-1][k+1] - q[d2-1][k]` and
  `p_sum = p[d2-1][k+1] + p[d2-1][k]`.

The §4.2.7.5.7 range-limiting bandwidth-expansion loop (shrinks
`a32_Q17[]` to fit Q12) and the §4.2.7.5.8 prediction-gain stability
check (chirps until `silk_LPC_inverse_pred_gain_QA` passes) are both
deferred to subsequent rounds.

22 new unit tests (195 lib tests total in the crate, up from 173 at
round-9 close) cover Table 27 row-widths + permutation-of-`0..d_LPC`
self-checks + bandwidth routing (SWB / FB rejected), Table 28 length
+ three anchor cells + strict-monotone-decreasing pairwise check +
the anti-symmetric-about-64 invariant + Q12-range bound + four row
spot-checks, `nlsf_to_c_q17` at the table anchor points (`f == 0`
round-trip against `cos_Q12[8*k]`) and at the linear-interpolation
midpoint (`f == 128` matching the `16*(a+b)` algebraic identity),
SWB / FB and length-mismatch rejection, the production
`LpcQ17::from_nlsf` agreeing bit-for-bit with an independent
2D-matrix spec-transcription oracle on synthetic ascending NLSF
vectors for both NB and WB, the same production / oracle agreement
across the full §4.2.7.5.2 → §4.2.7.5.3 → §4.2.7.5.4 pipeline ×
all 32 `I1` × {NB, MB, WB}, and a no-panic sweep over three buffers
× all 32 `I1` × {NB, MB, WB}.

Round 11 (2026-05-24) lands the SILK LPC range-limiting bandwidth
expansion for RFC 6716 §4.2.7.5.7 behind a new `LpcQ17::range_limited`
method. Given the raw §4.2.7.5.6 `a32_Q17[]` (which is too large to fit
a signed 16-bit value), the procedure shrinks the coefficients so they
fit Q12:

* **Up to 10 rounds of `silk_bwexpander_32` chirping.** Each round finds
  the index `k` with the largest `abs(a32_Q17[k])` (ties to the lowest
  `k`), computes `maxabs_Q12 = min((maxabs_Q17 + 16) >> 5, 163838)`, and
  stops once `maxabs_Q12 <= 32767`. Otherwise it derives the chirp factor
  `sc_Q16[0] = 65470 - ((maxabs_Q12 - 32767) << 14) /
  ((maxabs_Q12 * (k+1)) >> 2)` (integer division) and runs the recurrence
  `a32_Q17[k] = (a32_Q17[k]*sc_Q16[k]) >> 16`,
  `sc_Q16[k+1] = (sc_Q16[0]*sc_Q16[k] + 32768) >> 16`. The first multiply
  runs in i64 ("up to 48 bits of precision"); the second is unsigned per
  the §4.2.7.5.7 note to avoid 32-bit overflow.
* **Post-loop Q12 saturation.** If `maxabs_Q12` is still greater than
  32767 after the 10th round, each coefficient is saturated in the Q12
  domain and converted back to Q17:
  `a32_Q17[k] = clamp(-32768, (a32_Q17[k] + 16) >> 5, 32767) << 5`. In
  practice the adaptive chirp converges every realistic input within 10
  rounds, so this branch is the spec-documented belt-and-suspenders step.

The output is held in the Q17 domain (the §4.2.7.5.8 prediction-gain
limiting that follows consumes Q17 coefficients), so it shares the
`LpcQ17` representation. `maxabs_Q17` is taken via `i32::unsigned_abs()`
so an `i32::MIN` coefficient cannot panic.

Six new unit tests (201 lib tests total in the crate, up from 195 at
round-10 close) cover the small-coefficient pass-through, production /
independent-i128-oracle agreement on synthetic overflow vectors and on
the 163838-cap extreme, the Q12-fit post-condition, the `i32::MIN`
no-panic edge, the post-loop saturation formula pinned in isolation, and
a real §4.2.7.5.2 → §4.2.7.5.7 pipeline sweep across all 32 `I1` values
× {NB, MB, WB}.

Round 12 (2026-05-24) lands the SILK LPC prediction-gain limiting for
RFC 6716 §4.2.7.5.8 behind a new `LpcQ17::prediction_gain_limited` method
returning a new `LpcQ12` type. Even after the §4.2.7.5.7 range-limiting,
the filter may have so much prediction gain that it is unstable; this
stage drives up to 16 rounds of bandwidth expansion off the
`silk_LPC_inverse_pred_gain_QA` stability test rather than the coefficient
magnitude:

* **`silk_LPC_inverse_pred_gain_QA` stability test (`is_lpc_stable`).**
  Each round converts to the real Q12 coefficients `a32_Q12[n] =
  (a32_Q17[n] + 16) >> 5` and runs the DC-response check (`DC_resp =
  Σ a32_Q12[n] > 4096` ⇒ unstable) followed by a fixed-point Levinson
  recurrence on the Q24-widened coefficients (`inv_gain_Q30[d_LPC] =
  1<<30`, `a32_Q24[d_LPC-1][n] = a32_Q12[n] << 12`). For `k` from
  `d_LPC-1` down to `0` it rejects on `abs(a32_Q24[k][k]) > 16773022`
  (≈ 0.99975 in Q24) or `inv_gain_Q30[k] < 107374` (≈ 1/10000 in Q30),
  and otherwise (for `k > 0`) computes row `k-1` via the spec's
  `b1 = ilog(div_Q30)` / `inv_Qb2` / `err_Q29` / `gain_Qb1` / `num_Q24` /
  `a32_Q24[k-1][n]` formulas. Every spec-flagged ">32-bit" multiply runs
  in i64.
* **Stability-driven chirp loop.** If stable, the Q12 coefficients are
  returned; otherwise a chirp round with `sc_Q16[0] = 65536 - (2<<i)` is
  applied via the same `silk_bwexpander_32` as §4.2.7.5.7. On round 15
  `sc_Q16[0]` is `0`, zeroing every coefficient so an all-zero (trivially
  stable) filter is the worst-case outcome.

`LpcQ12` exposes `a_q12()`, `len()`, `is_empty()`, and `rounds()` (chirp
rounds run before stability).

Nine new unit tests (210 lib tests total in the crate, up from 201 at
round-11 close) cover `is_lpc_stable` agreement with an independent
2D-matrix spec oracle on hand-built filters, the all-zero stable case,
DC-response rejection, a round-0 pass-through on a typical decoded NLSF
vector, deliberately-unstable inputs always converging to a stable filter
within ≤ 16 rounds, the forced round-15 zeroing, the signed-16-bit Q12
fit, a real §4.2.7.5.2 → … → §4.2.7.5.8 pipeline sweep across all 32 `I1`
× {NB, MB, WB} on three buffers, and the `ilog64` §1.1.10 boundaries.

Round 13 (2026-05-24) lands the SILK Long-Term Prediction parameters
for RFC 6716 §4.2.7.6 behind a new `LtpParameters` / `LtpConfig` API.
The caller passes the SILK-layer bandwidth (NB / MB / WB), the signal
type from §4.2.7.3, the subframe count (2 for 10 ms; 4 for 20 ms /
Hybrid), a `LagCoding` enum selecting absolute vs relative primary-lag
coding (with the prior frame's unclamped primary lag for the latter),
and a boolean for whether the §4.2.7.6.3 LTP scaling field is present.
`decode` returns:

* **§4.2.7.6.1 pitch lags.** Non-voiced frames consume no bits.
  Voiced frames decode the primary lag either as
  `lag = lag_high * lag_scale + lag_low + lag_min` (absolute path:
  Table 29 32-entry high-part PDF + Table 30 bandwidth-conditioned
  low-part PDF + scale `{4, 6, 8}` for `{NB, MB, WB}` + `lag_min`
  `{16, 24, 32}` + `lag_max` `{144, 216, 288}`), or as
  `lag = previous_lag + (delta_lag_index - 9)` (relative path:
  Table 31 21-entry delta PDF, with a decoded delta of 0 falling back
  to the absolute-coding sub-path that reads the high + low parts).
  The pitch-contour VQ index follows from one of the four Table 32
  PDFs picked by `(bandwidth, num_subframes)`, then the per-subframe
  lag is `pitch_lags[k] = clamp(lag_min, lag + lag_cb[contour_index][k],
  lag_max)` with the offsets from Tables 33 (NB 10 ms, 3 entries × 2),
  34 (NB 20 ms, 11 × 4), 35 (MB/WB 10 ms, 12 × 2) and 36 (MB/WB
  20 ms, 34 × 4). The primary lag itself is held unclamped per the
  §4.2.7.6.1 note so the next frame's relative coding remains
  consistent.
* **§4.2.7.6.2 LTP filter coefficients.** A 3-entry periodicity
  index (Table 37 PDF `{77, 80, 99}/256`) gates one of three filter
  codebooks; each subframe then decodes a filter index from the
  periodicity-conditioned PDF in Table 38 (codebook sizes 8 / 16 /
  32) into a 5-tap signed Q7 filter from Tables 39 (periodicity 0),
  40 (periodicity 1) or 41 (periodicity 2).
* **§4.2.7.6.3 LTP scaling.** When `ltp_scaling_present` is true, a
  3-entry index from the Table 42 PDF `{128, 64, 64}/256` selects a
  Q14 scale factor from `{15565, 12288, 8192}` (≈ 0.95 / 0.75 / 0.5).
  When absent the default `15565` is used and no bits are consumed.
  Non-voiced frames also use the default.

The §4.2.7.9 LTP synthesis filter that consumes these parameters is
intentionally left to a later round — this module only produces the
decoded parameter set.

Nineteen new unit tests (229 lib tests total in the crate, up from 210
at round-12 close) cover the PDF → iCDF transcriptions for Tables 29 /
30 (per-bandwidth) / 31 / 32 (all four PDFs) / 37 / 38 (all three
codebooks) / 42 (each sums to 256, strictly monotone-decreasing iCDF,
terminator 0), Table 30 scale + min-lag + max-lag values, the
contour-codebook size-matches-PDF self-checks plus index-0 (all-zero
offset) and several interior-row spot-checks against the spec
(`CONTOUR_NB_20MS[1] == [2,1,0,-1]`, `CONTOUR_MBWB_20MS[33] == [-9,-3,
3,9]`, `CONTOUR_MBWB_10MS[11] == [-3,3]`), the LTP-filter-codebook
sizes (8 / 16 / 32) and four boundary-row spot-checks against Tables
39–41 (`P0[0]=[4,6,24,7,5]`, `P0[7]=[16,14,38,-3,33]`,
`P1[15]=[3,-1,21,16,41]`, `P2[31]=[2,0,9,10,88]`), the no-bits-consumed
property for non-voiced frames (both Inactive and Unvoiced signal
types), the malformed-config rejections (non-2-non-4 subframe count;
SWB / FB bandwidth), the in-range + formula-match property for absolute
coding across {NB, MB, WB} × {2, 4} subframes (independent re-derivation
of the production decode), the relative-coding non-zero-delta path
(`primary = previous_lag + (delta - 9)`), the relative-coding zero-delta
fallback into the absolute sub-path, the LTP-scaling-present path's
output landing in `{15565, 12288, 8192}`, the LTP-scaling-absent path
consuming strictly fewer bits than the present path, and a sweep
across {NB, MB, WB} × {2, 4} subframes × {absent, present} scaling ×
{Absolute, Relative} coding × three buffers that asserts no panics, the
`[lag_min, lag_max]` clamp post-condition, and the periodicity ≤ 2
invariant.

Round 14 (2026-05-25) lands the SILK Linear Congruential Generator
seed for RFC 6716 §4.2.7.7 behind a new `decode_lcg_seed` helper, plus
the full SILK excitation decoder for §4.2.7.8 behind a new
`Excitation` / `ExcitationConfig` API. The LCG seed reads a single
symbol from the uniform 4-entry Table 43 PDF (`{64, 64, 64, 64}/256`),
yielding a value in `0..=3` that initialises the pseudorandom sign
generator used by §4.2.7.8.6 reconstruction.

The §4.2.7.8 excitation decodes in six substeps:

* **§4.2.7.8.1 Rate level.** A single symbol per SILK frame drawn from
  one of two Table 45 PDFs selected by `(signal_type)` —
  `{15, 51, 12, 46, 45, 13, 33, 27, 14}/256` for Inactive/Unvoiced and
  `{33, 30, 36, 17, 34, 49, 18, 21, 18}/256` for Voiced. The decoded
  value `0..=8` indexes the per-block pulse-count PDF table.
* **§4.2.7.8.2 Pulses per shell block.** Table 44 routes
  `(bandwidth, frame_size)` to the shell-block count (5, 8, 10, 10,
  15, 20 for the six (NB/MB/WB × 10ms/20ms) cells). For each block,
  read from the rate-level-`r` PDF in Table 46. The special value 17
  flags "extra LSB present" — re-read from rate level 9; if the result
  is 17 again, re-read from level 9; on the tenth consecutive 17,
  switch to rate level 10, whose cell-17 probability is exactly zero
  (capping extra LSBs at 10 per block per the §4.2.7.8.2 note).
* **§4.2.7.8.3 Pulse locations.** A recursive-partition decoder runs
  per block with pulse count > 0: at each level the partition halves
  (16 → 8 → 4 → 2 → 1) and the left-half pulse count is decoded from
  the Table 47 / 48 / 49 / 50 split PDF (one PDF per `(partition_size,
  pulse_count)` cell). When the partition collapses to a single
  sample, the remaining pulse count is the sample's magnitude.
* **§4.2.7.8.4 LSB decoding.** For each block with `lsbs > 0`, read
  one binary symbol from the Table 51 PDF (`{136, 120}/256`) for every
  coefficient (even those with zero pulses) for `lsbs` iterations
  MSB-first, doubling the running magnitude and adding each bit.
* **§4.2.7.8.5 Sign decoding.** For every coefficient with magnitude
  > 0, read one binary symbol from the Table 52 PDF chosen by
  `(signal_type, qoff_type, min(pulses_in_block, 6))`. A 0 means
  negate; a 1 means keep positive. The pulse count for sign-PDF
  selection is the initial pre-LSB count.
* **§4.2.7.8.6 Reconstruction.** For each sample:
  `e_Q23[i] = (e_raw[i] << 8) - sign(e_raw[i])*20 + offset_Q23` with
  `offset_Q23` per Table 53 (`{Inactive,Unvoiced}/Low=25,
  /High=60; Voiced/Low=8, /High=25`), then a 32-bit LCG step
  `seed = (196314165*seed + 907633515) & 0xFFFFFFFF`. If the LCG MSB
  (`seed & 0x80000000`) is set, `e_Q23[i]` is negated. Finally
  `seed = (seed + e_raw[i]) & 0xFFFFFFFF` feeds the next sample.

Thirty new unit tests (259 lib tests total in the crate, up from 229
at round-13 close) cover the Table 43 LCG-seed iCDF transcription and
the 0..=3 + bits-consumed properties; Table 44 (all six valid
(bandwidth × frame_size) cells plus SWB/FB rejection); the two Table
45 rate-level PDFs; all eleven Table 46 pulse-count PDFs (sums to 256,
iCDF transcription, plus the L10 cell-17 = 0 boundary that caps the
LSB-chain depth); one spot-check per Table 47/48/49/50 (1- and ≥7-
pulse cells); Table 51 LSB PDF; six Table 52 sign PDFs across each
`(signal_type, qoff_type)` quadrant plus the "6 or more" saturation;
all six Table 53 quantization offsets; the LCG recurrence first few
steps pinned algebraically; `Excitation::decode` rejections (invalid
LCG seed, SWB/FB bandwidth); correct sample count per (bandwidth ×
frame_size); the §4.2.7.8 "fits in 24 bits including sign" invariant
across three buffers × all (NB/MB/WB × 10/20ms) cells with high
quantization offset; per-block pulse-count ≤ 16 and LSB-count ≤ 10
invariants; a hand-pinned reconstruction of an isolated mag=5, sign=-1
sample producing ±1235 (depending on LCG flip); the zero-magnitude
sample identity `|e_Q23[i]| == offset_Q23` after the LCG step; bit-
exact reproducibility across two decoder passes of the same buffer +
config; LCG-seed divergence (different seed = different output); and a
sweep across three buffers × {NB, MB, WB} × {10, 20 ms} × 3 signal
types × 2 qoff types × 4 seeds asserting no panics.

Total crate test count: 277 (5 TOC + 27 frame-packing + 19 range
decoder + 17 SILK header + 20 subframe gains + 30 LSF stage-2 +
26 LSF reconstruction + 19 LSF stabilization + 10 LSF interpolation
+ 22 LSF → LPC core + 6 LPC range-limiting + 9 LPC prediction-gain
limiting + 19 LTP parameters + 4 LCG seed + 26 excitation + 18 LPC
synthesis).

Round 14 stops after the §4.2.7.8 excitation — the SILK frame header,
the gains, the full LSF → LPC pipeline, the long-term-prediction
parameters, the LCG seed and the full excitation reconstruction are
all decoded.

Round 15 (2026-05-25) lands the §4.2.7.9.2 SILK LPC synthesis filter
behind a new `lpc_synthesis_subframe` / `lpc_synthesis_frame` /
`LpcSynthState` API. The per-subframe short-term predictor combines
the §4.2.7.4 Q16 gain, the §4.2.7.9.1 residual `res[i]`, and the
§4.2.7.5.8 stabilised Q12 filter `a_Q12[k]` into

```
                                  d_LPC-1
                 gain_Q16[s]         __              a_Q12[k]
        lpc[i] = ----------- * res[i] + \  lpc[i-k-1] * --------
                   65536.0              /_               4096.0
                                        k=0
        out[i] = clamp(-1.0, lpc[i], 1.0)
```

The `d_LPC` unclamped `lpc[i]` history is carried across subframes via
the stateful `LpcSynthState` (cleared to zero on a decoder reset per
RFC 6716 §4.5.2 or after an uncoded regular SILK frame). The
§4.2.7.9.2 wording "the decoder saves the unclamped values lpc[i] to
feed into the LPC filter for the next subframe, but saves the clamped
values out[i] for rewhitening in voiced frames" is honoured exactly:
state holds the unclamped values; the rendered output is the clamped
vector. d_LPC routing follows §4.2.7.5 — 10 for NB / MB, 16 for WB
(SWB / FB rejected at the SILK layer). The §4.2.7.9 preamble licenses
a floating-point implementation here ("the remainder of the
reconstruction process for the frame does not need to be bit-exact"),
so the accumulator runs in `f32`.

Eighteen new unit tests (277 lib tests total in the crate, up from 259
at round-14 close) cover `subframe_samples` routing including SWB / FB
rejection; `LpcSynthState` d_LPC routing + zero initialisation + reset
to zero; the three input-validation rejections (`res` / `out_clamped`
length mismatch + `a_q12` length mismatch); the algebraic identities
(`a_Q12 = 0 → lpc = gain * res`; zero residual with zero history → zero
output regardless of a_Q12 / gain); a hand-pinned NB unity-gain
single-tap impulse response (constant 1.0); a hand-pinned WB half-gain
single-tap impulse response (geometric series `0.5^(i+1)` matched to
1e-9 precision); a hand-traced two-tap NB filter with non-trivial
`res[]` producing the exact sequence `[1.0, 2.5, 4.5, 2.875, 2.5625]`
plus the per-sample clamp; the cross-subframe history carry-over (an
impulse in subframe 0 keeps the unit-feedback filter emitting 1.0 in
subframe 1); decoder-reset path zeroes history; out ∈ `[-1.0, 1.0]`
under deliberately over-driven inputs; the unclamped-history-vs-clamped-
output distinction; `lpc_synthesis_frame` agreement with an explicit
per-subframe loop including state, plus its length-mismatch rejection;
and a no-panic sweep over {NB, MB, WB} × {10 ms, 20 ms} asserting the
clamp post-condition and the d_LPC history length.

The §4.2.7.9.1 LTP synthesis filter that produces `res[i]` for voiced
frames is now wired up — see round 16 below. The CELT band machinery
and the §5 encoder pipeline are still ahead; the higher-level encode /
decode entry points still return `Error::NotImplemented`.

Round 16 (2026-05-25) lands the §4.2.7.9.1 SILK LTP synthesis filter
behind a new `ltp_synthesis_subframe` / `ltp_synth_commit_subframe` /
`LtpSynthState` API. Two regimes per the spec:

* **Unvoiced** (`signal_type != Voiced`). The LPC residual is just a
  normalised copy of the §4.2.7.8 excitation:
  `res[i] = e_Q23[i] / 2^23`.
* **Voiced**. The 5-tap Q7 LTP convolution is applied:
  `res[i] = e_Q23[i]/2^23 + Σ_{k=0..4} res[i - pitch_lag + 2 - k] *
  b_Q7[k] / 128`. The "prior res[]" values it reads come from
  rewhitening the prior-subframe outputs through the current
  subframe's LPC coefficients (because the coefficients may have
  changed between subframes):

  * **Region A** (out[] rewhiten, indices
    `(j - pitch_lag - 2) <= i < out_end`):
    `res[i] = 4 * LTP_scale_Q14 / gain_Q16 *
    clamp(out[i] - Σ out[i-k-1] * a_Q12[k]/4096, -1, 1)`.
  * **Region B** (lpc[] rewhiten, indices `out_end <= i < j`):
    `res[i] = 65536 / gain_Q16 *
    (lpc[i] - Σ lpc[i-k-1] * a_Q12[k]/4096)`.

`out_end` and the effective `LTP_scale_Q14` follow the §4.2.7.9.1
LSF-interpolation-split branch. For the third or fourth subframe of a
20 ms SILK frame that used a `w_Q2 < 4` LSF interpolation, `out_end =
j - (s-2) * n` and `LTP_scale_Q14 = 16384`; otherwise `out_end = j -
s*n` and the §4.2.7.6.3 decoded scaling factor is used directly.

`LtpSynthState` carries the spec-stated buffer sizes — 306 samples of
`out[]` (WB max pitch 288 + d_LPC 16 + 2) and 256 samples of `lpc[]`
(3 prior WB subframes 240 + d_LPC 16) — across subframes and across
SILK frame boundaries; `reset()` clears both for the §4.5.2
decoder-reset / uncoded-side-channel-frame paths, and `start_frame()`
resets only the in-frame subframe counter without touching the
cross-frame histories. The companion `ltp_synth_commit_subframe`
pushes the §4.2.7.9.2 outputs back into the state once the LPC
synthesis filter has run.

Twenty-one new unit tests (298 lib tests total, up from 277 at
round-15 close): the constant table matches the §4.2.7.9.1 buffer-size
paragraph (`LTP_OUT_HISTORY_MAX == 306`, `LTP_LPC_HISTORY_MAX == 256`,
`LTP_SCALE_FRESH_Q14 == 16384`); `LtpSynthState::new` d_LPC routing
(NB/MB = 10, WB = 16; SWB/FB rejected); zero-initialised and
reset-zeroed histories + subframe-index; `start_frame()` preserves
histories but clears the index; `push_subframe` keeps the most-recent
samples at the tail and shifts older samples down; the unvoiced
`res[i] = e_Q23[i]/2^23` identity (Wb 80-sample sweep); the Inactive
signal type is treated as unvoiced; the four input-validation
rejections (mismatched `e_q23` / `res_out` / `a_q12` lengths;
mismatched state-vs-cfg bandwidth; out-of-range subframe index;
non-positive pitch lag for voiced); the zero-history /
zero-excitation / zero-b voiced-decode identity (output is zero); the
voiced `b == 0` identity (LTP convolution drops out, residual is
`e_Q23/2^23` regardless of prior history); the voiced `b_Q7[0] = 64`
pitch-lookback algebra (rewhitening of an injected out[] sample
matches `0.5 * 4*LTP_scale_Q14/gain_Q16 * out[j-14]`); the voiced
`b_Q7[2] = 64` region-B (lpc[]) rewhiten algebra; the
LSF-interpolation-split branch override at `subframe_index = 2` with
`lsf_interp_used = true` (effective scale becomes
`4*16384/65536 = 1.0` exactly); voiced-decode determinism (same
inputs → same outputs); and a no-panic finite-output sweep across 3
buffers × {NB, MB, WB} × {10 ms, 20 ms} × 4 subframes with histories
committed back into state via `ltp_synth_commit_subframe`.

Round 17 (2026-05-25) lands the §4.2.8 SILK stereo unmixing
(`silk_stereo_MS_to_LR`) behind a new `stereo_ms_to_lr` /
`StereoUnmixState` / `StereoWeightsQ13` / `StereoFrame` API
(`silk_stereo` module). After both stereo channels finish §4.2.7.9
reconstruction, the mid/side `out[]` signals are converted to
left/right. The side channel is predicted from a low-passed mid term
`p0 = (mid[i-2] + 2*mid[i-1] + mid[i]) / 4` and the unfiltered,
one-sample-delayed mid (`mid[i-1]`), using the §4.2.7.1 Q13 weights:

```text
 left[i] = clamp(-1.0, (1 + w1)*mid[i-1] + side[i-1] + w0*p0, 1.0)
right[i] = clamp(-1.0, (1 - w1)*mid[i-1] - side[i-1] - w0*p0, 1.0)
```

The first `n1` samples (64 NB / 96 MB / 128 WB ≈ 8 ms) interpolate the
weights linearly from the previous frame's `(prev_w0_Q13, prev_w1_Q13)`
to the current frame's `(w0_Q13, w1_Q13)`; the rest of the frame uses
the current weights (`min(i, n1)` clamps the ramp). An uncoded side
channel (§4.2.7.2) is treated as all-zero. The two trailing mid
samples, one trailing side sample, and the previous-frame weights are
carried across the frame boundary by `StereoUnmixState`, cleared to
zero on a decoder reset (`StereoUnmixState::reset`) per the §4.2.8
closing paragraph. Per the §4.2.7.9 "does not need to be bit-exact"
preamble, the stage runs in `f32`.

Nine new unit tests (307 lib tests total, up from 298 at round-16
close): the `interp_phase_samples` table (64/96/128; SWB/FB rejected);
fresh/reset state zeroing; empty-mid and mismatched-side-length
rejection; the zero-weight no-side collapse to delayed mono
(`L = R = mid[i-1]`); a hand-computed constant-weight mid/side
reconstruction (coded side, fresh history); phase-1 ramp endpoints
(effective `w1` at samples 1, `n1`, and the steady region); mid-history
carry across two frames; side-history carry across two frames; and
output clamping under oversized weights.

Round 18 (2026-05-26) lands the RFC 6716 §4.2.3 SILK packet-level
header bits and the §4.2.4 per-frame LBRR flags behind a new
`SilkHeaderBits` / `SilkChannelHeader` / `PerFrameLbrr` API
(`silk_header` module). The decoder reads, in §4.2.2 Figures 15/16
order:

* For each channel (mono: 1; stereo: 2), `N` uniform-binary VAD bits
  followed by a single global LBRR flag — all via
  `RangeDecoder::dec_bit_logp(1)`. `N = silk_frame_count(frame_size)`
  per §4.2.2 (1 for 10/20 ms Opus frames, 2 for 40 ms, 3 for 60 ms).
* For Opus frames longer than 20 ms (`N >= 2`), one §4.2.4 per-frame
  LBRR symbol per channel whose global LBRR flag is set. The Table 4
  PDFs are `{0, 53, 53, 150}/256` (40 ms) and
  `{0, 41, 20, 29, 41, 15, 28, 82}/256` (60 ms). Both have a leading
  zero entry: per §4.1.3.3 the iCDF drops that entry
  (`PER_FRAME_LBRR_{40MS,60MS}_ICDF`) and the helper adds an offset of
  1, producing a 2- or 3-bit bitmap with at least one bit set, packed
  LSB-to-MSB so bit `i` is the LBRR flag for SILK frame `i`.

For 10/20 ms Opus frames the per-frame LBRR bitmap mirrors the global
LBRR flag without consuming any extra bits — per §4.2.4 "the global
LBRR flag in the header bits is already sufficient to indicate the
presence of that single LBRR frame".

The output records each channel's VAD bitmap, the global LBRR flag,
and a fully-expanded `PerFrameLbrr { mid, side }` bitmap consumed by
the (forthcoming) §4.2.5 LBRR / §4.2.6 regular SILK loop.

Fourteen new unit tests (321 lib tests total, up from 307 at round-17
close): the Table 4 PDF/iCDF transcription self-checks (40 ms and
60 ms, including strictly-decreasing + terminator-zero invariants);
the `per_frame_lbrr_pdf` dispatch fallback; the `silk_frame_count`
§4.2.2 dispatch including the CELT-only 2.5/5 ms `None` arm; a 10 ms
mono decode that consumes exactly 2 bits; a 60 ms stereo decode that
populates all 3-bit VAD + LBRR bitmaps within range; rejection of
`num_silk_frames ∉ {1, 2, 3}`; the §4.2.3-implied per-frame LBRR
mirror on 10 ms with the global flag set (verifying no extra symbol
is consumed); the §4.2.4 skip path on 60 ms when both global LBRR
flags are unset (verifying exactly 8 bits are consumed); the VAD /
LBRR bitmap accessors for present-side and missing-side cases; and
exhaustive 40 ms / 60 ms `decode_per_frame_lbrr` symbol-range sweeps
plus a 60 ms full-coverage sweep over `{1..=7}`.

Round 19 (2026-05-26) lands the RFC 6716 §4.2.9 SILK resampler delay
budget and the internal-vs-output sample-rate accounting behind a new
`silk_resampler` module:

* **Table 54 — normative delay allocation per SILK audio bandwidth.**
  NB = 0.538 ms, MB = 0.692 ms, WB = 0.706 ms. The §4.2.9 resampler
  itself is explicitly non-normative ("a decoder can use any method it
  wants to perform the resampling"), but the delay budget is normative
  so the encoder can apply a matching pre-delay to the MDCT layer and
  keep SILK and CELT aligned across a §4.5 mode switch. `silk_resampler_delay_ms`
  returns the bandwidth's delay in milliseconds; `silk_resampler_delay_samples_at`
  scales it to a sample count at any output rate (round half away from
  zero — §4.2.9 itself notes "it may not be possible to achieve exactly
  these delays while using a whole number of input or output samples").
  SWB and FB return `None`: they never reach the §4.2.9 SILK resampler.
* **Internal SILK sample rate per bandwidth.** NB = 8 kHz, MB = 12 kHz,
  WB = 16 kHz (implied by the §4.2.1 / §4.2.7.x decode pipeline; the
  resampler bridges this to the application's chosen output rate).
  `silk_internal_rate_hz` and `silk_frame_samples_internal` cover the
  pre-resampler sample-count accounting (NB 20 ms = 160; MB 20 ms =
  240; WB 20 ms = 320).
* **§4.2.9 supported output rates.** 8 / 12 / 16 / 24 / 48 kHz, the
  five rates "the reference implementation is able to resample to …
  within or near this delay constraint". Exposed as
  `SUPPORTED_OUTPUT_RATES_HZ` + `is_supported_output_rate`;
  `REFERENCE_RATE_HZ` (= 48 kHz) marks the rate Table 54 anchors
  against and the rate CELT operates at.
* **Per-frame output sample count.** `silk_frame_samples_at_output`
  returns the post-resampler sample count for one SILK frame at any
  output rate (e.g. 480 samples at 48 kHz for any bandwidth × 10 ms;
  960 for 20 ms). Sized so a caller can allocate the output buffer
  without knowing the resampler kernel.

Eighteen new unit tests (339 lib tests total, up from 321 at round-18
close): Table 54 transcription self-checks and the SWB/FB exclusion;
the strict NB < MB < WB monotonicity §4.2.9 explicitly motivates; the
Table 54 expansion to 48 kHz samples (NB = 26, MB = 33, WB = 34) plus
internal-rate samples and 24 kHz intermediate-rate samples; SWB / FB /
zero-rate rejections on the delay-samples helper; the five §4.2.9
supported output rates plus a sweep of unsupported rates (11.025 /
22.05 / 32 / 44.1 / 96 kHz); the SILK internal rate per bandwidth and
its membership in the §4.2.9 supported-output set; canonical
per-frame sample counts at internal + output rates plus rejection of
non-SILK durations (25 / 50 / 400 / 600 / 1234 tenths-ms); and a
cross-check that the Table 54 delay is strictly less than one 10 ms
SILK frame at every supported output rate × every SILK bandwidth.

## Planned clean-room sources

The clean-room rebuild will consult only:

* RFC 6716 — Definition of the Opus Audio Codec.
* RFC 8251 — Updates to the Opus Audio Codec.
* RFC 7587 — RTP Payload Format for the Opus Speech and Audio Codec.
* RFC 7845 — Ogg Encapsulation for the Opus Audio Codec.
* Black-box invocations of `opusdec` / `opusenc` (the binaries — not
  their source) as opaque validators.

No external library source — libopus, the Opus reference encoder /
decoder, etc. — is permitted as a reference under the workspace
clean-room policy.

## License

MIT. See `LICENSE`.
