use foxygnal_blocks::testing::{Collected, VectorSink, VectorSource};
use foxygnal_core::{Flowgraph, GraphSpec, Params};
use serde_json::json;

fn params(v: serde_json::Value) -> Params {
    v.as_object().unwrap().clone()
}

#[test]
fn add_two_vectors_through_small_buffers() {
    let reg = foxygnal_blocks::registry();
    // Tiny buffers force many wrap-arounds and back-pressure cycles.
    let mut fg = Flowgraph::new().with_buffer_capacity(7);
    let n = 10_000;
    let a = fg.add(
        "a",
        VectorSource::instance((0..n).map(|i| i as f32).collect()),
    );
    let b = fg.add("b", VectorSource::instance(vec![1.0f32; n]));
    let add = fg.add(
        "add",
        reg.build("add", &params(json!({"type": "f32"}))).unwrap(),
    );
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(a, 0, add, 0).unwrap();
    fg.connect(b, 0, add, 1).unwrap();
    fg.connect(add, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out.len(), n);
    assert!(out.iter().enumerate().all(|(i, &v)| v == i as f32 + 1.0));
}

#[test]
fn fan_out_and_head_from_toml() {
    let dir = std::env::temp_dir().join(format!("foxygnal-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tone.c32");
    let spec = GraphSpec::from_toml(&format!(
        r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = {{ type = "c32", frequency = 1000.0, sample_rate = 48000.0 }}

        [[block]]
        id = "head"
        kind = "head"
        params = {{ type = "c32", count = 4800 }}

        [[block]]
        id = "file"
        kind = "file_sink"
        params = {{ type = "c32", path = "{}" }}

        [[block]]
        id = "null"
        kind = "null_sink"
        params = {{ type = "c32" }}

        [[connect]]
        from = "src.out"
        to = "head.in"

        [[connect]]
        from = "head.out"
        to = "file.in"

        [[connect]]
        from = "head.out"
        to = "null.0"
        "#,
        path.display()
    ))
    .unwrap();
    let fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    fg.start().unwrap().wait().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 4800 * 8);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn type_mismatch_is_a_friendly_error() {
    let spec = GraphSpec::from_toml(
        r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = { type = "f32" }
        [[block]]
        id = "sink"
        kind = "null_sink"
        params = { type = "c32" }
        [[connect]]
        from = "src.out"
        to = "sink.in"
        "#,
    )
    .unwrap();
    let err = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry())
        .err()
        .expect("should fail");
    assert!(
        err.to_string()
            .contains("produces f32 but `sink.in` expects c32"),
        "{err}"
    );
}

#[test]
fn errors_name_the_block_or_edge() {
    let reg = foxygnal_blocks::registry();
    let build = |toml: &str| {
        Flowgraph::from_spec(&GraphSpec::from_toml(toml).unwrap(), &reg)
            .and_then(Flowgraph::start)
            .err()
            .expect("should fail")
    };

    // Bad parameter: attributed to the block id, not its kind.
    let err = build(
        r#"
        [[block]]
        id = "tone"
        kind = "signal_source"
        params = { waveform = "wobble" }
        "#,
    );
    assert_eq!(err.block(), Some("tone"), "{err}");

    let err = build(
        r#"
        [[block]]
        id = "tone"
        kind = "signal_source"
        params = { frequency = "loud" }
        "#,
    );
    assert_eq!(err.block(), Some("tone"), "{err}");
    // Strings in number params are expressions (`"$samp_rate / 2"`).
    assert!(
        err.detail()
            .starts_with("parameter `frequency`: `loud` isn't a number"),
        "{err}"
    );

    // Unconnected input: attributed to the block that owns it.
    let err = build(
        r#"
        [[block]]
        id = "sink"
        kind = "null_sink"
        "#,
    );
    assert_eq!(err.block(), Some("sink"));
    assert_eq!(err.detail(), "input `in` is not connected");

    // Bad connection: attributed to the edge as written in the spec.
    let err = build(
        r#"
        [[block]]
        id = "sink"
        kind = "null_sink"
        [[connect]]
        from = "nowhere.out"
        to = "sink.in"
        "#,
    );
    match err {
        foxygnal_core::Error::Edge { from, to, msg } => {
            assert_eq!((from.as_str(), to.as_str()), ("nowhere.out", "sink.in"));
            assert_eq!(msg, "unknown block `nowhere`");
        }
        e => panic!("expected an edge error, got {e}"),
    }
}

/// Emits nothing until a `go` parameter arrives, then emits it once and ends.
struct WaitForParam(Option<f32>);

