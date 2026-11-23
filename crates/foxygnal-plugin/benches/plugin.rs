//! Plugin blocks vs the same blocks compiled in: `VectorSource → block →
//! null_sink`, run to the end of the stream (as in foxygnal-blocks'
//! benches). Throughput is in items/s.
//!
//! - `gain_*`: the built-in `gain` vs `example:gain` (same loop).
//! - `moving_average_*`: a native copy of the example plugin's moving
//!   average (defined below) vs `example:moving_average`.
//!
//! `cargo bench -p foxygnal-plugin` (add `-- --quick` for a fast pass).
//! Uses the prebuilt plugin in tests/fixtures.

use std::ops::{Add, Mul, Sub};
use std::path::Path;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use foxygnal_blocks::testing::VectorSource;
use foxygnal_core::{
    Block, Complex32, Error, Flowgraph, Instance, Params, Registry, Sample, WorkIo, WorkStatus,
};
use foxygnal_plugin::{Options, PluginHost};
use serde_json::{Value, json};

const N: usize = 1 << 20;

fn params(v: Value) -> Params {
    v.as_object().expect("params object").clone()
}

fn noise(n: usize) -> Vec<f32> {
    let mut s = 2_654_435_761u32;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

fn registry() -> Registry {
    let mut reg = foxygnal_blocks::registry();
    let host = PluginHost::new(Options::default()).expect("host");
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/example.wasm");
    let p = host.load_file(&path).expect("example plugin");
    host.register(&p, &mut reg).expect("register");
    reg
}

/// The example plugin's moving average, compiled in.
struct MovingAverage<T> {
    history: Vec<T>,
    pos: usize,
    sum: T,
    since_exact: usize,
}

impl<T> MovingAverage<T>
where
    T: Sample + Default + Add<Output = T> + Sub<Output = T> + Mul<f32, Output = T>,
{
    fn instance(len: usize) -> Instance {
        Instance {
            block: Box::new(Self {
                history: vec![T::default(); len],
                pos: 0,
                sum: T::default(),
                since_exact: 0,
            }),
            inputs: vec![("in".into(), T::TYPE)],
            outputs: vec![("out".into(), T::TYPE)],
        }
    }
}

impl<T> Block for MovingAverage<T>
where
    T: Sample + Default + Add<Output = T> + Sub<Output = T> + Mul<f32, Output = T>,
{
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let len = self.history.len();
        let scale = 1.0 / len as f32;
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            let old = std::mem::replace(&mut self.history[self.pos], x);
            self.sum = self.sum + x - old;
            self.pos += 1;
            if self.pos == len {
                self.pos = 0;
            }
            *o = self.sum * scale;
        }
        self.since_exact += n;
        if self.since_exact >= 1 << 16 {
            self.since_exact = 0;
            self.sum = self.history.iter().fold(T::default(), |a, &v| a + v);
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn bench<T: Sample>(
    c: &mut Criterion,
    name: &str,
    data: &[T],
    sink: &str,
    make: &dyn Fn() -> Instance,
) {
    let reg = registry();
    let build = || {
        let mut fg = Flowgraph::new();
        let src = fg.add("src", VectorSource::instance(data.to_vec()));
        let blk = fg.add("blk", make());
        let sink = fg.add(
            "sink",
            reg.build("null_sink", &params(json!({ "type": sink })))
                .expect("null_sink"),
        );
        fg.connect(src, 0, blk, 0).expect("connect");
        fg.connect(blk, 0, sink, 0).expect("connect");
        fg
    };
    let mut g = c.benchmark_group("plugin");
    g.sample_size(10);
    g.warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Elements(data.len() as u64));
    g.bench_function(name, |b| {
        b.iter_batched(
            build,
            |fg| fg.start().expect("start").wait().expect("run"),
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

fn benches(c: &mut Criterion) {
    let reg = registry();
    let f = noise(N);
    let cx: Vec<Complex32> = f
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Complex32::new(p[0], p[1]))
        .collect();
    let gain = |kind: &'static str, ty: &'static str| {
        let reg = &reg;
        move || {
            reg.build(kind, &params(json!({ "type": ty, "gain": 0.5 })))
                .expect(kind)
        }
    };
    bench(c, "gain_f32_native", &f, "f32", &gain("gain", "f32"));
    bench(
        c,
        "gain_f32_plugin",
        &f,
        "f32",
        &gain("example:gain", "f32"),
    );
    bench(c, "gain_c32_native", &cx, "c32", &gain("gain", "c32"));
    bench(
        c,
        "gain_c32_plugin",
        &cx,
        "c32",
        &gain("example:gain", "c32"),
    );
    bench(c, "moving_average_f32_native", &f, "f32", &|| {
        MovingAverage::<f32>::instance(16)
    });
    bench(c, "moving_average_f32_plugin", &f, "f32", &|| {
        reg.build(
            "example:moving_average",
            &params(json!({ "type": "f32", "length": 16 })),
        )
        .expect("moving_average")
    });
}

criterion_group!(plugin, benches);
criterion_main!(plugin);
