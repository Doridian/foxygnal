//! Control inputs (a setting wired as an input, `block.@param`) and constant
//! inputs (`const_inputs`, a hidden constant source per input).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::resolve::{self, diff_params};
use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Error, Flowgraph, GraphSpec, Instance, NoLoader, ParamKind,
    ParamSpec, Params, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

fn params(v: serde_json::Value) -> Params {
    v.as_object().unwrap().clone()
}

/// Emits `chunks` chunks of `chunk` ones, one chunk per millisecond, then
/// ends: a stream slow enough for control values to land in the middle.
struct Paced {
    chunk: usize,
    left: usize,
    next: Option<Instant>,
}

impl Block for Paced {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.left == 0 {
            return Ok(WorkStatus::Done);
        }
        // Neighbours may wake us early: keep to the clock.
        let now = Instant::now();
        let next = *self.next.get_or_insert(now);
        if now < next {
            return Ok(WorkStatus::Wait(next - now));
        }
        self.next = Some(next + Duration::from_millis(1));
        let out = io.outputs[0].slice_mut::<f32>();
        if out.len() < self.chunk {
            return Ok(WorkStatus::Blocked);
        }
        out[..self.chunk].fill(1.0);
        io.produce(0, self.chunk);
        self.left -= 1;
        Ok(WorkStatus::Wait(Duration::from_millis(1)))
    }
}

fn paced(chunk: usize, chunks: usize) -> Instance {
    Instance {
        block: Box::new(Paced {
            chunk,
            left: chunks,
            next: None,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::F32)],
    }
}

/// Emits `first`, waits until `seen` reaches `after`, emits `then` and ends.
struct Stepper {
    first: f32,
    then: f32,
    after: usize,
    seen: Arc<AtomicUsize>,
    stage: u8,
}

impl Block for Stepper {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let v = match self.stage {
            0 => self.first,
            1 if self.seen.load(Ordering::Relaxed) >= self.after => self.then,
            1 => return Ok(WorkStatus::Wait(Duration::from_millis(1))),
            _ => return Ok(WorkStatus::Done),
        };
        let out = io.outputs[0].slice_mut::<f32>();
        if out.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        out[0] = v;
        io.produce(0, 1);
        self.stage += 1;
        Ok(WorkStatus::Ok)
    }
}

/// Counts what it receives (for [`Stepper`]) and keeps it.
struct CountingSink {
    seen: Arc<AtomicUsize>,
    data: Arc<std::sync::Mutex<Vec<f32>>>,
}

impl Block for CountingSink {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<f32>();
        if inp.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        let n = inp.len();
        self.data.lock().unwrap().extend_from_slice(inp);
        self.seen.fetch_add(n, Ordering::Relaxed);
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }
}

/// Timing: a control value applies before the next `work` call of the
/// block it controls, so the step lands within a `work` call's worth of
/// samples of when it arrived (sample and hold per call).
#[test]
fn a_wired_gain_changes_the_stream_mid_way_and_holds_after_the_control_ends() {
    let reg = foxygnal_blocks::registry();
    let (chunk, chunks) = (100, 400);
    let total = chunk * chunks;
    let seen = Arc::new(AtomicUsize::new(0));
    let data = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut fg = Flowgraph::new();
    let src = fg.add("src", paced(chunk, chunks));
    let ctl = fg.add(
        "ctl",
        Instance {
            block: Box::new(Stepper {
                first: 2.0,
                then: 5.0,
                after: total / 2,
                seen: seen.clone(),
                stage: 0,
            }),
            inputs: vec![],
            outputs: vec![("out".into(), StreamType::F32)],
        },
    );
    let gain = fg.add(
        "gain",
        reg.build("gain", &params(json!({"type": "f32"}))).unwrap(),
    );
    let sink = fg.add(
        "sink",
        Instance {
            block: Box::new(CountingSink {
                seen: seen.clone(),
                data: data.clone(),
            }),
            inputs: vec![("in".into(), StreamType::F32)],
            outputs: vec![],
        },
    );
    let spec = reg.get("gain").unwrap().params[1].clone();
    assert_eq!(spec.name, "gain");
    fg.connect(src, 0, gain, 0).unwrap();
    fg.connect_param(ctl, 0, gain, &spec).unwrap();
    fg.connect(gain, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();

    let out = data.lock().unwrap();
    assert_eq!(out.len(), total, "control inputs never end the block early");
    // 1 (before the first control value) → 2 → 5, never back.
    let rank = |v: f32| match v {
        1.0 => 0,
        2.0 => 1,
        5.0 => 2,
        v => panic!("unexpected value {v}"),
    };
    assert!(out.windows(2).all(|w| rank(w[0]) <= rank(w[1])));
    let first5 = out.iter().position(|&v| v == 5.0).expect("the step to 5");
    assert!(first5 >= total / 2, "applied before it was sent: {first5}");
    assert!(
        first5 < total / 2 + 20 * chunk,
        "applied late: sample {first5} of {total}"
    );
    // The producer ended after sending 5: the gain stays at 5.
    assert_eq!(*out.last().unwrap(), 5.0);
}

/// A control source that never produces anything.
struct Silent;

impl Block for Silent {
    fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        Ok(WorkStatus::Blocked)
    }
}

