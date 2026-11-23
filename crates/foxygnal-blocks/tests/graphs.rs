use foxygnal_blocks::testing::{VectorSink, VectorSource};
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
    assert_eq!(err.detail(), "parameter `frequency`: expected a number");

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
