# oxideav-opus

Pure-Rust **Opus** audio codec — RFC 6716 bitstream + RFC 7845 Ogg
mapping. SILK + CELT + Hybrid decode (mono + stereo) plus encoders
for a CELT-only full-band path, the full SILK-only config matrix
(NB / MB / WB, mono + stereo, 10 / 20 / 40 / 60 ms), and Hybrid 20 ms
mono + stereo (SWB / FB). Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

> **Heads-up on libopus interop:** the CELT decoder now produces
> usable output on libopus-encoded packets after a path-aware s16
> conversion landed (the historical `* 32 768` at the float→s16 site
> was peg-clipping every sample of a libopus-Q15-scale CELT IMDCT
> output to the rails — see `decode_packet` in `src/decoder.rs`).
> The SILK side follows the spec literal at §4.2.7.4 (gain index →
> Q16), §4.2.7.5.x (NLSF stage-1 + stage-2 with per-codebook PDFs +
> backwards-prediction dequant + IHMW-weighted reconstruction +
> spec-faithful LSF→LPC), and §4.2.7.8.6 (Q23 excitation with LCG
> dither).
> Cross-decode PSNR vs ffmpeg `opusdec` for the corpus fixtures
> (`docs/audio/opus/fixtures/`):
>
> - CELT-only mono / stereo @ 64 kbps: **11–21 dB**
> - CELT-only fullband stereo @ 128 kbps: **10 dB**
> - CELT multistream 5.1: **17 dB**
> - SILK NB mono @ 16 kbps: **17 dB** (was 11.95)
> - SILK MB mono 60 ms @ 20 kbps: **18 dB** (was 11.71)
> - SILK WB stereo @ 20 kbps: **12 dB** (was 11.34, still capped)
> - Hybrid SWB / FB: **3 dB** (SILK low band same gating as WB stereo)
>
> SILK NB and MB are now half-way to the 20 dB floor. Round-39 status:
> §4.2.7.5.5 frame-to-frame LSF interpolation is wired (sub-frames 0-1
> of a 20 ms frame use NLSF interpolated between the previous frame's
> final NLSFs and the current frame's, gated on `w_Q2 < 4`); the
> §4.2.7.5.7 Q12 saturation + §4.2.7.5.8 Levinson-reflection
> prediction-gain check is implemented in `src/silk/lsf.rs` (see
> `bandwidth_expand_q17` / `lpc_inverse_pred_gain_is_stable`) but is
> NOT in the active `nlsf_to_lpc` path because Q12 quantization on top
> of the float synthesis loses ~5 dB of in-crate encoder roundtrip
> SNR; the active path keeps the round-36 32-round γ=0.85 DC chirp
> guard. A clean-room §4.2.7.9.1 rewhitening prototype lives behind
> the disabled rewhitening branch in `src/silk/synth.rs` (the
> `state.out_history` ring is maintained for it). Closing the residual
> gap to ≥20 dB on libopus packets requires either (a) a Q15
> fixed-point synthesis filter that matches libopus' integer
> saturation byte-exactly, or (b) coordinating the encoder's LTP
> feedback to drop the off-spec `* ltp_scale` multiplier so the
> rewhitening pass can be re-enabled symmetrically.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-opus = "0.0"
```

## Status

### Decode

- **CELT-only frames** at every bandwidth — Narrowband (4 kHz),
  Wideband (8 kHz), Superwideband (12 kHz), Fullband (20 kHz).
- CELT frame sizes: 2.5 / 5 / 10 / 20 ms (config 16–31).
- **SILK-only frames** at NB / MB / WB (8 / 12 / 16 kHz internal rate).
- SILK frame sizes: 10 / 20 / 40 / 60 ms (config 0–11).
- **Hybrid frames (SILK + CELT, RFC 6716 §4.4)** — SILK-WB covers the
  0..8 kHz low band; CELT starts at band 17 (the 8 kHz edge) and fills
  8..12 kHz (SWB) or 8..20 kHz (FB) on the same range-coded bitstream.
  All four configs (12 = SWB 10 ms, 13 = SWB 20 ms, 14 = FB 10 ms,
  15 = FB 20 ms) decode mono and stereo.
- **Stereo**: CELT, SILK, and Hybrid stereo paths — SILK includes the
  mid/side unmixing filter with prediction-weight interpolation.
- **Mono**: CELT, SILK, and Hybrid mono paths.
- Framing codes 0, 1, 2, 3 — single-frame, paired-equal, paired-variable,
  and VBR/CBR multi-frame packets (RFC 6716 §3.2).
- Silence / DTX packets (0 / 1-byte frames) emit correctly-sized silence.
- CELT-frame silence flag decoded per RFC 6716 §4.3.
- Output: 48 kHz, S16 PCM, 1 or 2 channels.
- `OpusHead` identification packet parsing (RFC 7845 §5.1), channel
  mapping family 0, and the raw mapping-table bytes for families 1 / 2.

### Encode

Two explicit entry points, one per Opus mode:

- **CELT-only, Fullband, 20 ms, 48 kHz** (`OpusEncoder::new` /
  `OpusEncoder::new_celt_only_full_band`).
  - Packet layout: TOC byte `config = 31` + CELT bitstream, framing code
    0 (single frame per packet).
  - Mono input is encoded as-is.
  - Stereo input is **downmixed to mono** on the way in — the underlying
    CELT encoder is mono-only today, so the TOC stereo bit is set to
    zero and the per-channel detail is lost. The signal survives
    end-to-end and the decoder splats it back across two channels when
    asked.
  - Input sample rate: **48 kHz only**. Any other rate returns
    `Error::Unsupported` — resample upstream.

- **SILK-only**, full config matrix (configs 0..=11), mono and stereo,
  10 / 20 / 40 / 60 ms frames — 24 named constructors, one per
  (bandwidth, channels, duration) tuple:
  - 20 ms mono: `SilkEncoder::new_nb_mono_20ms` (config 1),
    `new_mb_mono_20ms` (config 5), `new_wb_mono_20ms` (config 9).
  - 20 ms stereo: `new_nb_stereo_20ms` / `new_mb_stereo_20ms` /
    `new_wb_stereo_20ms` (configs 1 / 5 / 9 + stereo bit).
  - 10 ms mono + stereo: configs 0 / 4 / 8. Each embedded SILK frame
    has 2 sub-frames instead of 4.
  - 40 ms mono + stereo: configs 2 / 6 / 10. Packet carries 2 back-to-
    back 20 ms SILK frame bodies per RFC §4.2.4 (still framing code 0).
  - 60 ms mono + stereo: configs 3 / 7 / 11. 3 back-to-back bodies.

  Each constructor accepts either the SILK internal rate (8 / 12 /
  16 kHz for NB / MB / WB) or 48 kHz; 48 kHz input is downsampled by a
  simple box-average pre-filter.

- **Hybrid (SILK + CELT) 20 ms and 10 ms** at SWB and FB, mono and
  stereo (`HybridEncoder::new_{swb,fb}_{mono,stereo}_{10,20}ms`) —
  TOC configs 12 / 13 (SWB 10 / 20 ms) and 14 / 15 (FB 10 / 20 ms).
  Per RFC 6716 §4.4 the SILK part runs WB (16 kHz internal, covering
  0..8 kHz) regardless of TOC bandwidth; the CELT part starts at band
  17 (the 8 kHz edge) and covers 8..12 kHz (SWB) or 8..20 kHz (FB)
  on the **same** range-coded bitstream — the CELT body is appended
  to the in-flight `RangeEncoder` after the SILK body so the whole
  packet is one shared arithmetic stream, exactly what the decoder
  expects.

  At 20 ms the SILK frame encoder uses 4 sub-frames and the CELT
  high-band runs at LM=3 (960-sample MDCT). At 10 ms the SILK frame
  encoder uses 2 sub-frames and the CELT high-band runs at LM=2
  (480-sample MDCT) via
  `oxideav_celt::CeltEncoder::new_with_frame_samples(_, 480)`.

  Stereo Hybrid runs a mid/side pair of WB SILK frame encoders for the
  low band (with the RFC §4.2.7.1 prediction header — weights shipped
  as (0, 0) for this MVP) and a dual-stereo CELT high-band via
  `oxideav_celt::CeltEncoder::encode_hybrid_body_stereo`. Packets are
  capped at the RFC 6716 §3.2.1 1275-byte per-frame limit so libopus /
  ffmpeg accept them.

  Input: 48 kHz mono or stereo.

  Round-trip through our own decoder on a 300 Hz low-band tone:
  - SWB 20 ms hybrid mono: ~24 dB low-band SNR
  - FB 20 ms hybrid mono: ~24 dB low-band SNR
  - SWB 20 ms hybrid stereo (300 Hz L / 400 Hz R): ~23 dB L / ~23 dB R

  Cross-decode through libopus / ffmpeg: SWB / FB mono and stereo at
  both 10 ms and 20 ms decode without error to non-trivial PCM.

  The high band is exercised by swept-sine tests (`hybrid_*_sweep_*`)
  that confirm both the < 4 kHz (SILK) and > 8 kHz (CELT) regions
  carry recovered energy after a round-trip — including per-channel
  on stereo Hybrid.

  Stereo paths feed a mid/side pair into two SILK frame encoders and
  emit the RFC §4.2.7.1 prediction header per embedded 20 ms SILK
  frame (weights are shipped as 0 for this pass — enough for a clean
  round-trip, see follow-up list below).

  Packet layout: TOC byte + SILK bitstream. Always framing code 0;
  40 / 60 ms packets use the RFC §4.2.4 multi-SILK-frame-per-Opus-frame
  mechanism rather than framing codes 1/2/3.
  - Analysis-by-synthesis design: each per-bandwidth SilkFrameEncoder
    runs the same LPC filter the decoder reconstructs from the NLSF
    stage-1 index (shared BandwidthParams descriptor: NB/MB use LPC
    order 10, WB uses LPC order 16), computes the residual sample-by-
    sample against the decoder's reconstructed past, and emits
    quantised residual magnitudes. Round-trip SNR through our own
    decoder clears 20 dB on speech-like tones — typical measured values
    (see `encoder_roundtrip.rs`):
    - NB mono: ~24 dB
    - MB mono: ~25 dB
    - WB mono: ~29 dB
    - NB stereo: ~31 dB (L) / ~27 dB (R) at all of 10 / 20 / 40 / 60 ms
    - MB stereo: ~36 dB (L) / ~31 dB (R) at 20 ms
    - WB stereo: ~43 dB (L) / ~33 dB (R) at 20 ms
  - Bitstream layout follows RFC 6716 §4.2 header order (frame type →
    gains → NLSF → LTP (skipped for unvoiced) → LCG seed → excitation);
    the excitation *body* uses an MVP carrier format documented in
    `src/silk/excitation.rs` (nibble-pair + sign per sample in place of
    the RFC's shell-pulse split). Byte-exact parity with libopus'
    `silk_enc` bit-stream is a tracked follow-up.
  - **NLSF stage-1 codebook search** (RFC 6716 §4.2.7.5.1) — every
    frame runs an open-loop search over the 32 candidate stage-1
    codebook entries and picks the one whose all-zero-residual LPC
    minimises the prediction residual on the input. Replaces the prior
    fixed idx-0 fallback so the encoded LPC tracks the signal's actual
    spectral envelope. Per-frame hysteresis + cold-start anchor keep
    near-stationary content (held vowels, tones, noise) on a single
    LPC across the run; clearly-different content (vowel transitions,
    formant slides) flips the index. See
    `silk::encoder::pick_nlsf_stage1_index`.
  - **NLSF stage-2 quantisation** (RFC 6716 §4.2.7.5.2 / §4.2.7.5.6) —
    after the stage-1 codebook entry is chosen, every frame runs a
    coordinate-descent search over per-coefficient residuals
    (candidate set `[-4..4]`, two passes) to refine the synthesised
    NLSF closer to the input's spectrum than the bare codebook row.
    Adoption guard (`STAGE2_ADOPTION_FACTOR = 0.95`) reverts to all-
    zero residuals on near-stationary content where the stage-1 entry
    is already near-optimal, keeping the existing tone round-trip
    tests at their historical SNR. On vowel-formant content the
    search reaches ~30 % open-loop residual reduction over the
    stage-1-only baseline (vs the ~5 % stage-1 alone achieved over
    the historical fixed-idx-0 fallback). The end-to-end closed-loop
    SNR delta is small with the current MVP excitation coder — the
    payoff is bitstream fidelity that will translate to SNR once the
    encoder grows the spec's full Q12-saturated synthesis chain. See
    `silk::encoder::pick_nlsf_stage2_residuals`.

- Input sample formats (all encoders): `S16`, `S16P`, `F32`, `F32P`.

### Not yet supported

- **SILK LBRR redundancy frames** — the LBRR flags are parsed (so the
  range coder stays aligned) but the redundancy payload itself is not
  yet decoded. Packets that enable LBRR return `Error::Unsupported`.
- **Channel mapping family 1 / 2** (Vorbis / ambisonic multistream,
  more than 2 channels).
- **SILK stereo predictor** — the stereo encoder currently emits
  prediction weights of (0, 0). Wiring the full Wiener-filter analysis
  path in `silk::encoder::stereo_predict_weights_q13` is a follow-up
  (the function is already in place; the remaining work is subtracting
  the predicted side from the coded side before it reaches the
  SilkFrameEncoder).
- **10 ms Hybrid** (configs 12 / 14) — 20 ms mono + stereo Hybrid is
  wired (configs 13 / 15); 10 ms Hybrid needs the LM=2 CELT encoder
  path which still runs LM=3 only.
- **Voiced / LTP-path SILK encoding** — the encoder emits
  `signal_type = unvoiced` on every frame so the LTP loop-back is not
  exercised; this still round-trips speech-like tones at ≥ 20 dB SNR
  but gives up the pitch-prediction gain that voiced LTP provides.
- **CELT encoding of 2.5 / 5 / 10 ms frames**, 40 / 60 ms multi-frame
  packets, and framing codes 1 / 2 / 3 on the encoder side.
- **Native CELT stereo encoding** (coupled L/R PVQ with intensity and
  dual-stereo) — tracked in `oxideav-celt`.
- **libopus bitstream interop.** Decoder symptoms after the round-7
  s16-scale fix:
  - **CELT** — works (`pair-mono-48k-64kbps`: 21 dB PSNR vs opusdec;
    `multistream-5.1`: 17 dB; `celt-fb-stereo-128kbps`: 10 dB). The
    historical bug was a uniform `* 32 768` at the f32→s16 conversion
    that pegged every libopus CELT IMDCT output (whose peak is
    `~32 768` already, because `denormalise_bands` exponentiates
    against an eMeans table calibrated for Q15) to the rails.
    `decode_packet` now picks the scale per packet by probing the
    pre-clamp f32 peak, leaving the in-crate encoder's [-1, 1]-scale
    output unaffected. The 2.5 ms low-latency CELT case still floors
    at ~2 dB — LM=0 transient handling needs a separate look.
  - **SILK** — §4.2.7.4 gain dequant, §4.2.7.8.6 Q23 excitation
    reconstruction with LCG sign-perturbation, §4.2.7.5.{1,2,3,4,6}
    NLSF stage-1 + stage-2 + IHMW reconstruction + monotone-spacing
    stabilisation + spec-faithful LSF→LPC are all RFC-literal now.
    Per-fixture PSNR is **NB 17 dB / MB 18 dB / WB stereo 12 dB**
    (was 4–7 dB pre-round-7, 11–12 dB after gain+excitation fix).
    The remaining gaps:
    - §4.2.7.5.5 frame-to-frame LSF interpolation (parsed but
      discarded — the synthesis path uses the freshly-decoded NLSF
      for both halves of a 20 ms frame). This is what bottlenecks
      WB stereo, where the side channel benefits most from the
      `n0/n1` interp blend.
    - §4.2.7.5.7 / §4.2.7.5.8 Q12 LPC saturation + Levinson-driven
      prediction-gain limiting (we use a coarse DC-response chirp
      instead).
    - §4.2.7.9.1 LSF-interpolation rewhitening branch (uses the
      `ltp_scale_q14` field which is currently parsed-and-discarded
      on the decode side).
  - **Hybrid** — still ~3 dB because the SILK low band is the
    bottleneck (same gating as SILK-only WB stereo above).

  Per-band energy decode (`unquant_coarse_energy` / `…_fine_energy`)
  is correct; the remaining SILK gap is now in §4.2.7.5.5/7/8 and the
  rewhitening branch listed above.

- **Bit-exact CELT PVQ + IMDCT output.** Even on the in-crate roundtrip
  (where bitstream parity is guaranteed) the reconstructed waveform
  phase drifts vs libopus. Round-trip PSNR is ~8 dB today — good
  enough to prove encode+decode work end-to-end, short of the 25+ dB
  a bit-exact decoder would give. Tracked in `oxideav-celt` module
  docs.

## Usage

### Decode

```rust,no_run
use oxideav_core::{CodecId, CodecParameters, Decoder, Frame, Packet, RuntimeContext, TimeBase};

let mut ctx = RuntimeContext::new();
oxideav_opus::register(&mut ctx);

let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
params.channels = Some(2);
params.sample_rate = Some(48_000);
let mut dec = ctx.codecs.make_decoder(&params)?;

let opus_packet_bytes: Vec<u8> = read_opus_packet_bytes();
let pkt = Packet::new(0, TimeBase::new(1, 48_000), opus_packet_bytes);
dec.send_packet(&pkt)?;
if let Frame::Audio(audio) = dec.receive_frame()? {
    // audio.format == SampleFormat::S16, interleaved LE.
    // audio.sample_rate == 48_000 (always — RFC 7845 §4).
}
# fn read_opus_packet_bytes() -> Vec<u8> { vec![0xFC] }
# Ok::<(), oxideav_core::Error>(())
```

For Opus-in-Ogg, pull packets via the `oxideav-ogg` demuxer first; the
first Ogg packet is the `OpusHead` which this crate parses with
`oxideav_opus::parse_opus_head`.

### Encode (CELT-only, 48 kHz)

```rust,no_run
use oxideav_codec::Encoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, SampleFormat, TimeBase};
use oxideav_opus::encoder::{OpusEncoder, OPUS_FRAME_SAMPLES};

