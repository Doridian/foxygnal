//! Element-wise arithmetic.

use std::marker::PhantomData;
use std::ops::{Add, Div, Mul, Sub};

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Error, Instance, ParamReader, PortSpec, Registry, Sample,
    StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Kernels, kernels};

use crate::params;

/// Samples `log_power` converts per kernel pass (its scratch buffer).
const LOG_CHUNK: usize = 4096;

fn port(name: &'static str, ty: Option<StreamType>) -> PortSpec {
    PortSpec { name, ty }
}

pub fn register(r: &mut Registry) {
    let binary: [(_, _, _, foxygnal_core::registry::Factory); 4] = [
        (
            "add",
            "Add",
            "Sums two streams sample by sample: a + b.",
            build_binary::<AddOp>,
        ),
        (
            "subtract",
            "Subtract",
            "Subtracts stream b from stream a sample by sample: a − b.",
            build_binary::<SubOp>,
        ),
        (
            "multiply",
            "Multiply",
            "Multiplies two streams sample by sample (mixing): a · b.",
            build_binary::<MulOp>,
        ),
        (
            "divide",
            "Divide",
            "Divides stream a by stream b sample by sample: a / b. Dividing by zero gives \
             ±infinity or NaN, as in IEEE floating point.",
            build_binary::<DivOp>,
        ),
    ];
    for (kind, label, help, factory) in binary {
        r.register(
            BlockDescriptor {
                kind,
                label,
                category: "Math",
                help,
                inputs: vec![port("a", None), port("b", None)],
                outputs: vec![port("out", None)],
                params: vec![params::float_type()],
            },
            factory,
        );
    }
    r.set_keywords("subtract", &["minus", "difference", "sub"]);
    r.set_keywords("multiply", &["mixer", "product", "mul"]);
    r.set_keywords("divide", &["quotient", "ratio", "div"]);
    r.register(
        BlockDescriptor {
            kind: "gain",
            label: "Gain",
            category: "Math",
            help: "Multiplies a stream by a constant.",
            inputs: vec![port("in", None)],
            outputs: vec![port("out", None)],
            params: vec![
                params::float_type(),
                params::float("gain", "Gain", 1.0, None, "Linear gain factor.").live(),
            ],
        },
        build_gain,
    );
    r.set_keywords(
        "gain",
        &[
            "multiply const",
            "multiply_const",
            "scale",
            "amplify",
            "attenuate",
        ],
    );
    r.register(
        BlockDescriptor {
            kind: "add_const",
            label: "Add Constant",
            category: "Math",
            help: "Adds a constant to every sample (a DC offset). For complex streams the \
                   constant is real + j·imaginary.",
            inputs: vec![port("in", None)],
            outputs: vec![port("out", None)],
            params: vec![
                params::float_type(),
                params::float(
                    "constant",
                    "Constant",
                    0.0,
                    None,
                    "Value added (real part).",
                )
                .live(),
                params::float(
                    "constant_imag",
                    "Imaginary",
                    0.0,
                    None,
                    "Imaginary part of the constant (complex streams only).",
                )
                .live(),
            ],
        },
        build_add_const,
    );
    r.set_keywords("add_const", &["offset", "bias", "dc", "plus"]);
    r.register(
        BlockDescriptor {
            kind: "conjugate",
            label: "Conjugate",
            category: "Math",
            help: "Complex conjugate: negates the imaginary part, mirroring the spectrum \
                   around 0 Hz.",
            inputs: vec![port("in", Some(StreamType::C32))],
            outputs: vec![port("out", Some(StreamType::C32))],
            params: vec![],
        },
        |_| {
            Ok(Instance {
                block: Box::new(Conjugate),
                inputs: vec![("in".into(), StreamType::C32)],
                outputs: vec![("out".into(), StreamType::C32)],
            })
        },
    );
    r.set_keywords("conjugate", &["conj", "mirror", "flip spectrum", "invert"]);
    r.register(
        BlockDescriptor {
            kind: "abs",
            label: "Absolute Value",
            category: "Math",
            help: "|x| of a real stream (full-wave rectifier). For the magnitude of a \
                   complex stream use Complex → Float (part: magnitude).",
            inputs: vec![port("in", Some(StreamType::F32))],
            outputs: vec![port("out", Some(StreamType::F32))],
            params: vec![],
        },
        |_| {
            Ok(Instance {
                block: Box::new(Abs),
                inputs: vec![("in".into(), StreamType::F32)],
                outputs: vec![("out".into(), StreamType::F32)],
            })
        },
    );
    r.set_keywords("abs", &["absolute", "rectify", "magnitude"]);
    r.register(
        BlockDescriptor {
            kind: "log_power",
            label: "Log Power (dB)",
            category: "Math",
            help: "Power in decibels: 10·log10(|x|²) + offset, never below the floor. A \
                   full-scale sample (|x| = 1) reads 0 dB plus the offset.",
            inputs: vec![port("in", None)],
            outputs: vec![port("out", Some(StreamType::F32))],
            params: vec![
                params::float_type(),
                params::float(
                    "offset",
                    "Offset",
                    0.0,
                    Some("dB"),
                    "Added to every value (calibration, or to reference another level).",
                )
                .live(),
                params::float(
                    "floor",
                    "Floor",
                    -200.0,
                    Some("dB"),
                    "The output never goes below this (applied after the offset). Keeps \
                     silence finite.",
                )
                .live(),
            ],
        },
        build_log_power,
    );
    r.set_keywords(
        "log_power",
        &["decibel", "db", "log10", "power", "rssi", "level"],
    );
}

