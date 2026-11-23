//! Pulse detection: complex baseband in, trains of (pulse, gap) widths out.
//!
//! The OOK detector works on the envelope (the square root of the power
//! averaged over a few microseconds). The noise floor is the quietest of the
//! envelope's 1 ms averages over the last 128 ms (minimum statistics: it
//! follows a rising floor within that time even while pulses keep coming,
//! and a falling one at once); a pulse starts when the envelope rises
//! `level` dB above it, and ends when it falls below a threshold halfway (in
//! amplitude) between the pulse's tracked height and the floor, with some
//! hysteresis. Pulses and gaps shorter than a minimum are glitches and merge
//! into their neighbours. A packet ends after a gap of `max_gap`.
//!
//! The FSK detector runs while a carrier is on (an OOK "pulse"): an FM
//! discriminator (the angle of the averaged `x[n]·conj(x[n-1])`), minimum
//! and maximum trackers, and a slicer at their midpoint. Time spent on the
//! upper frequency becomes a pulse, on the lower one a gap (as in rtl_433:
//! mark = higher frequency). A burst ends when the carrier has been off for
//! a little while, and counts if it has enough changes and the frequency
//! keeps close to the two tones (noise on a discriminator spreads evenly).

use foxygnal_core::Complex32;

/// Most pulses kept per train (longer trains are cut short).
pub const MAX_PULSES: usize = 2048;

/// One train of pulses and gaps, widths in samples.
pub struct Pulses {
    pub pulse: Vec<u32>,
    pub gap: Vec<u32>,
    pub n: usize,
    /// Sample rate.
    pub rate: f64,
    /// First sample of the train (input sample index).
    pub start: u64,
    pub fsk: bool,
    /// Envelope of the pulses and of the noise floor (amplitude, full scale 1).
    pub high: f32,
    pub noise: f32,
    /// Mean frequency offset of the carrier (Hz), and for FSK the two
    /// tones' estimates.
    pub offset: f32,
    pub f1: f32,
    pub f2: f32,
}

impl Pulses {
    pub fn new() -> Self {
        Self {
            pulse: vec![0; MAX_PULSES],
            gap: vec![0; MAX_PULSES],
            n: 0,
            rate: 1.0,
            start: 0,
            fsk: false,
            high: 0.0,
            noise: 0.0,
            offset: 0.0,
            f1: 0.0,
            f2: 0.0,
        }
    }

    /// Microseconds per sample.
    pub fn us(&self) -> f64 {
        1e6 / self.rate
    }

    /// Build a train from widths in microseconds (tests and tools).
    pub fn from_us(rate: f64, widths: &[(f64, f64)], fsk: bool) -> Self {
        let mut p = Self::new();
        p.rate = rate;
        p.fsk = fsk;
        for &(pw, gw) in widths.iter().take(MAX_PULSES) {
            p.pulse[p.n] = (pw * rate / 1e6).round() as u32;
            p.gap[p.n] = (gw * rate / 1e6).round() as u32;
            p.n += 1;
        }
        p
    }
}

impl Default for Pulses {
    fn default() -> Self {
        Self::new()
    }
}

/// What a sample completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    None,
    /// An OOK packet is ready in [`Detector::ook`].
    Ook,
    /// An FSK burst is ready in [`Detector::fsk`].
    Fsk,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Learning the noise floor.
    Warmup,
    Idle,
    Pulse,
    Gap,
}

/// 1 ms blocks the noise floor is the minimum of.
const NOISE_BLOCKS: usize = 128;

/// Largest mean distance of the frequency from the nearer tone (relative to
/// their spacing) for an FSK burst to count.
const FSK_MAX_DEV: f32 = 0.18;

/// Fewest FSK symbols changes for a burst to count.
const FSK_MIN_PULSES: usize = 16;

struct Fsk {
    active: bool,
    /// Samples into the burst (the first ones are skipped).
    age: u32,
    hi: f32,
    lo: f32,
    started: bool,
    /// Current level (true = upper frequency), and its run length.
    cur: bool,
    run: u32,
    /// Samples the other level has lasted, while too short to count.
    pend: u32,
    /// The current pulse (upper-frequency run) is stored and its gap not yet.
    have_pulse: bool,
    /// Carrier-off samples so far.
    off: u32,
    /// Sum of the distances (relative to the span) from the nearer tone,
    /// and their count.
    dev: f32,
    ndev: u32,
}

pub struct Detector {
    rate: f64,
    /// Boxcar averages of power and of the phase-difference product.
    ring_p: Vec<f32>,
    ring_z: Vec<Complex32>,
    pos: usize,
    sum_p: f64,
    sum_zr: f64,
    sum_zi: f64,
    prev: Complex32,
    scale: f32,

