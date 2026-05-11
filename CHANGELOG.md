# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **SILK §4.2.7.3 + §4.2.7.4 ICDF tables corrected against RFC 6716**
  (silence-rail regression). Round-prior, four critical SILK ICDF
  tables held simplified approximations rather than the spec PDFs:
  - `FRAME_TYPE_ACTIVE_ICDF` was the 6-symbol PDF
    `{16, 16, 80, 80, 32, 32}/256` instead of the spec's
    `{0, 0, 24, 74, 148, 10}/256` (the leading two zero-prob entries
    are dropped from storage and the `+2` offset is applied at decode
    time, per the standard libopus convention).
  - `GAIN_MSB_INACTIVE_ICDF` was `{32, 96, 48, 32, 16, 16, 16, 0}`
    instead of `{32, 112, 68, 29, 12, 1, 1, 1}/256`.
  - `GAIN_MSB_UNVOICED_ICDF` was `{16, 40, 40, 40, 40, 32, 24, 24}`
    instead of `{2, 17, 45, 60, 62, 47, 19, 4}/256`.
  - `GAIN_MSB_VOICED_ICDF` was `{8, 28, 40, 40, 44, 40, 32, 24}`
    instead of `{1, 3, 26, 71, 94, 50, 9, 2}/256`.
  - `GAIN_DELTA_ICDF` (Table 13, 41 symbols) was right-shifted by one
    PDF entry — the high-probability symbol 4 ("no change") read as 5,
    and the long tail (delta ≥ 8) collapsed onto smaller indices.
  On minimum-bitrate libopus packets where the bitstream is mostly
  carrier-frequency entropy, the wrong cumfreq buckets caused our
  decoder to read `delta_gain_index = 24` (or higher) instead of 4,
  inflating the sub-frame 1 log_gain by `2 * 24 - 16 = 32` indices and
  driving the SILK synthesis IIR amplitude to ±0.7 (saturating the
  s16 output to ±32 768). The silence-rail count on the 1 248-input
  fuzz corpus dropped from 16 → 15 overall, with the SILK-only mode
  going from 4 → 1 (75 % reduction). Hybrid + 2 celt-only offenders
  remain — those need the still-WIP CELT bit-exact path.
  New unit-test mod `silk::tables::gain_icdf_tests` reconstructs each
  PDF from the stored ICDF and pins it against the RFC table verbatim
  so a future agent doesn't accidentally re-introduce the "rough
  approximation" form.
- **Active-frame encoder offset** (`silk::encoder`) — the SILK encoder
  now emits frame-type symbols 2 → 0 and 4 → 2 to match the
  4-symbol-from-6-PDF storage convention introduced above.
- **`tests/roundtrip::silk_nb_voip_decodes_to_audio` RMS bar lowered**
  from 0.001 to 5e-4 to reflect the corrected (quieter, more
  libopus-faithful) SILK output. The previous bar was tuned to the
  inflated-gain output produced by the broken ICDF tables; with
  corrected gains the same VOIP file decodes at ~0.5x amplitude. The
  Goertzel sanity assertion still gates against total silence
  (RMS < 1e-5).

### Added

- **Fuzz scratch tool: `examples/scan_silence.rs`** — walks the
  `opus_oracle_decode` corpus and prints the silence-rail offenders
  (libopus_max ≤ 64, ours_max > 8 000) one per line with mode / cfg /
  channel / payload-length context. Used during the silence-rail
  triage to bisect which tables the regression depended on; kept in
  the tree so future agents can re-run it after each oxideav-celt
  bit-exact landing to track the remaining hybrid + celt-only
  offenders.

- **Fuzz scratch tool: `examples/scan_histogram.rs`** — companion to
  `scan_silence`: walks the same corpus, decodes through both libopus
  and oxideav-opus, and prints the per-mode max-|PCM-diff| histogram
  (silk / hybrid / celt × buckets `= 0 / ≤1 / ≤2 / ≤4 / ≤16 / ≤64 /
  ≤1024 / >1024` LSB). Avoids spinning up cargo-fuzz for the common
  "did my fix move the needle?" question — runs in one cargo invocation
  against the 1 248-packet on-disk corpus.

### Changed

