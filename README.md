# oxideav-opus

Pure-Rust **Opus** audio codec — RFC 6716 bitstream + RFC 7845 Ogg
mapping. SILK and CELT decode (mono + stereo) plus a CELT-only
full-band encoder. Zero C dependencies.

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
- **Stereo**: both CELT and SILK stereo paths — SILK includes the
  mid/side unmixing filter with prediction-weight interpolation.
- **Mono**: both CELT and SILK mono paths.
- Framing codes 0, 1, 2, 3 — single-frame, paired-equal, paired-variable,
  and VBR/CBR multi-frame packets (RFC 6716 §3.2).
- Silence / DTX packets (0 / 1-byte frames) emit correctly-sized silence.
- CELT-frame silence flag decoded per RFC 6716 §4.3.
- Output: 48 kHz, S16 PCM, 1 or 2 channels.
- `OpusHead` identification packet parsing (RFC 7845 §5.1), channel
  mapping family 0, and the raw mapping-table bytes for families 1 / 2.

### Encode

- **CELT-only, Fullband, 20 ms, 48 kHz.** This is the only mode emitted.
- Packet layout: TOC byte `config = 31` + CELT bitstream, framing code 0
  (single frame per packet).
- Mono input is encoded as-is.
- Stereo input is **downmixed to mono** on the way in — the underlying
  CELT encoder is mono-only today, so the TOC stereo bit is set to zero
  and the per-channel detail is lost. The signal survives end-to-end
  and the decoder splats it back across two channels when asked.
- Input sample formats: `S16`, `S16P`, `F32`, `F32P`.
- Input sample rate: **48 kHz only**. Any other rate returns
  `Error::Unsupported` — resample upstream.

### Not yet supported

- **Hybrid (SILK + CELT) frames** — bailed out with `Error::Unsupported`.
- **SILK LBRR redundancy frames** — the LBRR flags are parsed (so the
  range coder stays aligned) but the redundancy payload itself is not
  yet decoded. Packets that enable LBRR return `Error::Unsupported`.
- **Channel mapping family 1 / 2** (Vorbis / ambisonic multistream,
  more than 2 channels).
- **SILK or Hybrid encoding** — the encoder is CELT-only today.
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

### Codec IDs and capabilities

- Codec ID: `"opus"` (registered via `oxideav_opus::register`).
- The capability entry reports `max_channels = 2` and
  `max_sample_rate = 48_000`, which matches what the decoder + encoder
  actually accept today.

## License

MIT — see [LICENSE](LICENSE).
