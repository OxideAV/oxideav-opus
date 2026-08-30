//! CELT-only Opus **packet** encoder — §3.1 TOC + one §4.3 CELT frame
//! per packet (code 0), at a caller-chosen constant payload size
//! (RFC 6716 §3 / §4.3 / §5.3).
//!
//! The packets decode end-to-end through the crate's own
//! [`crate::decoder::OpusDecoder`]; the encode→decode chain carries
//! the fixed 2.5 ms §4.3.7 MDCT-overlap delay
//! ([`crate::celt_analysis`]).
//!
//! ## Provenance
//!
//! RFC 6716 §3 / §5.3 + the normative Appendix A reference listing
//! (staged `docs/audio/opus/rfc6716-opus.txt`, hash-verified per
//! §A.1). No external library source was consulted.

use crate::celt_frame_encode::{encode_celt_frame, CeltEncoderState, CeltFrameEncodeInfo};
use crate::decoder::OpusDecoder;
use crate::range_encoder::RangeEncoder;
use crate::toc::{Bandwidth, FrameCountCode, Mode, OpusTocByte};
use crate::Error;

/// The §3.2 maximum Opus frame payload.
const MAX_FRAME_BYTES: usize = 1275;

/// The fixed encode→decode chain delay of the CELT path: the 2.5 ms
/// §4.3.7 MDCT-overlap delay at 48 kHz (the tapset election's
/// reference alignment).
const CELT_CHAIN_DELAY: usize = 120;

/// The §5.3.1 tapset election's carried machinery: a mirror decoder
/// in stream lockstep (every emitted packet is fed to it) plus the
/// delay-aligned input history the trial decodes are scored against.
#[derive(Debug, Clone)]
struct TapsetElection {
    mirror: OpusDecoder,
    /// Last [`CELT_CHAIN_DELAY`] input samples per channel
    /// (interleaved) — the reference head for the next frame.
    hist: Vec<i16>,
}

/// A CELT-only packet encoder for one stream configuration.
#[derive(Debug, Clone)]
pub struct CeltEncoder {
    state: CeltEncoderState,
    bandwidth: Bandwidth,
    frame_tenths_ms: u16,
    stereo: bool,
    end_band: usize,
    lm: i32,
    tapset_election: Option<TapsetElection>,
}

impl CeltEncoder {
    /// New CELT-only encoder. `bandwidth` selects the coded band range
    /// (NB→13, WB→17, SWB→19, FB→21; MB is not a CELT bandwidth) and
    /// `frame_tenths_ms` the frame duration (25/50/100/200 tenths of
    /// a millisecond).
    pub fn new(bandwidth: Bandwidth, frame_tenths_ms: u16, stereo: bool) -> Result<Self, Error> {
        let end_band = match bandwidth {
            Bandwidth::Nb => 13,
            Bandwidth::Wb => 17,
            Bandwidth::Swb => 19,
            Bandwidth::Fb => 21,
            Bandwidth::Mb => return Err(Error::MalformedPacket),
        };
        let lm = match frame_tenths_ms {
            25 => 0i32,
            50 => 1,
            100 => 2,
            200 => 3,
            _ => return Err(Error::MalformedPacket),
        };
        // Validate the TOC row exists up front.
        let _ = OpusTocByte::compose_byte(
            Mode::CeltOnly,
            bandwidth,
            frame_tenths_ms,
            stereo,
            FrameCountCode::One,
        )?;
        let n = 120usize << lm;
        let channels = if stereo { 2 } else { 1 };
        Ok(Self {
            state: CeltEncoderState::new(channels, n),
            bandwidth,
            frame_tenths_ms,
            stereo,
            end_band,
            lm,
            tapset_election: None,
        })
    }

