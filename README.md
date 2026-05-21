# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT).

## Status — 2026-05-22 (clean-room round 5)

**Packet header + §3.2 frame-packing parser + §4.1 range decoder +
SILK §4.2.7.1–§4.2.7.5.1 frame-header decoder + §4.2.7.4 subframe
gains; no LSF stage-2 / LTP / excitation yet, no CELT band machinery
yet.**

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

Total crate test count: 88 (5 TOC + 27 frame-packing + 19 range
decoder + 17 SILK header + 20 subframe gains).

Actual LSF stage-2 / LTP / excitation decoding, the full CELT band
machinery, and the §5 encoder pipeline remain out of scope; the
higher-level encode / decode entry points still return
`Error::NotImplemented`.

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
