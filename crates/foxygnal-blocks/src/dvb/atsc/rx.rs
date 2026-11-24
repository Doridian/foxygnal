//! The 8-VSB receiver's signal processing: the input stage, carrier, timing,
//! segment and field sync, the equalizer, and the hand-over to the decoders.
//! See the module docs for the overview.

use std::f64::consts::{PI, TAU};
use std::sync::Arc;

use foxygnal_core::Complex32;
use foxygnal_dsp::{Rotator, kernels, taps::rrc_at};
use rustfft::{Fft, FftPlanner};

use super::fec::{
    FIELD_SEGMENTS, FIELD_SYNC_KNOWN, OuterDecoder, PN63_AT, PN511_AT, SEG_SYNC_LEVELS, SEGMENT,
    TrellisDecoder, field_sync_known, pn63, pn511,
};
use super::{ROLLOFF, SYMBOL_RATE};
use crate::dvb::ByteFifo;

/// Matched-filter phases (nearest phase: the timing error is under 1/512 of
/// an input sample).
const PHASES: usize = 256;
/// The matched filter spans ±16 symbols (±8 periods of its RRC).
const HALF_SPAN: f64 = 16.0;

/// Mean power of the 8 data levels.
const DATA_POWER: f32 = 21.0;

/// Segments in a field, field sync included.
const FIELD_LEN: usize = FIELD_SEGMENTS + 1;

/// The receive filter: a root-raised-cosine (α = 0.1152) for a "symbol"
/// of two ATSC symbols, centred on the channel, evaluated at any position
/// of the input: matched filtering and resampling in one step.
struct SymbolFilter {
    half: usize,
    taps: usize,
    /// (PHASES + 1) rows of `taps` coefficients, each twice (for `dot_c32`).
    bank: Vec<f32>,
    dot: fn(&[f32], &[f32]) -> Complex32,
}

impl SymbolFilter {
    /// `sps`: input samples per symbol.
    fn new(sps: f64) -> Self {
        let half = (HALF_SPAN * sps).ceil() as usize;
        let taps = 2 * half;
        let row = |mu: f64| -> Vec<f64> {
            (0..taps)
                .map(|t| {
                    let u = t as f64 + 1.0 - half as f64 - mu;
                    rrc_at(u / sps / 2.0, ROLLOFF)
                })
                .collect()
        };
        let norm: f64 = row(0.0).iter().sum();
        let mut bank = Vec::with_capacity((PHASES + 1) * taps * 2);
        for ph in 0..=PHASES {
            for v in row(ph as f64 / PHASES as f64) {
                let v = (v / norm) as f32;
                bank.push(v);
                bank.push(v);
            }
        }
        Self {
            half,
            taps,
            bank,
            dot: kernels().dot_c32,
        }
    }

    /// The filter output at input position `pos` (≥ half − 1; the samples
    /// up to pos + half must be in `x`, interleaved re/im).
    #[inline]
    fn at(&self, x: &[f32], pos: f64) -> Complex32 {
        let iu = pos as usize;
        let ph = ((pos - iu as f64) * PHASES as f64 + 0.5) as usize;
        let base = iu + 1 - self.half;
        let row = 2 * self.taps;
        (self.dot)(
            &self.bank[ph * row..(ph + 1) * row],
            &x[2 * base..2 * (base + self.taps)],
        )
    }
}

/// The response at the slicer to one symbol, `tau` symbols from its
/// centre: the raised cosine of two symbols, times the cosine left by the
/// half-band offset of the vestigial sideband.
fn pulse(tau: f64) -> f64 {
    let x = tau / 2.0;
    let sinc = if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    };
    let d = 1.0 - (2.0 * ROLLOFF * x).powi(2);
    let rc = if d.abs() < 1e-9 {
        PI / 4.0 * sinc
    } else {
        sinc * (PI * ROLLOFF * x).cos() / d
    };
    rc * (PI * tau / 2.0).cos()
}

// ---- Equalizer -----------------------------------------------------------------

/// Feed-forward taps, of which this many look ahead of the symbol being
/// decided (pre-echoes up to 40 symbols, 3.7 µs).
pub(super) const FF: usize = 64;
pub(super) const FF_AHEAD: usize = 40;
/// Feedback taps (post-echoes up to 192 symbols, 17.8 µs).
pub(super) const FB: usize = 192;
/// History kept, enough to replay a field sync's known part.
const RING: usize = 2048;
/// No position in a field (not locked).
const NONE: u32 = u32::MAX;

