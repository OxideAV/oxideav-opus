//! SILK resampler delay constants — RFC 6716 §4.2.9.
//!
//! The §4.2.9 resampler itself is *non-normative*: the spec explicitly
//! states "the resampler itself is non-normative, and a decoder can use
//! any method it wants to perform the resampling." What IS normative is
//! the **maximum decoder-side delay allocation** (Table 54), which
//! exists so that an encoder can apply a matching pre-delay to the MDCT
//! layer and keep SILK and CELT aligned across a mode switch (§4.5).
//!
//! This module owns:
//!
//! * The §4.2.9 Table 54 delay allocation per SILK audio bandwidth
//!   ([`silk_resampler_delay_ms`], [`silk_resampler_delay_samples_at`]).
//! * The five supported decoder output sample rates per §4.2.9
//!   ([`SUPPORTED_OUTPUT_RATES_HZ`], [`is_supported_output_rate`]).
//! * The internal SILK sample rate per audio bandwidth implied by the
//!   §4.2.1 / §4.2.7.x decode pipeline ([`silk_internal_rate_hz`],
//!   [`silk_frame_samples_internal`]).
//!
//! * The actual sample-rate conversion: [`SilkUpsampler`], a stateful
//!   streaming polyphase windowed-sinc upsampler from the SILK
//!   internal rate to 48 kHz. §4.2.9 makes the filter non-normative;
//!   what matters is the group **delay**, which decides where SILK
//!   audio lands on the 48 kHz timeline relative to the CELT layer
//!   of a Hybrid frame and to the RFC 7845 pre-skip. The design
//!   delays (see [`upsampler_design`]) are calibrated black-box
//!   against the reference decodes of the staged fixture corpus,
//!   per bandwidth and per reconstruction path
//!   ([`SilkChannelPath`]); Table 54 remains the spec's stated
//!   encoder-side target and is kept as the documented constants
//!   above.
//!
//! The kernel-width choice is forced by the delay: with the group
//! delay fixed at `d₄₈` 48 kHz samples and the output for one frame
//! due immediately (the decoder emits each frame's 48 kHz samples as
//! soon as the frame is decoded, with no lookahead into the next
//! frame), a symmetric interpolation kernel can reach at most `d₄₈`
//! output steps into the future, capping its half-width at
//! `floor((1 + d₄₈)/U)` input samples (`U` = the upsampling factor).
//! This is the §4.2.9 trade the prose describes ("NB is given a
//! smaller decoder delay allocation …"): the delay allocation *is*
//! the filter length budget.
//!
//! All numeric values are transcribed from RFC 6716 §4.2 (Table 54
//! plus the §4.2.9 prose, plus the SILK-internal sample rates implied
//! by the §4.2.7.x decode pipeline). No external library source was
//! consulted, paraphrased, or used as a cross-check oracle.

use crate::Bandwidth;

/// The five decoder output sample rates the §4.2.9 prose names as
/// supported (8, 12, 16, 24, 48 kHz). These are the rates the reference
/// implementation "is able to resample to … within or near this delay
/// constraint" per §4.2.9.
///
/// An application is free to ask for any output rate — the resampler
/// is non-normative — but staying inside this list is what the spec
/// promises will fit inside the Table 54 delay allocation.
pub const SUPPORTED_OUTPUT_RATES_HZ: &[u32] = &[8_000, 12_000, 16_000, 24_000, 48_000];

/// The 48 kHz "common" rate the §4.2.9 delay table is referenced
/// against (Table 54: "the maximum resampler delay in samples at
/// 48 kHz"). Also the rate at which the CELT MDCT operates, so the
/// natural rate to keep SILK and CELT aligned.
pub const REFERENCE_RATE_HZ: u32 = 48_000;

/// Table 54 NB delay, in milliseconds: 0.538 ms.
///
/// "NB is given a smaller decoder delay allocation than MB and WB to
/// allow a higher-order filter when resampling to 8 kHz in both the
/// encoder and decoder."
pub const SILK_RESAMPLER_DELAY_MS_NB: f64 = 0.538;

/// Table 54 MB delay, in milliseconds: 0.692 ms.
pub const SILK_RESAMPLER_DELAY_MS_MB: f64 = 0.692;

/// Table 54 WB delay, in milliseconds: 0.706 ms.
pub const SILK_RESAMPLER_DELAY_MS_WB: f64 = 0.706;

/// Return the §4.2.9 Table 54 normative resampler delay (in
/// milliseconds) for the given SILK audio bandwidth, or `None` if the
/// bandwidth never reaches the §4.2.9 resampler (SWB and FB are CELT-
/// or Hybrid-only at the SILK layer; they don't appear in Table 54).
///
/// The returned value is the spec's *maximum* allocation: a decoder
/// is free to use a resampler with less delay (or to use no
/// resampler at all when the output rate matches the internal SILK
/// rate). A decoder that wants *more* delay must compensate by
/// delaying the MDCT layer by the same extra amount.
pub fn silk_resampler_delay_ms(bw: Bandwidth) -> Option<f64> {
    match bw {
        Bandwidth::Nb => Some(SILK_RESAMPLER_DELAY_MS_NB),
        Bandwidth::Mb => Some(SILK_RESAMPLER_DELAY_MS_MB),
        Bandwidth::Wb => Some(SILK_RESAMPLER_DELAY_MS_WB),
        Bandwidth::Swb | Bandwidth::Fb => None,
    }
}

