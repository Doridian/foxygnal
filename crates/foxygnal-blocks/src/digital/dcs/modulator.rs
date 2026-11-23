//! A test modulator for GOES DCS: CS2 transmissions as complex baseband
//! (carrier, clock, FSS, scrambled and trellis-coded 8PSK, root raised
//! cosine pulses with α = 1, the shape the DAMS-NT receivers match), and a
//! simulator of many platforms bursting on their channels, which the
//! `goes_dcs_sim` block and the tests use.

use std::f64::consts::{FRAC_PI_4, TAU};

use foxygnal_core::Complex32;

use super::frame::{
    Baud, CLOCK, EOT, FLUSH_BYTES, FSS, Format, SCRAMBLER, TrellisEncoder, bch_encode,
    binary_length_bytes, channel_freq, flag_for, is_1200_channel, odd_parity, to_uplink,
};
use crate::sources::Rng;

/// What one platform sends.
#[derive(Clone, Debug)]
pub struct Burst {
    /// 32-bit GOES ID (a BCH codeword, see [`bch_encode`]).
    pub addr: u32,
    pub baud: Baud,
    pub flag: u8,
    /// The message after the flag word, as sent (with parity bits for ASCII
    /// and pseudo-binary). An EOT is added for those formats; binary gets
    /// the length field and a CRC placeholder instead.
    pub data: Vec<u8>,
}

/// Phases (×45°) of every symbol of a transmission, carrier to flush.
pub fn symbols(b: &Burst, out: &mut Vec<u8>) {
    out.clear();
    let carrier = (b.baud.carrier_s() * b.baud.symbol_rate()).round() as usize;
    out.resize(carrier, 0);
    out.extend(CLOCK.iter().chain(&FSS).map(|&bit| 4 * bit));
    let mut enc = TrellisEncoder::default();
    let mut i = 0;
    let mut put = |v: u8, out: &mut Vec<u8>| {
        let s = v ^ SCRAMBLER[i % SCRAMBLER.len()];
        i += 1;
        for p in 0..4 {
            out.push(enc.push(s >> (2 * p)));
        }
    };
    for v in b.addr.to_be_bytes() {
        put(v, out);
    }
    put(b.flag, out);
    let flush = if Format::of(b.flag) == Format::Binary {
        for v in binary_length_bytes(b.flag, b.data.len()) {
            put(v, out);
        }
        for &v in &b.data {
            put(v, out);
        }
        // The draft's CRC-16 isn't checked by the receiver; send zeros.
        put(0, out);
        put(0, out);
        2
    } else {
        for &v in &b.data {
            put(v, out);
        }
        put(EOT, out);
        FLUSH_BYTES
    };
    for _ in 0..flush {
        put(0, out);
    }
}

/// Table points per symbol of the pulse.
const TABLE_RES: usize = 64;
/// Pulse half-length in symbols.
const SPAN: usize = 4;

/// Root raised cosine with α = 1, `t` in symbols: 4·cos(2πt) / (π(1 − 16t²)).
pub fn rrc1(t: f64) -> f64 {
    let d = 1.0 - 16.0 * t * t;
    if d.abs() < 1e-9 {
        // The limit at t = ±1/4.
        1.0
    } else {
        4.0 * (TAU * t).cos() / (std::f64::consts::PI * d)
    }
}

/// 1 except over the outermost symbol of a pulse truncated at ±`span`
/// symbols, where it falls to 0 along a half cosine.
pub fn edge_taper(t: f64, span: f64) -> f64 {
    let x = t.abs() - (span - 1.0);
    if x <= 0.0 {
        1.0
    } else if x >= 1.0 {
        0.0
    } else {
        0.5 + 0.5 * (std::f64::consts::PI * x).cos()
    }
}

/// Pulse samples on a fine grid over ±[`SPAN`] symbols, scaled so that a
/// run of equal symbols (the carrier) has amplitude 1.
fn pulse_table() -> Vec<f32> {
    let n = 2 * SPAN * TABLE_RES + 1;
    let raw: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / TABLE_RES as f64 - SPAN as f64;
            rrc1(t) * edge_taper(t, SPAN as f64)
        })
        .collect();
    let dc: f64 = (0..=2 * SPAN).map(|k| raw[k * TABLE_RES]).sum();
    raw.iter().map(|v| (v / dc) as f32).collect()
}

/// The pulse table, built once.
fn table() -> &'static [f32] {
    use std::sync::OnceLock;
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(pulse_table)
}

