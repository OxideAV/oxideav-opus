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

Round 418 completed the encoder arc beyond SILK: **CELT-mode packet
encode is real, end to end** (`CeltEncoder`). The full §5.3 stage
sequence mirrors the §4.3 decoder symbol for symbol — silence flag,
post-filter (off), transient analysis + short blocks, two-pass
intra/inter §5.3.2 coarse energy with the decoder-lockstep quantized
`oldBandE` carry, budget-gated tf flags, the §5.3.4 spreading
decision, the dynalloc boost loop, trim analysis, the §4.3.3
allocation with coded skip / intensity / dual-stereo decisions
(encode→decode roundtrips to identical allocations), fine energy,
the recursive §4.3.4 band encode (split angles measured from the
band energies on the step/uniform/triangular PDFs, intensity
collapse, Haar/Hadamard time reorganisation, PVQ pyramid search +
exact §4.3.4.2 index construction at the leaves, the decode side's
exact 1/8-bit budget bookkeeping), the anti-collapse bit, the final
fine backfill, and the fixed-size §5.1.5 finalization where range
bytes and raw bits share exactly the frame's bytes. The whole
configuration matrix encodes — NB/WB/SWB/FB × 2.5/5/10/20 ms × mono
+ stereo at any constant payload 2..=1275 bytes — and every stream
was validated through BOTH decoders: the crate's own `OpusDecoder`
(13–46 dB multitone SNR by rate with a monotone rate ladder) and the
RFC 6716 §A reference-listing decoder (RFC 8251-patched,
hash-verified extraction), which reconstructs our streams
identically to ours at 88–108 dB (float-noise floor, max 1 LSB).
At matched CBR rates on the same content our encoder lands within
2.7–3.3 dB of the reference listing's own encoder (32→128 kb/s
sweep). **Hybrid encode works too** (`HybridEncoderMono`, configs
12–15: SWB/FB × 10/20 ms): the WB SILK layer and the CELT bands
17.. share one range coder with the §4.5.1.1 redundancy flag coded
off under the decoder's 37-bit gate, and the two layers sit on one
timeline (a 165-tap linear-phase 48→16 kHz decimator's 82-sample
delay + the §4.2.9 resampler's 35 + the §4.2.8 mono delay's 3
exactly equal the CELT path's 120-sample MDCT-overlap delay; an
empirical best-lag search returns 120). Hybrid streams decode
through both decoders as well (the listing decoder agrees with ours
at 105–108 dB). The SILK layer has no rate control yet, so a payload
it alone would overflow is rejected cleanly.

Round 431 adds **Opus-level VBR** (RFC 6716 §2.1.8 / §3.2.1):
`vbr::VbrRateControl` elects every code-0 packet's size against a
target bitrate — unconstrained mode corrects by the accumulated drift
(clamped to ±one frame's target, so silence cannot bank an unbounded
spree), constrained mode adds the §2.1.8 bit-reservoir simulation
(spend above target only what below-target packets banked; bank
capped at a documented 100 ms default, giving the provable
`n·target + cap` bound on every n-packet window). `CeltVbrEncoder`
covers the full CELT matrix with 3-byte digital-silence collapse and
a transient pre-detect boost the drift repays; `HybridVbrEncoderMono`
rides `encode_packet_elected` (SILK floor raises feed the drift).
Realized averages land within 2–5% of target on every arm and frame
size; at matched average rate VBR ≥ CBR on steady content and beats
CBR by ~3.3 dB on mixed tone/silence content at equal total bytes. A
15-stream VBR corpus (CELT NB/WB/SWB/FB × 2.5–20 ms × mono/stereo ×
constrained/unconstrained + all four Hybrid configs) decodes through
the §A reference-listing decoder with exact packet and sample counts,
agreeing with our decoder at 90–107 dB (max 1 LSB).

