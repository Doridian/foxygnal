//! One platform's demodulator at one data rate, fed the complex baseband of
//! its channel (already brought near 0 Hz and decimated by the channelizer):
//!
//! 1. **Acquire.** A matched filter (root raised cosine, α = 1), a Costas
//!    loop (BPSK: the carrier, clock and FSS are all 0°/180°), and a sliding
//!    correlation with the last carrier symbols, the clock and the FSS at
//!    the symbol spacing. A peak above [`FSS_THRESHOLD`] gives the symbol
//!    timing (to a fraction of a sample) and the absolute phase.
//! 2. **Data.** Symbols are interpolated from the matched filter output;
//!    a Gardner detector keeps the timing and a decision-directed 8PSK loop
//!    the phase. Each symbol goes to the trellis decoder, bit pairs become
//!    bytes (least significant pair first), bytes are descrambled and read:
//!    GOES ID, flag word, then data until EOT (ASCII, pseudo-binary), the
//!    packet length (binary), or until the signal goes away.

use std::f32::consts::FRAC_PI_4;

use foxygnal_core::Complex32;

use super::frame::{Baud, CLOCK, EOT, FSS, Format, SCRAMBLER, binary_length};
use super::viterbi::Tcm;
use crate::digital::loop_gains;

/// Normalised correlation with the preamble that counts as a sync.
pub const FSS_THRESHOLD: f32 = 0.75;
/// Carrier symbols before the clock included in the sync pattern.
const LEAD: usize = 6;
const PATTERN: usize = LEAD + 3 + 15;
/// History of matched filter outputs (a power of two, longer than the
/// pattern at the most samples per symbol).
const HIST: usize = 2048;
/// Half-length of the matched filter in symbols.
const MF_SPAN: f64 = 4.0;
/// A symbol is "good" within this many degrees of an 8PSK point (DAMS-NT).
const GOOD_PHASE_DEG: f32 = 8.4376;
/// Give up on a preamble after this long (s): the longest carrier is 0.5 s.
const ACQUIRE_TIMEOUT_S: f64 = 1.2;

/// How a message ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The EOT character (ASCII and pseudo-binary).
    Eot,
    /// The packet length of a binary message.
    Length,
    /// The signal went away (no EOT seen).
    Lost,
    /// Longer than the fail-safe allows.
    TooLong,
}

impl End {
    pub fn name(self) -> &'static str {
        match self {
            End::Eot => "eot",
            End::Length => "length",
            End::Lost => "lost",
            End::TooLong => "too_long",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Acquire,
    Data,
    /// A message is complete.
    Done(End),
    /// No preamble found in time.
    TimedOut,
}

/// The decoded fields of a message, read once the chain is `Done`.
pub struct Frame {
    pub addr: u32,
    pub flag: u8,
    pub format: Format,
    /// Bytes after the flag word (binary: the payload without length and
    /// CRCs).
    pub data: Vec<u8>,
    /// Bytes descrambled so far (GOES ID and flag included).
    nbytes: usize,
    cur: u8,
    npairs: u8,
    len_bytes: [u8; 3],
    /// Binary: payload length from the header, and payload bytes seen.
    bin_len: Option<usize>,
    bin_seen: usize,
    /// Binary: CRC bytes still to skip.
    crc_skip: u8,
    max: usize,
    end: Option<End>,
}

impl Frame {
    fn new(max: usize) -> Self {
        Self {
            addr: 0,
            flag: 0,
            format: Format::Unknown,
            data: Vec::with_capacity(max + 8),
            nbytes: 0,
            cur: 0,
            npairs: 0,
            len_bytes: [0; 3],
            bin_len: None,
            bin_seen: 0,
            crc_skip: 0,
            max,
            end: None,
        }
    }

    fn reset(&mut self) {
        self.addr = 0;
        self.flag = 0;
        self.format = Format::Unknown;
        self.data.clear();
        self.nbytes = 0;
        self.cur = 0;
        self.npairs = 0;
        self.bin_len = None;
        self.bin_seen = 0;
        self.crc_skip = 0;
        self.end = None;
    }

    /// GOES ID and flag word are in.
    pub fn has_header(&self) -> bool {
        self.nbytes >= 5
    }

    fn pair(&mut self, p: u8) {
        if self.end.is_some() {
            return;
        }
        self.cur |= (p & 3) << (2 * self.npairs);
        self.npairs += 1;
        if self.npairs == 4 {
            let b = self.cur ^ SCRAMBLER[self.nbytes % SCRAMBLER.len()];
            self.cur = 0;
            self.npairs = 0;
            self.byte(b);
            self.nbytes += 1;
        }
    }

