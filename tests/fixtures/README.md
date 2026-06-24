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

`fec-on.opus` was encoded with in-band FEC enabled (`-fec 1
-packet_loss 10`), so its SILK packets carry §4.2.5 LBRR redundancy of
the prior frame; it drives the `tests/fec_decode.rs` recovery path.
