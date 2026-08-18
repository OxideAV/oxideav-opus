//! RFC 7845 §4.1 gap-repair composer gates
//! (`compose_plc_gap_packets`): synthesized packet sequences of
//! §3.2.1 zero-length frames that request PLC across a gap, following
//! every §4.1 recommendation — same configuration for as long as
//! possible, frame-size changes delayed, a CELT switch only at the
//! end of the gap, MB→WB on that switch, cheapest packing codes, and
//! the R5 packet-duration bound.

use oxideav_opus::decoder::{FrameDecodeStatus, OpusDecoder};
use oxideav_opus::toc::{Bandwidth, ChannelMapping, Mode, OpusTocByte};
use oxideav_opus::{compose_plc_gap_packets, OpusPacket};

/// Sum of zero-length-frame durations (tenths) across the packets,
/// asserting every frame is in fact zero-length.
fn total_tenths(packets: &[Vec<u8>]) -> u32 {
    let mut total = 0u32;
    for p in packets {
        let toc = OpusTocByte::parse(p).expect("toc");
        let parsed = OpusPacket::parse(p).expect("parse");
        for f in parsed.frames() {
            assert!(f.is_empty(), "gap packet carries a non-empty frame");
            total += u32::from(toc.frame_size_tenths_ms);
        }
    }
    total
}

#[test]
fn rejects_zero_and_non_2_5ms_gaps() {
    // Config 1 = SILK NB 20 ms, mono, code 0.
    let toc = 1u8 << 3;
    assert!(compose_plc_gap_packets(toc, 0).is_err());
    assert!(compose_plc_gap_packets(toc, 30).is_err());
    assert!(compose_plc_gap_packets(toc, 24).is_err());
    assert!(compose_plc_gap_packets(toc, 25).is_ok());
}

/// The §4.1 worked example, on an MB predecessor so the MB→WB rule
/// fires too: a 95 ms gap after a 20 ms SILK frame becomes four 20 ms
/// SILK frames (one 2-byte CBR code-3 packet), one 10 ms SILK frame
/// and one 5 ms CELT frame (one byte each) — "two bytes for a CBR
/// code 3 and one byte each for two code 0 packets".
#[test]
fn worked_example_95ms_after_20ms_silk_mb() {
    // Config 5 = SILK MB 20 ms.
    let prev = 5u8 << 3;
    let packets = compose_plc_gap_packets(prev, 950).unwrap();
    assert_eq!(packets.len(), 3, "{packets:?}");
    assert_eq!(total_tenths(&packets), 950);

    let t0 = OpusTocByte::parse(&packets[0]).unwrap();
    assert_eq!(packets[0].len(), 2, "CBR code 3 is two bytes");
    assert_eq!(t0.mode, Mode::SilkOnly);
    assert_eq!(t0.bandwidth, Bandwidth::Mb);
    assert_eq!(t0.frame_size_tenths_ms, 200);
    assert_eq!(OpusPacket::parse(&packets[0]).unwrap().frames().len(), 4);

    let t1 = OpusTocByte::parse(&packets[1]).unwrap();
    assert_eq!(packets[1].len(), 1);
    assert_eq!(t1.mode, Mode::SilkOnly);
    assert_eq!(t1.bandwidth, Bandwidth::Mb);
    assert_eq!(t1.frame_size_tenths_ms, 100);

    // The CELT switch happens at the END, and MB becomes WB.
    let t2 = OpusTocByte::parse(&packets[2]).unwrap();
    assert_eq!(packets[2].len(), 1);
    assert_eq!(t2.mode, Mode::CeltOnly);
    assert_eq!(t2.bandwidth, Bandwidth::Wb);
    assert_eq!(t2.frame_size_tenths_ms, 50);
}

