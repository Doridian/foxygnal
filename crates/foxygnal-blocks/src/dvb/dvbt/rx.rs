//! The DVB-T receiver: elementary-rate samples → soft bits → TS packets.
//! See the module docs of `dvbt` for the steps.

use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use foxygnal_core::Complex32;
use foxygnal_dsp::kernels::{self, RotateFn};
use rustfft::{Fft, FftPlanner};

use super::frame::{
    Constellation, FRAME_SYMBOLS, Guard, Mode, ModeTables, PILOT_BOOST, TPS_SYNC, Tps, demap_axis,
    slice, tables,
};
use super::resample::ArbResampler;
use crate::dvb::conv::{CodeRate, Depuncturer, Puncture};
use crate::dvb::outer::{OuterDecoder, PACKET_BITS, TsSync};
use crate::dvb::viterbi::Viterbi;
use crate::dvb::{ByteFifo, TS_LEN};

/// Samples (at the elementary rate) collected for mode, guard and timing
/// detection: six of the longest symbols (8k, 1/4) plus one FFT.
const DETECT_LEN: usize = 6 * 10_240 + 8192;
/// Minimum normalized cyclic-prefix correlation to accept a detection.
const DETECT_MIN: f32 = 0.3;
/// Symbol pairs summed for the integer frequency offset search.
const ICFO_PAIRS: usize = 4;
/// The search's peak must stand this far above the mean.
const ICFO_SNR: f32 = 4.0;
/// Frequency loop gain (fraction of the measured residual per symbol).
const K_FREQ: f64 = 0.3;
/// Clock loop gain (fraction of the measured drift per symbol).
const K_CLOCK: f64 = 0.3;
/// Timing (channel centre) loop gain, and its largest step per symbol.
const K_TIMING: f64 = 0.05;
const MAX_TIMING_STEP: f64 = 0.02;
/// Clock offset limit.
const MAX_CLOCK: f64 = 500e-6;
/// Pilot coherence below which a symbol counts as noise, and how many in a
/// row lose the signal.
const MIN_COHERENCE: f32 = 0.15;
const LOST_SYMBOLS: u32 = 24;
/// Frames without a valid TPS before starting over (locked / acquiring).
const TPS_FAILS: u32 = 3;
const TPS_HUNT_SYMBOLS: u32 = 3 * FRAME_SYMBOLS as u32;
/// Frequency interpolation: taps per side and the padding of the pilot grid.
const FI_TAPS: usize = 16;
const FI_PAD: usize = 8;
/// Soft bit scale (one grid unit).
const SOFT_SCALE: f32 = 20.0;
/// Most coded bits in one symbol (8k, 64-QAM).
const MAX_SYMBOL_BITS: usize = 6048 * 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Collecting samples for mode, guard and timing.
    Detect,
    /// Integer frequency offset from the continual pilots.
    IntCfo,
    /// Hunting for the TPS (frame sync, constellation, code rate).
    Tps,
    /// TPS known: equalizing and decoding.
    Locked,
}

/// What the channel coding needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DataParams {
    constellation: Constellation,
    rate: CodeRate,
}

