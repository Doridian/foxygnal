//! Level control: automatic gain control and a power squelch.

use std::marker::PhantomData;
use std::ops::Mul;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    Sample, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::OnePole;

use crate::design;
use crate::params;

fn ports() -> (Vec<PortSpec>, Vec<PortSpec>) {
    (
        vec![PortSpec {
            name: "in",
            ty: None,
        }],
        vec![PortSpec {
            name: "out",
            ty: None,
        }],
    )
}

pub fn register(r: &mut Registry) {
    let (inputs, outputs) = ports();
    r.register(
        BlockDescriptor {
            kind: "agc",
            label: "AGC",
            category: "Level",
            help: "Automatic gain control: follows the signal's peak level (fast attack, slow \
                   decay) and scales it so the peaks sit at the target.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                design::float_min(
                    "target",
                    "Target",
                    1.0,
                    0.0,
                    None,
                    "Output peak level (amplitude, linear).",
                )
                .live(),
                design::float_min(
                    "attack",
                    "Attack",
                    0.001,
                    0.0,
                    Some("s"),
                    "How fast the gain drops when the signal gets louder (time constant).",
                )
                .live(),
                design::float_min(
                    "decay",
                    "Decay",
                    0.1,
                    0.0,
                    Some("s"),
                    "How fast the gain recovers when the signal gets quieter (time constant).",
                )
                .live(),
                design::float_min(
                    "max_gain",
                    "Max gain",
                    1e5,
                    0.0,
                    None,
                    "Upper limit on the gain (linear), so silence isn't amplified into \
                     roaring noise.",
                )
                .live(),
            ],
        },
        build_agc,
    );
    let (inputs, outputs) = ports();
    r.register(
        BlockDescriptor {
            kind: "squelch",
            label: "Power Squelch",
            category: "Level",
            help: "Mutes the stream (outputs zeros, never drops samples) while its average \
                   power is below the threshold. Opens at the threshold and closes \
                   `hysteresis` dB below it, so it doesn't chatter.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                params::float(
                    "threshold",
                    "Threshold",
                    -40.0,
                    Some("dB"),
                    "Opens when the average power rises above this (dB relative to a \
                     full-scale signal of amplitude 1).",
                )
                .live(),
                design::float_min(
                    "hysteresis",
                    "Hysteresis",
                    3.0,
                    0.0,
                    Some("dB"),
                    "Closes this far below the threshold.",
                )
                .live(),
                design::float_min(
                    "averaging",
                    "Averaging",
                    0.01,
                    0.0,
                    Some("s"),
                    "Time constant of the power average (the squelch's attack).",
                ),
                design::float_min(
                    "ramp",
                    "Ramp",
                    0.002,
                    0.0,
                    Some("s"),
                    "Fade in/out time when opening and closing, to avoid clicks. 0 = hard \
                     switch.",
                ),
            ],
        },
        build_squelch,
    );
}

/// Sample types the level blocks work on.
pub trait Level: Sample + Mul<f32, Output = Self> + Default {
    fn mag_sqr(self) -> f32;
}
impl Level for f32 {
    fn mag_sqr(self) -> f32 {
        self * self
    }
}
impl Level for Complex32 {
    fn mag_sqr(self) -> f32 {
        self.norm_sqr()
    }
}

// ---- AGC -----------------------------------------------------------------------

pub struct Agc<T> {
    target: f32,
    attack_s: f64,
    decay_s: f64,
    max_gain: f32,
    rate: f64,
    attack: f32,
    decay: f32,
    /// Peak level estimate of the input.
    level: f32,
    _t: PhantomData<T>,
}

impl<T: Level> Agc<T> {
    fn update_rates(&mut self) {
        self.attack = OnePole::alpha_for(self.attack_s, self.rate);
        self.decay = OnePole::alpha_for(self.decay_s, self.rate);
    }
}