    state: State,
    run: u32,
    warm: u32,
    noise: f32,
    /// 1 ms envelope averages: the one being summed, and the last ones.
    blk_sum: f32,
    blk_n: u32,
    blk_len: u32,
    blocks: [f32; NOISE_BLOCKS],
    blk_pos: usize,
    blk_full: bool,
    hi: f32,
    hi_decay: f32,
    hi_n: u32,
    k_on: f32,
    k_off: f32,
    min_len: u32,
    /// Shortest first pulse of a packet.
    first_min: u32,
    /// Look for FSK while a carrier is on.
    fsk_on: bool,
    max_gap: u32,
    /// Sum of the averaged phase products over pulses, for the offset.
    off_r: f64,
    off_i: f64,

    // FSK
    fsk_state: Fsk,
    fsk_attack: f32,
    fsk_decay: f32,
    fsk_min_span: f32,
    fsk_end: u32,

    pub ook: Pulses,
    pub fsk: Pulses,
    /// Input samples seen.
    now: u64,
}

impl Detector {
    /// The detector for every device: envelope averaged over ~6 µs (enough
    /// for 30 µs ERT chips), glitches up to 12 µs, FSK on.
    /// `level_db`: how far above the noise floor a pulse must rise;
    /// `max_gap_us`: the gap that ends a packet.
    pub fn new(rate: f64, level_db: f32, max_gap_us: f64) -> Self {
        Self::with_timing(rate, level_db, max_gap_us, 6.0, 12.0, 24.0, true)
    }

    /// A detector for OOK devices whose symbols are 150 µs or longer: the
    /// envelope averaged over 30 µs and glitches up to 50 µs, which keeps
    /// weak pulses whole, and its smoother noise lets the level sit 3 dB
    /// lower for the same false-alarm rate. No FSK.
    pub fn slow(rate: f64, level_db: f32, max_gap_us: f64) -> Self {
        Self::with_timing(rate, level_db - 3.0, max_gap_us, 30.0, 50.0, 100.0, false)
    }

    fn with_timing(
        rate: f64,
        level_db: f32,
        max_gap_us: f64,
        avg_us: f64,
        glitch_us: f64,
        first_us: f64,
        fsk_on: bool,
    ) -> Self {
        let len = ((rate * avg_us * 1e-6).round() as usize).max(3);
        let per = |us: f64| (rate * us * 1e-6).max(1.0);
        let k_on = 10f32.powf(level_db / 20.0);
        let mut ook = Pulses::new();
        ook.rate = rate;
        let mut fsk = Pulses::new();
        fsk.rate = rate;
        fsk.fsk = true;
        Self {
            rate,
            ring_p: vec![0.0; len],
            ring_z: vec![Complex32::new(0.0, 0.0); len],
            pos: 0,
            sum_p: 0.0,
            sum_zr: 0.0,
            sum_zi: 0.0,
            prev: Complex32::new(0.0, 0.0),
            scale: 1.0 / len as f32,
            state: State::Warmup,
            run: 0,
            warm: per(2000.0) as u32,
            noise: 0.0,
            blk_sum: 0.0,
            blk_n: 0,
            blk_len: per(1000.0) as u32,
            blocks: [0.0; NOISE_BLOCKS],
            blk_pos: 0,
            blk_full: false,
            hi: 0.0,
            hi_decay: (1.0 / per(2000.0)) as f32,
            hi_n: 0,
            k_on,
            k_off: k_on.sqrt(),
            // Glitches: up to ~12 µs (ERT chips, the shortest symbols, are 30).
            min_len: (per(glitch_us) as u32).max(2),
            first_min: (per(first_us) as u32).max(3),
            fsk_on,
            max_gap: per(max_gap_us) as u32,
            off_r: 0.0,
            off_i: 0.0,
            fsk_state: Fsk {
                active: false,
                age: 0,
                hi: 0.0,
                lo: 0.0,
                started: false,
                cur: false,
                run: 0,
                pend: 0,
                have_pulse: false,
                off: 0,
                dev: 0.0,
                ndev: 0,
            },
            fsk_attack: (1.0 / len as f64).min(0.5) as f32,
            fsk_decay: (1.0 / per(3000.0)) as f32,
            // 2·3 kHz peak to peak at least.
            fsk_min_span: (std::f64::consts::TAU * 6e3 / rate) as f32,
            fsk_end: per(100.0).max(f64::from((len as u32 / 2).max(2) * 2)) as u32,
            ook,
            fsk,
            now: 0,
        }
    }

    pub fn noise(&self) -> f32 {
        self.noise
    }

