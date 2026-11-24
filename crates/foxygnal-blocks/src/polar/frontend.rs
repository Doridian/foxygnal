//! A PSK receiver front end for the polar orbiters: complex baseband in, one
//! complex symbol per symbol out (carrier and timing recovered, unit RMS),
//! for BPSK, QPSK and offset QPSK. The demodulators of Meteor-M LRPT, Meteor-M
//! HRPT and Metop AHRPT own one each and hand its symbols to their deframer.
//!
//! Per chunk of input samples:
//!
//! 1. **Frequency**: an NCO removes the expected offset (the `offset`
//!    setting, which a Doppler source can drive) plus the residual found by
//!    the coarse search.
//! 2. **Matched filter**: an RRC at the input rate, decimating to 4–8
//!    samples a symbol when the input has more.
//! 3. **Timing**: Gardner detector, cubic interpolator, second-order loop;
//!    two samples a symbol (on time and halfway).
//! 4. **AGC**, then a second-order decision-directed **Costas loop**.
//! 5. **Coarse frequency search** while the deframer isn't locked: the
//!    samples at twice the symbol rate, squared (BPSK) or raised to the 4th
//!    power (QPSK, OQPSK), which strips the modulation and leaves a line at
//!    2× or 4× the residual offset; 4 FFTs of 4096 points are averaged and a
//!    clear line moves the NCO. The range is ± symbol rate / 2 (BPSK) or
//!    ± symbol rate / 4 (QPSK): 18 kHz for LRPT, 580 kHz for AHRPT.
//!
//! **Offset QPSK** (Q half a symbol after I): the timing detector and the
//! Costas loop take I from the on-time sample and Q from the halfway sample,
//! after derotation. The symbol out is (I on time, Q from the halfway sample
//! before it). Whether that Q belongs to this I or to the previous one
//! depends on where the loops locked (each of I and Q can be the branch the
//! timing locked to, and the carrier has four phases), so the caller tries
//! both pairings; see [`OqpskPairs`].

use std::f32::consts::{FRAC_1_SQRT_2, PI, TAU};
use std::sync::Arc;

use foxygnal_core::Complex32;
use foxygnal_dsp::{Resampler, Rotator, taps};
use rustfft::{Fft, FftPlanner};

use crate::digital::loop_gains;

/// The modulation a [`FrontEnd`] demodulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Psk {
    Bpsk,
    Qpsk,
    /// Offset QPSK: Q delayed by half a symbol.
    Oqpsk,
}

impl Psk {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bpsk => "bpsk",
            Self::Qpsk => "qpsk",
            Self::Oqpsk => "oqpsk",
        }
    }
}

/// Input samples processed per call.
pub const CHUNK: usize = 2048;
/// FFT length of the coarse search (samples at twice the symbol rate).
const COARSE_N: usize = 4096;
/// FFTs averaged per coarse estimate.
const COARSE_AVG: usize = 4;
/// The line must stand this far above the average bin (power ratio, after
/// averaging).
const COARSE_SNR: f32 = 10.0;
/// Residual offsets smaller than this (rad/symbol) are left to the Costas loop.
const COARSE_MIN: f64 = 0.002;
/// Timing and carrier loop bandwidths (per symbol) while searching, and
/// once locked.
const TIMING_BW: (f64, f64) = (0.008, 0.002);
const CARRIER_BW: (f64, f64) = (0.012, 0.004);
/// Symbols to wait after a frequency jump before trusting the symbols.
const SETTLE: u64 = 1500;

/// Cubic Lagrange interpolation between `p[1]` (mu = 0) and `p[2]` (mu = 1).
#[inline]
fn interpolate(p: &[Complex32; 4], mu: f32) -> Complex32 {
    let (a, b, c) = (mu + 1.0, mu - 1.0, mu - 2.0);
    let c0 = -mu * b * c / 6.0;
    let c1 = a * b * c / 2.0;
    let c2 = -a * mu * c / 2.0;
    let c3 = a * mu * b / 6.0;
    p[0] * c0 + p[1] * c1 + p[2] * c2 + p[3] * c3
}

#[inline]
fn dot(a: Complex32, b: Complex32) -> f32 {
    a.re * b.re + a.im * b.im
}

#[inline]
fn sign(v: f32) -> f32 {
    if v >= 0.0 { 1.0 } else { -1.0 }
}

/// Wrap a phase into −π…π.
#[inline]
fn wrap(p: f32) -> f32 {
    if p > PI {
        p - TAU
    } else if p < -PI {
        p + TAU
    } else {
        p
    }
}