impl foxygnal_core::Block for WaitForParam {
    fn work(
        &mut self,
        io: &mut foxygnal_core::WorkIo<'_>,
    ) -> Result<foxygnal_core::WorkStatus, foxygnal_core::Error> {
        let Some(v) = self.0.take() else {
            return Ok(foxygnal_core::WorkStatus::Blocked);
        };
        io.outputs[0].slice_mut::<f32>()[0] = v;
        io.produce(0, 1);
        Ok(foxygnal_core::WorkStatus::Done)
    }

    fn set_param(
        &mut self,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<(), foxygnal_core::Error> {
        assert_eq!(name, "go");
        self.0 = value.as_f64().map(|v| v as f32);
        Ok(())
    }
}

#[test]
fn live_params_reach_a_running_block() {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        foxygnal_core::Instance {
            block: Box::new(WaitForParam(None)),
            inputs: vec![],
            outputs: vec![("out".into(), foxygnal_core::StreamType::F32)],
        },
    );
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, sink, 0).unwrap();
    let running = fg.start().unwrap();
    running
        .param_sender()
        .send("src", "go", json!(42.0))
        .unwrap();
    running.wait().unwrap();
    assert_eq!(*out.lock().unwrap(), vec![42.0]);
}

#[test]
fn a_rejected_live_param_stops_the_graph_with_an_error() {
    let reg = foxygnal_blocks::registry();
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "tone",
        reg.build("signal_source", &params(json!({"type": "f32"})))
            .unwrap(),
    );
    let sink = fg.add(
        "sink",
        reg.build("null_sink", &params(json!({"type": "f32"})))
            .unwrap(),
    );
    fg.connect(src, 0, sink, 0).unwrap();
    let running = fg.start().unwrap();
    running
        .param_sender()
        .send("tone", "waveform", json!("square"))
        .unwrap();
    let err = running.wait().expect_err("waveform is not live");
    assert_eq!(err.block(), Some("tone"));
}

/// A throttled tone: the monitor's tap sees the samples and the counters see
/// roughly the throttle's rate, and a throttle that is waiting isn't "busy".
#[test]
fn monitor_taps_and_counts_a_running_graph() {
    let spec = GraphSpec::from_toml(
        r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = { type = "f32", waveform = "square", frequency = 1000.0, amplitude = 0.5, sample_rate = 48000.0 }

        [[block]]
        id = "thr"
        kind = "throttle"
        params = { type = "f32", sample_rate = 48000.0 }

        [[block]]
        id = "null"
        kind = "null_sink"
        params = { type = "f32" }

        [[connect]]
        from = "src.out"
        to = "thr.in"

        [[connect]]
        from = "thr.out"
        to = "null.in"
        "#,
    )
    .unwrap();
    let fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let running = fg.start().unwrap();
    let mon = running.monitor();
    assert!(mon.tap("thr", "nope").is_none());
    let (ty, tap) = mon.tap("thr", "0").expect("tap by index");
    assert_eq!(ty, foxygnal_core::StreamType::F32);

    let before = mon.stats();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = mon.stats();

    let mut buf = vec![0u8; 4 * 1024];
    let got = tap.read(tap.written().saturating_sub(1024), &mut buf);
    assert!(got.items > 0);
    let samples: Vec<f32> = buf[..got.items * 4]
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert!(
        samples.iter().all(|v| (v.abs() - 0.5).abs() < 1e-6),
        "a ±0.5 square wave"
    );

    let edge = |c: &foxygnal_core::GraphCounters| {
        c.edges
            .iter()
            .find(|e| e.from == "thr.out" && e.to == "null.in")
            .unwrap()
            .written
    };
    let rate = (edge(&after) - edge(&before)) as f64 / 0.3;
    assert!((30_000.0..70_000.0).contains(&rate), "rate {rate}");
    let busy = |c: &foxygnal_core::GraphCounters| {
        c.blocks.iter().find(|b| b.block == "thr").unwrap().busy_ns
    };
    let load = (busy(&after) - busy(&before)) as f64 / 0.3e9;
    assert!(load < 0.5, "an idle throttle isn't busy ({load})");

    running.stop();
    running.wait().unwrap();
}

