//! Bell 202 AFSK demodulator (1200 Bd, 1200 Hz mark, 2200 Hz space), the
//! modem of 1200 Bd packet radio and APRS.
//!
//! Two quadrature correlators: the audio is mixed down by each tone and
//! summed over one bit (a complex moving average), and the magnitudes are
//! compared. Received audio is rarely flat: a transmitter's pre-emphasis, a
//! receiver's de-emphasis, or both, tilt one tone against the other by
//! several dB. Each correlator's output is therefore divided by its own
//! slowly decaying peak before the comparison, so both tones swing over the
//! same range whatever the tilt. A digital PLL on the zero crossings of the
//! difference picks the middle of each bit.

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, WorkIo, WorkStatus,
};

use super::text::BitClock;
use crate::design::{self, fmt_hz, fmt_rate};
use crate::filters::not_configured;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "afsk1200_demod",
            label: "AFSK 1200 Demodulator",
            category: "Digital",
            help: "Bell 202 AFSK (1200 Hz mark, 2200 Hz space, 1200 Bd), as used by packet \
                   radio and APRS: FM-demodulated audio in (any rate from about 7 kS/s; \
                   de-emphasis on or off), bits out at the baud rate, mark = 1, for an HDLC \
                   Deframer (which undoes the NRZI coding). Quadrature correlators with \
                   per-tone level tracking, so pre-/de-emphasis tilt doesn't matter, and a \
                   PLL for the bit timing.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                design::choice(
                    "output",
                    "Output",
                    vec!["bits", "soft"],
                    "bits",
                    "Bits: u8, 1 for mark and 0 for space. Soft: f32 at each bit's middle, \
                     about +1 for mark and −1 for space.",
                ),
                design::float_min(
                    "mark",
                    "Mark",
                    1_200.0,
                    1.0,
                    Some("Hz"),
                    "Mark tone (a 1). Bell 202: 1200 Hz.",
                ),
                design::float_min(
                    "space",
                    "Space",
                    2_200.0,
                    1.0,
                    Some("Hz"),
                    "Space tone (a 0). Bell 202: 2200 Hz.",
                ),
                design::float_min(
                    "baud",
                    "Baud rate",
                    1_200.0,
                    1.0,
                    Some("Bd"),
                    "Bits per second.",
                ),
            ],
        },
        build,
    );
}

/// One tone's correlator: mix down, sum over one bit.
struct Correlator {
    lo: Complex32,
    step: Complex32,
    ring: Vec<Complex32>,
    sum_re: f64,
    sum_im: f64,
    /// Decaying peak of the magnitude.
    peak: f32,
}

impl Correlator {
    fn new(freq: f64, rate: f64, len: usize) -> Self {
        let w = std::f64::consts::TAU * freq / rate;
        Self {
            lo: Complex32::new(1.0, 0.0),
            step: Complex32::new(w.cos() as f32, -w.sin() as f32),
            ring: vec![Complex32::default(); len],
            sum_re: 0.0,
            sum_im: 0.0,
            peak: 1e-6,
        }
    }

    /// Magnitude of the sum over the last bit, after adding `x` at `pos`.
    #[inline]
    fn push(&mut self, x: f32, pos: usize) -> f32 {
        let v = self.lo * x;
        self.lo *= self.step;
        let old = std::mem::replace(&mut self.ring[pos], v);
        self.sum_re += f64::from(v.re) - f64::from(old.re);
        self.sum_im += f64::from(v.im) - f64::from(old.im);
        (self.sum_re * self.sum_re + self.sum_im * self.sum_im).sqrt() as f32
    }

    #[inline]
    fn normalized(&mut self, m: f32, attack: f32, decay: f32) -> f32 {
        let a = if m > self.peak { attack } else { decay };
        self.peak += a * (m - self.peak);
        m / self.peak.max(1e-9)
    }

    fn renormalize(&mut self) {
        let n = self.lo.norm();
        if n > 0.0 {
            self.lo /= n;
        }
        // Drop accumulated rounding from the running sum.
        let (re, im) = self.ring.iter().fold((0.0f64, 0.0f64), |(a, b), z| {
            (a + f64::from(z.re), b + f64::from(z.im))
        });
        (self.sum_re, self.sum_im) = (re, im);
    }
}

struct State {
    mark: Correlator,
    space: Correlator,
    pos: usize,
    len: usize,
    attack: f32,
    decay: f32,
    clock: BitClock,
    count: u32,
}

impl State {
    /// Soft value at the next bit decision, if this sample completes one.
    #[inline]
    fn sample(&mut self, x: f32) -> Option<f32> {
        let m = self.mark.push(x, self.pos);
        let s = self.space.push(x, self.pos);
        self.pos += 1;
        if self.pos == self.len {
            self.pos = 0;
        }
        self.count += 1;
        if self.count == 4096 {
            self.count = 0;
            self.mark.renormalize();
            self.space.renormalize();
        }
        let d = self.mark.normalized(m, self.attack, self.decay)
            - self.space.normalized(s, self.attack, self.decay);
        self.clock.sample(d)
    }
}

pub struct AfskDemod {
    soft: bool,
    mark: f64,
    space: f64,
    baud: f64,
    state: Option<State>,
}

impl Block for AfskDemod {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let top = self.mark.max(self.space) + self.baud;
        if 2.0 * top > rate || rate < 4.0 * self.baud {
            return Err(Error::Other(format!(
                "a {} input is too slow for tones up to {} at {} Bd: needs at least {}",
                fmt_rate(rate),
                fmt_hz(self.mark.max(self.space)),
                self.baud,
                fmt_rate((2.0 * top).max(4.0 * self.baud))
            )));
        }
        let sps = rate / self.baud;
        let len = (sps.round() as usize).max(1);
        // Peak follower: rises within a fraction of a bit, falls over ~40 bits.
        let attack = (1.0 - (-1.0 / (0.3 * sps)).exp()) as f32;
        let decay = (1.0 - (-1.0 / (40.0 * sps)).exp()) as f32;
        self.state = Some(State {
            mark: Correlator::new(self.mark, rate, len),
            space: Correlator::new(self.space, rate, len),
            pos: 0,
            len,
            attack,
            decay,
            clock: BitClock::new(sps, 0.25),
            count: 0,
        });
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(self.baud);
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let st = self.state.as_mut().ok_or_else(not_configured)?;
        let (used, made) = if self.soft {
            let (inp, out) = io.in_out::<f32, f32>(0, 0);
            run(st, inp, out, |d| d)
        } else {
            let (inp, out) = io.in_out::<f32, u8>(0, 0);
            run(st, inp, out, |d| u8::from(d >= 0.0))
        };
        if used == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

/// Demodulate until the input runs out or the output is full. Returns
/// (samples used, bits made).
fn run<O: Copy>(
    st: &mut State,
    inp: &[f32],
    out: &mut [O],
    map: impl Fn(f32) -> O,
) -> (usize, usize) {
    let (mut used, mut made) = (0, 0);
    // Each sample makes at most one bit, so stop while there's room for one.
    while used < inp.len() && made < out.len() {
        if let Some(d) = st.sample(inp[used]) {
            out[made] = map(d);
            made += 1;
        }
        used += 1;
    }
    (used, made)
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let soft = p.str("output")? == "soft";
    Ok(Instance {
        block: Box::new(AfskDemod {
            soft,
            mark: p.f64("mark")?,
            space: p.f64("space")?,
            baud: p.f64("baud")?,
            state: None,
        }),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![(
            "out".into(),
            if soft {
                StreamType::F32
            } else {
                StreamType::U8
            },
        )],
    })
}
