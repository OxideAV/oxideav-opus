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
then the §4.2.7.9 LTP / LPC synthesis filters in the **exact
fixed-point arithmetic of the RFC 6716 §A embedded reference listing**
(`silk_decode_core` — Q14 excitation, Q13/Q15 LTP with output-history
re-whitening and gain-change state rescaling, Q14 LPC, i16 output; the
per-subframe LPC selection and all cross-frame histories included),
then the §4.2.8 mono one-sample delay and the §4.2.9 resample to
48 kHz (`SilkUpsampler` — the reference decoder's fixed-point
resampler: per-rate delay compensation, 2× allpass upsampling,
fractional-phase 8-tap FIR interpolation, with the RFC 8251 §5
correction). For **stereo**, the §4.2.2 mid/side interleave (mid
frame then side frame per 20 ms interval, the §4.2.7.2 mid-only flag
skipping the side frame) is decoded into two independent per-channel
synthesis states and converted from mid/side to left/right by the
integer §4.2.8 unmixer (`stereo_ms_to_lr_i16`), run **per SILK
interval** with that interval's §4.2.7.1 weights and the cross-packet
unmix history. The
§4.5.2 SILK state reset (CELT→SILK transition) and the §4.2.7.1
mono→stereo weight reset are applied across packets. **SILK decode is
bit-exact against the reference listing's decoder** (RFC 8251
corrections applied): every pure-SILK fixture and a 100+-stream
oracle corpus (NB/MB/WB × 10/20/40/60 ms × mono + mid/side stereo ×
6–40 kb/s, transient-heavy content) reproduces the reference decode
sample-for-sample at 48 kHz; the gates in
`tests/silk_reference_waveform.rs` sit at a 100 dB floor.
**CELT-only packets now decode
end-to-end to real PCM** (`FrameDecodeStatus::CeltDecoded`): the whole
§4.3 Table-56 entropy layer runs with the normative per-symbol budget
gates (`celt_frame_decode` — silence with the exhausted-budget rule,
the §4.3.7.1 post-filter parameters, transient + intra, §4.3.2.1
coarse energy with its three low-budget fallbacks, §4.3.1 TF flags,
spread, the shrinking-budget dynalloc boosts, trim, the §4.3.3
*implicit* allocation ported in exact 1/8-bit integer arithmetic
(`celt_rate_alloc` — quality-row search, 6-step interpolation
bisection, backward skip decode, intensity / dual-stereo, fine-energy
split), §4.3.2.2 fine energy, the §4.3.4 recursive band decode
(`celt_band_decode` — PVQ leaves with the exact two-stride spreading
rotation, split angles on the triangular / uniform / step PDFs with
bit-exact mid/side weighting, stereo merge + intensity + dual-stereo,
Haar/Hadamard time-frequency reorganization, spectral folding with the
RFC 8251 §9 update, collapse masks), the §4.3.5 anti-collapse, and the
§4.3.2.3 final fine bits), then the signal half (`celt_mdct_synthesis`
— denormalisation in the log2-amplitude energy domain with the
RFC 8251 §8 cap, unit-scale inverse MDCT for long and short blocks
under the low-overlap window, overlap-add, the recursive §4.3.7.1 comb
filter with crossfaded parameter transitions, §4.3.7.2 de-emphasis),
with all cross-frame state carried and the §4.5.2 resets applied.
Validated against the reference decodes of the fixture corpus:
`celt-fb-stereo-128kbps` (20 ms FB stereo) and `celt-2.5ms-low-latency`
reconstruct at **~88–108 dB SNR** — i16-quantization-level waveform
agreement — and a 60+-stream black-box low-bitrate corpus (6–48 kb/s,
2.5–20 ms, mono/stereo, transient-heavy content) decodes at the
float-arithmetic noise floor against the reference listing's decoder
(~80–111 dB; every packet ≥ 55 dB — the formerly-reported transient
seam does not reproduce against a reference-lineage decode). **Hybrid packets decode end-to-end**
(`FrameDecodeStatus::HybridDecoded`): the SILK layer (WB internal) and
the CELT layer (bands 17–21) share one range coder with the §4.5.1
redundancy side information decoded between them (the main coder's
buffer reduced per §4.5.1.3 so its raw bits read from the reduced
end), and the 48 kHz outputs sum per §4.4 — the bit-exact SILK band
lands on the reference timeline through the reference §4.2.9
resampler, and `hybrid-fb-mono-28kbps` decodes at **~71 dB**
whole-stream against the reference-listing decode (float-noise floor;
gated at 60 dB), with hybrid SWB oracle streams at ~93–98 dB. The **§4.5
transition machinery is in place**: the 5 ms redundant CELT frame is
decoded like a CELT-only frame (own coder, no TOC, carrier channels /
bandwidth with the MB→WB override) through the stream's single CELT
state whose geometry adapts without dropping state, the §4.5.2 resets
land where Figure 18 puts them (an end-position redundant frame takes
the reset and warms the following CELT frames; a beginning-position
one continues the previous state ahead of the deferred main-layer
reset), and the §4.5.1.4 output stitching (first-2.5 ms-as-is +
power-complementary cross-lap) is applied on both placements — the
`mode-switching` fixture decodes at ~103 dB whole-stream against the
reference-listing decode (hybrid segment, transition window and
CELT-only segment all at the float-noise floor). **Packet-loss concealment
(§4.4)** is implemented per the RFC's per-mode guidance
(`OpusDecoder::conceal_loss`): LPC extrapolation (Burg fit +
pitch-cyclic residual) after SILK-bearing frames, pitch-periodic
waveform repetition after CELT-only frames, an energy-decay envelope
across consecutive losses down to the silence floor, and a 2.5 ms
extrapolation tail cross-lapped into the first packet decoded after
the loss run; in-band FEC (`decode_packet_fec`) remains the preferred
recovery when the next packet is available.