pub struct FrontEnd {
    psk: Psk,
    /// Input sample rate.
    rate: f64,
    /// Samples per symbol after the matched filter.
    sps: f64,
    symbol_rate: f64,
    /// The expected offset (Hz) and the residual found by the coarse
    /// search (cycles per input sample).
    base_hz: f64,
    resid: f64,
    nco: Rotator,
    filter: Resampler<Complex32>,
    mixed: Vec<Complex32>,
    filtered: Vec<Complex32>,
    // Timing recovery.
    hist: [Complex32; 4],
    next: f64,
    mid_next: bool,
    integ: f64,
    kp: f64,
    ki: f64,
    /// Previous on-time sample, current and previous halfway samples (raw
    /// for BPSK/QPSK, derotated and scaled for OQPSK).
    prev: Complex32,
    mid_raw: Complex32,
    prev_mid: Complex32,
    ted_power: f32,
    // AGC.
    power: f32,
    // Costas loop (rad/symbol).
    phase: f32,
    freq: f32,
    alpha: f32,
    beta: f32,
    // Coarse frequency search.
    coarse_on: bool,
    fft: Arc<dyn Fft<f32>>,
    fft_buf: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    window: Vec<f32>,
    acc: Vec<f32>,
    acc_n: usize,
    fft_fill: usize,
    settle_at: u64,
    /// Coarse frequency jumps so far.
    pub jumps: u64,
    /// Symbols out so far.
    pub symbols: u64,
    // Statistics since the last `take_mer`.
    abs_sum: f64,
    pow_sum: f64,
    stat_n: u64,
}

