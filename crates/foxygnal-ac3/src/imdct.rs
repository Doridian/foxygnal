//! The inverse transform (A/52 7.9.4): a 512-sample IMDCT of 256
//! coefficients, or two 256-sample ones of 128 each for a switched block,
//! with the KBD window and overlap-add.
//!
//! An IMDCT's 2M outputs are mirror images of M values, the DCT-IV of the
//! coefficients, so only those are kept: `half` is that DCT-IV reversed
//! (the second half of the window's input, in time order, then the first).
//! The DCT-IV is the usual M/2-point complex FFT between two twiddles.

use crate::tables::{cos_sin, window};

/// Twiddles and tables for M = 256 and 128 (FFTs of 128 and 64 points).
pub struct Imdct {
    /// e^{−iπ(m + 1/8)/M}, m < M/2: [re, im], for M = 256 and M = 128.
    tw256: [[f32; 2]; 128],
    tw128: [[f32; 2]; 64],
    /// e^{−2πik/128}, k < 64; the 64-point FFT takes every other one.
    roots: [[f32; 2]; 64],
    window: [f32; 256],
}

impl Imdct {
    pub fn new() -> Self {
        use core::f64::consts::PI;
        let mut tw256 = [[0f32; 2]; 128];
        for (m, t) in tw256.iter_mut().enumerate() {
            let (c, s) = cos_sin(-PI * (m as f64 + 0.125) / 256.0);
            *t = [c as f32, s as f32];
        }
        let mut tw128 = [[0f32; 2]; 64];
        for (m, t) in tw128.iter_mut().enumerate() {
            let (c, s) = cos_sin(-PI * (m as f64 + 0.125) / 128.0);
            *t = [c as f32, s as f32];
        }
        let mut roots = [[0f32; 2]; 64];
        for (k, r) in roots.iter_mut().enumerate() {
            let (c, s) = cos_sin(-2.0 * PI * k as f64 / 128.0);
            *r = [c as f32, s as f32];
        }
        Imdct {
            tw256,
            tw128,
            roots,
            window: window(),
        }
    }

    /// The DCT-IV of `x` (M = x.len(), 256 or 128, every `step`-th value
    /// from `from`), reversed, into `out[..M]`:
    /// out[M − 1 − n] = Σₖ x[k] cos(π/M (n + ½)(k + ½)).
    fn half(&self, x: &[f32], from: usize, step: usize, m: usize, out: &mut [f32]) {
        let tw: &[[f32; 2]] = if m == 256 { &self.tw256 } else { &self.tw128 };
        let n2 = m / 2;
        let mut z = [[0f32; 2]; 128];
        let at = |k: usize| x[from + k * step];
        for (j, zj) in z[..n2].iter_mut().enumerate() {
            let (a, b) = (at(2 * j), at(m - 1 - 2 * j));
            let [c, s] = tw[j];
            *zj = [a * c - b * s, a * s + b * c];
        }
        fft(&mut z[..n2], &self.roots, 128 / n2);
        for (j, &[re, im]) in z[..n2].iter().enumerate() {
            let [c, s] = tw[j];
            let (wr, wi) = (re * c - im * s, re * s + im * c);
            // u[2j] = Re w, u[M − 1 − 2j] = −Im w; out is u reversed.
            out[m - 1 - 2 * j] = wr;
            out[2 * j] = -wi;
        }
    }

    /// One block of one channel: 256 coefficients (scaled already) to 256
    /// samples, with `delay` (128 values) carried between blocks.
    pub fn block(&self, coef: &[f32; 256], short: bool, delay: &mut [f32; 128], out: &mut [f32]) {
        let mut t = [0f32; 256];
        if short {
            let (first, second) = t.split_at_mut(128);
            self.half(coef, 0, 2, 128, first);
            self.half(coef, 1, 2, 128, second);
        } else {
            self.half(coef, 0, 1, 256, &mut t);
        }
        let w = &self.window;
        for a in 0..128 {
            let (d, s) = (delay[a], t[127 - a]);
            out[a] = d * w[255 - a] - s * w[a];
            out[255 - a] = d * w[a] + s * w[255 - a];
        }
        delay.copy_from_slice(&t[128..]);
    }
}

/// In-place radix-2 FFT (forward, e^{−2πikn/N}) of N = z.len() points;
/// `roots[k·stride]` is e^{−2πik/N}.
fn fft(z: &mut [[f32; 2]], roots: &[[f32; 2]], stride: usize) {
    let n = z.len();
    // Bit reversal.
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            z.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let step = stride * (n / len);
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let [wr, wi] = roots[k * step];
                let [br, bi] = z[start + k + half];
                let t = [br * wr - bi * wi, br * wi + bi * wr];
                let a = z[start + k];
                z[start + k] = [a[0] + t[0], a[1] + t[1]];
                z[start + k + half] = [a[0] - t[0], a[1] - t[1]];
            }
        }
        len *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg's half-length IMDCT, straight from its definition.
    fn naive(x: &[f64]) -> Vec<f64> {
        let m = x.len();
        let half = m / 2;
        let phase = core::f64::consts::PI / (4.0 * m as f64);
        let mut out = vec![0.0; m];
        for i in 0..half {
            let (mut d, mut u) = (0.0, 0.0);
            let id = phase * (2 * m - 2 * i - 1) as f64;
            let iu = phase * (3 * m + 2 * i + 1) as f64;
            for (j, &v) in x.iter().enumerate() {
                let a = (2 * j + 1) as f64;
                d += (a * id).cos() * v;
                u += (a * iu).cos() * v;
            }
            out[i] = d;
            out[i + half] = -u;
        }
        out
    }

    #[test]
    fn matches_the_definition() {
        let t = Imdct::new();
        let mut seed = 1u32;
        for m in [256, 128] {
            let x: Vec<f32> = (0..m)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
                })
                .collect();
            let mut out = vec![0f32; m];
            t.half(&x, 0, 1, m, &mut out);
            let want = naive(&x.iter().map(|&v| f64::from(v)).collect::<Vec<_>>());
            for (a, b) in out.iter().zip(&want) {
                assert!((f64::from(*a) - b).abs() < 1e-4, "{m}: {a} vs {b}");
            }
        }
    }
}
