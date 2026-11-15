//! O(N log N) evaluation of the CELT MDCT kernel pair (RFC 6716
//! §4.3.7 inverse, §5.3 forward).
//!
//! Both directions of the lapped transform are the same cosine kernel
//! `cos[(π/N)(t + 1/2 + N/2)(k + 1/2)]` over `2N` time samples and `N`
//! coefficients. Its time-domain symmetries (odd about `N/2 − 1/2`
//! after the half-block shift, period `4N` with sign flip at `2N`)
//! fold the `2N` samples onto `N`, which leaves a type-IV DCT
//!
//! ```text
//! D(k) = Σ_{n<N} v(n) · cos[(π/N)(n + 1/2)(k + 1/2)]
//! ```
//!
//! and pairing `v(2n)` with `v(N−1−2n)` into one complex sample turns
//! that DCT into a single `N/2`-point complex DFT between two
//! unit-modulus rotations:
//!
//! ```text
//! α(n,k) = π(4n+1)(4k+1)/(4N) = 2π·nk/(N/2) + π·n/N + π(4k+1)/(4N)
//! S(k)   = Σ_n (v(2n) + i·v(N−1−2n)) · e^{−iα(n,k)}
//! D(2k)  = Re S(k),   D(N−1−2k) = −Im S(k)
//! ```
//!
//! The DFT sizes Opus needs are 60, 120, 240 and 480 (`N` = 120, 240,
//! 480, 960), all of the form `2^a·3·5`; a recursive mixed-radix DFT
//! with a dedicated radix-2 butterfly and a direct small-prime
//! butterfly for 3 and 5 covers them. Plans (root tables and the two
//! rotation tables) are built once per size and shared; the transforms
//! themselves work on stack scratch and allocate nothing.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// Largest supported coefficient count (a 20 ms frame at 48 kHz).
pub(crate) const MAX_N: usize = 960;

#[derive(Clone, Copy, Default)]
struct Cx {
    re: f64,
    im: f64,
}

