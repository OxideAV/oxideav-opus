//! Encoder-side input upsampler: 8 / 12 / 16 / 24 kHz → 48 kHz.
//!
//! RFC 6716 §2 lets an encoder accept input at 8, 12, 16, 24 or 48 kHz;
//! this crate's coding core runs on a 48 kHz timeline (the CELT MDCT
//! rate, and the rate SILK's internal resampling is referenced to), so
//! reduced-rate input is first interpolated to 48 kHz by the integer
//! factor `M = 48 000 / rate` (6, 4, 3 or 2). The resampler is
//! encoder-internal and non-normative (§4.2.9 makes even the decoder's
//! resampler non-normative); only its delay is visible, and the stream
//! adapter folds it into the RFC 7845 §5.1 pre-skip.
//!
//! ## Design
//!
//! Classic integer-factor interpolation (zero-stuff by `M`, then an
//! anti-imaging low-pass at the 48 kHz rate, computed in polyphase form
//! so the zeros are never multiplied): the low-pass is a windowed ideal
//! low-pass (`sinc`) with a Kaiser window (J. F. Kaiser, "Nonrecursive
//! digital filter design using the I0-sinh window function", 1974;
//! Oppenheim & Schafer, *Discrete-Time Signal Processing*, §7.5–7.6 and
//! §4.6.2 on interpolation). Parameters:
//!
//! * cutoff at 0.94 × the input Nyquist frequency (e.g. 3.76 kHz for
//!   8 kHz input), so the transition band ends close to Nyquist and
//!   the first image band is attenuated;
//! * `2·K·M + 1` taps with `K = 24` input-rate zero crossings per side,
//!   making the group delay exactly `K·M` output samples (`K / rate`
//!   seconds: 3 ms at 8 kHz down to 1 ms at 24 kHz);
//! * Kaiser β = 9 (≈ 90 dB stop-band rejection per Kaiser's empirical
//!   formula `A ≈ 2.285·Δω·(N−1) + 8` / β = 0.1102·(A − 8.7)).
//!
//! The pass-band gain is normalised to exactly `M` per phase-sum so DC
//! passes at unity after zero-stuffing.

/// Input-rate sinc zero crossings on each side of the kernel centre.
const HALF_ZERO_CROSSINGS: usize = 24;
/// Kaiser window shape parameter.
const KAISER_BETA: f64 = 9.0;
/// Cutoff as a fraction of the input Nyquist frequency.
const CUTOFF_FRACTION: f64 = 0.94;

/// Streaming integer-factor upsampler for interleaved S16 audio.
#[derive(Debug, Clone)]
pub struct InputUpsampler {
    factor: usize,
    channels: usize,
    /// Polyphase bank: `phases[p][k]` multiplies `x[q − k]` for output
    /// sample `q·M + p`.
    phases: Vec<Vec<f32>>,
    /// Per-channel input history, most recent sample last; always
    /// `taps_per_phase` long.
    history: Vec<Vec<f32>>,
}