/// The element types the binary operators run on.
pub trait Arith:
    Sample + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> + Div<Output = Self>
{
}
impl<T> Arith for T where
    T: Sample + Add<Output = T> + Sub<Output = T> + Mul<Output = T> + Div<Output = T>
{
}

pub trait BinaryOp: Send + 'static {
    fn apply<T: Arith>(a: T, b: T) -> T;
}
pub struct AddOp;
impl BinaryOp for AddOp {
    fn apply<T: Arith>(a: T, b: T) -> T {
        a + b
    }
}
pub struct SubOp;
impl BinaryOp for SubOp {
    fn apply<T: Arith>(a: T, b: T) -> T {
        a - b
    }
}
pub struct MulOp;
impl BinaryOp for MulOp {
    fn apply<T: Arith>(a: T, b: T) -> T {
        a * b
    }
}
pub struct DivOp;
impl BinaryOp for DivOp {
    fn apply<T: Arith>(a: T, b: T) -> T {
        a / b
    }
}

pub struct Binary<T, Op>(PhantomData<(T, Op)>);

impl<T: Arith, Op: BinaryOp> Block for Binary<T, Op> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let a = io.inputs[0].slice::<T>();
        let b = io.inputs[1].slice::<T>();
        let out = io.outputs[0].slice_mut::<T>();
        let n = a.len().min(b.len()).min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for ((o, &x), &y) in out[..n].iter_mut().zip(&a[..n]).zip(&b[..n]) {
            *o = Op::apply(x, y);
        }
        io.consume(0, n);
        io.consume(1, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_binary<Op: BinaryOp>(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let block: Box<dyn Block> = match ty {
        StreamType::C32 => Box::new(Binary::<Complex32, Op>(PhantomData)),
        _ => Box::new(Binary::<f32, Op>(PhantomData)),
    };
    Ok(Instance {
        block,
        inputs: vec![("a".into(), ty), ("b".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

/// A sample-by-sample function for a 1-in/1-out block of type `T`.
trait Elementwise: Send + 'static {
    type T: Sample;
    fn apply(&self, x: Self::T) -> Self::T;
}

struct Pointwise<E>(E);

impl<E: Elementwise> Pointwise<E> {
    fn run(&self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<E::T, E::T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = self.0.apply(x);
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

pub struct Gain<T> {
    gain: f32,
    _t: PhantomData<T>,
}

impl<T: Sample + Mul<f32, Output = T>> Block for Gain<T> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = x * self.gain;
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "gain" => self.gain = value.as_f64().unwrap_or_default() as f32,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_gain(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let gain = p.f64("gain")? as f32;
    let block: Box<dyn Block> = match ty {
        StreamType::C32 => Box::new(Gain::<Complex32> {
            gain,
            _t: PhantomData,
        }),
        _ => Box::new(Gain::<f32> {
            gain,
            _t: PhantomData,
        }),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

/// A constant offset; for f32 streams only the real part is used.
pub struct AddConst<T> {
    re: f32,
    im: f32,
    _t: PhantomData<T>,
}

/// How [`AddConst`] adds its constant to each sample type.
pub trait Offset: Sample {
    fn offset(self, re: f32, im: f32) -> Self;
}
impl Offset for f32 {
    fn offset(self, re: f32, _: f32) -> Self {
        self + re
    }
}
impl Offset for Complex32 {
    fn offset(self, re: f32, im: f32) -> Self {
        Complex32::new(self.re + re, self.im + im)
    }
}

struct AddConstOp<T> {
    re: f32,
    im: f32,
    _t: PhantomData<T>,
}
impl<T: Offset> Elementwise for AddConstOp<T> {
    type T = T;
    fn apply(&self, x: T) -> T {
        x.offset(self.re, self.im)
    }
}

impl<T: Offset> Block for AddConst<T> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        Pointwise(AddConstOp::<T> {
            re: self.re,
            im: self.im,
            _t: PhantomData,
        })
        .run(io)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default() as f32;
        match name {
            "constant" => self.re = v,
            "constant_imag" => self.im = v,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_add_const(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let (re, im) = (p.f64("constant")? as f32, p.f64("constant_imag")? as f32);
    let block: Box<dyn Block> = match ty {
        StreamType::C32 => Box::new(AddConst::<Complex32> {
            re,
            im,
            _t: PhantomData,
        }),
        _ => Box::new(AddConst::<f32> {
            re,
            im,
            _t: PhantomData,
        }),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

struct ConjOp;
impl Elementwise for ConjOp {
    type T = Complex32;
    fn apply(&self, x: Complex32) -> Complex32 {
        x.conj()
    }
}

pub struct Conjugate;
impl Block for Conjugate {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        Pointwise(ConjOp).run(io)
    }
}

struct AbsOp;
impl Elementwise for AbsOp {
    type T = f32;
    fn apply(&self, x: f32) -> f32 {
        x.abs()
    }
}

pub struct Abs;
impl Block for Abs {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        Pointwise(AbsOp).run(io)
    }
}

/// `|x|²` of a chunk into `y`, by sample type.
pub trait Power: Sample {
    fn power(k: &Kernels, x: &[Self], y: &mut [f32]);
}
impl Power for f32 {
    fn power(_: &Kernels, x: &[f32], y: &mut [f32]) {
        for (y, &x) in y.iter_mut().zip(x) {
            *y = x * x;
        }
    }
}
impl Power for Complex32 {
    fn power(k: &Kernels, x: &[Complex32], y: &mut [f32]) {
        (k.power)(x, 1.0, y);
    }
}

pub struct LogPower<T> {
    offset: f32,
    floor: f32,
    k: &'static Kernels,
    /// |x|², [`LOG_CHUNK`] floats.
    scratch: Vec<f32>,
    _t: PhantomData<T>,
}

impl<T: Power> Block for LogPower<T> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, f32>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let (offset, floor) = (self.offset, self.floor);
        for (x, y) in inp[..n]
            .chunks(LOG_CHUNK)
            .zip(out[..n].chunks_mut(LOG_CHUNK))
        {
            let p = &mut self.scratch[..x.len()];
            T::power(self.k, x, p);
            (self.k.power_to_db)(p, y);
            for v in y.iter_mut() {
                *v = (*v + offset).max(floor);
            }
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default() as f32;
        match name {
            "offset" => self.offset = v,
            "floor" => self.floor = v,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_log_power(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let (offset, floor) = (p.f64("offset")? as f32, p.f64("floor")? as f32);
    fn make<T: Power>(offset: f32, floor: f32) -> Box<dyn Block> {
        Box::new(LogPower::<T> {
            offset,
            floor,
            k: kernels(),
            scratch: vec![0.0; LOG_CHUNK],
            _t: PhantomData,
        })
    }
    let block = match ty {
        StreamType::C32 => make::<Complex32>(offset, floor),
        _ => make::<f32>(offset, floor),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), StreamType::F32)],
    })
}

#[cfg(test)]
mod tests {
    use foxygnal_core::{BufferKind, Flowgraph, Params};
    use serde_json::json;

    use super::*;
    use crate::testing::{VectorSink, VectorSource};

    /// One-input block on tiny plain buffers (split slices at the wrap) and
    /// on default ones; both must agree.
    fn run1<I: Sample, O: Sample + PartialEq + std::fmt::Debug>(
        kind: &str,
        params: serde_json::Value,
        input: Vec<I>,
    ) -> Vec<O> {
        let go = |kind_buf: Option<BufferKind>| {
            let mut fg = Flowgraph::new();
            if let Some(b) = kind_buf {
                fg = fg.with_buffer_capacity(5).with_buffer_kind(b);
            }
            let src = fg.add("src", VectorSource::instance(input.clone()));
            let b = fg.add(
                "b",
                crate::registry()
                    .build(kind, params.as_object().unwrap())
                    .unwrap(),
            );
            let (sink, out) = VectorSink::<O>::instance();
            let sink = fg.add("sink", sink);
            fg.connect(src, 0, b, 0).unwrap();
            fg.connect(b, 0, sink, 0).unwrap();
            fg.start().unwrap().wait().unwrap();
            out.lock().unwrap().clone()
        };
        let plain = go(Some(BufferKind::Plain));
        let auto = go(None);
        assert_eq!(plain, auto);
        auto
    }

    fn run2<T: Sample>(kind: &str, ty: &str, a: Vec<T>, b: Vec<T>) -> Vec<T> {
        let mut fg = Flowgraph::new()
            .with_buffer_capacity(3)
            .with_buffer_kind(BufferKind::Plain);
        let sa = fg.add("a", VectorSource::instance(a));
        let sb = fg.add("b", VectorSource::instance(b));
        let mut p = Params::new();
        p.insert("type".into(), json!(ty));
        let op = fg.add("op", crate::registry().build(kind, &p).unwrap());
        let (sink, out) = VectorSink::<T>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(sa, 0, op, 0).unwrap();
        fg.connect(sb, 0, op, 1).unwrap();
        fg.connect(op, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        out.lock().unwrap().clone()
    }

    #[test]
    fn subtract_and_divide() {
        let a = vec![6.0f32, 1.0, -4.0, 9.0, 2.0];
        let b = vec![2.0f32, 4.0, 2.0, 3.0];
        assert_eq!(
            run2("subtract", "f32", a.clone(), b.clone()),
            vec![4.0, -3.0, -6.0, 6.0]
        );
        assert_eq!(run2("divide", "f32", a, b), vec![3.0, 0.25, -2.0, 3.0]);
        let c = |re, im| Complex32::new(re, im);
        let a = vec![c(1.0, 2.0), c(0.0, 4.0)];
        let b = vec![c(1.0, 1.0), c(0.0, 2.0)];
        assert_eq!(
            run2("subtract", "c32", a.clone(), b.clone()),
            vec![c(0.0, 1.0), c(0.0, 2.0)]
        );
        let q = run2("divide", "c32", a, b);
        assert!((q[0] - c(1.5, 0.5)).norm() < 1e-6 && (q[1] - c(2.0, 0.0)).norm() < 1e-6);
    }

    #[test]
    fn conjugate_abs_add_const() {
        let c = |re, im| Complex32::new(re, im);
        let x = vec![c(1.0, 2.0), c(-3.0, -4.0), c(0.0, 1.0)];
        let y: Vec<Complex32> = run1("conjugate", json!({}), x);
        assert_eq!(y, vec![c(1.0, -2.0), c(-3.0, 4.0), c(0.0, -1.0)]);
        let y: Vec<f32> = run1("abs", json!({}), vec![-1.5f32, 2.0, -0.0, -7.0]);
        assert_eq!(y, vec![1.5, 2.0, 0.0, 7.0]);
        let y: Vec<f32> = run1(
            "add_const",
            json!({"type": "f32", "constant": 0.5, "constant_imag": 9.0}),
            vec![1.0f32, -1.0],
        );
        assert_eq!(y, vec![1.5, -0.5]);
        let y: Vec<Complex32> = run1(
            "add_const",
            json!({"constant": 1.0, "constant_imag": -1.0}),
            vec![c(1.0, 1.0)],
        );
        assert_eq!(y, vec![c(2.0, 0.0)]);
    }

    #[test]
    fn log_power_db() {
        let x: Vec<f32> = (0..20).map(|i| [1.0, -0.1, 0.0, 10.0][i % 4]).collect();
        let y: Vec<f32> = run1("log_power", json!({"type": "f32", "floor": -120.0}), x);
        for (i, &v) in y.iter().enumerate() {
            let want = [0.0, -20.0, -120.0, 20.0][i % 4];
            assert!((v - want).abs() < 1e-3, "{i}: {v} vs {want}");
        }
        let c = |re, im| Complex32::new(re, im);
        let y: Vec<f32> = run1(
            "log_power",
            json!({"offset": 3.0}),
            vec![c(0.6, 0.8), c(0.0, 0.1)],
        );
        assert!(
            (y[0] - 3.0).abs() < 1e-3 && (y[1] + 17.0).abs() < 1e-3,
            "{y:?}"
        );
    }

    #[test]
    fn search_keywords_are_served() {
        let reg = crate::registry();
        assert!(reg.keywords("gain").contains(&"multiply_const"));
        let cat = serde_json::to_value(reg.catalog().collect::<Vec<_>>()).unwrap();
        let conj = cat
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == "conjugate")
            .unwrap();
        assert_eq!(conj["keywords"][0], "conj");
    }
}