pub struct Receiver {
    /// Elementary sample rate.
    pub fs: f64,
    force_mode: Option<Mode>,
    force_guard: Option<Guard>,
    resamp: ArbResampler,
    /// Resampled samples, from absolute index `tbase`.
    tbuf: Vec<Complex32>,
    tbase: u64,
    pub phase: Phase,
    pub mode: Mode,
    pub guard: Guard,
    n: usize,
    g: usize,
    p: usize,
    tab: &'static ModeTables,
    /// Absolute index of the next FFT window.
    win: u64,
    // NCO (cycles/sample), its phase (cycles) at index `nco_ref`.
    nco_f: f64,
    nco_ph: f64,
    nco_ref: u64,
    nco_steps: [Complex32; 8],
    nco_step8: Complex32,
    rotate: RotateFn,
    ffts: [Arc<dyn Fft<f32>>; 2],
    scratch: Vec<Complex32>,
    fbuf: Vec<Complex32>,
    prev_fft: Vec<Complex32>,
    have_prev_fft: bool,
    /// Carriers of the current symbol (raw, then derotated).
    y: Vec<Complex32>,
    prev_cp: Vec<Complex32>,
    prev_tps: Vec<Complex32>,
    have_prev: bool,
    // Detection scratch.
    det_prod: Vec<Complex32>,
    det_pow: Vec<f32>,
    det_acc: Vec<Complex32>,
    det_e: Vec<f32>,
    // Integer frequency offset search.
    icfo: Vec<f32>,
    icfo_pairs: usize,
    icfo_tries: u32,
    // Loops.
    phi: f32,
    /// Clock offset estimate (positive: the input clock is fast).
    pub clock: f64,
    /// Window shift applied since the last drift measurement (samples).
    applied: f64,
    low_coherence: u32,
    // TPS.
    tps_hunt: u32,
    tps_bits: [u8; FRAME_SYMBOLS],
    /// Frame index of the next symbol, once the TPS sync word was seen.
    next_l: Option<usize>,
    tps_fails: u32,
    hunt_symbols: u32,
    pub tps: Option<Tps>,
    /// Cell id bytes (high, low) as last seen.
    cell: [Option<u8>; 2],
    pub tps_corrected: u64,
    data: Option<DataParams>,
    // Channel estimation.
    ring: Vec<Complex32>,
    ring_l: [usize; 4],
    sym_count: u64,
    ce_start: u64,
    g_last: Vec<Complex32>,
    g_prev: Vec<Complex32>,
    g_time: Vec<u64>,
    gpad: Vec<Complex32>,
    hfull: Vec<Complex32>,
    /// e^{+j2πk'(G/2)/N}: centres the channel's delays on 0.
    rot: Vec<Complex32>,
    fi_taps: [[f32; FI_TAPS]; 2],
    soft_c: Vec<i8>,
    soft_out: Vec<i8>,
    // Statistics since the last `take_*`.
    mer_sig: f64,
    mer_err: f64,
    /// Mean delay error of the channel (samples), last measured.
    pub delay: f64,
    pub fec: Fec,
}

fn nco_steps(f: f64) -> ([Complex32; 8], Complex32) {
    let turn = |n: f64| {
        let (s, c) = (std::f64::consts::TAU * f * n).sin_cos();
        Complex32::new(c as f32, s as f32)
    };
    (std::array::from_fn(|j| turn(j as f64)), turn(8.0))
}

/// Fractional-delay interpolators (1/3 and 2/3 of the pilot spacing) for a
/// cutoff `rho` of the pilot grid's Nyquist frequency.
fn fi_taps(rho: f64) -> [[f32; FI_TAPS]; 2] {
    std::array::from_fn(|r| {
        let mu = (r + 1) as f64 / 3.0;
        let half = FI_TAPS as f64 / 2.0 + 0.5;
        let beta = 4.0f64;
        let i0 = |x: f64| {
            let (mut s, mut t) = (1.0, 1.0);
            for k in 1..40 {
                t *= x * x / 4.0 / (k * k) as f64;
                s += t;
            }
            s
        };
        let mut h: [f64; FI_TAPS] = std::array::from_fn(|i| {
            let x = i as f64 - (FI_TAPS / 2 - 1) as f64 - mu;
            let a = std::f64::consts::PI * rho * x;
            let s = if a.abs() < 1e-12 { 1.0 } else { a.sin() / a };
            let q = (x / half).clamp(-1.0, 1.0);
            s * i0(beta * (1.0 - q * q).sqrt())
        });
        let sum: f64 = h.iter().sum();
        h.iter_mut().for_each(|v| *v /= sum);
        h.map(|v| v as f32)
    })
}