/// Return the §4.2.9 Table 54 normative resampler delay expressed as a
/// sample count at `output_rate_hz`. Rounded to the nearest whole
/// sample because §4.2.9 cautions that "the actual output rate may not
/// be 48 kHz, it may not be possible to achieve exactly these delays
/// while using a whole number of input or output samples."
///
/// Returns `None` for a SWB or FB bandwidth (which never reaches the
/// §4.2.9 SILK resampler) or for a zero `output_rate_hz`.
pub fn silk_resampler_delay_samples_at(bw: Bandwidth, output_rate_hz: u32) -> Option<u32> {
    if output_rate_hz == 0 {
        return None;
    }
    let delay_ms = silk_resampler_delay_ms(bw)?;
    // ms × rate_hz / 1000 → samples. Round half away from zero.
    let samples = (delay_ms * (output_rate_hz as f64) / 1000.0).round();
    // Clamp to u32 — the value can't realistically overflow at any
    // sensible rate (0.706 ms × 48 kHz ≈ 34 samples), but be defensive.
    if !samples.is_finite() || samples < 0.0 || samples > u32::MAX as f64 {
        return None;
    }
    Some(samples as u32)
}

/// Whether `rate_hz` is one of the §4.2.9 "supported output sampling
/// rates" enumerated in the spec (8, 12, 16, 24, 48 kHz). Any other
/// rate is still acceptable per §4.2.9's non-normative resampler
/// clause but is outside the Table 54 delay guarantee.
pub fn is_supported_output_rate(rate_hz: u32) -> bool {
    SUPPORTED_OUTPUT_RATES_HZ.contains(&rate_hz)
}

/// Return the internal SILK sample rate, in Hz, for the given audio
/// bandwidth.
///
/// The SILK §4.2.7.x decode pipeline operates at:
///
/// * NB → 8 000 Hz
/// * MB → 12 000 Hz
/// * WB → 16 000 Hz
///
/// SWB and FB never reach the SILK layer (they're CELT-only or run as
/// the upper half of a Hybrid frame whose lower half is WB SILK), so
/// they return `None`. The §4.2.9 resampler turns this internal rate
/// into whatever output rate the application asked for (commonly
/// 48 kHz to align with CELT).
pub fn silk_internal_rate_hz(bw: Bandwidth) -> Option<u32> {
    match bw {
        Bandwidth::Nb => Some(8_000),
        Bandwidth::Mb => Some(12_000),
        Bandwidth::Wb => Some(16_000),
        Bandwidth::Swb | Bandwidth::Fb => None,
    }
}

/// Return the number of internal-rate samples in one SILK frame of the
/// given bandwidth × duration, or `None` if the bandwidth doesn't
/// reach SILK or the duration isn't a SILK frame length.
///
/// `silk_frame_duration_tenths_ms` is in the same tenths-of-a-ms unit
/// as `OpusTocByte::frame_size_tenths_ms`: 100 = 10 ms, 200 = 20 ms.
/// SILK frames are always 10 ms or 20 ms per §4.2.2.
///
/// The result is the count of post-SILK pre-resampler samples that
/// flow into the §4.2.9 stage for one SILK frame. For example:
///
/// * NB 20 ms → 8 000 Hz × 0.020 s = 160 samples.
/// * MB 20 ms → 12 000 Hz × 0.020 s = 240 samples.
/// * WB 10 ms → 16 000 Hz × 0.010 s = 160 samples.
pub fn silk_frame_samples_internal(
    bw: Bandwidth,
    silk_frame_duration_tenths_ms: u16,
) -> Option<u32> {
    let rate = silk_internal_rate_hz(bw)?;
    match silk_frame_duration_tenths_ms {
        100 => Some(rate / 100), // 10 ms = rate / 100
        200 => Some(rate / 50),  // 20 ms = rate / 50
        _ => None,
    }
}

/// Return the number of output-rate samples in one SILK frame of the
/// given bandwidth × duration after resampling to `output_rate_hz`.
///
/// Computed as `output_rate_hz × duration_ms / 1000`, rounded to the
/// nearest whole sample. Returns `None` if the bandwidth doesn't reach
/// SILK, the duration isn't a SILK frame length, or the output rate is
/// zero. Out-of-range arithmetic returns `None`.
///
/// This is a convenience for callers sizing the post-resampler output
/// buffer; the actual sample-rate conversion filter itself stays
/// non-normative.
pub fn silk_frame_samples_at_output(
    bw: Bandwidth,
    silk_frame_duration_tenths_ms: u16,
    output_rate_hz: u32,
) -> Option<u32> {
    if output_rate_hz == 0 {
        return None;
    }
    // Validate bandwidth reaches SILK by computing the internal count
    // (which already vets the bandwidth and the duration); the result
    // itself isn't used — we just need the early `None`.
    silk_frame_samples_internal(bw, silk_frame_duration_tenths_ms)?;
    let ms = match silk_frame_duration_tenths_ms {
        100 => 10.0_f64,
        200 => 20.0_f64,
        _ => return None,
    };
    let s = (ms * (output_rate_hz as f64) / 1000.0).round();
    if !s.is_finite() || s < 0.0 || s > u32::MAX as f64 {
        return None;
    }
    Some(s as u32)
}