#[test]
fn an_idle_control_input_neither_blocks_nor_ends_the_block() {
    let reg = foxygnal_blocks::registry();
    let n = 50_000;
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(vec![1.0f32; n]));
    let ctl = fg.add(
        "ctl",
        Instance {
            block: Box::new(Silent),
            inputs: vec![],
            outputs: vec![("out".into(), StreamType::F32)],
        },
    );
    let gain = fg.add(
        "gain",
        reg.build("gain", &params(json!({"type": "f32", "gain": 3.0})))
            .unwrap(),
    );
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    let spec = reg.get("gain").unwrap().params[1].clone();
    fg.connect(src, 0, gain, 0).unwrap();
    fg.connect_param(ctl, 0, gain, &spec).unwrap();
    fg.connect(gain, 0, sink, 0).unwrap();
    // Finishes although the control producer never sends or ends: when the
    // gain finishes, the control producer is abandoned and stops too.
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out.len(), n);
    assert!(out.iter().all(|&v| v == 3.0), "the initial value applies");
}

fn wired(from_kind: &str, from_params: &str, to: &str) -> String {
    format!(
        r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = {{ type = "c32", sample_rate = 48000 }}

        [[block]]
        id = "ctl"
        kind = "{from_kind}"
        params = {from_params}

        [[block]]
        id = "x"
        kind = "freq_xlating_fir"
        params = {{ cutoff = 1000, transition = 1000 }}

        [[block]]
        id = "null"
        kind = "null_sink"
        params = {{ type = "c32" }}

        [[connect]]
        from = "src.out"
        to = "x.in"

        [[connect]]
        from = "x.out"
        to = "null.in"

        [[connect]]
        from = "ctl.out"
        to = "{to}"
        "#
    )
}

fn build_err(toml: &str) -> Error {
    let spec = GraphSpec::from_toml(toml).unwrap();
    match Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()) {
        Ok(_) => panic!("built"),
        Err(e) => e,
    }
}

fn assert_names_both(e: &Error, from: &str, to: &str) {
    let Error::Edge {
        from: f,
        to: t,
        msg,
    } = e
    else {
        panic!("not an edge error: {e}");
    };
    assert_eq!((f.as_str(), t.as_str()), (from, to), "{msg}");
    assert!(msg.contains(from) && msg.contains(to), "{msg}");
}

#[test]
fn wiring_errors_name_both_endpoints() {
    let f32_src = "{ type = \"f32\", sample_rate = 48000 }";
    // Not live.
    let e = build_err(&wired("signal_source", f32_src, "x.@transition"));
    assert_names_both(&e, "ctl.out", "x.@transition");
    assert!(e.to_string().contains("can't be wired"), "{e}");
    // Not a number (an enum).
    let e = build_err(&wired("signal_source", f32_src, "x.@window"));
    assert_names_both(&e, "ctl.out", "x.@window");
    // Unknown setting.
    let e = build_err(&wired("signal_source", f32_src, "x.@nope"));
    assert_names_both(&e, "ctl.out", "x.@nope");
    assert!(e.to_string().contains("no setting `nope`"), "{e}");
    // Wrong stream type: c32 into a control input.
    let e = build_err(&wired(
        "signal_source",
        "{ type = \"c32\", sample_rate = 48000 }",
        "x.@offset",
    ));
    assert_names_both(&e, "ctl.out", "x.@offset");
    assert!(e.to_string().contains("expects f32"), "{e}");
    // A good one builds and configures; control ports don't take part in
    // the rate: the filter's output keeps the main input's rate.
    let spec = GraphSpec::from_toml(&wired("signal_source", f32_src, "x.@offset")).unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let edges = fg.configure().unwrap();
    let ctl = edges.iter().find(|e| e.to == "x.@offset").unwrap();
    assert_eq!(ctl.from, "ctl.out");
}

