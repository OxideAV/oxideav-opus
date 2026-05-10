#![no_main]

//! Fuzz: feed arbitrary bytes to the oxideav Opus decoder via the
//! `oxideav_core::Decoder` trait and assert it returns a `Result`,
//! never panics.
//!
//! We exercise both mono and stereo by deriving the output channel
//! count from the first input byte. The remaining bytes are wrapped
//! in a single `Packet` and pushed through `send_packet` /
//! `receive_frame`. Any `Err` is acceptable — only `panic!` /
//! debug-assert / out-of-bounds slice / arithmetic overflow are
//! caught as bugs.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{CodecId, CodecParameters, Packet, TimeBase};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte selects channel count (1 or 2). Remainder is the
    // packet payload.
    let channels: u16 = if data[0] & 1 == 0 { 1 } else { 2 };
    let payload = &data[1..];

    let mut params = CodecParameters::audio(CodecId::new(oxideav_opus::CODEC_ID_STR));
    params.channels = Some(channels);
    params.sample_rate = Some(48_000);

    let mut dec = match oxideav_opus::decoder::make_decoder(&params) {
        Ok(d) => d,
        Err(_) => return,
    };

    let pkt = Packet::new(0, TimeBase::new(1, 48_000), payload.to_vec());
    let _ = dec.send_packet(&pkt);
    // We don't care whether decode succeeds — only that it doesn't
    // panic. Error returns are normal for random bytes.
    let _ = dec.receive_frame();
});
