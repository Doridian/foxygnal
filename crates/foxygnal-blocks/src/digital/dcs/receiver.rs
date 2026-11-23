//! The whole-band receiver: a fast-convolution channelizer, a carrier
//! detector, and a pool of demodulators.
//!
//! - **Channelizer.** The input is cut into blocks of `N` samples with 50 %
//!   overlap (`N` a power of two giving bins of 50–100 Hz) and transformed
//!   once. Each active platform takes the `M` bins around its carrier
//!   (32 for 300 bit/s, 64 for 1200 bit/s), weights them with a smooth
//!   low-pass, and an `M`-point inverse transform gives its channel at
//!   `M` × bin width samples per second; the middle half of each block is
//!   kept (overlap-save with a zero-phase filter). One big FFT per block
//!   serves every platform.
//! - **Detector.** Every block also gives a Hann-windowed power spectrum
//!   (three bins of the same transform), averaged over ~30 ms. The noise
//!   floor is a low percentile of 32-bin groups. A transmission starts with
//!   0.25 or 0.5 s of plain carrier: a narrow peak `level` dB above the
//!   floor that stays for ~40 ms starts a demodulator there, with the
//!   frequency from a parabola through the peak.
//! - **Demodulators.** Each starts a 300 and a 1200 bit/s chain on the
//!   carrier; whichever finds the preamble keeps going. While it runs, the
//!   detector ignores its band. A carrier that never shows a preamble (the
//!   pilot, an SDR's DC spike, a transmission caught in the middle) is
//!   masked until it goes away.

use std::sync::Arc;

use foxygnal_core::Complex32;
use rustfft::{Fft, FftPlanner};

use super::demod::{Chain, End, State, Stats};
use super::frame::{Baud, Format, bch_correct, channel_at, to_uplink};

/// Receiver settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub fs: f64,
    /// RF frequency of the stream's 0 Hz, if known.
    pub center: Option<f64>,
    /// Detection threshold above the noise floor (dB, in one bin).
    pub level_db: f32,
    /// Most transmissions demodulated at once.
    pub max_platforms: usize,
    /// Data rates to look for.
    pub b300: bool,
    pub b1200: bool,
}

/// A decoded transmission.
pub struct Message<'a> {
    /// Time of the end of the FSS (s from the start of the stream).
    pub t: f64,
    /// GOES ID as received, and corrected (`None`: not correctable).
    pub addr_rx: u32,
    pub addr: Option<(u32, u32)>,
    pub baud: Baud,
    /// Carrier frequency relative to the stream centre (Hz).
    pub freq: f64,
    /// Absolute RF frequency, if the centre is known.
    pub rf: Option<f64>,
    /// CS2 channel and offset from its centre (Hz).
    pub channel: Option<(u16, f64)>,
    pub stats: Stats,
    pub flag: u8,
    pub format: Format,
    pub data: &'a [u8],
    pub end: End,
}

impl Message<'_> {
    /// Characters whose parity bit is wrong (ASCII and pseudo-binary use
    /// odd parity).
    pub fn parity_errors(&self) -> usize {
        if self.format == Format::Binary {
            return 0;
        }
        self.data.iter().filter(|b| b.count_ones() % 2 == 0).count()
    }
}

/// One bin-range extraction of the channelizer for one chain.
struct Extract {
    m: usize,
    h: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    ifft: Arc<dyn Fft<f32>>,
}

struct Slot {
    active: bool,
    /// Finished: waiting for its message to be taken.
    ready: bool,
    /// Centre bin (signed).
    k0: i64,
    /// Carrier offset from bin `k0` at the start (Hz).
    start_freq: f64,
    chains: [Chain; 2],
    ext: [Extract; 2],
    enabled: [bool; 2],
    /// Which chain holds the message.
    winner: usize,
}

struct Tracker {
    k: i64,
    count: u32,
    seen: bool,
    fired: bool,
}

struct Mask {
    k: i64,
    quiet: u32,
}

pub struct Receiver {
    cfg: Config,
    n: usize,
    hop: usize,
    bin: f64,
    fft: Arc<dyn Fft<f32>>,
    fft_scratch: Vec<Complex32>,
    buf: Vec<Complex32>,
    fill: usize,
    spec: Vec<Complex32>,
    /// Input sample number of `buf[0]`.
    base: u64,
    block: u64,
    pavg: Vec<f32>,
    floor: Vec<f32>,
    sort: Vec<f32>,
    alpha: f32,
    thr: f32,
    persist: u32,
    quiet_hops: u32,
    trackers: Vec<Tracker>,
    masks: Vec<Mask>,
    slots: Vec<Slot>,
    /// Finished slots in the order they finished.
    ready: Vec<usize>,
    /// Bins ignored at each band edge.
    edge: i64,
}

