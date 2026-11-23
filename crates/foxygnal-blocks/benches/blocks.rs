//! Block and whole-graph throughput, measured through real flowgraphs:
//! `VectorSource → block → null_sink`, run to the end of the stream as fast as
//! the scheduler allows. Throughput is in input items, so criterion reports
//! items/s (the FM receiver: input samples/s, i.e. how many times faster than
//! a 2.4 MS/s radio it runs).
//!
//! Each iteration builds a fresh graph outside the timed section and times
//! `start()` to the end of the stream, so thread start-up (tens of µs) is
//! included; inputs are large enough for that to be noise. The
//! `passthrough` entries show what the source, sink and buffers alone cost.
//!
//! `cargo bench -p foxygnal-blocks` (add `-- --quick` for a fast pass).

use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use foxygnal_blocks::testing::VectorSource;
use foxygnal_core::{Complex32, Flowgraph, Instance, Params, Registry, Sample, StreamMeta};
use serde_json::{Value, json};

/// Items per run.
const N: usize = 1 << 19;

fn params(v: Value) -> Params {
    v.as_object().expect("params object").clone()
}

fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed.wrapping_mul(2_654_435_761).max(1);
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

fn cnoise(n: usize, seed: u32) -> Vec<Complex32> {
    noise(2 * n, seed)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Complex32::new(p[0], p[1]) * 0.5)
        .collect()
}

/// A wideband FM signal (a few tones modulating ±75 kHz) at `rate`, offset
/// from the centre by `offset` Hz, plus a little noise.
fn fm_signal(n: usize, rate: f64, offset: f64) -> Vec<Complex32> {
    use std::f64::consts::TAU;
    let dev = 75_000.0;
    let mut phase = 0.0f64;
    let nz = cnoise(n, 7);
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let m = 0.6 * (TAU * 1_000.0 * t).sin() + 0.3 * (TAU * 5_300.0 * t).sin();
            phase = (phase + TAU * (offset + dev * m) / rate).rem_euclid(TAU);
            Complex32::from_polar(0.8, phase as f32) + nz[i] * 0.01
        })
        .collect()
}

