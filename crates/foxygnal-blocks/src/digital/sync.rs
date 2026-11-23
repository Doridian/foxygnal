//! Synchronisation: symbol timing (clock) recovery and carrier recovery.

use std::f32::consts::{PI, TAU};
use std::marker::PhantomData;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, WorkIo, WorkStatus,
};

use super::{Symbol, loop_gains};
use crate::{design, params};

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "clock_recovery",
            label: "Clock Recovery",
            category: "Digital",
            help: "Symbol timing recovery: finds the best sampling instant of each symbol and \
                   outputs one sample per symbol (the output rate is the input rate divided \
                   by the samples per symbol). A cubic interpolator samples between input \
                   samples; a Gardner or Mueller-Müller detector steers it. Feed it a \
                   matched-filtered signal (e.g. FSK soft symbols, or a Costas loop's \
                   output) with roughly constant amplitude.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::float_type(),
                design::float_min(
                    "sps",
                    "Samples/symbol",
                    8.0,
                    2.0,
                    None,
                    "Nominal input samples per symbol (input rate ÷ symbol rate). Need not be \
                     whole. At least 2.",
                ),
                design::float_min(
                    "loop_bw",
                    "Loop bandwidth",
                    0.01,
                    0.0,
                    None,
                    "Timing loop bandwidth as a fraction of the symbol rate. Larger locks \
                     faster and follows clock drift better, smaller jitters less. 0.005–0.05 \
                     is typical.",
                ),
                design::choice(
                    "ted",
                    "Detector",
                    vec!["gardner", "mueller_muller"],
                    "gardner",
                    "Timing error detector. Gardner uses a sample between symbols and \
                     works before carrier lock; Mueller-Müller uses one sample per symbol \
                     and decisions (±1 per axis), so it needs the carrier locked.",
                ),
            ],
        },
        build_clock,
    );
    r.register(
        BlockDescriptor {
            kind: "costas_loop",
            label: "Costas Loop",
            category: "Digital",
            help: "Carrier recovery for PSK: removes the carrier's frequency and phase offset \
                   so the constellation stands still (one sample per symbol works best, \
                   after clock recovery). The second output is the frequency correction, \
                   in Hz when the sample rate is known (else cycles per sample). Like any \
                   Costas loop it leaves a phase ambiguity of a multiple of 360°/order.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::C32),
            }],
            outputs: vec![
                PortSpec {
                    name: "out",
                    ty: Some(StreamType::C32),
                },
                PortSpec {
                    name: "freq",
                    ty: Some(StreamType::F32),
                },
            ],
            params: vec![
                design::choice(
                    "order",
                    "Order",
                    vec!["2", "4", "8"],
                    "2",
                    "2 = BPSK, 4 = QPSK (points at ±45°, ±135°), 8 = 8PSK (points at 22.5° \
                     + k·45°).",
                ),
                design::float_min(
                    "loop_bw",
                    "Loop bandwidth",
                    0.0628,
                    0.0,
                    Some("rad/sample"),
                    "Loop bandwidth in radians per sample (2π/100 ≈ 0.063 is a common \
                     start). Larger pulls in bigger offsets faster, smaller is quieter.",
                ),
            ],
        },
        build_costas,
    );
}

// ---- Clock recovery ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ted {
    Gardner,
    MuellerMuller,
}

/// The integrator may move the symbol period this far (fraction) from nominal.
const MAX_DEVIATION: f64 = 0.05;

/// Cubic Lagrange interpolation between `p[1]` (mu = 0) and `p[2]` (mu = 1).
#[inline]
fn interpolate<T: Symbol>(p: &[T; 4], mu: f32) -> T {
    let (a, b, c) = (mu + 1.0, mu - 1.0, mu - 2.0);
    let c0 = -mu * b * c / 6.0;
    let c1 = a * b * c / 2.0;
    let c2 = -a * mu * c / 2.0;
    let c3 = a * mu * b / 6.0;
    p[0] * c0 + p[1] * c1 + p[2] * c2 + p[3] * c3
}

pub struct ClockRecovery<T> {
    sps: f64,
    ted: Ted,
    kp: f64,
    ki: f64,
    /// The last four input samples, oldest first.
    hist: [T; 4],
    /// Time of the next strobe, in input samples after `hist[1]`.
    next: f64,
    /// The next strobe is the mid-point between two symbols (Gardner).
    mid_next: bool,
    /// Integrator: fractional symbol period correction.
    integ: f64,
    prev: T,
    mid: T,
    /// Average symbol power, to make the error independent of the level.
    power: f32,
    _t: PhantomData<T>,
}

impl<T: Symbol> ClockRecovery<T> {
    fn new(sps: f64, bw: f64, ted: Ted) -> Self {
        let (kp, ki) = loop_gains(bw);
        Self {
            sps,
            ted,
            kp,
            ki,
            hist: [T::default(); 4],
            next: 0.0,
            mid_next: false,
            integ: 0.0,
            prev: T::default(),
            mid: T::default(),
            power: 0.0,
            _t: PhantomData,
        }
    }

    /// Timing error of symbol `y`, normalized so that `-τ` (τ = how late in
    /// symbols) is the expected value. Updates the power estimate.
    fn error(&mut self, y: T) -> f64 {
        let p = y.dot(y);
        self.power = if self.power == 0.0 {
            p
        } else {
            self.power + 0.01 * (p - self.power)
        };
        let norm = self.power.max(1e-20);
        let e = match self.ted {
            // ≈ −4τ on a transition, so −2τ on average.
            Ted::Gardner => (self.prev - y).dot(self.mid) / norm / 2.0,
            // ≈ −τ on average.
            Ted::MuellerMuller => {
                (self.prev.decide().dot(y) - y.decide().dot(self.prev)) / norm.sqrt()
            }
        };
        f64::from(e).clamp(-1.0, 1.0)
    }
}

