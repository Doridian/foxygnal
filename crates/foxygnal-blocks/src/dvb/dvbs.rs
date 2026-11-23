//! The DVB-S modem (EN 300 421): `dvbs_modulator` (TS → QPSK baseband) and
//! `dvbs_demodulator` (baseband → TS, plus a status text output).
//!
//! ## Receiver
//!
//! Per chunk of input samples (≤ 2048):
//!
//! 1. **Frequency correction**: an NCO ([`Rotator`]) removes the carrier
//!    offset estimated so far.
//! 2. **Matched filter**: RRC, roll-off 0.35, 10 symbols long, at the input
//!    rate.
//! 3. **Symbol timing**: Gardner detector, cubic interpolator, second-order
//!    loop (any samples per symbol ≥ 2, not necessarily whole).
//! 4. **AGC** on the symbols, then a second-order QPSK **Costas loop**.
//! 5. **Coarse frequency search** (only while the TS isn't locked): the FFT
//!    of 4096 symbols raised to the 4th power (which strips the QPSK
//!    modulation) has a line at 4× the residual offset. A clear line moves the
//!    NCO, which covers ±symbol rate / 8.
//! 6. **Soft bits** (`i8`) to the FEC.
//!
//! The FEC first **acquires**: it collects 14 000 soft symbols and tries every
//! hypothesis: code rate (or only the configured one), puncturing phase, and
//! 4 symmetries of the constellation (90° rotation and I/Q swap from a mirrored
//! spectrum; the other 4 are these inverted, which the TS sync notices). Each
//! is Viterbi-decoded over 2048 symbols and ranked by its re-encoded bit error
//! rate; the best few are decoded over the whole buffer and accepted when the
//! sync bytes (0x47 / 0xB8) show up 204 bytes apart in at least 6 of 8
//! packets. Then it **tracks**: the buffer is replayed and every new symbol
//! goes through depuncturing, Viterbi, the packet sync, the deinterleaver, RS
//! and energy dispersal removal into the TS output. Losing the packet grid for
//! 40 packets sends it back to acquiring.

use std::f32::consts::{FRAC_1_SQRT_2, PI, TAU};
use std::sync::Arc;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Resampler, Rotator, taps};
use rustfft::{Fft, FftPlanner};

use super::conv::{CodeRate, ConvEncoder, Depuncturer, Puncture};
use super::outer::{OuterDecoder, OuterEncoder, PACKET_BITS, TsSync, is_sync};
use super::viterbi::Viterbi;
use super::{ByteFifo, RS_LEN, SYNC, TS_LEN};
use crate::design::{self, fmt_rate};
use crate::digital::{MsgQueue, loop_gains, text_meta};
use crate::filters::not_configured;
use crate::params;

/// DVB-S roll-off.
const ROLLOFF: f64 = 0.35;
const RATE_NAMES: [&str; 5] = ["1/2", "2/3", "3/4", "5/6", "7/8"];

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

