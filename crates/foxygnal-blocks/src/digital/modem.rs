//! Modems: hard decisions (binary slicer, PSK demapper) and an FSK
//! demodulator.

use std::f64::consts::TAU;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Kernels, OnePole, kernels};

use crate::design::{self, fmt_hz, fmt_rate};
use crate::params;

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "binary_slicer",
            label: "Binary Slicer",
            category: "Digital",
            help: "Hard decision on soft symbols: 1 when the input is at or above the \
                   threshold, else 0. One bit per byte.",
            inputs: vec![port("in", StreamType::F32)],
            outputs: vec![port("out", StreamType::U8)],
            params: vec![params::float(
                "threshold",
                "Threshold",
                0.0,
                None,
                "Decision level.",
            )],
        },
        build_slicer,
    );
    r.register(
        BlockDescriptor {
            kind: "psk_demod",
            label: "PSK Demapper",
            category: "Digital",
            help: "Maps each complex symbol to the nearest constellation point and outputs its \
                   Gray-coded symbol number (or its bits, most significant first, one per \
                   byte). BPSK: +1 → 0, −1 → 1. QPSK points sit at ±45°/±135° and 8PSK at \
                   22.5° + k·45°, where a Costas loop of the same order locks them.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![port("out", StreamType::U8)],
            params: vec![
                design::choice(
                    "modulation",
                    "Modulation",
                    vec!["bpsk", "qpsk", "8psk"],
                    "bpsk",
                    "Constellation: 1, 2 or 3 bits per symbol.",
                ),
                design::choice(
                    "output",
                    "Output",
                    vec!["symbols", "bits"],
                    "bits",
                    "Symbols: one symbol number (0…M−1) per byte. Bits: the symbol's bits, \
                     one per byte, most significant first (the rate is multiplied by the \
                     bits per symbol).",
                ),
            ],
        },
        build_psk,
    );
    r.register(
        BlockDescriptor {
            kind: "fsk_demod",
            label: "FSK Demodulator",
            category: "Digital",
            help: "2-FSK to soft symbols: a quadrature discriminator (the frequency of each \
                   sample, ±deviation → ±1) followed by a moving average one symbol long \
                   (the matched filter for rectangular FSK). Positive = the upper tone. \
                   Follow it with clock recovery and a binary slicer. The output rate is \
                   the input rate.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![port("out", StreamType::F32)],
            params: vec![
                design::float_min(
                    "deviation",
                    "Deviation",
                    4_500.0,
                    0.0,
                    Some("Hz"),
                    "Frequency of each tone from the centre (half the tone spacing).",
                ),
                design::float_min(
                    "baud",
                    "Symbol rate",
                    1_200.0,
                    0.0,
                    Some("Bd"),
                    "Symbols per second; sets the averaging length.",
                ),
                design::float_min(
                    "dc_symbols",
                    "DC correction",
                    0.0,
                    0.0,
                    None,
                    "Time constant, in symbols, of a DC remover that cancels a frequency \
                     offset (the tones not centred on 0 Hz). 0 = off. Use tens of symbols or \
                     more, and only for data without long runs of one tone.",
                ),
            ],
        },
        build_fsk,
    );
}

// ---- Binary slicer -------------------------------------------------------------

pub struct BinarySlicer {
    threshold: f32,
}