/// A block whose live `level` (0…10) rejects 7 in `set_param`, and counts
/// the values it accepted.
struct Picky(Arc<AtomicUsize>);

impl Block for Picky {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let n = io.inputs[0].slice::<f32>().len();
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, _name: &str, value: &serde_json::Value) -> Result<(), Error> {
        if value.as_f64() == Some(7.0) {
            return Err(Error::Other("seven is unlucky".into()));
        }
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn rejected_control_values_are_reported_once_and_dont_stop_the_graph() {
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    reg.register(
        BlockDescriptor {
            kind: "picky",
            label: "Picky",
            category: "Test",
            help: "",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![],
            params: vec![ParamSpec {
                name: "level",
                label: "Level",
                kind: ParamKind::Float {
                    min: Some(0.0),
                    max: Some(10.0),
                    unit: None,
                },
                default: json!(1.0),
                help: "",
                live: true,
            }],
        },
        |_| unreachable!(),
    );
    let spec = reg.get("picky").unwrap().params[0].clone();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", paced(100, 400));
    // In range, then out of range many times, then rejected by the block.
    let mut ctl_values = vec![2.0f32];
    ctl_values.extend((0..100).map(|i| 20.0 + i as f32));
    ctl_values.push(7.0);
    // Each value in its own work call of the consumer would be ideal; even
    // when they arrive together only the last one counts, so send them
    // through a pacer.
    let ctl = fg.add(
        "ctl",
        Instance {
            block: Box::new(Drip {
                values: ctl_values,
                pos: 0,
            }),
            inputs: vec![],
            outputs: vec![("out".into(), StreamType::F32)],
        },
    );
    let picky = fg.add(
        "picky",
        Instance {
            block: Box::new(Picky(accepted.clone())),
            inputs: vec![("in".into(), StreamType::F32)],
            outputs: vec![],
        },
    );
    fg.connect(src, 0, picky, 0).unwrap();
    fg.connect_param(ctl, 0, picky, &spec).unwrap();
    let running = fg.start().unwrap();
    let monitor = running.monitor();
    running.wait().unwrap();
    let w = monitor.warnings();
    assert!(!w.is_empty(), "nothing reported");
    assert!(w.len() == 1, "{w:?}");
    assert_eq!(w[0].block, "picky");
    assert_eq!(w[0].key, "setting `level`");
    // Out of range reported once; 7 (rejected by the block) comes before
    // any accepted value, so it isn't reported again.
    assert_eq!(monitor.warning_count(), 1);
    assert!(accepted.load(Ordering::Relaxed) <= 1);
}

/// Sends one value per millisecond.
struct Drip {
    values: Vec<f32>,
    pos: usize,
}

impl Block for Drip {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let Some(&v) = self.values.get(self.pos) else {
            return Ok(WorkStatus::Done);
        };
        let out = io.outputs[0].slice_mut::<f32>();
        if out.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        out[0] = v;
        io.produce(0, 1);
        self.pos += 1;
        Ok(WorkStatus::Wait(Duration::from_millis(1)))
    }
}

#[test]
fn control_values_map_to_param_kinds() {
    let reg = foxygnal_blocks::registry();
    let float = reg.get("gain").unwrap().params[1].clone();
    assert_eq!(float.control_value(2.5), Some(json!(2.5)));
    assert_eq!(float.control_value(f32::NAN), None);
    let int = ParamSpec {
        name: "n",
        label: "N",
        kind: ParamKind::Int {
            min: Some(0),
            max: Some(10),
        },
        default: json!(0),
        help: "",
        live: true,
    };
    assert_eq!(int.control_value(2.6), Some(json!(3)));
    assert_eq!(int.control_value(11.0), None);
    let b = ParamSpec {
        kind: ParamKind::Bool,
        default: json!(false),
        ..int.clone()
    };
    assert_eq!(b.control_value(0.7), Some(json!(true)));
    assert_eq!(b.control_value(0.2), Some(json!(false)));
    assert!(b.wireable() && int.wireable() && float.wireable());
    let enum_ = reg.get("freq_xlating_fir").unwrap();
    assert!(
        enum_
            .params
            .iter()
            .any(|p| p.name == "offset" && p.wireable())
    );
    assert!(
        enum_
            .params
            .iter()
            .any(|p| p.name == "transition" && !p.wireable())
    );
}

