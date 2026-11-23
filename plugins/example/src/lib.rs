//! Example foxygnal plugin: a gain and a moving average, both with a live
//! parameter. Kinds in the engine: `example:gain`, `example:moving_average`.

use std::marker::PhantomData;
use std::ops::{Add, Mul, Sub};

use foxygnal_plugin_sdk::*;

struct Example;

impl Plugin for Example {
    fn info() -> PluginInfo {
        let types = [StreamType::F32, StreamType::C32];
        PluginInfo {
            name: "example".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            blocks: vec![
                BlockDescriptor {
                    kind: "gain".into(),
                    label: "Gain (plugin)".into(),
                    category: "Math".into(),
                    help: "Multiplies a stream by a constant. The plugin twin of the built-in \
                           gain, for comparison."
                        .into(),
                    inputs: vec![PortSpec::any("in")],
                    outputs: vec![PortSpec::any("out")],
                    params: vec![
                        ParamSpec::stream_type(&types, StreamType::F32),
                        ParamSpec::float("gain", "Gain", 1.0, "Linear gain factor.").live(),
                    ],
                    tags: TagMode::AllToAll,
                },
                BlockDescriptor {
                    kind: "moving_average".into(),
                    label: "Moving average".into(),
                    category: "Filters".into(),
                    help: "Mean of the last `length` samples (a boxcar low-pass).".into(),
                    inputs: vec![PortSpec::any("in")],
                    outputs: vec![PortSpec::any("out")],
                    params: vec![
                        ParamSpec::stream_type(&types, StreamType::F32),
                        ParamSpec::int("length", "Length", 16, "Samples averaged.")
                            .range(Some(1.0), Some(65536.0))
                            .live(),
                    ],
                    tags: TagMode::AllToAll,
                },
            ],
        }
    }

    fn create(kind: &str, params: &Params) -> Result<Created, String> {
        let ty = params.stream_type("type")?;
        let block: Box<dyn Block> = match (kind, ty) {
            ("gain", StreamType::F32) => Box::new(Gain::<f32>::new(params)?),
            ("gain", StreamType::C32) => Box::new(Gain::<Complex32>::new(params)?),
            ("moving_average", StreamType::F32) => Box::new(MovingAverage::<f32>::new(params)?),
            ("moving_average", StreamType::C32) => {
                Box::new(MovingAverage::<Complex32>::new(params)?)
            }
            _ => return Err(format!("`{kind}` doesn't support {ty:?}")),
        };
        Ok(Created {
            block,
            inputs: vec![ty],
            outputs: vec![ty],
        })
    }
}

export_plugin!(Example);

struct Gain<T> {
    gain: f32,
    _t: PhantomData<T>,
}

impl<T> Gain<T> {
    fn new(p: &Params) -> Result<Self, String> {
        Ok(Self {
            gain: p.f64("gain")? as f32,
            _t: PhantomData,
        })
    }
}

impl<T: Sample + Mul<f32, Output = T>> Block for Gain<T> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let g = self.gain;
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = x * g;
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &ParamValue) -> Result<(), String> {
        match (name, value.as_f64()) {
            ("gain", Some(g)) => {
                self.gain = g as f32;
                Ok(())
            }
            _ => Err(format!("parameter `{name}` cannot change while running")),
        }
    }
}

struct MovingAverage<T> {
    /// The last `len` inputs, as a ring.
    history: Vec<T>,
    pos: usize,
    sum: T,
    /// Steps since the sum was last recomputed (limits rounding drift).
    since_exact: usize,
}

impl<T: Sample + Add<Output = T> + Sub<Output = T> + Mul<f32, Output = T>> MovingAverage<T> {
    fn new(p: &Params) -> Result<Self, String> {
        let len = p.i64("length")?.clamp(1, 65536) as usize;
        Ok(Self {
            history: vec![T::default(); len],
            pos: 0,
            sum: T::default(),
            since_exact: 0,
        })
    }
}

impl<T: Sample + Add<Output = T> + Sub<Output = T> + Mul<f32, Output = T>> Block
    for MovingAverage<T>
{
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let len = self.history.len();
        let scale = 1.0 / len as f32;
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            let old = std::mem::replace(&mut self.history[self.pos], x);
            self.sum = self.sum + x - old;
            self.pos += 1;
            if self.pos == len {
                self.pos = 0;
            }
            *o = self.sum * scale;
        }
        self.since_exact += n;
        if self.since_exact >= 1 << 16 {
            self.since_exact = 0;
            self.sum = self.history.iter().fold(T::default(), |acc, &v| acc + v);
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &ParamValue) -> Result<(), String> {
        match (name, value.as_i64()) {
            ("length", Some(len)) => {
                // Start over with the new length: an empty (zero) history.
                let len = len.clamp(1, 65536) as usize;
                self.history.clear();
                self.history.resize(len, T::default());
                self.pos = 0;
                self.sum = T::default();
                Ok(())
            }
            _ => Err(format!("parameter `{name}` cannot change while running")),
        }
    }
}