impl Block for BinarySlicer {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<f32, u8>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = u8::from(x >= self.threshold);
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_slicer(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(BinarySlicer {
            threshold: p.f64("threshold")? as f32,
        }),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- PSK demapper --------------------------------------------------------------

/// The Gray-coded symbol nearest to `x` in an `m`-PSK constellation (points
/// at angle `offset + 2πk/m`).
#[inline]
pub(crate) fn psk_symbol(x: Complex32, m: u32) -> u8 {
    let k = match m {
        2 => u32::from(x.re < 0.0),
        4 => match (x.re >= 0.0, x.im >= 0.0) {
            (true, true) => 0,
            (false, true) => 1,
            (false, false) => 2,
            (true, false) => 3,
        },
        _ => {
            let step = std::f32::consts::TAU / m as f32;
            let a = x.im.atan2(x.re) - step / 2.0;
            (a / step).round().rem_euclid(m as f32) as u32 % m
        }
    };
    (k ^ (k >> 1)) as u8
}

pub struct PskDemod {
    m: u32,
    bits: u32,
    unpack: bool,
    /// A symbol whose last `left` bits haven't gone out yet.
    spill: u8,
    left: usize,
}

impl Block for PskDemod {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if self.unpack {
            let rate = cx.inputs[0].sample_rate;
            for o in cx.outputs.iter_mut() {
                o.sample_rate = rate.map(|r| r * f64::from(self.bits));
            }
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<Complex32, u8>(0, 0);
        if !self.unpack {
            let n = inp.len().min(out.len());
            if n == 0 {
                return Ok(WorkStatus::Blocked);
            }
            for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
                *o = psk_symbol(x, self.m);
            }
            io.consume(0, n);
            io.produce(0, n);
            return Ok(WorkStatus::Ok);
        }
        let per = self.bits as usize;
        let (mut used, mut made) = (0, 0);
        loop {
            // Bits of a symbol split across a buffer wrap go out first.
            while self.left > 0 && made < out.len() {
                self.left -= 1;
                out[made] = (self.spill >> self.left) & 1;
                made += 1;
            }
            if self.left > 0 || used == inp.len() || made == out.len() {
                break;
            }
            let n = (inp.len() - used).min((out.len() - made) / per);
            for (o, &x) in out[made..made + n * per]
                .chunks_exact_mut(per)
                .zip(&inp[used..used + n])
            {
                let s = psk_symbol(x, self.m);
                for (i, b) in o.iter_mut().enumerate() {
                    *b = (s >> (per - 1 - i)) & 1;
                }
            }
            used += n;
            made += n * per;
            if used < inp.len() && made < out.len() {
                self.spill = psk_symbol(inp[used], self.m);
                self.left = per;
                used += 1;
            } else {
                break;
            }
        }
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_psk(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (m, bits) = match p.str("modulation")? {
        "qpsk" => (4, 2),
        "8psk" => (8, 3),
        _ => (2, 1),
    };
    Ok(Instance {
        block: Box::new(PskDemod {
            m,
            bits,
            unpack: p.str("output")? == "bits",
            spill: 0,
            left: 0,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- FSK demodulator -----------------------------------------------------------

pub struct FskDemod {
    deviation: f64,
    baud: f64,
    dc_symbols: f64,
    k: &'static Kernels,
    gain: f32,
    prev: Complex32,
    /// Moving average: the last `len` discriminator outputs and their sum.
    window: Vec<f32>,
    pos: usize,
    sum: f64,
    dc: Option<OnePole>,
}

impl Block for FskDemod {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if !(self.deviation > 0.0 && self.deviation < rate / 2.0) {
            return Err(Error::Other(format!(
                "deviation {} must be above 0 Hz and below {} (half the {} input rate)",
                fmt_hz(self.deviation),
                fmt_hz(rate / 2.0),
                fmt_rate(rate)
            )));
        }
        if !(self.baud > 0.0 && self.baud <= rate / 2.0) {
            return Err(Error::Other(format!(
                "symbol rate {} Bd must be above 0 and at most half the {} input rate",
                self.baud,
                fmt_rate(rate)
            )));
        }
        let sps = rate / self.baud;
        self.gain = (rate / (TAU * self.deviation)) as f32;
        self.window = vec![0.0; (sps.round() as usize).max(1)];
        self.dc = (self.dc_symbols > 0.0)
            .then(|| OnePole::new(OnePole::alpha_for(self.dc_symbols / self.baud, rate)));
        for o in cx.outputs.iter_mut() {
            o.center_freq = None;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<Complex32, f32>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 || self.window.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        let out = &mut out[..n];
        self.prev = (self.k.fm_discriminate)(self.prev, &inp[..n], self.gain, out);
        let len = self.window.len();
        let scale = 1.0 / len as f64;
        let (mut pos, mut sum) = (self.pos, self.sum);
        for o in out.iter_mut() {
            sum += f64::from(*o) - f64::from(self.window[pos]);
            self.window[pos] = *o;
            pos += 1;
            if pos == len {
                pos = 0;
            }
            *o = (sum * scale) as f32;
        }
        (self.pos, self.sum) = (pos, sum);
        if let Some(dc) = &mut self.dc {
            for o in out.iter_mut() {
                *o -= dc.next(*o);
            }
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_fsk(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(FskDemod {
            deviation: p.f64("deviation")?,
            baud: p.f64("baud")?,
            dc_symbols: p.f64("dc_symbols")?,
            k: kernels(),
            gain: 1.0,
            prev: Complex32::new(1.0, 0.0),
            window: Vec::new(),
            pos: 0,
            sum: 0.0,
            dc: None,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::F32)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(deg: f32) -> Complex32 {
        Complex32::from_polar(1.0, deg.to_radians())
    }

    #[test]
    fn gray_coded_psk_neighbours_differ_by_one_bit() {
        for (m, offset) in [(2u32, 0.0f32), (4, 45.0), (8, 22.5)] {
            let step = 360.0 / m as f32;
            let syms: Vec<u8> = (0..m)
                .map(|k| psk_symbol(at(offset + step * k as f32), m))
                .collect();
            let mut sorted = syms.clone();
            sorted.sort();
            assert_eq!(sorted, (0..m as u8).collect::<Vec<_>>(), "m {m}: {syms:?}");
            for k in 0..m as usize {
                let next = syms[(k + 1) % m as usize];
                assert_eq!((syms[k] ^ next).count_ones(), 1, "m {m}: {syms:?}");
            }
            // A little off the point still decides the same.
            for k in 0..m {
                let a = offset + step * k as f32;
                assert_eq!(psk_symbol(at(a + step * 0.4), m), syms[k as usize]);
                assert_eq!(psk_symbol(at(a - step * 0.4), m), syms[k as usize]);
            }
        }
        assert_eq!(psk_symbol(Complex32::new(1.0, 0.0), 2), 0);
        assert_eq!(psk_symbol(Complex32::new(-1.0, 0.0), 2), 1);
    }
}