impl<T: Symbol> Block for ClockRecovery<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs[0].sample_rate;
        for o in cx.outputs.iter_mut() {
            o.sample_rate = rate.map(|r| r / self.sps);
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let (mut used, mut made) = (0, 0);
        // Symbols are at least 1.4 input samples apart (sps ≥ 2, the period
        // within 5% and the proportional step within a quarter symbol), so
        // one input sample yields at most one output.
        while used < inp.len() && made < out.len() {
            self.hist = [self.hist[1], self.hist[2], self.hist[3], inp[used]];
            used += 1;
            while self.next < 1.0 {
                let y = interpolate(&self.hist, self.next.max(0.0) as f32);
                let step = self.sps * (1.0 + self.integ);
                if self.ted == Ted::Gardner && self.mid_next {
                    self.mid = y;
                    self.mid_next = false;
                    self.next += step / 2.0;
                    continue;
                }
                let e = self.error(y);
                self.integ = (self.integ + self.ki * e).clamp(-MAX_DEVIATION, MAX_DEVIATION);
                let adjust = (self.sps * self.kp * e).clamp(-self.sps / 4.0, self.sps / 4.0);
                self.prev = y;
                out[made] = y;
                made += 1;
                if self.ted == Ted::Gardner {
                    self.mid_next = true;
                    self.next += step / 2.0 + adjust;
                } else {
                    self.next += step + adjust;
                }
            }
            self.next -= 1.0;
        }
        if used == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_clock(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let sps = p.f64("sps")?;
    if !(sps >= 2.0 && sps.is_finite()) {
        return Err(Error::Other(format!(
            "samples per symbol must be at least 2 (got {sps})"
        )));
    }
    let bw = p.f64("loop_bw")?;
    let ted = match p.str("ted")? {
        "mueller_muller" => Ted::MuellerMuller,
        _ => Ted::Gardner,
    };
    let ty = p.stream_type("type")?;
    let block: Box<dyn Block> = match ty {
        StreamType::C32 => Box::new(ClockRecovery::<Complex32>::new(sps, bw, ted)),
        _ => Box::new(ClockRecovery::<f32>::new(sps, bw, ted)),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

// ---- Costas loop ---------------------------------------------------------------

/// Largest frequency correction, in radians per sample.
const MAX_FREQ: f32 = 1.0;

pub struct Costas {
    order: u32,
    alpha: f32,
    beta: f32,
    phase: f32,
    freq: f32,
    /// Output units per radian/sample (Hz or cycles/sample).
    freq_scale: f32,
}

impl Costas {
    /// Phase error of a derotated sample, roughly sin(error) for small
    /// errors, independent of the amplitude.
    #[inline]
    fn error(&self, y: Complex32) -> f32 {
        let sign = |v: f32| if v >= 0.0 { 1.0 } else { -1.0 };
        let e = match self.order {
            2 => sign(y.re) * y.im,
            4 => sign(y.re) * y.im - sign(y.im) * y.re,
            _ => {
                const K: f32 = std::f32::consts::SQRT_2 - 1.0;
                if y.re.abs() >= y.im.abs() {
                    sign(y.re) * y.im - K * sign(y.im) * y.re
                } else {
                    K * sign(y.re) * y.im - sign(y.im) * y.re
                }
            }
        };
        let m = y.norm();
        if m > 1e-20 { e / m } else { 0.0 }
    }
}

impl Block for Costas {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs[0].sample_rate;
        self.freq_scale = (rate.unwrap_or(1.0) / std::f64::consts::TAU) as f32;
        if let Some(o) = cx.outputs.get_mut(1) {
            o.center_freq = None;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<Complex32>();
        let (o0, o1) = io.outputs.split_at_mut(1);
        let out = o0[0].slice_mut::<Complex32>();
        let fout = o1[0].slice_mut::<f32>();
        let n = inp.len().min(out.len()).min(fout.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let (mut phase, mut freq) = (self.phase, self.freq);
        for ((o, f), &x) in out[..n].iter_mut().zip(&mut fout[..n]).zip(&inp[..n]) {
            let (s, c) = phase.sin_cos();
            let y = x * Complex32::new(c, -s);
            *o = y;
            let e = self.error(y);
            freq = (freq + self.beta * e).clamp(-MAX_FREQ, MAX_FREQ);
            phase += freq + self.alpha * e;
            if phase > PI {
                phase -= TAU;
            } else if phase < -PI {
                phase += TAU;
            }
            *f = freq * self.freq_scale;
        }
        (self.phase, self.freq) = (phase, freq);
        io.consume(0, n);
        io.produce(0, n);
        io.produce(1, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_costas(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let order = p.str("order")?.parse::<u32>().unwrap_or(2);
    let (alpha, beta) = loop_gains(p.f64("loop_bw")?);
    Ok(Instance {
        block: Box::new(Costas {
            order,
            alpha: alpha as f32,
            beta: beta as f32,
            phase: 0.0,
            freq: 0.0,
            freq_scale: 1.0,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![
            ("out".into(), StreamType::C32),
            ("freq".into(), StreamType::F32),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_interpolation_is_exact_for_cubics() {
        let f = |t: f32| 0.5 * t * t * t - t * t + 2.0 * t - 1.0;
        let p = [f(-1.0), f(0.0), f(1.0), f(2.0)];
        for mu in [0.0, 0.25, 0.5, 0.9] {
            assert!((interpolate(&p, mu) - f(mu)).abs() < 1e-5, "mu {mu}");
        }
    }
}
