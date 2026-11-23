//! Running filters: a polyphase rational resampler (which is also a plain FIR
//! filter, a decimator and an interpolator), a frequency rotator and a
//! one-pole smoother. They allocate only when constructed.

use std::ops::{Add, Mul};

use num_complex::Complex32;

/// A sample type FIR filters can run on with real taps.
pub trait FilterSample:
    Copy + Default + Send + Sync + 'static + Add<Output = Self> + Mul<f32, Output = Self>
{
}
impl FilterSample for f32 {}
impl FilterSample for Complex32 {}

/// Polyphase rational resampler: interpolate by `L`, low-pass, decimate by
/// `M`, computing only the outputs that are kept. With `L = M = 1` it is a
/// plain FIR filter; with `L = 1` a decimator; with `M = 1` an interpolator.
///
/// The prototype taps run at `L` × the input rate; see
/// [`crate::fir::resampler_lowpass`].
pub struct Resampler<T> {
    interp: usize,
    decim: usize,
    /// Taps per phase.
    per_phase: usize,
    /// `interp` phases × `per_phase` taps: phase `p` holds `h[p + k·L]`.
    phases: Vec<f32>,
    /// Each sample is stored twice, `per_phase` apart, so the latest
    /// `per_phase` samples are always one contiguous slice.
    line: Vec<T>,
    pos: usize,
    phase: usize,
    /// Inputs to take before the next output.
    need: usize,
}

impl<T: FilterSample> Resampler<T> {
    pub fn new(interp: usize, decim: usize, taps: &[f32]) -> Self {
        assert!(interp >= 1 && decim >= 1 && !taps.is_empty());
        let per_phase = taps.len().div_ceil(interp);
        let mut r = Self {
            interp,
            decim,
            per_phase,
            phases: vec![0.0; interp * per_phase],
            line: vec![T::default(); 2 * per_phase],
            pos: 0,
            phase: 0,
            need: 1,
        };
        r.set_taps(taps);
        r
    }

    /// A plain FIR filter (`L = M = 1`).
    pub fn fir(taps: &[f32]) -> Self {
        Self::new(1, 1, taps)
    }

    pub fn ratio(&self) -> (usize, usize) {
        (self.interp, self.decim)
    }

    /// Replace the taps with a new set of the same length (allocation-free,
    /// keeps the history so a retune doesn't click).
    pub fn set_taps(&mut self, taps: &[f32]) {
        assert!(
            taps.len().div_ceil(self.interp) == self.per_phase,
            "set_taps: tap count changed"
        );
        let (l, k) = (self.interp, self.per_phase);
        for p in 0..l {
            for j in 0..k {
                self.phases[p * k + j] = taps.get(p + j * l).copied().unwrap_or(0.0);
            }
        }
    }

    /// Forget the history.
    pub fn reset(&mut self) {
        self.line.fill(T::default());
        self.pos = 0;
        self.phase = 0;
        self.need = 1;
    }

    fn push(&mut self, x: T) {
        let k = self.per_phase;
        self.pos = if self.pos == 0 { k - 1 } else { self.pos - 1 };
        self.line[self.pos] = x;
        self.line[self.pos + k] = x;
    }

    /// How many inputs the next `outputs` outputs need (upper bound), so
    /// blocks can size their reads.
    pub fn inputs_for(&self, outputs: usize) -> usize {
        if outputs == 0 {
            return 0;
        }
        self.need + ((self.phase + (outputs - 1) * self.decim) / self.interp)
    }

    /// Filter as much as fits; returns (inputs consumed, outputs written).
    pub fn process(&mut self, input: &[T], output: &mut [T]) -> (usize, usize) {
        self.process_with(input, output, |x| x)
    }

    /// Like [`Self::process`], passing each input through `pre` as it is
    /// taken (e.g. a mixer in front of the filter). `pre` runs exactly once
    /// per consumed input.
    #[inline]
    pub fn process_with<S: Copy>(
        &mut self,
        input: &[S],
        output: &mut [T],
        mut pre: impl FnMut(S) -> T,
    ) -> (usize, usize) {
        let (mut i, mut o) = (0, 0);
        let k = self.per_phase;
        loop {
            while self.need > 0 {
                let Some(&x) = input.get(i) else {
                    return (i, o);
                };
                self.push(pre(x));
                i += 1;
                self.need -= 1;
            }
            let Some(slot) = output.get_mut(o) else {
                return (i, o);
            };
            let taps = &self.phases[self.phase * k..(self.phase + 1) * k];
            let window = &self.line[self.pos..self.pos + k];
            let mut acc = T::default();
            for (&h, &x) in taps.iter().zip(window) {
                acc = acc + x * h;
            }
            *slot = acc;
            o += 1;
            self.phase += self.decim;
            self.need = self.phase / self.interp;
            self.phase %= self.interp;
        }
    }
}

