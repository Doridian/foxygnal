//! Type-agnostic stream plumbing: throttle, head, skip, selector, valve,
//! delay, null sink.

use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, TagPropagation, WorkIo, WorkStatus,
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
    register_more(r);
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

/// How much stream time the throttle passes at once. Each batch wakes the
/// blocks around it, so this sets the idle cost: 10 ms is a third of the CPU
/// of 2 ms, and still only 480 items of a 48 kS/s stream.
const THROTTLE_BATCH: Duration = Duration::from_millis(10);

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
        let elapsed = start.elapsed().as_secs_f64();
        let due = (elapsed * self.rate) as u64;
        let allowed = due.saturating_sub(self.passed) as usize;
        // Pass items in batches of THROTTLE_BATCH worth of time (at least one
        // item). Neighbours wake the throttle whenever they produce or
        // consume; passing every item as soon as it is due would make it
        // ping-pong with them at full speed instead of idling.
        let batch = ((self.rate * THROTTLE_BATCH.as_secs_f64()) as usize).max(1);
        if allowed < batch {
            // Sleep until the batch is due (the scheduler's timer wakes us).
            let at = (self.passed + batch as u64) as f64 / self.rate;
            return Ok(WorkStatus::Wait(Duration::from_secs_f64(
                (at - elapsed).max(0.000_1),
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

// ---- Skip, selector, valve, delay -----------------------------------------

/// Inputs the selector's descriptor offers; the `inputs` parameter picks how
/// many of them the block has.
const SELECTOR_PORTS: [&str; 4] = ["in0", "in1", "in2", "in3"];

/// Tag key the selector puts on the first item after a switch (int: the
/// input now selected).
pub const SELECTED_TAG: &str = "selected_input";

fn int_param(
    name: &'static str,
    label: &'static str,
    default: i64,
    (min, max): (i64, Option<i64>),
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Int {
            min: Some(min),
            max,
        },
        default: json!(default),
        help,
        live: false,
    }
}

fn in_port() -> Vec<PortSpec> {
    vec![PortSpec {
        name: "in",
        ty: None,
    }]
}

fn out_port() -> Vec<PortSpec> {
    vec![PortSpec {
        name: "out",
        ty: None,
    }]
}

fn register_more(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "skip",
            label: "Skip",
            category: "Flow",
            help: "Drops the first N items, then passes everything. The rate is unchanged; \
                   tags on dropped items are dropped, later tags move N items earlier.",
            inputs: in_port(),
            outputs: out_port(),
            params: vec![
                params::any_type(),
                int_param(
                    "count",
                    "Count",
                    48_000,
                    (0, None),
                    "Number of items to drop at the start.",
                ),
            ],
        },
        build_skip,
    );
    r.set_keywords("skip", &["skip head", "skiphead", "drop first", "discard"]);
    r.register(
        BlockDescriptor {
            kind: "selector",
            label: "Selector",
            category: "Flow",
            help: "Passes one of several inputs (in0, in1, …) to the output; switch live. \
                   The other inputs are read and discarded so their sources keep running. \
                   The first item after a switch is tagged `selected_input`. The output's \
                   metadata (rate) comes from the input selected at start, so feed inputs \
                   of the same rate.",
            inputs: SELECTOR_PORTS
                .iter()
                .map(|&name| PortSpec { name, ty: None })
                .collect(),
            outputs: out_port(),
            params: vec![
                params::any_type(),
                int_param(
                    "inputs",
                    "Inputs",
                    2,
                    (2, Some(SELECTOR_PORTS.len() as i64)),
                    "How many inputs the block has (connect in0 … in(N−1)).",
                ),
                int_param(
                    "input",
                    "Selected input",
                    0,
                    (0, Some(SELECTOR_PORTS.len() as i64 - 1)),
                    "Index of the input passed to the output (0 = in0).",
                )
                .live(),
            ],
        },
        build_selector,
    );
    r.set_keywords(
        "selector",
        &["switch", "mux", "multiplexer", "choose", "source select"],
    );
    r.register(
        BlockDescriptor {
            kind: "valve",
            label: "Valve",
            category: "Flow",
            help: "Passes the stream while open. While closed it either drops what arrives \
                   (the default: sources upstream keep running) or holds it back, which \
                   back-pressures and stalls everything upstream until it opens again.",
            inputs: in_port(),
            outputs: out_port(),
            params: vec![
                params::any_type(),
                ParamSpec {
                    name: "open",
                    label: "Open",
                    kind: ParamKind::Bool,
                    default: json!(true),
                    help: "Whether the stream passes.",
                    live: true,
                },
                ParamSpec {
                    name: "when_closed",
                    label: "When closed",
                    kind: ParamKind::Enum {
                        options: vec!["drop", "block"],
                    },
                    default: json!("drop"),
                    help: "drop: discard input while closed. block: stop reading, so \
                           upstream waits (a hardware source would overflow).",
                    live: true,
                },
            ],
        },
        build_valve,
    );
    r.set_keywords("valve", &["gate", "mute", "enable", "on off", "pause"]);
    r.register(
        BlockDescriptor {
            kind: "delay",
            label: "Delay",
            category: "Flow",
            help: "Delays the stream by N items: starts with N zeros. Raising the delay \
                   while running inserts zeros; lowering it drops input items. Tags move \
                   with their items.",
            inputs: in_port(),
            outputs: out_port(),
            params: vec![
                params::any_type(),
                int_param("delay", "Delay", 0, (0, None), "Delay in items (samples).").live(),
            ],
        },
        build_delay,
    );
    r.set_keywords("delay", &["latency", "shift", "align", "z^-1"]);
}

/// Copy up to `max` items from input `i` to output 0, forwarding their tags
/// (for blocks with [`TagPropagation::Manual`]). Returns the item count.
fn pass_tagged(io: &mut WorkIo<'_>, i: usize, max: usize) -> usize {
    let inp = &io.inputs[i];
    let out = &mut io.outputs[0];
    let item = inp.stream_type().item_size();
    let n = {
        let (src, dst) = (inp.bytes(), out.bytes_mut());
        let n = (src.len() / item).min(dst.len() / item).min(max);
        dst[..n * item].copy_from_slice(&src[..n * item]);
        n
    };
    for (k, t) in inp.tags_in(n) {
        out.forward_tag(k, t);
    }
    io.consume(i, n);
    io.produce(0, n);
    n
}

/// Readable items on input `i`.
fn available(io: &WorkIo<'_>, i: usize) -> usize {
    io.inputs[i].bytes().len() / io.inputs[i].stream_type().item_size()
}

/// Consume (and drop, with their tags) up to `max` items of input `i`.
fn discard(io: &mut WorkIo<'_>, i: usize, max: usize) -> usize {
    let n = available(io, i).min(max);
    io.consume(i, n);
    n
}

fn progress(moved: usize) -> WorkStatus {
    if moved > 0 {
        WorkStatus::Ok
    } else {
        WorkStatus::Blocked
    }
}

fn clamp_usize(n: u64) -> usize {
    n.min(usize::MAX as u64) as usize
}

pub struct Skip {
    remaining: u64,
}

impl Block for Skip {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut moved = 0;
        if self.remaining > 0 {
            let n = discard(io, 0, clamp_usize(self.remaining));
            self.remaining -= n as u64;
            moved += n;
        }
        if self.remaining == 0 {
            moved += pass_tagged(io, 0, usize::MAX);
        }
        Ok(progress(moved))
    }
}

