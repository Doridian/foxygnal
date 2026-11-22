//! Element-wise arithmetic.

use std::marker::PhantomData;
use std::ops::{Add, Mul};

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Error, Instance, ParamReader, PortSpec, Registry, Sample,
    StreamType, WorkIo, WorkStatus,
};

use crate::params;

pub fn register(r: &mut Registry) {
    for (kind, label, help) in [
        ("add", "Add", "Sums two streams sample by sample."),
        (
            "multiply",
            "Multiply",
            "Multiplies two streams sample by sample (mixing).",
        ),
    ] {
        r.register(
            BlockDescriptor {
                kind,
                label,
                category: "Math",
                help,
                inputs: vec![
                    PortSpec {
                        name: "a",
                        ty: None,
                    },
                    PortSpec {
                        name: "b",
                        ty: None,
                    },
                ],
                outputs: vec![PortSpec {
                    name: "out",
                    ty: None,
                }],
                params: vec![params::float_type()],
            },
            if kind == "add" {
                build_binary::<AddOp>
            } else {
                build_binary::<MulOp>
            },
        );
    }
    r.register(
        BlockDescriptor {
            kind: "gain",
            label: "Gain",
            category: "Math",
            help: "Multiplies a stream by a constant.",
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
                params::float("gain", "Gain", 1.0, None, "Linear gain factor."),
            ],
        },
        build_gain,
    );
}

pub trait BinaryOp: Send + 'static {
    fn apply<T: Add<Output = T> + Mul<Output = T>>(a: T, b: T) -> T;
}
pub struct AddOp;
impl BinaryOp for AddOp {
    fn apply<T: Add<Output = T> + Mul<Output = T>>(a: T, b: T) -> T {
        a + b
    }
}
pub struct MulOp;
impl BinaryOp for MulOp {
    fn apply<T: Add<Output = T> + Mul<Output = T>>(a: T, b: T) -> T {
        a * b
    }
}

pub struct Binary<T, Op>(PhantomData<(T, Op)>);

impl<T, Op> Block for Binary<T, Op>
where
    T: Sample + Add<Output = T> + Mul<Output = T>,
    Op: BinaryOp,
{
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