impl Receiver {
    pub fn new(in_rate: f64, fs: f64, mode: Option<Mode>, guard: Option<Guard>) -> Self {
        let mut planner = FftPlanner::new();
        let ffts = Mode::ALL.map(|m| planner.plan_fft_forward(m.fft_len()));
        let scratch_len = ffts
            .iter()
            .map(|f| f.get_inplace_scratch_len())
            .max()
            .unwrap_or(0);
        let kmax = Mode::K8.carriers();
        let grid = Mode::K8.kmax() / 3 + 1;
        let (nco_steps, nco_step8) = nco_steps(0.0);
        let m = mode.unwrap_or(Mode::K2);
        Self {
            fs,
            force_mode: mode,
            force_guard: guard,
            resamp: ArbResampler::new(in_rate, fs, super::CHUNK),
            tbuf: Vec::with_capacity(DETECT_LEN + 16_384),
            tbase: 0,
            phase: Phase::Detect,
            mode: m,
            guard: guard.unwrap_or(Guard::G1_4),
            n: m.fft_len(),
            g: 0,
            p: m.fft_len(),
            tab: tables(m),
            win: 0,
            nco_f: 0.0,
            nco_ph: 0.0,
            nco_ref: 0,
            nco_steps,
            nco_step8,
            rotate: kernels::kernels().rotate,
            ffts,
            scratch: vec![Complex32::default(); scratch_len],
            fbuf: vec![Complex32::default(); 8192],
            prev_fft: vec![Complex32::default(); 8192],
            have_prev_fft: false,
            y: vec![Complex32::default(); kmax],
            prev_cp: vec![Complex32::default(); 256],
            prev_tps: vec![Complex32::default(); 68],
            have_prev: false,
            det_prod: vec![Complex32::default(); DETECT_LEN],
            det_pow: vec![0.0; DETECT_LEN],
            det_acc: vec![Complex32::default(); 10_240],
            det_e: vec![0.0; 10_240],
            icfo: vec![0.0; 2 * 8192],
            icfo_pairs: 0,
            icfo_tries: 0,
            phi: 0.0,
            clock: 0.0,
            applied: 0.0,
            low_coherence: 0,
            tps_hunt: 0,
            tps_bits: [0; FRAME_SYMBOLS],
            next_l: None,
            tps_fails: 0,
            hunt_symbols: 0,
            tps: None,
            cell: [None; 2],
            tps_corrected: 0,
            data: None,
            ring: vec![Complex32::default(); 4 * kmax],
            ring_l: [0; 4],
            sym_count: 0,
            ce_start: 0,
            g_last: vec![Complex32::default(); grid],
            g_prev: vec![Complex32::default(); grid],
            g_time: vec![0; grid],
            gpad: vec![Complex32::default(); grid + 2 * FI_PAD],
            hfull: vec![Complex32::default(); kmax],
            rot: vec![Complex32::default(); kmax],
            fi_taps: fi_taps(1.0),
            soft_c: vec![0; MAX_SYMBOL_BITS],
            soft_out: vec![0; MAX_SYMBOL_BITS],
            mer_sig: 0.0,
            mer_err: 0.0,
            delay: 0.0,
            fec: Fec::new(),
        }
    }

    /// The carrier offset found, in Hz.
    pub fn offset_hz(&self) -> f64 {
        -self.nco_f * self.fs
    }

    /// The cell id, once both bytes were seen.
    pub fn cell_id(&self) -> Option<u16> {
        match self.cell {
            [Some(h), Some(l)] => Some(u16::from(h) << 8 | u16::from(l)),
            _ => None,
        }
    }

    /// MER in dB since the last call.
    pub fn take_mer(&mut self) -> Option<f64> {
        if self.mer_err <= 0.0 {
            return None;
        }
        let m = 10.0 * (self.mer_sig / self.mer_err).log10();
        self.mer_sig = 0.0;
        self.mer_err = 0.0;
        Some(m)
    }

    fn set_nco(&mut self, f: f64) {
        let at = self.win;
        self.nco_ph =
            (self.nco_ph + self.nco_f * (at as f64 - self.nco_ref as f64)).rem_euclid(1.0);
        self.nco_ref = at;
        self.nco_f = f;
        (self.nco_steps, self.nco_step8) = nco_steps(f);
    }

    /// Start over: detect the signal again.
    fn restart(&mut self) {
        self.phase = Phase::Detect;
        self.tbase += self.tbuf.len() as u64;
        self.tbuf.clear();
        self.next_l = None;
        self.tps = None;
        self.data = None;
        self.cell = [None; 2];
        self.have_prev = false;
        self.have_prev_fft = false;
        self.fec.stop();
        self.resamp.set_offset(self.clock);
    }

    /// Take input samples (at most `CHUNK`) and run everything they allow.
    pub fn process(&mut self, input: &[Complex32], fifo: &mut ByteFifo) {
        self.resamp.compact();
        self.resamp.push(input);
        loop {
            if self.phase == Phase::Detect {
                if !self.produce(DETECT_LEN) {
                    break;
                }
                self.detect();
                continue;
            }
            if !self.fill_window() {
                break;
            }
            self.symbol(fifo);
            self.win += self.p as u64;
        }
    }

    /// Fill `tbuf` to `len` samples; false if the input ran out.
    fn produce(&mut self, len: usize) -> bool {
        let old = self.tbuf.len();
        if old >= len {
            return true;
        }
        self.tbuf.resize(len, Complex32::default());
        let made = self.resamp.process(&mut self.tbuf[old..]);
        self.tbuf.truncate(old + made);
        self.tbuf.len() >= len
    }

    /// Make `tbuf` hold exactly the window at `win` (skipping the guard
    /// interval without computing it).
    fn fill_window(&mut self) -> bool {
        let end = self.tbase + self.tbuf.len() as u64;
        if end < self.win {
            self.resamp.skip((self.win - end) as f64);
            self.tbuf.clear();
            self.tbase = self.win;
        } else if self.win > self.tbase {
            self.tbuf.drain(..(self.win - self.tbase) as usize);
            self.tbase = self.win;
        }
        self.produce(self.n)
    }

