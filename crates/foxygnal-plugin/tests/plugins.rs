//! Plugin host tests against prebuilt plugins in `tests/fixtures` (built from
//! `plugins/` by `scripts/build-plugins.sh`; no wasm toolchain needed here).

use std::path::Path;
use std::time::{Duration, Instant};

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Flowgraph, Params, Registry, StreamMeta, Tag, TagValue};
use foxygnal_plugin::{Options, PluginHost};
use serde_json::{Value, json};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn params(v: Value) -> Params {
    v.as_object().expect("params object").clone()
}

/// The standard blocks plus both test plugins.
fn registry_with(options: Options) -> Registry {
    let mut reg = foxygnal_blocks::registry();
    let host = PluginHost::new(options).unwrap();
    for f in ["example.wasm", "misbehave.wasm"] {
        let p = host.load_file(&fixture(f)).unwrap();
        host.register(&p, &mut reg).unwrap();
    }
    reg
}

fn registry() -> Registry {
    registry_with(Options::default())
}

/// `VectorSource → kind → VectorSink`, run to the end.
fn run_f32(reg: &Registry, kind: &str, p: Value, input: Vec<f32>) -> Result<Vec<f32>, String> {
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(input));
    let blk = fg.add(
        "blk",
        reg.build(kind, &params(p)).map_err(|e| e.to_string())?,
    );
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    fg.start()
        .map_err(|e| e.to_string())?
        .wait()
        .map_err(|e| e.to_string())?;
    Ok(out.lock().unwrap().clone())
}

#[test]
fn lists_descriptors() {
    let reg = registry();
    let d = reg.get("example:moving_average").expect("registered");
    assert_eq!(d.label, "Moving average");
    assert_eq!(d.category, "Filters");
    assert_eq!(d.inputs.len(), 1);
    assert_eq!(d.inputs[0].ty, None);
    let names: Vec<_> = d.params.iter().map(|p| p.name).collect();
    assert_eq!(names, ["type", "length"]);
    assert!(d.params[1].live);

    let cat: Vec<Value> = reg
        .catalog()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    let gain = cat
        .iter()
        .find(|e| e["kind"] == "example:gain")
        .expect("in the catalog");
    assert_eq!(gain["plugin"], "example");
    assert_eq!(gain["params"][1]["kind"], "float");
    assert_eq!(gain["params"][1]["live"], true);
    // Built-in blocks don't carry the field.
    let native = cat.iter().find(|e| e["kind"] == "gain").unwrap();
    assert!(native.get("plugin").is_none());
}

#[test]
fn gain_and_moving_average_in_a_graph() {
    let reg = registry();
    let n = 100_000;
    let input: Vec<f32> = (0..n).map(|i| (i % 97) as f32).collect();

    let out = run_f32(&reg, "example:gain", json!({"gain": 0.5}), input.clone()).unwrap();
    assert_eq!(out.len(), n);
    assert!(out.iter().zip(&input).all(|(o, i)| *o == i * 0.5));

    let len = 8;
    let out = run_f32(
        &reg,
        "example:moving_average",
        json!({"length": len}),
        input.clone(),
    )
    .unwrap();
    assert_eq!(out.len(), n);
    for (k, o) in out.iter().enumerate() {
        let lo = k.saturating_sub(len - 1);
        let want = input[lo..=k].iter().sum::<f32>() / len as f32;
        assert!((o - want).abs() < 1e-3, "item {k}: {o} vs {want}");
    }
}

#[test]
fn complex_streams() {
    use foxygnal_core::Complex32;
    let reg = registry();
    let input: Vec<Complex32> = (0..5000).map(|i| Complex32::new(i as f32, -1.0)).collect();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(input.clone()));
    let blk = fg.add(
        "blk",
        reg.build("example:gain", &params(json!({"type": "c32", "gain": 2.0})))
            .unwrap(),
    );
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out.len(), input.len());
    assert!(out.iter().zip(&input).all(|(o, i)| *o == i * 2.0));
}

#[test]
fn live_param_change() {
    let reg = registry();
    let rate = 20_000.0;
    let n = 10_000; // 0.5 s at the throttle's rate
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(vec![1.0f32; n]));
    let thr = fg.add(
        "thr",
        reg.build(
            "throttle",
            &params(json!({"type": "f32", "sample_rate": rate})),
        )
        .unwrap(),
    );
    let blk = fg.add(
        "blk",
        reg.build("example:gain", &params(json!({"gain": 1.0})))
            .unwrap(),
    );
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, thr, 0).unwrap();
    fg.connect(thr, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    let running = fg.start().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    running
        .param_sender()
        .send("blk", "gain", json!(2.0))
        .unwrap();
    running.wait().unwrap();
    let out = out.lock().unwrap();
    assert_eq!(out.len(), n);
    let switch = out.iter().position(|&v| v == 2.0).expect("gain changed");
    assert!(switch > 0, "the change came too early");
    assert!(out[..switch].iter().all(|&v| v == 1.0));
    assert!(out[switch..].iter().all(|&v| v == 2.0));
}

#[test]
fn live_param_rejected_by_plugin_is_an_error() {
    let reg = registry();
    let mut inst = reg
        .build("example:moving_average", &params(json!({})))
        .unwrap();
    inst.block.set_param("length", &json!(4)).unwrap();
    let e = inst.block.set_param("type", &json!("c32")).unwrap_err();
    assert!(e.to_string().contains("cannot change while running"), "{e}");
}

