//! The `fm-autotune` and `add-constant` templates, run headless on the
//! simulated FM band.

use std::time::{Duration, Instant};

use foxygnal_blocks::testing::VectorSink;
use foxygnal_core::{Flowgraph, GraphSpec, NoLoader};
use serde_json::json;

#[test]
fn fm_autotune_latches_onto_the_strongest_simulated_station() {
    let mut spec =
        GraphSpec::from_toml(include_str!("../../../examples/fm-autotune.fox.toml")).unwrap();
    let sdr = spec.blocks.iter_mut().find(|b| b.id == "sdr").unwrap();
    sdr.params.insert("device".into(), json!("sim:"));
    let reg = foxygnal_blocks::registry();
    let r = foxygnal_core::resolve(&spec, &reg, &NoLoader).unwrap();
    let mut fg = Flowgraph::from_resolved(&r.graph, &reg).unwrap();
    let (sink, offsets) = VectorSink::<f32>::instance();
    let sink = fg.add("offsets", sink);
    let peak = fg.block_id("peak").unwrap();
    fg.connect(peak, 0, sink, 0).unwrap();
    let edges = fg.configure().unwrap();
    let ctl = edges.iter().find(|e| e.to == "channel.@offset").unwrap();
    assert_eq!(ctl.meta.sample_rate.map(|r| r.round()), Some(5.0));

    let running = fg.start().unwrap();
    let t0 = Instant::now();
    // 99.7 MHz centre: 100.3 MHz (−20 dBFS) at +600 kHz is the strongest;
    // 99.1 MHz (−34 dBFS) at −600 kHz is weaker.
    let want = 600_000.0f32;
    let locked = loop {
        std::thread::sleep(Duration::from_millis(50));
        let v = offsets.lock().unwrap().clone();
        if v.len() >= 3 && v[v.len() - 3..].iter().all(|x| (x - want).abs() < 3_000.0) {
            break true;
        }
        if t0.elapsed() > Duration::from_secs(10) || running.is_finished() {
            break false;
        }
    };
    let took = t0.elapsed();
    running.stop();
    running.wait().unwrap();
    let v = offsets.lock().unwrap();
    assert!(locked, "no lock within 10 s: {v:?}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

#[test]
fn add_constant_example_builds() {
    let spec =
        GraphSpec::from_toml(include_str!("../../../examples/add-constant.fox.toml")).unwrap();
    let reg = foxygnal_blocks::registry();
    let r = foxygnal_core::resolve(&spec, &reg, &NoLoader).unwrap();
    assert!(r.graph.blocks.iter().any(|b| b.id == "add/const:b"));
    let mut fg = Flowgraph::from_resolved(&r.graph, &reg).unwrap();
    let edges = fg.configure().unwrap();
    let out = edges.iter().find(|e| e.from == "add.out").unwrap();
    assert_eq!(out.meta.sample_rate, Some(48_000.0));
}