    /// Feed one sample.
    #[inline]
    pub fn sample(&mut self, x: Complex32) -> Event {
        self.now += 1;
        let p = x.norm_sqr();
        let z = x * self.prev.conj();
        self.prev = x;
        let old_p = std::mem::replace(&mut self.ring_p[self.pos], p);
        let old_z = std::mem::replace(&mut self.ring_z[self.pos], z);
        self.pos += 1;
        if self.pos == self.ring_p.len() {
            self.pos = 0;
        }
        self.sum_p += f64::from(p) - f64::from(old_p);
        self.sum_zr += f64::from(z.re) - f64::from(old_z.re);
        self.sum_zi += f64::from(z.im) - f64::from(old_z.im);
        let a = (self.sum_p.max(0.0) as f32 * self.scale).sqrt();
        self.floor(a);

        let mut ev = Event::None;
        let carrier = match self.state {
            State::Warmup => {
                self.run += 1;
                if self.run >= self.warm {
                    self.state = State::Idle;
                    self.run = 0;
                }
                false
            }
            State::Idle => {
                if a > self.noise * self.k_on {
                    self.start_packet(a);
                    true
                } else {
                    false
                }
            }
            State::Pulse => {
                // The pulses' mean height: a running mean over the packet's
                // pulses, then an average over the last couple of ms.
                self.hi_n += 1;
                let k = (1.0 / self.hi_n as f32).max(self.hi_decay);
                self.hi += (a - self.hi) * k;
                let (_, off) = self.thresholds();
                if a < off {
                    self.end_pulse();
                    self.state != State::Pulse
                } else {
                    self.run += 1;
                    true
                }
            }
            State::Gap => {
                let (on, _) = self.thresholds();
                if a > on {
                    if self.end_gap() {
                        // Full: end the packet here; the pulse starts the next.
                        self.finish_packet();
                        ev = Event::Ook;
                        false
                    } else {
                        true
                    }
                } else {
                    self.run += 1;
                    if self.run > self.max_gap {
                        self.finish_packet();
                        ev = Event::Ook;
                    }
                    false
                }
            }
        };
        if carrier {
            self.off_r += self.sum_zr;
            self.off_i += self.sum_zi;
        }
        if self.fsk_state.active && self.fsk_sample(carrier) {
            // An FSK burst is only reported when no OOK packet ended on
            // this very sample (it can't: OOK packets end in long gaps,
            // FSK bursts after short ones).
            ev = Event::Fsk;
        } else if carrier && !self.fsk_state.active && self.fsk_on {
            self.fsk_start();
        }
        ev
    }

    /// Update the noise floor with one envelope sample.
    #[inline]
    fn floor(&mut self, a: f32) {
        self.blk_sum += a;
        self.blk_n += 1;
        if self.blk_n < self.blk_len {
            if !self.blk_full && self.blk_pos == 0 {
                // Until the first block is done: the mean so far.
                self.noise = (self.blk_sum / self.blk_n as f32).max(1e-7);
            }
            return;
        }
        let mean = self.blk_sum / self.blk_n as f32;
        self.blk_sum = 0.0;
        self.blk_n = 0;
        self.blocks[self.blk_pos] = mean;
        self.blk_pos += 1;
        if self.blk_pos == NOISE_BLOCKS {
            self.blk_pos = 0;
            self.blk_full = true;
        }
        let used = if self.blk_full {
            NOISE_BLOCKS
        } else {
            self.blk_pos
        };
        let min = self.blocks[..used].iter().copied().fold(f32::MAX, f32::min);
        self.noise = min.max(1e-7);
    }

    /// On and off thresholds within a packet: around the midpoint between
    /// the pulses' height and the floor, but never too close to the floor.
    fn thresholds(&self) -> (f32, f32) {
        let mid = 0.5 * (self.hi + self.noise);
        (
            (self.noise * self.k_on).max(mid * 1.1),
            (self.noise * self.k_off * 0.85).max(mid * 0.8),
        )
    }

    fn start_packet(&mut self, a: f32) {
        self.state = State::Pulse;
        self.run = 1;
        self.hi = a;
        self.hi_n = 1;
        self.ook.n = 0;
        self.ook.start = self.now;
        self.off_r = 0.0;
        self.off_i = 0.0;
    }

    fn end_pulse(&mut self) {
        let n = self.ook.n;
        if n == 0 && self.run < self.first_min {
            // Noise, most likely: no device starts with so short a pulse.
            self.state = State::Idle;
            return;
        }
        if self.run < self.min_len {
            // A glitch: drop it.
            {
                self.ook.n -= 1;
                self.run += self.ook.gap[n - 1];
                self.state = State::Gap;
            }
            return;
        }
        self.ook.pulse[n] = self.run;
        self.state = State::Gap;
        self.run = 1;
    }

