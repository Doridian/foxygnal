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