Round 448 lands the last untouched §2.1 subsystem: **§2.1.9
discontinuous transmission**, on both sides of the wire. Decoder
side, a §3.2.1 zero-length frame no longer snaps to digital silence:
RFC 7845 §4.1 pins the intent ("explicitly request the use of Packet
Loss Concealment"), so `decode_packet` routes every zero-length frame
through the §4.4 hold — per-mode extrapolation with the concealment
energy decay — with the §4.4 bookkeeping moved per-frame so
zero-length frames combined into code-2/3 packets hold their exact
place in the concealment timeline. Encoder side, `set_dtx` lands on
**every arm** (`SilkEncoderMono/Stereo`, `HybridEncoderMono/Stereo`,
`CeltVbrEncoder`, and the four SILK/Hybrid VBR arms): a fully
inactive packet (the §4.2.3 activity floor; digital silence on the
CELT arm) is — after a 2-packet transmitted hangover that carries the
last active packet's §4.2.5 LBRR — replaced by the **1-byte TOC-only
marker** (one §3.2.1 zero-length frame), with one real packet coded
per 400 ms of suppression ("only one frame is encoded every
400 milliseconds", §2.1.9). While suppressing, the encoder freezes
every decoder-authoritative mirror (the decoder decodes nothing for a
zero-length frame, so both sides freeze identically), and the first
coded packet after a run carries no LTP (SILK) and intra energies
(CELT/Hybrid), so its reconstruction never depends on what a
decoder's own non-normative concealment left behind. Measured: 141 of
150 silent-run packets suppressed (silent-run bytes at 16% of the
DTX-off run); the §A reference-listing decoder accepts every DTX
stream with exact packet/sample counts — SILK bit-exact until the
first suppression, Hybrid at 118 dB pre-run and 50 dB from resume,
CELT at 105 dB pre-run and **102 dB (max 1 LSB) from the intra
resume** — and the listing encoder's own DTX stream (273 markers/401
packets, now a shipped fixture) decodes through our decoder bit-exact
before the first suppression and to 51 dB / max 60 one refresh period
after resume (`tests/dtx_reference_stream.rs`). RFC 7845 §4.1 **gap
repair** completes the packet-layer story: `compose_plc_gap_packets`
synthesizes the zero-length-frame packets that fill a capture gap
(§4.1's exact recommendations: configuration held, size changes
delayed, CELT switch only at the end, MB→WB, cheapest packings; the
RFC's 95 ms worked example is pinned byte-for-byte in
`tests/gap_repair.rs`).

Round 450 closes the decode tail and the framework-registry gap.
**Decoding at any §4.2.9 supported output rate** is real
(`OpusDecoder::with_output_rate`, 8 / 12 / 16 / 24 / 48 kHz): the
SILK layer resamples internal → output directly through the full
decoder-side reference-resampler matrix — pass-through, pure 2×,
allpass + fractional FIR, and the AR2 + decimating-FIR chains (3:4,
2:3, 1:2), every one of the 15 rate pairs pinned **bit-exact**
against the §A reference listing's resampler — while the CELT layer
keeps its 48 kHz MDCT grid, zeroes the spectrum above the output
Nyquist before the inverse MDCT, and decimates the de-emphasized
signal at phase 0 (the listing's reduced-rate construction), with
the §4.4 Hybrid sum, §4.5.1.4 cross-laps, FEC, DTX holds, PLC
(duration-rescaled), and the multistream assembly all on the
output-rate timeline. Whole-stream gates against the reference
decoder's own reduced-rate decodes: SILK fixtures **bit-exact** at
8 and 24 kHz, CELT at 88.6–104.0 dB, Hybrid at 70.7–72.9 dB,
mode-switching at 102.6–102.8 dB; a 270-decode corpus sweep
(NB/MB/WB/hybrid/CELT × 10–60 ms × mono/stereo × tone/click/noise ×
all 5 rates) measured **150/150 SILK decodes bit-exact** and
everything else at the float floor (max 1 LSB), zero failures. The
**§4.5 switch seams without redundancy** landed too: Hybrid→SILK now
performs the normative overlap-buffer flush (2.5 ms CELT silence
frame, Figure 18 `c`/`+`) — seam 31.9 dB → **bit-exact** — SILK
bandwidth switches carry the §4.2.8 delay sample (NB↔WB captures now
**bit-exact** whole-stream), and non-normative CELT↔SILK/Hybrid
switches apply the RECOMMENDED 5 ms PLC fill with the
power-complementary crossfade (whole-stream 34–40 dB vs ≈27 dB for a
hard switch, off-seam ≥ 97 dB). Loss re-convergence is gated against
reference decodes of planted-loss captures at 48 kHz AND 16 kHz, and
the decode fuzz target cycles output rates.

### Encoder

The **encode side is complete across every RFC 6716 mode** and
fronted by one streaming encoder. `OpusEncoder` (`opus_encoder`)
takes 48 kHz interleaved S16 and emits one packet per frame, with
every §2.1 control parameter live: bitrate (6–510 kb/s), operating
mode (auto / SILK / Hybrid / CELT), audio bandwidth (auto / NB / MB /
WB / SWB / FB), packet duration (2.5–60 ms; 40 / 60 ms packets are
native SILK frames or §3.2 code-3 packets of 20 ms CELT / Hybrid
frames), VBR / constrained VBR /
hard CBR (§3.2.5 exact-size padding), DTX, in-band FEC with the
§2.1.7 packet-loss knob, the 0–10 complexity ladder, tapset election,
and the §4.5.1 transition redundancy switch. In `auto`, the mode and
bandwidth follow the bitrate per application profile (`Voip` /
`Audio` / `RestrictedLowDelay`; §2.1.1's sweet spots) **and, with the
signal-adaptive election (`set_signal_adaptive`, registry
`signal-adaptive`, default on), the signal itself**: the crate's own
`SignalAnalyser` (§5 "type of signal (speech vs. music)", designed
from the RFC's mode semantics — tonality, spectral flux, harmonicity
and pitch stability, voiced/unvoiced alternation, transient density,
syllabic envelope modulation, stereo width and a held content-bandwidth
estimate on a 10 ms block grid, a fixed logistic, smoothing and a
two-threshold hysteresis with a 0.4 s dwell) classes each frame as
speech or music; speech keeps the §2.1.1 ladder, music takes the MDCT
layer from 12 kb/s up (no Hybrid rung: measured, Hybrid never beats
CELT-only on music), the content bandwidth caps the coded bandwidth
(raises immediate, everything else rate-limited to one signal-driven
change per 1.5 s), and a
configuration change is carried out as a **§4.5.3 Figure 18
normative transition**: a 5 ms redundant CELT frame in the last
old-configuration packet (SILK→CELT, Hybrid→CELT, NB/MB SILK→Hybrid),
in the first new-configuration packet (CELT→SILK / Hybrid), in both
(SILK bandwidth change, Hybrid→NB/MB SILK), or in neither (the WB
SILK↔Hybrid pairs, normative bare). One CELT state threads the whole
stream across the arms with the §4.5.2 reset placement mirrored from
the decoder's own rule table (an end-position frame reset-then-warms
the state the following CELT / Hybrid frames continue; a
beginning-position frame rides the carried chain ahead of the
deferred `|H` main-layer reset), the SILK analyzer carries across WB
SILK↔Hybrid where the decoder carries its SILK state, and every arm
sits on the same 120-sample stream timeline (measured end-to-end lag
120 ± 2 for NB/MB/WB × mono/stereo SILK-only, Hybrid, and CELT).
Changes land one packet after the knob moves — the packet coded when
a change is first seen is the transition carrier — and the redundant
bytes (~5 ms of the richer seam side's rate) ride on top of the
election, outside the VBR drift ledger.

Measured on the crate's corpus (`tests/signal_adaptive_election.rs`:
synthetic speech, four-voice music mono/stereo, speech over a music
bed, tones, silence, an optional real speech sample; own decode; bark
log-spectral distance, lower is better) at equal target rate, the
adaptive election against the bitrate-only ladder: music 12 kb/s
11.4 → 9.0 dB, 16 kb/s 9.6 → 8.9, 24 kb/s 7.6 → 5.7 (steady state
6.9 → 4.4); stereo music 24 kb/s 10.3 → 7.8, 36 kb/s 8.3 → 5.8; tones
16 kb/s 15.6 → 9.8 (steady 6.7), 24 kb/s 14.9 → 9.0 (steady 5.7);
speech over music 24 kb/s 5.7 → 5.3; speech streams are identical to
the ladder (zero switches); steady clips switch exactly once, at the
class decision (~0.8–1.5 s in); alternating 3 s speech/music segments
track every boundary. Black-box `opusdec` and `ffmpeg` decode every adaptive
capture (mono, stereo, alternating, silence) without diagnostics,
`opusdec` agreeing with our decode at 52–62 dB. The same batteries
found and fixed three rate-control defects on the way: the SILK rate
knob's ~4× size cliff at the default pulse target (no operating point
between ~10 and ~70 kb/s), the Hybrid arms coding their SILK layer at
a fixed ~190 B (every Hybrid target below ~80 kb/s emitted 70–80 kb/s;
now elected to a 0.8 payload share, the measured optimum), and the
missing §5.2.3.3 compensation gain on the noise-shaping quantizer
(+3.5–6 dB SNR at every SILK packet size). By these waveform/spectral
metrics the CELT-only arm still leads the SILK-only and Hybrid arms on
speech at 12–48 kb/s; the speech ladder follows the RFC's mode
semantics (the LP layer for speech at WB and below, with its FEC / DTX
/ PLC behaviour) rather than that measure.

Underneath sit the per-mode arms, each usable directly:

* **SILK-only** (`SilkEncoderMono` / `SilkEncoderStereo`): the full
  §5.2.3 signal analysis — Burg LPC → analysis-direction NLSF
  conversion → analysis-by-synthesis stage-1/2 NLSF quantisation on
  the real decode chain → whitened-domain pitch analysis with joint
  lag/contour quantisation → exact-distortion LTP codebook search →
  residual-energy gain selection through the §4.2.7.4 quantizer → a
  closed-loop excitation quantizer (LCG sign inversion included),
  the §5.2.3.8 noise-shaping quantizer with a 2–4-state
  delayed-decision trellis on the higher complexity rungs, the
  §5.2.2 stereo mixing front end with the §4.2.7.1 weight codebook
  and §4.2.7.2 mid-only escape per interval, §4.2.5 LBRR (FEC) and
  §2.1.9 DTX emission, and the §5.2.3.9 rate loop
  (`encode_packet_elected`: a warm-started secant on the pulse-RMS
  knob over cloned trial encodes). NB/MB/WB × 10/20/40/60 ms; every
  elected oracle stream decodes **bit-exactly** through the §A
  reference-listing decoder.
* **CELT-only** (`CeltEncoder`): the whole §5.3 mirror of the §4.3
  decoder — silence, the §5.3.1 pitch pre-filter with the full
  decision ladder and tapset election, transient / short blocks,
  two-pass coarse energy with the decoder-lockstep energy carry,
  §4.3.4.5 tf analysis (Haar-level L1 metric + Viterbi smoothing),
  spreading, dynalloc, trim, the §4.3.3 allocation with coded
  skip / intensity / dual-stereo decisions, fine energy, the
  recursive §4.3.4 band coder with PVQ search and exact §4.3.4.2
  index construction, anti-collapse, and the fixed-size §5.1.5
  finalization. NB/WB/SWB/FB × 2.5/5/10/20 ms × mono/stereo at any
  payload 2..=1275; streams decode through the reference listing at
  88–108 dB agreement with our decoder and land within 2.7–3.3 dB of
  the listing's own encoder at matched CBR rates.
* **Hybrid** (`HybridEncoderMono` / `HybridEncoderStereo`): the WB
  SILK layer and CELT bands 17.. on one range coder with the two
  layers delay-matched (a 165-tap 48→16 kHz decimator + the §4.2.9
  resampler + the §4.2.8 delay equal the 120-sample MDCT overlap),
  SWB/FB × 10/20 ms, with FEC / DTX and now the explicit §4.5.1.1
  redundancy flag, §4.5.1.2 position and §4.5.1.3 size when a
  transition rides (the main CELT layer is coded against the
  reduced budget). Listing agreement 104–108 dB.
* **Rate control** (`vbr`): `VbrRateControl` — unconstrained drift
  correction or the §2.1.8 constrained bit-reservoir bound
  (provable `n·target + cap` window) — behind `CeltVbrEncoder`,
  `HybridVbrEncoderMono/Stereo` and `SilkVbrEncoderMono/Stereo`
  (realized averages within 0.1–5 % of target on every arm).

Validation of the unified encoder's transition ladders (10 legs
walking every Figure 18 class, mono and stereo) through the crate's
own decoder pins the redundancy decision, its §4.5.1.2 position and
its §4.5.1.3 size packet by packet; through the two black-box
decoders (the §A reference listing's demo decoder and `opusdec`) the
streams decode without error and agree with our decoder at 86.6 dB
(stereo) / 60.3 dB (mono) whole-stream against the listing and
62 dB against `opusdec`, the residual being the decoders' own
non-normative post-switch conditioning on the CELT→Hybrid legs
(37 → 70 dB over four packets, present with and without redundancy).
Seam quality over the §4.5.3 Figure 19 concealment fallback:
SILK→CELT 6.9 → 32.3 dB, CELT→Hybrid 17.9 → 24.0 dB, CELT→SILK at
parity with the (already strong) §4.4 fill on steady content.

Differential encoder/decoder testing and a restored cargo-fuzz suite
(6 coverage-guided targets, incl. an encoder↔decoder range-coder
roundtrip and the CELT / VBR encode→decode harnesses) have also
hardened the decoder: five mis-transcribed rows
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
multichannel decode subsystem (1440+ lib tests + SILK-fixture,
multistream (incl. the 5.1 reference-listing gate), FEC, CELT
synthesis-backend, CELT-encode, Hybrid-encode, VBR, DTX (encoder,
decoder-hold, and reference-stream suites), §4.1 gap-repair, and
registry-resolution integration suites). Per-stage progress lives in
`CHANGELOG.md`.

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

- `OpusDecoder::with_output_rate` — decode at any §4.2.9 supported
  output rate (8/12/16/24/48 kHz): SILK internal → output through the
  full decoder-side reference resampler matrix (bit-exact), CELT via
  the output-Nyquist spectrum bound + phase-0 decimation of the
  de-emphasized 48 kHz signal, Hybrid summing and every cross-lap /
  concealment path on the output-rate timeline
  (`tests/downsampled_decode.rs`).
- §4.5 seams without redundancy: the normative Hybrid→SILK CELT
  overlap flush (2.5 ms silence frame, direct mix), SILK
  bandwidth-change state policy (delay sample carried; bit-exact
  NB↔WB switch captures), and the RECOMMENDED 5 ms PLC fill with
  power-complementary crossfade on CELT↔SILK/Hybrid switches
  (`tests/mode_switch_seams.rs`, `tests/plc_reconvergence.rs`).

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
  sine fixture.
- A **SILK waveform regression-gate suite**
  (`tests/silk_reference_waveform.rs`) that compares each SILK-bearing
  fixture's pre-skip-trimmed 48 kHz decode against its shipped
  reference decode (produced by the §A reference listing's decoder
  with the RFC 8251 corrections) at a **100 dB floor** — the SILK
  fixtures decode bit-exactly, pinning the fixed-point §4.2.7.9 core,
  the integer §4.2.8 unmix + mono delay, and the reference §4.2.9
  resampler.

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
- `register(ctx)` — the framework registration declares the `opus`
  codec id with its RFC 7845 §5.1 payload magic (`OpusHead`), so
  container layers without a codec tag resolve an Opus logical stream
  from its first payload bytes
  (`CodecRegistry::resolve_payload_magic_ref`); `OpusTags` and every
  truncation of the magic are refused by construction (pinned in
  `tests/registry_resolution.rs`). Since round 450 the registration
  also wires working **decoder/encoder factories** (`make_decoder` /
  `make_encoder`, the dual-API convention): registry resolution
  constructs an `OpusStreamDecoder` honouring `CodecParameters`
  (extradata `OpusHead` → channels / §5.1.1 multistream mapping /
  pre-skip / output gain, `sample_rate` → any §4.2.9 output rate) or
  an `OpusStreamEncoder` on the unified `OpusEncoder` (mode / bandwidth / application / cbr / fec / packet-loss / redundancy options included) (channels,
  `bit_rate`, and the typed `OpusEncoderOptions` schema: bandwidth,
  frame-ms, constrained-vbr, dtx, tapset-election, complexity,
  signal-adaptive).
  Registry-resolved decodes of the SILK fixtures are bit-exact
  against their reference decodes.

**Range coder (RFC 6716 §4.1 / §5.1):** `RangeDecoder` — the shared
entropy primitive consumed by both layers, including the §4.1.2
two-step `ec_decode` / `ec_dec_update` path and the Laplace / iCDF
helpers — and `RangeEncoder`, its bit-exact §5.1 write-side mirror
(validated by per-primitive roundtrips, `tell`/`tell_frac` lockstep,
a 5000-seed mixed-symbol fuzz roundtrip, and a coverage-guided
libfuzzer differential target).

**Unified encoder (RFC 6716 §2.1 / §4.5):** `OpusEncoder`
(`opus_encoder`) — one streaming front end over the three arms with
every §2.1 knob, bitrate-driven mode/bandwidth selection per
`Application`, and the §4.5.3 Figure 18 transitions written with
their §4.5.1 redundant CELT frames (`encode_redundant_celt_frame`;
the §4.5.1.2 position symbol via
`encode_silk_only_packet_{mono,stereo}_red`; the Hybrid arms'
`RedundancyPlan` through `encode_packet_elected_with`), the §4.5.2
reset placement mirrored from `decide_state_resets`, and 48 kHz →
8/12/16 kHz input decimation on the CELT 120-sample timeline
(`tests/unified_encoder_transitions.rs`).

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

**§2.1.9 discontinuous transmission (`set_dtx`, every encoder arm):**
inactive packets (the SILK arms' §4.2.3 activity floor; the CELT-only
arm's digital silence, or — with the signal-adaptive election on — the
analyser's tracked noise floor plus margin, so background noise
suppresses too) suppress to the 1-byte TOC-only §3.2.1 marker after
a 2-packet hangover, one coded refresh per 400 ms of suppression,
decoder-authoritative mirrors frozen across the run, LTP-free (SILK)
/ intra-energy (CELT, Hybrid) resume; markers pass through the
elected / VBR paths without an election and are never CBR-padded
(`EncodedSilkPacket::is_dtx`; `tests/dtx_encode.rs`,
`tests/dtx_reference_stream.rs`). Decoder side, every §3.2.1
zero-length frame runs the §4.4 hold per RFC 7845 §4.1 (see
`FrameDecodeStatus::DtxOrLost`).

**RFC 7845 §4.1 gap repair:** `compose_plc_gap_packets` — the
synthesized zero-length-frame packet sequence that requests PLC
across a capture gap (configuration held, frame-size changes
delayed, CELT switch only at the end of the gap, MB→WB, cheapest
§3.2 packings under the R5 bound; `tests/gap_repair.rs`).

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
(§4.2.8, including the mono one-sample delay), the §4.2.9 resampler
(`SilkUpsampler` — the reference decoder's fixed-point resampler over
the Table 54 budget machinery), and **in-band FEC
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

(The §4.3.3 allocation orchestration and the §4.3.5 anti-collapse —
once listed here as structural blockers — have long been implemented
in exact integer arithmetic; the historical note is kept only in the
changelog. With the RFC 6716 §A embedded reference listing ratified as
staged spec material, no CELT or SILK decode stage remains blocked on
external documentation.)

**CELT / Hybrid encode side (RFC 6716 §5.3):** `CeltEncoder`
(`celt_packet_encode`) — CELT-only code-0 packets over the full
configuration matrix at any constant payload size, via
`celt_analysis` (pre-emphasis, forward MDCT, band energies, transient
detector), `celt_energy_encode` (two-pass coarse + fine + finalise),
`celt_alloc_encode` (the §4.3.3 allocation, encode side),
`celt_band_encode` (the recursive §4.3.4 band coder, encode side),
`celt_pvq_encode` (PVQ search + §4.3.4.2 index construction), the
§4.3.2.1 Laplace encoder, and `RangeEncoder::finish_fixed` (the
fixed-size §5.1.5 finalization) — plus `HybridEncoderMono` /
`HybridEncoderStereo` (`hybrid_packet_encode`): the WB SILK layer
(mono, or the §5.2.2-mixed mid/side stereo pair) and CELT bands 17..
on one range coder with delay-matched layer alignment, each with
`encode_packet_elected` (elected payload with SILK-floor raise).
Validated through the crate's own decoder and the §A
reference-listing decoder (88–108 dB agreement between the two
decoders on our streams).

**Opus-level VBR (RFC 6716 §2.1.8 / §3.2.1):** `vbr::VbrRateControl`
— the per-frame size election under a target-bitrate drift
controller, with the constrained-VBR bit-reservoir discipline
(`elect_packet_bytes` / `commit` / `constrained_ceiling_bits`) — and
its mode arms `vbr::CeltVbrEncoder` (silence collapse, transient
boost), `vbr::HybridVbrEncoderMono` / `HybridVbrEncoderStereo`
(floor-raise feedback), and `vbr::SilkVbrEncoderMono` /
`SilkVbrEncoderStereo` (the election driving the SILK-layer rate
control, FEC riding inside the elected sizes). Gated by
`tests/vbr_encode_roundtrip.rs` (rate tracking, parity vs CBR, the
constrained window bound under adversarial bias, silence
banking/repayment, exact frame accounting) and the
`vbr_encode_roundtrip` / `silk_elected_roundtrip` fuzz targets.

**SILK-layer rate control (§5.2.3.8 / §5.2.3.9):**
`ChannelAnalyzer::set_pulse_target` (the quantization-rate knob) +
`SilkEncoderMono/Stereo::encode_packet_elected` (the size election
over cloned trial encodes) on the noise shaping quantizer
(`PulseRateControl` — the λ rate penalty and the `a_syn`
quantized-history shaping feedback; `Wana`-prefiltered targets, the
signal-RMS gain floor). The default (non-elected) encoders are
bit-identical to the pure closed-loop tracker.

**SILK delayed-decision NSQ (§5.2.3.8):**
`silk_nsq_del_dec::quantize_excitation_frame_del_dec` — the
multi-state trellis (dither-diverse states, two candidates per
sample, K-best pruning, winner-elected §4.2.7.7 seed) over per-state
§4.2.7.9 synthesis mirrors; armed per encoder via
`set_nsq_delayed_decision`, elected per frame against the
single-state quantiser on the measured `rd_q23` cost
(`tests/nsq_del_dec_roundtrip.rs`).

**CELT encoder tf analysis (§4.3.4.5):** `celt_tf_analysis` — the
listing's per-band Haar-level L1 sparsity metric (`haar1` /
`l1_metric` with the width bias), the byte-budget λ ladder, and the
Viterbi flip-cost smoothing against the Table 60/62 targets;
`encode_celt_frame` codes the analysed `tf_change` flags
(`tf_select` stays 0, coded only when the tables diverge).