Round 437 closes the remaining encoder-arc frontiers. **SILK-layer
rate control** (the §5.2.3.9 "iterative loop around the noise shaping
quantizer and entropy coding"):
`SilkEncoderMono/Stereo::encode_packet_elected` searches the
excitation-pulse-RMS knob with a warm-started secant over cloned
full-packet trial encodes, adopting the largest packet not exceeding
the election (floor-raising when even the coarsest quantization
overshoots — the drift accounting repays it). Below the default
quality the **§5.2.3.8 noise shaping quantizer** engages: the
§5.2.3.7 `Wana` prefilter on the target (quantized predictor chirped
by `g_ana = 0.95 − 0.01·C`), the `a_syn`-filtered quantized-history
feedback in every pulse decision (`g_syn = 0.95 + 0.01·C`, the
stable `1/Wsyn` noise loop), and a linear `(r − q)² + λ·|q|` rate
penalty — the pure closed-loop tracker's noise-chasing equilibrium
(≈ 1 pulse/sample) made voiced rate irreducible by gain coarsening
alone, and the default path stays bit-identical to before. Measured:
the knob spans ~16–200 bytes/packet (WB 20 ms); elections land at
96–98% of target across NB/WB mono and stereo (+FEC); all elected
oracle streams decode **bit-exactly** through the §A
reference-listing decoder. On top sit the **SILK-only VBR arms**
(`vbr::SilkVbrEncoderMono` / `SilkVbrEncoderStereo`: realized
averages within 0.1% of target at NB 12 k / WB 20–32 k / 40–60 ms /
stereo 28 k constrained + FEC; silence collapses to the header floor
with the post-silence spree bounded at 2× target; a 5-stream oracle
set decodes bit-exactly), **stereo Hybrid encode**
(`HybridEncoderStereo`, configs 12–15 stereo: the §5.2.2 mixing
front end + two-channel §4.2.3 header + mid/side frames and the
stereo CELT bands 17.. on one range coder at the mono arm's
120-sample timeline; L 16.4 / R 12.4 dB at FB 20 ms 144 kb/s,
oracle agreement 104–107 dB) with its **VBR arm**
(`vbr::HybridVbrEncoderStereo`, exact-on-target averages, 103–106 dB
oracle), and the **§4.3.4.5 CELT tf analysis** (the listing's
per-band Haar-level L1 metric + budget-λ Viterbi smoothing;
`encode_celt_frame` now codes real per-band `tf_change` flags —
313/420 band decisions fire on half-bin tone + click content — with
tf-flagged oracle streams agreeing at 93–99 dB). A new
`silk_elected_roundtrip` fuzz target hardened the election against
adversarial content (a §3.2.1 writer overflow at a generous starting
quality now steps the knob down instead of erroring). Finally, the
**§5.3.1 pitch pre-filter** is real: the listing's pitch estimator
(`pitch_downsample` / `pitch_search` / `remove_doubling` with the
sub-multiple confirmation walk) drives the §4.3.7.1 comb applied as
the decoder post-filter's inverse, with the full decision ladder and
octave/period/gain/tapset parameter coding — on voice-like periodic
content it fires on every frame at the exact true period, buys
+1.0–1.3 dB at equal rate over the pf-off encoder, stays off on
noise, and the coded streams agree with the reference-listing
decoder at 81 dB (max 1 LSB). No encoder-arc item remains open.

Round 442 works the encoder-quality tail with two new elections.
The **§5.2.3.8 delayed-decision NSQ**
(`silk_nsq_del_dec::quantize_excitation_frame_del_dec`, armed via
`set_nsq_delayed_decision`) runs the reference listing's multi-state
trellis — up to 4 states on distinct §4.2.7.7 dither seeds, two
quantization-level candidates per state per sample, K-best pruning,
the winner electing the frame's coded seed (two uniform bits either
way, so the election is rate-free) — with each state carrying its own
§4.2.7.9 synthesis mirrors, so the decision horizon spans the whole
frame. Every frame elects between the single-state quantiser and the
trellis on the measured `(recon − want)² + λ·|q|` frame cost, so only
measured wins are adopted and the 1-state default stays bit-identical.
Measured: **+0.8–1.2 dB** at equal elected rate on speech-like content
(WB 25/40/60 B, NB 30 B; 2 states already take most of it), rate −2.4%
at equal SNR on the default-quality path, and five delayed-decision
oracle streams (elected mono / FEC / default / 60 ms multiframe /
stereo) decode **bit-exactly** through the §A reference-listing
decoder. The **§5.3.1 tapset election**
(`CeltEncoder::set_tapset_election`) replaces the hardwired
post-filter tapset 0: each pre-filter-firing frame is trial-encoded
per tapset at the same payload, decoded through a clone of a lockstep
mirror decoder, and the measured-SNR winner is committed — **+0.2–1.7
dB** at equal rate over the fixed-0 encoder on periodic content
(within ±0.1 dB of the best fixed tapset per content), the elected
streams agreeing with the reference-listing decoder at 103 dB (max
1 LSB).

Round 445 closes the r442 followups and lands three new encoder
surfaces. The **delayed-decision × LBRR / Hybrid composition** is
measured and gated: the trellis election runs inside the §4.2.5 LBRR
re-encode itself (elected seeds ride on 7/10 LBRR frames), FEC
recoveries track the clean decode **+1.0 dB** better at equal elected
rate, and Hybrid framing composes at parity with byte-identical
elected sizes. The **tapset-election × VBR silence-collapse
interaction** (flagged untested in r442) is pinned: the lockstep
mirror survives 3-byte silence packets, winning **+1.7 dB
whole-stream / +1.9 dB post-silence** over tapset-0 at 16 kb/s across
a silence gap (oracle: 102.9 dB / max 1 LSB on the silence-gapped
elected VBR stream). The **§2.1.7 loss-optimised LBRR mode**
(`set_packet_loss_perc`, SILK + Hybrid + VBR arms) shapes redundancy
from the declared loss: onsets-only at ≤10% (carriers 143 → 10,
**+2.0 dB** clean at equal elected rate), a 0.5 → 0.9 rate-ratio ramp
above (recoveries **+1.6 dB** at the 50% point; three loss-optimised
oracle streams decode **bit-exactly**). The **complexity ladder**
(`set_complexity(0..=10)` on every encoder arm) maps the election
machinery onto one knob — measured monotone: CELT 14.6 / 19.1 /
20.3 dB at rungs 0/4/10, SILK 9.0 / 9.8 / 10.2 dB — with untouched
encoders bit-identical to the documented default rung. And **Hybrid
in-band FEC** closes the LBRR story across every SILK-bearing mode:
mono and stereo Hybrid packets carry the §4.2.5 redundancy on the
shared range coder (stereo with the §4.2.7.1 weights on the LBRR mid
frame), `decode_packet_fec` recovers the 0–8 kHz LP band, and the FEC
streams agree with the reference-listing decoder at **112–113 dB**
(max 1 LSB).

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
  an `OpusStreamEncoder` on the CELT-only VBR arm (channels,
  `bit_rate`, and the typed `OpusEncoderOptions` schema: bandwidth,
  frame-ms, constrained-vbr, dtx, tapset-election, complexity).
  Registry-resolved decodes of the SILK fixtures are bit-exact
  against their reference decodes.

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

**§2.1.9 discontinuous transmission (`set_dtx`, every encoder arm):**
inactive packets suppress to the 1-byte TOC-only §3.2.1 marker after
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
