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
//! The actual sample-rate conversion is left to the application or to
//! a future round that picks a particular polyphase / windowed-sinc
//! kernel — but everything *downstream* needs to know the delay
//! budget and the input/output rates, which is what this module
//! supplies.
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
