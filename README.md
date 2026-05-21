# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT).

## Status — 2026-05-21 (clean-room round 3)

**Packet header + §3.2 frame-packing parser + §4.1 range decoder;
no SILK / CELT band machinery yet.**

The prior implementation was retired under the workspace clean-room
policy: provenance for several core modules could not be defended
against the "no external library source as reference" rule that
governs every crate in this workspace. Per workspace policy, the only
acceptable response is a full clean-room re-implementation against the
Opus standards documents and black-box validator binaries.

Round 1 (2026-05-20) landed the RFC 6716 §3.1 packet TOC byte parser:
the 32-config × stereo-flag × frame-count-code triple plus the
implied `(min, max)` frame-count range. Five unit tests sweep Table 2,
Table 3, Table 4 and the R1 empty-packet rejection.

Round 2 (2026-05-21) lands the RFC 6716 §3.2 frame-packing parser
behind a new `OpusPacket::parse` entry point:

* **Code 0** (§3.2.2) — one frame, the remaining `N - 1` bytes.
* **Code 1** (§3.2.3) — two equal-size frames; rejects odd `(N - 1)`
  per requirement R3.
* **Code 2** (§3.2.4) — two frames with a one- or two-byte §3.2.1
  length prefix for the first; rejects R4 violations
  (length-exceeds-remaining, length-byte missing, etc.).
* **Code 3** (§3.2.5) — signalled frame count `M ∈ 1..=48` (R5) in
  the frame-count byte, optional Opus padding (with the §3.2.5
  "value 255 chains another length byte" extension), then either CBR
  (every frame is `R / M` bytes; R6 enforces `R % M == 0`) or VBR
  (`M − 1` §3.2.1 length sequences with the final frame implicit;
  R7 enforces no length overrun).

The §3.2.1 helper decodes the one- and two-byte length sequence
(`0`, `1..=251`, `252..=255 → (second*4 + first)`) and treats length
zero as a valid DTX / lost-frame marker (zero-byte slice in the
returned list).

`OpusPacket::frames()` returns `&[&[u8]]` borrowed from the input
buffer; the slices are ready to feed into the SILK / CELT decoders
once those land. Padding length is exposed separately so the caller
can sanity-check against the §3.2.5 budget.

Twenty-seven new unit tests cover each `c` code (round-trip plus
under-length and over-length rejections), the §3.2.1 length encoding
end-to-end (including the 252/255 extension boundaries), the
padding-chain 255-extension behaviour, the R5 cap at 48 frames, and
the R6/R7 boundary conditions.

Round 3 (2026-05-21) lands the RFC 6716 §4.1 range decoder behind a
new `RangeDecoder` API. This is the shared entropy primitive that
every SILK and CELT symbol passes through. The implementation covers:

* §4.1.1 initialization (`b0 >> 1` into `val`, leftover bit into the
  renorm buffer, immediate renormalization to the `rng > 2^23`
  invariant).
* §4.1.2 generic symbol decode (`ec_decode` / `ec_dec_update`) and
  §4.1.2.1 renormalization (MSB-first byte intake with the
  zero-extension past end-of-frame).
* §4.1.3.1 `decode_bin` for power-of-two `ft`.
* §4.1.3.2 `dec_bit_logp` for `2^-logp` binary symbols.
* §4.1.3.3 `dec_icdf` for inverse-CDF table decoding.
* §4.1.4 `dec_bits` for raw bits packed LSB-first from the end of
  the frame, with §4.1.4 zero-extension.
* §4.1.5 `dec_uint` covering both the small (`ftb <= 8`) range-coded
  branch and the large (`ftb > 8`) range-plus-raw-bits branch, with
  the §4.1.5 corrupt-frame error-flag latch.
* §4.1.6.1 `tell()` and §4.1.6.2 `tell_frac()` accounting, satisfying
  the `tell() == ceil(tell_frac() / 8.0)` identity.

The sibling `oxideav-celt` crate carries an independent clean-room
copy of the same primitive — both crates own their own copy until a
shared low-level primitives crate is introduced.

Nineteen new unit tests cover: initialization on empty + non-empty
buffers, `dec_bit_logp` bias under extreme inputs, raw-bit LSB-first
ordering, zero-extension past EOF, `dec_uint` degenerate (`ft=0`,
`ft=1`) and both ftb regimes, `decode_bin` matching the generic
`decode(1<<ftb)` path bit-for-bit, `dec_icdf` agreement with
`dec_bit_logp` on binary distributions plus uniform and
single-symbol coverage, `tell()` and `tell_frac()` monotonicity, the
§4.1.6.1 ceiling identity, and the `dec_bits` zero-width and
over-large-width guards.

Total crate test count: 51 (5 TOC + 27 frame-packing + 19 range
decoder).

Actual SILK / CELT band decoding and the §5 encoder pipeline remain
out of scope; the higher-level encode / decode entry points still
return `Error::NotImplemented`.

## Planned clean-room sources

The clean-room rebuild will consult only:

* RFC 6716 — Definition of the Opus Audio Codec.
* RFC 8251 — Updates to the Opus Audio Codec.
* RFC 7587 — RTP Payload Format for the Opus Speech and Audio Codec.
* RFC 7845 — Ogg Encapsulation for the Opus Audio Codec.
* Black-box invocations of `opusdec` / `opusenc` (the binaries — not
  their source) as opaque validators.

No external library source — libopus, the Opus reference encoder /
decoder, etc. — is permitted as a reference under the workspace
clean-room policy.

## License

MIT. See `LICENSE`.
