//! Pulse demodulation at any sample rate of 2 MS/s or more.
//!
//! A reply's pulses rarely line up with the samples: at 2 MS/s a 0.5 µs
//! pulse that starts half a sample late lands half in one sample and half in
//! the next, and a detector that compares single samples (dump1090's 2 MHz
//! one) misses it. Here the magnitude is treated as a step function (each
//! sample holding its value for one sample period) and integrated over the
//! exact intervals where pulses may be, at a fractional start time found by
//! trying a few phases and keeping the one that puts the most energy into the
//! preamble's pulses. The integrals come from a running sum, so each costs two
//! lookups.

/// The magnitude samples being searched, with their running sum.
pub struct Signal<'a> {
    pub m: &'a [f32],
    /// `pre[k]` = sum of `m[..k]`.
    pub pre: &'a [f64],
}

impl Signal<'_> {
    /// Integral up to `x` samples.
    #[inline]
    fn cum(&self, x: f64) -> f64 {
        let k = x.floor();
        let i = k as usize;
        self.pre[i] + (x - k) * f64::from(self.m[i])
    }

    /// Mean level over `[a, b)` (sample units, `0 <= a < b < len`).
    #[inline]
    pub fn level(&self, a: f64, b: f64) -> f64 {
        (self.cum(b) - self.cum(a)) / (b - a)
    }
}

/// Mode S timing in samples.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Samples per µs.
    pub s: f64,
}

impl Timing {
    /// Samples a 112-bit reply needs from its preamble's first sample, with
    /// room for the phase search.
    pub fn frame(&self) -> usize {
        (120.0 * self.s).ceil() as usize + 4
    }

    /// Mean level of the four preamble pulses and of the gaps between them,
    /// for a preamble starting at `t`.
    #[inline]
    fn preamble(&self, sig: &Signal<'_>, t: f64) -> (f64, f64) {
        let s = self.s;
        let h = s / 2.0;
        let pulses = sig.level(t, t + h)
            + sig.level(t + s, t + s + h)
            + sig.level(t + 3.5 * s, t + 4.0 * s)
            + sig.level(t + 4.5 * s, t + 5.0 * s);
        let quiet = sig.level(t + h, t + s) * 0.5
            + sig.level(t + 1.5 * s, t + 3.5 * s) * 2.0
            + sig.level(t + 4.0 * s, t + 4.5 * s) * 0.5
            + sig.level(t + 5.0 * s, t + 7.75 * s) * 2.75;
        (pulses / 4.0, quiet / 5.75)
    }

    /// If a preamble starts within half a sample of `n`, its start time,
    /// refined to an eighth of a sample, and the pulse amplitude above the
    /// quiet level. `noise` is the mean magnitude around. The caller makes
    /// sure `n >= 1` and the frame fits.
    pub fn find_preamble(&self, sig: &Signal<'_>, n: usize, noise: f64) -> Option<(f64, f64)> {
        let m = sig.m;
        if f64::from(m[n]) < 1.5 * noise {
            return None;
        }
        let base = n as f64;
        // Coarse look at two phases first: most candidates end here.
        let ok = |(p, q): (f64, f64)| p > 2.0 * q && p > 2.0 * noise;
        if !ok(self.preamble(sig, base - 0.25)) && !ok(self.preamble(sig, base + 0.25)) {
            return None;
        }
        let mut best = (f64::NEG_INFINITY, base);
        for k in -4..=4 {
            let t = base + f64::from(k) * 0.125;
            let (p, q) = self.preamble(sig, t);
            if ok((p, q)) && p - q > best.0 {
                best = (p - q, t);
            }
        }
        best.0.is_finite().then_some((best.1, best.0))
    }

    /// The `n_bits` bits after a preamble starting at `t`, MSB first into
    /// `out`. `amp` is the pulse amplitude (from [`Self::find_preamble`]).
    ///
    /// Each bit compares the levels of its two halves. When a half-bit
    /// boundary falls inside a sample, that sample mixes the two halves on
    /// either side: a pulse leaks `λ = f(1 − f)/h` of its level into the next
    /// window, `f` being the boundary's fractional position and `h` the
    /// half-bit length in samples. The pulse before the bit is known (the
    /// previous decision), the one after isn't; working through the four
    /// cases, the threshold for `E1 − E2` that splits "1" from "0" with the
    /// most margin is `λa·A·(prev − ½)`, with `prev` 1 if the previous bit
    /// ended in a pulse. At an aligned phase λ is 0 and this is the plain
    /// comparison.
    pub fn bits(&self, sig: &Signal<'_>, t: f64, amp: f64, out: &mut [u8], n_bits: usize) {
        let s = self.s;
        let h = s / 2.0;
        let lambda = |x: f64| {
            let f = x - x.floor();
            f * (1.0 - f) / h
        };
        // The preamble ends in silence.
        let mut prev = 0.0;
        for (k, o) in out.iter_mut().enumerate().take(n_bits.div_ceil(8)) {
            let mut v = 0u8;
            for i in 0..8 {
                let a = t + (8 + 8 * k + i) as f64 * s;
                let d = sig.level(a, a + h) - sig.level(a + h, a + s);
                let one = d > lambda(a) * amp * (prev - 0.5);
                prev = if one { 0.0 } else { 1.0 };
                v = v << 1 | u8::from(one);
            }
            *o = v;
        }
    }
}

/// Running sum of `m` into `pre` (one longer).
pub fn prefix(m: &[f32], pre: &mut Vec<f64>) {
    pre.clear();
    let mut acc = 0.0;
    pre.push(0.0);
    for &v in m {
        acc += f64::from(v);
        pre.push(acc);
    }
}