#[test]
fn trap_is_a_block_error() {
    let reg = registry();
    let e = run_f32(
        &reg,
        "misbehave:trap",
        json!({"after": 1000}),
        vec![0.0; 20_000],
    )
    .unwrap_err();
    assert!(e.contains("blk"), "names the block: {e}");
    assert!(e.contains("plugin crashed in work"), "{e}");
    // The process is fine: the same plugin still works in a new graph.
    let out = run_f32(&reg, "misbehave:trap", json!({"after": 10}), vec![1.0; 10]).unwrap();
    assert_eq!(out, vec![1.0; 10]);
}

#[test]
fn endless_loop_is_interrupted() {
    let options = Options {
        timeout: Duration::from_millis(100),
        ..Options::default()
    };
    let reg = registry_with(options);
    let t = Instant::now();
    let e = run_f32(&reg, "misbehave:spin", json!({}), vec![0.0; 100]).unwrap_err();
    let took = t.elapsed();
    assert!(e.contains("ran longer than 100 ms"), "{e}");
    assert!(e.contains("blk"), "{e}");
    assert!(took < Duration::from_secs(2), "stopped after {took:?}");
}

#[test]
fn memory_limit() {
    let options = Options {
        memory_limit: 16 << 20,
        ..Options::default()
    };
    let reg = registry_with(options);
    let e = run_f32(&reg, "misbehave:hog", json!({}), vec![0.0; 100]).unwrap_err();
    assert!(e.contains("out of memory (limit 16 MiB)"), "{e}");
}

#[test]
fn create_error_and_param_check() {
    let reg = registry();
    let e = reg
        .build("misbehave:fail_create", &params(json!({})))
        .err()
        .unwrap();
    assert!(e.to_string().contains("never builds"), "{e}");
    let e = reg
        .build("example:moving_average", &params(json!({"length": 0})))
        .err()
        .unwrap();
    assert!(e.to_string().contains("at least 1"), "{e}");
}

#[test]
fn manual_tags_and_configure() {
    let reg = registry();
    // Tagger: forwards tags itself and tags its first item.
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::with_tags(vec![0.0f32; 100], vec![Tag::new(40, "mark", 7i64)]),
    );
    let blk = fg.add(
        "blk",
        reg.build("misbehave:tagger", &params(json!({}))).unwrap(),
    );
    let (sink, out, tags) = VectorSink::<f32>::with_tags();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    assert_eq!(out.lock().unwrap().len(), 100);
    let tags = tags.lock().unwrap();
    assert!(
        tags.iter()
            .any(|t| t.offset == 0 && &*t.key == "seen" && t.value == TagValue::Bool(true)),
        "{tags:?}"
    );
    assert!(
        tags.iter()
            .any(|t| t.offset == 40 && &*t.key == "mark" && t.value == TagValue::Int(7)),
        "{tags:?}"
    );

    // Half: halves the rate in configure; tags follow (all-to-all, scaled).
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta((0..10).map(|i| i as f32).collect(), {
            StreamMeta::with_rate(48_000.0)
        }),
    );
    let blk = fg.add(
        "blk",
        reg.build("misbehave:half", &params(json!({}))).unwrap(),
    );
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    let meta = fg.configure().unwrap();
    let edge = meta.iter().find(|m| m.from == "blk.out").unwrap();
    assert_eq!(edge.meta.sample_rate, Some(24_000.0));
    fg.start().unwrap().wait().unwrap();
    assert_eq!(*out.lock().unwrap(), [0.0, 2.0, 4.0, 6.0, 8.0]);
}

#[test]
fn rejects_what_is_not_a_plugin() {
    let host = PluginHost::new(Options::default()).unwrap();
    let e = host.load_bytes(b"hello").err().unwrap();
    assert!(e.to_string().contains("compiling"), "{e}");
    // An empty core module: no WIT metadata.
    let e = host.load_bytes(b"\0asm\x01\0\0\0").err().unwrap();
    assert!(e.to_string().contains("not a foxygnal plugin"), "{e}");
    assert!(e.to_string().contains("doesn't export"), "{e}");
}

#[test]
fn same_plugin_twice_is_refused() {
    let host = PluginHost::new(Options::default()).unwrap();
    let mut reg = Registry::new();
    let p = host.load_file(&fixture("example.wasm")).unwrap();
    host.register(&p, &mut reg).unwrap();
    let e = host.register(&p, &mut reg).unwrap_err();
    assert!(e.to_string().contains("already registered"), "{e}");
}

#[test]
fn load_dirs_skips_missing_and_reports_failures() {
    let dir = std::env::temp_dir().join(format!("foxygnal-plugins-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture("example.wasm"), dir.join("example.wasm")).unwrap();
    std::fs::write(dir.join("broken.wasm"), b"nope").unwrap();
    std::fs::write(dir.join("notes.txt"), b"ignored").unwrap();
    let host = PluginHost::new(Options::default()).unwrap();
    let mut reg = Registry::new();
    let reports = host.load_dirs(&[dir.join("missing"), dir.clone()], &mut reg);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(reports.len(), 2);
    assert!(reports[0].path.ends_with("broken.wasm") && reports[0].result.is_err());
    assert!(reports[1].result.is_ok());
    assert!(reg.get("example:gain").is_some());
}