    // ---- Detection ------------------------------------------------------------------

    /// Mode, guard, symbol timing and fractional frequency offset from the
    /// cyclic prefix correlation, folded over the symbols in `tbuf`.
    fn detect(&mut self) {
        let len = self.tbuf.len();
        // (rho, mode, guard, position, frac offset)
        let mut best: Option<(f32, Mode, Guard, usize, f64)> = None;
        for mode in Mode::ALL {
            if self.force_mode.is_some_and(|m| m != mode) {
                continue;
            }
            let n = mode.fft_len();
            let m = len - n;
            let r = &self.tbuf;
            for i in 0..m {
                let (a, b) = (r[i], r[i + n]);
                self.det_prod[i] = a * b.conj();
                self.det_pow[i] = 0.5 * (a.norm_sqr() + b.norm_sqr());
            }
            for guard in Guard::ALL {
                if self.force_guard.is_some_and(|g| g != guard) {
                    continue;
                }
                let g = guard.len(n);
                let p = n + g;
                let nsym = (m - g) / p;
                if nsym < 2 {
                    continue;
                }
                let (acc, e) = (&mut self.det_acc[..p], &mut self.det_e[..p]);
                acc.fill(Complex32::default());
                e.fill(0.0);
                let (mut s_re, mut s_im, mut s_e) = (0f64, 0f64, 0f64);
                for i in 0..g {
                    s_re += f64::from(self.det_prod[i].re);
                    s_im += f64::from(self.det_prod[i].im);
                    s_e += f64::from(self.det_pow[i]);
                }
                let mut j = 0;
                for i in 0..nsym * p {
                    acc[j] += Complex32::new(s_re as f32, s_im as f32);
                    e[j] += s_e as f32;
                    let (add, sub) = (self.det_prod[i + g], self.det_prod[i]);
                    s_re += f64::from(add.re - sub.re);
                    s_im += f64::from(add.im - sub.im);
                    s_e += f64::from(self.det_pow[i + g] - self.det_pow[i]);
                    j += 1;
                    if j == p {
                        j = 0;
                    }
                }
                for i in 0..p {
                    let rho = acc[i].norm() / e[i].max(1e-30);
                    if best.is_none_or(|b| rho > b.0) {
                        let frac = -f64::from(acc[i].arg()) / std::f64::consts::TAU;
                        best = Some((rho, mode, guard, i, frac));
                    }
                }
            }
        }
        let Some((_, mode, guard, pos, frac)) = best.filter(|b| b.0 >= DETECT_MIN) else {
            // Nothing: keep the newer half and try again.
            let keep = len / 2;
            self.tbuf.drain(..len - keep);
            self.tbase += (len - keep) as u64;
            return;
        };
        self.set_mode(mode, guard);
        // The first window: half a guard into the cyclic prefix (echoes up to
        // G/2 before and after the main path stay inside the prefix), on the
        // first symbol not yet in `tbuf`.
        let start = pos + self.g / 2;
        let ahead = (len - start).div_ceil(self.p);
        self.win = self.tbase + (start + ahead * self.p) as u64;
        self.tbase += len as u64;
        self.tbuf.clear();
        self.nco_ph = 0.0;
        self.nco_f = 0.0;
        self.set_nco(-frac / self.n as f64);
        self.phase = Phase::IntCfo;
        self.icfo_pairs = 0;
        self.icfo_tries = 0;
        self.icfo.fill(0.0);
        self.have_prev_fft = false;
        self.have_prev = false;
        self.applied = 0.0;
        self.low_coherence = 0;
    }

    fn set_mode(&mut self, mode: Mode, guard: Guard) {
        self.mode = mode;
        self.guard = guard;
        self.n = mode.fft_len();
        self.g = guard.len(self.n);
        self.p = self.n + self.g;
        self.tab = tables(mode);
        let l = &tables(mode).layout;
        let c = (self.g / 2) as f32;
        for k in 0..l.k {
            self.rot[k] = Complex32::from_polar(1.0, TAU * l.rel(k) * c / self.n as f32);
        }
        // Delays span ±G/2 around the centre; the pilot grid (every 3rd
        // carrier) resolves ±N/6.
        let rho = ((self.g as f64 / 2.0) / (self.n as f64 / 6.0) + 0.25).min(1.0);
        self.fi_taps = fi_taps(rho);
    }

    // ---- Per symbol -----------------------------------------------------------------