    fn byte(&mut self, b: u8) {
        match self.nbytes {
            0..=3 => self.addr = (self.addr << 8) | u32::from(b),
            4 => {
                self.flag = b;
                self.format = Format::of(b);
            }
            i if self.format == Format::Binary && i < 8 => {
                self.len_bytes[i - 5] = b;
                if i == 7 {
                    self.bin_len =
                        binary_length(self.flag, self.len_bytes).filter(|&l| l <= self.max);
                    if self.bin_len == Some(0) {
                        self.end = Some(End::Length);
                    }
                }
            }
            _ => {
                if self.format == Format::Binary {
                    if self.crc_skip > 0 {
                        self.crc_skip -= 1;
                        if self.crc_skip == 0 && Some(self.bin_seen) == self.bin_len {
                            self.end = Some(End::Length);
                        }
                        return;
                    }
                    self.bin_seen += 1;
                    if self.bin_len.is_some()
                        && (self.bin_seen.is_multiple_of(4000)
                            || Some(self.bin_seen) == self.bin_len)
                    {
                        self.crc_skip = 2;
                    }
                } else if b == EOT {
                    self.end = Some(End::Eot);
                    return;
                }
                if self.data.len() >= self.max {
                    self.end = Some(End::TooLong);
                    return;
                }
                self.data.push(b);
            }
        }
    }
}

/// Signal statistics of a message.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Carrier amplitude (input units, before the preamble).
    pub level: f32,
    /// Es/N0 from the spread of the symbols around their points (dB).
    pub snr_db: f32,
    /// RMS phase error (degrees).
    pub phase_noise_deg: f32,
    /// Share of symbols within ±8.44° of a point (%).
    pub good_phase_pct: f32,
    /// Carrier frequency relative to the chain's input centre (Hz).
    pub freq_correction: f64,
    /// Correlation of the preamble (0–1).
    pub sync: f32,
}

/// Per-symbol measurements, held until the trellis decoder releases the
/// symbol (so the statistics end with the message, not with the carrier).
const SYM_RING: usize = 256;

struct Acc {
    /// Phase error, in-phase amplitude, power.
    ring: [(f32, f32, f32); SYM_RING],
    pushed: usize,
    emitted: usize,
    nsym: u32,
    good: u32,
    err2: f64,
    a_sum: f64,
    e2_sum: f64,
}

impl Default for Acc {
    fn default() -> Self {
        Self {
            ring: [(0.0, 0.0, 0.0); SYM_RING],
            pushed: 0,
            emitted: 0,
            nsym: 0,
            good: 0,
            err2: 0.0,
            a_sum: 0.0,
            e2_sum: 0.0,
        }
    }
}

impl Acc {
    /// The decoder released the oldest held symbol; count it if `keep`.
    fn release(&mut self, keep: bool) {
        let (err, re, p) = self.ring[self.emitted % SYM_RING];
        self.emitted += 1;
        if !keep {
            return;
        }
        self.nsym += 1;
        if err.abs().to_degrees() < GOOD_PHASE_DEG {
            self.good += 1;
        }
        self.err2 += f64::from(err * err);
        self.a_sum += f64::from(re);
        self.e2_sum += f64::from(p);
    }
}

/// One data rate's demodulator for one platform.
pub struct Chain {
    pub baud: Baud,
    fo: f64,
    sps: f64,
    pub state: State,
    // Mixer.
    ph: f64,
    w: f64,
    // Frequency estimate during the carrier: running average, lag (chain
    // samples), its weight, the estimate (rad/sample) and the matching
    // rotations of the pattern's samples.
    fll: Complex32,
    lag: u64,
    beta: f32,
    omega: f32,
    rots: [Complex32; PATTERN],
    // Matched filter.
    taps: Vec<f32>,
    line: Vec<Complex32>,
    pos: usize,
    // Matched filter outputs, by chain sample number.
    hist: Vec<Complex32>,
    n: u64,
    start: u64,
    pattern: [f32; PATTERN],
    offsets: [u64; PATTERN],
    c1: f32,
    c2: f32,
    timeout: u64,
    // Data.
    next_t: f64,
    prev: Complex32,
    psi: f32,
    nu: f32,
    kp_dd: f32,
    ki_dd: f32,
    kp_t: f64,
    ki_t: f64,
    t_int: f64,
    ref_amp: f32,
    p_avg: f32,
    low: u32,
    acc: Acc,
    sync: f32,
    tcm: Tcm,
    pub frame: Frame,
    /// Input sample number of the end of the FSS.
    pub fss_time: f64,
    t0: f64,
    d: f64,
}

