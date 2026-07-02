#![no_main]

//! Coverage-guided fuzz harness for `OpusDecoder::decode_packet_fec` —
//! the RFC 6716 §2.1.7 / §4.2.5 in-band-FEC (LBRR) recovery entry
//! point, interleaved with regular decodes on the same decoder state.
//!
//! The FEC path decodes the §4.2.4 LBRR flags and the §4.2.5 LBRR
//! frame(s) (mono or interleaved mid/side stereo) in Table-5 order and
//! runs the full §4.2.7.9 synthesis from a fresh state, so it reaches
//! deep SILK decode surface from a distinct entry angle: the recovered
//! state then feeds a subsequent regular decode.
//!
//! Contract under test: every byte sequence produces `Ok(FecRecovered)`
//! or `Err(..)` from the FEC entry and `Ok`/`Err` from the follow-up
//! regular decode, without panicking.

use libfuzzer_sys::fuzz_target;
use oxideav_opus::decoder::OpusDecoder;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let mut dec = OpusDecoder::new();
    // Split: first half warms the decoder as a "previous packet", the
    // whole input then plays the "next received packet" the FEC
    // recovery is invoked on, followed by its regular decode (the real
    // call sequence an application performs on a detected loss).
    let mid = data.len() / 2;
    let _ = dec.decode_packet(&data[..mid]);
    let _ = dec.decode_packet_fec(data);
    let _ = dec.decode_packet(data);
});
