//! FIR filter design: windowed-sinc low-pass, high-pass, band-pass and
//! band-stop filters, tap-count estimates and frequency responses.
//!
//! Frequencies here are *normalized*: cycles per sample, so 0.5 is the
//! Nyquist frequency. Blocks convert from Hz with the stream's sample rate.

use std::f64::consts::{PI, TAU};

use serde::{Deserialize, Serialize};

/// The most taps a design may have. Beyond this the transition band is
/// unreasonably narrow for the sample rate and the filter would be too slow.
pub const MAX_TAPS: usize = 32_767;

/// The window applied to the ideal (sinc) impulse response.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignWindow {
    /// About 53 dB stopband attenuation, narrow transition. A good default.
    Hamming,
    /// About 74 dB stopband attenuation, wider transition than Hamming.
    Blackman,
    /// Adjustable: the attenuation (dB) sets the shape and the tap count.
    Kaiser { attenuation: f64 },
}

impl DesignWindow {
    /// Typical stopband attenuation of a design with this window, in dB.
    pub fn attenuation(self) -> f64 {
        match self {
            Self::Hamming => 53.0,
            Self::Blackman => 74.0,
            Self::Kaiser { attenuation } => attenuation,
        }
    }

    /// Number of taps (always odd) for a transition band `transition` wide
    /// (normalized, cycles/sample).
    pub fn num_taps(self, transition: f64) -> usize {
        let n = match self {
            Self::Hamming => 3.3 / transition,
            Self::Blackman => 5.5 / transition,
            Self::Kaiser { attenuation } => (attenuation - 7.95) / (14.36 * transition) + 1.0,
        };
        let n = if n.is_finite() {
            n.ceil().max(3.0).min(MAX_TAPS as f64 + 2.0) as usize
        } else {
            MAX_TAPS + 2
        };
        n | 1
    }

    /// The window value at tap `i` of `n` (symmetric form).
    pub fn value(self, i: usize, n: usize) -> f64 {
        if n <= 1 {
            return 1.0;
        }
        let x = i as f64 / (n - 1) as f64;
        match self {
            Self::Hamming => 0.54 - 0.46 * (TAU * x).cos(),
            Self::Blackman => 0.42 - 0.5 * (TAU * x).cos() + 0.08 * (2.0 * TAU * x).cos(),
            Self::Kaiser { attenuation } => {
                let beta = kaiser_beta(attenuation);
                let r = 2.0 * x - 1.0;
                bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
            }
        }
    }
}

