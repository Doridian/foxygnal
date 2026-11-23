//! IIR filters: RBJ-cookbook biquads (in cascades) and a DC blocker. State
//! is kept in f64, which matters for poles close to z = 1 (low cut-offs at
//! high sample rates). Samples are plain floats with `lanes` interleaved
//! channels (1 for real, 2 for complex re/im), filtered independently.

use std::f64::consts::TAU;

/// The RBJ "Audio EQ Cookbook" filter shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiquadKind {
    LowPass,
    HighPass,
    /// Band-pass with 0 dB peak gain.
    BandPass,
    Notch,
    AllPass,
    /// Bell: `gain_db` at `f0`, 0 dB far away.
    Peaking,
    /// `gain_db` below `f0`, 0 dB above (half the gain at `f0`).
    LowShelf,
    /// 0 dB below `f0`, `gain_db` above (half the gain at `f0`).
    HighShelf,
}

/// Normalized biquad coefficients (`a0 = 1`):
/// `H(z) = (b0 + b1 z⁻¹ + b2 z⁻²) / (1 + a1 z⁻¹ + a2 z⁻²)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    /// Design per the cookbook. `f0` is normalized (cycles/sample, 0..0.5),
    /// `q` > 0; `gain_db` is used by peaking and shelf filters only.
    pub fn design(kind: BiquadKind, f0: f64, q: f64, gain_db: f64) -> Self {
        let w0 = TAU * f0;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f64.powf(gain_db / 40.0);
        let sa = 2.0 * a.sqrt() * alpha;
        let (b0, b1, b2, a0, a1, a2) = match kind {
            BiquadKind::LowPass => (
                (1.0 - cos) / 2.0,
                1.0 - cos,
                (1.0 - cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BiquadKind::HighPass => (
                (1.0 + cos) / 2.0,
                -(1.0 + cos),
                (1.0 + cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BiquadKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BiquadKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BiquadKind::AllPass => (
                1.0 - alpha,
                -2.0 * cos,
                1.0 + alpha,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BiquadKind::Peaking => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            BiquadKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sa),
                (a + 1.0) + (a - 1.0) * cos + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sa,
            ),
            BiquadKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sa),
                (a + 1.0) - (a - 1.0) * cos + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sa,
            ),
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// |H| at normalized frequency `f`.
    pub fn magnitude_at(&self, f: f64) -> f64 {
        let (s, c) = (-TAU * f).sin_cos();
        let (s2, c2) = (-2.0 * TAU * f).sin_cos();
        let num = (self.b0 + self.b1 * c + self.b2 * c2).hypot(self.b1 * s + self.b2 * s2);
        let den = (1.0 + self.a1 * c + self.a2 * c2).hypot(self.a1 * s + self.a2 * s2);
        num / den
    }
}

/// Values below this in the filter state are flushed to zero, so a decaying
/// state never lingers in (slow) subnormal numbers.
const FLUSH: f64 = 1e-200;

/// A cascade of identical biquads (transposed direct form II) over
/// interleaved channels. Allocates only when built.
#[derive(Debug, Clone)]
pub struct BiquadCascade {
    c: Biquad,
    stages: usize,
    lanes: usize,
    /// `stages × lanes` pairs `(z1, z2)`.
    state: Vec<[f64; 2]>,
}

impl BiquadCascade {
    pub fn new(c: Biquad, stages: usize, lanes: usize) -> Self {
        let (stages, lanes) = (stages.max(1), lanes.max(1));
        Self {
            c,
            stages,
            lanes,
            state: vec![[0.0; 2]; stages * lanes],
        }
    }

    /// New coefficients, keeping the state (no click, no allocation).
    pub fn set(&mut self, c: Biquad) {
        self.c = c;
    }

    pub fn coeffs(&self) -> Biquad {
        self.c
    }

    pub fn stages(&self) -> usize {
        self.stages
    }

    pub fn reset(&mut self) {
        self.state.fill([0.0; 2]);
    }

    /// |H| of the whole cascade at normalized frequency `f`.
    pub fn magnitude_at(&self, f: f64) -> f64 {
        self.c.magnitude_at(f).powi(self.stages as i32)
    }

    /// Filter `x` into `y` (whole frames of `lanes` floats, up to the shorter
    /// slice).
    pub fn process(&mut self, x: &[f32], y: &mut [f32]) {
        let Biquad { b0, b1, b2, a1, a2 } = self.c;
        let lanes = self.lanes;
        let n = x.len().min(y.len()) / lanes * lanes;
        for (xs, ys) in x[..n]
            .chunks_exact(lanes)
            .zip(y[..n].chunks_exact_mut(lanes))
        {
            for (l, (&xi, yo)) in xs.iter().zip(ys.iter_mut()).enumerate() {
                let mut v = xi as f64;
                for z in self.state[l..].iter_mut().step_by(lanes) {
                    let out = b0 * v + z[0];
                    z[0] = b1 * v - a1 * out + z[1];
                    z[1] = b2 * v - a2 * out;
                    v = out;
                }
                *yo = v as f32;
            }
        }
        for z in &mut self.state {
            for s in z.iter_mut() {
                if s.abs() < FLUSH {
                    *s = 0.0;
                }
            }
        }
    }
}

/// Single-pole DC blocker, `H(z) = g·(1 − z⁻¹) / (1 − R·z⁻¹)` with
/// `g = (1 + R)/2` (unity gain at Nyquist).
#[derive(Debug, Clone)]
pub struct DcBlocker {
    r: f64,
    g: f64,
    lanes: usize,
    /// Per lane: previous input, previous output.
    state: Vec<[f64; 2]>,
}

impl DcBlocker {
    /// The pole radius that puts the −3 dB point at normalized `fc`
    /// (0 < fc < 0.25): `R = (1 − sin ω)/cos ω`.
    pub fn pole_for(fc: f64) -> f64 {
        let (s, c) = (TAU * fc).sin_cos();
        ((1.0 - s) / c).clamp(0.0, 1.0)
    }

    pub fn new(fc: f64, lanes: usize) -> Self {
        let mut d = Self {
            r: 0.0,
            g: 1.0,
            lanes: lanes.max(1),
            state: vec![[0.0; 2]; lanes.max(1)],
        };
        d.set_cutoff(fc);
        d
    }

    /// Move the −3 dB point, keeping the state.
    pub fn set_cutoff(&mut self, fc: f64) {
        self.r = Self::pole_for(fc);
        self.g = (1.0 + self.r) / 2.0;
    }

    /// |H| at normalized frequency `f`.
    pub fn magnitude_at(&self, f: f64) -> f64 {
        if f <= 0.0 {
            return 0.0;
        }
        let w = TAU * f;
        let num = 2.0 * (w / 2.0).sin().abs();
        let den = (1.0 - self.r * w.cos()).hypot(self.r * w.sin());
        self.g * num / den
    }

    pub fn process(&mut self, x: &[f32], y: &mut [f32]) {
        let (r, g, lanes) = (self.r, self.g, self.lanes);
        let n = x.len().min(y.len()) / lanes * lanes;
        for (xs, ys) in x[..n]
            .chunks_exact(lanes)
            .zip(y[..n].chunks_exact_mut(lanes))
        {
            for ((&xi, yo), s) in xs.iter().zip(ys.iter_mut()).zip(self.state.iter_mut()) {
                let xi = xi as f64;
                let out = xi - s[0] + r * s[1];
                s[0] = xi;
                s[1] = if out.abs() < FLUSH { 0.0 } else { out };
                *yo = (g * out) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_gain(f: f64, mut run: impl FnMut(&[f32], &mut [f32])) -> f64 {
        let n = 20_000;
        let x: Vec<f32> = (0..n).map(|i| (TAU * f * i as f64).sin() as f32).collect();
        let mut y = vec![0.0; n];
        run(&x, &mut y);
        let tail = &y[n / 2..];
        (2.0 * tail.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / tail.len() as f64).sqrt()
    }

    #[test]
    fn cookbook_gains_at_f0() {
        let f0 = 0.05;
        let db = |m: f64| 20.0 * m.log10();
        let lp = Biquad::design(BiquadKind::LowPass, f0, 2.0, 0.0);
        assert!((lp.magnitude_at(f0) - 2.0).abs() < 1e-9); // |H(f0)| = Q
        assert!((lp.magnitude_at(0.0) - 1.0).abs() < 1e-9);
        let hp = Biquad::design(BiquadKind::HighPass, f0, 0.5, 0.0);
        assert!((hp.magnitude_at(f0) - 0.5).abs() < 1e-9);
        assert!((hp.magnitude_at(0.5) - 1.0).abs() < 1e-9);
        let bp = Biquad::design(BiquadKind::BandPass, f0, 3.0, 0.0);
        assert!((bp.magnitude_at(f0) - 1.0).abs() < 1e-9);
        let notch = Biquad::design(BiquadKind::Notch, f0, 3.0, 0.0);
        assert!(notch.magnitude_at(f0) < 1e-9);
        let ap = Biquad::design(BiquadKind::AllPass, f0, 1.0, 0.0);
        for f in [0.0, 0.01, f0, 0.3] {
            assert!((ap.magnitude_at(f) - 1.0).abs() < 1e-9);
        }
        let pk = Biquad::design(BiquadKind::Peaking, f0, 1.0, 6.0);
        assert!((db(pk.magnitude_at(f0)) - 6.0).abs() < 1e-9);
        assert!(db(pk.magnitude_at(0.0)).abs() < 1e-9);
        let ls = Biquad::design(BiquadKind::LowShelf, f0, 0.707, -12.0);
        assert!((db(ls.magnitude_at(f0)) + 6.0).abs() < 1e-6);
        assert!((db(ls.magnitude_at(0.0)) + 12.0).abs() < 1e-6);
        let hs = Biquad::design(BiquadKind::HighShelf, f0, 0.707, 10.0);
        assert!((db(hs.magnitude_at(f0)) - 5.0).abs() < 1e-6);
        assert!((db(hs.magnitude_at(0.5)) - 10.0).abs() < 1e-6);
    }

    #[test]
    fn cascade_runs_like_its_response() {
        let c = Biquad::design(BiquadKind::LowPass, 0.02, 0.707, 0.0);
        for stages in [1, 3] {
            let mut bq = BiquadCascade::new(c, stages, 1);
            for f in [0.005, 0.02, 0.08] {
                bq.reset();
                let want = bq.magnitude_at(f);
                let got = tone_gain(f, |x, y| bq.process(x, y));
                assert!(
                    (got - want).abs() < 1e-3,
                    "{stages} stages at {f}: {got} vs {want}"
                );
            }
        }
        // Two lanes filter independently: lane 1 all zeros stays zero.
        let mut bq = BiquadCascade::new(c, 2, 2);
        let x: Vec<f32> = (0..100).flat_map(|i| [(i % 7) as f32, 0.0]).collect();
        let mut y = vec![9.0; 200];
        bq.process(&x, &mut y);
        assert!(y.iter().skip(1).step_by(2).all(|&v| v == 0.0));
    }

    #[test]
    fn dc_blocker_cutoff_and_attenuation() {
        let fc = 0.001;
        let d = DcBlocker::new(fc, 1);
        let db = |m: f64| 20.0 * m.log10();
        assert!((db(d.magnitude_at(fc)) + 3.0103).abs() < 1e-3);
        assert!((d.magnitude_at(0.5) - 1.0).abs() < 1e-12);
        assert!(d.magnitude_at(0.0) == 0.0);
        // A DC offset under a tone is removed; the tone passes.
        let mut d = DcBlocker::new(fc, 1);
        let n = 50_000;
        let x: Vec<f32> = (0..n)
            .map(|i| 0.7 + 0.5 * (TAU * 0.05 * i as f64).sin() as f32)
            .collect();
        let mut y = vec![0.0; n];
        d.process(&x, &mut y);
        let tail = &y[n - 10_000..];
        let mean = tail.iter().map(|&v| v as f64).sum::<f64>() / tail.len() as f64;
        assert!(mean.abs() < 1e-3, "DC left: {mean}");
        let peak = tail.iter().fold(0f32, |m, &v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.01, "tone peak {peak}");
    }
}