/// A decision-feedback equalizer, LMS-adapted: trained on the field sync's
/// known symbols and the segment syncs, decision-directed elsewhere. Its
/// output lags its input by [`FF_AHEAD`] symbols.
struct Dfe {
    f: [f32; FF],
    b: [f32; FB],
    bias: f32,
    /// Inputs and decisions, each stored twice so any window is one slice.
    x: Box<[f32]>,
    d: Box<[f32]>,
    /// Each input's position in its field (or [`NONE`]).
    meta: Box<[u32]>,
    head: usize,
    filled: usize,
    /// An update not applied yet: (ring index, error × step).
    pending: Option<(usize, f32)>,
    dot: fn(&[f32], &[f32]) -> f32,
    axpy: fn(f32, &[f32], &mut [f32]),
}

impl Dfe {
    fn new() -> Self {
        let mut e = Self {
            f: [0.0; FF],
            b: [0.0; FB],
            bias: 0.0,
            x: vec![0.0; 2 * RING].into_boxed_slice(),
            d: vec![0.0; 2 * RING].into_boxed_slice(),
            meta: vec![NONE; RING].into_boxed_slice(),
            head: 0,
            filled: 0,
            pending: None,
            dot: kernels().dot_f32,
            axpy: kernels().axpy,
        };
        e.reset_taps();
        e
    }

    fn reset_taps(&mut self) {
        self.f = [0.0; FF];
        self.f[FF - 1 - FF_AHEAD] = 1.0;
        self.b = [0.0; FB];
        self.bias = 0.0;
        self.pending = None;
    }

    #[inline]
    fn push(&mut self, x: f32, fidx: u32) {
        let h = self.head;
        self.x[h] = x;
        self.x[h + RING] = x;
        self.meta[h] = fidx;
        self.head = if h + 1 == RING { 0 } else { h + 1 };
        self.filled += 1;
    }

    /// Give the latest `count` inputs field positions `first`, `first + 1`, …
    fn relabel(&mut self, count: usize, first: u32) {
        for i in 0..count.min(RING) {
            let m = (self.head + RING - count + i) % RING;
            self.meta[m] = first + i as u32;
        }
    }

    /// Ring index of the symbol to decide now, if enough has arrived.
    #[inline]
    fn current(&self) -> Option<usize> {
        (self.filled > FF_AHEAD).then(|| (self.head + RING - 1 - FF_AHEAD) % RING)
    }

    #[inline]
    fn windows(m: usize) -> (usize, usize) {
        (
            (m + RING - (FF - 1 - FF_AHEAD)) % RING,
            (m + RING - FB) % RING,
        )
    }

    #[inline]
    fn filter(&self, m: usize) -> f32 {
        let (xs, ds) = Self::windows(m);
        (self.dot)(&self.f, &self.x[xs..xs + FF])
            - (self.dot)(&self.b, &self.d[ds..ds + FB])
            - self.bias
    }

    /// Queue an update, applying the one queued before. The filter for
    /// the next symbol then doesn't wait on this symbol's update (a delayed
    /// LMS, by one symbol: the same convergence at these steps), so the
    /// two overlap instead of running back to back.
    #[inline]
    fn adapt_delayed(&mut self, m: usize, e: f32, mu: f32) {
        self.flush();
        self.pending = Some((m, mu * e));
    }

    #[inline]
    fn flush(&mut self) {
        if let Some((m, g)) = self.pending.take() {
            self.update(m, g);
        }
    }

    #[inline]
    fn adapt(&mut self, m: usize, e: f32, mu: f32) {
        self.update(m, mu * e);
    }

    #[inline]
    fn update(&mut self, m: usize, g: f32) {
        let (xs, ds) = Self::windows(m);
        (self.axpy)(-g, &self.x[xs..xs + FF], &mut self.f);
        (self.axpy)(g, &self.d[ds..ds + FB], &mut self.b);
        self.bias += g * 4.0;
    }

    #[inline]
    fn decide(&mut self, m: usize, v: f32) {
        self.d[m] = v;
        self.d[m + RING] = v;
    }

    fn healthy(&self) -> bool {
        self.f
            .iter()
            .chain(&self.b)
            .all(|v| v.is_finite() && v.abs() < 100.0)
    }
}

/// The nearest of the 8 levels. (Truncating a value clamped positive,
/// rather than `round`, which is a libm call without SSE4.1.)
#[inline]
fn slice(z: f32) -> f32 {
    ((z + 8.0) * 0.5).clamp(0.0, 7.5) as i32 as f32 * 2.0 - 7.0
}

/// The known level of field position `fidx`, if any.
#[inline]
fn reference(fidx: u32, second: bool) -> Option<f32> {
    if fidx == NONE {
        return None;
    }
    let p = fidx as usize % SEGMENT;
    if p < 4 {
        return Some(SEG_SYNC_LEVELS[p]);
    }
    if fidx as usize >= SEGMENT || p >= FIELD_SYNC_KNOWN {
        return None;
    }
    let v = field_sync_known()[p];
    let mid = PN63_AT[1]..PN63_AT[1] + 63;
    Some(if second && mid.contains(&p) { -v } else { v })
}

