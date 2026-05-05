# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

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
