#![no_main]

//! Coverage-guided fuzz harness for the RFC 7845 multistream decode
//! subsystem: the §5.1 / §5.1.1 `OpusHead` identification-header parse,
//! the §3 multistream packet split (Appendix-B self-delimited framing
//! for the first `N − 1` streams), and the `MultistreamDecoder`
//! multichannel assembly.
//!
//! Layout of the fuzz input: one length byte `L`, then `L` bytes of
//! OpusHead candidate, then the remainder as up to 4 multistream
//! packets fed to one stateful decoder. A parse failure on the header
//! is a valid outcome; a panic anywhere is a bug.

use libfuzzer_sys::fuzz_target;
use oxideav_opus::multistream::MultistreamDecoder;
use oxideav_opus::opus_head::OpusHead;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let head_len = (data[0] as usize).min(data.len() - 1);
    let head_bytes = &data[1..1 + head_len];
    let body = &data[1 + head_len..];

    let Ok(head) = OpusHead::parse(head_bytes) else {
        return;
    };
    let mut dec = MultistreamDecoder::new(head.mapping);
    if body.is_empty() {
        let _ = dec.decode_packet(body);
        return;
    }
    let chunk = body.len().div_ceil(4).max(1);
    for part in body.chunks(chunk) {
        let _ = dec.decode_packet(part);
    }
});
