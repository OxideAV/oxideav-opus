#![no_main]

//! Differential fuzz harness for the RFC 6716 §5.1 range encoder
//! against the §4.1 range decoder.
//!
//! The fuzz input is interpreted as a symbol script: each step consumes
//! a few bytes that select one of the four write primitives (inverse-CDF
//! symbol, binary logp symbol, uniform integer, raw bits) plus its
//! operands. The script is encoded through `RangeEncoder`, finalized,
//! and decoded back through `RangeDecoder`; every symbol must decode to
//! exactly the value written and the decoder must finish with no
//! latched error. Any mismatch or panic is a bug in one of the two
//! coders (§5.1 requires the encoder/decoder pair to be bit-exact
//! mirrors).

use libfuzzer_sys::fuzz_target;
use oxideav_opus::range_decoder::RangeDecoder;
use oxideav_opus::range_encoder::RangeEncoder;

/// One decoded script step.
enum Step {
    Icdf(usize),
    BitLogp { bit: bool, logp: u32 },
    Uint { t: u32, ft: u32 },
    Raw { v: u32, bits: u32 },
}

fuzz_target!(|data: &[u8]| {
    // A fixed valid inverse-CDF table (strictly decreasing, 0-terminated)
    // with ftb = 8.
    const ICDF: [u8; 5] = [220, 150, 90, 30, 0];

    let mut steps: Vec<Step> = Vec::new();
    let mut enc = RangeEncoder::new();
    let mut it = data.iter().copied();
    // Cap the script so a huge input cannot allocate unboundedly.
    while steps.len() < 512 {
        let Some(op) = it.next() else { break };
        match op & 3 {
            0 => {
                let k = ((op >> 2) & 3) as usize; // 0..=3 (valid symbols)
                enc.enc_icdf(k, &ICDF, 8);
                steps.push(Step::Icdf(k));
            }
            1 => {
                let bit = (op >> 2) & 1 == 1;
                let logp = 1 + ((op >> 3) & 15) as u32; // 1..=16
                enc.enc_bit_logp(bit, logp);
                steps.push(Step::BitLogp { bit, logp });
            }
            2 => {
                let (Some(a), Some(b), Some(c), Some(d)) =
                    (it.next(), it.next(), it.next(), it.next())
                else {
                    break;
                };
                let ft = 2 + (u32::from_le_bytes([a, b, c, d]) & 0x00FF_FFFF);
                let Some(e) = it.next() else { break };
                let t = (e as u32).wrapping_mul(0x0101_0101) % ft;
                enc.enc_uint(t, ft);
                steps.push(Step::Uint { t, ft });
            }
            _ => {
                let (Some(a), Some(b)) = (it.next(), it.next()) else {
                    break;
                };
                let bits = 1 + ((op >> 2) & 31) as u32; // 1..=32
                let v = u32::from_le_bytes([a, b, a ^ 0xFF, b ^ 0xA5])
                    & if bits >= 32 { !0 } else { (1u32 << bits) - 1 };
                enc.enc_bits(v, bits);
                steps.push(Step::Raw { v, bits });
            }
        }
    }

    let bytes = enc.finish();
    let mut dec = RangeDecoder::new(&bytes);
    for step in &steps {
        match *step {
            Step::Icdf(k) => assert_eq!(dec.dec_icdf(&ICDF, 8) as usize, k),
            Step::BitLogp { bit, logp } => {
                assert_eq!(dec.dec_bit_logp(logp) == 1, bit)
            }
            Step::Uint { t, ft } => assert_eq!(dec.dec_uint(ft).unwrap(), t),
            Step::Raw { v, bits } => assert_eq!(dec.dec_bits(bits), v),
        }
    }
    assert!(!dec.has_error());
});