/// One transmission being played out at a sample rate, on a frequency.
pub struct Tx {
    syms: Vec<Complex32>,
    table: &'static [f32],
    /// Symbols per sample.
    step: f64,
    /// Time in symbols of the next sample (symbol k is centred at t = k).
    t: f64,
    /// Carrier phase (rad) and its step per sample.
    ph: f64,
    dph: f64,
    amp: f32,
}

impl Tx {
    /// `freq` relative to the stream's centre, `phase` the carrier's start
    /// phase (rad).
    pub fn new(phases: &[u8], symbol_rate: f64, fs: f64, freq: f64, amp: f32, phase: f64) -> Self {
        let mut tx = Self {
            syms: Vec::with_capacity(phases.len()),
            table: table(),
            step: 0.0,
            t: 0.0,
            ph: 0.0,
            dph: 0.0,
            amp: 0.0,
        };
        tx.start(phases, symbol_rate, fs, freq, amp, phase);
        tx
    }

    /// Reuse this transmitter for another transmission (no allocation once
    /// it has held one as long).
    pub fn start(
        &mut self,
        phases: &[u8],
        symbol_rate: f64,
        fs: f64,
        freq: f64,
        amp: f32,
        phase: f64,
    ) {
        self.syms.clear();
        self.syms.extend(phases.iter().map(|&p| {
            let a = f64::from(p) * FRAC_PI_4;
            Complex32::new(a.cos() as f32, a.sin() as f32)
        }));
        self.step = symbol_rate / fs;
        self.t = -(SPAN as f64);
        self.ph = phase;
        self.dph = TAU * freq / fs;
        self.amp = amp;
    }

    /// Samples still to come.
    pub fn remaining(&self) -> usize {
        let end = self.syms.len() as f64 + SPAN as f64;
        ((end - self.t) / self.step).ceil().max(0.0) as usize
    }

    pub fn done(&self) -> bool {
        self.t >= self.syms.len() as f64 + SPAN as f64
    }

    /// Add this transmission to `out` (until it ends).
    pub fn add_to(&mut self, out: &mut [Complex32]) {
        let n = self.syms.len() as i64;
        let res = TABLE_RES as f64;
        for o in out.iter_mut() {
            if self.done() {
                break;
            }
            let base = self.t.floor() as i64;
            let mut acc = Complex32::new(0.0, 0.0);
            for k in (base - SPAN as i64 + 1).max(0)..=(base + SPAN as i64).min(n - 1) {
                let x = (self.t - k as f64 + SPAN as f64) * res;
                let i = x.floor() as usize;
                if i + 1 >= self.table.len() {
                    continue;
                }
                let f = (x - i as f64) as f32;
                let p = self.table[i] + f * (self.table[i + 1] - self.table[i]);
                acc += self.syms[k as usize] * p;
            }
            let (s, c) = self.ph.sin_cos();
            *o += acc * Complex32::new(c as f32, s as f32) * self.amp;
            self.ph = (self.ph + self.dph) % TAU;
            self.t += self.step;
        }
    }
}

/// The whole transmission of `b` at rate `fs`, `freq` Hz from the centre.
pub fn modulate(b: &Burst, fs: f64, freq: f64, amp: f32, phase: f64) -> Vec<Complex32> {
    let mut ph = Vec::new();
    symbols(b, &mut ph);
    let mut tx = Tx::new(&ph, b.baud.symbol_rate(), fs, freq, amp, phase);
    let mut out = vec![Complex32::new(0.0, 0.0); tx.remaining()];
    tx.add_to(&mut out);
    out
}

// ---- Message contents ----------------------------------------------------------

/// ASCII text with odd parity on every character, as a platform sends it.
pub fn ascii(text: &str) -> Vec<u8> {
    text.bytes().map(odd_parity).collect()
}

