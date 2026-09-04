//! Unified streaming Opus encoder — RFC 6716 §2.1 knobs over the
//! crate's SILK-only / Hybrid / CELT-only packet arms, with the
//! **§4.5 configuration-switching machinery on the write side**:
//! bitrate-driven operating-mode and audio-bandwidth selection
//! (§2.1.1 / §2.1.3 — "by default, the reference encoder attempts to
//! make the best decision possible given the current bitrate"), and
//! the §4.5.3 Figure 18 **normative transitions with redundancy**:
//! an extra 5 ms redundant CELT frame is embedded in the last
//! SILK-only/Hybrid Opus frame before a transition to CELT (or a
//! SILK bandwidth / LP↔MDCT model change) and/or in the first one
//! after it, with the §4.5.2 encoder-side state placement mirroring
//! the decoder rule for rule:
//!
//! * one **CELT state** threads the whole stream: the CELT-only
//!   arm's, the Hybrid arm's, and every §4.5.1 redundant frame's —
//!   moved across arms at each switch so an end-position redundant
//!   frame's freshly-reset-then-warmed state (Figure 18 `!R`) is
//!   exactly what the following CELT-only / Hybrid frames continue
//!   from, and a beginning-position frame (`R`) continues the
//!   carried chain before the new mode's own reset (`|H`) lands;
//! * the **SILK analyzer state** carries across the WB SILK ↔ Hybrid
//!   transitions (which reset neither side per §4.5.2 rule 1) and is
//!   rebuilt fresh wherever the decoder resets or clears carries
//!   (from CELT-only, and on any SILK bandwidth change);
//! * transitions the encoder cannot make normative (a §2.1.9 DTX
//!   marker at the seam, redundancy disabled) fall back to the
//!   §4.5.3 Figure 19 recommended forms, which the decoder conceals.
//!
//! Because the §4.5.1 side information for a transition must ride in
//! the LAST old-configuration packet, configuration changes take
//! effect **one packet after** the knob moves: the packet coded when
//! a change is first observed is the transition carrier.
//!
//! With [`OpusEncoder::set_signal_adaptive`] the decision also takes
//! the §5 "type of signal (speech vs. music)" input from the crate's
//! own [`SignalAnalyser`]: the per-frame class selects a speech or a
//! music rate ladder under the application profile, and the analyser's
//! content-bandwidth estimate caps the coded bandwidth (§2.1.3 "the
//! best bandwidth decision possible given the current bitrate" — and
//! the content). Signal-driven changes are rate-limited to one per
//! [`SIGNAL_SWITCH_DWELL_MS`] on top of the analyser's own hysteresis
//! and go through exactly the same §4.5 transition machinery as
//! knob-driven ones, so every switch stays conformant.
//!
//! §2.1.4 / §3.2: 40 and 60 ms packets are native single frames on
//! the SILK-only arm and **code-3 multi-frame packets** of 20 ms frames
//! on the CELT-only and Hybrid arms (two or three frames sharing one
//! TOC; a transition's §4.5.1 redundancy rides in the first or last
//! frame of the packet exactly as it would in a 20 ms packet; hard CBR
//! pads the code-3 framing itself).
//!
//! The SILK-only arms consume internal-rate PCM, so this module owns
//! the 48 kHz → 8/12/16 kHz decimators; their tap counts put each
//! SILK-only chain (decimator group delay + the decoder's §4.2.9
//! Table 54 resampler delay + the §4.2.8 delay) on the same
//! 120-sample / 2.5 ms stream timeline as the CELT MDCT overlap and
//! the Hybrid arm, so every mode switch is time-aligned at the
//! decoder's output.
//!
//! No external library source was consulted: every rule here is
//! transcribed from RFC 6716 §2.1, §4.5.1–§4.5.3 and §5
//! (`docs/audio/opus/rfc6716-opus.txt`).

use crate::celt_frame_encode::CeltEncoderState;
use crate::celt_packet_encode::{
    encode_redundant_celt_frame, CeltEncoder, REDUNDANT_FRAME_MAX_BYTES, REDUNDANT_FRAME_MIN_BYTES,
    REDUNDANT_FRAME_SAMPLES,
};
use crate::celt_redundancy::{RedundancyDecision, RedundancyPosition};
use crate::framing::OperatingMode;
use crate::hybrid_packet_encode::{
    Decimator48, HybridEncoderMono, HybridEncoderStereo, RedundancyPlan,
};
use crate::mode_transition_reset::{decide_state_resets, CeltResetPlacement};
use crate::packet_compose::{compose_packet, compose_packet_code3, pad_packet_to};
use crate::signal_analysis::{SignalAnalyser, SignalClass, SignalVerdict};
use crate::silk_encoder::{DtxState, SilkEncoderMono, SilkEncoderStereo};
use crate::toc::{Bandwidth, FrameCountCode, Mode, OpusTocByte};
use crate::vbr::VbrRateControl;
use crate::Error;

/// §3.2.1 maximum Opus frame payload.
const MAX_FRAME_BYTES: usize = 1275;

/// Smallest electable code-0 packet (TOC + 2-byte CELT minimum).
const MIN_PACKET_BYTES: usize = 3;

/// Minimum spacing of signal-driven configuration changes (the
/// analyser's hysteresis already delays each decision by ~0.5 s; this
/// bounds the switch rate on content that keeps hovering at a class
/// boundary, e.g. speech over a music bed).
pub const SIGNAL_SWITCH_DWELL_MS: u32 = 1_500;

/// 48 kHz input history kept per channel for re-priming decimators at
/// a configuration switch (longest FIR is 177 taps).
const HIST48_SAMPLES: usize = 256;

/// Decimator tap counts per SILK bandwidth for the MONO SILK-only
/// arm: group delay `(taps - 1) / 2`, chosen so decimator + §4.2.9
/// resampler (measured against the reference-lineage upsampler:
/// NB ≈ 26, MB ≈ 33, WB ≈ 34–35 samples at 48 kHz) + the §4.2.8
/// one-sample internal delay (6 / 4 / 3 at 48 kHz) lands on the
/// 120-sample CELT timeline. WB uses the Hybrid arm's 165 taps so
/// the WB SILK ↔ Hybrid transitions share one input chain.
const fn mono_decim_taps(ratio: usize) -> usize {
    match ratio {
        6 => 177, // NB: 88 + 26 + 6 = 120
        4 => 167, // MB: 83 + 33 + 4 = 120
        _ => 165, // WB: 82 + 35 + 3 = 120
    }
}

/// Stereo SILK-only decimators are one internal sample shorter: the
/// §4.2.8 one-sample-lookahead hold (see [`SilkArmStereo`]) adds one
/// internal-rate sample of delay, repaid here.
const fn stereo_decim_taps(ratio: usize) -> usize {
    mono_decim_taps(ratio) - 2 * ratio
}

