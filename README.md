# oxideav-opus

[![CI](https://github.com/OxideAV/oxideav-opus/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-opus/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-opus.svg)](https://crates.io/crates/oxideav-opus) [![docs.rs](https://docs.rs/oxideav-opus/badge.svg)](https://docs.rs/oxideav-opus) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

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
On top of the coarse energy, a non-silent frame now also decodes the
full run of Table-56 symbols between coarse energy and the §4.3.4
residual, from the same range coder in exact Table-56 order: the §4.3.1
per-band `tf_change` flags and gated `tf_select` bit (`celt_tf_decode`),
the §4.3.4.3 `spread` symbol (`celt_spreading`), then the §4.3.3
allocation *header* — the signalled part of the bit allocation: the band
boosts
(`celt_band_boost`, walked over the `start..end` coding window with the
per-band `cap[]` from `celt_cache_caps50` and the per-channel MDCT-bin
counts), the allocation trim (`celt_alloc_trim`, gated on the running
`ec_tell_frac`), and the §4.3.3 anti-collapse / skip / intensity-stereo
/ dual-stereo reservations (`celt_reservations`). This advances the
entropy decoder through everything the bitstream explicitly carries
before the implicit interpolation, leaving the coder positioned exactly
where the §4.3.4 PVQ shape decode resumes, and the frame reports
`FrameDecodeStatus::CeltAllocationDecoded`. The remaining CELT band-data
stages — the §4.3.3 *implicit* allocation (`interp_bits2pulses`, the
per-band pulse / fine-energy split, which is reference-code-only and
absent from the RFC narrative body), §4.3.4 PVQ band shapes, and
§4.3.2.2 fine energy — are still pending, so the band shapes are
all-zero and these frames emit correct-length silence until those land.
The one residual coarse-energy seam is the RFC's "clamped internally"
bound, which is not in the normative body (only in reference code) and
is left as a documented identity in `celt_coarse_energy` — exact for
every in-range bitstream — pending a clean-room docs trace. The §4.4
packet-loss concealment is also outstanding (the RFC defines PLC as a
non-normative decoder feature with no bitstream algorithm; lost / DTX
frames currently emit the §4.6 silence floor).

The crate now also carries the start of the **encode side**: the
bit-exact §5.1 range *encoder* (`RangeEncoder` — the §5.1.1 symbol
update, §5.1.1.2 carry propagation, the §5.1.2 division-free variants
sharing the decoder's `icdf[]` tables, §5.1.3 raw bits, §5.1.4
uniform integers, §5.1.5 finalization, §5.1.6 `tell`/`tell_frac`
matching the decoder bit-for-bit), write-side mirrors of **every**
SILK §4.2.7 decode stage (header / gains with a deterministic
quantizer / LSF stage-1 + stage-2 / interpolation index / LTP / seed
/ excitation, each returning the value the decoder will
reconstruct), the whole-frame Table-5 composition
(`encode_silk_frame`), and SILK-only **packet encoders for both mono
and stereo** (`encode_silk_only_packet_mono` /
`encode_silk_only_packet_stereo`: TOC byte + §4.2.3/§4.2.4 header
bits + 1–3 SILK frames at 10/20/40/60 ms — the stereo entry writing
the §4.2.2 mid/side interleave with the §4.2.7.1 weight quintuple and
gated §4.2.7.2 mid-only flag on each mid frame, and two independent
per-channel carried states) whose packets decode end-to-end through a
fresh `OpusDecoder::decode_packet` to real SILK PCM, with every
per-frame parameter verified equal to the encoder's prediction. LBRR
(in-band FEC, §4.2.5) emission is included for both channel layouts
and closes the FEC loop: `decode_packet_fec` recovers real (mono or
two-channel) audio from the encoder's own redundancy. On top of the
packet writers sit the **stereo analysis front half** — the exact
§4.2.8 algebraic-inverse downmix `stereo_lr_to_ms` (L/R → mid/side
with the decoder's weight-interpolation ramp; roundtrips to the
input at the §4.2.8 one-sample delay), the least-squares §4.2.7.1
weight estimator `estimate_stereo_weights`, and the exhaustive
codebook quantizer `StereoWeightSymbols::quantize` — plus the **§3.2
/ Appendix-B framing writers** (`compose_packet`,
`compose_packet_code3`, `compose_self_delimited`; all four codes,
CBR/VBR, §3.2.5 padding chains, parser-validated R2/R3/R5/R6) and
the **RFC 7845 write side** (`OpusHead::compose`, byte-identical on
reparse, and `assemble_multistream_packet`, roundtripped against the
splitter and decoded sample-identically through
`MultistreamDecoder`). On top of it all now sits the **§5.2.3 SILK
signal analysis** — `encode(pcm)` is real: `SilkEncoderMono` /
`SilkEncoderStereo` derive every Table-5 symbol from internal-rate
PCM across the full SILK packet matrix — 10 / 20 / 40 / 60 ms
packets (one 2-subframe frame, or one to three 20 ms frames with the
intra-packet delta-gain / §4.2.7.6.1 relative-lag / §4.2.7.6.3
scaling-presence threading of the decoder's regular walk), per-frame
§4.2.3 VAD flags derived from the signal (silent intervals code
frame type 0 and skip the pitch search), §4.2.5 **LBRR in-band FEC**
from PCM (`set_fec(true)`: each packet re-encodes the previous
packet's active intervals at a reduced rate from a pre-packet
analyzer snapshot with a fresh closed-loop state, recovered
end-to-end through `decode_packet_fec`), and §3.2.5 **CBR transport
shaping** (`encode_packet_cbr` / `pad_packet_to`: exact-byte-size
code-3 re-framing, every target size reachable, decode-identical).
The chain is Burg's-method LPC
(§5.2.3.4.2.1) → analysis-direction LPC→NLSF conversion (deflated
line-spectral root search, verified as the exact inverse of the
§4.2.7.5.6 fixed-point reconstruction) → exhaustive stage-1
analysis-by-synthesis NLSF quantisation scored on the real decode
chain → whitened-domain §5.2.3.2 pitch analysis with joint
(primary-lag × Table 33-36 contour) quantisation → §5.2.3.6
exact-distortion LTP codebook search → per-subframe residual-energy
gain selection through the §4.2.7.4 quantizer (cross-packet
clamp-safe) → a closed-loop excitation quantiser (the §5.2.3.8 role)
that rounds each pulse against the prediction the decoder will
actually form, LCG sign inversion included, and updates the carried
state through the real §4.2.7.9 synthesis chain. Sine, pulse-train
(voiced), and amplitude-panned stereo inputs all decode back through
the real streaming `OpusDecoder` at >10 dB tone-projection SNR on
the 48 kHz output, with stereo panning preserved.

Differential encoder/decoder testing and a restored cargo-fuzz suite
(4 coverage-guided targets, incl. an encoder↔decoder range-coder
roundtrip) have also hardened the decoder: five mis-transcribed rows
in the §4.2.7.8.3 split tables (now verified cell-by-cell against the
RFC across all 64 rows), a `dec_bits(32)` shift overflow, a
§4.2.7.5.8 recurrence i64 overflow on adversarial input, and the
§4.2.7.8 10 ms-MB 128-vs-120-sample special case (previously every
10 ms MB SILK packet failed to synthesize) are all fixed with
regression tests. The round-388 encoder work exposed one more
long-standing decode bug: the §4.2.7.5.6 P/Q recurrence dropped the
"p_Q16[k][k+2] = p_Q16[k][k]" symmetric-mirror boundary condition at
the j = k+1 read (substituting 0), producing badly wrong LPC filters
that burned up to 12 prediction-gain-limiter rounds on perfectly
stable codebook vectors — now fixed and pinned by an analytic
closed-form regression over all 64 NB/WB stage-1 codebook entries.
Round 391 closed two more reconstruction-level streaming gaps: the
§4.2.7.4 gain-clamp base (`previous_log_gain`) and the §4.2.7.5.5
NLSF interpolation base `n0` now carry ACROSS Opus frames in the
streaming `OpusDecoder` (both were previously re-armed per packet,
so the first frame of every packet skipped the independent-gain
clamp and ignored its coded `w_Q2`), cleared exactly on the RFC's
reset events (§4.5.2 SILK reset, bandwidth change, uncoded side
frame) and seeded from the LBRR reconstruction after FEC recovery
under the §4.2.7.4 packet-loss latitude.

The crate ships a large, individually unit-tested set of SILK and
CELT building blocks plus a complete RFC 7845 multistream /
multichannel decode subsystem (1357 lib tests + SILK-fixture,
multistream, FEC, and CELT synthesis-backend integration suites).
Per-stage progress lives in `CHANGELOG.md`.

## What works

**Packet → PCM orchestration (RFC 6716 §3 / §4):**

- `OpusDecoder::decode_packet` — the top-level packet → interleaved
  48 kHz PCM path: TOC parse, §3.2 frame split, §4.5 multi-frame loop,
  per-mode routing, the §4.5.2 cross-packet SILK state reset, the
  cross-packet §4.2.7.4 / §4.2.7.5.5 reconstruction carry, and the
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
  envelope differs from the polyphase-resampled reference decoder.

**Multistream / multichannel (RFC 7845 §3 / §5.1 / §5.1.1):**

- `OpusHead` — the §5.1 identification-header parser: version (with the
  major-nibble compatibility bound), output channel count, pre-skip,
  input sample rate, output gain, mapping family, and the §5.1.1
  channel-mapping table (stream count N, coupled count M, per-output
  mapping indices). Enforces every MUST in §5.1 / §5.1.1 (non-zero
  channel/stream counts, per-family channel ranges, `M ≤ N`,
  `M + N ≤ 255`, and the `< M+N` / 255 mapping-index bound). Family 0
  synthesizes the table from the RFC-pinned defaults.
- `split_multistream_packet` — the §3 N-packet split: the first `N − 1`
  streams via Appendix-B self-delimited framing, the final stream as the
  undelimited remainder.
- `MultistreamDecoder` — the multichannel decode: one stateful
  sub-decoder per coded stream, decoding each split packet and
  assembling the `C` output channels by the §5.1.1 index rule
  (coupled-stream L/R by parity, mono streams, index-255 silence, a
  decoded channel routed to multiple outputs), with the §3 equal-duration
  constraint enforced. Validated end-to-end against the real SILK
  fixtures: an `N = 1` family-0 decode is byte-identical to a plain
  `OpusDecoder`, a coupled-stream L/R split reproduces a plain stereo
  decode exactly, and mono-pair / swapped / silence / duplicate maps all
  route correctly.
- `apply_output_gain` / `PreSkip` — the §5.1 post-decode output-gain
  application (Q7.8 dB, i16-saturating) and the cross-packet pre-skip
  accumulator.

**Range coder (RFC 6716 §4.1 / §5.1):** `RangeDecoder` — the shared
entropy primitive consumed by both layers, including the §4.1.2
two-step `ec_decode` / `ec_dec_update` path and the Laplace / iCDF
helpers — and `RangeEncoder`, its bit-exact §5.1 write-side mirror
(validated by per-primitive roundtrips, `tell`/`tell_frac` lockstep,
a 5000-seed mixed-symbol fuzz roundtrip, and a coverage-guided
libfuzzer differential target).

**SILK encode side (RFC 6716 §5.2 bitstream back end):** write-side
mirrors of every §4.2.7 stage sharing the decode tables
(`SilkFrameHeader::encode_pre_gains` / `encode_lsf_stage1`,
`SubframeGains::encode`/`quantize`, `LsfStage2::encode`,
`LsfInterpolated::encode_index`, `encode_lcg_seed`,
`LtpParameters::encode`, `Excitation::encode`), the Table-5
whole-frame composition `encode_silk_frame`, the §4.2.3/§4.2.4
header-bit writer `SilkHeaderBits::encode` (mono + two-channel), the
§3.1 TOC composer `OpusTocByte::compose_byte`, and the packet-level
`encode_silk_only_packet_mono` / `encode_silk_only_packet_stereo`
(each with a `_with_lbrr` variant for §4.2.5 in-band-FEC emission;
the stereo entry writes the §4.2.2 mid/side interleave with the
§4.2.7.1 weights and gated §4.2.7.2 mid-only flag per interval and
threads two independent per-channel carried states, exactly
mirroring the decoder's stereo walk) — every layer
roundtrip-verified against the decoder, up to whole packets decoding
end-to-end through `OpusDecoder::decode_packet` (mono and stereo)
and FEC recovery through `decode_packet_fec`.

**Stereo encode analysis (§4.2.7.1 / §4.2.8 write half):**
`stereo_lr_to_ms` — the exact algebraic inverse of the §4.2.8
unmixer (frame-aligned L/R → mid/side with the decoder's
weight-interpolation ramp, one-sample lookahead for the final `p0`,
`StereoDownmixState` history; a multi-frame roundtrip through
`stereo_ms_to_lr` reproduces the input at the §4.2.8 one-sample
delay) — `estimate_stereo_weights` (least-squares fit of the raw
side onto the `p0` / mid predictor pair, f64 normal equations) and
`StereoWeightSymbols::quantize` (exhaustive deterministic argmin
over the 5625-quintuple §4.2.7.1 codebook; representable targets
roundtrip value-exactly).

**Packet-framing / RFC 7845 write side:** `compose_packet` /
`compose_packet_code3` / `compose_self_delimited` / `encode_length` —
the §3.2 + Appendix-B framing writers (all four codes, CBR/VBR,
§3.2.5 padding chains, every parser-enforced requirement validated
before writing; roundtripped against `OpusPacket::parse` /
`parse_self_delimited`, including chained self-delimited buffers and
multi-frame SILK packets decoding end-to-end) — plus
`OpusHead::compose` (byte-identical reparse, full §5.1/§5.1.1 MUST
validation) and `assemble_multistream_packet` (§3 stream packing via
the Appendix-B writer, equal-duration constraint enforced,
sample-identical decode through `MultistreamDecoder`).

**SILK (RFC 6716 §4.2):** frame-header decode (§4.2.7.1–§4.2.7.5.1),
subframe gains (§4.2.7.4), the full LSF chain (stage-2 residual → NLSF
reconstruction → stabilization → interpolation → NLSF→LPC →
bandwidth-expansion → prediction-gain limiting, §4.2.7.5.2–§4.2.7.5.8),
LTP parameters (§4.2.7.6), LCG seed (§4.2.7.7), excitation
(§4.2.7.8), LTP + LPC synthesis filters (§4.2.7.9), stereo unmixing
(§4.2.8), the §4.2.9 resampler delay budget, and **in-band FEC
recovery** (§2.1.7 / §4.2.5): `OpusDecoder::decode_packet_fec`
reconstructs a lost frame's audio from the Low Bit-Rate Redundancy
(LBRR) frames carried in the next received packet — decoding the §4.2.5
LBRR frame(s) (mono, or interleaved mid/side for stereo), running the
full §4.2.7.9 synthesis from a fresh state, unmixing a stereo recovery
via §4.2.8, and resampling to 48 kHz, reported through `FecDecodeStatus`.

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
aliasing-free time-domain signal), the §4.3.4.5 *time-frequency Hadamard
transform* (`celt_tf_hadamard` — the across-block / sequency-order
orthonormal Walsh–Hadamard reshaping that consumes the per-band
`TfDirection`, preserving the unit-norm shape energy), the §4.3.4
*per-band shape decode orchestrator* (`celt_band_shape` — composing
§4.3.4.2 PVQ decode → §4.3.4.3 spreading → §4.3.4.5 TF transform into
one `decode_band_shape` call given a band's `(N, K, spread, tf_adjust,
nb_blocks)`), and the §4.5 redundancy / mode-transition state-reset
machinery.

The one structural blocker on the CELT-only real-PCM path is the §4.3.3
**allocation orchestration** (the reference `interp_bits2pulses`:
reallocation of unused bits with concurrent skip decoding, the
fine-energy-vs-shape split, and the final reallocation) together with
the §4.3.4.4 **split-decoding gain precision** ("derived from the
current allocation"). RFC 6716 §4.3.3 *names* these steps (p. 111) but
provides no algorithm for them — they live only in the reference
`rate.c` / `bands.c`, which the clean-room wall bars. Without the
per-band pulse count `K` they produce, the §4.3.4 shape decode (now
fully composed) cannot be driven against a real bitstream, so non-silent
CELT-only frames still emit correct-length silence after consuming their
prefix + coarse energy. This is a precise docs gap: a clean-room trace
of `interp_bits2pulses` + the split-gain `qb` derivation would unblock
the end-to-end CELT path. The §4.3.5 anti-collapse remains separately
gapped (no PRNG / energy-injection algorithm in the RFC narrative).

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