impl Cx {
    #[inline]
    fn mul(self, o: Cx) -> Cx {
        Cx {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }

    #[inline]
    fn add(self, o: Cx) -> Cx {
        Cx {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }

    #[inline]
    fn sub(self, o: Cx) -> Cx {
        Cx {
            re: self.re - o.re,
            im: self.im - o.im,
        }
    }

    fn unit(angle: f64) -> Cx {
        Cx {
            re: angle.cos(),
            im: angle.sin(),
        }
    }
}

/// Tables for one coefficient count `n` (DFT size `m = n/2`).
struct Plan {
    n: usize,
    m: usize,
    /// Radices of the DFT recursion, outermost first.
    radices: Vec<usize>,
    /// `e^{−2πi·j/m}` for `j < m`.
    roots: Vec<Cx>,
    /// `e^{−iπ·j/n}` for `j < m` (rotation before the DFT).
    pre: Vec<Cx>,
    /// `e^{−iπ(4k+1)/(4n)}` for `k < m` (rotation after the DFT).
    post: Vec<Cx>,
}

impl Plan {
    /// A plan for `n` coefficients, or `None` when `n` is odd, too
    /// large, or `n/2` has a prime factor other than 2, 3 and 5.
    fn new(n: usize) -> Option<Plan> {
        if n == 0 || !n.is_multiple_of(2) || n > MAX_N {
            return None;
        }
        let m = n / 2;
        let mut radices = Vec::new();
        let mut rest = m;
        for p in [5usize, 3, 2] {
            while rest.is_multiple_of(p) {
                radices.push(p);
                rest /= p;
            }
        }
        if rest != 1 {
            return None;
        }
        let nf = n as f64;
        Some(Plan {
            n,
            m,
            radices,
            roots: (0..m)
                .map(|j| Cx::unit(-2.0 * PI * j as f64 / m as f64))
                .collect(),
            pre: (0..m).map(|j| Cx::unit(-PI * j as f64 / nf)).collect(),
            post: (0..m)
                .map(|k| Cx::unit(-PI * (4 * k + 1) as f64 / (4.0 * nf)))
                .collect(),
        })
    }

    /// Forward DFT of the `out.len()` samples `input[0], input[stride],
    /// …` into `out`, recursing on `self.radices[level..]`.
    fn dft(&self, input: &[Cx], stride: usize, out: &mut [Cx], level: usize) {
        let len = out.len();
        if len == 1 {
            out[0] = input[0];
            return;
        }
        let p = self.radices[level];
        let sub = len / p;
        for (q, chunk) in out.chunks_exact_mut(sub).enumerate() {
            self.dft(&input[q * stride..], stride * p, chunk, level + 1);
        }
        // Combine the p interleaved sub-transforms:
        // X[k + r·sub] = Σ_q Y_q[k] · w^{q·k} · e^{−2πi·q·r/p},
        // with w = e^{−2πi/len}.
        let step = self.m / len;
        if p == 2 {
            let (lo, hi) = out.split_at_mut(sub);
            for (k, (a, b)) in lo.iter_mut().zip(hi.iter_mut()).enumerate() {
                let t = b.mul(self.roots[k * step]);
                let s = *a;
                *a = s.add(t);
                *b = s.sub(t);
            }
            return;
        }
        let prime_step = self.m / p;
        let mut turned = [Cx::default(); 5];
        for k in 0..sub {
            for (q, slot) in turned.iter_mut().enumerate().take(p) {
                *slot = out[q * sub + k].mul(self.roots[(q * k * step) % self.m]);
            }
            for r in 0..p {
                let mut acc = turned[0];
                for (q, &t) in turned.iter().enumerate().take(p).skip(1) {
                    acc = acc.add(t.mul(self.roots[((q * r) % p) * prime_step]));
                }
                out[k + r * sub] = acc;
            }
        }
    }

    /// Type-IV DCT: `d[k] = Σ_n v[n]·cos[(π/N)(n+1/2)(k+1/2)]`.
    fn dct4(&self, v: &[f64], d: &mut [f64]) {
        let (n, m) = (self.n, self.m);
        let mut z = [Cx::default(); MAX_N / 2];
        let mut spectrum = [Cx::default(); MAX_N / 2];
        for (j, (slot, &rot)) in z.iter_mut().zip(&self.pre).enumerate() {
            *slot = Cx {
                re: v[2 * j],
                im: v[n - 1 - 2 * j],
            }
            .mul(rot);
        }
        self.dft(&z[..m], 1, &mut spectrum[..m], 0);
        for (k, (&s, &rot)) in spectrum.iter().zip(&self.post).enumerate() {
            let c = s.mul(rot);
            d[2 * k] = c.re;
            d[n - 1 - 2 * k] = -c.im;
        }
    }
}

/// The shared plan for `n` coefficients (Opus sizes only).
fn plan(n: usize) -> Option<&'static Plan> {
    static PLANS: [OnceLock<Option<Plan>>; 4] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    let slot = match n {
        120 => 0,
        240 => 1,
        480 => 2,
        960 => 3,
        _ => return None,
    };
    PLANS[slot].get_or_init(|| Plan::new(n)).as_ref()
}

/// Inverse kernel: `out[t] = Σ_k spec[k]·cos[(π/N)(t+1/2+N/2)(k+1/2)]`
/// for `t < 2N`, `N = spec.len()` (unit scale). Returns `false`
/// (leaving `out` untouched) when `N` is not an Opus block size.
pub(crate) fn imdct_unscaled(spec: &[f64], out: &mut [f64]) -> bool {
    let n = spec.len();
    let Some(plan) = plan(n) else {
        return false;
    };
    debug_assert_eq!(out.len(), 2 * n);
    let mut w = [0.0f64; MAX_N];
    plan.dct4(spec, &mut w[..n]);
    let half = n / 2;
    // Unfold: t in [0, N/2) reads w rising from N/2; t in [N/2, 3N/2)
    // reads −w falling from N−1; t in [3N/2, 2N) reads −w rising
    // from 0.
    for (slot, &v) in out[..half].iter_mut().zip(&w[half..n]) {
        *slot = v;
    }
    for (slot, &v) in out[half..half + n].iter_mut().zip(w[..n].iter().rev()) {
        *slot = -v;
    }
    for (slot, &v) in out[half + n..].iter_mut().zip(&w[..half]) {
        *slot = -v;
    }
    true
}

/// Forward kernel: `coeffs[k] = Σ_{t<2N} x[t]·cos[(π/N)(t+1/2+N/2)(k+1/2)]`
/// for `k < N`, `N = coeffs.len()` (unit scale). Returns `false`
/// (leaving `coeffs` untouched) when `N` is not an Opus block size.
pub(crate) fn mdct_unscaled(x: &[f64], coeffs: &mut [f64]) -> bool {
    let n = coeffs.len();
    let Some(plan) = plan(n) else {
        return false;
    };
    debug_assert_eq!(x.len(), 2 * n);
    let half = n / 2;
    let mut v = [0.0f64; MAX_N];
    // Fold 2N samples onto N (see the module docs).
    for (j, slot) in v[..half].iter_mut().enumerate() {
        *slot = -x[3 * half + j] - x[3 * half - 1 - j];
    }
    for (j, slot) in v[half..n].iter_mut().enumerate() {
        *slot = x[j] - x[n - 1 - j];
    }
    plan.dct4(&v[..n], coeffs);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u32) -> Vec<f64> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                f64::from(state >> 8) / f64::from(1u32 << 24) - 0.5
            })
            .collect()
    }

    fn kernel(n: usize, t: usize, k: usize) -> f64 {
        let nf = n as f64;
        ((PI / nf) * (t as f64 + 0.5 + nf / 2.0) * (k as f64 + 0.5)).cos()
    }

    #[test]
    fn inverse_matches_the_direct_sum_at_every_block_size() {
        for n in [120usize, 240, 480, 960] {
            let spec = noise(n, n as u32);
            let mut fast = vec![0.0; 2 * n];
            assert!(imdct_unscaled(&spec, &mut fast));
            for t in (0..2 * n)
                .step_by(7)
                .chain([0, n / 2 - 1, n / 2, 2 * n - 1])
            {
                let direct: f64 = (0..n).map(|k| spec[k] * kernel(n, t, k)).sum();
                assert!(
                    (fast[t] - direct).abs() < 1e-10,
                    "n {n} t {t}: {} vs {direct}",
                    fast[t]
                );
            }
        }
    }

    #[test]
    fn forward_matches_the_direct_sum_at_every_block_size() {
        for n in [120usize, 240, 480, 960] {
            let x = noise(2 * n, 7 + n as u32);
            let mut fast = vec![0.0; n];
            assert!(mdct_unscaled(&x, &mut fast));
            for k in (0..n).step_by(5).chain([0, 1, n - 1]) {
                let direct: f64 = (0..2 * n).map(|t| x[t] * kernel(n, t, k)).sum();
                assert!(
                    (fast[k] - direct).abs() < 1e-10,
                    "n {n} k {k}: {} vs {direct}",
                    fast[k]
                );
            }
        }
    }

    #[test]
    fn other_sizes_are_declined() {
        let mut out = [0.0; 14];
        assert!(!imdct_unscaled(&[0.0; 7], &mut out));
        let mut coeffs = [0.0; 100];
        assert!(!mdct_unscaled(&[0.0; 200], &mut coeffs));
        assert!(Plan::new(14).is_none(), "7 is not a supported radix");
        assert!(Plan::new(1920).is_none());
        assert!(Plan::new(60).is_some());
    }
}