/// §2.1 application profile: steers the automatic §2.1.1/§2.1.3
/// mode-and-bandwidth decision (the split is this encoder's
/// documented freedom — RFC 6716 §2.1 leaves the policy to the
/// implementation and §2.1.1 pins the sweet spots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Application {
    /// Speech-leaning: SILK-only up to wideband speech rates, Hybrid
    /// through the §2.1.1 "28-40 kbit/s FB speech" band, CELT above.
    Voip,
    /// Music-leaning (the default): switches to Hybrid and CELT-only
    /// at lower rates than [`Self::Voip`].
    #[default]
    Audio,
    /// Always CELT-only (RFC 6716 §5's "restricted low-delay" demo
    /// mode): no SILK and no mode transitions.
    RestrictedLowDelay,
}

/// One resolved stream configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamConfig {
    mode: Mode,
    bandwidth: Bandwidth,
}

impl StreamConfig {
    fn operating_mode(self) -> OperatingMode {
        match self.mode {
            Mode::SilkOnly => OperatingMode::SilkOnly,
            Mode::Hybrid => OperatingMode::Hybrid,
            Mode::CeltOnly => OperatingMode::CeltOnly,
        }
    }

    /// The SILK layer's internal bandwidth, when the mode has one.
    fn silk_bandwidth(self) -> Option<Bandwidth> {
        match self.mode {
            Mode::SilkOnly => Some(self.bandwidth),
            Mode::Hybrid => Some(Bandwidth::Wb),
            Mode::CeltOnly => None,
        }
    }
}

/// The 48 kHz → internal-rate ratio for a SILK bandwidth.
fn silk_ratio(bw: Bandwidth) -> usize {
    match bw {
        Bandwidth::Nb => 6,
        Bandwidth::Mb => 4,
        _ => 3,
    }
}

/// Mono SILK-only arm: encoder + input decimator.
#[derive(Debug, Clone)]
struct SilkArmMono {
    enc: SilkEncoderMono,
    decim: Decimator48,
}

/// Stereo SILK-only arm. `held` is the previous packet's final
/// decimated L/R pair: each packet encodes `[held | out[..n-1]]` with
/// `next_lr = out[n-1]`, giving [`SilkEncoderStereo::encode_packet`]
/// its exact §4.2.8 one-sample lookahead at the cost of one
/// internal-rate sample of delay (repaid by the shorter decimator —
/// see [`stereo_decim_taps`]).
#[derive(Debug, Clone)]
struct SilkArmStereo {
    enc: SilkEncoderStereo,
    decim_l: Decimator48,
    decim_r: Decimator48,
    held: (f32, f32),
}

/// The active per-mode packet arm.
#[derive(Debug, Clone)]
enum Arm {
    SilkMono(Box<SilkArmMono>),
    SilkStereo(Box<SilkArmStereo>),
    HybridMono(Box<HybridEncoderMono>),
    HybridStereo(Box<HybridEncoderStereo>),
    Celt(Box<CeltEncoder>),
}

/// Unified streaming Opus encoder: 48 kHz interleaved S16 in, one
/// Opus packet per [`Self::frame_samples`] samples per channel out.
///
/// See the module docs for the §4.5 transition machinery. Every knob
/// below only affects the encoder (§2.1: "any impact they have on
/// the bitstream is signaled in-band").
#[derive(Debug, Clone)]
pub struct OpusEncoder {
    channels: usize,
    application: Application,
    frame_tenths_ms: u16,
    bitrate_bps: u32,
    forced_mode: Option<Mode>,
    forced_bandwidth: Option<Bandwidth>,
    vbr: bool,
    constrained_vbr: bool,
    dtx: bool,
    fec: bool,
    loss_perc: u8,
    complexity: u8,
    redundancy: bool,
    tapset_election: bool,

    rc: VbrRateControl,
    cur: Option<StreamConfig>,
    arm: Option<Arm>,
    /// The stream's carried CELT state while the active arm is
    /// SILK-only (no CELT layer of its own); `None` until something
    /// warms it (a CELT/Hybrid arm handing over, or an end-position
    /// redundant frame's reset-then-encode).
    celt_carry: Option<CeltEncoderState>,
    /// A §4.5.1.2 beginning-position redundant frame is due in the
    /// next packet (the first one of the new configuration).
    begin_r_pending: bool,
    /// Its §4.5.1.3 size, fixed at the switch (the richer seam side).
    begin_r_bytes: usize,
    /// §4.5.2: the new Hybrid configuration's main-layer CELT reset
    /// is deferred until after the beginning-position redundant frame
    /// is coded (Figure 18 `R & |H`).
    reset_celt_after_begin_r: bool,
    /// §2.1.9 DTX driver for the CELT-only arm (the SILK-bearing
    /// arms carry their own).
    celt_dtx: DtxState,
    /// Recent raw 48 kHz input per channel (planar, oldest first) for
    /// re-priming decimators at a switch.
    hist48: Vec<Vec<f64>>,
    /// The bitrate the PREVIOUS packet was coded at: §4.5.1 redundant
    /// frames around a rate-driven transition are sized from the
    /// richer side of the seam (a downward switch would otherwise
    /// starve the redundancy below the concealment it replaces).
    prev_bitrate_bps: u32,
    /// The §5 signal-type input (`None` = bitrate-only election).
    signal: Option<SignalAnalyser>,
    /// Packets coded since the last configuration change.
    packets_since_switch: u32,
    /// The previous packet's knob-only decision (signal-driven
    /// changes are the ones made while this stands still).
    prev_knob_target: Option<StreamConfig>,
    /// Configuration changes the analyser (not a knob) caused.
    signal_switches: u32,
    /// Hybrid SILK-layer share.
    hybrid_silk_share: f64,
}

impl OpusEncoder {
    /// New unified encoder: `channels` 1 or 2, at `bitrate_bps`
    /// (§2.1.1: 6 000..=510 000) with 20 ms packets.
    pub fn new(channels: usize, application: Application, bitrate_bps: u32) -> Result<Self, Error> {
        if !(1..=2).contains(&channels) {
            return Err(Error::MalformedPacket);
        }
        let bitrate_bps = bitrate_bps.clamp(6_000, 510_000);
        let frame_tenths_ms = 200;
        Ok(Self {
            channels,
            application,
            frame_tenths_ms,
            bitrate_bps,
            forced_mode: None,
            forced_bandwidth: None,
            vbr: true,
            constrained_vbr: false,
            dtx: false,
            fec: false,
            loss_perc: 0,
            complexity: 4,
            redundancy: true,
            tapset_election: false,
            rc: VbrRateControl::new(bitrate_bps, frame_tenths_ms, false)?,
            cur: None,
            arm: None,
            celt_carry: None,
            begin_r_pending: false,
            begin_r_bytes: 0,
            reset_celt_after_begin_r: false,
            celt_dtx: DtxState::default(),
            hist48: vec![Vec::new(); channels],
            prev_bitrate_bps: bitrate_bps,
            signal: None,
            packets_since_switch: 0,
            prev_knob_target: None,
            signal_switches: 0,
            hybrid_silk_share: crate::hybrid_packet_encode::HYBRID_SILK_SHARE,
        })
    }

