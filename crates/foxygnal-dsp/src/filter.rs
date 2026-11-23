//! Running filters: a polyphase rational resampler (which is also a plain FIR
//! filter, a decimator and an interpolator), a frequency rotator and a
//! one-pole smoother. They allocate only when constructed.

use std::ops::{Add, Mul};

use num_complex::Complex32;

use crate::kernels::{Kernels, RotateFn, kernels};

/// A sample type FIR filters can run on with real taps.
pub trait FilterSample:
    Copy + Default + Send + Sync + 'static + Add<Output = Self> + Mul<f32, Output = Self>
{
    /// f32 values per sample.
    const LANES: usize;
    /// The samples as plain floats (interleaved re/im for complex).
    fn as_floats(s: &[Self]) -> &[f32];
    /// The dot-product kernel for taps stored [`Self::LANES`] times each.
    fn dot_kernel(k: &Kernels) -> fn(&[f32], &[f32]) -> Self;
}

impl FilterSample for f32 {
    const LANES: usize = 1;
    fn as_floats(s: &[Self]) -> &[f32] {
        s
    }
    fn dot_kernel(k: &Kernels) -> fn(&[f32], &[f32]) -> Self {
        k.dot_f32
    }
}

impl FilterSample for Complex32 {
    const LANES: usize = 2;
    fn as_floats(s: &[Self]) -> &[f32] {
        bytemuck::cast_slice(s)
    }
    fn dot_kernel(k: &Kernels) -> fn(&[f32], &[f32]) -> Self {
        k.dot_c32
    }
}