// ---- Receiver ------------------------------------------------------------------

/// Where the receiver is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Phase {
    /// Looking for the pilot in the spectrum.
    Coarse,
    /// Pilot found, carrier loop running: looking for segment syncs.
    Search,
    /// Segment sync found, timing loop running: looking for a field sync.
    Segment,
    /// Field sync found: equalizing and decoding.
    Field,
}

/// Matched filter outputs computed in one go, at most.
const AHEAD: usize = 64;

/// Symbols per carrier-loop and AGC update.
const REFRESH: u64 = 16;

/// Search range for the pilot around its nominal place.
const MAX_OFFSET: f64 = 250e3;
/// Pilot spectrum averages.
const COARSE_ROUNDS: usize = 4;
/// Segments without a segment sync before going back to the pilot search.
const SEARCH_SEGMENTS: u64 = 800;

pub(super) struct Receiver {
    nominal: f64,
    // Input stage.
    dc: Complex32,
    pub inverted: bool,
    nco: Rotator,
    /// The pilot's offset from its nominal place (after `inverted`).
    coarse_hz: f64,
    in_rate: f64,
    pub phase: Phase,
    // Pilot search.
    fft: Arc<dyn Fft<f32>>,
    fft_buf: Vec<Complex32>,
    fft_pow: Vec<f32>,
    fft_win: Vec<f32>,
    fft_scratch: Vec<Complex32>,
    fft_rounds: usize,
    tmp: Vec<Complex32>,
    // Matched filter and resampling.
    filt: SymbolFilter,
    buf: Vec<Complex32>,
    /// Matched filter outputs computed ahead (see `symbols`).
    ahead: Box<[Complex32; AHEAD]>,
    pos: f64,
    step: f64,
    n: u64,
    /// `n` mod [`SEGMENT`].
    n_seg: usize,
    /// 1 / the pilot's magnitude, and the AGC gain, refreshed every 16
    /// symbols (they change slowly).
    inv_pilot: f32,
    // Carrier loop: e^{-jθ}, ω (rad/symbol), the pilot low-passed.
    rot: Complex32,
    /// e^{-jδ}, the per-symbol turn of `rot`, refreshed with the loop.
    rot_step: Complex32,
    omega: f64,
    pilot: Complex32,
    pll: (f64, f64),
    // Level.
    dc_r: f32,
    power: f32,
    gain: f32,
    level_alpha: f32,
    search_start: u64,
    sign: f32,
    // Segment sync.
    hist: [f32; 4],
    acc: Vec<f32>,
    origin: usize,
    seg_pos: usize,
    segments: u64,
    slip: u32,
    weak: u32,
    // Timing.
    te: f32,
    kd: f32,
    timing_age: u32,
    // Field sync.
    segbuf: Vec<f32>,
    field_seg: usize,
    second: bool,
    missed: u32,
    stray: u32,
    pub fields: u64,
    pub pn63_errors: u64,
    // Equalizer and decoding.
    eq: Box<Dfe>,
    trained: u32,
    armed: bool,
    decoding: bool,
    pub tdec: Box<TrellisDecoder>,
    pub outer: OuterDecoder,
    mer: (f64, f64),
    /// Set when a lock begins or ends, for the status output.
    pub lock_changed: bool,
}