/// A chain of blocks from a vector source, ending in a null sink.
struct Chain<T> {
    data: Vec<T>,
    rate: f64,
    blocks: Vec<(&'static str, Value)>,
    sink_type: &'static str,
}

impl<T: Sample> Chain<T> {
    fn build(&self, reg: &Registry) -> Flowgraph {
        let mut fg = Flowgraph::new();
        let src =
            VectorSource::instance_with_meta(self.data.clone(), StreamMeta::with_rate(self.rate));
        let mut prev = fg.add("src", src);
        for (i, (kind, p)) in self.blocks.iter().enumerate() {
            let inst: Instance = reg.build(kind, &params(p.clone())).expect(kind);
            let id = fg.add(format!("b{i}"), inst);
            fg.connect(prev, 0, id, 0).expect("connect");
            prev = id;
        }
        let sink = reg
            .build("null_sink", &params(json!({ "type": self.sink_type })))
            .expect("null_sink");
        let sink = fg.add("sink", sink);
        fg.connect(prev, 0, sink, 0).expect("connect sink");
        fg
    }
}

fn bench_chain<T: Sample>(c: &mut Criterion, group: &str, name: &str, chain: Chain<T>) {
    let reg = foxygnal_blocks::registry();
    // Fail loudly here rather than inside the timing loop.
    chain.build(&reg).configure().expect("configure");
    let mut g = c.benchmark_group(group);
    g.sample_size(10);
    g.warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Elements(chain.data.len() as u64));
    g.bench_function(name, |b| {
        b.iter_batched(
            || chain.build(&reg),
            |fg| fg.start().expect("start").wait().expect("run"),
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

fn c32(blocks: Vec<(&'static str, Value)>, sink: &'static str, rate: f64) -> Chain<Complex32> {
    Chain {
        data: cnoise(N, 1),
        rate,
        blocks,
        sink_type: sink,
    }
}

fn f32s(blocks: Vec<(&'static str, Value)>, rate: f64) -> Chain<f32> {
    Chain {
        data: noise(N, 2),
        rate,
        blocks,
        sink_type: "f32",
    }
}

fn blocks(c: &mut Criterion) {
    let g = "block";
    bench_chain(c, g, "passthrough_c32", c32(vec![], "c32", 1e6));
    // 127 taps (Hamming: 3.3 / (26 kHz / 1 MS/s)).
    let fir = |ty: &str| {
        json!({ "type": ty, "mode": "low_pass", "cutoff": 100_000.0, "transition": 26_000.0,
                "window": "hamming" })
    };
    bench_chain(
        c,
        g,
        "fir_filter_c32_127",
        c32(vec![("fir_filter", fir("c32"))], "c32", 1e6),
    );
    bench_chain(
        c,
        g,
        "fir_filter_f32_127",
        f32s(vec![("fir_filter", fir("f32"))], 1e6),
    );
    // The FM receiver's channel filter: 198 taps, ÷10.
    bench_chain(
        c,
        g,
        "freq_xlating_fir_198_dec10",
        c32(
            vec![(
                "freq_xlating_fir",
                json!({ "offset": 300_000.0, "cutoff": 100_000.0, "transition": 40_000.0,
                        "decimation": 10, "window": "hamming" }),
            )],
            "c32",
            2.4e6,
        ),
    );
    bench_chain(
        c,
        g,
        "decimator_c32_10",
        c32(
            vec![("decimator", json!({ "type": "c32", "decimation": 10 }))],
            "c32",
            2.4e6,
        ),
    );
    bench_chain(
        c,
        g,
        "rational_resampler_f32_240k_48k",
        f32s(
            vec![(
                "rational_resampler",
                json!({ "type": "f32", "output_rate": 48_000.0 }),
            )],
            240e3,
        ),
    );
    bench_chain(
        c,
        g,
        "fm_demod",
        c32(
            vec![(
                "fm_demod",
                json!({ "deviation": 75_000.0, "deemphasis": "50us" }),
            )],
            "f32",
            240e3,
        ),
    );
    bench_chain(
        c,
        g,
        "am_demod",
        c32(vec![("am_demod", json!({ "dc_time": 0.05 }))], "f32", 48e3),
    );
    bench_chain(
        c,
        g,
        "agc_c32",
        c32(vec![("agc", json!({ "type": "c32" }))], "c32", 48e3),
    );
    bench_chain(
        c,
        g,
        "agc_f32",
        f32s(vec![("agc", json!({ "type": "f32" }))], 48e3),
    );
    bench_chain(
        c,
        g,
        "gain_c32",
        c32(
            vec![("gain", json!({ "type": "c32", "gain": 0.5 }))],
            "c32",
            1e6,
        ),
    );
    bench_chain(
        c,
        g,
        "gain_f32",
        f32s(vec![("gain", json!({ "type": "f32", "gain": 0.5 }))], 1e6),
    );
    bench_chain(
        c,
        g,
        "c32_to_f32_magnitude",
        c32(
            vec![("c32_to_f32", json!({ "part": "magnitude" }))],
            "f32",
            1e6,
        ),
    );
    bench_chain(
        c,
        g,
        "c32_to_f32_phase",
        c32(vec![("c32_to_f32", json!({ "part": "phase" }))], "f32", 1e6),
    );
    // Converters count input items: bytes / shorts (two per complex sample).
    let bytes: Vec<u8> = noise(2 * N, 3)
        .iter()
        .map(|v| ((v + 1.0) * 127.5) as u8)
        .collect();
    bench_chain(
        c,
        g,
        "cu8_to_c32",
        Chain {
            data: bytes,
            rate: 4.8e6,
            blocks: vec![("cu8_to_c32", json!({}))],
            sink_type: "c32",
        },
    );
    let shorts: Vec<i16> = noise(2 * N, 4)
        .iter()
        .map(|v| (v * 32767.0) as i16)
        .collect();
    bench_chain(
        c,
        g,
        "ci16_to_c32",
        Chain {
            data: shorts,
            rate: 4.8e6,
            blocks: vec![("ci16_to_c32", json!({}))],
            sink_type: "c32",
        },
    );
}

fn sources(c: &mut Criterion) {
    let reg = foxygnal_blocks::registry();
    let mut g = c.benchmark_group("block");
    g.sample_size(10);
    g.warm_up_time(Duration::from_secs(1));
    g.throughput(Throughput::Elements(N as u64));
    for ty in ["c32", "f32"] {
        g.bench_function(format!("signal_source_{ty}"), |b| {
            b.iter_batched(
                || {
                    let mut fg = Flowgraph::new();
                    let p = json!({ "type": ty, "frequency": 1234.5, "sample_rate": 48_000.0 });
                    let src = fg.add("src", reg.build("signal_source", &params(p)).unwrap());
                    let p = json!({ "type": ty, "count": N });
                    let head = fg.add("head", reg.build("head", &params(p)).unwrap());
                    let p = json!({ "type": ty });
                    let sink = fg.add("sink", reg.build("null_sink", &params(p)).unwrap());
                    fg.connect(src, 0, head, 0).unwrap();
                    fg.connect(head, 0, sink, 0).unwrap();
                    fg
                },
                |fg| fg.start().unwrap().wait().unwrap(),
                BatchSize::PerIteration,
            )
        });
    }
    g.finish();
}

/// The broadcast FM receiver from `examples/fm-receiver.fox.toml`, fed from
/// memory instead of a file: 2.4 MS/s → xlating FIR ÷10 → FM demod →
/// 48 kS/s. Throughput is input samples/s; divide by 2.4 M for "× real time".
fn fm_receiver(c: &mut Criterion) {
    let rate = 2.4e6;
    let chain = Chain {
        data: fm_signal(4 * N, rate, 300_000.0),
        rate,
        blocks: vec![
            (
                "freq_xlating_fir",
                json!({ "offset": 300_000.0, "cutoff": 100_000.0, "transition": 40_000.0,
                        "decimation": 10, "window": "hamming" }),
            ),
            (
                "fm_demod",
                json!({ "deviation": 75_000.0, "deemphasis": "50us" }),
            ),
            (
                "rational_resampler",
                json!({ "type": "f32", "output_rate": 48_000.0 }),
            ),
        ],
        sink_type: "f32",
    };
    bench_chain(c, "graph", "fm_receiver_2m4", chain);
}

/// DVB-S at 2 samples per symbol, rate 3/4, Es/N0 10 dB, 1.5 kHz off: the
/// signal from `dvbs_modulator`, then the demodulator (acquisition included,
/// so the first ~20 k symbols try every hypothesis). Throughput is input
/// samples/s; symbols/s is half that.
fn dvbs_demodulator(c: &mut Criterion) {
    use foxygnal_blocks::testing::VectorSink;
    let reg = foxygnal_blocks::registry();
    let (symbol_rate, sps) = (1e6, 2usize);
    // 600 packets ≈ 650 k symbols.
    let ts: Vec<u8> = (0..600u32)
        .flat_map(|i| {
            let mut p = vec![0x47, 0x01, 0x00, 0x10 | (i as u8 & 15)];
            p.extend((4..188u32).map(|k| (i.wrapping_mul(31) ^ k.wrapping_mul(97)) as u8));
            p
        })
        .collect();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(ts));
    let p = json!({ "rate": "3/4", "sps": sps, "symbol_rate": symbol_rate, "gain": 0.5 });
    let m = fg.add("mod", reg.build("dvbs_modulator", &params(p)).unwrap());
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, m, 0).unwrap();
    fg.connect(m, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let clean = out.lock().unwrap().clone();
    // σ per component for Es/N0 = 10 dB at signal power 0.25.
    let sigma = (0.25 * sps as f32 / 10.0 / 2.0).sqrt();
    let nz = cnoise(clean.len(), 9);
    let rate = symbol_rate * sps as f64;
    let data: Vec<Complex32> = clean
        .iter()
        .zip(&nz)
        .enumerate()
        .map(|(i, (&x, &n))| {
            let ph = std::f64::consts::TAU * 1_500.0 * i as f64 / rate;
            // cnoise is uniform with σ ≈ 0.29 per component.
            x * Complex32::from_polar(1.0, ph as f32) + n * (sigma / 0.2887)
        })
        .collect();
    let chain = Chain {
        data,
        rate,
        blocks: vec![(
            "dvbs_demodulator",
            json!({ "symbol_rate": symbol_rate, "rate": "3/4" }),
        )],
        sink_type: "u8",
    };
    bench_chain(c, "graph", "dvbs_demodulator_3_4_2sps", chain);
}

/// DVB-T 8k, 64-QAM, guard 1/32, C/N 25 dB, 2 kHz off, at the 8 MHz
/// elementary rate (9.14 MS/s): rate 2/3 (24.1 Mbit/s, the common UK/EU
/// mode) and 7/8 (31.7 Mbit/s, the most Viterbi work). 1000 symbols (0.92 s
/// of signal), of which the first ~140 are acquisition (TPS lock). Real time
/// is 9.14 M input samples/s.
fn dvbt_demodulator(c: &mut Criterion) {
    use foxygnal_blocks::testing::VectorSink;
    let reg = foxygnal_blocks::registry();
    let rate = 64e6 / 7.0;
    for (code, name) in [("2/3", "dvbt_8k_64qam_2_3"), ("7/8", "dvbt_8k_64qam_7_8")] {
        let packets = if code == "2/3" { 14_800 } else { 19_400 };
        let ts: Vec<u8> = (0..packets as u32)
            .flat_map(|i| {
                let mut p = vec![0x47, 0x01, 0x00, 0x10 | (i as u8 & 15)];
                p.extend((4..188u32).map(|k| (i.wrapping_mul(31) ^ k.wrapping_mul(97)) as u8));
                p
            })
            .collect();
        let mut fg = Flowgraph::new();
        let src = fg.add("src", VectorSource::instance(ts));
        let p = json!({ "mode": "8k", "constellation": "64qam", "rate": code, "guard": "1/32",
                        "bandwidth": "8", "cell_id": 1, "gain": 0.25 });
        let m = fg.add("mod", reg.build("dvbt_modulator", &params(p)).unwrap());
        let (sink, out) = VectorSink::<Complex32>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(src, 0, m, 0).unwrap();
        fg.connect(m, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        let clean = out.lock().unwrap().clone();
        // Noise for C/N 25 dB in the occupied 6817/8192 of the band.
        let sigma = (0.0625 / (6817.0 / 8192.0) / 10f32.powf(2.5) / 2.0).sqrt();
        let nz = cnoise(clean.len(), 11);
        let data: Vec<Complex32> = clean
            .iter()
            .zip(&nz)
            .enumerate()
            .map(|(i, (&x, &n))| {
                let ph = std::f64::consts::TAU * 2_000.0 * i as f64 / rate;
                x * Complex32::from_polar(1.0, ph as f32) + n * (sigma / 0.2887)
            })
            .collect();
        let chain = Chain {
            data,
            rate,
            blocks: vec![(
                "dvbt_demodulator",
                json!({ "mode": "auto", "guard": "auto", "bandwidth": "8" }),
            )],
            sink_type: "u8",
        };
        bench_chain(c, "graph", name, chain);
    }
}

criterion_group!(
    benches,
    blocks,
    sources,
    fm_receiver,
    dvbs_demodulator,
    dvbt_demodulator
);
criterion_main!(benches);