fn symbol_rate_param() -> foxygnal_core::ParamSpec {
    design::float_min(
        "symbol_rate",
        "Symbol rate",
        333_000.0,
        1.0,
        Some("Bd"),
        "Symbols per second. Amateur DATV uses 33 k–2 MBd (QO-100: 333 k, 250 k, 125 k \
         and lower), satellite TV 22–30 MBd.",
    )
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "dvbs_modulator",
            label: "DVB-S Modulator",
            category: "Digital",
            help: "DVB-S (EN 300 421) transmitter: an MPEG transport stream (whole 188-byte \
                   packets starting with 0x47) in, QPSK baseband out at samples/symbol × the \
                   symbol rate. Energy dispersal, RS(204,188), interleaving, convolutional \
                   code (punctured to the code rate), Gray-mapped QPSK and RRC pulses \
                   (roll-off 0.35). The TS must arrive at the channel's useful bit rate \
                   (symbol rate × 2 × code rate × 188/204 bits/s), e.g. from a throttled \
                   file. Bytes before a sync byte are skipped.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: vec![
                symbol_rate_param(),
                design::choice(
                    "rate",
                    "Code rate",
                    RATE_NAMES.to_vec(),
                    "3/4",
                    "Convolutional code rate: 1/2 is the most robust, 7/8 carries the \
                     most data.",
                ),
                design::int(
                    "sps",
                    "Samples/symbol",
                    4,
                    2,
                    "Output samples per symbol; the output rate is this × the symbol rate.",
                ),
                params::float(
                    "gain",
                    "Level",
                    0.5,
                    None,
                    "RMS amplitude of the output (peaks are about 1.6× this).",
                ),
            ],
        },
        build_modulator,
    );
    r.set_keywords(
        "dvbs_modulator",
        &[
            "datv",
            "dvb-s",
            "digital tv",
            "qpsk",
            "transport stream",
            "mpeg-ts",
        ],
    );
    r.register(
        BlockDescriptor {
            kind: "dvbs_demodulator",
            label: "DVB-S Demodulator",
            category: "Digital",
            help: "DVB-S (EN 300 421) receiver: complex baseband at a known symbol rate in \
                   (at least 2 samples per symbol, centred within a few kHz, up to ±symbol \
                   rate / 8), MPEG transport stream out on `ts`: whole 188-byte packets, \
                   those RS couldn't correct with the transport_error_indicator set. Finds \
                   the timing, carrier, code rate, puncturing phase and phase ambiguity by \
                   itself. `status` has one text line per second and one per lock change: \
                   `t=… state=lock rate=3/4 mer_db=… ber=… rs_corrected=… rs_failed=… pps=… \
                   freq_hz=…`.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![port("ts", StreamType::U8), port("status", StreamType::U8)],
            params: vec![
                symbol_rate_param(),
                design::choice(
                    "rate",
                    "Code rate",
                    vec!["auto", "1/2", "2/3", "3/4", "5/6", "7/8"],
                    "auto",
                    "auto tries all five (acquiring takes longer).",
                ),
            ],
        },
        build_demodulator,
    );
    r.set_keywords(
        "dvbs_demodulator",
        &[
            "datv",
            "dvb-s",
            "digital tv",
            "qpsk",
            "transport stream",
            "mpeg-ts",
            "qo-100",
            "viterbi",
            "reed-solomon",
        ],
    );
}

fn read_symbol_rate(p: &ParamReader<'_>) -> Result<f64, Error> {
    let rs = p.f64("symbol_rate")?;
    if !(rs > 0.0 && rs.is_finite()) {
        return Err(Error::Other(format!(
            "the symbol rate must be above 0 (got {rs})"
        )));
    }
    Ok(rs)
}

/// TS bytes per second carried at `symbol_rate` with `rate`.
pub fn ts_byte_rate(symbol_rate: f64, rate: CodeRate) -> f64 {
    symbol_rate * 2.0 * rate.value() * (TS_LEN as f64 / RS_LEN as f64) / 8.0
}

// ---- Modulator -----------------------------------------------------------------

pub struct DvbsModulator {
    symbol_rate: f64,
    sps: usize,
    outer: OuterEncoder,
    conv: ConvEncoder,
    filter: Resampler<Complex32>,
    /// Symbols of the latest packet, not yet filtered.
    syms: Vec<Complex32>,
    sym_pos: usize,
    /// An I bit waiting for its Q bit.
    half: Option<u8>,
    packet: [u8; TS_LEN],
    coded: [u8; RS_LEN],
}

/// QPSK point for bits (I, Q): 0 → +, 1 → − on each axis.
#[inline]
fn qpsk(i: u8, q: u8) -> Complex32 {
    let a = |b: u8| {
        if b == 0 {
            FRAC_1_SQRT_2
        } else {
            -FRAC_1_SQRT_2
        }
    };
    Complex32::new(a(i), a(q))
}

impl DvbsModulator {
    fn new(symbol_rate: f64, rate: CodeRate, sps: usize, gain: f64) -> Self {
        let mut h = taps::rrc(sps as f64, ROLLOFF, 16, 1.0);
        // Unit-power symbols with one non-zero sample per `sps`: output power
        // is Σh²/sps, so scale for `gain²`.
        let energy: f64 = h.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        let k = gain * (sps as f64 / energy).sqrt();
        h.iter_mut().for_each(|v| *v = (f64::from(*v) * k) as f32);
        Self {
            symbol_rate,
            sps,
            outer: OuterEncoder::new(),
            conv: ConvEncoder::new(rate),
            filter: Resampler::new(sps, 1, &h),
            syms: Vec::with_capacity(RS_LEN * 8 + 2),
            sym_pos: 0,
            half: None,
            packet: [0; TS_LEN],
            coded: [0; RS_LEN],
        }
    }