    fn symbol(&mut self, fifo: &mut ByteFifo) {
        let n = self.n;
        // NCO, then FFT.
        let ph = self.nco_ph + self.nco_f * (self.win as f64 - self.nco_ref as f64);
        let ph = std::f64::consts::TAU * ph.rem_euclid(1.0);
        let p0 = Complex32::new(ph.cos() as f32, ph.sin() as f32);
        (self.rotate)(
            p0,
            &self.nco_steps,
            self.nco_step8,
            &self.tbuf[..n],
            &mut self.fbuf[..n],
        );
        let fft = &self.ffts[usize::from(self.mode == Mode::K8)];
        fft.process_with_scratch(&mut self.fbuf[..n], &mut self.scratch);
        if self.phase == Phase::IntCfo {
            self.int_cfo();
            return;
        }
        let tab = self.tab;
        let layout = &tab.layout;
        let k = layout.k;
        let b0 = layout.bin0;
        // Carriers 0 … kmax: bins N − kmax/2 … N − 1, then 0 … kmax/2.
        let first = n - b0;
        self.y[..first].copy_from_slice(&self.fbuf[b0..n]);
        self.y[first..k].copy_from_slice(&self.fbuf[..k - first]);

        let l = self.next_l;
        self.next_l = l.map(|l| (l + 1) % FRAME_SYMBOLS);
        if self.have_prev {
            let (dphi, drift, coherence) = self.measure();
            self.loops(dphi, drift);
            if coherence < MIN_COHERENCE {
                self.low_coherence += 1;
                if self.low_coherence > LOST_SYMBOLS {
                    self.restart();
                    return;
                }
            } else {
                self.low_coherence = 0;
            }
            let bit = self.tps_bit(dphi, drift);
            self.tps_step(l, bit);
            if self.phase == Phase::Detect {
                return;
            }
        }
        // Keep the raw pilots for the next differential measurement.
        for (i, &c) in layout.continual.iter().enumerate() {
            self.prev_cp[i] = self.y[c as usize];
        }
        for (i, &c) in layout.tps.iter().enumerate() {
            self.prev_tps[i] = self.y[c as usize];
        }
        self.have_prev = true;
        if self.phase == Phase::Locked
            && let Some(l) = l
        {
            self.channel(l, fifo);
        }
    }

    /// The integer frequency offset: the shift at which the continual pilots
    /// of consecutive symbols correlate.
    fn int_cfo(&mut self) {
        let n = self.n;
        let tab = self.tab;
        let layout = &tab.layout;
        let range = (n - layout.k) / 2;
        if self.have_prev_fft {
            for (mi, m) in (-(range as isize)..=range as isize).enumerate() {
                let mut s = Complex32::default();
                for &c in &layout.continual {
                    let b = (layout.bin(c as usize) as isize + m).rem_euclid(n as isize) as usize;
                    s += self.fbuf[b] * self.prev_fft[b].conj();
                }
                self.icfo[mi] += s.norm();
            }
            self.icfo_pairs += 1;
        }
        std::mem::swap(&mut self.fbuf, &mut self.prev_fft);
        self.have_prev_fft = true;
        if self.icfo_pairs < ICFO_PAIRS {
            return;
        }
        let cands = &self.icfo[..2 * range + 1];
        let (best, &peak) = cands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap_or((range, &0.0));
        let mean = cands.iter().sum::<f32>() / cands.len() as f32;
        if peak < ICFO_SNR * mean {
            self.icfo_tries += 1;
            self.icfo_pairs = 0;
            self.icfo.fill(0.0);
            if self.icfo_tries >= 3 {
                self.restart();
            }
            return;
        }
        let m = best as f64 - range as f64;
        // The next window starts at win + p: keep the phase continuous there.
        self.set_nco(self.nco_f - m / n as f64);
        self.phase = Phase::Tps;
        self.have_prev = false;
        self.next_l = None;
        self.tps_fails = 0;
        self.hunt_symbols = 0;
    }

    /// Common phase change and timing drift since the last symbol, from the
    /// continual pilots, and their coherence (1 = clean).
    fn measure(&mut self) -> (f32, f64, f32) {
        let tab = self.tab;
        let layout = &tab.layout;
        let mut zsum = Complex32::default();
        let mut mag = 0.0f32;
        for (i, &c) in layout.continual.iter().enumerate() {
            let z = self.y[c as usize] * self.prev_cp[i].conj();
            zsum += z;
            mag += z.norm();
        }
        let dphi = zsum.arg();
        let un = zsum.conj() / zsum.norm().max(1e-30);
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for (i, &c) in layout.continual.iter().enumerate() {
            let z = self.y[c as usize] * self.prev_cp[i].conj() * un;
            let w = f64::from(z.norm());
            let kr = f64::from(layout.rel(c as usize));
            num += kr * f64::from(z.arg()) * w;
            den += kr * kr * w;
        }
        let slope = if den > 0.0 { num / den } else { 0.0 };
        let drift = slope * self.n as f64 / std::f64::consts::TAU;
        (dphi, drift, zsum.norm() / mag.max(1e-30))
    }

