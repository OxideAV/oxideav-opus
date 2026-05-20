# oxideav-opus

Pure-Rust Opus audio codec (SILK + CELT).

## Status — 2026-05-20

**Orphan-rebuild scaffold.** The crate's prior implementation was
retired under the workspace clean-room policy: provenance for several
core modules could not be defended against the "no external library
source as reference" rule that governs every crate in this workspace.

Per workspace policy, the only acceptable response is a full
clean-room re-implementation against the Opus standards documents and
black-box validator binaries. That work has not yet been scheduled.

Every public entry point currently returns `Error::NotImplemented`.

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