impl Receiver {
    /// Input chunks must not exceed `max_push` samples.
    pub fn new(in_rate: f64, max_push: usize) -> Self {
        let nominal = in_rate / SYMBOL_RATE;
        let filt = SymbolFilter::new(nominal);
        let fft_n = ((in_rate / 300.0) as usize)
            .next_power_of_two()
            .min(1 << 17);
        let fft = FftPlanner::new().plan_fft_forward(fft_n);
        let fft_win = (0..fft_n)
            .map(|i| (0.5 - 0.5 * (TAU * i as f64 / fft_n as f64).cos()) as f32)
            .collect();
        // Timing detector gain: d/dδ of p(δ + ½) − p(δ − ½) at 0.
        let h = 1e-4;
        let dd = |d: f64| pulse(d + 0.5) - pulse(d - 0.5);
        let kd = ((dd(h) - dd(-h)) / (2.0 * h)) as f32;
        let mut r = Self {
            nominal,
            dc: Complex32::default(),
            inverted: false,
            nco: Rotator::new(0.0),
            coarse_hz: 0.0,
            in_rate,
            phase: Phase::Coarse,
            fft_scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            fft,
            fft_buf: Vec::with_capacity(fft_n),
            fft_pow: vec![0.0; fft_n],
            fft_win,
            fft_rounds: 0,
            tmp: Vec::with_capacity(max_push),
            buf: Vec::with_capacity(max_push + 4 * filt.taps + 64),
            filt,
            ahead: Box::new([Complex32::default(); AHEAD]),
            pos: 0.0,
            step: nominal,
            n: 0,
            n_seg: 0,
            inv_pilot: 0.0,
            rot: Complex32::new(1.0, 0.0),
            rot_step: Complex32::new(1.0, 0.0),
            omega: 0.0,
            pilot: Complex32::default(),
            pll: (0.0, 0.0),
            dc_r: 0.0,
            power: DATA_POWER,
            gain: 1.0,
            level_alpha: 0.0,
            search_start: 0,
            sign: 1.0,
            hist: [0.0; 4],
            acc: vec![0.0; SEGMENT],
            origin: 0,
            seg_pos: 0,
            segments: 0,
            slip: 0,
            weak: 0,
            te: 0.0,
            kd,
            timing_age: 0,
            segbuf: vec![0.0; SEGMENT],
            field_seg: 0,
            second: false,
            missed: 0,
            stray: 0,
            fields: 0,
            pn63_errors: 0,
            eq: Box::new(Dfe::new()),
            trained: 0,
            armed: false,
            decoding: false,
            tdec: Box::new(TrellisDecoder::new()),
            outer: OuterDecoder::new(),
            mer: (0.0, 0.0),
            lock_changed: false,
        };
        r.restart();
        r
    }

    /// Back to the pilot search.
    fn restart(&mut self) {
        if self.phase == Phase::Field {
            self.lock_changed = true;
        }
        self.phase = Phase::Coarse;
        self.fft_buf.clear();
        self.fft_pow.fill(0.0);
        self.fft_rounds = 0;
        self.buf.clear();
        self.step = self.nominal;
        self.decoding = false;
        self.armed = false;
    }

    /// The carrier offset found, in Hz.
    pub fn offset_hz(&self) -> f64 {
        let f = self.coarse_hz + self.omega * SYMBOL_RATE / TAU;
        if self.inverted { -f } else { f }
    }

    /// The sample clock offset (positive: the input runs fast).
    pub fn clock(&self) -> f64 {
        self.step / self.nominal - 1.0
    }

    pub fn locked(&self) -> bool {
        self.phase == Phase::Field
    }

    /// MER over the field syncs since the last call, in dB.
    pub fn take_mer(&mut self) -> Option<f64> {
        let (e, n) = std::mem::take(&mut self.mer);
        (n > 0.0 && e > 0.0).then(|| 10.0 * (DATA_POWER as f64 * n / e).log10())
    }

    /// Whether the field being received is the second of its frame.
    pub fn second_field(&self) -> bool {
        self.second
    }

    pub fn process(&mut self, input: &[Complex32], fifo: &mut ByteFifo) {
        // DC removal (a direct-conversion receiver's offset), and the
        // spectrum flipped back if it is inverted.
        self.tmp.clear();
        for &v in input {
            // A NaN or infinity (some drivers emit one now and then) would
            // poison the DC estimate, and through it every loop, for good.
            let v = if v.re.is_finite() && v.im.is_finite() {
                v
            } else {
                Complex32::default()
            };
            let v = if self.inverted { v.conj() } else { v };
            self.dc += (v - self.dc) * 2e-5;
            self.tmp.push(v - self.dc);
        }
        if self.phase == Phase::Coarse {
            let tmp = std::mem::take(&mut self.tmp);
            self.coarse(&tmp);
            self.tmp = tmp;
            return;
        }
        let at = self.buf.len();
        self.buf.resize(at + self.tmp.len(), Complex32::default());
        self.nco.rotate(&self.tmp, &mut self.buf[at..]);
        self.symbols(fifo);
    }