    /// Share of a Hybrid packet's elected payload the WB SILK layer
    /// targets (see [`crate::hybrid_packet_encode::HYBRID_SILK_SHARE`]).
    pub fn set_hybrid_silk_share(&mut self, share: f64) {
        self.hybrid_silk_share = share.clamp(0.3, 0.9);
        match self.arm.as_mut() {
            Some(Arm::HybridMono(h)) => h.set_silk_share(self.hybrid_silk_share),
            Some(Arm::HybridStereo(h)) => h.set_silk_share(self.hybrid_silk_share),
            _ => {}
        }
    }

    /// Enable the signal-adaptive election (§5 "type of signal"):
    /// the encoder's own analyser classifies the input as speech or
    /// music and estimates its content bandwidth per frame, and the
    /// automatic mode / bandwidth decision follows (forced mode and
    /// bandwidth knobs still win; `RestrictedLowDelay` stays
    /// CELT-only). Off by default.
    pub fn set_signal_adaptive(&mut self, enabled: bool) {
        match (enabled, self.signal.is_some()) {
            (true, false) => self.signal = Some(SignalAnalyser::new(self.channels)),
            (false, true) => self.signal = None,
            _ => {}
        }
    }

    /// Whether the signal-adaptive election is on.
    #[must_use]
    pub fn signal_adaptive(&self) -> bool {
        self.signal.is_some()
    }

    /// The analyser's current verdict (class, probability, content
    /// bandwidth, features) when the adaptive election is on.
    #[must_use]
    pub fn signal_verdict(&self) -> Option<SignalVerdict> {
        self.signal.as_ref().map(SignalAnalyser::verdict)
    }

    /// Configuration changes the signal analyser caused so far.
    #[must_use]
    pub fn signal_switches(&self) -> u32 {
        self.signal_switches
    }