/// Multiplies a complex stream by `e^{j2πfn}` (a numerically controlled
/// oscillator), shifting it by `f` cycles/sample.
pub struct Rotator {
    phasor: Complex32,
    step: Complex32,
    count: u32,
}

impl Rotator {
    pub fn new(freq: f64) -> Self {
        let mut r = Self {
            phasor: Complex32::new(1.0, 0.0),
            step: Complex32::new(1.0, 0.0),
            count: 0,
        };
        r.set_freq(freq);
        r
    }

    /// Change the shift, keeping the phase continuous.
    pub fn set_freq(&mut self, freq: f64) {
        let (s, c) = (std::f64::consts::TAU * freq).sin_cos();
        self.step = Complex32::new(c as f32, s as f32);
    }

    #[inline]
    pub fn next(&mut self, x: Complex32) -> Complex32 {
        let y = x * self.phasor;
        self.phasor *= self.step;
        self.count += 1;
        if self.count == 512 {
            // Keep |phasor| = 1 despite rounding.
            self.count = 0;
            self.phasor /= self.phasor.norm();
        }
        y
    }
}

/// One-pole low-pass `y += α(x − y)`: smoothing, averages, de-emphasis.
#[derive(Debug, Clone, Copy)]
pub struct OnePole {
    pub alpha: f32,
    pub y: f32,
}

impl OnePole {
    pub fn new(alpha: f32) -> Self {
        Self { alpha, y: 0.0 }
    }

    /// α for a time constant `tau` seconds at `rate` samples per second.
    pub fn alpha_for(tau: f64, rate: f64) -> f32 {
        if tau <= 0.0 {
            1.0
        } else {
            (1.0 - (-1.0 / (tau * rate)).exp()) as f32
        }
    }

    #[inline]
    pub fn next(&mut self, x: f32) -> f32 {
        self.y += self.alpha * (x - self.y);
        self.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: upsample with zeros, convolve, keep every M-th.
    fn reference(l: usize, m: usize, taps: &[f32], x: &[f32]) -> Vec<f32> {
        let up: Vec<f32> = x
            .iter()
            .flat_map(|&v| std::iter::once(v).chain(std::iter::repeat_n(0.0, l - 1)))
            .collect();
        (0..up.len())
            .step_by(m)
            .map(|n| {
                taps.iter()
                    .enumerate()
                    .filter(|(k, _)| *k <= n)
                    .map(|(k, &h)| h * up[n - k])
                    .sum()
            })
            .collect()
    }

    #[test]
    fn polyphase_matches_the_direct_form_in_any_chunking() {
        let taps: Vec<f32> = (0..23).map(|i| (i as f32 * 0.37).sin()).collect();
        let x: Vec<f32> = (0..200).map(|i| (i as f32 * 0.11).cos()).collect();
        for (l, m) in [(1, 1), (1, 4), (3, 1), (3, 7), (5, 2)] {
            let want = reference(l, m, &taps, &x);
            let mut r = Resampler::<f32>::new(l, m, &taps);
            let mut got = Vec::new();
            let mut out = [0.0; 3];
            let mut i = 0;
            // Feed in odd-sized pieces with a small output buffer.
            loop {
                let end = (i + 5).min(x.len());
                let (used, made) = r.process(&x[i..end], &mut out);
                got.extend_from_slice(&out[..made]);
                i += used;
                if used == 0 && made == 0 {
                    break;
                }
            }
            assert_eq!(got.len(), want.len(), "{l}/{m}");
            for (a, b) in got.iter().zip(&want) {
                assert!((a - b).abs() < 1e-4, "{l}/{m}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn inputs_for_is_enough() {
        let taps = [1.0f32; 9];
        let mut r = Resampler::<f32>::new(3, 7, &taps);
        let x = [1.0f32; 100];
        let mut out = [0.0; 5];
        for _ in 0..4 {
            let need = r.inputs_for(5);
            let (used, made) = r.process(&x[..need], &mut out);
            assert_eq!((used, made), (need, 5));
        }
    }

    #[test]
    fn rotator_shifts_and_stays_unit() {
        let mut r = Rotator::new(0.25);
        let mut y = Complex32::default();
        for _ in 0..10_001 {
            y = r.next(Complex32::new(1.0, 0.0));
        }
        // After 10000 steps of a quarter turn: back to 1.
        assert!((y - Complex32::new(1.0, 0.0)).norm() < 1e-3, "{y}");
    }
}