// ---------------------------------------------------------------------
// §4.2.9 streaming upsampler (SILK internal rate → 48 kHz).
// ---------------------------------------------------------------------

/// Zeroth-order modified Bessel function of the first kind, by its
/// power series `Σ ((x/2)^k / k!)²` — the normalization core of the
/// Kaiser window. Converges rapidly for the argument range used here
/// (`x ≤ ~12`); terms fall below f64 epsilon after a few dozen steps.
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for k in 1..64 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

/// Normalized sinc: `sin(πx)/(πx)` with the removable singularity at 0.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Which SILK reconstruction path feeds the §4.2.9 upsampler. The two
/// paths carry different upstream delays — the stereo output leaves
/// the §4.2.8 unmixer one input sample late (its formulas read
/// `mid[i-1]` / `side[i-1]`), while the mono path's §4.2.8 delay is
/// imposed explicitly — and the black-box calibration against the
/// reference decodes of the staged fixture corpus shows the reference
/// decoder's mono output sits exactly one further input sample later
/// than its stereo output. Selecting the path picks the filter delay
/// that lands our total on the reference's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilkChannelPath {
    /// A mono SILK channel (SILK-only mono, Hybrid mono, FEC mono).
    Mono,
    /// One channel of an unmixed stereo pair (left or right).
    Stereo,
}

/// Per-(bandwidth × path) §4.2.9 filter design: upsampling factor to
/// 48 kHz, filter group delay `d₄₈` in 48 kHz samples, the
/// anti-imaging cutoff as a fraction of the input Nyquist, and the
/// Kaiser window shape β. The kernel half-width `M` in input samples
/// follows as the §4.2.9 causality cap `floor((1 + d₄₈)/U)` (see the
/// module docs).
///
/// §4.2.9 makes the filter non-normative and Table 54 gives the delay
/// the *encoder* targets; a decoder may use more delay. The `d₄₈`
/// values here are the **measured effective group delays of the
/// reference decodes** shipped with the staged fixture corpus
/// (black-box waveform calibration; exact-integer alignment lifts the
/// WB fixtures from ~44 dB to ~69–72 dB SNR), so decoded SILK lands
/// sample-aligned with a reference decode trimmed by the RFC 7845
/// pre-skip, and — for Hybrid — with the CELT layer. Together with
/// the path's one-input-sample §4.2.8 delay they put the decoder's
/// total SILK delay at 36/40/39 (mono NB/MB/WB) and 30/36/36 (stereo)
/// 48 kHz samples. The NB/MB *stereo* rows extrapolate the
/// mono−stereo = one-input-sample offset measured at WB (the staged
/// corpus has no NB/MB stereo fixture to pin them directly).
fn upsampler_design(bw: Bandwidth, path: SilkChannelPath) -> Option<(usize, f64, f64, f64)> {
    match (bw, path) {
        // NB 8 kHz → 48 kHz: U = 6 (total 36 = 30 + one 8 kHz sample).
        (Bandwidth::Nb, SilkChannelPath::Mono) => Some((6, 30.0, 0.92, 5.0)),
        (Bandwidth::Nb, SilkChannelPath::Stereo) => Some((6, 24.0, 0.92, 5.0)),
        // MB 12 kHz → 48 kHz: U = 4 (total 40 = 36 + one 12 kHz sample).
        (Bandwidth::Mb, SilkChannelPath::Mono) => Some((4, 36.0, 0.94, 7.0)),
        (Bandwidth::Mb, SilkChannelPath::Stereo) => Some((4, 32.0, 0.94, 7.0)),
        // WB 16 kHz → 48 kHz: U = 3 (total 39 = 36 + one 16 kHz sample).
        (Bandwidth::Wb, SilkChannelPath::Mono) => Some((3, 36.0, 0.95, 8.0)),
        (Bandwidth::Wb, SilkChannelPath::Stereo) => Some((3, 33.0, 0.95, 8.0)),
        (Bandwidth::Swb | Bandwidth::Fb, _) => None,
    }
}