impl Chain {
    /// A chain for `baud` at chain rate `fo`.
    pub fn new(baud: Baud, fo: f64) -> Self {
        let sps = fo / baud.symbol_rate();
        let half = (MF_SPAN * sps).round() as usize;
        let mut taps: Vec<f32> = (0..=2 * half)
            .map(|i| {
                let t = (i as f64 - half as f64) / sps;
                let w = super::modulator::edge_taper(t, MF_SPAN);
                (super::modulator::rrc1(t) * w) as f32
            })
            .collect();
        let dc: f32 = taps.iter().sum();
        for t in &mut taps {
            *t /= dc;
        }
        let ntaps = taps.len();
        let mut pattern = [0f32; PATTERN];
        let bits = std::iter::repeat_n(&0u8, LEAD).chain(&CLOCK).chain(&FSS);
        for (p, &b) in pattern.iter_mut().zip(bits) {
            *p = if b == 0 { 1.0 } else { -1.0 };
        }
        let mut offsets = [0u64; PATTERN];
        for (k, o) in offsets.iter_mut().enumerate() {
            *o = ((PATTERN - 1 - k) as f64 * sps).round() as u64;
        }
        let (kp_dd, ki_dd) = loop_gains(0.02);
        let (kp_t, ki_t) = loop_gains(0.004);
        Self {
            baud,
            fo,
            sps,
            state: State::Idle,
            ph: 0.0,
            w: 0.0,
            fll: Complex32::new(0.0, 0.0),
            lag: sps.round() as u64,
            // ~40 ms.
            beta: (1.0 / (0.04 * fo)) as f32,
            omega: 0.0,
            rots: [Complex32::new(1.0, 0.0); PATTERN],
            taps,
            line: vec![Complex32::new(0.0, 0.0); 2 * ntaps],
            pos: 0,
            hist: vec![Complex32::new(0.0, 0.0); HIST],
            n: 0,
            start: 0,
            pattern,
            offsets,
            c1: 0.0,
            c2: 0.0,
            timeout: (ACQUIRE_TIMEOUT_S * fo) as u64,
            next_t: 0.0,
            prev: Complex32::new(0.0, 0.0),
            psi: 0.0,
            nu: 0.0,
            kp_dd: kp_dd as f32,
            ki_dd: ki_dd as f32,
            kp_t,
            ki_t,
            t_int: 0.0,
            ref_amp: 1.0,
            p_avg: 0.0,
            low: 0,
            acc: Acc::default(),
            sync: 0.0,
            tcm: Tcm::new(),
            frame: Frame::new(baud.max_bytes()),
            fss_time: 0.0,
            t0: 0.0,
            d: 1.0,
        }
    }

    pub fn rate(&self) -> f64 {
        self.fo
    }

    /// Start acquiring a carrier `freq` Hz from the channel centre.
    pub fn begin(&mut self, freq: f64) {
        self.state = State::Acquire;
        self.ph = 0.0;
        self.w = std::f64::consts::TAU * freq / self.fo;
        self.fll = Complex32::new(0.0, 0.0);
        self.omega = 0.0;
        self.rots = [Complex32::new(1.0, 0.0); PATTERN];
        self.line.fill(Complex32::new(0.0, 0.0));
        self.pos = 0;
        self.n = 0;
        self.start = 0;
        self.c1 = 0.0;
        self.c2 = 0.0;
        self.tcm.reset();
        self.frame.reset();
        self.acc = Acc::default();
        self.low = 0;
        self.t_int = 0.0;
        self.nu = 0.0;
    }

    pub fn stop(&mut self) {
        self.state = State::Idle;
    }

    /// The input ended: a message being received ends here.
    pub fn end_input(&mut self) {
        if self.state == State::Data {
            let (frame, acc) = (&mut self.frame, &mut self.acc);
            self.tcm.finish(&mut |p| {
                acc.release(frame.end.is_none());
                frame.pair(p);
            });
            self.state = State::Done(self.frame.end.unwrap_or(End::Lost));
        }
    }

    /// The carrier frequency the chain has settled on, relative to the
    /// frequency it was started at plus that start offset (Hz).
    pub fn freq(&self) -> f64 {
        self.w * self.fo / std::f64::consts::TAU
            + f64::from(self.nu) * self.baud.symbol_rate() / std::f64::consts::TAU
    }