/// A pseudo-binary message: the header word (format number), then each value
/// as `chars` 6-bit characters, most significant first (CS2 Appendix B).
pub fn pseudo_binary(format: u8, values: &[(u32, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    pseudo_binary_into(format, values, &mut out);
    out
}

fn pseudo_binary_into(format: u8, values: &[(u32, usize)], out: &mut Vec<u8>) {
    out.clear();
    out.push(odd_parity(0x40 | (format & 0x3F)));
    for &(v, chars) in values {
        for i in (0..chars).rev() {
            out.push(odd_parity(0x40 | ((v >> (6 * i)) & 0x3F) as u8));
        }
    }
}

// ---- Many platforms ------------------------------------------------------------

/// A simulated platform.
struct Platform {
    addr: u32,
    channel: u16,
    baud: Baud,
    /// Frequency relative to the stream centre (channel plus oscillator
    /// error).
    freq: f64,
    amp: f32,
    format: Format,
    count: u32,
    /// Sample at which it transmits next.
    next: u64,
}

/// Which data rates the simulated platforms use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mix {
    Both,
    Only(Baud),
}

/// Settings of [`Sim`].
#[derive(Clone, Debug)]
pub struct SimConfig {
    pub fs: f64,
    /// RF frequency of the stream's 0 Hz (downlink or uplink).
    pub center: f64,
    pub platforms: usize,
    /// Mean time between one platform's transmissions (s).
    pub interval: f64,
    pub mix: Mix,
    /// Carrier amplitude of the strongest platform; others are up to 10 dB
    /// weaker.
    pub amplitude: f32,
    /// Largest oscillator error (Hz).
    pub freq_error: f64,
    pub seed: u64,
}

fn unit(r: &mut Rng) -> f64 {
    (r.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// Many platforms transmitting on their own channels at random times, as
/// the DCS band looks from a dish.
pub struct Sim {
    cfg: SimConfig,
    rng: Rng,
    platforms: Vec<Platform>,
    active: Vec<Tx>,
    idle: Vec<Tx>,
    sample: u64,
    phases: Vec<u8>,
    burst: Burst,
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Self {
        let mut rng = Rng::new(if cfg.seed == 0 { 1 } else { cfg.seed });
        let half = 0.45 * cfg.fs;
        let center_up = to_uplink(cfg.center);
        // Channels inside the band the stream covers.
        let mut pool: Vec<u16> = (1..=266)
            .chain(301..=566)
            .filter(|&ch| (channel_freq(ch).unwrap() - center_up).abs() < half - 2_000.0)
            .collect();
        let mut platforms = Vec::new();
        for i in 0..cfg.platforms {
            let want_1200 = match cfg.mix {
                Mix::Both => i % 3 == 2,
                Mix::Only(b) => b == Baud::B1200,
            };
            let candidates: Vec<usize> = pool
                .iter()
                .enumerate()
                .filter(|&(_, &ch)| !want_1200 || is_1200_channel(ch))
                .map(|(j, _)| j)
                .collect();
            if candidates.is_empty() {
                break;
            }
            let j = candidates[(rng.next_u64() % candidates.len() as u64) as usize];
            let channel = pool.swap_remove(j);
            // Keep the neighbours of a 1200 bit/s platform free, as the
            // channel plan does.
            let f = channel_freq(channel).unwrap();
            let guard = if want_1200 { 1_600.0 } else { 100.0 };
            pool.retain(|&c| (channel_freq(c).unwrap() - f).abs() > guard);
            let err = (2.0 * unit(&mut rng) - 1.0) * cfg.freq_error;
            let db = -10.0 * unit(&mut rng);
            let info = (rng.next_u64() & 0x1F_FFFF) as u32;
            let format = if i % 2 == 0 {
                Format::Ascii
            } else {
                Format::PseudoBinary
            };
            let first = (unit(&mut rng) * cfg.interval.min(5.0) * cfg.fs) as u64;
            platforms.push(Platform {
                addr: bch_encode(info),
                channel,
                baud: if want_1200 { Baud::B1200 } else { Baud::B300 },
                freq: f - center_up + err,
                amp: cfg.amplitude * 10f32.powf(db as f32 / 20.0),
                format,
                count: 0,
                next: first,
            });
        }
        let n = platforms.len();
        let idle = (0..n)
            .map(|_| Tx::new(&[0; 1024], 150.0, cfg.fs, 0.0, 0.0, 0.0))
            .collect();
        Self {
            rng,
            platforms,
            active: Vec::with_capacity(n),
            idle,
            sample: 0,
            phases: Vec::with_capacity(8192),
            burst: Burst {
                addr: 0,
                baud: Baud::B300,
                flag: 0,
                data: Vec::with_capacity(256),
            },
            cfg,
        }
    }

    /// (address, channel, baud) of every platform.
    pub fn platforms(&self) -> Vec<(u32, u16, Baud)> {
        self.platforms
            .iter()
            .map(|p| (p.addr, p.channel, p.baud))
            .collect()
    }

    /// The message platform `p` sends for its `n`th transmission, into `out`.
    fn message(p: &Platform, n: u32, out: &mut Vec<u8>) {
        use std::io::Write;
        match p.format {
            Format::PseudoBinary => pseudo_binary_into(
                1,
                &[(n & 0xFFF, 2), (2048 + n * 7 % 900, 2), (131_000 + n, 3)],
                out,
            ),
            _ => {
                out.clear();
                let _ = write!(
                    out,
                    "{:03} STAGE {:.2} BATT {:.1} CH{:03}",
                    n % 1000,
                    3.0 + f64::from(n % 40) * 0.01,
                    12.5 + f64::from(n % 9) * 0.1,
                    p.channel
                );
                for b in out.iter_mut() {
                    *b = odd_parity(*b);
                }
            }
        }
    }

    /// The `n`th message (from 0) of the platform with address `addr`.
    pub fn expected(&self, addr: u32, n: u32) -> Option<Vec<u8>> {
        let p = self.platforms.iter().find(|p| p.addr == addr)?;
        let mut v = Vec::new();
        Self::message(p, n, &mut v);
        Some(v)
    }

    /// Fill `out` with the next samples of the band (no noise).
    pub fn fill(&mut self, out: &mut [Complex32]) {
        out.fill(Complex32::new(0.0, 0.0));
        for tx in &mut self.active {
            tx.add_to(out);
        }
        let mut i = 0;
        while i < self.active.len() {
            if self.active[i].done() {
                let tx = self.active.swap_remove(i);
                self.idle.push(tx);
            } else {
                i += 1;
            }
        }
        let end = self.sample + out.len() as u64;
        for i in 0..self.platforms.len() {
            while self.platforms[i].next < end {
                let Some(mut tx) = self.idle.pop() else {
                    break;
                };
                let p = &self.platforms[i];
                let off = p.next.saturating_sub(self.sample) as usize;
                Self::message(p, p.count, &mut self.burst.data);
                self.burst.addr = p.addr;
                self.burst.baud = p.baud;
                self.burst.flag = flag_for(p.format);
                symbols(&self.burst, &mut self.phases);
                let phase = TAU * unit(&mut self.rng);
                tx.start(
                    &self.phases,
                    p.baud.symbol_rate(),
                    self.cfg.fs,
                    p.freq,
                    p.amp,
                    phase,
                );
                let len = tx.remaining() as u64;
                tx.add_to(&mut out[off..]);
                if tx.done() {
                    self.idle.push(tx);
                } else {
                    self.active.push(tx);
                }
                let gap = (self.cfg.interval * (0.5 + unit(&mut self.rng)) * self.cfg.fs) as u64;
                let p = &mut self.platforms[i];
                p.count += 1;
                p.next += len + gap;
            }
        }
        self.sample = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_has_unit_amplitude_and_clock_follows() {
        let b = Burst {
            addr: bch_encode(1),
            baud: Baud::B300,
            flag: flag_for(Format::Ascii),
            data: ascii("HI"),
        };
        let mut ph = Vec::new();
        symbols(&b, &mut ph);
        // 75 carrier, 3 clock, 15 FSS, (4 + 1 + 2 + 1 + 4) bytes × 4.
        assert_eq!(ph.len(), 75 + 18 + 12 * 4);
        assert_eq!(&ph[75..78], &[4, 0, 4]);
        let x = modulate(&b, 4800.0, 0.0, 1.0, 0.0);
        // Middle of the carrier: 32 samples per symbol, 4 symbols of lead-in.
        let mid = x[32 * 40];
        assert!((mid.norm() - 1.0).abs() < 1e-3, "{mid}");
        assert!(mid.im.abs() < 1e-3);
    }

    #[test]
    fn sim_places_platforms_on_their_channels() {
        let mut sim = Sim::new(SimConfig {
            fs: 100_000.0,
            center: 1_679_900_000.0,
            platforms: 6,
            interval: 1.0,
            mix: Mix::Both,
            amplitude: 0.1,
            freq_error: 50.0,
            seed: 3,
        });
        let p = sim.platforms();
        assert_eq!(p.len(), 6);
        assert_eq!(p.iter().filter(|x| x.2 == Baud::B1200).count(), 2);
        let mut buf = vec![Complex32::new(0.0, 0.0); 100_000];
        sim.fill(&mut buf);
        let e: f32 = buf.iter().map(|c| c.norm_sqr()).sum();
        assert!(e > 0.0);
    }
}
