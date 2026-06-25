//! Opus multistream packet handling — RFC 7845 §3 + §5.1.1.
//!
//! A multichannel Ogg-Opus stream encodes its `M + N` decoded channels
//! as `N` independent Opus streams (the first `M` of which are stereo).
//! Every Ogg packet therefore carries `N` separate Opus packets glued
//! together. RFC 7845 §3 (pp. 4–5) pins the layout:
//!
//! > The first (N − 1) Opus packets, if any, are packed one after
//! > another into the Ogg packet, using the self-delimiting framing from
//! > Appendix B of \[RFC6716\]. The remaining Opus packet is packed at
//! > the end of the Ogg packet using the regular, undelimited framing
//! > from Section 3 of \[RFC6716\].
//!
//! This module performs that split — and only the split. It takes a
//! whole multistream packet plus the stream count `N` (from the
//! [`crate::opus_head::ChannelMappingTable`]) and recovers the `N`
//! per-stream Opus packet byte-slices, each of which is a complete Opus
//! packet directly decodable by [`crate::decoder::OpusDecoder`].
//!
//! The actual per-stream decode + channel-map mixing is composed on top
//! of this split by the multistream decoder; keeping the split as a pure
//! function makes it independently testable against the §3 framing
//! rules.
//!
//! ## Provenance
//!
//! RFC 7845 §3 (pp. 4–5) for the N-packet layout and §5.1.1 for `N`.
//! The self-delimiting framing it relies on is RFC 6716 Appendix B,
//! already implemented in [`crate::framing_self_delim`]. No external
//! library source is consulted.

use crate::framing_self_delim::parse_self_delimited;
use crate::Error;

/// One stream's raw Opus packet bytes within a multistream packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPacket<'a> {
    /// The complete Opus packet bytes for this stream. For the first
    /// `N − 1` streams these are the bytes the self-delimited parser
    /// claimed *minus* the Appendix-B extra length fields — i.e. a
    /// reconstructed regular packet is NOT produced here; instead this
    /// slice is the self-delimited packet's full extent (TOC through its
    /// frames), which a stream decoder consumes via the self-delimited
    /// entry point. The final stream's slice is the undelimited
    /// remainder.
    pub bytes: &'a [u8],
    /// `true` if this stream's bytes use RFC 6716 Appendix-B
    /// self-delimited framing (the first `N − 1` streams); `false` for
    /// the final stream, which uses the regular §3 framing.
    pub self_delimited: bool,
}