    /// Feed chain samples; `t0` is the input sample number of `x[0]` and `d`
    /// input samples per chain sample (for time stamps).
    pub fn push(&mut self, x: &[Complex32], t0: f64, d: f64) {
        self.t0 = t0;
        self.d = d;
        for (i, &v) in x.iter().enumerate() {
            if !matches!(self.state, State::Acquire | State::Data) {
                return;
            }
            self.sample(v, i);
        }
    }

    fn sample(&mut self, v: Complex32, i: usize) {
        let (s, c) = self.ph.sin_cos();
        let x = v * Complex32::new(c as f32, -s as f32);
        // Matched filter over a doubled delay line.
        let nt = self.taps.len();
        self.line[self.pos] = x;
        self.line[self.pos + nt] = x;
        self.pos = (self.pos + 1) % nt;
        let mut z = Complex32::new(0.0, 0.0);
        for (h, y) in self.taps.iter().zip(&self.line[self.pos..self.pos + nt]) {
            z += y * *h;
        }
        let n = self.n;
        self.hist[(n as usize) & (HIST - 1)] = z;
        self.n += 1;
        self.ph += self.w;
        if self.ph.abs() > 1e4 {
            self.ph %= std::f64::consts::TAU;
        }
        match self.state {
            State::Acquire => self.acquire(z, n, i),
            State::Data => self.data(n),
            _ => {}
        }
    }