    /// Code the packet in `self.packet` into symbols.
    fn encode_packet(&mut self) {
        self.outer.encode(&self.packet, &mut self.coded);
        self.syms.clear();
        self.sym_pos = 0;
        let (syms, half) = (&mut self.syms, &mut self.half);
        for &byte in &self.coded {
            self.conv.push_byte(byte, |bit| match half.take() {
                None => *half = Some(bit),
                Some(i) => syms.push(qpsk(i, bit)),
            });
        }
    }
}

impl Block for DvbsModulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(self.symbol_rate * self.sps as f64);
        }
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        loop {
            if self.sym_pos < self.syms.len() {
                let out = &mut io.outputs[0].slice_mut::<Complex32>()[made..];
                if out.is_empty() {
                    break;
                }
                let (took, n) = self.filter.process(&self.syms[self.sym_pos..], out);
                self.sym_pos += took;
                made += n;
                if self.sym_pos < self.syms.len() {
                    break;
                }
                continue;
            }
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.len() < TS_LEN {
                break;
            }
            if inp[0] != SYNC {
                // Not on the packet grid: skip to the next sync byte.
                used += inp.iter().position(|&b| b == SYNC).unwrap_or(inp.len());
                continue;
            }
            self.packet.copy_from_slice(&inp[..TS_LEN]);
            used += TS_LEN;
            self.encode_packet();
        }
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn read_rate(p: &ParamReader<'_>) -> Result<Option<CodeRate>, Error> {
    let s = p.str("rate")?;
    if s == "auto" {
        return Ok(None);
    }
    CodeRate::parse(s)
        .map(Some)
        .ok_or_else(|| Error::Other(format!("unknown code rate `{s}`")))
}