- **Fuzz oracle: post-celt-0.1.5 baseline recorded** in
  `fuzz_targets/opus_oracle_decode.rs`. With the celt 0.1.5 mixed-radix
  FFT + `norm_len` fixes (issue #762) plus this round's silk ICDF
  correction, the on-disk corpus (1 248 inputs, 1 194 oracle-accepted)
  now distributes:
  | mode | n | = 0 | ≤ 16 | ≤ 1024 | > 1024 |
  |------|--:|----:|----:|------:|------:|
  | silk-only | 316 | 10 (3.2 %) | 19 | 55 | 261 |
  | hybrid | 106 | 0 | 0 | 18 | 88 |
  | celt-only | 772 | 13 (1.7 %) | 14 | 51 | 721 |
  Bit-exact celt-only packet count went 0 → 13 — the headline
  improvement from celt 0.1.5. `STRICT_PCM` stays `false`: tightening
  `PCM_TOL` ±2 → ±16 would only add 6 silk / 0 hybrid / 1 celt to the
  exact-match bucket, so the global strict-equality gate would still
  trip on 1 094 / 1 194 = 92 % of the corpus.
- **Silence-rail count: 16 → 15** on the same corpus after celt 0.1.5
  publish. Per-mode breakdown shifted to **12 hybrid / 1 silk / 2
  celt**. The silk count is now within one stray (was 4). Hybrid went
  10 → 12: two short cfg=12/14 hybrid packets (3-11 B payload) that
  previously just-cleared the 8 000-LSB rail on the broken-FFT codepath
  now saturate on the corrected one. The hybrid bit-allocation +
  start-band-17 path is the next round's target.

### Added (round-prior, retained)

- **`SilkChannelState::upsample_history`** — new persistent state
  carrying the previous frame's last `factor` internal-rate samples
  (where `factor` is 6 / 4 / 3 for NB / MB / WB at 8 / 12 / 16 → 48 kHz)
  through the windowed-sinc upsampler so the FIR convolution sees real
  history at the leading edge of each frame instead of zeros. Without
  it, every SILK frame boundary saw zero-history lookback — a real
  opus-side discontinuity (RFC 6716 §4.2.9 mandates a continuous
  resampler chain). New `synth::upsample_to_48k_with_state` API takes
  the history by `&mut Vec<f32>`; the existing stateless
  `upsample_to_48k` is retained for tests and routes through a scratch
  history. PSNR impact on the docs corpus is small because the
  half-window contribution is small relative to the per-sample synthesis
  divergence the celt-bit-exact rebuild has to close, but the change is
  correct in the "concatenation of stateful calls equals one big call"
  sense and clears the way for future improvements to the resampler
  kernel itself.

- **Fuzz oracle: per-mode divergence histogram + scale-saturation
  gate** in `fuzz_targets/opus_oracle_decode.rs`. Replaces the single
  per-divergence `eprintln!` with two new layers:

  * **Per-mode bucket histogram** (`silk` / `hybrid` / `celt`) bucketed
    at `0 / ≤1 / ≤2 / ≤4 / ≤16 / ≤64 / ≤1024 / >1024` LSB, dumped
    every power-of-two iterations from 1024 onward. Distribution shape
    over time tells future agents whether a fix moved the needle or
    just shifted noise.
  * **Scale-saturation gate** — when libopus reports a near-silent
    packet (max |sample| ≤ 64 LSB), our decoder should also stay
    below 8000 LSB. The 1248-input round-next sweep found 16
    violations (10 hybrid, 4 silk-only, 2 celt-only) so the gate is
    `eprintln!` not `assert!` today; once those land, the
    `[oracle silence-saturation]` site flips to a hard panic.

  Throttled the per-divergence trace to 1 in 64 packets. Documented the
  full contract in the file header.

## [0.0.9](https://github.com/OxideAV/oxideav-opus/compare/v0.0.8...v0.0.9) - 2026-05-06

### Other

- drop dead `linkme` dep
- clean-room §4.2.7.5.7/§4.2.7.5.8 + §4.2.7.9.1 rewhitening scaffold
- rewhitening §4.2.7.9.1 + RFC LTP codebook + proper filter search
- §4.2.7.5.5 LSF interp + RFC LTP codebook transcription + 10ms bitstream fix
- add oxideav_core::register! auto-registration macro
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-opus/pull/502))

### Added

- **`fuzz/` cargo-fuzz harness with three targets**:
  `panic_free_decode` (`Decoder::decode(arbitrary_bytes)` returns
  `Result`, never panics), `roundtrip` (random PCM →
  `OpusEncoder::encode` → `OpusDecoder::decode` shape contract),
  and `opus_oracle_decode` (libopus's `opus_decode` via
  `libloading` as oracle; same sample-count contract; PCM ±2 LSB
  divergence is logging-only until the oxideav-celt PVQ/IMDCT path
  is bit-exact). New `.github/workflows/fuzz.yml` schedules the
  daily run; uses the shared `OxideAV/.github` `crate-fuzz`
  workflow with `libopus0 + libopus-dev + opus-tools` apt packages
  preinstalled so the oracle actually validates.

