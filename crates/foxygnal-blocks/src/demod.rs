//! Analog demodulators: FM (quadrature), AM (envelope) and SSB (Weaver),
//! complex baseband in, real audio out.

use std::f64::consts::TAU;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, FrequencyResponse, Instance, ParamReader,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Band, DesignWindow, FirSpec, OnePole, Resampler, Rotator};

use crate::design::{self, fmt_hz, fmt_rate};
use crate::filters::not_configured;

fn c32_in_f32_out() -> (Vec<PortSpec>, Vec<PortSpec>) {
    (
        vec![PortSpec {
            name: "in",
            ty: Some(StreamType::C32),
        }],
        vec![PortSpec {
            name: "out",
            ty: Some(StreamType::F32),
        }],
    )
}

fn instance(block: Box<dyn Block>) -> Instance {
    Instance {
        block,
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::F32)],
    }
}

/// Audio has no RF centre frequency.
fn audio_out(cx: &mut Configure<'_>) {
    for o in cx.outputs.iter_mut() {
        o.center_freq = None;
    }
}

pub fn register(r: &mut Registry) {
    let (inputs, outputs) = c32_in_f32_out();
    r.register(
        BlockDescriptor {
            kind: "fm_demod",
            label: "FM Demodulator",
            category: "Demodulators",
            help: "Quadrature FM demodulator: the phase change between samples. ±deviation \
                   maps to ±1. Feed it a channel already filtered to the signal's \
                   bandwidth (e.g. from a frequency-translating FIR).",
            inputs,
            outputs,
            params: vec![
                design::float_min(
                    "deviation",
                    "Deviation",
                    75_000.0,
                    0.0,
                    Some("Hz"),
                    "Peak frequency deviation: 75 kHz for broadcast FM, 5 kHz (or 2.5 kHz) \
                     for narrowband FM.",
                ),
                design::choice(
                    "deemphasis",
                    "De-emphasis",
                    vec!["off", "50us", "75us"],
                    "50us",
                    "Undoes broadcast pre-emphasis: 50 µs in Europe and most of the world, \
                     75 µs in the Americas and Korea. Off for narrowband FM.",
                ),
            ],
        },
        build_fm,
    );
    let (inputs, outputs) = c32_in_f32_out();
    r.register(
        BlockDescriptor {
            kind: "am_demod",
            label: "AM Demodulator",
            category: "Demodulators",
            help: "Envelope detector: the magnitude of the signal, with the carrier's DC \
                   removed so silence is 0.",
            inputs,
            outputs,
            params: vec![design::float_min(
                "dc_time",
                "DC removal",
                0.05,
                0.0,
                Some("s"),
                "Time constant of the DC (carrier) remover. 0 = keep the DC.",
            )],
        },
        build_am,
    );
    let (inputs, outputs) = c32_in_f32_out();
    r.register_with_response(
        BlockDescriptor {
            kind: "ssb_demod",
            label: "SSB Demodulator",
            category: "Demodulators",
            help: "Single-sideband demodulator (Weaver method): keeps the audio band on \
                   one side of 0 Hz and returns it as real audio. Tune the suppressed \
                   carrier to 0 Hz first.",
            inputs,
            outputs,
            params: vec![
                design::choice(
                    "sideband",
                    "Sideband",
                    vec!["usb", "lsb"],
                    "usb",
                    "Upper sideband (above the carrier) or lower sideband.",
                ),
                design::float_min(
                    "low_cut",
                    "Low cut",
                    300.0,
                    0.0,
                    Some("Hz"),
                    "Lowest audio frequency kept.",
                ),
                design::float_min(
                    "high_cut",
                    "High cut",
                    3_000.0,
                    0.0,
                    Some("Hz"),
                    "Highest audio frequency kept.",
                ),
                design::float_min(
                    "transition",
                    "Transition",
                    200.0,
                    0.0,
                    Some("Hz"),
                    "Steepness of the band edges. Narrower = more taps.",
                ),
            ],
        },
        build_ssb,
        ssb_response,
    );
}

// ---- FM ------------------------------------------------------------------------

pub struct FmDemod {
    deviation: f64,
    tau: Option<f64>,
    gain: f32,
    prev: Complex32,
    deemph: Option<OnePole>,
}