    fn acquire(&mut self, z: Complex32, n: u64, i: usize) {
        let warm = n - self.start;
        let nt = self.taps.len() as u64;
        if warm < nt {
            return;
        }
        if warm > self.timeout {
            self.state = State::TimedOut;
            return;
        }
        // Residual frequency from the carrier: the average of
        // (z[n]·z*[n−L])², which the BPSK of clock and FSS doesn't upset.
        if warm >= nt + self.lag {
            let q = z * self.hist[((n - self.lag) as usize) & (HIST - 1)].conj();
            self.fll += (q * q - self.fll) * self.beta;
            if warm.is_multiple_of(16) {
                self.omega = self.fll.arg() / (2.0 * self.lag as f32);
                for (r, &o) in self.rots.iter_mut().zip(&self.offsets) {
                    *r = Complex32::from_polar(1.0, self.omega * o as f32);
                }
            }
        }
        if warm < nt + self.lag + self.offsets[0] {
            return;
        }
        // Correlate with carrier, clock and FSS (non-coherently, each
        // sample turned back by the frequency estimate).
        let (mut acc, mut e) = (Complex32::new(0.0, 0.0), 0.0f32);
        for ((p, &o), r) in self.pattern.iter().zip(&self.offsets).zip(&self.rots) {
            let y = self.hist[((n - o) as usize) & (HIST - 1)] * r;
            acc += y * *p;
            e += y.norm_sqr();
        }
        let c = acc.norm() / (PATTERN as f32 * e + 1e-30).sqrt();
        // A peak one sample ago.
        let (c0, c1, c2) = (c, self.c1, self.c2);
        if c1 >= FSS_THRESHOLD && c1 >= c2 && c1 > c0 {
            let den = c2 - 2.0 * c1 + c0;
            let delta = if den < 0.0 {
                (0.5 * (c2 - c0) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let last = (n - 1) as f64 + f64::from(delta);
            self.sync = c1;
            // Phase, frequency and amplitude from the known preamble: the
            // pattern's two halves, wiped of their modulation.
            let step = self.omega * self.sps as f32;
            let (mut a, mut b) = (Complex32::new(0.0, 0.0), Complex32::new(0.0, 0.0));
            for (k, p) in self.pattern.iter().enumerate() {
                let back = (PATTERN - 1 - k) as f32;
                let z = self.interp(last - f64::from(back) * self.sps)
                    * Complex32::from_polar(*p, step * back);
                if k < PATTERN / 2 {
                    a += z;
                } else {
                    b += z;
                }
            }
            let half = (PATTERN / 2) as f32;
            let resid = (b * a.conj()).arg() / half;
            // Phase at the last FSS symbol, then one symbol on.
            let psi = (a + b).arg() + resid * (PATTERN as f32 - 1.0) / 2.0;
            self.nu = step + resid;
            self.psi = psi + self.nu;
            self.next_t = last + self.sps;
            self.prev = self.interp(last) * Complex32::from_polar(1.0, -psi);
            self.ref_amp = ((a + b).norm() / PATTERN as f32).max(1e-12);
            self.p_avg = self.ref_amp * self.ref_amp;
            self.fss_time = self.t0 + (i as f64 - 1.0 + f64::from(delta)) * self.d;
            self.state = State::Data;
            return;
        }
        self.c2 = self.c1;
        self.c1 = c;
    }

    fn interp(&self, t: f64) -> Complex32 {
        let i = t.floor();
        let f = (t - i) as f32;
        let i = i as usize;
        let a = self.hist[i & (HIST - 1)];
        let b = self.hist[(i + 1) & (HIST - 1)];
        a + (b - a) * f
    }

    fn data(&mut self, n: u64) {
        while self.state == State::Data && self.next_t.floor() + 1.0 <= n as f64 {
            let t = self.next_t;
            let rot = Complex32::from_polar(1.0, -self.psi);
            let s = self.interp(t) * rot;
            let mid = self.interp(t - 0.5 * self.sps) * rot;
            // Gardner timing error, normalised.
            let et = f64::from(((s - self.prev) * mid.conj()).re / (self.ref_amp * self.ref_amp));
            let et = et.clamp(-1.0, 1.0);
            self.t_int += self.ki_t * et;
            self.next_t += self.sps * (1.0 - (self.kp_t * et + self.t_int));
            // 8PSK decision and phase loop.
            let k = (s.im.atan2(s.re) / FRAC_PI_4).round();
            let dec = Complex32::from_polar(1.0, k * FRAC_PI_4);
            let r = s * dec.conj();
            let err = r.im.atan2(r.re);
            self.psi += self.nu + self.kp_dd * err;
            self.nu += self.ki_dd * err;
            self.prev = s;
            // The signal is gone when its power stays below a fifth.
            let p = s.norm_sqr();
            self.p_avg += 0.1 * (p - self.p_avg);
            if self.p_avg < 0.2 * self.ref_amp * self.ref_amp {
                self.low += 1;
            } else {
                self.low = 0;
            }
            // Statistics are counted as the decoder releases each symbol,
            // up to the end of the message.
            self.acc.ring[self.acc.pushed % SYM_RING] = (err, r.re, s.norm_sqr());
            self.acc.pushed += 1;
            let (frame, acc) = (&mut self.frame, &mut self.acc);
            self.tcm.push(s / self.ref_amp, &mut |p| {
                acc.release(frame.end.is_none());
                frame.pair(p);
            });
            if let Some(e) = self.frame.end {
                self.state = State::Done(e);
            } else if self.low >= 12 {
                let (frame, acc) = (&mut self.frame, &mut self.acc);
                self.tcm.finish(&mut |p| {
                    // The last symbols were the signal fading out.
                    acc.release(frame.end.is_none() && acc.emitted + 12 < acc.pushed);
                    frame.pair(p);
                });
                self.state = State::Done(self.frame.end.unwrap_or(End::Lost));
            }
        }
    }

    /// Signal statistics so far.
    pub fn stats(&self) -> Stats {
        let acc = &self.acc;
        let n = f64::from(acc.nsym.max(1));
        let a = acc.a_sum / n;
        let noise = (acc.e2_sum / n - a * a).max(a * a * 1e-6).max(1e-30);
        Stats {
            level: self.ref_amp,
            snr_db: (10.0 * (a * a / noise).log10()) as f32,
            phase_noise_deg: ((acc.err2 / n).sqrt() as f32).to_degrees(),
            good_phase_pct: 100.0 * acc.good as f32 / n as f32,
            freq_correction: self.freq(),
            sync: self.sync,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::{Format, bch_encode, flag_for};
    use super::super::modulator::{Burst, ascii, modulate};
    use super::*;

    #[test]
    fn decodes_a_clean_burst_at_baseband() {
        for (baud, fo) in [(Baud::B300, 2000.0), (Baud::B1200, 4000.0)] {
            let b = Burst {
                addr: bch_encode(0x1234),
                baud,
                flag: flag_for(Format::Ascii),
                data: ascii("HELLO GOES"),
            };
            let mut x = modulate(&b, fo, 3.0, 0.5, 1.0);
            x.resize(x.len() + fo as usize, Complex32::new(0.0, 0.0));
            let mut c = Chain::new(baud, fo);
            c.begin(0.0);
            c.push(&x, 0.0, 1.0);
            assert_eq!(c.state, State::Done(End::Eot), "{baud:?}");
            assert_eq!(c.frame.addr, bch_encode(0x1234));
            assert_eq!(c.frame.data, ascii("HELLO GOES"));
            let s = c.stats();
            assert!(s.snr_db > 25.0, "{s:?}");
            assert!((s.freq_correction - 3.0).abs() < 1.0, "{s:?}");
        }
    }
}