const GROUP: usize = 32;
const MAX_TRACKERS: usize = 128;
const MAX_MASKS: usize = 64;

/// The low-pass weights for `m` bins of width `bin` around a carrier: flat
/// to `pass` Hz, raised-cosine to zero at `stop` Hz.
fn taper(m: usize, bin: f64, pass: f64, stop: f64, scale: f32) -> Vec<f32> {
    (0..m)
        .map(|i| {
            let k = if i < m / 2 {
                i as f64
            } else {
                i as f64 - m as f64
            };
            let f = (k * bin).abs();
            let w = if f <= pass {
                1.0
            } else if f >= stop {
                0.0
            } else {
                0.5 + 0.5 * (std::f64::consts::PI * (f - pass) / (stop - pass)).cos()
            };
            w as f32 * scale
        })
        .collect()
}

impl Receiver {
    pub fn new(cfg: Config) -> Self {
        let n = (cfg.fs / 100.0).ceil().max(256.0) as usize;
        let n = n.next_power_of_two();
        let hop = n / 2;
        let bin = cfg.fs / n as f64;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let fft_scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let mut ext = |m: usize, rate: f64| {
            let ifft = planner.plan_fft_inverse(m);
            let pass = rate + 150.0;
            let stop = (pass + (4.0 * bin).max(rate)).min(m as f64 / 2.0 * bin - bin);
            Extract {
                m,
                h: taper(m, bin, pass, stop, 1.0 / n as f32),
                buf: vec![Complex32::new(0.0, 0.0); m],
                scratch: vec![Complex32::new(0.0, 0.0); ifft.get_inplace_scratch_len()],
                ifft,
            }
        };
        let slots = (0..cfg.max_platforms.max(1))
            .map(|_| Slot {
                active: false,
                ready: false,
                k0: 0,
                start_freq: 0.0,
                chains: [
                    Chain::new(Baud::B300, 32.0 * bin),
                    Chain::new(Baud::B1200, 64.0 * bin),
                ],
                ext: [ext(32, 150.0), ext(64, 600.0)],
                enabled: [false; 2],
                winner: 0,
            })
            .collect();
        let hop_s = hop as f64 / cfg.fs;
        Self {
            n,
            hop,
            bin,
            fft,
            fft_scratch,
            buf: vec![Complex32::new(0.0, 0.0); n],
            fill: 0,
            spec: vec![Complex32::new(0.0, 0.0); n],
            base: 0,
            block: 0,
            pavg: vec![0.0; n],
            floor: vec![1.0; n],
            sort: vec![0.0; GROUP],
            alpha: (1.0 - (-hop_s / 0.03).exp()) as f32,
            thr: 10f32.powf(cfg.level_db / 10.0),
            persist: (0.04 / hop_s).ceil().max(2.0) as u32,
            quiet_hops: (0.3 / hop_s).ceil() as u32,
            trackers: Vec::with_capacity(MAX_TRACKERS),
            masks: Vec::with_capacity(MAX_MASKS),
            ready: Vec::with_capacity(cfg.max_platforms.max(1)),
            edge: (n as f64 * 0.05).ceil() as i64 + 4,
            slots,
            cfg,
        }
    }

    /// Bin width of the channelizer (Hz).
    pub fn bin_hz(&self) -> f64 {
        self.bin
    }

    fn idx(&self, k: i64) -> usize {
        k.rem_euclid(self.n as i64) as usize
    }

    /// Feed input; returns how much was taken. Stops early once a message is
    /// ready ([`Self::take`]).
    pub fn push(&mut self, x: &[Complex32]) -> usize {
        let mut used = 0;
        while used < x.len() && self.ready.is_empty() {
            let take = (self.n - self.fill).min(x.len() - used);
            self.buf[self.fill..self.fill + take].copy_from_slice(&x[used..used + take]);
            self.fill += take;
            used += take;
            if self.fill == self.n {
                self.process_block();
                self.buf.copy_within(self.hop.., 0);
                self.fill = self.n - self.hop;
                self.base += self.hop as u64;
                self.block += 1;
            }
        }
        used
    }