/// Exactly two same-size frames pack as a 1-byte code-1 packet.
#[test]
fn two_frames_pack_as_code_1() {
    // Config 9 = SILK WB 20 ms.
    let prev = 9u8 << 3;
    let packets = compose_plc_gap_packets(prev, 400).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].len(), 1, "code 1 is the TOC byte alone");
    let toc = OpusTocByte::parse(&packets[0]).unwrap();
    assert_eq!(toc.frame_size_tenths_ms, 200);
    assert_eq!(OpusPacket::parse(&packets[0]).unwrap().frames().len(), 2);
    assert_eq!(total_tenths(&packets), 400);
}

/// A long gap respects the R5 120 ms packet bound (six 20 ms frames
/// per packet) and still closes exactly.
#[test]
fn long_gap_honours_the_120ms_packet_bound() {
    // Config 31 = CELT FB 20 ms.
    let prev = 31u8 << 3;
    let gap = 100_000; // 10 s
    let packets = compose_plc_gap_packets(prev, gap).unwrap();
    assert_eq!(total_tenths(&packets), gap);
    for p in &packets {
        let toc = OpusTocByte::parse(p).unwrap();
        let frames = OpusPacket::parse(p).unwrap().frames().len();
        assert!(
            frames as u32 * u32::from(toc.frame_size_tenths_ms) <= 1200,
            "packet exceeds 120 ms"
        );
    }
    // 500 frames at 6 per packet: 83 full packets + one 2-frame tail.
    assert_eq!(packets.len(), 84);
}

/// 2.5 ms CELT frames: one 2-byte packet covers a full 100 ms gap,
/// and the stereo flag rides through.
#[test]
fn celt_2_5ms_stereo_gap_in_one_packet() {
    // Config 28 = CELT FB 2.5 ms, stereo flag set.
    let prev = (28u8 << 3) | 0x04;
    let packets = compose_plc_gap_packets(prev, 1000).unwrap();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].len(), 2);
    let toc = OpusTocByte::parse(&packets[0]).unwrap();
    assert_eq!(toc.channels, ChannelMapping::Stereo);
    assert_eq!(OpusPacket::parse(&packets[0]).unwrap().frames().len(), 40);
    assert_eq!(total_tenths(&packets), 1000);
}

/// End-to-end: a real stream with a repaired gap decodes packet for
/// packet — every synthesized frame reports `DtxOrLost` (the §4.4
/// hold), the durations land exactly, and the stream resumes.
#[test]
fn repaired_gap_decodes_as_plc_requests() {
    // Walk the NB SILK fixture's Ogg pages (as the PLC suite does).
    let data: &[u8] = include_bytes!("fixtures/silk-nb-mono-16kbps.opus");
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut off = 0usize;
    while off + 27 <= data.len() {
        assert_eq!(&data[off..off + 4], b"OggS");
        let nseg = data[off + 26] as usize;
        let segtab = &data[off + 27..off + 27 + nseg];
        let mut p = off + 27 + nseg;
        for &s in segtab {
            cur.extend_from_slice(&data[p..p + s as usize]);
            p += s as usize;
            if s < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        off = p;
    }
    packets.drain(..2);

    let mid = packets.len() / 2;
    let mut dec = OpusDecoder::new();
    for pk in &packets[..mid] {
        dec.decode_packet(pk).expect("decode");
    }
    // Repair a 95 ms gap with the previous packet's own TOC.
    let gap = compose_plc_gap_packets(packets[mid - 1][0], 950).unwrap();
    let mut held = 0usize;
    for gp in &gap {
        let out = dec.decode_packet(gp).expect("gap decode");
        for o in &out.frame_outcomes {
            assert_eq!(o.status, FrameDecodeStatus::DtxOrLost);
            held += o.samples_per_channel;
        }
    }
    assert_eq!(held, 950 * 48 / 10, "gap output duration");
    // The stream resumes.
    let out = dec.decode_packet(&packets[mid]).expect("resume");
    assert_eq!(
        out.frame_outcomes[0].status,
        FrameDecodeStatus::SilkParamsDecoded
    );
}