    /// Samples **per channel** consumed by one [`Self::encode_frame`].
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        usize::from(self.frame_tenths_ms) * 48 / 10
    }

    /// Channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// §2.1.1 target bitrate (clamped to 6 000..=510 000 b/s; takes
    /// effect from the next packet — with the §4.5 one-packet
    /// transition latency when it moves the mode/bandwidth decision).
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        self.bitrate_bps = bitrate_bps.clamp(6_000, 510_000);
        self.rc =
            VbrRateControl::new(self.bitrate_bps, self.frame_tenths_ms, self.constrained_vbr)?;
        Ok(())
    }

    /// §2.1.4 packet duration in tenths of a millisecond (25 / 50 /
    /// 100 / 200 / 400 / 600). 2.5 / 5 ms frames exist only in
    /// CELT-only mode, so they force it; 40 / 60 ms packets are single
    /// SILK frames on the SILK-only arm and §3.2 code-3 packets of two
    /// / three 20 ms frames on the CELT-only and Hybrid arms.
    pub fn set_frame_tenths_ms(&mut self, tenths: u16) -> Result<(), Error> {
        if !matches!(tenths, 25 | 50 | 100 | 200 | 400 | 600) {
            return Err(Error::MalformedPacket);
        }
        if matches!(tenths, 25 | 50)
            && matches!(self.forced_mode, Some(Mode::SilkOnly | Mode::Hybrid))
        {
            return Err(Error::MalformedPacket);
        }
        self.frame_tenths_ms = tenths;
        self.rc = VbrRateControl::new(self.bitrate_bps, tenths, self.constrained_vbr)?;
        Ok(())
    }

    /// Force the §3.1 operating mode (`None` = automatic §2.1.1
    /// selection). Must be compatible with the frame duration (see
    /// [`Self::set_frame_tenths_ms`]).
    pub fn set_mode(&mut self, mode: Option<Mode>) -> Result<(), Error> {
        if matches!(mode, Some(Mode::SilkOnly | Mode::Hybrid))
            && matches!(self.frame_tenths_ms, 25 | 50)
        {
            return Err(Error::MalformedPacket);
        }
        self.forced_mode = mode;
        Ok(())
    }

    /// Force the §2.1.3 audio bandwidth (`None` = automatic). The
    /// resolved mode clamps it to a codable one (SILK-only tops out
    /// at WB, Hybrid needs SWB/FB, CELT-only has no MB).
    pub fn set_bandwidth(&mut self, bandwidth: Option<Bandwidth>) {
        self.forced_bandwidth = bandwidth;
    }

    /// §2.1.8 VBR (the default) vs. hard CBR (§3.2.5 code-3 padding
    /// to the exact per-packet byte target).
    pub fn set_vbr(&mut self, vbr: bool) {
        self.vbr = vbr;
    }

    /// §2.1.8 constrained VBR (bit-reservoir discipline).
    pub fn set_constrained_vbr(&mut self, constrained: bool) -> Result<(), Error> {
        self.constrained_vbr = constrained;
        self.rc = VbrRateControl::new(self.bitrate_bps, self.frame_tenths_ms, constrained)?;
        Ok(())
    }

    /// §2.1.9 discontinuous transmission on every arm.
    pub fn set_dtx(&mut self, dtx: bool) {
        self.dtx = dtx;
        self.celt_dtx.enabled = dtx;
        self.celt_dtx.reset();
        match self.arm.as_mut() {
            Some(Arm::SilkMono(a)) => a.enc.set_dtx(dtx),
            Some(Arm::SilkStereo(a)) => a.enc.set_dtx(dtx),
            Some(Arm::HybridMono(h)) => h.set_dtx(dtx),
            Some(Arm::HybridStereo(h)) => h.set_dtx(dtx),
            Some(Arm::Celt(_)) | None => {}
        }
    }

    /// §2.1.7 in-band FEC (§4.2.5 LBRR) on the SILK-bearing arms.
    pub fn set_fec(&mut self, fec: bool) {
        self.fec = fec;
        match self.arm.as_mut() {
            Some(Arm::SilkMono(a)) => a.enc.set_fec(fec),
            Some(Arm::SilkStereo(a)) => a.enc.set_fec(fec),
            Some(Arm::HybridMono(h)) => h.set_fec(fec),
            Some(Arm::HybridStereo(h)) => h.set_fec(fec),
            Some(Arm::Celt(_)) | None => {}
        }
    }

    /// §2.1.7 expected packet-loss percentage (shapes LBRR).
    pub fn set_packet_loss_perc(&mut self, loss_perc: u8) {
        self.loss_perc = loss_perc.min(100);
        match self.arm.as_mut() {
            Some(Arm::SilkMono(a)) => a.enc.set_packet_loss_perc(self.loss_perc),
            Some(Arm::SilkStereo(a)) => a.enc.set_packet_loss_perc(self.loss_perc),
            Some(Arm::HybridMono(h)) => h.set_packet_loss_perc(self.loss_perc),
            Some(Arm::HybridStereo(h)) => h.set_packet_loss_perc(self.loss_perc),
            Some(Arm::Celt(_)) | None => {}
        }
    }

    /// §2.1.5 complexity rung (0..=10) on every arm.
    pub fn set_complexity(&mut self, complexity: u8) {
        self.complexity = complexity.min(10);
        match self.arm.as_mut() {
            Some(Arm::SilkMono(a)) => a.enc.set_complexity(self.complexity),
            Some(Arm::SilkStereo(a)) => a.enc.set_complexity(self.complexity),
            Some(Arm::HybridMono(h)) => h.set_complexity(self.complexity),
            Some(Arm::HybridStereo(h)) => h.set_complexity(self.complexity),
            Some(Arm::Celt(c)) => c.set_complexity(self.complexity),
            None => {}
        }
    }

    /// §5.3.1 tapset election on the CELT-only arm (three trial
    /// encodes + mirror decodes per pre-filter-firing frame; carried
    /// across mode switches and re-applied whenever the CELT arm is
    /// rebuilt).
    pub fn set_tapset_election(&mut self, enabled: bool) {
        self.tapset_election = enabled;
        if let Some(Arm::Celt(c)) = self.arm.as_mut() {
            c.set_tapset_election(enabled);
        }
    }

    /// Enable / disable the §4.5.1 transition side information
    /// (default on). Off, configuration switches use the §4.5.3
    /// Figure 19 non-normative forms (decoder-side concealment fills
    /// the seam).
    pub fn set_transition_redundancy(&mut self, redundancy: bool) {
        self.redundancy = redundancy;
    }

    /// The §4.5.1.3 redundant-frame size: about 5 ms worth of the
    /// richer of the current and previous packets' target bitrates
    /// (see [`Self::prev_bitrate_bps`]), in the codable range.
    fn redundant_frame_bytes(&self) -> usize {
        (self.bitrate_bps.max(self.prev_bitrate_bps) as usize / 1600)
            .clamp(REDUNDANT_FRAME_MIN_BYTES, REDUNDANT_FRAME_MAX_BYTES)
    }

    /// Resolve the §2.1.1 / §2.1.3 configuration for the current
    /// knobs (see [`Application`]) and, when given, the analyser's
    /// verdict. Pure in its inputs: the decision only moves when a
    /// knob or the verdict does.
    fn decide(&self, verdict: Option<SignalVerdict>) -> StreamConfig {
        // Stereo spends bits on the second channel; compare on an
        // effective per-stream rate so stereo switches up later.
        let eff = if self.channels == 2 {
            self.bitrate_bps.saturating_mul(2) / 3
        } else {
            self.bitrate_bps
        };
        let class = verdict.map_or(SignalClass::Unknown, |v| v.class);
        // The content-bandwidth cap applies once the analyser has
        // decided a class (its hold-down memory has then heard ≥ 0.8 s
        // of active input); before that a stream opening on a
        // narrowband onset would otherwise start capped and pay a §4.5
        // bandwidth transition a few packets in.
        let cap = verdict
            .filter(|v| v.class != SignalClass::Unknown)
            .map_or(Bandwidth::Fb, |v| v.bandwidth);
        let celt_only_duration = matches!(self.frame_tenths_ms, 25 | 50);
        let mut mode = self.forced_mode.unwrap_or_else(|| {
            if celt_only_duration || self.application == Application::RestrictedLowDelay {
                Mode::CeltOnly
            } else {
                let (hybrid_at, celt_at) = Self::rate_ladder(self.application, class);
                if eff < hybrid_at {
                    Mode::SilkOnly
                } else if eff < celt_at {
                    Mode::Hybrid
                } else {
                    Mode::CeltOnly
                }
            }
        });
        // Hybrid codes only what lies above 8 kHz in its CELT layer:
        // content capped at WB or below has nothing for it, so the
        // automatic decision falls back to the WB arm of the class.
        if mode == Mode::Hybrid
            && self.forced_mode.is_none()
            && matches!(cap, Bandwidth::Nb | Bandwidth::Mb | Bandwidth::Wb)
        {
            mode = if class == SignalClass::Music {
                Mode::CeltOnly
            } else {
                Mode::SilkOnly
            };
        }
        let bandwidth = match mode {
            Mode::SilkOnly => match self.forced_bandwidth {
                Some(Bandwidth::Nb) => Bandwidth::Nb,
                Some(Bandwidth::Mb) => Bandwidth::Mb,
                Some(_) => Bandwidth::Wb,
                None => {
                    let ladder = if eff < 12_000 {
                        Bandwidth::Nb
                    } else if eff < 16_000 {
                        Bandwidth::Mb
                    } else {
                        Bandwidth::Wb
                    };
                    match cap {
                        Bandwidth::Nb => Bandwidth::Nb,
                        Bandwidth::Mb if ladder != Bandwidth::Nb => Bandwidth::Mb,
                        _ => ladder,
                    }
                }
            },
            Mode::Hybrid => match self.forced_bandwidth {
                Some(Bandwidth::Fb) => Bandwidth::Fb,
                Some(_) => Bandwidth::Swb,
                None => {
                    if eff < 32_000 || cap == Bandwidth::Swb {
                        Bandwidth::Swb
                    } else {
                        Bandwidth::Fb
                    }
                }
            },
            Mode::CeltOnly => match self.forced_bandwidth {
                Some(Bandwidth::Nb) => Bandwidth::Nb,
                Some(Bandwidth::Mb) | Some(Bandwidth::Wb) => Bandwidth::Wb,
                Some(bw) => bw,
                None => {
                    let ladder = if eff < 12_000 {
                        Bandwidth::Nb
                    } else if eff < 24_000 {
                        Bandwidth::Wb
                    } else if eff < 40_000 {
                        Bandwidth::Swb
                    } else {
                        Bandwidth::Fb
                    };
                    // CELT has no MB row: MB content codes as WB.
                    let cap = if cap == Bandwidth::Mb {
                        Bandwidth::Wb
                    } else {
                        cap
                    };
                    if Self::bw_rank(cap) < Self::bw_rank(ladder) {
                        cap
                    } else {
                        ladder
                    }
                }
            },
        };
        StreamConfig { mode, bandwidth }
    }

    /// The (Hybrid-from, CELT-from) effective-rate thresholds per
    /// application and signal class.
    ///
    /// Speech (and an undecided signal) follows §2.1.1's sweet spots —
    /// the LP layer through the WB speech band, Hybrid across the
    /// "28–40 kbit/s FB speech" band, CELT above — with the Voip
    /// profile holding the LP layer longer. Music takes the MDCT layer
    /// from 12 kb/s up (§2: "the MDCT layer should be used for music
    /// signals"): measured on the corpus of
    /// `tests/signal_adaptive_election.rs`, CELT-only beats both the
    /// SILK-only and the Hybrid arm on music, tones and speech-over-
    /// music at every rate from 12 kb/s (e.g. 24 kb/s music: LSD 7.9 dB
    /// Hybrid → 4.8 dB CELT; tones: 16.6 → 5.5 dB), and Hybrid never
    /// beats CELT-only on music, so the music ladder has no Hybrid
    /// rung.
    fn rate_ladder(application: Application, class: SignalClass) -> (u32, u32) {
        match (application, class) {
            (Application::RestrictedLowDelay, _) => (0, 0),
            (_, SignalClass::Music) => (12_000, 12_000),
            (Application::Voip, _) => (24_000, 48_000),
            (Application::Audio, _) => (20_000, 32_000),
        }
    }

    fn bw_rank(bw: Bandwidth) -> u8 {
        match bw {
            Bandwidth::Nb => 0,
            Bandwidth::Mb => 1,
            Bandwidth::Wb => 2,
            Bandwidth::Swb => 3,
            Bandwidth::Fb => 4,
        }
    }

    /// §4.5.3: does the (old → new) transition carry an END-position
    /// redundant frame in the last old-configuration packet?
    fn end_r_applies(old: StreamConfig, new: StreamConfig) -> bool {
        match (old.mode, new.mode) {
            // "SILK to SILK with Redundancy" — a bandwidth change.
            (Mode::SilkOnly, Mode::SilkOnly) => old.bandwidth != new.bandwidth,
            // "NB or MB SILK to Hybrid with Redundancy" ("WB SILK to
            // Hybrid" carries none).
            (Mode::SilkOnly, Mode::Hybrid) => old.bandwidth != Bandwidth::Wb,
            // "SILK to CELT with Redundancy" / "Hybrid to CELT with
            // Redundancy".
            (Mode::SilkOnly | Mode::Hybrid, Mode::CeltOnly) => true,
            // "Hybrid to NB or MB SILK with Redundancy" ("Hybrid to
            // WB SILK" uses the overlap flush instead).
            (Mode::Hybrid, Mode::SilkOnly) => new.bandwidth != Bandwidth::Wb,
            _ => false,
        }
    }

    /// §4.5.3: does the (old → new) transition carry a
    /// BEGINNING-position redundant frame in the first
    /// new-configuration packet?
    fn begin_r_applies(old: StreamConfig, new: StreamConfig) -> bool {
        match (old.mode, new.mode) {
            (Mode::SilkOnly, Mode::SilkOnly) => old.bandwidth != new.bandwidth,
            (Mode::Hybrid, Mode::SilkOnly) => new.bandwidth != Bandwidth::Wb,
            // "CELT to SILK with Redundancy" / "CELT to Hybrid with
            // Redundancy".
            (Mode::CeltOnly, Mode::SilkOnly | Mode::Hybrid) => true,
            _ => false,
        }
    }

    /// The Opus-frame duration the arm for `mode` codes: the packet
    /// duration, except that the CELT-only and Hybrid arms code 20 ms
    /// frames and a 40 / 60 ms packet holds two / three of them.
    fn arm_tenths(&self, mode: Mode) -> u16 {
        match mode {
            Mode::SilkOnly => self.frame_tenths_ms,
            Mode::Hybrid | Mode::CeltOnly => self.frame_tenths_ms.min(200),
        }
    }

    /// Opus frames per packet for `mode` (1, 2 or 3).
    fn sub_frames(&self, mode: Mode) -> usize {
        usize::from(self.frame_tenths_ms / self.arm_tenths(mode))
    }

    /// The elected total packet size (TOC included) for this frame.
    fn elect_packet_bytes(&self) -> usize {
        if self.vbr {
            self.rc.elect_packet_bytes(0.0)
        } else {
            self.cbr_packet_bytes()
        }
    }

    /// Hard-CBR per-packet byte budget.
    fn cbr_packet_bytes(&self) -> usize {
        let bytes = (self.bitrate_bps as usize * usize::from(self.frame_tenths_ms)) / (8 * 10_000);
        bytes.clamp(MIN_PACKET_BYTES, 1 + MAX_FRAME_BYTES)
    }

    /// Encode one frame of interleaved 48 kHz S16 PCM
    /// (`channels() * frame_samples()` values) into one Opus packet.
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<u8>, Error> {
        if pcm.len() != self.channels * self.frame_samples() {
            return Err(Error::MalformedPacket);
        }
        let verdict = self.signal.as_mut().map(|a| a.analyse(pcm));
        let knob_target = self.decide(None);
        let mut target = self.decide(verdict);
        if self.cur.is_none() {
            self.install_fresh(target)?;
            self.cur = Some(target);
        }
        let cur = self.cur.expect("installed above");
        // A purely signal-driven change (the knob decision did not
        // move since the previous packet) waits out the dwell in both
        // directions; a knob move always goes through (to the
        // adaptive target).
        let signal_driven = target != cur && self.prev_knob_target == Some(knob_target);
        self.prev_knob_target = Some(knob_target);
        if signal_driven {
            // A bandwidth RAISE in the same mode goes through at once
            // (content above the coded band is lost until it does);
            // everything else waits.
            let raise = target.mode == cur.mode
                && Self::bw_rank(target.bandwidth) > Self::bw_rank(cur.bandwidth);
            let dwell = SIGNAL_SWITCH_DWELL_MS * 10 / u32::from(self.frame_tenths_ms);
            if !raise && self.packets_since_switch < dwell {
                target = cur;
            }
        }

        // §4.5.1: plan an end-position redundant frame when this is
        // the last packet before a transition that carries one (a
        // pending beginning-position frame defers any further
        // transition by one packet — §4.5.1.2: "There is no way to
        // specify that an Opus frame contains separate redundant CELT
        // frames at both the beginning and the end").
        let switching = target != cur && !self.begin_r_pending;
        let plan_end = switching && self.redundancy && Self::end_r_applies(cur, target);

        let elected = self.elect_packet_bytes();
        let (packet, end_r_carried, red_extra, marker) =
            self.encode_in_current(pcm, cur, elected, plan_end)?;
        // §4.5.3: the transition side information is "the extra
        // bitrate required for redundancy" — charged on top of the
        // election, not to the drift ledger (repaying ~5 ms of CELT
        // out of a low-rate SILK leg's следующие packets would starve
        // the primary encoding right where the seam needs it most).
        self.rc.commit(packet.len().saturating_sub(red_extra));

        // Roll the raw-input history (planar per channel).
        for (c, hist) in self.hist48.iter_mut().enumerate() {
            hist.extend(
                pcm.iter()
                    .skip(c)
                    .step_by(self.channels)
                    .map(|&v| f64::from(v)),
            );
            let len = hist.len();
            if len > HIST48_SAMPLES {
                hist.drain(..len - HIST48_SAMPLES);
            }
        }

        if switching {
            self.switch_to(cur, target, end_r_carried)?;
            self.cur = Some(target);
            self.packets_since_switch = 0;
            if signal_driven {
                self.signal_switches += 1;
            }
        } else {
            self.packets_since_switch = self.packets_since_switch.saturating_add(1);
        }
        self.prev_bitrate_bps = self.bitrate_bps;

        // Hard CBR: §3.2.5 code-3 padding to the exact target (§2.1.9
        // DTX markers stay at their framing minimum — suppressing the
        // payload is DTX's point). A floor-raised packet larger than
        // the target stands.
        if !self.vbr && !marker && packet.len() < self.cbr_packet_bytes() {
            let target = self.cbr_packet_bytes();
            if self.sub_frames(cur.mode) == 1 {
                return pad_packet_to(&packet, target);
            }
            return Self::pad_code3_to(&packet, target);
        }
        Ok(packet)
    }

    /// Pad an already code-3 multi-frame packet to exactly
    /// `target_len` bytes (re-composed with the §3.2.5 padding chain;
    /// the frame bytes are unchanged).
    fn pad_code3_to(packet: &[u8], target_len: usize) -> Result<Vec<u8>, Error> {
        let parsed = crate::frames::OpusPacket::parse(packet)?;
        let frames: Vec<&[u8]> = parsed.frames().to_vec();
        let vbr = frames.iter().any(|f| f.len() != frames[0].len());
        let toc = packet[0];
        // The chain header grows by one byte per 254 bytes of padding:
        // search the few candidate counts around the shortfall.
        let base = compose_packet_code3(toc, &frames, vbr, 0)?.len();
        let need = target_len.saturating_sub(base);
        for padding in (need.saturating_sub(8)..=need).rev() {
            let out = compose_packet_code3(toc, &frames, vbr, padding)?;
            if out.len() == target_len {
                return Ok(out);
            }
        }
        compose_packet_code3(toc, &frames, vbr, need)
    }

    /// Build a cold-start arm for `config` (fresh states everywhere).
    fn install_fresh(&mut self, config: StreamConfig) -> Result<(), Error> {
        self.arm = Some(self.build_arm(config, None, None)?);
        Ok(())
    }

    /// Construct the arm for `config`, adopting `celt_state` (else
    /// fresh) and, for SILK-bearing arms, `silk_from` — the previous
    /// arm when its SILK front end must carry over (WB SILK ↔ Hybrid).
    fn build_arm(
        &mut self,
        config: StreamConfig,
        celt_state: Option<CeltEncoderState>,
        silk_from: Option<Arm>,
    ) -> Result<Arm, Error> {
        let stereo = self.channels == 2;
        let arm = match config.mode {
            Mode::CeltOnly => {
                let mut enc =
                    CeltEncoder::new(config.bandwidth, self.arm_tenths(config.mode), stereo)?;
                enc.set_complexity(self.complexity);
                if self.tapset_election {
                    enc.set_tapset_election(true);
                }
                if let Some(state) = celt_state {
                    enc.adopt_celt_state(state);
                }
                Arm::Celt(Box::new(enc))
            }
            Mode::Hybrid => {
                if stereo {
                    let mut h =
                        HybridEncoderStereo::new(config.bandwidth, self.arm_tenths(config.mode))?;
                    h.set_silk_share(self.hybrid_silk_share);
                    h.set_dtx(self.dtx);
                    h.set_fec(self.fec);
                    h.set_packet_loss_perc(self.loss_perc);
                    h.set_complexity(self.complexity);
                    if let Some(state) = celt_state {
                        h.adopt_celt_state(state);
                    }
                    h.prime_decimators(&self.hist48[0], &self.hist48[1]);
                    if let Some(Arm::SilkStereo(mut prev)) = silk_from {
                        let (pm, ps, pd, pp) = prev.enc.stereo_parts_mut();
                        let (m, s2, d, p) = h.silk_stereo_parts_mut();
                        *m = pm.clone();
                        *s2 = ps.clone();
                        *d = *pd;
                        *p = *pp;
                        h.drop_pending_fec();
                    }
                    Arm::HybridStereo(Box::new(h))
                } else {
                    let mut h =
                        HybridEncoderMono::new(config.bandwidth, self.arm_tenths(config.mode))?;
                    h.set_silk_share(self.hybrid_silk_share);
                    h.set_dtx(self.dtx);
                    h.set_fec(self.fec);
                    h.set_packet_loss_perc(self.loss_perc);
                    h.set_complexity(self.complexity);
                    if let Some(state) = celt_state {
                        h.adopt_celt_state(state);
                    }
                    h.prime_decimator(&self.hist48[0]);
                    if let Some(Arm::SilkMono(mut prev)) = silk_from {
                        *h.silk_analyzer_mut() = prev.enc.analyzer_mut().clone();
                        h.drop_pending_fec();
                    }
                    Arm::HybridMono(Box::new(h))
                }
            }
            Mode::SilkOnly => {
                self.celt_carry = celt_state;
                let ratio = silk_ratio(config.bandwidth);
                if stereo {
                    let mut enc = SilkEncoderStereo::with_packet_duration(
                        config.bandwidth,
                        self.frame_tenths_ms,
                    )?;
                    enc.set_dtx(self.dtx);
                    enc.set_fec(self.fec);
                    enc.set_packet_loss_perc(self.loss_perc);
                    enc.set_complexity(self.complexity);
                    if let Some(Arm::HybridStereo(mut prev)) = silk_from {
                        let (pm, ps, pd, pp) = prev.silk_stereo_parts_mut();
                        let (m, s2, d, p) = enc.stereo_parts_mut();
                        *m = pm.clone();
                        *s2 = ps.clone();
                        *d = *pd;
                        *p = *pp;
                        enc.drop_pending_fec();
                    }
                    let mut decim_l = Decimator48::with_taps(ratio, stereo_decim_taps(ratio));
                    let mut decim_r = Decimator48::with_taps(ratio, stereo_decim_taps(ratio));
                    decim_l.prime(&self.hist48[0]);
                    decim_r.prime(&self.hist48[1]);
                    let held = (
                        decim_l.sample_before(&self.hist48[0]),
                        decim_r.sample_before(&self.hist48[1]),
                    );
                    Arm::SilkStereo(Box::new(SilkArmStereo {
                        enc,
                        decim_l,
                        decim_r,
                        held,
                    }))
                } else {
                    let mut enc = SilkEncoderMono::with_packet_duration(
                        config.bandwidth,
                        self.frame_tenths_ms,
                    )?;
                    enc.set_dtx(self.dtx);
                    enc.set_fec(self.fec);
                    enc.set_packet_loss_perc(self.loss_perc);
                    enc.set_complexity(self.complexity);
                    if let Some(Arm::HybridMono(mut prev)) = silk_from {
                        *enc.analyzer_mut() = prev.silk_analyzer_mut().clone();
                        enc.drop_pending_fec();
                    }
                    let mut decim = Decimator48::with_taps(ratio, mono_decim_taps(ratio));
                    decim.prime(&self.hist48[0]);
                    Arm::SilkMono(Box::new(SilkArmMono { enc, decim }))
                }
            }
        };
        Ok(arm)
    }

    /// Encode one packet through the current arm, embedding the
    /// planned §4.5.1 side information. Returns the packet, whether
    /// an END-position redundant frame was actually carried (a
    /// §2.1.9 DTX marker carries none), the redundant bytes appended
    /// (kept out of the rate-control ledger), and whether the packet
    /// is a DTX marker (every frame zero-length).
    ///
    /// A 40 / 60 ms packet on the CELT-only or Hybrid arm is two /
    /// three 20 ms Opus frames in one §3.2 code-3 packet: the
    /// beginning-position redundancy belongs to the first frame, the
    /// end-position one to the last, and the election is split evenly
    /// after the framing bytes.
    fn encode_in_current(
        &mut self,
        pcm: &[i16],
        cur: StreamConfig,
        elected: usize,
        plan_end: bool,
    ) -> Result<(Vec<u8>, bool, usize, bool), Error> {
        let m = self.sub_frames(cur.mode);
        if m == 1 {
            let (packet, end_carried, red) = self.encode_one(pcm, cur, elected, plan_end, true)?;
            let marker = packet.len() == 1;
            return Ok((packet, end_carried, red, marker));
        }
        let ch = self.channels;
        let sub = usize::from(self.arm_tenths(cur.mode)) * 48 / 10 * ch;
        // Framing: TOC + count byte + up to two length bytes per
        // non-final frame; each frame's own election keeps a TOC byte
        // of headroom that the composer strips.
        let framing = 2 + (m - 1) * 2;
        let per_frame = elected
            .saturating_sub(framing)
            .div_ceil(m)
            .max(MIN_PACKET_BYTES);
        let mut frames: Vec<Vec<u8>> = Vec::with_capacity(m);
        let mut end_carried = false;
        let mut red_total = 0usize;
        for k in 0..m {
            let last = k + 1 == m;
            let (pkt, end_c, red) = self.encode_one(
                &pcm[k * sub..(k + 1) * sub],
                cur,
                per_frame,
                plan_end && last,
                k == 0,
            )?;
            end_carried |= end_c;
            red_total += red;
            frames.push(pkt[1..].to_vec());
        }
        let marker = frames.iter().all(Vec::is_empty);
        let toc = OpusTocByte::compose_byte(
            cur.mode,
            cur.bandwidth,
            self.arm_tenths(cur.mode),
            ch == 2,
            FrameCountCode::Arbitrary,
        )?;
        let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
        let packet = compose_packet(toc, &refs)?;
        Ok((packet, end_carried, red_total, marker))
    }

    /// One Opus frame through the current arm (see
    /// [`Self::encode_in_current`]); `first` frames of a packet take
    /// the pending beginning-position redundancy.
    fn encode_one(
        &mut self,
        pcm: &[i16],
        cur: StreamConfig,
        elected: usize,
        plan_end: bool,
        first: bool,
    ) -> Result<(Vec<u8>, bool, usize), Error> {
        let begin_r = first && std::mem::take(&mut self.begin_r_pending);
        let red_bytes = if begin_r {
            self.begin_r_bytes
        } else {
            self.redundant_frame_bytes()
        };
        let reset_after_begin = first && std::mem::take(&mut self.reset_celt_after_begin_r);
        let ch = self.channels;
        let mut arm = self.arm.take().expect("arm installed");
        let result = (|| -> Result<(Vec<u8>, bool, usize), Error> {
            match &mut arm {
                Arm::Celt(enc) => {
                    debug_assert!(
                        !plan_end && !begin_r,
                        "CELT frames carry no §4.5.1 side info"
                    );
                    let digital_silence = pcm.iter().all(|&v| v == 0);
                    if self.celt_dtx.step(
                        digital_silence,
                        false,
                        u32::from(self.arm_tenths(cur.mode)),
                    ) {
                        return Ok((enc.dtx_marker()?, false, 0));
                    }
                    if self.celt_dtx.take_resume() {
                        enc.force_intra_next();
                    }
                    let payload = if digital_silence && self.vbr {
                        MIN_PACKET_BYTES - 1
                    } else {
                        elected.saturating_sub(1).clamp(2, MAX_FRAME_BYTES)
                    };
                    let (packet, _info) = enc.encode_packet(pcm, payload)?;
                    Ok((packet, false, 0))
                }
                Arm::HybridMono(h) => {
                    let plan = if begin_r {
                        let head = &pcm[..ch * REDUNDANT_FRAME_SAMPLES];
                        let frame = encode_redundant_celt_frame(
                            h.celt_state_mut(),
                            head,
                            cur.bandwidth,
                            red_bytes,
                        )?;
                        if reset_after_begin {
                            h.celt_state_mut().reset();
                        }
                        RedundancyPlan::Beginning(frame)
                    } else if plan_end {
                        RedundancyPlan::End { bytes: red_bytes }
                    } else {
                        RedundancyPlan::None
                    };
                    let red_extra = plan.bytes();
                    let payload = (elected.saturating_sub(1) + red_extra).clamp(2, MAX_FRAME_BYTES);
                    let packet = h.encode_packet_elected_with(pcm, payload, plan)?;
                    let carried = packet.len() > 1;
                    Ok((
                        packet,
                        plan_end && carried,
                        if carried { red_extra } else { 0 },
                    ))
                }
                Arm::HybridStereo(h) => {
                    let plan = if begin_r {
                        let head = &pcm[..ch * REDUNDANT_FRAME_SAMPLES];
                        let frame = encode_redundant_celt_frame(
                            h.celt_state_mut(),
                            head,
                            cur.bandwidth,
                            red_bytes,
                        )?;
                        if reset_after_begin {
                            h.celt_state_mut().reset();
                        }
                        RedundancyPlan::Beginning(frame)
                    } else if plan_end {
                        RedundancyPlan::End { bytes: red_bytes }
                    } else {
                        RedundancyPlan::None
                    };
                    let red_extra = plan.bytes();
                    let payload = (elected.saturating_sub(1) + red_extra).clamp(2, MAX_FRAME_BYTES);
                    let packet = h.encode_packet_elected_with(pcm, payload, plan)?;
                    let carried = packet.len() > 1;
                    Ok((
                        packet,
                        plan_end && carried,
                        if carried { red_extra } else { 0 },
                    ))
                }
                Arm::SilkMono(a) => {
                    let pcm48: Vec<f64> = pcm.iter().map(|&v| f64::from(v)).collect();
                    let internal = a.decim.process(&pcm48);
                    // Pre-encode a beginning-position redundant frame
                    // on the carried CELT chain (§4.5.2: no reset).
                    let begin_frame = if begin_r {
                        Some(self.silk_redundant_frame(
                            &pcm[..ch * REDUNDANT_FRAME_SAMPLES],
                            cur.bandwidth,
                            red_bytes,
                            false,
                        )?)
                    } else {
                        None
                    };
                    if begin_r || plan_end {
                        a.enc.set_redundancy_position(Some(if begin_r {
                            RedundancyPosition::Beginning
                        } else {
                            RedundancyPosition::End
                        }));
                    }
                    // The §4.5.1 redundancy is EXTRA bitrate at the
                    // transition ("the extra bitrate required for
                    // redundancy", §4.5.3): the primary SILK frames
                    // keep the full election and the committed
                    // overage is repaid by the VBR drift control.
                    let inner = elected.clamp(MIN_PACKET_BYTES, 1 + MAX_FRAME_BYTES);
                    let pkt = a.enc.encode_packet_elected(&internal, inner)?;
                    if pkt.is_dtx() {
                        return Ok((pkt.packet, false, 0));
                    }
                    let mut packet = pkt.packet;
                    if let Some(frame) = begin_frame {
                        let extra = frame.len();
                        packet.extend_from_slice(&frame);
                        Ok((packet, false, extra))
                    } else if plan_end {
                        // §4.5.2 / Figure 18 `!R`: reset, then encode.
                        let tail = &pcm[pcm.len() - ch * REDUNDANT_FRAME_SAMPLES..];
                        let frame =
                            self.silk_redundant_frame(tail, cur.bandwidth, red_bytes, true)?;
                        let extra = frame.len();
                        packet.extend_from_slice(&frame);
                        Ok((packet, true, extra))
                    } else {
                        Ok((packet, false, 0))
                    }
                }
                Arm::SilkStereo(a) => {
                    let l48: Vec<f64> = pcm.iter().step_by(2).map(|&v| f64::from(v)).collect();
                    let r48: Vec<f64> = pcm
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|&v| f64::from(v))
                        .collect();
                    let l16 = a.decim_l.process(&l48);
                    let r16 = a.decim_r.process(&r48);
                    let n = l16.len();
                    // §4.2.8 one-sample lookahead hold.
                    let mut left = Vec::with_capacity(n);
                    left.push(a.held.0);
                    left.extend_from_slice(&l16[..n - 1]);
                    let mut right = Vec::with_capacity(n);
                    right.push(a.held.1);
                    right.extend_from_slice(&r16[..n - 1]);
                    let next_lr = Some((l16[n - 1], r16[n - 1]));
                    a.held = (l16[n - 1], r16[n - 1]);

                    let begin_frame = if begin_r {
                        Some(self.silk_redundant_frame(
                            &pcm[..ch * REDUNDANT_FRAME_SAMPLES],
                            cur.bandwidth,
                            red_bytes,
                            false,
                        )?)
                    } else {
                        None
                    };
                    if begin_r || plan_end {
                        a.enc.set_redundancy_position(Some(if begin_r {
                            RedundancyPosition::Beginning
                        } else {
                            RedundancyPosition::End
                        }));
                    }
                    // See the mono arm: redundancy is extra bitrate.
                    let inner = elected.clamp(MIN_PACKET_BYTES, 1 + MAX_FRAME_BYTES);
                    let pkt = a.enc.encode_packet_elected(&left, &right, next_lr, inner)?;
                    if pkt.is_dtx() {
                        return Ok((pkt.packet, false, 0));
                    }
                    let mut packet = pkt.packet;
                    if let Some(frame) = begin_frame {
                        let extra = frame.len();
                        packet.extend_from_slice(&frame);
                        Ok((packet, false, extra))
                    } else if plan_end {
                        let tail = &pcm[pcm.len() - ch * REDUNDANT_FRAME_SAMPLES..];
                        let frame =
                            self.silk_redundant_frame(tail, cur.bandwidth, red_bytes, true)?;
                        let extra = frame.len();
                        packet.extend_from_slice(&frame);
                        Ok((packet, true, extra))
                    } else {
                        Ok((packet, false, 0))
                    }
                }
            }
        })();
        self.arm = Some(arm);
        result
    }

    /// Encode a §4.5.1 redundant CELT frame from a SILK-only packet's
    /// head/tail 5 ms, on the stream's carried CELT state
    /// (`reset_before` per the §4.5.2 end-position rule).
    fn silk_redundant_frame(
        &mut self,
        pcm: &[i16],
        carrier_bandwidth: Bandwidth,
        bytes: usize,
        reset_before: bool,
    ) -> Result<Vec<u8>, Error> {
        let channels = self.channels;
        let state = self
            .celt_carry
            .get_or_insert_with(|| CeltEncoderState::new(channels, REDUNDANT_FRAME_SAMPLES));
        if state.channels() != channels {
            *state = CeltEncoderState::new(channels, REDUNDANT_FRAME_SAMPLES);
        }
        if reset_before {
            state.reset();
        }
        encode_redundant_celt_frame(state, pcm, carrier_bandwidth, bytes)
    }

    /// Move to the new configuration after the transition-carrier
    /// packet: thread the CELT state, mirror the §4.5.2 resets, carry
    /// the SILK front end where the decoder carries its SILK state,
    /// and arm the beginning-position redundant frame when Figure 18
    /// places one.
    fn switch_to(
        &mut self,
        old: StreamConfig,
        new: StreamConfig,
        end_r_carried: bool,
    ) -> Result<(), Error> {
        let old_arm = self.arm.take().expect("arm installed");
        // The stream's one CELT state leaves the old arm.
        let celt_state = match &old_arm {
            Arm::Celt(_) | Arm::HybridMono(_) | Arm::HybridStereo(_) => None, // taken below
            Arm::SilkMono(_) | Arm::SilkStereo(_) => self.celt_carry.take(),
        };
        let mut old_arm = old_arm;
        let celt_state = match (&mut old_arm, celt_state) {
            (Arm::Celt(enc), _) => Some(enc.celt_state_mut().clone()),
            (Arm::HybridMono(h), _) => Some(h.celt_state_mut().clone()),
            (Arm::HybridStereo(h), _) => Some(h.celt_state_mut().clone()),
            (_, s) => s,
        };

        // §4.5.2 mirror. The decoder keys rule 3 off the END-position
        // redundancy it decoded in the transition-carrier frame.
        let red = if end_r_carried {
            RedundancyDecision::Present {
                position: RedundancyPosition::End,
                size_bytes: self.redundant_frame_bytes(),
            }
        } else {
            RedundancyDecision::NotPresent
        };
        let reset = decide_state_resets(old.operating_mode(), new.operating_mode(), red);

        let begin_r = self.redundancy && Self::begin_r_applies(old, new);
        let mut celt_state = celt_state;
        match reset.celt {
            CeltResetPlacement::BeforeFrame => {
                if begin_r && new.mode == Mode::Hybrid {
                    // Figure 18 `R & |H`: reset lands between the
                    // beginning-position redundant frame and the main
                    // CELT layer of the first Hybrid frame.
                    self.reset_celt_after_begin_r = true;
                } else if let Some(state) = celt_state.as_mut() {
                    state.reset();
                } else {
                    celt_state = None; // fresh state == reset state
                }
            }
            // Rule 3: the reset already happened before the
            // end-position redundant frame; carry the warmed state.
            CeltResetPlacement::BeforeRedundantOnly | CeltResetPlacement::None => {}
        }

        // SILK front-end carry: only when the decoder carries its
        // SILK state — same internal bandwidth, no §4.5.2 rule-1
        // reset (rule 1 only fires from CELT-only, where there is no
        // SILK front end to carry anyway).
        let silk_from = if !reset.silk
            && old.silk_bandwidth().is_some()
            && old.silk_bandwidth() == new.silk_bandwidth()
        {
            Some(old_arm)
        } else {
            None
        };

        self.arm = Some(self.build_arm(new, celt_state, silk_from)?);
        self.begin_r_pending = begin_r;
        self.begin_r_bytes = self.redundant_frame_bytes();
        Ok(())
    }
}