/// Kaiser's formula for the window shape that reaches `attenuation` dB.
pub fn kaiser_beta(attenuation: f64) -> f64 {
    let a = attenuation;
    if a > 50.0 {
        0.1102 * (a - 8.7)
    } else if a >= 21.0 {
        0.5842 * (a - 21.0).powf(0.4) + 0.07886 * (a - 21.0)
    } else {
        0.0
    }
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Which frequencies a filter passes. Edges are normalized (0 < f < 0.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Band {
    LowPass(f64),
    HighPass(f64),
    BandPass(f64, f64),
    BandStop(f64, f64),
}

impl Band {
    /// A frequency in the passband where the gain is normalized.
    fn reference(self) -> f64 {
        match self {
            Self::LowPass(_) | Self::BandStop(..) => 0.0,
            Self::HighPass(_) => 0.5,
            Self::BandPass(a, b) => (a + b) / 2.0,
        }
    }
}

/// Why a design was rejected. Normalized frequencies; the caller turns this
/// into a message in Hz.
#[derive(Debug, Clone, PartialEq)]
pub enum DesignError {
    /// An edge is not strictly between 0 and Nyquist.
    EdgeOutOfRange(f64),
    /// Band edges are not in increasing order.
    EdgesOrder,
    /// The transition width is not positive.
    Transition,
    /// More than [`MAX_TAPS`] taps would be needed.
    TooManyTaps(usize),
}

/// A windowed-sinc FIR design.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FirSpec {
    pub band: Band,
    /// Width of each transition band, normalized.
    pub transition: f64,
    pub window: DesignWindow,
    /// Gain in the passband (at DC for low-pass / band-stop, Nyquist for
    /// high-pass, the band centre for band-pass).
    pub gain: f64,
}

impl FirSpec {
    /// Check the edges and transition; returns the tap count.
    pub fn validate(&self) -> Result<usize, DesignError> {
        let edges: &[f64] = match &self.band {
            Band::LowPass(f) | Band::HighPass(f) => std::slice::from_ref(f),
            Band::BandPass(a, b) | Band::BandStop(a, b) => {
                if a >= b {
                    return Err(DesignError::EdgesOrder);
                }
                &[*a, *b]
            }
        };
        if let Some(&f) = edges.iter().find(|f| !(**f > 0.0 && **f < 0.5)) {
            return Err(DesignError::EdgeOutOfRange(f));
        }
        if !(self.transition > 0.0 && self.transition.is_finite()) {
            return Err(DesignError::Transition);
        }
        let n = self.num_taps();
        if n > MAX_TAPS {
            return Err(DesignError::TooManyTaps(n));
        }
        Ok(n)
    }

    pub fn num_taps(&self) -> usize {
        self.window.num_taps(self.transition)
    }

    /// Design the taps.
    pub fn design(&self) -> Result<Vec<f32>, DesignError> {
        let n = self.validate()?;
        let mut taps = vec![0.0; n];
        self.design_into(&mut taps)?;
        Ok(taps)
    }

    /// Design into an existing buffer of length [`Self::num_taps`], without
    /// allocating (for retuning a running filter).
    pub fn design_into(&self, taps: &mut [f32]) -> Result<(), DesignError> {
        let n = self.validate()?;
        assert_eq!(taps.len(), n, "design_into: wrong tap count");
        let mid = (n / 2) as f64;
        // Ideal low-pass of normalized cutoff `fc`, centred on `mid`.
        let lp = |fc: f64, i: usize| {
            let t = i as f64 - mid;
            if t == 0.0 {
                2.0 * fc
            } else {
                (TAU * fc * t).sin() / (PI * t)
            }
        };
        let delta = |i: usize| if i == n / 2 { 1.0 } else { 0.0 };
        for (i, h) in taps.iter_mut().enumerate() {
            let ideal = match self.band {
                Band::LowPass(f) => lp(f, i),
                Band::HighPass(f) => delta(i) - lp(f, i),
                Band::BandPass(a, b) => lp(b, i) - lp(a, i),
                Band::BandStop(a, b) => delta(i) - lp(b, i) + lp(a, i),
            };
            *h = (ideal * self.window.value(i, n)) as f32;
        }
        let at_ref = magnitude_at(taps, self.band.reference());
        if at_ref > 0.0 {
            let scale = (self.gain / at_ref) as f32;
            taps.iter_mut().for_each(|h| *h *= scale);
        }
        Ok(())
    }
}

/// A low-pass for rate changes: passes up to `(1 − transition/2)·nyquist` of
/// the *slower* side and stops at its Nyquist frequency, designed at the
/// rate the filter runs at (`interp` × the input rate). The gain is `interp`
/// so interpolation keeps amplitudes. `transition` is a fraction of the
/// slower side's Nyquist (0.2 is a sensible default).
pub fn resampler_lowpass(interp: usize, decim: usize, transition: f64) -> FirSpec {
    // Nyquist of the slower side, normalized to the filter's rate.
    let nyq = 0.5 / interp.max(decim) as f64;
    let tw = transition.clamp(0.01, 1.0) * nyq;
    FirSpec {
        band: Band::LowPass(nyq - tw / 2.0),
        transition: tw,
        window: DesignWindow::Kaiser { attenuation: 60.0 },
        gain: interp as f64,
    }
}

/// |H(f)| of real taps at normalized frequency `f`.
pub fn magnitude_at(taps: &[f32], f: f64) -> f64 {
    // Σ h[k]·e^{−j2πfk}, with the exponential advanced by a complex
    // multiply per tap instead of a sin/cos (previews evaluate this a lot).
    let (s, c) = (-TAU * f).sin_cos();
    let (mut pr, mut pi) = (1.0f64, 0.0f64);
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for &h in taps {
        re += h as f64 * pr;
        im += h as f64 * pi;
        (pr, pi) = (pr * c - pi * s, pr * s + pi * c);
    }
    re.hypot(im)
}

/// Magnitude response in dB of real taps at `points` frequencies evenly
/// spaced from 0 to Nyquist (inclusive). Returns (normalized freqs, dB).
pub fn response_db(taps: &[f32], points: usize) -> (Vec<f64>, Vec<f32>) {
    let points = points.max(2);
    let freqs: Vec<f64> = (0..points)
        .map(|i| 0.5 * i as f64 / (points - 1) as f64)
        .collect();
    let db = freqs
        .iter()
        .map(|&f| to_db(magnitude_at(taps, f)))
        .collect();
    (freqs, db)
}

/// Floor at -200 dB so a perfect null stays finite (JSON has no −∞).
pub fn to_db(mag: f64) -> f32 {
    (20.0 * mag.max(1e-10).log10()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_at(taps: &[f32], f: f64) -> f64 {
        20.0 * magnitude_at(taps, f).log10()
    }

    /// Worst (highest) gain in [a, b], in dB.
    fn worst(taps: &[f32], a: f64, b: f64) -> f64 {
        (0..=200)
            .map(|i| db_at(taps, a + (b - a) * i as f64 / 200.0))
            .fold(f64::MIN, f64::max)
    }

    /// Passband ripple in [a, b]: max − min in dB.
    fn ripple(taps: &[f32], a: f64, b: f64) -> f64 {
        let v: Vec<f64> = (0..=200)
            .map(|i| db_at(taps, a + (b - a) * i as f64 / 200.0))
            .collect();
        v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
    }

    #[test]
    fn low_pass_meets_its_window_attenuation() {
        for window in [
            DesignWindow::Hamming,
            DesignWindow::Blackman,
            DesignWindow::Kaiser { attenuation: 80.0 },
        ] {
            let spec = FirSpec {
                band: Band::LowPass(0.1),
                transition: 0.02,
                window,
                gain: 2.0,
            };
            let taps = spec.design().unwrap();
            assert_eq!(taps.len() % 2, 1);
            // DC gain = gain.
            let dc: f32 = taps.iter().sum();
            assert!((dc - 2.0).abs() < 1e-4, "{window:?}: dc {dc}");
            let stop = worst(&taps, 0.11, 0.5) - 20.0 * 2f64.log10();
            assert!(
                stop < -(window.attenuation() - 3.0),
                "{window:?}: stopband {stop} dB"
            );
            let r = ripple(&taps, 0.0, 0.09);
            assert!(r < 0.1, "{window:?}: ripple {r} dB");
        }
    }

    #[test]
    fn kaiser_taps_grow_with_attenuation() {
        let a = DesignWindow::Kaiser { attenuation: 40.0 }.num_taps(0.01);
        let b = DesignWindow::Kaiser { attenuation: 90.0 }.num_taps(0.01);
        assert!(b > 2 * a, "{a} {b}");
        assert!((kaiser_beta(60.0) - 5.65326).abs() < 1e-4);
        assert_eq!(kaiser_beta(10.0), 0.0);
    }

    #[test]
    fn high_band_and_stop() {
        let mk = |band| {
            FirSpec {
                band,
                transition: 0.02,
                window: DesignWindow::Blackman,
                gain: 1.0,
            }
            .design()
            .unwrap()
        };
        let hp = mk(Band::HighPass(0.2));
        assert!(db_at(&hp, 0.5).abs() < 0.01);
        assert!(worst(&hp, 0.0, 0.19) < -70.0);
        assert!(ripple(&hp, 0.21, 0.5) < 0.05);

        let bp = mk(Band::BandPass(0.1, 0.2));
        assert!(db_at(&bp, 0.15).abs() < 0.01);
        assert!(worst(&bp, 0.0, 0.09) < -70.0);
        assert!(worst(&bp, 0.21, 0.5) < -70.0);
        assert!(ripple(&bp, 0.11, 0.19) < 0.05);

        let bs = mk(Band::BandStop(0.1, 0.2));
        assert!(db_at(&bs, 0.0).abs() < 0.01);
        assert!(db_at(&bs, 0.5).abs() < 0.05);
        assert!(worst(&bs, 0.11, 0.19) < -70.0);
    }

    #[test]
    fn design_into_matches_design_and_rejects_bad_specs() {
        let spec = FirSpec {
            band: Band::LowPass(0.2),
            transition: 0.05,
            window: DesignWindow::Hamming,
            gain: 1.0,
        };
        let a = spec.design().unwrap();
        let mut b = vec![0.0; a.len()];
        spec.design_into(&mut b).unwrap();
        assert_eq!(a, b);

        let bad = |band, transition| FirSpec {
            band,
            transition,
            ..spec
        };
        assert_eq!(
            bad(Band::LowPass(0.6), 0.05).design(),
            Err(DesignError::EdgeOutOfRange(0.6))
        );
        assert_eq!(
            bad(Band::BandPass(0.3, 0.2), 0.05).design(),
            Err(DesignError::EdgesOrder)
        );
        assert_eq!(
            bad(Band::LowPass(0.2), 0.0).design(),
            Err(DesignError::Transition)
        );
        assert!(matches!(
            bad(Band::LowPass(0.2), 1e-6).design(),
            Err(DesignError::TooManyTaps(_))
        ));
    }

    #[test]
    fn response_spans_dc_to_nyquist() {
        let taps = [0.5f32, 0.5];
        let (f, db) = response_db(&taps, 5);
        assert_eq!(f, vec![0.0, 0.125, 0.25, 0.375, 0.5]);
        assert!(db[0].abs() < 1e-6);
        assert!(db[4] <= -190.0);
    }
}