// ---- Constant inputs ---------------------------------------------------------

fn resolve(toml: &str) -> Result<resolve::Resolved, Error> {
    resolve::resolve(
        &GraphSpec::from_toml(toml).unwrap(),
        &foxygnal_blocks::registry(),
        &NoLoader,
    )
}

const ADD_CONST: &str = r#"
    [vars]
    k = 1.5

    [[block]]
    id = "src"
    kind = "signal_source"
    params = { type = "f32", waveform = "constant", amplitude = 1.0, sample_rate = 48000 }

    [[block]]
    id = "add"
    kind = "add"
    params = { type = "f32" }
    const_inputs = { b = "$k * 2" }

    [[block]]
    id = "head"
    kind = "head"
    params = { type = "f32", count = 5000 }

    [[connect]]
    from = "src.out"
    to = "add.a"

    [[connect]]
    from = "add.out"
    to = "head.in"
"#;

/// Run a resolved graph, collecting `block`'s output `out` (f32 or c32).
fn run_collect<T: foxygnal_core::Sample>(r: &resolve::Resolved, block: &str) -> Vec<T> {
    let reg = foxygnal_blocks::registry();
    let mut fg = Flowgraph::from_resolved(&r.graph, &reg).unwrap();
    let (sink, out) = VectorSink::<T>::instance();
    let sink = fg.add("sink", sink);
    let b = fg.block_id(block).unwrap();
    fg.connect(b, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    out.lock().unwrap().clone()
}

#[test]
fn a_constant_input_becomes_a_hidden_source_with_no_rate_of_its_own() {
    let r = resolve(ADD_CONST).unwrap();
    let hidden = r
        .graph
        .blocks
        .iter()
        .find(|b| b.id == "add/const:b")
        .expect("hidden source");
    assert_eq!(hidden.kind, resolve::CONST_KIND);
    assert_eq!(hidden.params["type"], json!("f32"));
    assert_eq!(hidden.params["value"], json!(3.0), "`$k * 2` evaluated");
    let i = r
        .graph
        .connections
        .iter()
        .position(|c| c.from == "add/const:b.out" && c.to == "add.b")
        .expect("wired to add.b");
    assert!(r.origins[i].is_none(), "not a wire the user drew");
    // The other input sets the rate.
    let reg = foxygnal_blocks::registry();
    let mut fg = Flowgraph::from_resolved(&r.graph, &reg).unwrap();
    let edges = fg.configure().unwrap();
    let e = edges.iter().find(|e| e.from == "add.out").unwrap();
    assert_eq!(e.meta.sample_rate, Some(48_000.0));
    let e = edges.iter().find(|e| e.to == "add.b").unwrap();
    assert_eq!(e.meta.sample_rate, None);

    let out = run_collect::<f32>(&r, "head");
    assert_eq!(out.len(), 5000);
    assert!(out.iter().all(|&v| v == 4.0), "1 + 3");
}

#[test]
fn complex_constants_and_type_checks() {
    let toml = r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = { type = "c32", waveform = "constant", amplitude = 2.0, sample_rate = 1000 }

        [[block]]
        id = "mul"
        kind = "multiply"
        params = { type = "c32" }
        const_inputs = { b = [0.5, "-1"] }

        [[block]]
        id = "head"
        kind = "head"
        params = { type = "c32", count = 100 }

        [[connect]]
        from = "src.out"
        to = "mul.a"

        [[connect]]
        from = "mul.out"
        to = "head.in"
    "#;
    let r = resolve(toml).unwrap();
    let out = run_collect::<Complex32>(&r, "head");
    assert_eq!(out.len(), 100);
    assert!(
        out.iter().all(|&v| v == Complex32::new(1.0, -2.0)),
        "{:?}",
        out[0]
    );

    // `[re, im]` on a real input is an error naming the block and input.
    let e = resolve(&toml.replace("\"c32\"", "\"f32\"")).unwrap_err();
    assert_eq!(e.block(), Some("mul"));
    assert!(e.to_string().contains("input `b`"), "{e}");
    // An input that doesn't exist.
    let e = resolve(&toml.replace("const_inputs = { b", "const_inputs = { c")).unwrap_err();
    assert!(e.to_string().contains("no input `c`"), "{e}");
}

#[test]
fn constant_input_errors() {
    // Every input constant: nothing paces the block.
    let all = ADD_CONST
        .replace(
            "const_inputs = { b = \"$k * 2\" }",
            "const_inputs = { a = 1, b = 2 }",
        )
        .replace(
            "[[connect]]\n    from = \"src.out\"\n    to = \"add.a\"",
            "",
        );
    let e = resolve(&all).unwrap_err();
    assert_eq!(e.block(), Some("add"));
    assert!(e.to_string().contains("every input is a constant"), "{e}");
    // A constant and a wire into the same input.
    let both = ADD_CONST.replace("to = \"add.a\"", "to = \"add.b\"");
    let e = resolve(&both).unwrap_err();
    let Error::Edge { from, to, msg } = &e else {
        panic!("{e}")
    };
    assert_eq!((from.as_str(), to.as_str()), ("src.out", "add.b"));
    assert!(msg.contains("constant"), "{msg}");
    // Unknown variable.
    let e = resolve(&ADD_CONST.replace("$k * 2", "$nope")).unwrap_err();
    assert!(e.to_string().contains("unknown variable"), "{e}");
}

#[test]
fn changing_a_constant_is_a_live_change_of_the_hidden_source() {
    let reg = foxygnal_blocks::registry();
    let a = resolve(ADD_CONST).unwrap();
    let b = resolve(&ADD_CONST.replace("k = 1.5", "k = 4")).unwrap();
    let changes = diff_params(&reg, &a.graph, &b.graph).expect("same topology");
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].block, "add/const:b");
    assert_eq!(changes[0].param, "value");
    assert_eq!(changes[0].value, json!(8.0));
    assert!(changes[0].live);

    // And it applies while running.
    let r = resolve(&ADD_CONST.replace("count = 5000", "count = 2000000")).unwrap();
    let mut fg = Flowgraph::from_resolved(&r.graph, &reg).unwrap();
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    let head = fg.block_id("head").unwrap();
    fg.connect(head, 0, sink, 0).unwrap();
    let running = fg.start().unwrap();
    let params = running.param_sender();
    loop {
        if out.lock().unwrap().len() > 10_000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    params.send("add/const:b", "value", json!(9.0)).unwrap();
    running.wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out[0], 4.0);
    assert_eq!(*out.last().unwrap(), 10.0, "1 + 9 after the change");
}