**§2.1.7 loss-optimised LBRR (`set_packet_loss_perc`):** on every
FEC-capable arm (`SilkEncoderMono/Stereo`, `HybridEncoderMono/Stereo`,
their VBR arms) — expected loss ≤10% keeps redundancy on onset
intervals only (an interval whose RMS at least doubles its
predecessor's, or that follows an inactive one; stereo decides on the
mid channel), higher loss protects every active interval with the
LBRR rate ratio ramping from 0.5 to 0.9 at 50%+
(`ChannelAnalyzer::set_lbrr_rate_ratio`); 0 / untouched is
bit-identical legacy FEC (`tests/loss_optimized_fec.rs`).

**Hybrid in-band FEC (`HybridEncoderMono/Stereo::set_fec`):** the
§4.2.5 LBRR re-encode of the previous packet's WB SILK band riding
the shared range coder ahead of the regular SILK frame(s) — stereo
with the §4.2.7.1 weight quintuple on the LBRR mid frame and the
§4.2.5 mid/side interleave — recovered as the 0–8 kHz LP band via
`decode_packet_fec`.

**Complexity ladder (`set_complexity(0..=10)`, every encoder arm):**
one knob over the election machinery — CELT rungs gate the §5.3.1
pre-filter analysis (0..=1 off) and the tapset election (8..=10 on);
SILK rungs pick the §5.2.3.8 state count (1/2/4); Hybrid forwards the
SILK mapping. Untouched defaults are bit-identical to rung 4
(`tests/complexity_ladder.rs`).

