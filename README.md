# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT).

## Status — 2026-05-20 (clean-room round 1)

**Orphan-rebuild scaffold, TOC byte parser only.**

The prior implementation was retired under the workspace clean-room
policy: provenance for several core modules could not be defended
against the "no external library source as reference" rule that
governs every crate in this workspace. Per workspace policy, the only
acceptable response is a full clean-room re-implementation against the
Opus standards documents and black-box validator binaries.

Round 1 lands the RFC 6716 §3.1 packet TOC byte parser:

* Five-bit `config` decoded against Table 2 (32 entries × mode ×
  bandwidth × frame-size).
* `s` stereo bit decoded against the Table 3 prose immediately
  following Table 2 (0 = mono, 1 = stereo).
* `c` two-bit frame-count code decoded against the Table 4 prose
  (codes 0..3 → one frame / two-equal / two-unequal / arbitrary).
* `frame_count_range()` returns the implied `(min, max)` frame count
  without consulting further bytes (codes 0/1/2 are exact; code 3
  reports the legal `(1, 48)` range derived from §3.2.5's "no more
  than 120 ms total" rule).
* `parse(packet)` rejects an empty packet per requirement R1 of §3.1.

Five unit tests cover the entire enumeration: the 32 configs (Table
2), the stereo bit polarity across all `(config, c)` combinations
(Table 3), the four frame-count codes with their `(min, max)` frame
ranges (Table 4), the R1 empty-packet rejection, and a spot-check
parse of a hand-assembled byte at both ends of the config space.

Actual SILK / CELT frame decoding, the §3.2 frame-packing layer, the
§4 range coder, and the §5 encoder pipeline are out of scope for
round 1; the encode / decode entry points still return
`Error::NotImplemented`.

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