    fn coarse(&mut self, input: &[Complex32]) {
        let n = self.fft_pow.len();
        for &v in input {
            self.fft_buf.push(v);
            if self.fft_buf.len() < n {
                continue;
            }
            for (x, &w) in self.fft_buf.iter_mut().zip(&self.fft_win) {
                *x *= w;
            }
            self.fft
                .process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
            for (p, x) in self.fft_pow.iter_mut().zip(&self.fft_buf) {
                *p += x.norm_sqr();
            }
            self.fft_buf.clear();
            self.fft_rounds += 1;
            if self.fft_rounds < COARSE_ROUNDS {
                continue;
            }
            // The pilot at −Rs/4, or at +Rs/4 if the spectrum is inverted.
            let lower = self.find_pilot(-SYMBOL_RATE / 4.0);
            let upper = self.find_pilot(SYMBOL_RATE / 4.0);
            self.fft_pow.fill(0.0);
            self.fft_rounds = 0;
            let (f, inv) = match (lower, upper) {
                (Some(l), Some(u)) if u.1 > l.1 => (u.0, true),
                (Some(l), _) => (l.0, false),
                (None, Some(u)) => (u.0, true),
                (None, None) => continue,
            };
            if inv != self.inverted {
                self.inverted = inv;
                // The DC estimate was of the other spectrum's samples.
                self.dc = self.dc.conj();
            }
            // Offset of the pilot, in the domain after the flip.
            self.coarse_hz = if inv {
                -(f - SYMBOL_RATE / 4.0)
            } else {
                f + SYMBOL_RATE / 4.0
            };
            self.nco.set_freq(-self.coarse_hz / self.in_rate);
            self.start_search();
            return;
        }
    }

