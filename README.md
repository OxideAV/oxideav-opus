# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT).

## Status — 2026-05-24 (clean-room round 11)

**Packet header + §3.2 frame-packing parser + §4.1 range decoder +
SILK §4.2.7.1–§4.2.7.5.1 frame-header decoder + §4.2.7.4 subframe
gains + §4.2.7.5.2 LSF Stage-2 residual + §4.2.7.5.3 NLSF
reconstruction + §4.2.7.5.4 NLSF stabilization + §4.2.7.5.5 NLSF
interpolation + §4.2.7.5.6 NLSF→LPC core conversion (`silk_NLSF2A`) +
§4.2.7.5.7 LPC range-limiting bandwidth expansion; no §4.2.7.5.8
prediction-gain stability check yet, no LTP / excitation, no CELT band
machinery yet.**

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

Total crate test count: 201 (5 TOC + 27 frame-packing + 19 range
decoder + 17 SILK header + 20 subframe gains + 30 LSF stage-2 +
26 LSF reconstruction + 19 LSF stabilization + 10 LSF interpolation
+ 22 LSF → LPC core + 6 LPC range-limiting).

Round 11 stops after the §4.2.7.5.7 range-limiting; the §4.2.7.5.8
prediction-gain stability check (Levinson reflection-coefficient
recurrence + chirp loop) is deferred to a later round. LTP / excitation
decoding, the full CELT band machinery, and the §5 encoder pipeline
remain out of scope; the higher-level encode / decode entry points still
return `Error::NotImplemented`.

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