/// Polyphase rational resampler: interpolate by `L`, low-pass, decimate by
/// `M`, computing only the outputs that are kept. With `L = M = 1` it is a
/// plain FIR filter; with `L = 1` a decimator; with `M = 1` an interpolator.
///
/// The prototype taps run at `L` × the input rate; see
/// [`crate::fir::resampler_lowpass`].
pub struct Resampler<T> {
    interp: usize,
    decim: usize,
    /// Taps per phase of the prototype.
    taps_per_phase: usize,
    /// Taps per phase as stored: padded with zeros to a whole number of
    /// 8-float vectors, so the kernel never runs a scalar tail.
    per_phase: usize,
    /// `interp` phases × `per_phase` taps: phase `p` holds `h[p + k·L]`,
    /// each tap repeated `T::LANES` times (to line up with interleaved
    /// complex samples).
    phases: Vec<f32>,
    dot: fn(&[f32], &[f32]) -> T,
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
        let taps_per_phase = taps.len().div_ceil(interp);
        let per_phase = taps_per_phase.next_multiple_of(8 / T::LANES);
        let mut r = Self {
            interp,
            decim,
            taps_per_phase,
            per_phase,
            phases: vec![0.0; interp * per_phase * T::LANES],
            dot: T::dot_kernel(kernels()),
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

    /// Use this kernel set instead of [`kernels()`] (tests, benchmarks).
    pub fn set_kernels(&mut self, k: &Kernels) {
        self.dot = T::dot_kernel(k);
    }

    /// Replace the taps with a new set of the same length (allocation-free,
    /// keeps the history so a retune doesn't click).
    pub fn set_taps(&mut self, taps: &[f32]) {
        assert!(
            taps.len().div_ceil(self.interp) == self.taps_per_phase,
            "set_taps: tap count changed"
        );
        let (l, k, lanes) = (self.interp, self.per_phase, T::LANES);
        for p in 0..l {
            for j in 0..k {
                // Phase p, tap j is h[p + j·L]; past the prototype (and in
                // the padding) it is 0.
                let h = taps.get(p + j * l).copied().unwrap_or(0.0);
                let at = (p * k + j) * lanes;
                self.phases[at..at + lanes].fill(h);
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

    /// How many inputs the next `outputs` outputs need, so blocks can size
    /// their reads. Exact: [`Self::process`] given at most this many inputs
    /// and room for `outputs` outputs consumes all of them.
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
        let kl = k * T::LANES;
        // Work on locals: the compiler can't keep fields in registers
        // across the stores into `line`.
        let (mut need, mut phase, mut pos) = (self.need, self.phase, self.pos);
        let (interp, step, carry) = (
            self.interp,
            self.decim % self.interp,
            self.decim / self.interp,
        );
        let (line, phases, dot) = (&mut self.line[..], &self.phases[..], self.dot);
        loop {
            while need > 0 {
                let Some(&x) = input.get(i) else {
                    (self.need, self.phase, self.pos) = (need, phase, pos);
                    return (i, o);
                };
                // Newest sample first, stored twice so the window is contiguous.
                pos = if pos == 0 { k - 1 } else { pos - 1 };
                let x = pre(x);
                line[pos] = x;
                line[pos + k] = x;
                i += 1;
                need -= 1;
            }
            let Some(slot) = output.get_mut(o) else {
                (self.need, self.phase, self.pos) = (need, phase, pos);
                return (i, o);
            };
            let taps = &phases[phase * kl..(phase + 1) * kl];
            *slot = dot(taps, T::as_floats(&line[pos..pos + k]));
            o += 1;
            // phase += decim; need = phase / L; phase %= L, without dividing.
            phase += step;
            need = carry;
            if phase >= interp {
                phase -= interp;
                need += 1;
            }
        }
    }
}

/// Multiplies a complex stream by `e^{j2πfn}` (a numerically controlled
/// oscillator), shifting it by `f` cycles/sample.
///
/// [`Self::next`] does one sample; [`Self::rotate`] does a block, eight
/// samples at a time with SIMD. They share the phase and can be mixed.
pub struct Rotator {
    phasor: Complex32,
    step: Complex32,
    count: u32,
    /// `step⁰ … step⁷` and `step⁸`, computed in f64 so the eight lanes
    /// don't drift apart.
    steps: [Complex32; 8],
    step8: Complex32,
    rotate: RotateFn,
}

impl Rotator {
    pub fn new(freq: f64) -> Self {
        let one = Complex32::new(1.0, 0.0);
        let mut r = Self {
            phasor: one,
            step: one,
            count: 0,
            steps: [one; 8],
            step8: one,
            rotate: kernels().rotate,
        };
        r.set_freq(freq);
        r
    }

    /// Use this kernel set instead of [`kernels()`] (tests, benchmarks).
    pub fn set_kernels(&mut self, k: &Kernels) {
        self.rotate = k.rotate;
    }

    /// Change the shift, keeping the phase continuous.
    pub fn set_freq(&mut self, freq: f64) {
        let turn = |n: f64| {
            let (s, c) = (std::f64::consts::TAU * freq * n).sin_cos();
            Complex32::new(c as f32, s as f32)
        };
        self.step = turn(1.0);
        self.steps = std::array::from_fn(|j| turn(j as f64));
        self.step8 = turn(8.0);
    }

    /// `y[n] = x[n]·e^{j2πf(n₀+n)}` for as many samples as both slices
    /// hold, continuing the phase.
    pub fn rotate(&mut self, x: &[Complex32], y: &mut [Complex32]) {
        let n = x.len().min(y.len());
        let (x, y) = (&x[..n], &mut y[..n]);
        self.phasor = (self.rotate)(self.phasor, &self.steps, self.step8, x, y);
        let done = n / 8 * 8;
        for (o, &v) in y[done..].iter_mut().zip(&x[done..]) {
            *o = self.next(v);
        }
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
        for k in Kernels::all() {
            for ntaps in [1, 7, 23, 64, 129] {
                polyphase_case(&k, ntaps);
            }
        }
    }

    fn polyphase_case(k: &Kernels, ntaps: usize) {
        let taps: Vec<f32> = (0..ntaps).map(|i| (i as f32 * 0.37).sin()).collect();
        let x: Vec<f32> = (0..200).map(|i| (i as f32 * 0.11).cos()).collect();
        for (l, m) in [(1, 1), (1, 4), (3, 1), (3, 7), (5, 2)] {
            let want = reference(l, m, &taps, &x);
            let mut r = Resampler::<f32>::new(l, m, &taps);
            r.set_kernels(k);
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
                assert!(
                    (a - b).abs() < 1e-4,
                    "{:?} {ntaps} {l}/{m}: {a} vs {b}",
                    k.level
                );
            }

            // Complex input: real and imaginary parts filter independently.
            let xc: Vec<Complex32> = x
                .iter()
                .enumerate()
                .map(|(i, &v)| Complex32::new(v, (i as f32 * 0.29).sin()))
                .collect();
            let im: Vec<f32> = xc.iter().map(|v| v.im).collect();
            let want_im = reference(l, m, &taps, &im);
            let mut r = Resampler::<Complex32>::new(l, m, &taps);
            r.set_kernels(k);
            let mut out = vec![Complex32::default(); 7];
            let (mut i, mut n) = (0, 0);
            loop {
                let end = (i + 11).min(xc.len());
                let (used, made) = r.process(&xc[i..end], &mut out);
                for o in &out[..made] {
                    assert!((o.re - want[n]).abs() < 1e-4, "{:?} c32 re {n}", k.level);
                    assert!((o.im - want_im[n]).abs() < 1e-4, "{:?} c32 im {n}", k.level);
                    n += 1;
                }
                i += used;
                if used == 0 && made == 0 {
                    break;
                }
            }
            assert_eq!(n, want.len());
        }
    }

    #[test]
    fn set_taps_keeps_the_padding_zero() {
        let mut r = Resampler::<Complex32>::new(2, 3, &[1.0; 9]);
        r.set_taps(&[0.5; 9]);
        // 5 taps per phase, padded to 8 complex samples (16 floats).
        assert_eq!(r.per_phase, 8);
        let p0 = &r.phases[..r.per_phase * 2];
        assert!(p0[..10].iter().all(|&h| h == 0.5));
        assert!(p0[10..].iter().all(|&h| h == 0.0));
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
    fn rotate_blocks_match_next_and_mix_with_it() {
        for k in Kernels::all() {
            let x: Vec<Complex32> = (0..1000)
                .map(|i| Complex32::new((i as f32 * 0.1).sin(), (i as f32 * 0.07).cos()))
                .collect();
            let mut a = Rotator::new(-0.0371);
            let mut b = Rotator::new(-0.0371);
            b.set_kernels(&k);
            let want: Vec<Complex32> = x.iter().map(|&v| a.next(v)).collect();
            let mut got = vec![Complex32::default(); x.len()];
            // Odd block sizes, with single samples in between.
            let mut i = 0;
            for (n, len) in [13usize, 1, 64, 7, 200, 3].iter().cycle().enumerate() {
                if i >= x.len() {
                    break;
                }
                let end = (i + len).min(x.len());
                if n % 2 == 1 {
                    for j in i..end {
                        got[j] = b.next(x[j]);
                    }
                } else {
                    b.rotate(&x[i..end], &mut got[i..end]);
                }
                i = end;
            }
            for (j, (g, w)) in got.iter().zip(&want).enumerate() {
                assert!((g - w).norm() < 1e-4, "{:?} {j}: {g} vs {w}", k.level);
            }
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