    /// The strongest line within ±MAX_OFFSET of `nominal`, with its
    /// strength over the mean of the range, if it stands out.
    fn find_pilot(&self, nominal: f64) -> Option<(f64, f32)> {
        let n = self.fft_pow.len();
        let df = self.in_rate / n as f64;
        let bin = |f: f64| ((f / df).round() as isize).rem_euclid(n as isize) as usize;
        let span = (MAX_OFFSET / df).ceil() as isize;
        let centre = (nominal / df).round() as isize;
        let mut best = (0isize, 0f32);
        let mut sum = 0.0f64;
        for k in -span..=span {
            let p = self.fft_pow[bin((centre + k) as f64 * df)];
            sum += p as f64;
            if p > best.1 {
                best = (centre + k, p);
            }
        }
        let mean = (sum / (2 * span + 1) as f64) as f32;
        let ratio = best.1 / mean.max(1e-30);
        if ratio < 30.0 {
            return None;
        }
        // Parabolic interpolation on the log power.
        let p = |k: isize| (self.fft_pow[bin(k as f64 * df)].max(1e-30) as f64).ln();
        let (a, b, c) = (p(best.0 - 1), p(best.0), p(best.0 + 1));
        let den = a - 2.0 * b + c;
        let frac = if den.abs() > 1e-12 {
            (0.5 * (a - c) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        Some(((best.0 as f64 + frac) * df, ratio))
    }

    fn start_search(&mut self) {
        self.phase = Phase::Search;
        self.buf.clear();
        self.pos = self.filt.half as f64 + 1.0;
        self.step = self.nominal;
        self.rot = Complex32::new(1.0, 0.0);
        self.rot_step = Complex32::new(1.0, 0.0);
        self.omega = 0.0;
        self.pilot = Complex32::default();
        self.set_pll(1000.0);
        self.dc_r = 0.0;
        self.power = 0.0;
        self.level_alpha = 1.0 / 256.0;
        self.search_start = self.n;
        self.acc.fill(0.0);
        self.segments = 0;
        self.eq.reset_taps();
        self.trained = 0;
    }

    /// Carrier loop with a natural frequency of `bw` Hz, damping 0.707.
    fn set_pll(&mut self, bw: f64) {
        let wn = TAU * bw / SYMBOL_RATE;
        self.pll = (2.0 * 0.707 * wn, wn * wn);
    }

    /// How many symbols from the next one on need neither early/late
    /// samples nor can move the timing: the timing loop only acts at the
    /// segment sync's 4 symbols, and the segment sync is only found, moved
    /// or lost at the symbol where `segment_check` runs.
    fn plain_run(&self) -> usize {
        // Up to and including the symbol whose `k_seg` is 2.
        let check = (2 + SEGMENT - self.n_seg) % SEGMENT + 1;
        if self.phase < Phase::Segment {
            return check;
        }
        if self.seg_pos < 4 {
            return 0;
        }
        check.min(SEGMENT - self.seg_pos)
    }

    fn symbols(&mut self, fifo: &mut ByteFifo) {
        let half = self.filt.half as f64;
        loop {
            // The early sample needs the filter's history. Only a loop gone
            // non-finite or wild breaks this; start over rather than index
            // before the buffer.
            let early = self.pos - 0.5 * self.step;
            if !early.is_finite() || early < half - 1.0 {
                self.restart();
                return;
            }
            // A stretch where the sample positions are known in advance:
            // the matched filter runs over it in a tight loop of its own,
            // where its independent dot products overlap, instead of
            // interleaved with the long per-symbol chain below.
            let run = self.plain_run().min(AHEAD);
            if run >= 2 {
                let x: &[f32] = bytemuck::cast_slice(&self.buf);
                let (mut p, step) = (self.pos, self.step);
                let mut n = 0;
                while n < run && ((p + 0.5 * step + half) as usize + 1) < self.buf.len() {
                    self.ahead[n] = self.filt.at(x, p);
                    p += step;
                    n += 1;
                }
                if n == 0 {
                    break;
                }
                for i in 0..n {
                    self.symbol(self.ahead[i], None, fifo);
                    if self.phase == Phase::Coarse {
                        return;
                    }
                    self.pos += self.step;
                }
                continue;
            }
            let late = self.pos + 0.5 * self.step;
            if (late + half) as usize + 1 >= self.buf.len() {
                break;
            }
            let x: &[f32] = bytemuck::cast_slice(&self.buf);
            let y = self.filt.at(x, self.pos);
            let el = (self.phase >= Phase::Segment && self.seg_pos < 4).then(|| {
                (
                    self.filt.at(x, self.pos - 0.5 * self.step),
                    self.filt.at(x, late),
                )
            });
            self.symbol(y, el, fifo);
            if self.phase == Phase::Coarse {
                return;
            }
            self.pos += self.step;
        }
        // Drop the input no longer needed (keep a little history).
        let first = self.pos as usize + 1 - self.filt.half;
        let drop = first.saturating_sub(8).min(self.buf.len());
        if drop > 0 {
            self.buf.drain(..drop);
            self.pos -= drop as f64;
        }
    }

    /// Multiply by j^k.
    #[inline]
    fn turn(y: Complex32, k: u64) -> Complex32 {
        match k & 3 {
            0 => y,
            1 => Complex32::new(-y.im, y.re),
            2 => -y,
            _ => Complex32::new(y.im, -y.re),
        }
    }

    fn symbol(&mut self, y: Complex32, el: Option<(Complex32, Complex32)>, fifo: &mut ByteFifo) {
        let k = self.n;
        self.n += 1;
        let k_seg = self.n_seg;
        self.n_seg = if k_seg + 1 == SEGMENT { 0 } else { k_seg + 1 };
        let refresh = k & (REFRESH - 1) == 0;
        // The pilot to DC and the data into the real part.
        let v = Self::turn(y, k) * self.rot;
        self.pilot += (v - self.pilot) * (1.0 / 512.0);
        // The loop runs once per REFRESH symbols (the pilot is averaged over
        // 512, and the loop is 300–1000 Hz wide against 10.76 MBd): steered
        // every symbol, its chain (rotate, average, error, integrate, turn)
        // would be one long dependency from symbol to symbol, and the
        // symbol rate would wait on its latency.
        if refresh {
            // (`norm` would be a libm `hypot` call.)
            let mag = self.pilot.norm_sqr().sqrt();
            self.inv_pilot = if mag > 0.0 { 1.0 / mag } else { 0.0 };
            let err = (self.pilot.im * self.inv_pilot) as f64;
            self.omega += self.pll.1 * REFRESH as f64 * err;
            let d = (self.omega + self.pll.0 * err) as f32;
            self.rot_step = Complex32::new(1.0 - 0.5 * d * d, -d);
        }
        self.rot *= self.rot_step;
        if k & 255 == 0 {
            self.rot /= self.rot.norm_sqr().sqrt();
        }

        // Level: remove the pilot, scale the data to ±1 … ±7.
        let r = v.re * self.sign;
        let a = self.level_alpha;
        self.dc_r += (r - self.dc_r) * a;
        let c = r - self.dc_r;
        self.power += (c * c - self.power) * a;
        if k == self.search_start + 20_000 {
            self.level_alpha = 1.0 / 8192.0;
        }
        if refresh {
            self.gain = (DATA_POWER / self.power.max(1e-30)).sqrt();
        }
        let x = c * self.gain;

        // Timing, from the segment sync: Σ s·(late − early) at the 4 sync
        // symbols (the neighbours' contributions cancel by symmetry).
        if let Some((ye, yl)) = el {
            const R: f32 = std::f32::consts::FRAC_1_SQRT_2;
            let ve = Self::turn(ye, k) * Complex32::new(R, -R) * self.rot;
            let vl = Self::turn(yl, k) * Complex32::new(R, R) * self.rot;
            let s = SEG_SYNC_LEVELS[self.seg_pos];
            self.te += s * (vl.re - ve.re) * self.sign * self.gain;
            if self.seg_pos == 3 {
                // How late we sample, in symbols.
                let delta = (self.te / (100.0 * self.kd)).clamp(-0.5, 0.5) as f64;
                self.te = 0.0;
                // A second-order loop, critically damped, narrowing as it
                // settles: wide enough to pull in a clock off by 50 ppm
                // (0.04 symbols a segment) within a few dozen segments.
                self.timing_age += 1;
                let kp = match self.timing_age {
                    0..100 => 0.1,
                    100..400 => 0.05,
                    _ => 0.03,
                };
                let ki = kp * kp / 4.0 / SEGMENT as f64;
                self.pos -= kp * delta * self.nominal;
                self.step -= ki * delta * self.nominal;
                let lim = self.nominal * 2e-3;
                self.step = self.step.clamp(self.nominal - lim, self.nominal + lim);
            }
        }

        // Segment sync correlation, accumulated per position mod 832.
        self.hist = [self.hist[1], self.hist[2], self.hist[3], x];
        let corr = self.hist[0] - self.hist[1] - self.hist[2] + self.hist[3];
        let j0 = if k_seg >= 3 {
            k_seg - 3
        } else {
            k_seg + SEGMENT - 3
        };
        self.acc[j0] += (corr - self.acc[j0]) * (1.0 / 16.0);
        if j0 == SEGMENT - 1 {
            self.segment_check();
            if self.phase == Phase::Coarse {
                return;
            }
        }

        if self.phase < Phase::Segment {
            return;
        }
        let p = self.seg_pos;
        self.segbuf[p] = x;
        let fidx = if self.phase == Phase::Field {
            (self.field_seg * SEGMENT + p) as u32
        } else {
            NONE
        };
        self.eq.push(x, fidx);
        if p == PN63_AT[2] + 63 {
            self.field_check();
        }
        self.equalize(fifo);
        self.seg_pos += 1;
        if self.seg_pos == SEGMENT {
            self.seg_pos = 0;
            if self.phase == Phase::Field {
                self.field_seg += 1;
                if self.field_seg == FIELD_LEN {
                    self.field_seg = 0;
                    self.second = !self.second;
                }
            }
        }
    }

    /// Once per 832 symbols: find, keep or lose the segment sync.
    fn segment_check(&mut self) {
        self.segments += 1;
        let (mut a, mut max) = (0, f32::MIN);
        for (i, &v) in self.acc.iter().enumerate() {
            if v > max {
                (a, max) = (i, v);
            }
        }
        match self.phase {
            Phase::Search => {
                let second = self
                    .acc
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| {
                        let d = (i + SEGMENT - a) % SEGMENT;
                        d > 3 && d < SEGMENT - 3
                    })
                    .map(|(_, &v)| v)
                    .fold(f32::MIN, f32::max);
                if self.segments >= 24 && max > 8.0 && max > 2.5 * second {
                    self.lock_segment(a);
                } else if self.segments > SEARCH_SEGMENTS {
                    self.restart();
                }
            }
            Phase::Segment | Phase::Field => {
                let own = self.acc[self.origin];
                if a != self.origin && max > 1.3 * own {
                    self.slip += 1;
                } else {
                    self.slip = 0;
                }
                if own < 4.0 {
                    self.weak += 1;
                } else {
                    self.weak = 0;
                }
                if self.weak > 400 {
                    self.restart();
                } else if self.slip >= 8 {
                    self.lose_field();
                    self.lock_segment(a);
                }
            }
            Phase::Coarse => {}
        }
    }

    fn lock_segment(&mut self, origin: usize) {
        self.origin = origin;
        // The symbol being processed is n − 1.
        let k = (self.n_seg + SEGMENT - 1) % SEGMENT;
        self.seg_pos = (k + SEGMENT - origin) % SEGMENT;
        self.phase = Phase::Segment;
        self.timing_age = 0;
        self.set_pll(500.0);
        self.te = 0.0;
        self.slip = 0;
        self.weak = 0;
    }

    fn lose_field(&mut self) {
        if self.phase == Phase::Field {
            self.lock_changed = true;
            self.phase = Phase::Segment;
        }
        self.decoding = false;
        self.armed = false;
    }

    /// At the end of the third PN63 of every segment: is it a field sync?
    fn field_check(&mut self) {
        let seg = &self.segbuf;
        let pn = pn511();
        let mut c = 0.0f32;
        let mut e = 0.0f32;
        for (i, &b) in pn.iter().enumerate() {
            let v = seg[PN511_AT + i];
            c += if b != 0 { v } else { -v };
            e += v * v;
        }
        let rho = c / (e * 511.0).sqrt().max(1e-30);
        let pn63c = |at: usize| -> f32 {
            pn63()
                .iter()
                .enumerate()
                .map(|(i, &b)| if b != 0 { seg[at + i] } else { -seg[at + i] })
                .sum()
        };
        let second = (pn63c(PN63_AT[1]) > 0.0) != (pn63c(PN63_AT[0]) > 0.0);
        const THRESHOLD: f32 = 0.35;
        match self.phase {
            Phase::Segment => {
                if rho < -THRESHOLD {
                    // Upside down: flip, and lock on the next one.
                    self.sign = -self.sign;
                } else if rho > THRESHOLD && self.timing_age >= 64 {
                    self.lock_field(second);
                }
            }
            Phase::Field if self.field_seg == 0 => {
                if rho > THRESHOLD {
                    self.missed = 0;
                    self.fields += 1;
                    if second != self.second {
                        self.pn63_errors += 1;
                        self.second = second;
                    }
                } else {
                    self.missed += 1;
                    if self.missed >= 3 {
                        self.lose_field();
                    }
                }
            }
            Phase::Field => {
                // A field sync where none should be: twice in a row, follow it.
                if rho > 0.6 {
                    self.stray += 1;
                    if self.stray >= 2 {
                        self.lose_field();
                        self.lock_field(second);
                    }
                } else if rho < 0.2 {
                    self.stray = 0;
                }
            }
            _ => {}
        }
    }

    fn lock_field(&mut self, second: bool) {
        self.phase = Phase::Field;
        self.lock_changed = true;
        self.field_seg = 0;
        self.second = second;
        self.missed = 0;
        self.stray = 0;
        self.fields += 1;
        self.armed = true;
        self.decoding = false;
        self.set_pll(300.0);
        // The field sync so far is in the equalizer already, unlabelled.
        let p = self.seg_pos + 1;
        self.eq.relabel(p, 0);
    }

    fn equalize(&mut self, fifo: &mut ByteFifo) {
        let Some(m) = self.eq.current() else {
            return;
        };
        let fidx = self.eq.meta[m];
        if self.phase != Phase::Field || fidx == NONE {
            let x = self.eq.x[m];
            self.eq.decide(m, slice(x));
            return;
        }
        let z = self.eq.filter(m);
        let known = reference(fidx, self.second);
        let d = known.unwrap_or_else(|| slice(z));
        let e = z - d;
        let fast = self.trained < 4;
        let norm = 1.0 / ((FF + FB) as f32 * DATA_POWER);
        // Once trained, decision-directed adaptation runs on every other
        // symbol (at twice the step): half the cost, about the same
        // tracking.
        let mu = norm
            * match (known.is_some(), fast) {
                (true, true) => 0.2,
                (true, false) => 0.05,
                (false, true) => 0.04,
                (false, false) if fidx & 1 == 0 => 0.02,
                (false, false) => 0.0,
            };
        if (fidx as usize) < FIELD_SYNC_KNOWN && fidx >= 4 {
            self.mer.0 += (e * e) as f64;
            self.mer.1 += 1.0;
        }
        if mu > 0.0 {
            self.eq.adapt_delayed(m, e, mu);
        }
        self.eq.decide(m, d);
        if fidx as usize == FIELD_SYNC_KNOWN - 1 {
            self.train(m);
        }
        if (fidx as usize).is_multiple_of(SEGMENT) && !self.eq.healthy() {
            self.eq.reset_taps();
            self.trained = 0;
        }

        // Decoding starts with a field's first data symbol.
        let fi = fidx as usize;
        if self.armed && fi == SEGMENT + 4 {
            self.armed = false;
            self.decoding = true;
            self.tdec.reset();
            self.outer.reset();
        }
        if self.decoding && fi >= SEGMENT && fi % SEGMENT >= 4 && self.tdec.push(z) {
            self.outer.push(self.tdec.round(), |p| fifo.write(p));
        }
    }

    /// At the end of a field sync's known part: go over it again a few
    /// times while the equalizer is new.
    fn train(&mut self, end: usize) {
        let passes = match self.trained {
            0 => 32,
            1..4 => 8,
            _ => 0,
        };
        // Before the first training the symbols before the field sync were
        // decided without an equalizer: leave out the stretch whose feedback
        // taps would reach back to them.
        let first = if self.trained == 0 { FB } else { 0 };
        self.trained += 1;
        self.eq.flush();
        let mu = 0.15 / ((FF + FB) as f32 * DATA_POWER);
        // The feedback taps take the known symbols, not what was decided.
        for i in 0..FIELD_SYNC_KNOWN {
            let m = (end + RING - (FIELD_SYNC_KNOWN - 1 - i)) % RING;
            if let Some(want) = reference(i as u32, self.second) {
                self.eq.decide(m, want);
            }
        }
        for _ in 0..passes {
            for i in first..FIELD_SYNC_KNOWN {
                let m = (end + RING - (FIELD_SYNC_KNOWN - 1 - i)) % RING;
                let Some(want) = reference(i as u32, self.second) else {
                    continue;
                };
                let e = self.eq.filter(m) - want;
                self.eq.adapt(m, e, mu);
            }
        }
    }
}