    /// End of input: finish whatever is being received.
    pub fn finish(&mut self) {
        for (i, s) in self.slots.iter_mut().enumerate() {
            if s.active && !s.ready {
                let c = &mut s.chains[s.winner];
                c.end_input();
                if matches!(c.state, State::Done(_)) && c.frame.has_header() {
                    s.ready = true;
                    self.ready.push(i);
                } else {
                    s.active = false;
                }
            }
        }
    }

    /// Hand the oldest finished message to `f`, then free its demodulator.
    /// False if none is waiting.
    pub fn take(&mut self, f: impl FnOnce(&Message<'_>)) -> bool {
        if self.ready.is_empty() {
            return false;
        }
        let i = self.ready.remove(0);
        let slot = &mut self.slots[i];
        let c = &slot.chains[slot.winner];
        let State::Done(end) = c.state else {
            unreachable!("ready slots are done");
        };
        let stats = c.stats();
        // The chain started at `start_freq` and its correction includes it.
        let freq = slot.k0 as f64 * self.bin + stats.freq_correction;
        let rf = self.cfg.center.map(|c| c + freq);
        let fr = &c.frame;
        f(&Message {
            t: c.fss_time / self.cfg.fs,
            addr_rx: fr.addr,
            addr: bch_correct(fr.addr),
            baud: c.baud,
            freq,
            rf,
            channel: rf.and_then(|f| channel_at(to_uplink(f))),
            stats,
            flag: fr.flag,
            format: fr.format,
            data: &fr.data,
            end,
        });
        slot.active = false;
        slot.ready = false;
        slot.chains[0].stop();
        slot.chains[1].stop();
        true
    }

    fn process_block(&mut self) {
        self.spec.copy_from_slice(&self.buf);
        self.fft
            .process_with_scratch(&mut self.spec, &mut self.fft_scratch);
        self.feed_slots();
        self.detect();
    }

    fn feed_slots(&mut self) {
        let n = self.n as i64;
        let block = self.block;
        for si in 0..self.slots.len() {
            let slot = &mut self.slots[si];
            if !slot.active || slot.ready {
                continue;
            }
            for c in 0..2 {
                if !slot.enabled[c] {
                    continue;
                }
                let e = &mut slot.ext[c];
                let m = e.m as i64;
                for j in 0..m {
                    let k = if j < m / 2 { j } else { j - m };
                    let src = (slot.k0 + k).rem_euclid(n) as usize;
                    e.buf[j as usize] = self.spec[src] * e.h[j as usize];
                }
                e.ifft.process_with_scratch(&mut e.buf, &mut e.scratch);
                // Continuity between blocks: the mix by k0 bins restarts at
                // every block, half a block later each time.
                if (slot.k0 * block as i64).rem_euclid(2) == 1 {
                    for v in &mut e.buf {
                        *v = -*v;
                    }
                }
                let d = (self.n / e.m) as f64;
                let q = e.m / 4;
                let t0 = self.base as f64 + q as f64 * d;
                slot.chains[c].push(&e.buf[q..3 * q], t0, d);
            }
            // Settle which chain wins, or give up.
            for c in 0..2 {
                if !slot.enabled[c] {
                    continue;
                }
                match slot.chains[c].state {
                    State::Data | State::Done(_) => {
                        slot.enabled[1 - c] = false;
                        slot.chains[1 - c].stop();
                        slot.winner = c;
                    }
                    State::TimedOut => slot.enabled[c] = false,
                    _ => {}
                }
            }
            if !slot.enabled[0] && !slot.enabled[1] {
                slot.active = false;
                if self.masks.len() < MAX_MASKS {
                    self.masks.push(Mask {
                        k: slot.k0,
                        quiet: 0,
                    });
                }
            } else if matches!(slot.chains[slot.winner].state, State::Done(_)) {
                if slot.chains[slot.winner].frame.has_header() {
                    slot.ready = true;
                    self.ready.push(si);
                } else {
                    // A sync on something that wasn't a transmission.
                    slot.active = false;
                    slot.chains[slot.winner].stop();
                }
            }
        }
    }

    /// Half-width (bins) of the band a slot keeps the detector off.
    fn slot_span(&self, s: &Slot) -> i64 {
        let hz = if s.enabled[0] && s.enabled[1] {
            // Still a plain carrier: neighbours 750 Hz away are other
            // platforms.
            450.0
        } else if s.enabled[1] {
            800.0
        } else {
            300.0
        };
        (hz / self.bin).ceil() as i64
    }

    fn busy(&self, k: i64) -> bool {
        let mask = (1_000.0 / self.bin).ceil() as i64;
        self.slots
            .iter()
            .any(|s| s.active && !s.ready && (k - s.k0).abs() <= self.slot_span(s))
            || self.masks.iter().any(|m| (k - m.k).abs() <= mask)
    }

    fn detect(&mut self) {
        let n = self.n;
        // Hann-windowed power from the rectangular transform, averaged.
        for i in 0..n {
            let a = self.spec[i];
            let b = self.spec[(i + n - 1) % n];
            let c = self.spec[(i + 1) % n];
            let h = a * 0.5 - (b + c) * 0.25;
            self.pavg[i] += self.alpha * (h.norm_sqr() - self.pavg[i]);
        }
        // Noise floor: a low percentile per group of bins.
        for g in 0..n / GROUP {
            let s = &self.pavg[g * GROUP..(g + 1) * GROUP];
            self.sort.copy_from_slice(s);
            let q = GROUP / 4;
            let (_, v, _) = self.sort.select_nth_unstable_by(q, f32::total_cmp);
            let fl = (*v / 0.75).max(1e-30);
            self.floor[g * GROUP..(g + 1) * GROUP].fill(fl);
        }
        // Masks lift once their carrier has been gone a while.
        let thr = self.thr;
        let mut mi = 0;
        while mi < self.masks.len() {
            let k = self.masks[mi].k;
            let loud = (k - 1..=k + 1).any(|j| {
                let i = self.idx(j);
                self.pavg[i] > thr * self.floor[i]
            });
            let m = &mut self.masks[mi];
            m.quiet = if loud { 0 } else { m.quiet + 1 };
            if m.quiet >= self.quiet_hops {
                self.masks.swap_remove(mi);
            } else {
                mi += 1;
            }
        }
        for t in &mut self.trackers {
            t.seen = false;
        }
        let half = (n / 2) as i64;
        let mut k = -half + self.edge;
        while k < half - self.edge {
            let i = self.idx(k);
            let p = self.pavg[i];
            if p <= thr * self.floor[i] {
                k += 1;
                continue;
            }
            let at = |j: i64| self.pavg[self.idx(j)];
            let is_peak = p >= at(k - 1) && p >= at(k + 1) && p > at(k - 2) && p > at(k + 2);
            if !is_peak || self.busy(k) {
                k += 1;
                continue;
            }
            // Narrow: most of the power above the floor sits in two bins.
            let mut total = 0.0;
            for j in k - 5..=k + 5 {
                total += (at(j) - self.floor[self.idx(j)]).max(0.0);
            }
            let core = p + at(k - 1).max(at(k + 1)) - 2.0 * self.floor[i];
            if core < 0.6 * total {
                k += 1;
                continue;
            }
            if let Some(t) = self
                .trackers
                .iter_mut()
                .find(|t| !t.seen && (t.k - k).abs() <= 1)
            {
                t.k = k;
                t.count += 1;
                t.seen = true;
            } else if self.trackers.len() < MAX_TRACKERS {
                self.trackers.push(Tracker {
                    k,
                    count: 1,
                    seen: true,
                    fired: false,
                });
            }
            k += 3;
        }
        self.trackers.retain(|t| t.seen);
        for ti in 0..self.trackers.len() {
            let t = &self.trackers[ti];
            if t.fired || t.count < self.persist {
                continue;
            }
            let k = t.k;
            if self.busy(k) {
                continue;
            }
            let Some(si) = self.slots.iter().position(|s| !s.active) else {
                break;
            };
            self.trackers[ti].fired = true;
            // Peak frequency from a parabola through the log powers.
            let l = |j: i64| self.pavg[self.idx(j)].max(1e-30).ln();
            let (a, b, c) = (l(k - 1), l(k), l(k + 1));
            let den = a - 2.0 * b + c;
            let delta = if den < 0.0 {
                (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let f = (k as f64 + f64::from(delta)) * self.bin;
            let k0 = (f / self.bin).round() as i64;
            let slot = &mut self.slots[si];
            slot.active = true;
            slot.ready = false;
            slot.k0 = k0;
            slot.start_freq = f - k0 as f64 * self.bin;
            slot.enabled = [self.cfg.b300, self.cfg.b1200];
            slot.winner = if self.cfg.b300 { 0 } else { 1 };
            for c in 0..2 {
                if slot.enabled[c] {
                    slot.chains[c].begin(slot.start_freq);
                }
            }
        }
    }
}