impl FrontEnd {
    /// `rate`: input samples a second, at least 2 a symbol. `rolloff`: of
    /// the RRC matched filter.
    pub fn new(rate: f64, symbol_rate: f64, psk: Psk, rolloff: f64) -> Self {
        let in_sps = rate / symbol_rate;
        let decim = ((in_sps / 4.0).floor() as usize).max(1);
        let sps = in_sps / decim as f64;
        let h = taps::rrc(in_sps, rolloff, 10, 1.0);
        let (kp, ki) = loop_gains(TIMING_BW.0);
        let (alpha, beta) = loop_gains(CARRIER_BW.0);
        let fft = FftPlanner::new().plan_fft_forward(COARSE_N);
        let window = (0..COARSE_N)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / COARSE_N as f32).cos())
            .collect();
        Self {
            psk,
            rate,
            sps,
            symbol_rate,
            base_hz: 0.0,
            resid: 0.0,
            nco: Rotator::new(0.0),
            filter: Resampler::new(1, decim, &h),
            mixed: vec![Complex32::default(); CHUNK],
            filtered: vec![Complex32::default(); CHUNK],
            hist: [Complex32::default(); 4],
            next: 0.0,
            mid_next: false,
            integ: 0.0,
            kp,
            ki,
            prev: Complex32::default(),
            mid_raw: Complex32::default(),
            prev_mid: Complex32::default(),
            ted_power: 0.0,
            power: 0.0,
            phase: 0.0,
            freq: 0.0,
            alpha: alpha as f32,
            beta: beta as f32,
            coarse_on: true,
            fft_scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            fft,
            fft_buf: vec![Complex32::default(); COARSE_N],
            window,
            acc: vec![0.0; COARSE_N],
            acc_n: 0,
            fft_fill: 0,
            settle_at: (COARSE_AVG * COARSE_N / 2) as u64 + SETTLE,
            jumps: 0,
            symbols: 0,
            abs_sum: 0.0,
            pow_sum: 0.0,
            stat_n: 0,
        }
    }

    pub fn psk(&self) -> Psk {
        self.psk
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn symbol_rate(&self) -> f64 {
        self.symbol_rate
    }

    /// The expected carrier offset in Hz (tuning error, Doppler): the NCO
    /// moves it to 0 Hz. The residual the coarse search found is kept.
    pub fn set_offset(&mut self, hz: f64) {
        if hz.is_finite() {
            self.base_hz = hz;
            self.nco.set_freq(-(hz / self.rate + self.resid));
        }
    }

    /// Narrow the loops and stop the coarse search once the deframer is
    /// locked (less jitter, fewer cycle slips at low SNR); widen them again
    /// to search.
    pub fn set_locked(&mut self, locked: bool) {
        if locked == !self.coarse_on {
            return;
        }
        self.coarse_on = !locked;
        self.acc.fill(0.0);
        self.acc_n = 0;
        self.fft_fill = 0;
        let i = usize::from(locked);
        let (kp, ki) = loop_gains([TIMING_BW.0, TIMING_BW.1][i]);
        let (alpha, beta) = loop_gains([CARRIER_BW.0, CARRIER_BW.1][i]);
        (self.kp, self.ki) = (kp, ki);
        (self.alpha, self.beta) = (alpha as f32, beta as f32);
    }

    /// True once the symbols since the last frequency jump can be trusted.
    pub fn settled(&self) -> bool {
        self.symbols >= self.settle_at
    }

    /// The carrier offset estimate in Hz (the expected offset included).
    pub fn offset_hz(&self) -> f64 {
        self.base_hz
            + self.resid * self.rate
            + f64::from(self.freq) * self.symbol_rate / std::f64::consts::TAU
    }

    /// The symbol clock's deviation from nominal, in ppm.
    pub fn clock_ppm(&self) -> f64 {
        -self.integ * 1e6
    }

    /// MER in dB over the symbols since the last call (and reset).
    pub fn take_mer(&mut self) -> Option<f64> {
        if self.stat_n == 0 {
            return None;
        }
        let n = self.stat_n as f64;
        let dims = if self.psk == Psk::Bpsk { 1.0 } else { 2.0 };
        let amp = self.abs_sum / (dims * n);
        let sig = dims * amp * amp;
        let err = (self.pow_sum / n - sig).max(1e-12);
        self.abs_sum = 0.0;
        self.pow_sum = 0.0;
        self.stat_n = 0;
        Some(10.0 * (sig / err).log10())
    }

    /// Process up to [`CHUNK`] input samples; symbols are appended to `out`.
    /// Returns the samples used.
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) -> usize {
        let n = input.len().min(CHUNK);
        self.nco.rotate(&input[..n], &mut self.mixed[..n]);
        let (_, made) = self
            .filter
            .process(&self.mixed[..n], &mut self.filtered[..n]);
        for i in 0..made {
            let x = self.filtered[i];
            self.hist = [self.hist[1], self.hist[2], self.hist[3], x];
            while self.next < 1.0 {
                let y = interpolate(&self.hist, self.next.max(0.0) as f32);
                let step = self.sps * (1.0 + self.integ);
                if self.mid_next {
                    self.mid_raw = y;
                    self.mid_next = false;
                    self.next += step / 2.0;
                    continue;
                }
                let e = self.symbol(y, out);
                self.integ = (self.integ + self.ki * e).clamp(-0.01, 0.01);
                let adjust = (self.sps * self.kp * e).clamp(-self.sps / 4.0, self.sps / 4.0);
                self.mid_next = true;
                self.next += step / 2.0 + adjust;
            }
            self.next -= 1.0;
        }
        n
    }

    /// One on-time sample `y` (the halfway sample before it is in
    /// `mid_raw`): AGC, coarse search, Costas loop, the symbol out. Returns
    /// the timing error.
    #[inline]
    fn symbol(&mut self, y: Complex32, out: &mut Vec<Complex32>) -> f64 {
        self.symbols += 1;
        let p = dot(y, y);
        self.power = if self.power == 0.0 {
            p.max(1e-20)
        } else {
            self.power + 0.002 * (p - self.power)
        };
        let g = self.power.max(1e-20).sqrt().recip();
        let (y, m) = (y * g, self.mid_raw * g);
        if self.coarse_on {
            self.coarse_push(m);
            self.coarse_push(y);
        }
        let (s, c) = self.phase.sin_cos();
        let z = y * Complex32::new(c, -s);
        let (s, c) = (self.phase - 0.5 * self.freq).sin_cos();
        let zm = m * Complex32::new(c, -s);
        let (e_phase, e_time, sym) = match self.psk {
            Psk::Bpsk => {
                let e_time = dot(self.prev - y, m);
                self.prev = y;
                (sign(z.re) * z.im, e_time, Complex32::new(z.re, 0.0))
            }
            Psk::Qpsk => {
                let e_time = dot(self.prev - y, m);
                self.prev = y;
                let e = (sign(z.re) * z.im - sign(z.im) * z.re) * FRAC_1_SQRT_2;
                (e, e_time, z)
            }
            Psk::Oqpsk => {
                // I: on time; Q: halfway. Each branch's Gardner term on the
                // derotated samples.
                let e_i = (self.prev.re - z.re) * zm.re;
                let e_q = (self.prev_mid.im - zm.im) * self.prev.im;
                self.prev = z;
                self.prev_mid = zm;
                let e = (sign(z.re) * z.im - sign(zm.im) * zm.re) * FRAC_1_SQRT_2;
                (e, e_i + e_q, Complex32::new(z.re, zm.im))
            }
        };
        self.freq = (self.freq + self.beta * e_phase).clamp(-0.5, 0.5);
        self.phase = wrap(self.phase + self.freq + self.alpha * e_phase);
        self.ted_power = if self.ted_power == 0.0 {
            1.0
        } else {
            self.ted_power + 0.01 * (dot(y, y) - self.ted_power)
        };
        match self.psk {
            Psk::Bpsk => {
                self.abs_sum += f64::from(sym.re.abs());
                self.pow_sum += f64::from(sym.re * sym.re);
            }
            _ => {
                self.abs_sum += f64::from(sym.re.abs() + sym.im.abs());
                self.pow_sum += f64::from(dot(sym, sym));
            }
        }
        self.stat_n += 1;
        out.push(sym);
        // Gardner: ≈ −2τ on average (τ: how late, in symbols).
        f64::from(e_time / self.ted_power.max(1e-20) / 2.0).clamp(-1.0, 1.0)
    }

    #[inline]
    fn coarse_push(&mut self, v: Complex32) {
        let v2 = v * v;
        let p = if self.psk == Psk::Bpsk { v2 } else { v2 * v2 };
        self.fft_buf[self.fft_fill] = p * self.window[self.fft_fill];
        self.fft_fill += 1;
        if self.fft_fill == COARSE_N {
            self.fft_fill = 0;
            self.fft
                .process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
            for (a, v) in self.acc.iter_mut().zip(&self.fft_buf) {
                *a += v.norm_sqr();
            }
            self.acc_n += 1;
            if self.acc_n == COARSE_AVG {
                self.coarse_estimate();
                self.acc.fill(0.0);
                self.acc_n = 0;
            }
        }
    }

    /// Move the NCO if the averaged power spectrum has a clear line away
    /// from 0.
    fn coarse_estimate(&mut self) {
        let n = COARSE_N;
        let mut best = 0;
        let mut best_p = 0.0f32;
        let mut total = 0.0f32;
        for (k, &p) in self.acc.iter().enumerate() {
            total += p;
            if p > best_p {
                best_p = p;
                best = k;
            }
        }
        let mean = (total - best_p) / (n - 1) as f32;
        if best_p < COARSE_SNR * mean {
            return;
        }
        // Parabolic interpolation on the magnitudes around the peak.
        let mag = |k: usize| self.acc[k % n].sqrt();
        let (a, b, c) = (mag(best + n - 1), mag(best), mag(best + 1));
        let d = a - 2.0 * b + c;
        let delta = if d.abs() > 1e-20 {
            (0.5 * (a - c) / d).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let k = if best > n / 2 {
            best as f64 - n as f64
        } else {
            best as f64
        } + f64::from(delta);
        let power = if self.psk == Psk::Bpsk { 2.0 } else { 4.0 };
        // Cycles per half symbol, then per input sample.
        let per_half = k / (power * n as f64);
        let resid = per_half * 2.0 * self.symbol_rate / self.rate;
        // The Costas loop's own estimate goes too: the line measured what
        // was left after it.
        let rad_per_symbol = std::f64::consts::TAU * per_half * 2.0;
        if rad_per_symbol.abs() < COARSE_MIN {
            return;
        }
        self.resid += resid;
        self.nco.set_freq(-(self.base_hz / self.rate + self.resid));
        self.freq = 0.0;
        self.settle_at = self.symbols + SETTLE;
        self.jumps += 1;
    }
}

