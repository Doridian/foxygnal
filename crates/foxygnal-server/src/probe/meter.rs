//! Level meter: RMS, peak and mean over everything since the last frame.

use foxygnal_core::Complex32;

use super::Instrument;
use super::source::{Frame, Source};

#[derive(Default)]
pub struct Meter {
    complex: bool,
    count: u64,
    sum_sq: f64,
    peak: f32,
    sum: Complex32,
    buf: Vec<Complex32>,
}

impl Meter {
    pub fn new(complex: bool) -> Self {
        Self {
            complex,
            ..Self::default()
        }
    }
}

impl Instrument for Meter {
    fn poll(&mut self, src: &mut Source) {
        self.buf.clear();
        src.fresh(usize::MAX, &mut self.buf);
        for x in &self.buf {
            let p = x.norm_sqr();
            self.sum_sq += f64::from(p);
            self.peak = self.peak.max(p);
            self.sum += x;
        }
        self.count += self.buf.len() as u64;
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.count == 0 {
            return false;
        }
        let n = self.count as f64;
        let mean = self.sum / n as f32;
        Frame(out)
            .u32(self.count.min(u64::from(u32::MAX)) as u32)
            .bytes4([u8::from(self.complex), 0, 0, 0])
            .f32((self.sum_sq / n).sqrt() as f32)
            .f32(self.peak.sqrt())
            .f32(mean.re)
            .f32(mean.im);
        self.count = 0;
        self.sum_sq = 0.0;
        self.peak = 0.0;
        self.sum = Complex32::default();
        true
    }
}