fn build_modulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let rate = read_rate(p)?.ok_or_else(|| Error::Other("pick a code rate".into()))?;
    let sps = p.u64("sps")?;
    if !(2..=64).contains(&sps) {
        return Err(Error::Other(format!(
            "samples per symbol must be 2 to 64 (got {sps})"
        )));
    }
    Ok(Instance {
        block: Box::new(DvbsModulator::new(
            read_symbol_rate(p)?,
            rate,
            sps as usize,
            p.f64("gain")?,
        )),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}

// ---- Demodulator: front end -----------------------------------------------------

/// Input samples per processing step.
const CHUNK: usize = 2048;
/// Soft value of an ideal QPSK point (±1/√2 after the AGC).
const SOFT_SCALE: f32 = 40.0;
/// Symbols per coarse frequency estimate.
const COARSE_N: usize = 4096;
/// The line must stand this far above the average bin (power ratio).
const COARSE_SNR: f32 = 25.0;
/// Residual offsets smaller than this (rad/symbol) are left to the Costas loop.
const COARSE_MIN: f64 = 0.002;
/// Timing and carrier loop bandwidths (per symbol) while acquiring, and
/// once locked.
const TIMING_BW: (f64, f64) = (0.008, 0.002);
const CARRIER_BW: (f64, f64) = (0.015, 0.004);
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
fn soft(v: f32) -> i8 {
    (v * SOFT_SCALE).round().clamp(-127.0, 127.0) as i8
}

struct FrontEnd {
    rate: f64,
    sps: f64,
    /// Carrier offset estimate, cycles/sample (the NCO runs at minus this).
    offset: f64,
    nco: Rotator,
    filter: Resampler<Complex32>,
    mixed: Vec<Complex32>,
    filtered: Vec<Complex32>,
    // Timing recovery (Gardner).
    hist: [Complex32; 4],
    next: f64,
    mid_next: bool,
    integ: f64,
    kp: f64,
    ki: f64,
    prev: Complex32,
    mid: Complex32,
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
    fft_fill: usize,
    /// Symbol count from which the symbols can be trusted.
    settle_at: u64,
    jumps: u64,
    // Statistics since the last report.
    symbols: u64,
    abs_sum: f64,
    pow_sum: f64,
    stat_n: u64,
}

impl FrontEnd {
    fn new(rate: f64, symbol_rate: f64) -> Self {
        let sps = rate / symbol_rate;
        let h = taps::rrc(sps, ROLLOFF, 10, 1.0);
        let (kp, ki) = loop_gains(TIMING_BW.0);
        let (alpha, beta) = loop_gains(CARRIER_BW.0);
        let fft = FftPlanner::new().plan_fft_forward(COARSE_N);
        let window = (0..COARSE_N)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / COARSE_N as f32).cos())
            .collect();
        Self {
            rate,
            sps,
            offset: 0.0,
            nco: Rotator::new(0.0),
            filter: Resampler::fir(&h),
            mixed: vec![Complex32::default(); CHUNK],
            filtered: vec![Complex32::default(); CHUNK],
            hist: [Complex32::default(); 4],
            next: 0.0,
            mid_next: false,
            integ: 0.0,
            kp,
            ki,
            prev: Complex32::default(),
            mid: Complex32::default(),
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
            fft_fill: 0,
            settle_at: (COARSE_N as u64) + SETTLE,
            jumps: 0,
            symbols: 0,
            abs_sum: 0.0,
            pow_sum: 0.0,
            stat_n: 0,
        }
    }

    /// Narrow the loops once the TS is locked (less jitter, fewer cycle
    /// slips at low SNR), widen them again to acquire.
    fn set_locked(&mut self, locked: bool) {
        if locked == !self.coarse_on {
            return;
        }
        self.coarse_on = !locked;
        let i = usize::from(locked);
        let (kp, ki) = loop_gains([TIMING_BW.0, TIMING_BW.1][i]);
        let (alpha, beta) = loop_gains([CARRIER_BW.0, CARRIER_BW.1][i]);
        (self.kp, self.ki) = (kp, ki);
        (self.alpha, self.beta) = (alpha as f32, beta as f32);
    }

    fn settled(&self) -> bool {
        self.symbols >= self.settle_at
    }

    /// Carrier offset estimate in Hz.
    fn offset_hz(&self) -> f64 {
        self.offset * self.rate
            + f64::from(self.freq) * self.rate / self.sps / std::f64::consts::TAU
    }

    /// MER in dB over the symbols since the last call (and reset).
    fn take_mer(&mut self) -> Option<f64> {
        if self.stat_n == 0 {
            return None;
        }
        let n = self.stat_n as f64;
        let amp = self.abs_sum / (2.0 * n);
        let sig = 2.0 * amp * amp;
        let err = (self.pow_sum / n - sig).max(1e-12);
        self.abs_sum = 0.0;
        self.pow_sum = 0.0;
        self.stat_n = 0;
        Some(10.0 * (sig / err).log10())
    }

    /// Process up to [`CHUNK`] samples; soft symbols are appended to `soft`.
    fn process(&mut self, input: &[Complex32], soft_out: &mut Vec<[i8; 2]>) {
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
                    self.mid = y;
                    self.mid_next = false;
                    self.next += step / 2.0;
                    continue;
                }
                // Gardner: ≈ −2τ on average (τ: how late, in symbols).
                let p = dot(y, y);
                self.ted_power = if self.ted_power == 0.0 {
                    p
                } else {
                    self.ted_power + 0.01 * (p - self.ted_power)
                };
                let e = f64::from(dot(self.prev - y, self.mid) / self.ted_power.max(1e-20) / 2.0)
                    .clamp(-1.0, 1.0);
                self.integ = (self.integ + self.ki * e).clamp(-0.01, 0.01);
                let adjust = (self.sps * self.kp * e).clamp(-self.sps / 4.0, self.sps / 4.0);
                self.prev = y;
                self.mid_next = true;
                self.next += step / 2.0 + adjust;
                let s = self.symbol(y);
                soft_out.push(s);
            }
            self.next -= 1.0;
        }
    }

    /// One symbol from the timing loop: AGC, coarse search, Costas, soft bits.
    #[inline]
    fn symbol(&mut self, y: Complex32) -> [i8; 2] {
        self.symbols += 1;
        let p = dot(y, y);
        self.power = if self.power == 0.0 {
            p.max(1e-20)
        } else {
            self.power + 0.002 * (p - self.power)
        };
        let y = y * self.power.max(1e-20).sqrt().recip();
        if self.coarse_on {
            let y2 = y * y;
            self.fft_buf[self.fft_fill] = y2 * y2 * self.window[self.fft_fill];
            self.fft_fill += 1;
            if self.fft_fill == COARSE_N {
                self.fft_fill = 0;
                self.coarse_estimate();
            }
        }
        let (s, c) = self.phase.sin_cos();
        let z = y * Complex32::new(c, -s);
        // QPSK decision-directed phase error, ≈ sin(error) near lock.
        let sign = |v: f32| if v >= 0.0 { 1.0 } else { -1.0 };
        let e = (sign(z.re) * z.im - sign(z.im) * z.re) * FRAC_1_SQRT_2;
        self.freq = (self.freq + self.beta * e).clamp(-0.5, 0.5);
        self.phase += self.freq + self.alpha * e;
        if self.phase > PI {
            self.phase -= TAU;
        } else if self.phase < -PI {
            self.phase += TAU;
        }
        self.abs_sum += f64::from(z.re.abs() + z.im.abs());
        self.pow_sum += f64::from(dot(z, z));
        self.stat_n += 1;
        [soft(z.re), soft(z.im)]
    }

    /// The 4th-power spectrum: move the NCO if it has a clear line away from 0.
    fn coarse_estimate(&mut self) {
        self.fft
            .process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
        let n = COARSE_N;
        let mut best = 0;
        let mut best_p = 0.0f32;
        let mut total = 0.0f32;
        for (k, v) in self.fft_buf.iter().enumerate() {
            let p = v.norm_sqr();
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
        let mag = |k: usize| self.fft_buf[k % n].norm();
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
        // Line at 4× the residual: rad/symbol.
        let resid = std::f64::consts::TAU * k / (4.0 * n as f64);
        if resid.abs() < COARSE_MIN {
            return;
        }
        self.offset += resid / (std::f64::consts::TAU * self.sps);
        self.nco.set_freq(-self.offset);
        self.freq = 0.0;
        self.settle_at = self.symbols + SETTLE;
        self.jumps += 1;
    }
}