    fn loops(&mut self, dphi: f32, drift: f64) {
        // Clock: a window that slides later every symbol means the input
        // is consumed too fast.
        let d = drift - self.applied;
        self.applied = 0.0;
        self.clock = (self.clock - K_CLOCK * d / self.p as f64).clamp(-MAX_CLOCK, MAX_CLOCK);
        self.resamp.set_offset(self.clock);
        // Frequency: the residual in carrier spacings.
        let eps = f64::from(dphi) / (std::f64::consts::TAU * self.p as f64 / self.n as f64);
        self.set_nco(self.nco_f - K_FREQ * eps / self.n as f64);
        self.phi += dphi;
        if self.phi > PI {
            self.phi -= TAU;
        } else if self.phi < -PI {
            self.phi += TAU;
        }
    }

    /// The differential TPS bit of this symbol (soft combined over all TPS
    /// carriers, with the common phase and timing drift removed).
    fn tps_bit(&self, dphi: f32, drift: f64) -> u8 {
        let layout = &self.tab.layout;
        let mut s = 0.0f32;
        let k = TAU * drift as f32 / self.n as f32;
        for (i, &c) in layout.tps.iter().enumerate() {
            let z = self.y[c as usize] * self.prev_tps[i].conj();
            let r = Complex32::from_polar(1.0, -(dphi + k * layout.rel(c as usize)));
            s += (z * r).re;
        }
        u8::from(s < 0.0)
    }

    /// Frame sync and TPS decoding. `l`: this symbol's index in its frame.
    fn tps_step(&mut self, l: Option<usize>, bit: u8) {
        self.tps_hunt = (self.tps_hunt << 1) | u32::from(bit);
        let Some(l) = l else {
            self.hunt_symbols += 1;
            let w = self.tps_hunt as u16;
            if w == TPS_SYNC || w == !TPS_SYNC {
                // This was s16.
                for i in 0..16 {
                    self.tps_bits[16 - i] = ((w >> i) & 1) as u8;
                }
                self.next_l = Some(17);
            } else if self.hunt_symbols > TPS_HUNT_SYMBOLS {
                self.restart();
            }
            return;
        };
        if l > 0 {
            self.tps_bits[l] = bit;
        }
        if l != FRAME_SYMBOLS - 1 {
            return;
        }
        match Tps::decode(&self.tps_bits) {
            Some((tps, fixed)) if tps.mode == self.mode && tps.guard == self.guard => {
                self.tps_fails = 0;
                self.tps_corrected += u64::from(fixed);
                self.cell[usize::from(tps.frame % 2 == 1)] = Some(tps.cell_id_byte);
                self.tps = Some(tps);
                let data = (tps.hierarchy == 0).then_some(DataParams {
                    constellation: tps.constellation,
                    rate: tps.rate_hp,
                });
                if self.phase != Phase::Locked || data != self.data {
                    self.start_data(data);
                }
            }
            Some(_) => {
                // The TPS disagrees with what was detected: detect again,
                // trusting it.
                self.restart();
            }
            None => {
                self.tps_fails += 1;
                if self.phase == Phase::Tps {
                    self.next_l = None;
                    self.hunt_symbols = 0;
                }
                if self.tps_fails >= TPS_FAILS {
                    self.restart();
                }
            }
        }
    }

    fn start_data(&mut self, data: Option<DataParams>) {
        self.phase = Phase::Locked;
        self.data = data;
        self.ce_start = self.sym_count;
        match data {
            Some(d) => self.fec.start(d.rate),
            None => self.fec.stop(),
        }
    }

    // ---- Channel estimation, demapping, deinterleaving ------------------------------