/// The two ways of pairing an OQPSK receiver's I and Q (see the module
/// docs): `a` as the front end gives them (I with the Q before it), `b`
/// with the Q after it.
pub struct OqpskPairs {
    last: Option<Complex32>,
}

impl OqpskPairs {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// The `b` pairing of `syms`, appended to `out` (one symbol behind).
    pub fn pair(&mut self, syms: &[Complex32], out: &mut Vec<Complex32>) {
        for &s in syms {
            if let Some(l) = self.last {
                out.push(Complex32::new(l.re, s.im));
            }
            self.last = Some(s);
        }
    }
}

impl Default for OqpskPairs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn gauss(&mut self) -> f64 {
            let u = ((self.next() >> 11) as f64 / (1u64 << 53) as f64).max(1e-300);
            let v = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    /// Shaped symbols at `sps` samples each, with an offset of `hz` at
    /// `rate`, `delay` samples late, noise sigma `sigma` per dimension.
    fn signal(
        psk: Psk,
        bits: &[(bool, bool)],
        sps: usize,
        rate: f64,
        hz: f64,
        sigma: f64,
        seed: u64,
    ) -> Vec<Complex32> {
        let h = taps::rrc(sps as f64, 0.5, 10, sps as f64);
        let n = bits.len() * sps + h.len() + sps;
        let mut x = vec![Complex32::default(); n];
        let a = FRAC_1_SQRT_2;
        for (k, &(i, q)) in bits.iter().enumerate() {
            let (vi, vq) = match psk {
                Psk::Bpsk => (if i { -1.0 } else { 1.0 }, 0.0),
                _ => (if i { -a } else { a }, if q { -a } else { a }),
            };
            let qoff = if psk == Psk::Oqpsk { sps / 2 } else { 0 };
            for (j, &t) in h.iter().enumerate() {
                x[k * sps + j].re += vi * t;
                x[k * sps + qoff + j].im += vq * t;
            }
        }
        let mut rng = Rng(seed);
        x.iter()
            .enumerate()
            .map(|(n, &s)| {
                let ph = std::f64::consts::TAU * hz * n as f64 / rate + 0.7;
                s * Complex32::from_polar(1.0, ph as f32)
                    + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32)
            })
            .collect()
    }

    /// Fraction of symbol decisions equal to the sent bits under the best
    /// of the eight symmetries (and, for OQPSK, either pairing) and symbol
    /// alignment, over 3000 symbols near the end.
    fn accuracy(psk: Psk, bits: &[(bool, bool)], syms: &[Complex32]) -> f64 {
        let start = bits.len() - 3200;
        let mut best = 0.0f64;
        let mut alt = Vec::new();
        OqpskPairs::new().pair(syms, &mut alt);
        for cand in [syms, &alt[..]] {
            for rot in 0..4 {
                for conj in [false, true] {
                    for shift in -100i64..100 {
                        let mut ok = 0;
                        for (k, b) in bits.iter().enumerate().skip(start).take(3000) {
                            let Some(s) = cand.get((k as i64 + shift) as usize) else {
                                continue;
                            };
                            let mut s = if conj { s.conj() } else { *s };
                            for _ in 0..rot {
                                s *= Complex32::new(0.0, 1.0);
                            }
                            let i_ok = (s.re < 0.0) == b.0;
                            let q_ok = psk == Psk::Bpsk || (s.im < 0.0) == b.1;
                            ok += usize::from(i_ok && q_ok);
                        }
                        best = best.max(ok as f64 / 3000.0);
                    }
                }
            }
        }
        best
    }

    fn run(psk: Psk, hz: f64, sps: usize) -> (f64, f64) {
        let symbol_rate = 72_000.0;
        let rate = symbol_rate * sps as f64;
        let mut rng = Rng(5);
        let bits: Vec<(bool, bool)> = (0..40_000)
            .map(|_| (rng.next() & 1 == 1, rng.next() & 2 == 2))
            .collect();
        // Es/N0 9 dB.
        let x = signal(psk, &bits, sps, rate, hz, 0.25 * (sps as f64).sqrt(), 9);
        let mut fe = FrontEnd::new(rate, symbol_rate, psk, 0.5);
        let mut syms = Vec::new();
        let mut at = 0;
        while at < x.len() {
            at += fe.process(&x[at..], &mut syms);
        }
        (accuracy(psk, &bits, &syms), fe.offset_hz())
    }

    #[test]
    fn locks_bpsk_qpsk_oqpsk_with_an_offset() {
        for (psk, hz) in [
            (Psk::Bpsk, 12_000.0),
            (Psk::Qpsk, 9_000.0),
            (Psk::Qpsk, -14_000.0),
            (Psk::Oqpsk, 7_000.0),
            (Psk::Oqpsk, -12_000.0),
            (Psk::Oqpsk, 0.0),
            (Psk::Oqpsk, 300.0),
            (Psk::Oqpsk, 3000.0),
            (Psk::Oqpsk, 15000.0),
            (Psk::Qpsk, 300.0),
        ] {
            let (acc, est) = run(psk, hz, 4);
            assert!(acc > 0.99, "{psk:?} at {hz} Hz: {acc}");
            assert!((est - hz).abs() < 50.0, "{psk:?} at {hz} Hz: {est}");
        }
    }
}