// ---- Demodulator: FEC -----------------------------------------------------------

/// Soft symbols collected for an acquisition attempt.
const ACQ_LEN: usize = 14_000;
/// Symbols each hypothesis is ranked on.
const PRESCREEN: usize = 2048;
/// Hypotheses decoded in full per attempt (best ranked first).
const FULL_CHECKS: usize = 6;
/// Packets checked for sync bytes, and how many must have them.
const SYNC_PERIODS: usize = 8;
const SYNC_NEEDED: usize = 6;
/// Decoded bits without a packet grid before acquiring again.
const LOST_BITS: usize = 40 * PACKET_BITS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hyp {
    rate: CodeRate,
    /// Puncturing phase (sent bits).
    phase: usize,
    /// Bit 0: rotate by −90°. Bit 1: mirror (conjugate) first.
    sym: u8,
}

#[inline]
fn transform(s: [i8; 2], sym: u8) -> [i8; 2] {
    let [i, q] = s;
    let q = if sym & 2 != 0 { -q } else { q };
    if sym & 1 != 0 { [q, -i] } else { [i, q] }
}

/// TS FIFO size, and the room kept free before taking more input (one chunk
/// yields far less: at most 2048 symbols × 1.75 bits, plus packet bursts).
const FIFO_LEN: usize = 1 << 16;
const FIFO_MARGIN: usize = 1 << 14;

struct Fec {
    rates: Vec<CodeRate>,
    tracking: bool,
    hyp: Hyp,
    acq: Vec<[i8; 2]>,
    search_vit: Viterbi,
    search_bits: Vec<u8>,
    windows: Vec<bool>,
    pairs: Vec<[i8; 2]>,
    dp: Depuncturer,
    vit: Viterbi,
    sync: TsSync,
    outer: OuterDecoder,
    unlocked_bits: usize,
    acquisitions: u64,
    /// Hypotheses with their prescreen BER (preallocated).
    cands: Vec<(f64, Hyp)>,
}