    /// Samples per channel consumed by one packet (48 kHz).
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        self.state.frame_len()
    }

    /// The carried cross-frame CELT state (the §4.5.2 "CELT decoder
    /// state" mirror) — the unified encoder moves it across mode
    /// transitions so the state a §4.5.1 redundant frame warmed
    /// continues into the following CELT-only frames.
    pub(crate) fn celt_state_mut(&mut self) -> &mut CeltEncoderState {
        &mut self.state
    }

    /// Replace the carried CELT state (geometry re-applied to this
    /// encoder's frame size; the tapset-election mirror, when armed,
    /// re-arms from a fresh decoder — non-normative trial machinery).
    pub(crate) fn adopt_celt_state(&mut self, mut state: CeltEncoderState) {
        let channels = self.channels();
        state.set_geometry(channels, 120usize << self.lm);
        self.state = state;
        if self.tapset_election.is_some() {
            self.tapset_election = Some(Self::fresh_election(channels));
        }
    }

    /// Channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.state.channels()
    }

    /// Reset all carried state (stream start / §4.5.2).
    pub fn reset(&mut self) {
        self.state.reset();
        let channels = self.channels();
        if let Some(el) = &mut self.tapset_election {
            *el = Self::fresh_election(channels);
        }
    }

    /// Force the §4.3.7.1 tapset coded when the §5.3.1 pre-filter
    /// fires (0..=2; default 0). Ignored while the tapset election is
    /// enabled — the election overwrites it per frame.
    pub fn set_tapset(&mut self, tapset: u8) {
        self.state.tapset_request = tapset.min(2);
    }

    /// Complexity ladder (0..=10; RFC 6716 leaves encoder complexity
    /// free, so the rungs are documented crate choices): `0..=1`
    /// skips the §5.3.1 pitch pre-filter analysis entirely (the
    /// post-filter is signalled off); `2..=7` runs the pre-filter's
    /// full decision ladder (the untouched default — complexity 4);
    /// `8..=10` additionally arms the §5.3.1 tapset election (three
    /// trial encodes + mirror decodes per pre-filter-firing frame).
    /// Overrides an earlier [`Self::set_tapset_election`] call.
    pub fn set_complexity(&mut self, complexity: u8) {
        let c = complexity.min(10);
        self.state.prefilter_enabled = c >= 2;
        self.set_tapset_election(c >= 8);
    }

    /// Enable / disable the §5.3.1 **tapset election**: on every
    /// frame where the pitch pre-filter fires, the frame is
    /// trial-encoded with each §4.3.7.1 tapset (0/1/2) at the same
    /// payload size, each trial is decoded through a clone of a
    /// mirror decoder held in stream lockstep, and the tapset whose
    /// decode measures the best SNR against the (delay-aligned) input
    /// is committed — quality measured at equal rate, ties resolved
    /// toward tapset 0. Enable BEFORE the first packet (or right
    /// after [`Self::reset`]); enabling mid-stream re-arms the mirror
    /// from a fresh decoder, which only re-converges after the next
    /// stream reset.
    pub fn set_tapset_election(&mut self, enabled: bool) {
        self.tapset_election = enabled.then(|| Self::fresh_election(self.channels()));
    }

    /// The 1-byte TOC-only §2.1.9 DTX marker for this stream
    /// configuration (one §3.2.1 zero-length frame, code 0).
    pub(crate) fn dtx_marker(&self) -> Result<Vec<u8>, Error> {
        Ok(vec![OpusTocByte::compose_byte(
            Mode::CeltOnly,
            self.bandwidth,
            self.frame_tenths_ms,
            self.stereo,
            FrameCountCode::One,
        )?])
    }

    /// Force the next coded frame's §5.3.2 energies INTRA (the §2.1.9
    /// DTX resume treatment: after suppressed frames, a decoder's
    /// carried energy state is whatever its own concealment left, so
    /// the resume frame must not predict from it).
    pub(crate) fn force_intra_next(&mut self) {
        self.state.force_intra = true;
    }

    /// Frame duration in tenths of a millisecond.
    pub(crate) fn frame_tenths_ms(&self) -> u16 {
        self.frame_tenths_ms
    }

    fn fresh_election(channels: usize) -> TapsetElection {
        TapsetElection {
            mirror: OpusDecoder::new(),
            hist: vec![0i16; channels * CELT_CHAIN_DELAY],
        }
    }

    /// Encode one frame of interleaved 48 kHz PCM
    /// (`channels * frame_samples()` values) into a code-0 Opus packet
    /// of exactly `1 + payload_bytes` bytes.
    ///
    /// `payload_bytes` is the CELT frame budget (2..=1275); a constant
    /// value gives CBR transport.
    pub fn encode_packet(
        &mut self,
        pcm: &[i16],
        payload_bytes: usize,
    ) -> Result<(Vec<u8>, CeltFrameEncodeInfo), Error> {
        if pcm.len() != self.channels() * self.frame_samples() {
            return Err(Error::MalformedPacket);
        }
        if !(2..=MAX_FRAME_BYTES).contains(&payload_bytes) {
            return Err(Error::MalformedPacket);
        }
        let toc = OpusTocByte::compose_byte(
            Mode::CeltOnly,
            self.bandwidth,
            self.frame_tenths_ms,
            self.stereo,
            FrameCountCode::One,
        )?;
        if self.tapset_election.is_none() {
            let mut state = core::mem::replace(&mut self.state, CeltEncoderState::new(1, 120));
            let r = Self::encode_with_state(
                toc,
                &mut state,
                pcm,
                payload_bytes,
                self.end_band,
                self.lm,
            );
            self.state = state;
            return r;
        }
        self.encode_packet_tapset_elected(toc, pcm, payload_bytes)
    }

    /// One frame encode against an explicit state (the trial-encode
    /// primitive of the tapset election).
    fn encode_with_state(
        toc: u8,
        state: &mut CeltEncoderState,
        pcm: &[i16],
        payload_bytes: usize,
        end_band: usize,
        lm: i32,
    ) -> Result<(Vec<u8>, CeltFrameEncodeInfo), Error> {
        let mut enc = RangeEncoder::new();
        let info = encode_celt_frame(state, &mut enc, pcm, payload_bytes, 0, end_band, lm);
        debug_assert!(enc.tell() as usize <= payload_bytes * 8, "budget bust");
        let payload = enc
            .finish_fixed(payload_bytes)
            .ok_or(Error::MalformedPacket)?;
        let mut packet = Vec::with_capacity(1 + payload_bytes);
        packet.push(toc);
        packet.extend_from_slice(&payload);
        Ok((packet, info))
    }

    /// The §5.3.1 tapset election (see [`Self::set_tapset_election`]):
    /// trial-encode the frame per tapset, decode each trial on a clone
    /// of the lockstep mirror decoder, adopt the measured-SNR winner
    /// at the frame's fixed payload size, then advance the real
    /// mirror with the committed packet.
    fn encode_packet_tapset_elected(
        &mut self,
        toc: u8,
        pcm: &[i16],
        payload_bytes: usize,
    ) -> Result<(Vec<u8>, CeltFrameEncodeInfo), Error> {
        // Trial 0 also decides whether the pre-filter fires at all
        // (the pf decision does not depend on the tapset).
        let mut best_state = self.state.clone();
        best_state.tapset_request = 0;
        let (mut best_packet, mut best_info) = Self::encode_with_state(
            toc,
            &mut best_state,
            pcm,
            payload_bytes,
            self.end_band,
            self.lm,
        )?;

        if best_info.postfilter_on {
            let el = self.tapset_election.as_ref().expect("election armed");
            // Delay-aligned reference: the previous frame's tail plus
            // this frame's head (the chain's fixed 2.5 ms delay).
            let ch = self.channels();
            let n_i = ch * self.frame_samples();
            let mut reference: Vec<i16> = Vec::with_capacity(n_i);
            reference.extend_from_slice(&el.hist);
            reference.extend_from_slice(&pcm[..n_i - el.hist.len()]);

            let mut best_snr = Self::trial_snr(&el.mirror, &best_packet, &reference)?;
            for tapset in 1..=2u8 {
                let mut state = self.state.clone();
                state.tapset_request = tapset;
                let (packet, info) = Self::encode_with_state(
                    toc,
                    &mut state,
                    pcm,
                    payload_bytes,
                    self.end_band,
                    self.lm,
                )?;
                debug_assert!(info.postfilter_on, "pf decision is tapset-independent");
                let snr = Self::trial_snr(&el.mirror, &packet, &reference)?;
                if snr > best_snr {
                    best_snr = snr;
                    best_state = state;
                    best_packet = packet;
                    best_info = info;
                }
            }
        }

        // Commit the winner: adopt its state, advance the lockstep
        // mirror, roll the delay history.
        self.state = best_state;
        let el = self.tapset_election.as_mut().expect("election armed");
        let _ = el.mirror.decode_packet(&best_packet)?;
        let keep = el.hist.len();
        el.hist.clear();
        el.hist.extend_from_slice(&pcm[pcm.len() - keep..]);
        Ok((best_packet, best_info))
    }

    /// SNR of one trial packet's decode (on a clone of the lockstep
    /// mirror) against the delay-aligned input reference.
    fn trial_snr(mirror: &OpusDecoder, packet: &[u8], reference: &[i16]) -> Result<f64, Error> {
        let mut dec = mirror.clone();
        let out = dec.decode_packet(packet)?;
        if out.pcm.len() != reference.len() {
            return Err(Error::MalformedPacket);
        }
        let mut sig = 0.0f64;
        let mut err = 0.0f64;
        for (&r, &t) in reference.iter().zip(out.pcm.iter()) {
            let rf = f64::from(r);
            let tf = f64::from(t);
            sig += rf * rf;
            err += (rf - tf) * (rf - tf);
        }
        if err == 0.0 {
            return Ok(150.0);
        }
        Ok(10.0 * (sig / err).log10())
    }
}

