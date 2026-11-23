//! Type-agnostic stream plumbing: throttle, head, null sink.

use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, WorkIo, WorkStatus,
};
use serde_json::json;

use crate::params;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "throttle",
            label: "Throttle",
            category: "Flow",
            help: "Limits throughput to a fixed rate. Use it when no hardware sets the pace \
                   (generators, files) so the graph doesn't spin at 100% CPU.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::any_type(),
                params::optional_rate(
                    "Items per second. 0 = the stream's own rate, from upstream.",
                ),
            ],
        },
        build_throttle,
    );
    r.register(
        BlockDescriptor {
            kind: "head",
            label: "Head",
            category: "Flow",
            help: "Passes the first N items, then ends the stream.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::any_type(),
                ParamSpec {
                    name: "count",
                    label: "Count",
                    kind: ParamKind::Int {
                        min: Some(0),
                        max: None,
                    },
                    default: json!(48_000),
                    help: "Number of items to pass.",
                    live: false,
                },
            ],
        },
        build_head,
    );
    r.register(
        BlockDescriptor {
            kind: "null_sink",
            label: "Null Sink",
            category: "Sinks",
            help: "Discards everything. Handy for terminating a branch.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![params::any_type()],
        },
        build_null,
    );
}

/// Copy as many whole items as possible from input 0 to output 0, up to `max`.
fn pass_through(io: &mut WorkIo<'_>, max: usize) -> usize {
    let item = io.inputs[0].stream_type().item_size();
    let inp = io.inputs[0].bytes();
    let out = io.outputs[0].bytes_mut();
    let n = (inp.len() / item).min(out.len() / item).min(max);
    out[..n * item].copy_from_slice(&inp[..n * item]);
    io.consume(0, n);
    io.produce(0, n);
    n
}

pub struct Throttle {
    rate: f64,
    start: Option<Instant>,
    passed: u64,
}

impl Block for Throttle {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if self.rate > 0.0 {
            // A raw source may not know its rate: the throttle tells downstream.
            if cx.inputs[0].sample_rate.is_none() {
                cx.set_rate(self.rate);
            }
        } else {
            self.rate = cx.input_rate(0)?;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let start = *self.start.get_or_insert_with(Instant::now);
        let due = (start.elapsed().as_secs_f64() * self.rate) as u64;
        let allowed = due.saturating_sub(self.passed) as usize;
        if allowed == 0 {
            // Park until an item is due: at least 1 ms, so slow rates pass
            // items in small batches instead of waking per item, and at most
            // 10 ms to stay responsive.
            return Ok(WorkStatus::Wait(Duration::from_secs_f64(
                (1.0 / self.rate).clamp(0.001, 0.01),
            )));
        }
        let n = pass_through(io, allowed);
        self.passed += n as u64;
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
}

fn build_throttle(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    Ok(Instance {
        block: Box::new(Throttle {
            rate: p.f64("sample_rate")?,
            start: None,
            passed: 0,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct Head {
    remaining: u64,
}

impl Block for Head {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.remaining == 0 {
            return Ok(WorkStatus::Done);
        }
        let n = pass_through(io, self.remaining.min(usize::MAX as u64) as usize);
        self.remaining -= n as u64;
        Ok(match (n, self.remaining) {
            (_, 0) => WorkStatus::Done,
            (0, _) => WorkStatus::Blocked,
            _ => WorkStatus::Ok,
        })
    }
}

fn build_head(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    Ok(Instance {
        block: Box::new(Head {
            remaining: p.u64("count")?,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct NullSink;

impl Block for NullSink {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let item = io.inputs[0].stream_type().item_size();
        let n = io.inputs[0].bytes().len() / item;
        io.consume(0, n);
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
}

fn build_null(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(NullSink),
        inputs: vec![("in".into(), p.stream_type("type")?)],
        outputs: vec![],
    })
}