impl Fec {
    fn new(rate: Option<CodeRate>) -> Self {
        let rates = rate.map_or(CodeRate::ALL.to_vec(), |r| vec![r]);
        let hyp = Hyp {
            rate: rates[0],
            phase: 0,
            sym: 0,
        };
        Self {
            rates,
            tracking: false,
            hyp,
            acq: Vec::with_capacity(ACQ_LEN),
            search_vit: Viterbi::new(),
            search_bits: Vec::with_capacity(2 * ACQ_LEN),
            windows: Vec::with_capacity(2 * ACQ_LEN),
            pairs: Vec::with_capacity(2 * CHUNK),
            dp: Depuncturer::new(hyp.rate, 0),
            vit: Viterbi::new(),
            sync: TsSync::new(),
            outer: OuterDecoder::new(),
            unlocked_bits: 0,
            acquisitions: 0,
            cands: Vec::with_capacity(64),
        }
    }

    fn locked(&self) -> bool {
        self.tracking && self.sync.locked()
    }

    /// Back to acquiring.
    fn lose(&mut self) {
        self.tracking = false;
        self.acq.clear();
        self.sync.reset();
    }

    fn process(&mut self, soft: &[[i8; 2]], fifo: &mut ByteFifo) {
        if self.tracking {
            self.track(soft, fifo);
            return;
        }
        let room = ACQ_LEN - self.acq.len();
        self.acq.extend_from_slice(&soft[..soft.len().min(room)]);
        if self.acq.len() < ACQ_LEN {
            return;
        }
        self.acquisitions += 1;
        let Some(h) = self.search() else {
            self.acq.clear();
            return;
        };
        self.hyp = h;
        self.dp = Depuncturer::new(h.rate, h.phase);
        self.vit.reset();
        self.sync.reset();
        self.outer.reset();
        self.unlocked_bits = 0;
        self.tracking = true;
        // Replay what was collected, then carry on with the live symbols.
        let acq = std::mem::take(&mut self.acq);
        for c in acq.chunks(CHUNK) {
            self.track(c, fifo);
        }
        self.acq = acq;
        self.acq.clear();
        if self.tracking {
            self.track(&soft[soft.len().min(room)..], fifo);
        }
    }

    fn track(&mut self, soft: &[[i8; 2]], fifo: &mut ByteFifo) {
        if !self.tracking || soft.is_empty() {
            return;
        }
        self.pairs.clear();
        for &s in soft {
            let [a, b] = transform(s, self.hyp.sym);
            if let Some(p) = self.dp.push(a) {
                self.pairs.push(p);
            }
            if let Some(p) = self.dp.push(b) {
                self.pairs.push(p);
            }
        }
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
        // Losing a grid that was there usually means a 90° carrier slip:
        // acquire again right away.
        if self.unlocked_bits > LOST_BITS || self.sync.losses != losses {
            self.lose();
        }
    }

    /// Try every hypothesis on the collected symbols.
    fn search(&mut self) -> Option<Hyp> {
        let mut cands = std::mem::take(&mut self.cands);
        cands.clear();
        for ri in 0..self.rates.len() {
            let rate = self.rates[ri];
            for phase in Puncture::new(rate).phases(2) {
                for sym in 0..4 {
                    let h = Hyp { rate, phase, sym };
                    let ber = self.decode_with(h, PRESCREEN, false);
                    cands.push((ber, h));
                }
            }
        }
        cands.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut found = None;
        for &(ber, h) in cands.iter().take(FULL_CHECKS) {
            if ber > 0.2 {
                break;
            }
            self.decode_with(h, ACQ_LEN, true);
            if self.has_sync() {
                found = Some(h);
                break;
            }
        }
        self.cands = cands;
        found
    }