/// Coded band count for a CELT layer at `bandwidth` (§4.3, Table 55
/// end band: NB 13 / WB 17 / SWB 19 / FB 21). MB is not a CELT
/// bandwidth; §4.5.1.4 maps an MB SILK carrier's redundant frame to
/// WB.
#[must_use]
pub fn celt_end_band(bandwidth: Bandwidth) -> usize {
    match bandwidth {
        Bandwidth::Nb => 13,
        Bandwidth::Mb | Bandwidth::Wb => 17,
        Bandwidth::Swb => 19,
        Bandwidth::Fb => 21,
    }
}

/// Samples per channel of the §4.5.1 redundant CELT frame (5 ms at
/// 48 kHz).
pub const REDUNDANT_FRAME_SAMPLES: usize = 240;

/// Smallest §4.5.1.3 redundant frame this crate emits. The §4.5.1.1
/// SILK-only signal needs at least 17 unused bits after the SILK
/// portion; the SILK range coder finalizes into exactly
/// `ceil(tell / 8)` bytes, so two appended bytes can leave exactly
/// 16 — three always clear the gate (and Hybrid's explicit size is
/// `2 + uint(256)`).
pub const REDUNDANT_FRAME_MIN_BYTES: usize = 3;

/// Largest redundant frame the Hybrid §4.5.1.3 size field can code
/// (`2 + 255`).
pub const REDUNDANT_FRAME_MAX_BYTES: usize = 257;