    fn channel(&mut self, l: usize, fifo: &mut ByteFifo) {
        let tab = self.tab;
        let layout = &tab.layout;
        let k = layout.k;
        let s = self.sym_count;
        self.sym_count += 1;
        // Store the derotated symbol.
        let slot = (s % 4) as usize;
        let rot = Complex32::from_polar(1.0, -self.phi);
        let dst = &mut self.ring[slot * k..(slot + 1) * k];
        for (d, &v) in dst.iter_mut().zip(&self.y[..k]) {
            *d = v * rot;
        }
        self.ring_l[slot] = l;
        // Scattered pilots of this symbol into the grid.
        let mut c = 3 * (l % 4);
        while c < k {
            let j = c / 3;
            self.g_prev[j] = self.g_last[j];
            self.g_last[j] = dst[c] * (1.0 / (PILOT_BOOST * layout.prbs[c]));
            self.g_time[j] = s;
            c += 12;
        }
        if s >= self.ce_start + 6 && self.data.is_some() {
            self.equalize(s - 3, fifo);
        }
    }

    fn equalize(&mut self, m: u64, fifo: &mut ByteFifo) {
        let Some(data) = self.data else {
            return;
        };
        let tab = self.tab;
        let layout = &tab.layout;
        let k = layout.k;
        let jmax = (k - 1) / 3;
        // Time interpolation (linear, between pilots 4 symbols apart), and
        // centring of the delays.
        for j in 0..=jmax {
            let t = self.g_time[j];
            let h = if t <= m {
                self.g_last[j]
            } else {
                let frac = (m + 4 - t) as f32 / 4.0;
                self.g_prev[j] + (self.g_last[j] - self.g_prev[j]) * frac
            };
            self.gpad[FI_PAD + j] = h * self.rot[3 * j];
        }
        let (g0, gn) = (self.gpad[FI_PAD], self.gpad[FI_PAD + jmax]);
        for i in 1..=FI_PAD {
            self.gpad[FI_PAD - i] = g0 * 2.0 - self.gpad[FI_PAD + i];
            self.gpad[FI_PAD + jmax + i] = gn * 2.0 - self.gpad[FI_PAD + jmax - i];
        }
        // Mean delay error (the delays were centred on G/2).
        let mut z = Complex32::default();
        for j in FI_PAD..FI_PAD + jmax {
            z += self.gpad[j + 1] * self.gpad[j].conj();
        }
        self.delay = -f64::from(z.arg()) * self.n as f64 / (6.0 * std::f64::consts::PI);
        let step = (K_TIMING * self.delay).clamp(-MAX_TIMING_STEP, MAX_TIMING_STEP);
        self.resamp.skip(step);
        self.applied += step;
        // Frequency interpolation.
        for j in 0..jmax {
            let w = &self.gpad[j + FI_PAD + 1 - FI_TAPS / 2..j + FI_PAD + 1 + FI_TAPS / 2];
            self.hfull[3 * j] = self.gpad[FI_PAD + j];
            for r in 0..2 {
                let mut acc = Complex32::default();
                for (h, &v) in self.fi_taps[r].iter().zip(w) {
                    acc += v * *h;
                }
                self.hfull[3 * j + 1 + r] = acc;
            }
        }
        self.hfull[3 * jmax] = self.gpad[FI_PAD + jmax];

        // Equalize and demap the data carriers.
        let slot = (m % 4) as usize;
        let l = self.ring_l[slot];
        let ys = &self.ring[slot * k..(slot + 1) * k];
        let carriers = &layout.data[l % 4];
        let cons = data.constellation;
        let v = cons.bits();
        let scale = cons.grid_power().sqrt();
        let mut wsum = 0.0f32;
        for &c in carriers {
            let c = c as usize;
            let h = self.hfull[c] * self.rot[c].conj();
            self.hfull[c] = h;
            wsum += h.norm_sqr();
        }
        let wmean = (wsum / carriers.len() as f32).max(1e-30);
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        let mut soft = [0f32; 6];
        for (ci, &c) in carriers.iter().enumerate() {
            let c = c as usize;
            let h = self.hfull[c];
            let w = h.norm_sqr().max(1e-30 * wmean);
            let r = ys[c] * h.conj() * (scale / w);
            let cw = SOFT_SCALE * (w / wmean).min(4.0);
            demap_axis(cons, r.re, cw, &mut soft[0..]);
            demap_axis(cons, r.im, cw, &mut soft[1..]);
            for (o, &f) in self.soft_c[ci * v..(ci + 1) * v].iter_mut().zip(&soft) {
                // Saturating, truncating (rounding would cost a libm call).
                *o = f.clamp(-127.0, 127.0) as i8;
            }
            let (er, ei) = (r.re - slice(cons, r.re), r.im - slice(cons, r.im));
            err += f64::from(er * er + ei * ei);
            sig += f64::from(cons.grid_power());
        }
        self.mer_sig += sig;
        self.mer_err += err;
        // Symbol and bit deinterleaving in one gather.
        let inner = tab.inner(cons);
        let perm = &inner.perm[l % 2];
        let bits = inner.bits;
        for (o, &src) in self.soft_out[..bits].iter_mut().zip(perm) {
            *o = self.soft_c[src as usize];
        }
        self.fec.push(&self.soft_out[..bits], fifo);
    }
}