    /// Decode the first `n` collected symbols under `h`; returns the
    /// re-encoded bit error rate. Keeps the bits if `keep`.
    fn decode_with(&mut self, h: Hyp, n: usize, keep: bool) -> f64 {
        let mut dp = Depuncturer::new(h.rate, h.phase);
        self.pairs.clear();
        self.search_bits.clear();
        self.search_vit.reset();
        self.search_vit.reset_counters();
        let Self {
            acq,
            pairs,
            search_vit,
            search_bits,
            ..
        } = self;
        for c in acq[..n.min(acq.len())].chunks(CHUNK) {
            pairs.clear();
            for &s in c {
                let [a, b] = transform(s, h.sym);
                if let Some(p) = dp.push(a) {
                    pairs.push(p);
                }
                if let Some(p) = dp.push(b) {
                    pairs.push(p);
                }
            }
            search_vit.decode(pairs, |bits| {
                if keep {
                    search_bits.extend_from_slice(bits);
                }
            });
        }
        search_vit.flush(|bits| {
            if keep {
                search_bits.extend_from_slice(bits);
            }
        });
        if search_vit.compared == 0 {
            return 1.0;
        }
        search_vit.errors as f64 / search_vit.compared as f64
    }

    /// Sync bytes 204 bytes apart in [`SYNC_NEEDED`] of [`SYNC_PERIODS`]
    /// packets of `search_bits`, at any bit offset.
    fn has_sync(&mut self) -> bool {
        let bits = &self.search_bits;
        if bits.len() < SYNC_PERIODS * PACKET_BITS + 8 {
            return false;
        }
        self.windows.clear();
        let mut w = 0u8;
        for &b in bits.iter() {
            w = (w << 1) | b;
            self.windows.push(is_sync(w));
        }
        // windows[i]: the byte ending at bit i is a sync byte.
        (7..7 + PACKET_BITS).any(|o| {
            (0..SYNC_PERIODS)
                .filter(|&k| self.windows[o + k * PACKET_BITS])
                .count()
                >= SYNC_NEEDED
        })
    }
}

// ---- Demodulator block ----------------------------------------------------------

struct State {
    fe: FrontEnd,
    fec: Fec,
    fifo: ByteFifo,
    soft: Vec<[i8; 2]>,
    /// Input samples processed.
    samples: u64,
    /// Sample count of the next status line.
    next_report: u64,
    last_locked: bool,
    // Counters at the last report.
    last_packets: u64,
    last_samples: u64,
    last_vit: (u64, u64),
}

pub struct DvbsDemodulator {
    symbol_rate: f64,
    rate: Option<CodeRate>,
    state: Option<Box<State>>,
    msgs: MsgQueue,
}

/// Status lines are dropped rather than queued beyond this (a stalled text
/// consumer must not stall the video).
const MSG_BACKLOG: usize = 2048;

impl DvbsDemodulator {
    fn new(symbol_rate: f64, rate: Option<CodeRate>) -> Self {
        Self {
            symbol_rate,
            rate,
            state: None,
            msgs: MsgQueue::new(),
        }
    }

    fn process(st: &mut State, msgs: &mut MsgQueue, input: &[Complex32]) {
        st.soft.clear();
        let jumps = st.fe.jumps;
        st.fe.set_locked(st.fec.locked());
        st.fe.process(input, &mut st.soft);
        st.samples += input.len() as u64;
        if st.fe.jumps != jumps && !st.fec.tracking {
            // The frequency moved: what was collected is stale.
            st.fec.acq.clear();
        }
        if st.fec.tracking || st.fe.settled() {
            st.fec.process(&st.soft, &mut st.fifo);
        }
        let locked = st.fec.locked();
        let t = st.samples as f64 / st.fe.rate;
        if locked != st.last_locked {
            st.last_locked = locked;
            if msgs.pending() < MSG_BACKLOG {
                if locked {
                    let h = st.fec.hyp;
                    msgs.line(format_args!(
                        "t={t:.3} event=lock rate={} spectrum={} freq_hz={:+.0}",
                        h.rate.name(),
                        if h.sym & 2 != 0 { "inverted" } else { "normal" },
                        st.fe.offset_hz()
                    ));
                } else {
                    msgs.line(format_args!("t={t:.3} event=unlock"));
                }
            }
        }
        if st.samples >= st.next_report {
            st.next_report += st.fe.rate.round().max(1.0) as u64;
            Self::report(st, msgs, t);
        }
    }