/// Encode one §4.5.1 **redundant CELT frame**: "decoded like any other
/// CELT-only frame, with the exception that it does not contain a
/// TOC byte. The frame size is fixed at 5 ms, the channel count is set
/// to that of the current frame, and the audio bandwidth is also set
/// to that of the current frame, with the exception that for MB SILK
/// frames, it is set to WB" (§4.5.1.4). It "does not use the same
/// entropy coder state as the rest of the Opus frame" (§4.5.1.3), so
/// it is finalized on its own coder into exactly `bytes` bytes.
///
/// `state` is the stream's carried CELT state: the caller places the
/// §4.5.2 reset (before an end-position frame — Figure 18's `!R`; not
/// before a beginning-position one) and the geometry is switched to
/// 5 ms for the frame and restored by the caller. `pcm` holds
/// `channels * 240` interleaved 48 kHz samples — the LAST 5 ms of the
/// carrier Opus frame's input for an end-position frame (only its
/// second half is used by the decoder: the first half of a
/// just-reset MDCT chain has no overlap partner), the FIRST 5 ms for
/// a beginning-position one.
pub fn encode_redundant_celt_frame(
    state: &mut CeltEncoderState,
    pcm: &[i16],
    carrier_bandwidth: Bandwidth,
    bytes: usize,
) -> Result<Vec<u8>, Error> {
    let channels = state.channels();
    if pcm.len() != channels * REDUNDANT_FRAME_SAMPLES
        || !(REDUNDANT_FRAME_MIN_BYTES..=MAX_FRAME_BYTES).contains(&bytes)
    {
        return Err(Error::MalformedPacket);
    }
    let end_band = celt_end_band(carrier_bandwidth);
    let n_before = state.frame_len();
    state.set_geometry(channels, REDUNDANT_FRAME_SAMPLES);
    let mut enc = RangeEncoder::new();
    let _info = encode_celt_frame(state, &mut enc, pcm, bytes, 0, end_band, 1);
    state.set_geometry(channels, n_before);
    enc.finish_fixed(bytes).ok_or(Error::MalformedPacket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_configs() {
        assert!(CeltEncoder::new(Bandwidth::Mb, 200, false).is_err());
        assert!(CeltEncoder::new(Bandwidth::Fb, 400, false).is_err());
        let mut e = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
        assert_eq!(e.frame_samples(), 960);
        let pcm = vec![0i16; 960];
        assert!(e.encode_packet(&pcm, 1).is_err());
        assert!(e.encode_packet(&pcm[..100], 100).is_err());
    }

    #[test]
    fn digital_silence_encodes_and_decodes_as_celt_silence() {
        let mut e = CeltEncoder::new(Bandwidth::Fb, 200, false).unwrap();
        let pcm = vec![0i16; 960];
        let (packet, info) = e.encode_packet(&pcm, 60).unwrap();
        assert!(info.silence);
        assert_eq!(packet.len(), 61);
        let mut dec = crate::decoder::OpusDecoder::new();
        let out = dec.decode_packet(&packet).unwrap();
        assert_eq!(out.samples_per_channel(), 960);
        assert!(out.pcm.iter().all(|&v| v == 0));
    }
}