**CELT §5.3.1 pitch pre-filter:** `celt_prefilter` — the listing's
pitch estimator (`pitch_downsample` / `pitch_search` /
`remove_doubling`) and the §4.3.7.1 `comb_filter` in the encoder
direction (negated gains, crossfaded transitions), driven by the
listing's decision ladder in `encode_celt_frame` with the
octave / fine-period / 3-bit-gain / tapset parameter coding;
`CeltAnalysis` carries the 1024-sample unfiltered comb lookback via
the two-phase `pre_emphasize` / `finish_frame` API. Hybrid frames
never run it (the decoder's `start == 0` gate). The coded tapset is
an election (`CeltEncoder::set_tapset_election`): per-tapset trial
encodes at the frame's payload size, each decoded through a clone of
a lockstep mirror `OpusDecoder`, best measured SNR committed
(`set_tapset` forces a fixed choice;
`tests/tapset_election_roundtrip.rs`).

## Clean-room sources

The rebuild consults only:

- RFC 6716 — Definition of the Opus Audio Codec, **including its
  Appendix A embedded reference listing** (extracted from the staged
  RFC text itself and hash-verified against the RFC-pinned digest;
  ratified as staged spec material). An instrumented build of the
  listing serves as the decode-exactness oracle.
- RFC 8251 — Updates to the Opus Audio Codec, including the correction
  patches embedded in its text (applied to the oracle and reflected in
  the decoder).
- RFC 7587 — RTP Payload Format for Opus.
- RFC 7845 — Ogg Encapsulation for Opus.
- Black-box invocations of the `opusdec` / `opusenc` binaries (not
  their source) as opaque validators.

No external library source is permitted as a reference under the
workspace clean-room policy.

## License

MIT. See `LICENSE`.
