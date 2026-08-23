# SILK decode test fixtures

These `.opus` files are Ogg-Opus streams used by
`tests/silk_fixture_decode.rs` to exercise the SILK-only decode path
end-to-end. They are copied verbatim from the project's clean-room Opus
fixture corpus at `docs/audio/opus/fixtures/<name>/input.opus` and embedded
here (via `include_bytes!`) so the test runs in the crate's standalone CI,
which checks out only this repository and not the umbrella `docs/`
submodule.

Each was produced by a **black-box encoder** (only its output bytes are
embedded) from a known synthetic source. The generation commands and
per-stream notes live alongside the originals in
`docs/audio/opus/fixtures/<name>/notes.md`.

| File                              | Config | Mode | Bandwidth | Channels | Frame    |
| --------------------------------- | ------ | ---- | --------- | -------- | -------- |
| `silk-nb-mono-16kbps.opus`        | 1      | SILK | NB        | mono     | 20 ms    |
| `silk-wb-stereo-20kbps.opus`      | 9      | SILK | WB        | stereo   | 20 ms    |
| `silk-mb-60ms-mono-20kbps.opus`   | 7      | SILK | MB        | mono     | 60 ms    |
| `fec-on.opus`                     | 9      | SILK | WB        | mono     | 20 ms    |
| `silence-low-bitrate.opus`        | 1      | SILK | NB        | mono     | 20 ms    |
| `mode-switching.opus`             | 15/31  | Hybrid/CELT | FB | mono     | 20 ms    |
| `code-0-single-frame.opus`        | 13/15/27/31 | Hybrid/CELT | SWB/FB | mono | 20 ms |
| `code-1-two-equal-frames.opus`    | 15     | Hybrid | FB      | mono     | 20 ms    |
| `code-2-two-different-frames.opus`| 31     | CELT | FB        | mono     | 20 ms    |
| `code-3-arbitrary-frames-with-padding.opus` | 15 | Hybrid | FB | mono   | 20 ms    |
| `pair-mono-48k-64kbps.opus`       | 31     | CELT | FB        | mono     | 20 ms    |
| `pair-stereo-48k-64kbps.opus`     | 31     | CELT | FB        | stereo   | 20 ms    |

The §3.2 packing fixtures (`code-*`) and the mono/stereo CELT pair
drive `tests/packing_fixture_decode.rs`. `code-1` is a degenerate
repacked stream whose first frame legally overreads its budget into
§4.1.2.1 zero-fill; reference implementations disagree with each
other on it, so its gate is structural + a loose floor.

The streams also ship their reference decodes
(`<name>.expected.wav`, 48 kHz s16le, copied from
`docs/audio/opus/fixtures/<name>/expected.wav`; produced by the
RFC 6716 §A reference listing decoder with the RFC 8251 corrections
applied — see the per-fixture `notes.md` in `docs/` for the exact
extraction + patch + decode recipe). They drive the waveform-level
SNR regression gates in `tests/silk_reference_waveform.rs`: the SILK
fixtures decode **bit-exactly** (the §4.2.7.9 fixed-point core, the
integer §4.2.8 unmix, and the reference §4.2.9 resampler), so those
gates sit at 100 dB.
`silence-low-bitrate.opus` is a voice-silence-voice signal at 6 kb/s
whose silent region produces near-DTX 6-byte packets (LCG-driven
comfort-noise excitation).

`mode-switching.opus` switches from Hybrid (low-frequency tone) to
CELT-only (full-band content) mid-stream; the black-box encoder emits
§4.5.1 redundancy frames at the transition, so it drives the
`tests/mode_switching_decode.rs` §4.5 transition machinery
(redundant-frame decode + cross-lap + §4.5.2 reset placement). Its
`mode-switching.expected.wav` is the reference decode.

`fec-on.opus` was encoded with in-band FEC enabled (`-fec 1
-packet_loss 10`), so its SILK packets carry §4.2.5 LBRR redundancy of
the prior frame; it drives the `tests/fec_decode.rs` recovery path.

