//! Spectrum (and waterfall: the UI draws each frame as one row).

use foxygnal_core::Complex32;
use foxygnal_dsp::spectrum::power_to_db;
use foxygnal_dsp::{PowerSpectrum, Window};
use serde::Deserialize;

use super::Instrument;
use super::source::{Frame, Source};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SpectrumOptions {
    /// FFT length; rounded up to a power of two.
    pub fft_size: u32,
    pub window: Window,
    /// Exponential averaging over about this many FFTs (1 = none).
    pub average: u32,
    pub peak_hold: bool,
    /// Output bins (the canvas width). Groups of FFT bins keep their maximum,
    /// so narrow peaks survive.
    pub bins: u32,
    /// Changing it clears the average and the peak hold.
    pub reset: u32,
}

impl Default for SpectrumOptions {
    fn default() -> Self {
        Self {
            fft_size: 2048,
            window: Window::Hann,
            average: 1,
            peak_hold: false,
            bins: 1024,
            reset: 0,
        }
    }
}

/// At most this many FFTs per frame; older data is skipped.
const MAX_FFTS: usize = 16;

pub struct Spectrum {
    ps: PowerSpectrum,
    complex: bool,
    alpha: f32,
    bins: usize,
    pending: Vec<Complex32>,
    fresh: Vec<Complex32>,
    power: Vec<f32>,
    avg: Option<Vec<f32>>,
    peak: Option<Vec<f32>>,
    /// Reduced bins, linear then dB.
    reduced: Vec<f32>,
    db: Vec<f32>,
}

impl Spectrum {
    pub fn new(o: &SpectrumOptions, complex: bool) -> Self {
        let n = (o.fft_size.clamp(64, 1 << 16) as usize).next_power_of_two();
        Self {
            ps: PowerSpectrum::new(n, o.window),
            complex,
            alpha: 1.0 / o.average.clamp(1, 10_000) as f32,
            bins: (o.bins as usize).clamp(16, 8192),
            pending: Vec::new(),
            fresh: Vec::new(),
            power: vec![0.0; n],
            avg: None,
            peak: o.peak_hold.then(|| vec![0.0; n]),
            reduced: Vec::new(),
            db: Vec::new(),
        }
    }

    /// Write `p` in display order and in dB, reduced to `bins`: −fs/2…+fs/2
    /// for complex input, 0…fs/2 for real input (one-sided, so a real tone
    /// reads its amplitude).
    fn write_db(
        complex: bool,
        p: &[f32],
        f: &mut Frame<'_>,
        bins: usize,
        count: usize,
        (reduced, db): (&mut Vec<f32>, &mut Vec<f32>),
    ) {
        let n = p.len();
        let (first, gain) = if complex { (n / 2, 1.0) } else { (0, 4.0) };
        let at = |i: usize| {
            let k = (first + i) % n;
            if k == 0 { p[k] } else { p[k] * gain }
        };
        reduced.clear();
        reduced.extend((0..bins).map(|b| {
            let lo = b * count / bins;
            let hi = ((b + 1) * count / bins).max(lo + 1);
            (lo..hi).map(at).fold(0.0f32, f32::max)
        }));
        db.resize(bins, 0.0);
        power_to_db(reduced, db);
        f.f32s(db.iter().copied());
    }
}

impl Instrument for Spectrum {
    fn poll(&mut self, src: &mut Source) {
        let n = self.ps.size();
        let cap = n * (MAX_FFTS + 1);
        let mut fresh = std::mem::take(&mut self.fresh);
        fresh.clear();
        if !src.fresh(cap, &mut fresh) {
            // An FFT across a gap is garbage: keep only what follows it.
            self.pending.clear();
        }
        self.pending.extend_from_slice(&fresh);
        self.fresh = fresh;
        if self.pending.len() > cap {
            let drop = self.pending.len() - cap;
            self.pending.drain(..drop);
        }
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        let n = self.ps.size();
        if self.pending.len() < n {
            return false;
        }
        // 50% overlap: smoother at low rates, and nothing is wasted.
        let hop = n / 2;
        let ffts = ((self.pending.len() - n) / hop + 1).min(MAX_FFTS);
        let first = self.pending.len() - n - (ffts - 1) * hop;
        for i in 0..ffts {
            let at = first + i * hop;
            self.ps.compute(&self.pending[at..at + n], &mut self.power);
            match &mut self.avg {
                None => self.avg = Some(self.power.clone()),
                Some(avg) => {
                    for (a, p) in avg.iter_mut().zip(&self.power) {
                        *a += (p - *a) * self.alpha;
                    }
                }
            }
        }
        // Keep the tail the next FFT will overlap with.
        self.pending.drain(..first + ffts * hop);
        let avg = self.avg.as_ref().expect("computed above");
        if let Some(peak) = &mut self.peak {
            for (m, a) in peak.iter_mut().zip(avg) {
                *m = m.max(*a);
            }
        }
        let count = if self.complex { n } else { n / 2 + 1 };
        let bins = self.bins.min(count);
        let mut f = Frame(out);
        f.u32(n as u32).u32(bins as u32).bytes4([
            u8::from(self.complex) | u8::from(self.peak.is_some()) << 1,
            0,
            0,
            0,
        ]);
        let scratch = (&mut self.reduced, &mut self.db);
        Self::write_db(self.complex, avg, &mut f, bins, count, scratch);
        if let Some(peak) = &self.peak {
            let scratch = (&mut self.reduced, &mut self.db);
            Self::write_db(self.complex, peak, &mut f, bins, count, scratch);
        }
        true
    }
}
