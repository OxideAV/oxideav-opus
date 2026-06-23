# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT) for the
[oxideav](https://github.com/OxideAV) framework.

## Status

**Clean-room rebuild in progress (orphan scaffold).** The prior
implementation was retired under the workspace clean-room policy; the
crate is being re-implemented from scratch against the published RFCs
using only material under `docs/` and black-box validator binaries.

A top-level `OpusDecoder::decode_packet` packet → PCM orchestration is
now in place: it parses the §3.1 TOC, splits the §3.2 frame packing
(all four frame-count codes), runs the §4.5 multi-frame loop, routes
each Opus frame by mode, and lays out the interleaved 48 kHz output
buffer (RFC 7845 §5.1) with correct per-frame sample counts. Both **mono
and stereo SILK-only** packets now decode **end-to-end to real PCM**: the
§4.2 bitstream decode (the §4.2.3 header bits, the §4.2.5 LBRR / §4.2.6
regular SILK frame loop, each frame decoded in Table-5 order through
gains / LSF chain / LTP / excitation with inter-frame state threaded),
then the §4.2.7.9 LTP / LPC synthesis filters (composed in the
`silk_synthesis` module with the §4.2.7.9 per-subframe LPC selection and
cross-frame histories), then a §4.2.9 non-normative resample to 48 kHz
and i16 conversion. For **stereo**, the §4.2.2 mid/side interleave (mid
frame then side frame per 20 ms interval, the §4.2.7.2 mid-only flag
skipping the side frame) is decoded into two independent per-channel
synthesis states and converted from mid/side to left/right by the §4.2.8
`silk_stereo` unmixer, run **per SILK interval** with that interval's
§4.2.7.1 weights and the cross-packet `StereoUnmixState` history. The
§4.5.2 SILK state reset (CELT→SILK transition) and the §4.2.7.1
mono→stereo weight reset are applied across packets. The CELT **synthesis
backend** is now composed end-to-end: `celt_synthesis::CeltSynthState`
turns already-decoded per-band shapes + `log2` energies into time-domain
PCM through §4.3.6 denormalise → §4.3.7 inverse MDCT → §4.3.7 weighted
overlap-add → §4.3.7.2 de-emphasis, threading the cross-frame overlap and
de-emphasis state (the CELT analogue of `silk_synthesis`), and emits
interleaved 48 kHz i16 via `synthesize_frame_interleaved_i16`. A
**CELT-only silence frame now decodes end-to-end to real PCM**: the
§4.3.7.1 range-coded frame prefix (silence flag + post-filter group +
transient + intra) is decoded from the real range coder by
`celt_frame_prefix`, and a set silence flag drives the synthesis backend
with all-zero bands, emitting zero PCM while advancing the overlap-add /
de-emphasis state (`FrameDecodeStatus::CeltSilence`). The §4.3.2.1
coarse-energy *front half* now decodes for non-silent CELT-only frames:
the Laplace symbol decoder (`celt_laplace::ec_laplace_decode`, the
15-bit path) feeds the new `celt_coarse_energy` *reconstruction
recurrence*, which runs the §4.3.2.1 2-D prediction filter
`A(z_l, z_b)` in reverse (the frequency accumulator `pred_freq[b+1] =
pred_freq[b] + (1-beta)*R[b]` and `E[b][l] = alpha*E[b][l-1] +
pred_freq[b] + R[b]`, derived algebraically from the RFC z-transform),
adds back the §4.3 `e_means` Q4 baseline, and threads the cross-frame
`E[b][l-1]` predictor state on `OpusDecoder` (reset on an intra frame /
SILK→CELT transition). A non-silent CELT-only frame thus consumes its
prefix + coarse energy from the real range coder and advances the
synthesis state, reporting `FrameDecodeStatus::CeltCoarseEnergyDecoded`.
The remaining CELT band-data stages — §4.3.3 bit allocation (boost /
trim / reservations / interpolation), §4.3.4 PVQ band shapes, and
§4.3.2.2 fine energy — are still pending, so the band shapes are
all-zero and these frames emit correct-length silence until those land.
The one residual coarse-energy seam is the RFC's "clamped internally"
bound, which is not in the normative body (only in reference code) and
is left as a documented identity in `celt_coarse_energy` — exact for
every in-range bitstream — pending a clean-room docs trace. The §4.4
packet-loss concealment is also outstanding (the RFC defines PLC as a
non-normative decoder feature with no bitstream algorithm; lost / DTX
frames currently emit the §4.6 silence floor). The crate ships a large,
individually unit-tested set of SILK and CELT decode building blocks
(1200 lib tests + a CELT synthesis-backend integration suite). Per-stage
progress lives in `CHANGELOG.md`.

## What works

**Packet → PCM orchestration (RFC 6716 §3 / §4):**

- `OpusDecoder::decode_packet` — the top-level packet → interleaved
  48 kHz PCM path: TOC parse, §3.2 frame split, §4.5 multi-frame loop,
  per-mode routing, the §4.5.2 cross-packet SILK state reset, and the
  RFC 7845 §5.1 output sample-count layout. Mono SILK-only packets decode
  end-to-end to real PCM (bitstream → §4.2.7.9 synthesis → §4.2.9
  resample); other modes emit correct-length silence flagged via
  `FrameDecodeStatus`.
- `silk_decode::decode_silk_frame` — the §4.2.6 / §4.2.7 in-order SILK
  frame decode that composes the per-stage decoders in exact Table-5
  symbol order and runs the LSF → stable-Q12-LPC chain.
- `silk_synthesis::synthesize_silk_frame` — the §4.2.7.9 synthesis
  composition: §4.2.7.9.1 LTP + §4.2.7.9.2 LPC filters with the §4.2.7.9
  per-subframe LPC selection and cross-frame `SilkSynthState` histories,
  producing internal-rate (8/12/16 kHz) time-domain samples.
- `OpusDecoder::decode_silk_only_stereo` — the §4.2.2 stereo SILK decode:
  the §4.2.3 two-channel header bits, the §4.2.5 / §4.2.6 interleaved
  mid/side SILK frames (the §4.2.7.1 weights + §4.2.7.2 mid-only flag on
  the mid frame; an uncoded side frame clears its §4.2.7.9 LTP buffer per
  §4.5.2), two independent per-channel synthesis states, and the §4.2.8
  `silk_stereo::stereo_ms_to_lr` mid/side → left/right unmix run per SILK
  interval into interleaved L/R PCM.

**Packet & framing (RFC 6716 §3 / §4.2):**

- `OpusTocByte` — the §3.1 TOC parser (config × stereo flag × frame-count
  code).
- `OpusPacket` — the §3.2 frame-packing parser for all four frame-count
  codes (single, two-equal, two-unequal, signalled with optional VBR
  lengths + padding); returned frame slices borrow from the input.
- `parse_self_delimited` — RFC 6716 Appendix B self-delimiting framing
  (for chaining inside a multistream demuxer).
- `OpusFrameRouting` — §3.1 / §4.2 mode dispatch (SILK-only / Hybrid /
  CELT-only, SILK-frame count, per-frame LBRR-flag gating, channel
  multiplier).
- A §3.4 R1–R7 malformed-input rejection audit
  (`tests/malformed_input.rs`).
- An **end-to-end SILK fixture-decode suite** (`tests/silk_fixture_decode.rs`)
  that decodes the in-project NB-mono / WB-stereo / MB-60 ms-mono Opus
  streams packet-by-packet through `decode_packet` and validates §3.1 TOC
  routing, whole-stream error-free SILK decode (mono + stereo, NB/MB/WB,
  20/60 ms), §3 sample-count accounting, and 440 Hz dominance on the NB
  sine fixture. Validation is signal- / structure-based, not bit-exact:
  the §4.2.9 SILK→48 kHz resampler is non-normative, so the decoded
  envelope differs from the polyphase-resampled `opusdec` reference.

**Range coder (RFC 6716 §4.1):** `RangeDecoder` — the shared entropy
primitive consumed by both layers, including the §4.1.2 two-step
`ec_decode` / `ec_dec_update` path and the Laplace / iCDF helpers.

**SILK (RFC 6716 §4.2):** frame-header decode (§4.2.7.1–§4.2.7.5.1),
subframe gains (§4.2.7.4), the full LSF chain (stage-2 residual → NLSF
reconstruction → stabilization → interpolation → NLSF→LPC →
bandwidth-expansion → prediction-gain limiting, §4.2.7.5.2–§4.2.7.5.8),
LTP parameters (§4.2.7.6), LCG seed (§4.2.7.7), excitation
(§4.2.7.8), LTP + LPC synthesis filters (§4.2.7.9), stereo unmixing
(§4.2.8), and the §4.2.9 resampler delay budget.

**CELT (RFC 6716 §4.3 / §4.5):** the §4.3 band layout (Table 55), the
pre-band header symbols (silence / post-filter / transient / intra),
the §4.3.4.5 *time-frequency change decode* (`celt_tf_decode` — the
per-band `tf_change` flag loop, first band absolute and subsequent
bands difference-coded relative to the previous band's choice, plus the
§4.3.1-gated `tf_select` flag and the resulting per-band TF adjustment
vector) layered on the §4.3.4.5 TF-resolution adjustment tables, the
coarse-energy Laplace
parameter tables (§4.3.2.1), the allocation parameter surfaces
(log2-frac / alloc-trim / cache-caps / static-allocation), the
§4.3.4.1 *Bits-to-Pulses* pulse-cost cache (the run-packed
`cache_bits50` / `cache_index50` lookup plus the budget-to-pulse-count
inversion), the §4.3.6 band denormalisation (unit-norm PVQ shape ×
`sqrt(2**log2_energy)`, laid out across the coded bands into the
inverse-MDCT input buffer), the §4.3.7 inverse MDCT transform core (the
`N` frequency-domain bins → `2N` time-domain samples mapping, scaled by
`1/2`, with the §4.3.7 overlap-add window already landed at
`celt_mdct_window`), the §4.3.7 *weighted overlap-add* (`celt_overlap_add`
— the stateful per-channel adder that windows each `2N` inverse-MDCT
block with the low-overlap synthesis window and overlap-adds the leading
half with the previous block's windowed trailing half at hop `N`,
carrying the overlap history across frames and reconstructing the
aliasing-free time-domain signal), and the §4.5 redundancy /
mode-transition state-reset machinery. The allocation orchestration and
the PVQ shape decode are partially landed; the §4.3.5 anti-collapse and
wiring the now-complete time-domain chain (denormalise → inverse MDCT →
weighted overlap-add → post-filter → de-emphasis) into a full CELT
decode loop are the remaining decode milestones.

## Clean-room sources

The rebuild consults only:

- RFC 6716 — Definition of the Opus Audio Codec.
- RFC 8251 — Updates to the Opus Audio Codec.
- RFC 7587 — RTP Payload Format for Opus.
- RFC 7845 — Ogg Encapsulation for Opus.
- Black-box invocations of the `opusdec` / `opusenc` binaries (not
  their source) as opaque validators.

No external library source is permitted as a reference under the
workspace clean-room policy.

## License

MIT. See `LICENSE`.