- **RFC 6716 §4.2.7.5.7 / §4.2.7.5.8 spec-faithful Q12 saturation +
  Levinson-derived inverse-prediction-gain stability check** —
  `silk::lsf::bandwidth_expand_q17` implements the §4.2.7.5.7 10-round
  bandwidth-expansion + Q12 saturation pass and `lpc_inverse_pred_gain_is_stable`
  implements the §4.2.7.5.8 16-round Levinson recurrence (with the
  spec's `inv_gain_Q30` accumulation across rows). Both are exercised
  by unit tests but are NOT plugged into the active `nlsf_to_lpc`
  hot path because Q12 quantization on top of the float synthesis
  loses ~5 dB on the in-crate encoder roundtrip; the active path
  retains the round-36 32-round γ=0.85 DC chirp guard. Tracked as a
  follow-up: replace the f32 synthesis with a Q15 fixed-point IIR
  matching libopus' integer saturation byte-exactly to drop the chirp
  guard and unlock the spec-correct spec saturation (~5 dB PSNR
  upside on libopus interop).
- **`SilkChannelState::out_history`** — new persistent ring (320 f32
  samples) carrying the previous frame's clamped output for the
  §4.2.7.9.1 rewhitening pass; the ring is maintained on every frame
  but the rewhitening branch in `silk::synth::synthesize` is gated
  off because writing to `state.ltp_history` mid-frame is incompatible
  with the in-crate encoder's `* ltp_scale` LTP-feedback convention
  (the spec scales once in rewhitening, the encoder scales in every
  feedback tap; both ways are self-consistent in isolation but cannot
  be mixed in the same loop). Re-enabling rewhitening requires a
  coordinated encoder-side change to drop the off-spec scale; tracked
  as a follow-up.

- **RFC 6716 §4.2.7.5.5 LSF interpolation** — implemented for 20 ms SILK
  frames. `silk::lsf::decode_nlsf` now returns `(nlsf_q15, interp_coef_q2)`
  and the decoder builds per-sub-frame LPC coefficient vectors: sub-frames 0
  and 1 use NLSFs interpolated between the previous frame's NLSFs (`n0_Q15`)
  and the current frame's NLSFs (`n2_Q15`) according to
  `n1_Q15[k] = n0_Q15[k] + (w_Q2*(n2_Q15[k]-n0_Q15[k]) >> 2)` when
  `w_Q2 < 4`; sub-frames 2 and 3 always use the uninterpolated NLSFs. The
  SILK decoder now stores `prev_nlsf_q15` across frames and forces
  `w_Q2 = 4` on the first frame or after a decoder reset.

- **RFC 6716 Tables 39-41 LTP codebook transcription** — the exact Q7
  coefficients for periodicity indices 0 (8 entries), 1 (16 entries), and
  2 (32 entries) are transcribed from the RFC into `ltp.rs` as named
  constants (`LTP_P0_Q7`, `LTP_P1_Q7`, `LTP_P2_Q7`). The encoder now uses
  `pick_ltp_filter_from_history` (proper cross-correlation codebook search)
  for voiced frames when the pitch lag exceeds 2; `ltp_filter_from_index`
  (correlation-index map) is retained as a fallback.

### Fixed

- **Decoder panic when stereo TOC arrives at a mono-constructed
  decoder** — `CeltState`'s per-channel buffers (`overlap_buf`,
  `history`, `deemph_state`) were sized at construction by
  `params.channels`. A stereo TOC byte then drove
  `decode_celt_body` to index `state.overlap_buf[1]` on a 1-element
  Vec, panicking with `index out of bounds: the len is 1 but the
  index is 1`. Found by the new `panic_free_decode` cargo-fuzz
  harness on input `[0x26, 0xbc, 0xbc, 0xff]`. Fix: new
  `CeltState::ensure_channels(channels)` grows the per-channel
  buffers (zero-initialised — correct "no prior history" state) on
  the first frame that needs them; `decode_celt_body` calls it up
  front.

- **Decoder panics from CELT pipeline now surface as `Err`** —
  `OpusDecoder::receive_frame` now wraps `decode_packet` in
  `std::panic::catch_unwind` and returns `Error::other("Opus
  decoder: panic in CELT/SILK pipeline — packet rejected")` on a
  caught panic. Restores the `Decoder` trait contract that
  `receive_frame` returns `Result`, never panics, even when the
  internal CELT/SILK pipeline trips an `unreachable_unchecked` /
  shift-overflow / index-out-of-bounds on malformed input.

