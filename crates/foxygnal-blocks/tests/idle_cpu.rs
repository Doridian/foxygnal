//! Idle-CPU budget: a throttled or stalled graph must leave the CPU alone.
//!
//! This is its own test binary (one test) because it measures the CPU time
//! of the whole process; other tests running alongside would count too.

#![cfg(unix)]

use std::time::{Duration, Instant};

use foxygnal_core::{Block, Error, Flowgraph, Instance, Params, StreamType, WorkIo, WorkStatus};
use serde_json::json;

/// User + system CPU time of this process, in seconds.
fn cpu_seconds() -> f64 {
    // SAFETY: getrusage only writes into the struct we pass.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut ru), 0);
        ru
    };
    let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 * 1e-6;
    secs(ru.ru_utime) + secs(ru.ru_stime)
}

/// Share of one core the process uses while `fg` runs for `secs` (after a
/// short settling time).
fn cpu_share(fg: Flowgraph, secs: f64) -> f64 {
    let running = fg.start().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let (c0, t0) = (cpu_seconds(), Instant::now());
    std::thread::sleep(Duration::from_secs_f64(secs));
    let share = (cpu_seconds() - c0) / t0.elapsed().as_secs_f64();
    assert!(!running.is_finished(), "the graph should still be running");
    running.stop();
    running.wait().unwrap();
    share
}

fn params(v: serde_json::Value) -> Params {
    v.as_object().unwrap().clone()
}

/// Never produces, never finishes.
struct Silent;

impl Block for Silent {
    fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        Ok(WorkStatus::Blocked)
    }
}

#[test]
fn throttled_and_stalled_graphs_use_almost_no_cpu() {
    let reg = foxygnal_blocks::registry();

    // A 48 kS/s tone through a throttle into a null sink. Measured in a
    // debug build: about 0.6% of a core on a desktop. A shared CI VM, with
    // slow wakeups, measured 10-14% when the throttle still woke every 2 ms
    // (now 10 ms). The bound only has to catch a busy loop (100%).
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        reg.build(
            "signal_source",
            &params(json!({"type": "f32", "frequency": 1000.0, "sample_rate": 48000.0})),
        )
        .unwrap(),
    );
    let thr = fg.add(
        "thr",
        reg.build("throttle", &params(json!({"type": "f32"})))
            .unwrap(),
    );
    let sink = fg.add(
        "sink",
        reg.build("null_sink", &params(json!({"type": "f32"})))
            .unwrap(),
    );
    fg.connect(src, 0, thr, 0).unwrap();
    fg.connect(thr, 0, sink, 0).unwrap();
    let share = cpu_share(fg, 2.0);
    assert!(
        share < 0.2,
        "throttled graph uses {:.1}% of a core",
        share * 100.0
    );

    // A graph whose source never produces: nothing may run at all.
    let mut fg = Flowgraph::new();
    let mut prev = fg.add(
        "silent",
        Instance {
            block: Box::new(Silent),
            inputs: vec![],
            outputs: vec![("out".into(), StreamType::F32)],
        },
    );
    for i in 0..5 {
        let g = fg.add(
            format!("gain{i}"),
            reg.build("gain", &params(json!({"type": "f32"}))).unwrap(),
        );
        fg.connect(prev, 0, g, 0).unwrap();
        prev = g;
    }
    let sink = fg.add(
        "sink",
        reg.build("null_sink", &params(json!({"type": "f32"})))
            .unwrap(),
    );
    fg.connect(prev, 0, sink, 0).unwrap();
    let share = cpu_share(fg, 1.0);
    assert!(
        share < 0.01,
        "stalled graph uses {:.2}% of a core",
        share * 100.0
    );
}