impl Block for FmDemod {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if !(self.deviation > 0.0 && self.deviation <= rate / 2.0) {
            return Err(Error::Other(format!(
                "deviation {} must be above 0 Hz and at most {} (half the {} input rate); \
                 raise the sample rate or lower the deviation",
                fmt_hz(self.deviation),
                fmt_hz(rate / 2.0),
                fmt_rate(rate)
            )));
        }
        self.gain = (rate / (TAU * self.deviation)) as f32;
        self.deemph = self.tau.map(|t| OnePole::new(OnePole::alpha_for(t, rate)));
        audio_out(cx);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<Complex32, f32>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let mut prev = self.prev;
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = (x * prev.conj()).arg() * self.gain;
            prev = x;
        }
        self.prev = prev;
        if let Some(d) = &mut self.deemph {
            for o in &mut out[..n] {
                *o = d.next(*o);
            }
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_fm(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let tau = match p.str("deemphasis")? {
        "50us" => Some(50e-6),
        "75us" => Some(75e-6),
        _ => None,
    };
    Ok(instance(Box::new(FmDemod {
        deviation: p.f64("deviation")?,
        tau,
        gain: 1.0,
        prev: Complex32::new(0.0, 0.0),
        deemph: None,
    })))
}

// ---- AM ------------------------------------------------------------------------

pub struct AmDemod {
    dc_time: f64,
    dc: Option<OnePole>,
    started: bool,
}

impl Block for AmDemod {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if self.dc_time > 0.0 {
            let rate = cx.input_rate(0)?;
            self.dc = Some(OnePole::new(OnePole::alpha_for(self.dc_time, rate)));
        }
        audio_out(cx);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<Complex32, f32>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        match &mut self.dc {
            Some(dc) => {
                if !self.started {
                    // Start the DC estimate at the first carrier level so
                    // the output doesn't begin with a long thump.
                    dc.y = inp[0].norm();
                    self.started = true;
                }
                for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
                    let m = x.norm();
                    *o = m - dc.next(m);
                }
            }
            None => {
                for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
                    *o = x.norm();
                }
            }
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_am(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(instance(Box::new(AmDemod {
        dc_time: p.f64("dc_time")?,
        dc: None,
        started: false,
    })))
}

// ---- SSB -----------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct SsbConfig {
    lsb: bool,
    low: f64,
    high: f64,
    transition: f64,
}

impl SsbConfig {
    fn from_params(p: &ParamReader<'_>) -> Result<Self, Error> {
        Ok(Self {
            lsb: p.str("sideband")? == "lsb",
            low: p.f64("low_cut")?,
            high: p.f64("high_cut")?,
            transition: p.f64("transition")?,
        })
    }

    /// The Weaver centre (Hz) and the low-pass that keeps the band around it.
    fn design(&self, rate: f64) -> Result<(f64, Vec<f32>), Error> {
        if self.low >= self.high {
            return Err(Error::Other(format!(
                "the low cut ({}) must be below the high cut ({})",
                fmt_hz(self.low),
                fmt_hz(self.high)
            )));
        }
        if self.high >= rate / 2.0 {
            return Err(Error::Other(format!(
                "high cut {} must be below {} (the Nyquist frequency at {})",
                fmt_hz(self.high),
                fmt_hz(rate / 2.0),
                fmt_rate(rate)
            )));
        }
        let centre = (self.low + self.high) / 2.0;
        let taps = FirSpec {
            band: Band::LowPass((self.high - self.low) / 2.0 / rate),
            transition: self.transition / rate,
            window: DesignWindow::Kaiser { attenuation: 60.0 },
            gain: 1.0,
        }
        .design()
        .map_err(|e| design::design_error(e, rate))?;
        Ok((centre, taps))
    }
}

fn ssb_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let cfg = SsbConfig::from_params(p)?;
    let (centre, taps) = cfg.design(rate)?;
    let shift = if cfg.lsb { -centre } else { centre };
    Ok(design::shifted_response(&taps, rate, shift))
}

/// Complex samples per inner pass; the scratch buffer is this big.
const SSB_CHUNK: usize = 1024;

pub struct SsbDemod {
    cfg: SsbConfig,
    down: Rotator,
    up: Rotator,
    filter: Option<Resampler<Complex32>>,
    scratch: Vec<Complex32>,
}

impl Block for SsbDemod {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let (centre, taps) = self.cfg.design(rate)?;
        self.down.set_freq(-centre / rate);
        self.up.set_freq(centre / rate);
        self.filter = Some(Resampler::fir(&taps));
        self.scratch = vec![Complex32::default(); SSB_CHUNK];
        audio_out(cx);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let filter = self.filter.as_mut().ok_or_else(not_configured)?;
        let (inp, out) = io.in_out::<Complex32, f32>(0, 0);
        let (down, up, lsb) = (&mut self.down, &mut self.up, self.cfg.lsb);
        let (mut used, mut made) = (0, 0);
        loop {
            let room = (out.len() - made).min(SSB_CHUNK);
            let (u, m) = filter.process_with(&inp[used..], &mut self.scratch[..room], |x| {
                // LSB: mirror the spectrum so the wanted band is above 0 Hz.
                down.next(if lsb { x.conj() } else { x })
            });
            for (o, &y) in out[made..made + m].iter_mut().zip(&self.scratch[..m]) {
                *o = up.next(y).re;
            }
            used += u;
            made += m;
            if u == 0 && m == 0 {
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

fn build_ssb(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(instance(Box::new(SsbDemod {
        cfg: SsbConfig::from_params(p)?,
        down: Rotator::new(0.0),
        up: Rotator::new(0.0),
        filter: None,
        scratch: Vec::new(),
    })))
}