    fn report(st: &mut State, msgs: &mut MsgQueue, t: f64) {
        let mer = st.fe.take_mer();
        let vit = (st.fec.vit.errors, st.fec.vit.compared);
        let (de, dc) = (vit.0 - st.last_vit.0, vit.1 - st.last_vit.1);
        st.last_vit = vit;
        let outer = &st.fec.outer;
        let secs = (st.samples - st.last_samples) as f64 / st.fe.rate;
        let pps = (outer.packets - st.last_packets) as f64 / secs.max(1e-9);
        st.last_packets = outer.packets;
        st.last_samples = st.samples;
        if msgs.pending() >= MSG_BACKLOG {
            return;
        }
        let state = if st.fec.locked() {
            "lock"
        } else if st.fec.tracking {
            "sync"
        } else {
            "search"
        };
        msgs.text(format_args!("t={t:.3} state={state}"));
        if st.fec.tracking {
            msgs.text(format_args!(" rate={}", st.fec.hyp.rate.name()));
        }
        if let Some(m) = mer {
            msgs.text(format_args!(" mer_db={m:.1}"));
        }
        if dc > 0 {
            msgs.text(format_args!(" ber={:.2e}", de as f64 / dc as f64));
        }
        msgs.line(format_args!(
            " rs_corrected={} rs_failed={} packets={} pps={pps:.0} freq_hz={:+.0}",
            outer.corrected,
            outer.failed,
            outer.packets,
            st.fe.offset_hz()
        ));
    }
}

impl Block for DvbsDemodulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let sps = rate / self.symbol_rate;
        if sps < 2.0 {
            return Err(Error::Other(format!(
                "the input rate {} is less than 2 samples per symbol at {} Bd; \
                 the demodulator needs at least 2",
                fmt_rate(rate),
                self.symbol_rate
            )));
        }
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = self.rate.map(|r| ts_byte_rate(self.symbol_rate, r));
            o.center_freq = None;
        }
        text_meta(cx, 1);
        self.state = Some(Box::new(State {
            fe: FrontEnd::new(rate, self.symbol_rate),
            fec: Fec::new(self.rate),
            fifo: ByteFifo::new(FIFO_LEN),
            soft: Vec::with_capacity(CHUNK + 8),
            samples: 0,
            next_report: rate.round().max(1.0) as u64,
            last_locked: false,
            last_packets: 0,
            last_samples: 0,
            last_vit: (0, 0),
        }));
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let st = self.state.as_mut().ok_or_else(not_configured)?;
        let (mut used, mut ts_made, mut msg_made) = (0, 0, 0);
        loop {
            ts_made += st
                .fifo
                .read_into(&mut io.outputs[0].slice_mut::<u8>()[ts_made..]);
            msg_made += self
                .msgs
                .flush(&mut io.outputs[1].slice_mut::<u8>()[msg_made..]);
            let inp = io.inputs[0].slice::<Complex32>();
            if used == inp.len() || st.fifo.free() < FIFO_MARGIN {
                break;
            }
            let n = (inp.len() - used).min(CHUNK);
            Self::process(st, &mut self.msgs, &inp[used..used + n]);
            used += n;
        }
        if used == 0 && ts_made == 0 && msg_made == 0 {
            let waiting = !st.fifo.is_empty() || !self.msgs.is_empty();
            if waiting && io.inputs[0].exhausted() && !io.outputs[0].abandoned() {
                return Ok(WorkStatus::Wait(std::time::Duration::from_millis(20)));
            }
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, ts_made);
        io.produce(1, msg_made);
        Ok(WorkStatus::Ok)
    }
}

fn build_demodulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(DvbsDemodulator::new(read_symbol_rate(p)?, read_rate(p)?)),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![
            ("ts".into(), StreamType::U8),
            ("status".into(), StreamType::U8),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_cover_the_eight_symmetries() {
        let s = [3i8, 5];
        let mut all: Vec<[i8; 2]> = (0..4).map(|k| transform(s, k)).collect();
        all.extend((0..4).map(|k| {
            let [a, b] = transform(s, k);
            [-a, -b]
        }));
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 8);
    }

    #[test]
    fn byte_rate() {
        // 333 kBd at 3/4: 499.5 kbit/s into RS → 460.3 kbit/s of TS.
        let r = ts_byte_rate(333_000.0, CodeRate::R3_4) * 8.0;
        assert!((r - 460_323.53).abs() < 1.0, "{r}");
    }
}