/// Zeroth-order modified Bessel function of the first kind (power
/// series), for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl InputUpsampler {
    /// Build an upsampler from `input_rate` Hz to 48 kHz for `channels`
    /// interleaved channels. `None` when `input_rate` is not one of
    /// 8 / 12 / 16 / 24 kHz.
    pub fn new(input_rate: u32, channels: usize) -> Option<Self> {
        let factor = match input_rate {
            8_000 => 6,
            12_000 => 4,
            16_000 => 3,
            24_000 => 2,
            _ => return None,
        };
        let len = 2 * HALF_ZERO_CROSSINGS * factor + 1;
        let centre = (len - 1) as f64 / 2.0;
        // Cutoff in cycles per output (48 kHz) sample.
        let fc = CUTOFF_FRACTION * 0.5 / factor as f64;
        let i0_beta = bessel_i0(KAISER_BETA);
        let proto: Vec<f64> = (0..len)
            .map(|n| {
                let t = n as f64 - centre;
                let sinc = if t == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t)
                };
                let r = t / centre;
                let w = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
                sinc * w
            })
            .collect();
        let taps_per_phase = len.div_ceil(factor);
        let mut phases = vec![vec![0f32; taps_per_phase]; factor];
        for (p, phase) in phases.iter_mut().enumerate() {
            // Normalise each phase to unity DC gain (the zero-stuffed
            // signal's gain of 1/M is restored per phase).
            let sum: f64 = (0..taps_per_phase)
                .filter_map(|k| proto.get(p + k * factor))
                .sum();
            for (k, tap) in phase.iter_mut().enumerate() {
                if let Some(h) = proto.get(p + k * factor) {
                    *tap = (h / sum) as f32;
                }
            }
        }
        Some(Self {
            factor,
            channels,
            phases,
            history: vec![vec![0.0; taps_per_phase]; channels],
        })
    }

    /// The integer interpolation factor `M`.
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Group delay in 48 kHz output samples (`K · M`).
    pub fn delay_samples(&self) -> usize {
        HALF_ZERO_CROSSINGS * self.factor
    }

    /// Input samples (per channel) still inside the filter: feeding this
    /// many zeros flushes the last real input out.
    pub fn tail_input_samples(&self) -> usize {
        HALF_ZERO_CROSSINGS
    }

    /// Upsample interleaved `input` (a whole number of frames) and
    /// append the interleaved 48 kHz result to `out`.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let ch = self.channels;
        out.reserve(input.len() * self.factor);
        let taps = self.history[0].len();
        let mut frame_out = vec![0f32; ch * self.factor];
        for frame in input.chunks_exact(ch) {
            for (c, &s) in frame.iter().enumerate() {
                let h = &mut self.history[c];
                h.copy_within(1.., 0);
                h[taps - 1] = f32::from(s);
                for (p, phase) in self.phases.iter().enumerate() {
                    // phase[k] pairs with x[q − k] = h[taps − 1 − k].
                    let acc: f32 = phase.iter().zip(h.iter().rev()).map(|(a, b)| a * b).sum();
                    frame_out[p * ch + c] = acc;
                }
            }
            out.extend(
                frame_out
                    .iter()
                    .map(|&v| v.round().clamp(-32_768.0, 32_767.0) as i16),
            );
        }
    }

    /// Clear the carried history.
    pub fn reset(&mut self) {
        for h in &mut self.history {
            h.iter_mut().for_each(|v| *v = 0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, freq: f64, n: usize, amp: f64) -> Vec<i16> {
        (0..n)
            .map(|i| {
                (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn rejects_unsupported_rates() {
        assert!(InputUpsampler::new(44_100, 1).is_none());
        assert!(InputUpsampler::new(48_000, 1).is_none());
        for r in [8_000, 12_000, 16_000, 24_000] {
            let u = InputUpsampler::new(r, 2).unwrap();
            assert_eq!(u.factor() as u32 * r, 48_000);
            assert_eq!(u.delay_samples(), 24 * u.factor());
        }
    }

    #[test]
    fn dc_passes_at_unity() {
        let mut u = InputUpsampler::new(16_000, 1).unwrap();
        let mut out = Vec::new();
        u.process(&vec![10_000i16; 400], &mut out);
        assert_eq!(out.len(), 1200);
        for &v in &out[300..] {
            assert!((i32::from(v) - 10_000).abs() <= 1, "{v}");
        }
    }

    /// A tone in band reproduces at 48 kHz (delay-compensated) with a
    /// small error; nothing appears at the first image frequency.
    #[test]
    fn tone_interpolates_cleanly() {
        for (rate, f) in [
            (8_000u32, 1_000.0),
            (12_000, 2_500.0),
            (16_000, 3_000.0),
            (24_000, 7_000.0),
        ] {
            let mut u = InputUpsampler::new(rate, 1).unwrap();
            let n_in = rate as usize / 2;
            let mut out = Vec::new();
            u.process(&tone(rate, f, n_in, 12_000.0), &mut out);
            let want = tone(48_000, f, out.len(), 12_000.0);
            let d = u.delay_samples();
            let (mut sig, mut err) = (0f64, 0f64);
            for i in 2_000..out.len() - 100 {
                let w = f64::from(want[i - d]);
                let e = f64::from(out[i]) - w;
                sig += w * w;
                err += e * e;
            }
            let snr = 10.0 * (sig / err).log10();
            assert!(snr > 60.0, "{rate} Hz: SNR {snr:.1} dB");
        }
    }

    #[test]
    fn interleaved_channels_stay_independent() {
        let mut u = InputUpsampler::new(24_000, 2).unwrap();
        let input: Vec<i16> = (0..200).flat_map(|_| [1_000i16, -2_000]).collect();
        let mut out = Vec::new();
        u.process(&input, &mut out);
        assert_eq!(out.len(), 800);
        let tail = &out[400..];
        assert!(tail
            .chunks_exact(2)
            .all(|f| (f[0] - 1_000).abs() <= 1 && (f[1] + 2_000).abs() <= 1));
    }
}