// ---- FEC ----------------------------------------------------------------------------

/// Soft bits collected to find the puncturing phase.
const ACQ_BITS: usize = 12_096;
/// Decoded bits without a packet grid before searching again.
const LOST_BITS: usize = 40 * PACKET_BITS;

/// Depuncturing, Viterbi, packet sync and the outer decoder, plus the
/// puncturing phase search (the phase isn't signalled; a symbol always
/// holds whole puncturing periods, so it stays put once found).
pub struct Fec {
    pub rate: Option<CodeRate>,
    pub tracking: bool,
    acq: Vec<i8>,
    pairs: Vec<[i8; 2]>,
    dp: Depuncturer,
    pub vit: Viterbi,
    search_vit: Viterbi,
    pub sync: TsSync,
    pub outer: OuterDecoder,
    unlocked_bits: usize,
}

impl Fec {
    fn new() -> Self {
        Self {
            rate: None,
            tracking: false,
            acq: Vec::with_capacity(ACQ_BITS + MAX_SYMBOL_BITS),
            pairs: Vec::with_capacity(MAX_SYMBOL_BITS),
            dp: Depuncturer::new(CodeRate::R1_2, 0),
            vit: Viterbi::new(),
            search_vit: Viterbi::new(),
            sync: TsSync::new(),
            outer: OuterDecoder::new(),
            unlocked_bits: 0,
        }
    }

    pub fn locked(&self) -> bool {
        self.tracking && self.sync.locked()
    }

    fn start(&mut self, rate: CodeRate) {
        self.rate = Some(rate);
        self.tracking = false;
        self.acq.clear();
        self.sync.reset();
    }

    fn stop(&mut self) {
        self.rate = None;
        self.tracking = false;
        self.acq.clear();
        self.sync.reset();
    }

    /// Soft bits of whole symbols, in coded order.
    fn push(&mut self, soft: &[i8], fifo: &mut ByteFifo) {
        let Some(rate) = self.rate else {
            return;
        };
        if self.tracking {
            self.track(soft, fifo);
            return;
        }
        self.acq.extend_from_slice(soft);
        if self.acq.len() < ACQ_BITS {
            return;
        }
        let mut best = (f64::INFINITY, 0);
        for phase in Puncture::new(rate).phases(1) {
            let ber = self.try_phase(rate, phase);
            if ber < best.0 {
                best = (ber, phase);
            }
        }
        self.dp = Depuncturer::new(rate, best.1);
        self.vit.reset();
        self.sync.reset();
        self.outer.reset();
        self.unlocked_bits = 0;
        self.tracking = true;
        let acq = std::mem::take(&mut self.acq);
        for c in acq.chunks(MAX_SYMBOL_BITS) {
            self.track(c, fifo);
        }
        self.acq = acq;
        self.acq.clear();
    }

    /// Re-encoded bit error rate of the collected bits under a phase.
    fn try_phase(&mut self, rate: CodeRate, phase: usize) -> f64 {
        let mut dp = Depuncturer::new(rate, phase);
        self.pairs.clear();
        dp.push_slice(&self.acq[..ACQ_BITS], &mut self.pairs);
        self.search_vit.reset();
        self.search_vit.reset_counters();
        self.search_vit.decode(&self.pairs, |_| {});
        if self.search_vit.compared == 0 {
            return 1.0;
        }
        self.search_vit.errors as f64 / self.search_vit.compared as f64
    }

    fn track(&mut self, soft: &[i8], fifo: &mut ByteFifo) {
        self.pairs.clear();
        self.dp.push_slice(soft, &mut self.pairs);
        let Self {
            vit,
            sync,
            outer,
            unlocked_bits,
            pairs,
            ..
        } = self;
        let mut ts = [0u8; TS_LEN];
        let losses = sync.losses;
        vit.decode(pairs, |bits| {
            sync.push(bits, |pkt, fresh| {
                if fresh {
                    outer.reset();
                }
                if outer.decode(pkt, &mut ts) {
                    fifo.write(&ts);
                }
            });
            if sync.locked() {
                *unlocked_bits = 0;
            } else {
                *unlocked_bits += bits.len();
            }
        });
        if self.unlocked_bits > LOST_BITS || self.sync.losses != losses {
            self.tracking = false;
            self.acq.clear();
            self.sync.reset();
        }
    }
}