/// Split a multistream Opus packet into its `N` per-stream packets.
///
/// `stream_count` is `N` from the channel-mapping table; it MUST be
/// ≥ 1 (RFC 7845 §5.1.1 item 1 forbids zero). The first `N − 1` streams
/// are parsed with the Appendix-B self-delimited framing to find each
/// one's exact byte extent; the final stream is the undelimited
/// remainder.
///
/// Returns [`Error::MalformedPacket`] if `stream_count` is zero, if a
/// self-delimited sub-packet is malformed, or if the self-delimited
/// prefixes overrun the buffer leaving nothing for the final stream.
pub fn split_multistream_packet(
    packet: &[u8],
    stream_count: u8,
) -> Result<Vec<StreamPacket<'_>>, Error> {
    if stream_count == 0 {
        return Err(Error::MalformedPacket);
    }
    let n = stream_count as usize;
    let mut streams = Vec::with_capacity(n);
    let mut offset = 0usize;

    // The first N − 1 streams use self-delimited framing; consume each
    // one and advance past it.
    for _ in 0..(n - 1) {
        if offset >= packet.len() {
            // Ran out of bytes before reaching the final stream.
            return Err(Error::MalformedPacket);
        }
        let parsed = parse_self_delimited(&packet[offset..])?;
        let consumed = parsed.consumed;
        // `parse_self_delimited` guarantees `consumed ≥ 1` on success.
        streams.push(StreamPacket {
            bytes: &packet[offset..offset + consumed],
            self_delimited: true,
        });
        offset += consumed;
    }

    // The final stream is the undelimited remainder. RFC 7845 §3 +
    // RFC 6716 §3.4 R1: a zero-octet final Opus packet is malformed.
    if offset >= packet.len() {
        return Err(Error::MalformedPacket);
    }
    streams.push(StreamPacket {
        bytes: &packet[offset..],
        self_delimited: false,
    });

    Ok(streams)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toc::OpusTocByte;

    /// A minimal self-delimited code-0 packet: TOC byte | 1-byte length
    /// `len` | `len` frame bytes. `config` selects the TOC config so the
    /// duration is well-defined.
    fn sd_code0(config: u8, frame: &[u8]) -> Vec<u8> {
        let toc = (config << 3) & 0xF8; // s = 0 (mono), code 0
        let mut v = vec![toc];
        assert!(frame.len() < 252, "test helper only emits 1-byte lengths");
        v.push(frame.len() as u8);
        v.extend_from_slice(frame);
        v
    }

    /// A regular (undelimited) code-0 packet: TOC byte | frame bytes.
    fn regular_code0(config: u8, frame: &[u8]) -> Vec<u8> {
        let toc = (config << 3) & 0xF8;
        let mut v = vec![toc];
        v.extend_from_slice(frame);
        v
    }

    #[test]
    fn single_stream_is_whole_packet() {
        // N = 1 → no self-delimited prefixes; the whole packet is the
        // final (regular) stream.
        let pkt = regular_code0(1, &[1, 2, 3, 4]);
        let streams = split_multistream_packet(&pkt, 1).unwrap();
        assert_eq!(streams.len(), 1);
        assert!(!streams[0].self_delimited);
        assert_eq!(streams[0].bytes, pkt.as_slice());
    }

    #[test]
    fn two_streams_split_at_self_delim_boundary() {
        // N = 2: one self-delimited stream then one regular remainder.
        let s0 = sd_code0(1, &[0xAA, 0xBB]);
        let s1 = regular_code0(1, &[0xCC, 0xDD, 0xEE]);
        let mut pkt = s0.clone();
        pkt.extend_from_slice(&s1);
        let streams = split_multistream_packet(&pkt, 2).unwrap();
        assert_eq!(streams.len(), 2);
        assert!(streams[0].self_delimited);
        assert_eq!(streams[0].bytes, s0.as_slice());
        assert!(!streams[1].self_delimited);
        assert_eq!(streams[1].bytes, s1.as_slice());
        // The two halves recover their TOCs.
        assert_eq!(
            OpusTocByte::from_byte(streams[0].bytes[0]).frame_size_tenths_ms,
            OpusTocByte::from_byte(streams[1].bytes[0]).frame_size_tenths_ms
        );
    }

    #[test]
    fn four_streams_5_1_layout() {
        // A 5.1 layout has N = 4 (2 coupled + 2 mono). Build 3
        // self-delimited prefixes + 1 regular tail.
        let s0 = sd_code0(1, &[1]);
        let s1 = sd_code0(1, &[2, 2]);
        let s2 = sd_code0(1, &[3, 3, 3]);
        let s3 = regular_code0(1, &[4, 4, 4, 4]);
        let mut pkt = Vec::new();
        for s in [&s0, &s1, &s2] {
            pkt.extend_from_slice(s);
        }
        pkt.extend_from_slice(&s3);
        let streams = split_multistream_packet(&pkt, 4).unwrap();
        assert_eq!(streams.len(), 4);
        assert_eq!(streams[0].bytes, s0.as_slice());
        assert_eq!(streams[1].bytes, s1.as_slice());
        assert_eq!(streams[2].bytes, s2.as_slice());
        assert_eq!(streams[3].bytes, s3.as_slice());
        assert!(streams[3].bytes.starts_with(&[(1u8 << 3) & 0xF8]));
    }

    #[test]
    fn zero_stream_count_rejected() {
        assert_eq!(
            split_multistream_packet(&[0x08, 1, 2], 0),
            Err(Error::MalformedPacket)
        );
    }

    #[test]
    fn missing_final_stream_rejected() {
        // N = 2 but the self-delimited prefix consumes the whole buffer,
        // leaving nothing for the final regular stream.
        let s0 = sd_code0(1, &[0xAA, 0xBB]);
        assert_eq!(
            split_multistream_packet(&s0, 2),
            Err(Error::MalformedPacket)
        );
    }

    #[test]
    fn truncated_self_delim_prefix_rejected() {
        // A self-delimited length that runs off the end is malformed.
        let bad = vec![(1u8 << 3) & 0xF8, 200, 1, 2]; // claims 200 bytes
        assert_eq!(
            split_multistream_packet(&bad, 2),
            Err(Error::MalformedPacket)
        );
    }
}