#[test]
fn constants_on_a_hierarchical_blocks_input() {
    struct Files;
    impl resolve::GraphLoader for Files {
        fn load(&self, _name: &str) -> Result<GraphSpec, Error> {
            GraphSpec::from_toml(
                r#"
                [[block]]
                id = "add"
                kind = "add"
                params = { type = "f32" }

                [[export.input]]
                name = "x"
                to = "add.a"
                [[export.input]]
                name = "y"
                to = "add.b"
                [[export.output]]
                name = "out"
                from = "add.out"
                "#,
            )
        }
    }
    let toml = r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = { type = "f32", waveform = "constant", sample_rate = 1000 }

        [[block]]
        id = "sum"
        kind = "graph:sum.fox.toml"
        const_inputs = { y = 2 }

        [[block]]
        id = "head"
        kind = "head"
        params = { type = "f32", count = 10 }

        [[connect]]
        from = "src.out"
        to = "sum.x"

        [[connect]]
        from = "sum.out"
        to = "head.in"
    "#;
    let r = resolve::resolve(
        &GraphSpec::from_toml(toml).unwrap(),
        &foxygnal_blocks::registry(),
        &Files,
    )
    .unwrap();
    assert!(
        r.graph
            .connections
            .iter()
            .any(|c| c.from == "sum/const:y.out" && c.to == "sum/add.b")
    );
    let out = run_collect::<f32>(&r, "head");
    assert_eq!(out, vec![3.0; 10]);
}

#[test]
fn hidden_blocks_are_marked_in_the_catalog() {
    let reg = foxygnal_blocks::registry();
    let c = reg
        .catalog()
        .find(|c| c.descriptor.kind == resolve::CONST_KIND)
        .unwrap();
    assert!(c.hidden);
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["hidden"], json!(true));
    assert!(
        reg.catalog()
            .filter(|c| c.descriptor.kind == "add")
            .all(|c| !c.hidden)
    );
}
