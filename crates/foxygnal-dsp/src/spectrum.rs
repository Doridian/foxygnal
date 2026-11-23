//! Windowed FFT power spectra.

use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::Window;
use crate::kernels::{Kernels, kernels};

/// Computes windowed power spectra of a fixed size.
///
/// Output is normalized so that a complex tone of amplitude `A` centred on a
/// bin reads `A²` in that bin (0 dBFS for a full-scale tone), whatever the
/// window. Bins are in FFT order: DC first, negative frequencies in the
/// upper half.
pub struct PowerSpectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    norm: f32,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    k: &'static Kernels,
}

impl PowerSpectrum {
    pub fn new(size: usize, window: Window) -> Self {
        assert!(size > 0);
        let fft = FftPlanner::new().plan_fft_forward(size);
        let window = window.coefficients(size);
        let gain: f32 = window.iter().sum();
        Self {
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            fft,
            window,
            norm: 1.0 / (gain * gain),
            buf: vec![Complex32::default(); size],
            k: kernels(),
        }
    }

    pub fn size(&self) -> usize {
        self.window.len()
    }

    /// Power spectrum of `input` (exactly [`Self::size`] samples) into `out`
    /// (same length).
    pub fn compute(&mut self, input: &[Complex32], out: &mut [f32]) {
        assert_eq!(input.len(), self.size());
        assert_eq!(out.len(), self.size());
        (self.k.mul_c32_f32)(input, &self.window, &mut self.buf);
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        (self.k.power)(&self.buf, self.norm, out);
    }
}

/// `10·log10(p)` of each power value (floored at −200 dB), vectorized.
pub fn power_to_db(p: &[f32], db: &mut [f32]) {
    (kernels().power_to_db)(p, db)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, bin: f32, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| Complex32::from_polar(amp, std::f32::consts::TAU * bin * i as f32 / n as f32))
            .collect()
    }

    #[test]
    fn full_scale_tone_reads_its_power_with_any_window() {
        for w in [Window::Rectangular, Window::Hann, Window::BlackmanHarris] {
            let mut ps = PowerSpectrum::new(256, w);
            let mut out = vec![0.0; 256];
            ps.compute(&tone(256, 16.0, 0.5), &mut out);
            assert!((out[16] - 0.25).abs() < 1e-3, "{w:?}: {}", out[16]);
            // Negative frequency lands in the upper half.
            ps.compute(&tone(256, -16.0, 1.0), &mut out);
            assert!((out[240] - 1.0).abs() < 1e-3, "{w:?}: {}", out[240]);
            assert!(out[16] < 1e-6);
        }
    }
}