    /// The gap ends; true if the train is full and must end instead.
    fn end_gap(&mut self) -> bool {
        let n = self.ook.n;
        if self.run < self.min_len {
            // A dip within a pulse.
            self.run += self.ook.pulse[n] + 1;
            self.state = State::Pulse;
            return false;
        }
        if n + 1 >= MAX_PULSES {
            return true;
        }
        self.ook.gap[n] = self.run;
        self.ook.n += 1;
        self.state = State::Pulse;
        self.run = 1;
        false
    }

    fn finish_packet(&mut self) {
        let n = self.ook.n;
        self.ook.gap[n] = self.run;
        self.ook.n = n + 1;
        self.ook.high = self.hi;
        self.ook.noise = self.noise;
        self.ook.offset = (self.off_i.atan2(self.off_r) * self.rate / std::f64::consts::TAU) as f32;
        self.state = State::Idle;
        self.run = 0;
    }

    fn fsk_start(&mut self) {
        let f = &mut self.fsk_state;
        f.active = true;
        f.age = 0;
        f.started = false;
        f.cur = false;
        f.run = 0;
        f.pend = 0;
        f.have_pulse = false;
        f.off = 0;
        f.dev = 0.0;
        f.ndev = 0;
        self.fsk.n = 0;
        self.fsk.start = self.now;
    }

    /// One sample of an FSK burst; true when the burst is complete and long
    /// enough.
    fn fsk_sample(&mut self, carrier: bool) -> bool {
        let len = self.ring_p.len() as u32;
        let f = &mut self.fsk_state;
        f.age += 1;
        if carrier {
            f.off = 0;
        } else {
            f.off += 1;
            if f.off > self.fsk_end {
                return self.fsk_finish();
            }
        }
        if f.age <= len || !carrier {
            // The average still holds samples from before the carrier, or
            // the carrier dipped: the current level goes on.
            if f.age <= len {
                self.fsk.start = self.now;
            } else {
                f.run += 1;
            }
            return false;
        }
        let fr = (self.sum_zi as f32).atan2(self.sum_zr as f32);
        if !f.started {
            f.started = true;
            f.hi = fr;
            f.lo = fr;
        }
        let k = if fr > f.hi {
            self.fsk_attack
        } else {
            self.fsk_decay
        };
        f.hi += (fr - f.hi) * k;
        let k = if fr < f.lo {
            self.fsk_attack
        } else {
            self.fsk_decay
        };
        f.lo += (fr - f.lo) * k;
        let span = f.hi - f.lo;
        let d = if span < self.fsk_min_span {
            f.cur
        } else {
            let mid = 0.5 * (f.hi + f.lo);
            let h = span / 6.0;
            f.dev += (fr - f.hi).abs().min((fr - f.lo).abs()) / span;
            f.ndev += 1;
            if fr > mid + h {
                true
            } else if fr < mid - h {
                false
            } else {
                f.cur
            }
        };
        if d == f.cur {
            f.run += f.pend + 1;
            f.pend = 0;
        } else {
            f.pend += 1;
            if f.pend >= self.min_len {
                let run = f.run;
                let pend = f.pend;
                Self::fsk_commit(f, &mut self.fsk, run);
                f.cur = d;
                f.run = pend;
                f.pend = 0;
            }
        }
        false
    }

    /// Store a finished run of the current level.
    fn fsk_commit(f: &mut Fsk, out: &mut Pulses, run: u32) {
        if out.n >= MAX_PULSES - 1 {
            return;
        }
        if f.cur {
            out.pulse[out.n] = run;
            f.have_pulse = true;
        } else {
            if !f.have_pulse {
                out.pulse[out.n] = 0;
            }
            out.gap[out.n] = run;
            out.n += 1;
            f.have_pulse = false;
        }
    }

    fn fsk_finish(&mut self) -> bool {
        let f = &mut self.fsk_state;
        f.active = false;
        let run = f.run + f.pend;
        if f.cur {
            Self::fsk_commit(f, &mut self.fsk, run);
            self.fsk.gap[self.fsk.n] = self.max_gap;
            self.fsk.n += 1;
        } else {
            Self::fsk_commit(f, &mut self.fsk, run + self.max_gap);
        }
        self.fsk.high = self.hi;
        self.fsk.noise = self.noise;
        let hz = |w: f32| (f64::from(w) * self.rate / std::f64::consts::TAU) as f32;
        self.fsk.f1 = hz(f.hi);
        self.fsk.f2 = hz(f.lo);
        self.fsk.offset = 0.5 * (self.fsk.f1 + self.fsk.f2);
        // Noise on a discriminator spreads between the trackers; two tones
        // keep close to them.
        self.fsk.n >= FSK_MIN_PULSES && f.dev < FSK_MAX_DEV * f.ndev as f32
    }
}