/// A stateful streaming upsampler from one SILK internal rate
/// (8/12/16 kHz) to the 48 kHz decoder output rate — the crate's
/// §4.2.9 resampler.
///
/// §4.2.9 makes the filter non-normative; what matters is the group
/// **delay**, which decides where the decoded SILK audio lands on the
/// 48 kHz timeline relative to the CELT layer of a Hybrid frame and
/// to the RFC 7845 §4.2 pre-skip. This upsampler's per-(bandwidth ×
/// path) delays are calibrated black-box against the reference
/// decodes of the staged fixture corpus (see [`upsampler_design`]),
/// which lifts the WB fixtures to ~69–72 dB waveform SNR and aligns
/// the Hybrid SILK band with the (bit-aligned) CELT band.
///
/// Implementation: a polyphase windowed-sinc interpolator. Output
/// sample `n` (48 kHz timeline) is the kernel-weighted sum of the
/// input samples around the input-domain position
/// `τ(n) = (n − d₄₈)/U`, where `d₄₈` is the Table 54 delay in 48 kHz
/// samples and `U` the upsampling factor. The `U` fractional phases
/// are precomputed as tap tables (Kaiser-windowed sinc, per-phase
/// DC-normalized so constant inputs pass through exactly); the last
/// `P` input samples are carried across calls so frame boundaries
/// are seamless. Feeding zeros from a fresh state reproduces the
/// warm-up transient any delay-matched filter has; the RFC 7845
/// pre-skip discards it.
#[derive(Debug, Clone)]
pub struct SilkUpsampler {
    bandwidth: Bandwidth,
    path: SilkChannelPath,
    /// Filter group delay `d₄₈` in 48 kHz output samples.
    delay_48k: f64,
    /// Upsampling factor `U` (48000 / internal rate): 6, 4, or 3.
    factor: usize,
    /// Kernel half-width `M` in input samples (2·M taps per phase).
    half_width: usize,
    /// Per-phase base offset: for output phase `p`, the first tap
    /// reads input index `a + offset[p] − M + 1` where `a = n / U`.
    offsets: [isize; 6],
    /// Per-phase tap tables, `factor` phases × `2·M` taps, indexed so
    /// that tap `i` multiplies input sample `q − M + 1 + i`.
    phases: Vec<Vec<f32>>,
    /// Carried input history: the last `P` input samples of the
    /// previous call (`P = M − 1 + ceil(d₄₈/U)`), zeros after a reset.
    hist: Vec<f32>,
}

impl SilkUpsampler {
    /// Construct an upsampler for one SILK audio bandwidth on one
    /// reconstruction path, or `None` for SWB / FB (which never reach
    /// the §4.2.9 SILK resampler).
    pub fn new(bandwidth: Bandwidth, path: SilkChannelPath) -> Option<Self> {
        let (factor, d48, cutoff, beta) = upsampler_design(bandwidth, path)?;
        // §4.2.9 causality cap: with each frame's 48 kHz output due as
        // soon as the frame is decoded, the kernel can reach at most
        // d₄₈ output samples into the future.
        let half_width = ((1.0 + d48) / factor as f64).floor() as usize;
        let d_in = d48 / factor as f64; // …in input samples.

        // History depth: the earliest tap of output n = 0 reads input
        // index floor(−d_in) − M + 1; carry that many prior samples.
        let hist_len = (half_width - 1) + d_in.ceil() as usize;

        // Precompute the U phase tables. For output n with phase
        // p = n mod U and a = n / U, the input-domain position is
        // τ = a + (p/U − d_in); split into integer offset + fraction.
        let i0_beta = bessel_i0(beta);
        let mut offsets = [0isize; 6];
        let mut phases = Vec::with_capacity(factor);
        for (p, off_slot) in offsets.iter_mut().enumerate().take(factor) {
            let t = p as f64 / factor as f64 - d_in;
            let off = t.floor();
            let frac = t - off;
            *off_slot = off as isize;
            // Tap i multiplies input sample q − M + 1 + i, whose
            // kernel argument is τ − j = frac + (M − 1 − i).
            let mut taps = Vec::with_capacity(2 * half_width);
            let mut sum = 0.0f64;
            for i in 0..2 * half_width {
                let x = frac + (half_width as f64 - 1.0) - i as f64;
                // Kaiser-windowed sinc, cutoff relative to the input
                // Nyquist. Window support is |x| ≤ M; the extreme tap
                // arguments stay inside it (|frac + M − 1| < M).
                let w = {
                    let r = x / half_width as f64;
                    if r.abs() >= 1.0 {
                        0.0
                    } else {
                        bessel_i0(beta * (1.0 - r * r).sqrt()) / i0_beta
                    }
                };
                let k = cutoff * sinc(cutoff * x) * w;
                sum += k;
                taps.push(k);
            }
            // Per-phase DC normalization: constant inputs reproduce
            // exactly, removing the window's passband ripple at DC.
            let taps_f32: Vec<f32> = taps.iter().map(|&k| (k / sum) as f32).collect();
            phases.push(taps_f32);
        }

        Some(Self {
            bandwidth,
            path,
            delay_48k: d48,
            factor,
            half_width,
            offsets,
            phases,
            hist: vec![0.0; hist_len],
        })
    }

    /// The bandwidth this upsampler was built for (its input rate).
    pub fn bandwidth(&self) -> Bandwidth {
        self.bandwidth
    }

    /// The reconstruction path this upsampler was built for.
    pub fn path(&self) -> SilkChannelPath {
        self.path
    }

    /// The filter's group delay in 48 kHz output samples (the
    /// black-box-calibrated design value; see [`upsampler_design`]).
    pub fn delay_48k(&self) -> f64 {
        self.delay_48k
    }

    /// The upsampling factor to 48 kHz (6 / 4 / 3 for NB / MB / WB).
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Clear the carried input history (a §4.5.2 SILK state reset).
    pub fn reset(&mut self) {
        self.hist.fill(0.0);
    }