/// The source's rate flows downstream; a throttle at 0 takes it from there.
#[test]
fn stream_metadata_propagates() {
    let toml = |src_rate: &str| {
        format!(
            r#"
            [[block]]
            id = "src"
            kind = "file_source"
            params = {{ type = "c32", path = "does-not-exist.c32", sample_rate = {src_rate}, center_freq = 100e6 }}
            [[block]]
            id = "thr"
            kind = "throttle"
            params = {{ type = "c32" }}
            [[block]]
            id = "sink"
            kind = "null_sink"
            params = {{ type = "c32" }}
            [[connect]]
            from = "src.out"
            to = "thr.in"
            [[connect]]
            from = "thr.out"
            to = "sink.in"
            "#
        )
    };
    let reg = foxygnal_blocks::registry();
    // Building and configuring opens no files.
    let mut fg =
        Flowgraph::from_spec(&GraphSpec::from_toml(&toml("2.4e6")).unwrap(), &reg).unwrap();
    let meta = fg.configure().unwrap();
    assert_eq!(meta.len(), 2);
    for m in &meta {
        assert_eq!(m.meta.sample_rate, Some(2.4e6), "{}", m.to);
        assert_eq!(m.meta.center_freq, Some(100e6));
    }
    assert_eq!(meta[1].from, "thr.out");

    // Nothing sets a rate: the throttle says so, by name.
    let mut fg = Flowgraph::from_spec(&GraphSpec::from_toml(&toml("0")).unwrap(), &reg).unwrap();
    let err = fg.configure().unwrap_err();
    assert_eq!(err.block(), Some("thr"));
    assert!(err.to_string().contains("sample rate"), "{err}");
}

/// Keeps one item in `n`; relies on the runtime to pass tags on.
struct KeepOneIn(usize);

impl foxygnal_core::Block for KeepOneIn {
    fn work(
        &mut self,
        io: &mut foxygnal_core::WorkIo<'_>,
    ) -> Result<foxygnal_core::WorkStatus, foxygnal_core::Error> {
        let (inp, out) = io.in_out::<f32, f32>(0, 0);
        let k = (inp.len() / self.0).min(out.len());
        if k == 0 {
            return Ok(foxygnal_core::WorkStatus::Blocked);
        }
        for (o, i) in out.iter_mut().zip(inp.iter().step_by(self.0)) {
            *o = *i;
        }
        io.consume(0, k * self.0);
        io.produce(0, k);
        Ok(foxygnal_core::WorkStatus::Ok)
    }
}

#[test]
fn tags_flow_through_blocks_and_scale_with_the_rate() {
    use foxygnal_core::{Instance, StreamType, Tag, TagValue};
    let reg = foxygnal_blocks::registry();
    // Small buffers: tags must stay with their items across wrap-arounds.
    // Even, so the decimator never sees a 1-item slice at the wrap point.
    let mut fg = Flowgraph::new().with_buffer_capacity(14);
    let n = 2_000;
    let tags = vec![
        Tag::new(0, "start", true),
        Tag::new(10, "freq", 100e6),
        Tag::new(11, "burst", 3i64),
        Tag::new(1_501, "label", "hello"),
    ];
    let src = fg.add(
        "src",
        VectorSource::with_tags((0..n).map(|i| i as f32).collect(), tags),
    );
    let gain = fg.add(
        "gain",
        reg.build("gain", &params(json!({"type": "f32", "gain": 1.0})))
            .unwrap(),
    );
    let dec = fg.add(
        "dec",
        Instance {
            block: Box::new(KeepOneIn(2)),
            inputs: vec![("in".into(), StreamType::F32)],
            outputs: vec![("out".into(), StreamType::F32)],
        },
    );
    let (full, _, full_tags) = VectorSink::<f32>::with_tags();
    let full = fg.add("full", full);
    let (half, half_data, half_tags) = VectorSink::<f32>::with_tags();
    let half = fg.add("half", half);
    fg.connect(src, 0, gain, 0).unwrap();
    fg.connect(gain, 0, full, 0).unwrap();
    fg.connect(gain, 0, dec, 0).unwrap();
    fg.connect(dec, 0, half, 0).unwrap();
    fg.start().unwrap().wait().unwrap();

    let offs = |t: &Collected<Tag>| -> Vec<(u64, String)> {
        t.lock()
            .unwrap()
            .iter()
            .map(|t| (t.offset, t.key.to_string()))
            .collect()
    };
    assert_eq!(
        offs(&full_tags),
        [(0, "start"), (10, "freq"), (11, "burst"), (1501, "label")]
            .map(|(o, k)| (o, k.to_string()))
    );
    assert_eq!(
        full_tags.lock().unwrap()[3].value,
        TagValue::Str("hello".into())
    );
    // Decimated by 2: offsets halve (rounding down), and each tag's item is
    // the kept item covering it.
    let half_offs = offs(&half_tags);
    assert_eq!(
        half_offs,
        [(0, "start"), (5, "freq"), (5, "burst"), (750, "label")].map(|(o, k)| (o, k.to_string()))
    );
    assert_eq!(half_data.lock().unwrap()[750], 1500.0);
}
