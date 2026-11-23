//! An arbitrary-ratio resampler whose ratio and phase can be steered: the
//! DVB-T receiver's input stage. It takes the input to the elementary rate
//! and, driven by the pilot-based timing loop, removes the sampling clock
//! offset in the time domain (at 50 ppm, the 8k mode's outer carriers would
//! otherwise be 0.17 carrier spacings off: far too much inter-carrier
//! interference for 64-QAM).
//!
//! A Kaiser-windowed sinc, 256 phases (nearest phase, no interpolation
//! between them: the timing error is under 1/512 sample), cut off at half
//! the output rate.

use foxygnal_core::Complex32;
use foxygnal_dsp::kernels;

const PHASES: usize = 256;
const KAISER_BETA: f64 = 5.0;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

pub struct ArbResampler {
    taps: usize,
    /// (PHASES + 1) rows of `taps` coefficients, each twice (for `dot_c32`).
    bank: Vec<f32>,
    buf: Vec<Complex32>,
    /// Input position (in `buf`) of the next output sample.
    pos: f64,
    /// Nominal input samples per output sample.
    nominal: f64,
    /// Current step: nominal × (1 + clock offset).
    step: f64,
    dot: fn(&[f32], &[f32]) -> Complex32,
}

impl ArbResampler {
    /// Input chunks pushed must not exceed `max_push` samples.
    pub fn new(in_rate: f64, out_rate: f64, max_push: usize) -> Self {
        let nominal = in_rate / out_rate;
        // ~18 taps per output sample: the band edge at 0.417 of the output
        // rate leaves a transition of 0.167 before images fold onto it.
        let taps = 2 * (9.0 * nominal.max(1.0)).ceil() as usize;
        let fc = 0.5 / nominal.max(1.0);
        let half = taps as f64 / 2.0;
        let i0b = bessel_i0(KAISER_BETA);
        let mut bank = Vec::with_capacity((PHASES + 1) * taps * 2);
        for ph in 0..=PHASES {
            let mu = ph as f64 / PHASES as f64;
            let row: Vec<f64> = (0..taps)
                .map(|t| {
                    let x = t as f64 - (half - 1.0) - mu;
                    let s = if x.abs() < 1e-12 {
                        1.0
                    } else {
                        let a = std::f64::consts::PI * 2.0 * fc * x;
                        a.sin() / a
                    };
                    let r = (x / half).clamp(-1.0, 1.0);
                    s * bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0b
                })
                .collect();
            let sum: f64 = row.iter().sum();
            for v in row {
                let v = (v / sum) as f32;
                bank.push(v);
                bank.push(v);
            }
        }
        Self {
            taps,
            bank,
            buf: Vec::with_capacity(max_push + 4 * taps + 64),
            pos: half - 1.0,
            nominal,
            step: nominal,
            dot: kernels().dot_c32,
        }
    }

    /// Set the clock offset (positive: the input clock runs fast).
    pub fn set_offset(&mut self, offset: f64) {
        self.step = self.nominal * (1.0 + offset);
    }

    /// Move by `n` output samples without computing them (negative: back,
    /// as far as the history allows).
    pub fn skip(&mut self, n: f64) {
        self.pos = (self.pos + n * self.step).max(self.taps as f64 / 2.0 - 1.0);
    }

    /// Append input. Call [`Self::compact`] between pushes.
    pub fn push(&mut self, x: &[Complex32]) {
        self.buf.extend_from_slice(x);
    }

    /// Drop the input no longer needed.
    pub fn compact(&mut self) {
        let first = self.pos.floor() as isize + 1 - (self.taps / 2) as isize;
        // Keep some history so a small step back stays possible.
        let drop = (first - 8).max(0) as usize;
        let drop = drop.min(self.buf.len());
        if drop > 0 {
            self.buf.drain(..drop);
            self.pos -= drop as f64;
        }
    }

    /// Compute up to `out.len()` output samples; returns how many.
    pub fn process(&mut self, out: &mut [Complex32]) -> usize {
        let half = self.taps / 2;
        let row = 2 * self.taps;
        let x: &[f32] = bytemuck::cast_slice(&self.buf);
        let mut made = 0;
        for o in out.iter_mut() {
            // `pos` is never negative: truncation is floor (and no libm call).
            let iu = self.pos as usize;
            let i = iu as f64;
            if iu + half >= self.buf.len() {
                break;
            }
            let ph = ((self.pos - i) * PHASES as f64 + 0.5) as usize;
            let base = iu + 1 - half;
            *o = (self.dot)(
                &self.bank[ph * row..(ph + 1) * row],
                &x[2 * base..2 * (base + self.taps)],
            );
            self.pos += self.step;
            made += 1;
        }
        made
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_ratio_passes_a_tone_through() {
        let n = 4000;
        let f = 0.4;
        let x: Vec<Complex32> = (0..n)
            .map(|i| Complex32::from_polar(1.0, (std::f64::consts::TAU * f * i as f64) as f32))
            .collect();
        let mut r = ArbResampler::new(1.0, 1.0, n);
        r.push(&x);
        // A quarter-sample delay: the hardest phase for the filter.
        r.skip(0.25);
        let mut y = vec![Complex32::default(); n];
        let m = r.process(&mut y);
        assert!(m > n - 40);
        let half = r.taps / 2 - 1;
        let mut err = 0.0f64;
        for (j, v) in y[..m].iter().enumerate().skip(10) {
            let t = (j + half) as f64 + 0.25;
            let want = Complex32::from_polar(1.0, (std::f64::consts::TAU * f * t) as f32);
            err = err.max(f64::from((v - want).norm()));
        }
        // 0.4 of the rate is near the top of a DVB-T channel: still ~-38 dB.
        assert!(err < 0.015, "{err}");
    }
}