let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
params.channels = Some(1);
params.sample_rate = Some(48_000);
let mut enc = OpusEncoder::new(&params)?;

// One Opus frame = 960 samples at 48 kHz = 20 ms.
let pcm_s16 = vec![0u8; OPUS_FRAME_SAMPLES * 2];
let frame = Frame::Audio(AudioFrame {
    format: SampleFormat::S16,
    channels: 1,
    sample_rate: 48_000,
    samples: OPUS_FRAME_SAMPLES as u32,
    pts: None,
    time_base: TimeBase::new(1, 48_000),
    data: vec![pcm_s16],
});
enc.send_frame(&frame)?;
let pkt = enc.receive_packet()?;
// pkt.data[0] is the TOC byte: (31 << 3) | (stereo_bit << 2) | 0
# Ok::<(), oxideav_core::Error>(())
```

### Encode (SILK-only, NB mono, 20 ms)

Analogous constructors exist for MB mono (`new_mb_mono_20ms`, 12 kHz
internal), WB mono (`new_wb_mono_20ms`, 16 kHz internal) and NB stereo
(`new_nb_stereo_20ms`, 8 kHz internal, 2-channel input).


```rust,no_run
use oxideav_codec::Encoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, SampleFormat, TimeBase};
use oxideav_opus::encoder::{SilkEncoder, SILK_NB_RATE};

let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
params.channels = Some(1);
params.sample_rate = Some(SILK_NB_RATE); // 8 000 Hz
let mut enc = SilkEncoder::new_nb_mono_20ms(&params)?;

// One SILK NB frame at the internal rate = 160 samples = 20 ms.
let pcm_s16 = vec![0u8; 160 * 2];
let frame = Frame::Audio(AudioFrame {
    format: SampleFormat::S16,
    channels: 1,
    sample_rate: SILK_NB_RATE,
    samples: 160,
    pts: None,
    time_base: TimeBase::new(1, SILK_NB_RATE as i64),
    data: vec![pcm_s16],
});
enc.send_frame(&frame)?;
// One Opus packet per 20 ms of input.
let pkt = enc.receive_packet()?;
// pkt.data[0] is the TOC byte: (1 << 3) | 0 — SILK NB 20 ms mono.
# Ok::<(), oxideav_core::Error>(())
```

### Codec IDs and capabilities

- Codec ID: `"opus"` (registered via `oxideav_opus::register`).
- The capability entry reports `max_channels = 2` and
  `max_sample_rate = 48_000`, which matches what the decoder + encoder
  actually accept today.

## License

MIT — see [LICENSE](LICENSE).