`multistream-5.1.opus` is the RFC 7845 family-1 5.1 multistream
fixture (4 streams: FL/FR + BL/BR coupled, FC + LFE mono, CELT-only
FB 20 ms). Its `multistream-5.1.expected.wav` is the decode by the
**reference listing's multistream decoder** (`opus_multistream.c`
from the §A extraction, RFC 8251 patches applied): pre-skip (312)
dropped, end-trimmed to the 48 000-sample granule length, in the
RFC 7845 §5.1.1.2 **Vorbis channel order** (FL, FC, FR, BL, BR, LFE)
— re-anchored 2026-07 from the old third-party lineage (which was in
WAV channel order). It drives the whole-corpus multistream gate in
`tests/multistream_decode.rs` (whole-stream ≥ 90 dB, per-channel
≥ 80 dB; measured ~100 dB).

## §2.1.9 DTX reference capture

`dtx-refenc-voice-silence.bits` is a packet capture produced by the
RFC 6716 §A reference listing's demo program (opaque invocation with
DTX enabled; voip mode, 16 kHz mono, 16 kb/s) over a synthetic
1 s voice | 3 s digital silence | 1 s voice | 3 s low-level noise
input. Framing: per packet, a big-endian u32 payload length, a
big-endian u32 range-coder word, then the payload (the demo program's
own capture format). It carries 401 packets, 273 of them 1-byte
§3.2.1 DTX markers, with the §2.1.9 one-refresh-per-400 ms cadence
inside the runs.

`dtx-refenc.pre.expected48.pcm` and `dtx-refenc.tail.expected48.pcm`
are two windows of the same program's decoder output (48 kHz s16le
mono): packets 0..56 (start through the last packet before the first
suppression) and packets 215..250 (one refresh period into the second
voice segment). They drive `tests/dtx_reference_stream.rs`: bit-exact
agreement before the first suppression, silence-floor agreement across
the DTX run, and a ≥45 dB steady re-convergence gate after resume
(the residual is the reference decoder's own non-normative
post-concealment smoothing).

## Reduced-output-rate reference decodes (round 450)

`<name>.expected<rate>.pcm` files (e.g.
`silk-nb-mono-16kbps.expected8000.pcm`,
`celt-fb-stereo-128kbps.expected24000.pcm`,
`mode-switching.expected8000.pcm`) are the raw s16le interleaved
output of the §A reference listing's demo program (RFC 8251 patches
applied) decoding the SAME `.opus` stream at that output sample rate
(`-d <rate> <channels>`, Ogg packets re-framed as the demo capture
format; no pre-skip trim — comparison starts at sample zero). They
drive `tests/downsampled_decode.rs`: bit-exact SILK decodes at 8 and
24 kHz, float-floor CELT / Hybrid / mode-switching decodes at 12, 16,
24 and 8 kHz. `dtx-refenc.pre.expected16.pcm` is the DTX capture's
16 kHz reference decode up to the first suppression
(`tests/dtx_reference_stream.rs`).

## §4.5 configuration-switch captures (round 450)

`switch-<a>-to-<b>.bits` are synthetic mode-switch streams in the
demo capture format: 15 × 20 ms mono packets from one reference-
encoder run (opaque invocation) followed by 15 from another —
`hybrid` (voip 48 kHz 32 kb/s FB → config 15), `silkwb` (voip 16 kHz
16 kb/s → config 9), `silknb` (voip 8 kHz 10 kb/s → config 1), `celt`
(restricted-lowdelay 48 kHz 64 kb/s → config 31) — over the same
tone-mix source. Because the halves come from separate encodes no
packet carries §4.5.1 redundancy, which exercises exactly the seams
the §4.5 text handles without it: the Hybrid → SILK overlap flush
(Figure 18 `c` / `+`), SILK bandwidth changes, and the recommended
PLC fill on the non-normative CELT ↔ SILK/Hybrid switches (Figure 19
`P`). `switch-*.expected48.pcm` are the reference listing decoder's
48 kHz decodes; they drive `tests/mode_switch_seams.rs`.

## §4.4 loss-pattern captures (round 450)

`loss-silkwb.bits` / `loss-celt.bits` / `loss-hybrid.bits` are
reference-encoder streams (WB SILK 20 kb/s, FB CELT 64 kb/s, FB
Hybrid 32 kb/s; 20 ms mono, 51 packets over a tone-mix source) with
packets 20–21 (SILK) or packet 20 (CELT/Hybrid) replaced by
zero-length entries — the demo capture format's lost-packet
convention. `loss-*.expected<rate>.pcm` are the reference listing
decoder's decodes of those captures (its own PLC across the holes).
They drive `tests/plc_reconvergence.rs`: bit-exact/float-floor
agreement before the loss and pinned re-convergence floors after it,
identical at 48 kHz and 16 kHz.