fn build_skip(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    Ok(Instance {
        block: Box::new(Skip {
            remaining: p.u64("count")?,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct Selector {
    selected: usize,
    inputs: usize,
    /// Tag the next item passed: the selection changed.
    switched: bool,
}

impl Block for Selector {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let m = cx.inputs[self.selected];
        for o in cx.outputs.iter_mut() {
            *o = m;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut moved = 0;
        // Keep the other inputs flowing (dropped, tags and all).
        for i in (0..self.inputs).filter(|&i| i != self.selected) {
            moved += discard(io, i, usize::MAX);
        }
        let sel = self.selected;
        if self.switched && available(io, sel) > 0 && !io.outputs[0].bytes_mut().is_empty() {
            io.outputs[0].add_tag(0, SELECTED_TAG, sel as i64);
            self.switched = false;
        }
        moved += pass_tagged(io, sel, usize::MAX);
        if moved > 0 {
            return Ok(WorkStatus::Ok);
        }
        if io.inputs[sel].exhausted() {
            return Ok(WorkStatus::Done);
        }
        // The runtime finishes a block that is blocked while *any* input has
        // ended, but an unselected input ending must not end the selector.
        // Wait instead: new input or a switch wakes the block before the
        // timer does.
        if io.inputs.iter().any(|i| i.exhausted()) {
            return Ok(WorkStatus::Wait(Duration::from_secs(1)));
        }
        Ok(WorkStatus::Blocked)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "input" => {
                // Out of range keeps the current input: the editor shows the
                // error (it builds every edit) and the stream keeps going.
                if let Some(i) = value.as_u64().map(clamp_usize)
                    && i < self.inputs
                    && i != self.selected
                {
                    self.selected = i;
                    self.switched = true;
                }
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_selector(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let err = |param: &str, msg: String| Error::Param {
        block: "selector".into(),
        param: param.into(),
        msg,
    };
    let inputs = clamp_usize(p.u64("inputs")?);
    if !(2..=SELECTOR_PORTS.len()).contains(&inputs) {
        return Err(err(
            "inputs",
            format!("must be 2 to {}", SELECTOR_PORTS.len()),
        ));
    }
    let selected = clamp_usize(p.u64("input")?);
    if selected >= inputs {
        return Err(err(
            "input",
            format!(
                "input {selected} does not exist: the block has {inputs} inputs (0 to {})",
                inputs - 1
            ),
        ));
    }
    Ok(Instance {
        block: Box::new(Selector {
            selected,
            inputs,
            switched: false,
        }),
        inputs: SELECTOR_PORTS[..inputs]
            .iter()
            .map(|&n| (n.into(), ty))
            .collect(),
        outputs: vec![("out".into(), ty)],
    })
}

pub struct Valve {
    open: bool,
    /// Closed means "stop reading" rather than "drop".
    block_when_closed: bool,
}

impl Block for Valve {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let n = if self.open {
            pass_tagged(io, 0, usize::MAX)
        } else if self.block_when_closed {
            // Opening is a parameter change, which wakes the block.
            0
        } else {
            discard(io, 0, usize::MAX)
        };
        Ok(progress(n))
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "open" => self.open = value.as_bool().unwrap_or(self.open),
            "when_closed" => self.block_when_closed = value.as_str() == Some("block"),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_valve(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    Ok(Instance {
        block: Box::new(Valve {
            open: p.bool("open")?,
            block_when_closed: p.str("when_closed")? == "block",
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct Delay {
    delay: u64,
    /// Zeros still to insert.
    zeros: u64,
    /// Input items still to drop.
    drop: u64,
}

impl Delay {
    fn set_delay(&mut self, new: u64) {
        if new > self.delay {
            // Cancel pending drops first, then add zeros.
            let d = new - self.delay;
            let c = d.min(self.drop);
            self.drop -= c;
            self.zeros += d - c;
        } else {
            let d = self.delay - new;
            let c = d.min(self.zeros);
            self.zeros -= c;
            self.drop += d - c;
        }
        self.delay = new;
    }
}

impl Block for Delay {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut moved = 0;
        if self.zeros > 0 {
            let item = io.outputs[0].stream_type().item_size();
            let out = io.outputs[0].bytes_mut();
            let n = (out.len() / item).min(clamp_usize(self.zeros));
            // All-zero bytes are 0 in every stream type.
            out[..n * item].fill(0);
            io.produce(0, n);
            self.zeros -= n as u64;
            moved += n;
        }
        if self.drop > 0 {
            let n = discard(io, 0, clamp_usize(self.drop));
            self.drop -= n as u64;
            moved += n;
        }
        if self.zeros == 0 && self.drop == 0 {
            moved += pass_tagged(io, 0, usize::MAX);
        }
        Ok(progress(moved))
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "delay" => {
                if let Some(d) = value.as_u64() {
                    self.set_delay(d);
                }
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_delay(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let delay = p.u64("delay")?;
    Ok(Instance {
        block: Box::new(Delay {
            delay,
            zeros: delay,
            drop: 0,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

#[cfg(test)]
mod tests {
    use foxygnal_core::{BufferKind, Flowgraph, Instance, Sample, StreamMeta, Tag, TagValue};
    use serde_json::json;

    use super::*;
    use crate::testing::{VectorSink, VectorSource};

    fn block(kind: &str, p: serde_json::Value) -> Instance {
        crate::registry()
            .build(kind, p.as_object().unwrap())
            .unwrap()
    }

    /// src (tagged) → block → sink on tiny plain buffers (split slices).
    fn run_tagged<T: Sample>(input: Vec<T>, tags: Vec<Tag>, b: Instance) -> (Vec<T>, Vec<Tag>) {
        let mut fg = Flowgraph::new()
            .with_buffer_capacity(7)
            .with_buffer_kind(BufferKind::Plain);
        let src = fg.add("src", VectorSource::with_tags(input, tags));
        let b = fg.add("b", b);
        let (sink, out, got) = VectorSink::<T>::with_tags();
        let sink = fg.add("sink", sink);
        fg.connect(src, 0, b, 0).unwrap();
        fg.connect(b, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        let out = out.lock().unwrap().clone();
        let got = got.lock().unwrap().clone();
        (out, got)
    }

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    fn tag(offset: u64, v: i64) -> Tag {
        Tag::new(offset, "t", v)
    }

    fn offsets(tags: &[Tag]) -> Vec<(u64, TagValue)> {
        tags.iter().map(|t| (t.offset, t.value.clone())).collect()
    }

    #[test]
    fn skip_drops_the_first_items_and_shifts_tags() {
        let tags = vec![tag(2, 0), tag(10, 1), tag(29, 2)];
        let (out, got) = run_tagged(
            ramp(30),
            tags,
            block("skip", json!({"type": "f32", "count": 10})),
        );
        assert_eq!(out, (10..30).map(|i| i as f32).collect::<Vec<_>>());
        assert_eq!(
            offsets(&got),
            vec![(0, TagValue::Int(1)), (19, TagValue::Int(2))]
        );
    }

    #[test]
    fn delay_inserts_zeros_and_shifts_tags() {
        let (out, got) = run_tagged(
            ramp(20),
            vec![tag(0, 0), tag(5, 1)],
            block("delay", json!({"type": "f32", "delay": 9})),
        );
        let mut want = vec![0.0; 9];
        want.extend(ramp(20));
        assert_eq!(out, want);
        assert_eq!(
            offsets(&got),
            vec![(9, TagValue::Int(0)), (14, TagValue::Int(1))]
        );
    }

    #[test]
    fn delay_changes_live() {
        let mut d = Delay {
            delay: 5,
            zeros: 5,
            drop: 0,
        };
        d.set_delay(8);
        assert_eq!((d.zeros, d.drop), (8, 0));
        d.zeros = 0; // as if all were sent
        d.set_delay(2);
        assert_eq!((d.zeros, d.drop), (0, 6));
        d.set_delay(4);
        assert_eq!((d.zeros, d.drop), (0, 4));
        d.set_delay(10);
        assert_eq!((d.zeros, d.drop), (2, 0));
    }

    #[test]
    fn valve_open_passes_closed_drops() {
        let (out, got) = run_tagged(
            ramp(25),
            vec![tag(3, 0)],
            block("valve", json!({"type": "f32"})),
        );
        assert_eq!(out, ramp(25));
        assert_eq!(offsets(&got), vec![(3, TagValue::Int(0))]);
        // Closed (drop): the source drains and the graph ends, nothing out.
        let (out, got) = run_tagged(
            ramp(25),
            vec![tag(3, 0)],
            block("valve", json!({"type": "f32", "open": false})),
        );
        assert!(out.is_empty() && got.is_empty());
    }

    #[test]
    fn valve_block_mode_holds_input_until_opened() {
        let mut fg = Flowgraph::new().with_buffer_capacity(8);
        let src = fg.add("src", VectorSource::instance(ramp(100)));
        let v = fg.add(
            "v",
            block(
                "valve",
                json!({"type": "f32", "open": false, "when_closed": "block"}),
            ),
        );
        let (sink, out) = VectorSink::<f32>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(src, 0, v, 0).unwrap();
        fg.connect(v, 0, sink, 0).unwrap();
        let run = fg.start().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(out.lock().unwrap().is_empty());
        run.param_sender().send("v", "open", json!(true)).unwrap();
        run.wait().unwrap();
        // Nothing was lost while it was closed.
        assert_eq!(*out.lock().unwrap(), ramp(100));
    }

    #[test]
    fn selector_drains_unselected_inputs() {
        // Input 1 is much longer than input 0 and the buffers are tiny: if
        // the selector didn't read it, its source would stall forever.
        let mut fg = Flowgraph::new()
            .with_buffer_capacity(4)
            .with_buffer_kind(BufferKind::Plain);
        let a = fg.add("a", VectorSource::with_tags(ramp(50), vec![tag(1, 7)]));
        let b = fg.add(
            "b",
            VectorSource::with_tags(vec![-1.0f32; 1000], vec![tag(2, 9)]),
        );
        let s = fg.add(
            "s",
            block("selector", json!({"type": "f32", "inputs": 2, "input": 0})),
        );
        let (sink, out, got) = VectorSink::<f32>::with_tags();
        let sink = fg.add("sink", sink);
        fg.connect(a, 0, s, 0).unwrap();
        fg.connect(b, 0, s, 1).unwrap();
        fg.connect(s, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        assert_eq!(*out.lock().unwrap(), ramp(50));
        // Only the selected input's tags come through.
        assert_eq!(offsets(&got.lock().unwrap()), vec![(1, TagValue::Int(7))]);
    }

    #[test]
    fn selector_outlives_an_unselected_input() {
        let mut fg = Flowgraph::new()
            .with_buffer_capacity(4)
            .with_buffer_kind(BufferKind::Plain);
        let a = fg.add("a", VectorSource::instance(vec![5.0f32; 3]));
        let b = fg.add("b", VectorSource::instance(ramp(500)));
        let s = fg.add("s", block("selector", json!({"type": "f32", "input": 1})));
        let (sink, out) = VectorSink::<f32>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(a, 0, s, 0).unwrap();
        fg.connect(b, 0, s, 1).unwrap();
        fg.connect(s, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        assert_eq!(*out.lock().unwrap(), ramp(500));
    }

    #[test]
    fn selector_switch_tags_and_metadata() {
        let mut sel = Selector {
            selected: 0,
            inputs: 3,
            switched: false,
        };
        sel.set_param("input", &json!(2)).unwrap();
        assert!(sel.switched && sel.selected == 2);
        sel.set_param("input", &json!(7)).unwrap(); // out of range: ignored
        assert_eq!(sel.selected, 2);
        let inputs = [
            StreamMeta::with_rate(1.0),
            StreamMeta::with_rate(2.0),
            StreamMeta::with_rate(3.0),
        ];
        let mut outputs = [StreamMeta::merge(&inputs)];
        sel.configure(&mut Configure {
            inputs: &inputs,
            outputs: &mut outputs,
        })
        .unwrap();
        assert_eq!(outputs[0].sample_rate, Some(3.0));
        let reg = crate::registry();
        let p = json!({"inputs": 2, "input": 2});
        let e = reg.build("selector", p.as_object().unwrap()).err().unwrap();
        assert!(e.to_string().contains("does not exist"), "{e}");
    }

    #[test]
    fn selector_type_mismatch_names_both_ends() {
        let mut fg = Flowgraph::new();
        let a = fg.add("a", VectorSource::instance(vec![1u8]));
        let s = fg.add("s", block("selector", json!({"type": "f32"})));
        let e = fg.connect(a, 0, s, 1).err().unwrap().to_string();
        assert!(e.contains("a.out") && e.contains("s.in1"), "{e}");
    }
}