- **Encoder bitstream desynced for 10 ms SILK frames** — the encoder
  previously emitted `NLSF_INTERP_ICDF` (the §4.2.7.5.5 interpolation
  factor) for both 10 ms and 20 ms frames. The decoder only reads this
  field for 20 ms frames (RFC §4.2.7.5.5: "This field is not transmitted
  for 10 ms frames"), so for 10 ms encode paths the range coder lost sync
  after the NLSF block. All three 10 ms internal-rate SNR roundtrip tests
  (`encode_decode_nb/mb/wb_10ms_internal_rate_snr`) now pass.

- **RFC §4.2.7.9.1 rewhitening — LTP now operates in residual space** —
  the SILK synthesis filter (`synth.rs`) previously fed the post-LPC
  clamped output back into the LTP loop; it now maintains a separate
  `res_ring` buffer for pre-LPC residuals and stores that in
  `state.ltp_history` instead. The encoder's voiced path is updated in
  lockstep: `res_enc[]` replaces `out[]` as the LTP history, and
  `ltp_scale_q14` is applied consistently in both encoder and decoder.
  This aligns with libopus `silk_LTP_analysis_filter_FIX.c` semantics and
  improves MB cross-decode SNR by 0.11 dB (17.82→17.93 dB).

### Changed

- **`register` entry point unified on `RuntimeContext`** (task #502).
  The legacy `pub fn register(reg: &mut CodecRegistry)` is renamed to
  `register_codecs` and a new `pub fn register(ctx: &mut
  oxideav_core::RuntimeContext)` calls it internally. Breaking change
  for direct callers passing a `CodecRegistry`; switch to either the
  new `RuntimeContext` entry or the explicit `register_codecs` name.

## [0.0.8](https://github.com/OxideAV/oxideav-opus/compare/v0.0.7...v0.0.8) - 2026-05-05

### Other

- RFC 4.2.7.5.x NLSF stage-2 + LSF→LPC + BW-expansion guard
- RFC 4.2.7.4 gain dequant + 4.2.7.8.6 LCG dither
- path-aware s16 conversion fixes libopus CELT interop
- apply RFC 6716 §4.3.7.2 CELT post-IMDCT de-emphasis
- honour OpusHead output_gain (RFC 7845 §5.1)
- implement RFC 7845 §5.1.2 pre-skip trimming
- trim crate-level allow block + fix 14 mechanical lints
- rustfmt fix for tests/docs_corpus.rs
- wire docs/audio/opus/fixtures/ corpus into integration tests

### Fixed

- **libopus SILK interop — RFC §4.2.7.5.x NLSF stage-2 residual decoder
  + LSF→LPC + bandwidth-expansion guard** (task #464). The remaining
  major gap in the SILK decode path is now closed:
  - `silk::lsf::decode_nlsf` reads the per-codebook stage-2 PDFs
    (Tables 15 a..h for NB/MB and 16 i..p for WB), the codebook
    selectors (Tables 17/18), and the magnitude-extension PDF
    (Table 19) instead of the previous uniform-11 stub. The decoded
    `I2[k]` chain runs the §4.2.7.5.2 backwards-prediction reconstruction
    via the prediction-weight tables A..D (Table 20) and the
    per-coefficient selectors (Tables 21/22) at integer precision.
  - The §4.2.7.5.3 IHMW weighting is now exact: `w_Q9[k]` is computed
    via the spec's `ilog`-driven sqrt approximation, falling inside
    `[1819, 5227]` for every stage-1 codebook entry. NLSF reconstruction
    is `clamp(0, (cb1_Q8[k]<<7) + (res_Q10[k]<<14)/w_Q9[k], 32767)`
    verbatim from the spec.
  - `silk::lsf::stabilize` now runs RFC §4.2.7.5.4 — the small-
    adjustment loop bounded at 20 iterations against per-coefficient
    minimum spacing (Table 25 NB/MB and WB rows), followed by the
    bullet-proof sort + fallback clamp.
  - `silk::lsf::nlsf_to_lpc` runs the spec's §4.2.7.5.6 P/Q recurrence
    in i64 Q16 against the Q12 cosine LUT (Table 28) with the LSF
    ordering from Table 27 (NB and WB columns transcribed verbatim).
    The resulting LPC matches a float LSP→LPC reference to within
    rounding noise (`< 5e-3` per coefficient across all 64 codebook
    entries).
  - A simplified §4.2.7.5.7/§4.2.7.5.8 bandwidth-expansion guard caps
    the LPC's DC response at `|sum(lpc)| < 0.05` via up to 32 chirp
    rounds at γ=0.85 — without this, the spec-correct cb1<<7 NLSF
    yields LPC vectors aggressive enough to push the float synthesis
    IIR (which lacks the spec's full Q12 saturation pass) to the ±1
    rails on sustained inputs.
  - The encoder side now writes the same per-codebook stage-2 PDFs
    (so encoder/decoder bit alignment is preserved) and uses the
    decoder's NLSF reconstruction for analysis. All 32 SILK encoder
    roundtrip tests still pass at ≥20 dB SNR.
  - **Per-fixture PSNR** vs `opusdec` on `docs/audio/opus/fixtures/`:
    `silk-nb-mono-16kbps` 11.95 → **16.62 dB**;
    `silk-mb-60ms-mono-20kbps` 11.71 → **17.82 dB**;
    `silk-wb-stereo-20kbps` 11.34 → **11.73 dB**. NB/MB hit the half-
    way mark to the 20 dB floor; WB barely moves because its synthesis
    bottleneck is now elsewhere (likely §4.2.7.5.5 LSF interpolation
    or the side-channel LTP path — both still MVP).

- **libopus SILK interop — RFC §4.2.7.4 gain dequantisation +
  §4.2.7.8.6 excitation reconstruction** (task #464). Three
  bit-stream-spec gaps in the SILK decode path are now closed:
  - `silk::gain_index_to_q16` is the bit-exact integer form of
    `silk_log2lin((0x1D1C71 * idx >> 16) + 2090)`. The previous
    float approximation `2^(2090/65536) ≈ 1.022 * 65536 ≈ 67000`
    for `idx = 0` undershot the spec's lower bound (81920, linear
    1.25) by ~18 % and overshot the upper end (1 686 110 208,
    linear 25728) by enough to pin the synth IIR to its clamp
    rails. New unit tests pin both endpoints and round-trip every
    log-gain index 0..=63 through the integer formulation.
  - The §4.2.7.4 delta-coding chain now uses the spec's full
    `clamp(0, max(2*delta - 16, prev_log_gain + delta - 4), 63)`
    formula and tracks `prev_log_gain` as an integer (no Q16 round-
    trip). The `max(2*delta - 16, …)` branch was missing entirely,
    biasing quiet-frame chains toward zero gain.
  - `silk::shell::decode_excitation` now wires the §4.2.7.7 `seed`
    through to the §4.2.7.8.6 reconstruction step
    (`e_Q23 = (e_raw << 8) - sign(e_raw)*20 + offset_Q23`, then
    LCG-driven sign flip + seed update), and emits `e_Q23 / 2^23`
    in normalised Q0 form so the synth filter can apply
    `gain_Q16 / 65536` verbatim per §4.2.7.9.2. The previous
    `signed / 128` scaling silently amplified the excitation by
    256× and the missing LCG dither let zero runs lock the LPC
    integrator into a DC offset — both fixed.
  - `silk::synth::synthesize` now applies `g` only to the
    excitation (not to the LTP feedback term, which our MVP reads
    from the post-LPC `out[]` buffer that already carries a `g`
    factor). Feeding the unclamped LPC ring back into next-
    subframe synthesis matches RFC §4.2.7.9.2's "save unclamped
    `lpc[i]`, export clamped `out[i]`" wording.
  - **Per-fixture PSNR** vs `opusdec` on `docs/audio/opus/fixtures/`:
    `silk-nb-mono-16kbps` 4.31 → **11.99 dB**;
    `silk-mb-60ms-mono-20kbps` 3.81 → **11.95 dB**;
    `silk-wb-stereo-20kbps` 7.04 → **11.34 dB**.
    **Saturation rate** on the same NB clip: ~17 % → **1.18 %** of
    s16 output samples at ±32 767 (the synth filter no longer
    bottoms out the clamp every other sample). Hybrid SILK low band
    rides the same fix but stays at ~3 dB because the NLSF stage-2
    residual decode is still a stub (see README "libopus interop").

- **libopus CELT interop — path-aware s16 conversion** (RFC 6716 §4.3
  + §3.2). The single hard-coded `* 32 768` at the f32→s16 site in
  `decode_packet` / `MultistreamOpusDecoder::receive_frame` was
  pegging every libopus-encoded CELT/Hybrid packet's output to the
  ±32 767 rails, because libopus's CELT `denormalise_bands`
  exponentiates against an `eMeans` table calibrated for Q15 IMDCT
  peaks (~32 768 already), while our in-crate CELT encoder runs on
  [-1, 1]-scale input and produces unit-scale IMDCT output. The two
  conventions can't share a single conversion factor.
  - The interleaver now picks the s16 scale per packet by probing
    the pre-clamp f32 peak: CELT-only or Hybrid frames whose post-
    deemph peak exceeds 4.0 are taken to be in libopus Q15 already
    (multiplier 1.0); everything else (own-encoder CELT, all SILK)
    keeps the historical `* 32 768` (multiplier 32 768.0). The
    threshold of 4.0 sits comfortably above the SILK upsampler's
    1.227× peak FIR overshoot and well below the libopus CELT
    minimum useful magnitude (`2^eMeans[0]` ≈ 86).
  - **PSNR vs `opusdec`** on `docs/audio/opus/fixtures/`:
    `pair-mono-48k-64kbps` 0.77 → **21.07 dB**, `code-1-two-equal-
    frames` 0.59 → **26.27 dB**, `multistream-5.1` 0.69 →
    **16.95 dB**, `celt-fb-stereo-128kbps` 0.37 → **10.24 dB**,
    `pair-cbr-64kbps` 0.47 → **11.84 dB**. Five CELT-bearing fixtures
    promoted to a new `Tier::MinPsnr` corpus tier so the gate hard-
    asserts on the floor (catches the saturation regression that
    `Tier::ReportOnly` happily tolerates).
  - SILK-only and the SILK low-band of Hybrid stay where they were
    (4–7 dB) — those paths' bug is in the synthesis filter / gain /
    LCG dither, not in the s16 scaling. README now carries the
    detailed RFC 6716 §4.2.7.4 / §4.2.7.8.6 anchor for the
    remaining SILK gap.

- **Mono/stereo channel routing on mono-TOC packets** — passing
  `channels = 2` through to `decode_celt_body` for a TOC-mono packet
  was reading stereo bit-budget assignments out of a mono bitstream
  and emitting divergent garbage on both channels (after the deemph
  IIR integrated the bad input it grew to 4×10⁹ in f32). `decode_frame`
  now caps the per-frame channel count to `min(out_channels,
  toc.channels())`, and `decode_packet`'s post-decode loop splats
  the single decoded channel into every output channel instead of
  zero-filling the unfilled buffers. Fixes a long-standing
  `stereo_phase_offset_roundtrip_has_energy_both_channels` failure
  whose `e_l = 1.5e-9, e_r = 0.83` asymmetry was exactly this CELT-
  stereo-on-mono garbage leaking through (the test had been passing
  pre-fix only because every sample saturated to ±32 767 in both
  channels).

### Added

- RFC 6716 §4.3.7.2 **CELT post-IMDCT de-emphasis** — the encoder
  applies a single-pole pre-emphasis high-pass (`y[n] = x[n] -
  alpha_p * x[n-1]`, `alpha_p = 0.85`) to its input PCM before MDCT
  analysis; the decoder MUST run the matching low-pass de-emphasis
  (`y[n] = x[n] + alpha_p * y[n-1]`) on the post-comb-filter output
  to recover the original sample range. Without it every CELT /
  Hybrid output sample is the high-frequency residual (visible as
  sample-by-sample alternation that pegs the s16 converter at
  ±full-scale). The opus crate's `decode_celt_body` now carries the
  per-channel `deemph_state` IIR memory across frames, applies the
  filter via `oxideav_celt::post_filter::deemphasis`, and resets the
  state on `Decoder::reset()` along with the rest of the CELT
  cross-frame state. The standalone CELT crate already had this; the
  opus wrapper was missing it. New
  `silk_nb_440hz_e2e_pipeline_runs_end_to_end` regression test
  drives the libopus → demux → decode path end-to-end (pinning the
  pipeline against a "decoder dispatch broken" regression). README
  now carries an explicit "libopus interop" disclaimer at the top
  and a detailed Not-yet-supported entry covering the remaining SILK
  excitation + CELT PVQ shape gaps.

- RFC 7845 §5.1 **OpusHead `output_gain` honoured** — the Q7.8 dB
  field is now converted to a linear multiplier
  (`10^(g_q8 / (20 * 256))`) at decoder construction and applied to
  every emitted sample before the i16 saturate. Wired into both
  `OpusDecoder` and `MultistreamOpusDecoder`. Was parsed but ignored
  before, so streams with `output_gain != 0` (mostly user-applied
  loudness corrections in Ogg-Opus files) decoded at the wrong
  volume. New `q8_db_to_linear_known_values` test pins the
  conversion (0 dB → 1.0, +6 dB → 1.995, -6 dB → 0.501,
  round-trip cancels).

- RFC 7845 §5.1.2 **pre-skip trimming** — when the OpusHead extradata
  carries a non-zero `pre_skip` (the codec startup delay in 48 kHz
  samples), the decoder now drops that many samples from the head of
  the decoded stream before emitting the first AudioFrame. The first
  emitted frame is shortened by the remaining pre-skip; if pre-skip
  consumes the whole packet, `receive_frame` returns `Error::NeedMore`
  so the caller pulls the next packet without seeing a zero-length
  frame. State updates (CELT IMDCT overlap, SILK LPC history) still
  run on the skipped samples — only output is suppressed. Wired into
  both `OpusDecoder` (single-stream) and `MultistreamOpusDecoder`
  (channel mapping family 1/2). `reset()` replays the original
  pre-skip so a fresh decode of the stream re-trims correctly.

  Visible in the docs/audio/opus/fixtures/ corpus: 4 fixtures now
  decode to **exactly** the reference WAV's sample count (was off by
  exactly one Opus frame each before): celt-2.5ms-low-latency,
  code-1-two-equal-frames, code-2-two-different-frames, and
  code-3-arbitrary-frames-with-padding.

### Other

- Trim crate-level `#![allow]` block from 20 lints to 4. The 16 dropped
  lints (`useless_vec`, `collapsible_if`, `collapsible_else_if`,
  `nonminimal_bool`, `manual_range_contains`, `needless_late_init`,
  `needless_return`, `let_unit_value`, `needless_borrow`, `unused_mut`,
  `unused_variables`, `unused_assignments`, `unnecessary_cast`,
  `manual_memcpy`, `neg_multiply`, `precedence`) no longer fire because
  the underlying call sites have been cleaned up. Resulting `cargo
  clippy --all-targets -- -D warnings` is green with the trimmed allow
  set, so any future regression in those categories surfaces in CI
  rather than being silently masked.

## [0.0.7](https://github.com/OxideAV/oxideav-opus/compare/v0.0.6...v0.0.7) - 2026-05-03

### Other

- revert celt 0.2 → 0.1
- bump oxideav-celt 0.1 -> 0.2 for new AudioFrame layout
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- Hybrid 10 ms (configs 12 + 14) mono and stereo
- Hybrid stereo 20 ms (configs 13 + 15) and broadened SILK MB/WB stereo
- add Hybrid (SILK + CELT) encoder for mono 20 ms SWB / FB
- SNR-validate MB/WB SILK stereo encode + fill NB stereo durations
- cargo fmt cleanup of decoder/encoder/roundtrip tests
- adopt slim VideoFrame/AudioFrame shape

### Added

- Hybrid (SILK + CELT) **10 ms** mono and stereo encoders for SWB
  (config 12) and FB (config 14) per RFC 6716 §4.4. New named
  constructors `HybridEncoder::new_swb_mono_10ms`,
  `new_fb_mono_10ms`, `new_swb_stereo_10ms`, `new_fb_stereo_10ms`
  mirror the existing 20 ms variants but pick the SILK 2-sub-frame
  WB body (160 internal samples) and the CELT LM=2 (480-sample / 10 ms)
  short-block path via the new
  `oxideav_celt::encoder::CeltEncoder::new_with_frame_samples(_, 480)`
  constructor. The shared range-coded layout, SILK VAD/LBRR header,
  stereo prediction header, and CELT
  `start_band = 17` plumbing are unchanged from the 20 ms variant —
  10 ms is otherwise the same hybrid body, just at half the duration.
  RFC 6716 §3.2.1 1275-byte per-frame cap still applies.
- Hybrid (SILK + CELT) mono 20 ms encoder for SWB (config 13) and FB
  (config 15) per RFC 6716 §4.4. Runs SILK-WB on the 0..8 kHz low band
  and CELT (`start_band = 17`) on the 8..12 kHz / 8..20 kHz high band,
  sharing a single range-coded bitstream — the CELT body is appended
  to the in-flight `RangeEncoder` after the SILK body, mirroring the
  decoder's `decode_hybrid_frame` which uses the same arithmetic
  stream end-to-end. New named constructors: `HybridEncoder::new_swb_
  mono_20ms` and `new_fb_mono_20ms`.
- New CELT-encoder helper `CeltEncoder::encode_hybrid_body_mono(pcm,
  enc, start_band, end_band, budget_bytes)` — encodes a CELT body
  into a caller-supplied `RangeEncoder` with the given band range,
  threading all the §4.3 stages (coarse + fine energy, tf_decode,
  spread, dynalloc, allocator, PVQ, fine-finalise) through the
  `start_band..end_band` window. Mirrors `decode_celt_body` on the
  encoder side. Mono only for now.

### Tests

- Promote MB / WB stereo SILK 20 ms tests from "energy + smoke" to full
  SNR + channel-separation. Both clear the 20 dB bar on a 300 Hz
  L-sin / R-cos tone pair (MB ~36 / 31 dB, WB ~43 / 33 dB).
- Add NB stereo round-trip SNR + channel-separation tests at 10 / 40 /
  60 ms (configs 0 / 2 / 3 + stereo bit), filling out the duration
  matrix to match the existing 20 ms NB stereo coverage.
- Add hybrid_roundtrip integration tests covering Hybrid SWB + FB
  20 ms TOC sanity, low-band tone SNR (~24 dB), swept-sine
  band-energy survival in both the < 4 kHz and > 8 kHz regions, and
  silence-stays-quiet bounds.
- Extend hybrid_roundtrip with **10 ms** Hybrid coverage (configs
  12 / 14 SWB / FB, mono + stereo): TOC config sanity, swept-sine
  band-energy survival in both < 4 kHz and > 8 kHz regions, silence
  bounds, and libopus + ffmpeg cross-decode checks for every
  10 ms variant. 32 hybrid_roundtrip tests total (was 20).

## [0.0.6](https://github.com/OxideAV/oxideav-opus/compare/v0.0.5...v0.0.6) - 2026-04-25

### Other

- revert celt pin to 0.1
- pin release-plz to patch-only bumps

## [0.0.5](https://github.com/OxideAV/oxideav-opus/compare/v0.0.4...v0.0.5) - 2026-04-25

### Other

- bump oxideav-celt to 0.2
- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- promote hybrid decode from hidden to first-class + interop tests
- implement real RFC 4.2.7.8 shell-pulse coder
- add SILK encoder voiced-path round-trip tests
- wire SILK encoder voiced/LTP path end-to-end
- add SILK encoder pitch lag + LTP filter encode helpers
- add SILK encoder pitch analysis (autocorrelation)
- README + Cargo description for full SILK encode matrix
- full SILK-only config matrix (0-11) encoder
- clamp CELT post-filter head to `n.min(120)` for short frames
- add SILK 10 ms NB/MB/WB mono frame encoder
- add RFC 6716 Appendix A test-vector smoke test (ignored)
- multistream decoder (channel mapping family 1 + 2)
- decode Hybrid (SILK+CELT) frames per RFC 6716 §4.4
- decode-and-discard SILK LBRR bodies to keep range coder aligned
- add BSD-3-Clause attribution for libopus-derived code
- document SILK MB/WB/stereo encoder modes in README
- round-trip tests + stereo M/S scaling fix for SILK MB/WB/stereo
- add SILK MB/WB mono + NB stereo encoder modes
- factor SILK frame encoder into BandwidthParams descriptor (NB/MB/WB)

## [0.0.4](https://github.com/OxideAV/oxideav-opus/compare/v0.0.3...v0.0.4) - 2026-04-19

### Other

- restore integration tests + bump oxideav-ogg/celt deps to 0.1
- gate ogg-dep integration tests behind `ogg-tests` feature
- add SILK NB mono 20 ms encoder + fix LCG-seed ftb
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
- release v0.0.3

## [0.0.3](https://github.com/OxideAV/oxideav-opus/releases/tag/v0.0.3) - 2026-04-19

### Other

- claim WAVEFORMATEX tags via oxideav-codec CodecTag registry
- rewrite README + refresh crate-level doc to match reality
- exclude local /.cargo dev-only path-patch dir
- make crate standalone (pin deps, add CI + release-plz + LICENSE)
- add Decoder::reset overrides for Vorbis + Opus (+ FLAC note)
- move repo to OxideAV/oxideav-workspace
- add publish metadata (readme/homepage/keywords/categories)
- add CELT-only full-band encoder path
- address workspace-wide lints to unblock CI
- cargo fmt across the workspace
- SRT + WebVTT + ASS/SSA parsing + cross-format transforms
- CELT-only encoder wrapping oxideav-celt's mono long-block encoder
- SILK stereo + 40/60 ms decode
- SILK 10 ms frames + stereo CELT pin + scoped follow-ups
- add SILK decoder skeleton — NB/MB/WB mono 20ms routes to audio
- land full §4.3.3-§4.3.8 pipeline (allocator + PVQ + IMDCT + post-filter)
- bit-exact range decoder + §4.3.2 coarse energy + IFFT scaffold
- land CELT frame-header decode + name the next gap
- TOC byte + framing parser, silence/DTX/CELT-silence-flag decode
- multi-impl registry with capabilities, priority, and fallback
- add FLAC encoder, Opus crate, faithful Ogg page preservation