    /// Upsample one frame of internal-rate samples to 48 kHz.
    ///
    /// `out.len()` must equal `input.len() × factor` (each SILK frame
    /// produces exactly its 48 kHz worth of output; the design delay
    /// is *inside* the signal, not extra samples). The input history
    /// carried from previous calls makes consecutive frames seamless.
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) {
        assert_eq!(
            out.len(),
            input.len() * self.factor,
            "output must be factor × input"
        );
        let m = self.half_width;
        let p_len = self.hist.len();
        // ext = history ++ input; input index j lives at ext[j + P].
        let mut ext = Vec::with_capacity(p_len + input.len());
        ext.extend_from_slice(&self.hist);
        ext.extend_from_slice(input);

        for (n, o) in out.iter_mut().enumerate() {
            let p = n % self.factor;
            let a = (n / self.factor) as isize;
            let q = a + self.offsets[p];
            // First tap reads input index q − M + 1 → ext index
            // q − M + 1 + P (never negative by construction of P, and
            // the last tap q + M never passes the frame end by the
            // §4.2.9 causality cap on M).
            let base = (q - m as isize + 1 + p_len as isize) as usize;
            let taps = &self.phases[p];
            let window = &ext[base..base + 2 * m];
            let mut acc = 0.0f64;
            for (t, s) in taps.iter().zip(window) {
                acc += f64::from(*t) * f64::from(*s);
            }
            *o = acc as f32;
        }

        // Carry the trailing P input samples (all-zero history if the
        // frame was shorter than P — only possible for degenerate
        // inputs, which real SILK frames never produce).
        if ext.len() >= p_len {
            self.hist.copy_from_slice(&ext[ext.len() - p_len..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // Table 54 transcription self-checks.
    // ----------------------------------------------------------------

    #[test]
    fn table54_delay_ms_matches_rfc_for_nb_mb_wb() {
        // RFC 6716 §4.2.9 Table 54.
        assert_eq!(silk_resampler_delay_ms(Bandwidth::Nb), Some(0.538));
        assert_eq!(silk_resampler_delay_ms(Bandwidth::Mb), Some(0.692));
        assert_eq!(silk_resampler_delay_ms(Bandwidth::Wb), Some(0.706));
    }

    #[test]
    fn table54_excludes_swb_and_fb() {
        // SWB and FB never reach the §4.2.9 SILK resampler (they only
        // exist as CELT-only configs or as the upper half of a Hybrid
        // frame whose SILK half is WB).
        assert_eq!(silk_resampler_delay_ms(Bandwidth::Swb), None);
        assert_eq!(silk_resampler_delay_ms(Bandwidth::Fb), None);
    }

    #[test]
    fn delay_table_is_strictly_increasing_nb_lt_mb_lt_wb() {
        // §4.2.9 prose: "NB is given a smaller decoder delay
        // allocation than MB and WB to allow a higher-order filter".
        // MB sits between NB and WB. Verify the table's monotonicity.
        let nb = silk_resampler_delay_ms(Bandwidth::Nb).unwrap();
        let mb = silk_resampler_delay_ms(Bandwidth::Mb).unwrap();
        let wb = silk_resampler_delay_ms(Bandwidth::Wb).unwrap();
        assert!(nb < mb, "NB ({nb}) should be < MB ({mb})");
        assert!(mb < wb, "MB ({mb}) should be < WB ({wb})");
    }

    // ----------------------------------------------------------------
    // Delay in samples at output rate.
    // ----------------------------------------------------------------

    #[test]
    fn delay_samples_at_48khz_matches_table54_reference() {
        // Table 54 is stated at 48 kHz. Check the round-to-nearest
        // expansion matches the spec's "samples at 48 kHz" framing.
        //
        // NB: 0.538 ms × 48 = 25.824 samples → 26.
        // MB: 0.692 ms × 48 = 33.216 samples → 33.
        // WB: 0.706 ms × 48 = 33.888 samples → 34.
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Nb, 48_000),
            Some(26)
        );
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Mb, 48_000),
            Some(33)
        );
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Wb, 48_000),
            Some(34)
        );
    }

    #[test]
    fn delay_samples_at_internal_rate_makes_sense() {
        // At 8 kHz: NB delay × 8 = 0.538 × 8 = 4.304 → 4.
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Nb, 8_000),
            Some(4)
        );
        // At 12 kHz: MB delay × 12 = 0.692 × 12 = 8.304 → 8.
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Mb, 12_000),
            Some(8)
        );
        // At 16 kHz: WB delay × 16 = 0.706 × 16 = 11.296 → 11.
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Wb, 16_000),
            Some(11)
        );
    }

    #[test]
    fn delay_samples_at_24khz_intermediate_rate() {
        // 24 kHz is one of the §4.2.9 supported output rates.
        // NB: 0.538 × 24 = 12.912 → 13.
        // MB: 0.692 × 24 = 16.608 → 17.
        // WB: 0.706 × 24 = 16.944 → 17.
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Nb, 24_000),
            Some(13)
        );
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Mb, 24_000),
            Some(17)
        );
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Wb, 24_000),
            Some(17)
        );
    }

    #[test]
    fn delay_samples_rejects_swb_fb_and_zero_rate() {
        assert_eq!(
            silk_resampler_delay_samples_at(Bandwidth::Swb, 48_000),
            None
        );
        assert_eq!(silk_resampler_delay_samples_at(Bandwidth::Fb, 48_000), None);
        assert_eq!(silk_resampler_delay_samples_at(Bandwidth::Nb, 0), None);
    }

    // ----------------------------------------------------------------
    // Supported-output-rate dispatch.
    // ----------------------------------------------------------------

    #[test]
    fn supported_output_rates_are_the_five_spec_rates() {
        // §4.2.9: "8, 12, 16, 24, or 48 kHz".
        assert_eq!(
            SUPPORTED_OUTPUT_RATES_HZ,
            &[8_000, 12_000, 16_000, 24_000, 48_000][..]
        );
        for &r in SUPPORTED_OUTPUT_RATES_HZ {
            assert!(is_supported_output_rate(r), "rate {r} should be supported");
        }
        // A few that aren't on the list.
        for r in [0u32, 11_025, 22_050, 32_000, 44_100, 96_000] {
            assert!(
                !is_supported_output_rate(r),
                "rate {r} should NOT be in §4.2.9 list"
            );
        }
    }

    #[test]
    fn reference_rate_is_48khz() {
        // Table 54 is anchored at 48 kHz; CELT also runs at 48 kHz.
        assert_eq!(REFERENCE_RATE_HZ, 48_000);
        assert!(is_supported_output_rate(REFERENCE_RATE_HZ));
    }

    // ----------------------------------------------------------------
    // Internal SILK rate per bandwidth.
    // ----------------------------------------------------------------

    #[test]
    fn internal_silk_rate_per_bandwidth() {
        assert_eq!(silk_internal_rate_hz(Bandwidth::Nb), Some(8_000));
        assert_eq!(silk_internal_rate_hz(Bandwidth::Mb), Some(12_000));
        assert_eq!(silk_internal_rate_hz(Bandwidth::Wb), Some(16_000));
        // SWB / FB don't reach the SILK layer.
        assert_eq!(silk_internal_rate_hz(Bandwidth::Swb), None);
        assert_eq!(silk_internal_rate_hz(Bandwidth::Fb), None);
    }

    #[test]
    fn internal_silk_rate_is_a_supported_output_rate_for_nb_and_wb() {
        // Decoders that don't want resampling can ask for the SILK
        // internal rate directly — for NB and WB this is also a §4.2.9
        // supported output rate (8 kHz, 16 kHz). MB's 12 kHz is also on
        // the supported-output list.
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            let r = silk_internal_rate_hz(bw).unwrap();
            assert!(
                is_supported_output_rate(r),
                "internal SILK rate {r} for {bw:?} should be in §4.2.9 list"
            );
        }
    }

    // ----------------------------------------------------------------
    // Per-frame sample counts.
    // ----------------------------------------------------------------

    #[test]
    fn silk_frame_samples_internal_canonical_cases() {
        // NB internal = 8000 Hz.
        // 10 ms = 80 samples; 20 ms = 160 samples.
        assert_eq!(silk_frame_samples_internal(Bandwidth::Nb, 100), Some(80));
        assert_eq!(silk_frame_samples_internal(Bandwidth::Nb, 200), Some(160));
        // MB internal = 12000 Hz.
        // 10 ms = 120 samples; 20 ms = 240 samples.
        assert_eq!(silk_frame_samples_internal(Bandwidth::Mb, 100), Some(120));
        assert_eq!(silk_frame_samples_internal(Bandwidth::Mb, 200), Some(240));
        // WB internal = 16000 Hz.
        // 10 ms = 160 samples; 20 ms = 320 samples.
        assert_eq!(silk_frame_samples_internal(Bandwidth::Wb, 100), Some(160));
        assert_eq!(silk_frame_samples_internal(Bandwidth::Wb, 200), Some(320));
    }

    #[test]
    fn silk_frame_samples_internal_rejects_non_silk_durations() {
        // 40 ms and 60 ms Opus frames carry MULTIPLE SILK frames; this
        // helper measures ONE SILK frame so 400 / 600 are not valid
        // inputs. 25 / 50 (2.5 / 5 ms) are CELT-only.
        for dur in [0u16, 25, 50, 400, 600, 1234] {
            assert_eq!(
                silk_frame_samples_internal(Bandwidth::Nb, dur),
                None,
                "dur {dur} should be rejected"
            );
            assert_eq!(silk_frame_samples_internal(Bandwidth::Mb, dur), None);
            assert_eq!(silk_frame_samples_internal(Bandwidth::Wb, dur), None);
        }
    }

    #[test]
    fn silk_frame_samples_internal_rejects_swb_and_fb() {
        for bw in [Bandwidth::Swb, Bandwidth::Fb] {
            for dur in [100u16, 200] {
                assert_eq!(
                    silk_frame_samples_internal(bw, dur),
                    None,
                    "{bw:?} {dur} should be rejected"
                );
            }
        }
    }

    #[test]
    fn silk_frame_samples_at_output_48khz_matches_duration() {
        // At 48 kHz: 10 ms = 480 samples; 20 ms = 960 samples;
        // independent of bandwidth.
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            assert_eq!(
                silk_frame_samples_at_output(bw, 100, 48_000),
                Some(480),
                "{bw:?} 10 ms"
            );
            assert_eq!(
                silk_frame_samples_at_output(bw, 200, 48_000),
                Some(960),
                "{bw:?} 20 ms"
            );
        }
    }

    #[test]
    fn silk_frame_samples_at_output_matches_internal_when_rate_matches() {
        // When the output rate equals the internal SILK rate, the
        // output sample count is identical to the internal one (no
        // resampling needed).
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            let r = silk_internal_rate_hz(bw).unwrap();
            for dur in [100u16, 200] {
                assert_eq!(
                    silk_frame_samples_at_output(bw, dur, r),
                    silk_frame_samples_internal(bw, dur),
                    "{bw:?} {dur} at {r} Hz"
                );
            }
        }
    }

    #[test]
    fn silk_frame_samples_at_output_rejects_zero_rate_and_non_silk() {
        assert_eq!(silk_frame_samples_at_output(Bandwidth::Nb, 100, 0), None);
        assert_eq!(
            silk_frame_samples_at_output(Bandwidth::Swb, 100, 48_000),
            None
        );
        assert_eq!(
            silk_frame_samples_at_output(Bandwidth::Nb, 25, 48_000),
            None
        );
        assert_eq!(
            silk_frame_samples_at_output(Bandwidth::Nb, 400, 48_000),
            None
        );
    }

    // ----------------------------------------------------------------
    // SilkUpsampler: construction and geometry.
    // ----------------------------------------------------------------

    #[test]
    fn upsampler_constructs_for_silk_bandwidths_only() {
        for (bw, factor) in [
            (Bandwidth::Nb, 6usize),
            (Bandwidth::Mb, 4),
            (Bandwidth::Wb, 3),
        ] {
            for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
                let up = SilkUpsampler::new(bw, path).expect("SILK bandwidth must construct");
                assert_eq!(up.factor(), factor, "{bw:?} {path:?}");
                assert_eq!(up.bandwidth(), bw);
                assert_eq!(up.path(), path);
            }
        }
        for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
            assert!(SilkUpsampler::new(Bandwidth::Swb, path).is_none());
            assert!(SilkUpsampler::new(Bandwidth::Fb, path).is_none());
        }
    }

    #[test]
    fn upsampler_design_delays_match_calibrated_totals() {
        // The black-box-calibrated totals (filter delay + the path's
        // one-input-sample §4.2.8 delay) on the 48 kHz timeline:
        // mono 36 / 40 / 39, stereo 30 / 36 / 36 for NB / MB / WB.
        // The one-input-sample §4.2.8 delay is U samples at 48 kHz.
        for (bw, mono_total, stereo_total) in [
            (Bandwidth::Nb, 36.0, 30.0),
            (Bandwidth::Mb, 40.0, 36.0),
            (Bandwidth::Wb, 39.0, 36.0),
        ] {
            let mono = SilkUpsampler::new(bw, SilkChannelPath::Mono).unwrap();
            let stereo = SilkUpsampler::new(bw, SilkChannelPath::Stereo).unwrap();
            let u = mono.factor() as f64;
            assert_eq!(mono.delay_48k() + u, mono_total, "{bw:?} mono total");
            assert_eq!(stereo.delay_48k() + u, stereo_total, "{bw:?} stereo total");
            // Reference decoder asymmetry: mono sits exactly one input
            // sample (U samples at 48 kHz) later than stereo.
            assert_eq!(mono.delay_48k() - stereo.delay_48k(), u);
        }
    }

    #[test]
    fn upsampler_kernel_halfwidth_respects_causality_cap() {
        // The §4.2.9 cap: M ≤ floor((1 + d₄₈)/U). Anything larger
        // would need input from the *next* frame to produce this
        // frame's last output samples. Verified behaviourally: the
        // taps of the highest phase must never reach past the newest
        // available input sample (checked by `process` slicing — a
        // violation would panic on the last output of a frame).
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
                let (factor, d48, _, _) = upsampler_design(bw, path).unwrap();
                let mut up = SilkUpsampler::new(bw, path).unwrap();
                let cap = ((1.0 + d48) / factor as f64).floor() as usize;
                assert_eq!(
                    up.half_width, cap,
                    "{bw:?} {path:?}: M != causality cap {cap}"
                );
                // Behavioural: a one-sample frame is the tightest
                // causality case (out.len() = U, inputs available = 1).
                let mut out = vec![0.0f32; factor];
                up.process(&[1.0], &mut out);
            }
        }
    }

    // ----------------------------------------------------------------
    // SilkUpsampler: signal behaviour.
    // ----------------------------------------------------------------

    #[test]
    fn upsampler_passes_dc_exactly_after_warmup() {
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
                let mut up = SilkUpsampler::new(bw, path).unwrap();
                let f = up.factor();
                let input = vec![0.25f32; 200];
                let mut out = vec![0.0f32; 200 * f];
                up.process(&input, &mut out);
                // After the warm-up (history is zeros), every output
                // must be exactly the DC value thanks to the per-phase
                // normalization. Warm-up spans the kernel + delay:
                // skip the first 120 output samples generously.
                for (i, &v) in out.iter().enumerate().skip(120) {
                    assert!(
                        (v - 0.25).abs() < 1e-6,
                        "{bw:?} {path:?}: DC not preserved at output {i}: {v}"
                    );
                }
            }
        }
    }

    #[test]
    fn upsampler_group_delay_matches_design_delay() {
        // Feed a unit impulse mid-stream; the output energy centroid
        // must land at U·t + d₄₈ on the 48 kHz timeline — the design
        // delay exactly (linear-phase kernel, so the centroid IS the
        // group delay at all frequencies).
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
                let mut up = SilkUpsampler::new(bw, path).unwrap();
                let f = up.factor();
                let d48 = up.delay_48k();
                let mut input = vec![0.0f32; 256];
                input[100] = 1.0;
                let mut out = vec![0.0f32; 256 * f];
                up.process(&input, &mut out);
                let mut wsum = 0.0f64;
                let mut esum = 0.0f64;
                for (i, &v) in out.iter().enumerate() {
                    let e = f64::from(v) * f64::from(v);
                    wsum += e * i as f64;
                    esum += e;
                }
                let centroid = wsum / esum;
                let want = 100.0 * f as f64 + d48;
                assert!(
                    (centroid - want).abs() < 0.35,
                    "{bw:?} {path:?}: impulse centroid {centroid:.3} != {want:.3}"
                );
            }
        }
    }

    #[test]
    fn upsampler_reconstructs_in_band_sine_at_design_delay() {
        // A mid-band sine upsampled must match the analytically
        // delayed 48 kHz sine: out[n] ≈ sin(2πf·(n − d₄₈)/48000).
        for (bw, tone_hz) in [
            (Bandwidth::Nb, 440.0f64),
            (Bandwidth::Mb, 700.0),
            (Bandwidth::Wb, 1000.0),
        ] {
            for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
                let mut up = SilkUpsampler::new(bw, path).unwrap();
                let f = up.factor();
                let in_rate = 48_000.0 / f as f64;
                let d48 = up.delay_48k();
                let n_in = 640usize;
                let input: Vec<f32> = (0..n_in)
                    .map(|i| {
                        (2.0 * std::f64::consts::PI * tone_hz * i as f64 / in_rate).sin() as f32
                    })
                    .collect();
                let mut out = vec![0.0f32; n_in * f];
                up.process(&input, &mut out);
                // Compare over a steady window (skip warm-up and tail).
                let (mut sig, mut err) = (0.0f64, 0.0f64);
                let tail = out.len() - 200;
                for (n, &o) in out.iter().enumerate().take(tail).skip(200) {
                    let want =
                        (2.0 * std::f64::consts::PI * tone_hz * (n as f64 - d48) / 48_000.0).sin();
                    let got = f64::from(o);
                    sig += want * want;
                    err += (want - got) * (want - got);
                }
                let snr = 10.0 * (sig / err).log10();
                assert!(
                    snr > 40.0,
                    "{bw:?} {path:?}: in-band sine SNR {snr:.1} dB too low"
                );
            }
        }
    }

    #[test]
    fn upsampler_streaming_equals_batch() {
        // Splitting the input across process() calls (as the decoder
        // does per Opus frame) must be sample-identical to one big
        // call: the carried history makes frame boundaries seamless.
        for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
            for path in [SilkChannelPath::Mono, SilkChannelPath::Stereo] {
                let f = SilkUpsampler::new(bw, path).unwrap().factor();
                // A deterministic pseudo-random-ish signal.
                let input: Vec<f32> = (0..480u32)
                    .map(|i| ((i.wrapping_mul(2654435761) >> 16) as f32 / 65536.0) - 0.5)
                    .collect();

                let mut up_batch = SilkUpsampler::new(bw, path).unwrap();
                let mut batch = vec![0.0f32; input.len() * f];
                up_batch.process(&input, &mut batch);

                let mut up_stream = SilkUpsampler::new(bw, path).unwrap();
                let mut streamed = Vec::with_capacity(batch.len());
                for chunk in input.chunks(160) {
                    let mut out = vec![0.0f32; chunk.len() * f];
                    up_stream.process(chunk, &mut out);
                    streamed.extend_from_slice(&out);
                }
                assert_eq!(batch, streamed, "{bw:?} {path:?}: streaming != batch");
            }
        }
    }

    #[test]
    fn upsampler_reset_clears_history() {
        let mut up = SilkUpsampler::new(Bandwidth::Nb, SilkChannelPath::Mono).unwrap();
        let input = vec![0.9f32; 160];
        let mut out = vec![0.0f32; 960];
        up.process(&input, &mut out);
        up.reset();
        // After a reset, an all-zero input must produce all-zero
        // output (no residue from the 0.9-DC history).
        let zeros = vec![0.0f32; 160];
        up.process(&zeros, &mut out);
        assert!(out.iter().all(|&v| v == 0.0), "history survived the reset");
    }

    // ----------------------------------------------------------------
    // Cross-checks: delay never exceeds one SILK frame.
    // ----------------------------------------------------------------

    #[test]
    fn delay_is_smaller_than_one_silk_frame_at_every_supported_rate() {
        // Sanity: the §4.2.9 delay allocation is well under 1 ms.
        // For every supported output rate, the delay in samples
        // must be strictly less than the per-SILK-frame sample count
        // (10 ms = the shorter SILK frame). Otherwise the resampler
        // would be holding more than one frame of data.
        for &out_rate in SUPPORTED_OUTPUT_RATES_HZ {
            for bw in [Bandwidth::Nb, Bandwidth::Mb, Bandwidth::Wb] {
                let delay = silk_resampler_delay_samples_at(bw, out_rate).unwrap();
                let one_frame = silk_frame_samples_at_output(bw, 100, out_rate).unwrap();
                assert!(
                    (delay as u64) < (one_frame as u64),
                    "{bw:?} delay {delay} >= one 10ms SILK frame {one_frame} at {out_rate} Hz"
                );
            }
        }
    }
}
