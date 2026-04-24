# oxideav-opus

Pure-Rust **Opus** audio codec — RFC 6716 bitstream + RFC 7845 Ogg
mapping. SILK + CELT + Hybrid decode (mono + stereo) plus encoders
for a CELT-only full-band path and the full SILK-only config matrix
(NB / MB / WB, mono + stereo, 10 / 20 / 40 / 60 ms). Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

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
    - NB stereo: ~30 dB (L) / ~27 dB (R)
  - Bitstream layout follows RFC 6716 §4.2 header order (frame type →
    gains → NLSF → LTP (skipped for unvoiced) → LCG seed → excitation);
    the excitation *body* uses an MVP carrier format documented in
    `src/silk/excitation.rs` (nibble-pair + sign per sample in place of
    the RFC's shell-pulse split). Byte-exact parity with libopus'
    `silk_enc` bit-stream is a tracked follow-up.

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
- **Hybrid encoding** — still `Error::Unsupported` end-to-end.
- **Voiced / LTP-path SILK encoding** — the encoder emits
  `signal_type = unvoiced` on every frame so the LTP loop-back is not
  exercised; this still round-trips speech-like tones at ≥ 20 dB SNR
  but gives up the pitch-prediction gain that voiced LTP provides.
- **CELT encoding of 2.5 / 5 / 10 ms frames**, 40 / 60 ms multi-frame
  packets, and framing codes 1 / 2 / 3 on the encoder side.
- **Native CELT stereo encoding** (coupled L/R PVQ with intensity and
  dual-stereo) — tracked in `oxideav-celt`.
- **Bit-exact CELT PVQ + IMDCT output.** The current CELT decoder
  preserves energy (roughly 90 % of the input energy on a 1 kHz sine
  round-trip) but the reconstructed waveform phase can drift vs libopus.
  The round-trip PSNR bar in the integration tests is ~8 dB today —
  good enough to prove encode+decode work end-to-end, short of the
  25+ dB a bit-exact decoder would give. Tracked in `oxideav-celt`
  module docs.

## Usage

### Decode

```rust,no_run
use oxideav_codec::{CodecRegistry, Decoder};
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};

let mut codecs = CodecRegistry::new();
oxideav_opus::register(&mut codecs);

let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
params.channels = Some(2);
params.sample_rate = Some(48_000);
let mut dec = codecs.make_decoder(&params)?;

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