impl<T: Level> Block for Agc<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.input_rate(0)?;
        self.update_rates();
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let mut level = self.level;
        if level == 0.0 {
            // Start at the first sample's level instead of blasting it at
            // max gain while the estimate rises from zero.
            level = inp[0].mag_sqr().sqrt();
        }
        let floor = self.target / self.max_gain.max(f32::MIN_POSITIVE);
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            let m = x.mag_sqr().sqrt();
            let a = if m > level { self.attack } else { self.decay };
            level += a * (m - level);
            *o = x * (self.target / level.max(floor));
        }
        self.level = level;
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        match name {
            "target" => self.target = v as f32,
            "attack" => self.attack_s = v,
            "decay" => self.decay_s = v,
            "max_gain" => self.max_gain = v as f32,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        self.update_rates();
        Ok(())
    }
}

fn build_agc(p: &ParamReader<'_>) -> Result<Instance, Error> {
    fn make<T: Level>(p: &ParamReader<'_>) -> Result<Box<dyn Block>, Error> {
        Ok(Box::new(Agc::<T> {
            target: p.f64("target")? as f32,
            attack_s: p.f64("attack")?,
            decay_s: p.f64("decay")?,
            max_gain: p.f64("max_gain")? as f32,
            rate: 1.0,
            attack: 1.0,
            decay: 1.0,
            level: 0.0,
            _t: PhantomData,
        }))
    }
    let ty = p.stream_type("type")?;
    let block = match ty {
        StreamType::C32 => make::<Complex32>(p)?,
        _ => make::<f32>(p)?,
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

// ---- Squelch -------------------------------------------------------------------

pub struct Squelch<T> {
    threshold_db: f64,
    hysteresis_db: f64,
    averaging: f64,
    ramp: f64,
    /// Linear power thresholds.
    open_at: f32,
    close_at: f32,
    power: OnePole,
    /// Gain change per sample while ramping.
    step: f32,
    gain: f32,
    open: bool,
    _t: PhantomData<T>,
}

impl<T: Level> Squelch<T> {
    fn update_thresholds(&mut self) {
        self.open_at = 10f64.powf(self.threshold_db / 10.0) as f32;
        self.close_at = 10f64.powf((self.threshold_db - self.hysteresis_db) / 10.0) as f32;
    }
}

impl<T: Level> Block for Squelch<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        self.power = OnePole::new(OnePole::alpha_for(self.averaging, rate));
        self.step = if self.ramp > 0.0 {
            (1.0 / (self.ramp * rate)).min(1.0) as f32
        } else {
            1.0
        };
        self.update_thresholds();
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let (mut open, mut gain) = (self.open, self.gain);
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            let p = self.power.next(x.mag_sqr());
            if open {
                open = p >= self.close_at;
            } else {
                open = p > self.open_at;
            }
            gain = if open {
                (gain + self.step).min(1.0)
            } else {
                (gain - self.step).max(0.0)
            };
            *o = if gain == 0.0 { T::default() } else { x * gain };
        }
        (self.open, self.gain) = (open, gain);
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        match name {
            "threshold" => self.threshold_db = v,
            "hysteresis" => self.hysteresis_db = v,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        self.update_thresholds();
        Ok(())
    }
}

fn build_squelch(p: &ParamReader<'_>) -> Result<Instance, Error> {
    fn make<T: Level>(p: &ParamReader<'_>) -> Result<Box<dyn Block>, Error> {
        Ok(Box::new(Squelch::<T> {
            threshold_db: p.f64("threshold")?,
            hysteresis_db: p.f64("hysteresis")?,
            averaging: p.f64("averaging")?,
            ramp: p.f64("ramp")?,
            open_at: 0.0,
            close_at: 0.0,
            power: OnePole::new(1.0),
            step: 1.0,
            gain: 0.0,
            open: false,
            _t: PhantomData,
        }))
    }
    let ty = p.stream_type("type")?;
    let block = match ty {
        StreamType::C32 => make::<Complex32>(p)?,
        _ => make::<f32>(p)?,
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}
