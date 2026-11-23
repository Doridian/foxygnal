//! `peak_frequency`: finding the strongest signal's offset.

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, Params, StreamMeta};
use serde_json::json;

const RATE: f64 = 240_000.0;

/// Deterministic Gaussian-ish noise (sum of uniforms).
struct Noise(u64);

impl Noise {
    fn uniform(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    }

    fn sample(&mut self, sigma: f32) -> Complex32 {
        let g = |n: &mut Self| (0..4).map(|_| n.uniform()).sum::<f32>() * sigma * 1.7;
        Complex32::new(g(self), g(self))
    }
}

/// `secs` of tones (offset Hz, amplitude) plus noise, appended to `out`.
fn signal(out: &mut Vec<Complex32>, secs: f64, tones: &[(f64, f32)], noise: &mut Noise) {
    let start = out.len();
    let n = (secs * RATE) as usize;
    for i in 0..n {
        let t = (start + i) as f64 / RATE;
        let mut v = noise.sample(0.05);
        for &(f, a) in tones {
            let ph = std::f64::consts::TAU * f * t;
            v += Complex32::new((ph.cos() as f32) * a, (ph.sin() as f32) * a);
        }
        out.push(v);
    }
}

fn run(input: Vec<Complex32>, params: serde_json::Value) -> Vec<f32> {
    let reg = foxygnal_blocks::registry();
    let p: Params = params.as_object().unwrap().clone();
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(RATE)),
    );
    let pf = fg.add("pf", reg.build("peak_frequency", &p).unwrap());
    let (sink, out) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, pf, 0).unwrap();
    fg.connect(pf, 0, sink, 0).unwrap();
    let mut fg = fg;
    let edges = fg.configure().unwrap();
    let rate = edges
        .iter()
        .find(|e| e.from == "pf.out")
        .unwrap()
        .meta
        .sample_rate
        .unwrap();
    // 240 kS/s / 1024 per spectrum / 12 spectra per value ≈ 19.5 values/s.
    assert!((rate - 19.53).abs() < 0.1, "{rate}");
    fg.start().unwrap().wait().unwrap();
    out.lock().unwrap().clone()
}

/// Bin width at these settings.
const BIN: f32 = (RATE / 1024.0) as f32;

fn base() -> serde_json::Value {
    json!({"fft_size": 1024, "update_rate": 20.0})
}

#[test]
fn finds_a_tone_in_noise_and_holds_on_noise() {
    let mut noise = Noise(1);
    let mut x = Vec::new();
    signal(&mut x, 0.2, &[], &mut noise);
    signal(&mut x, 0.5, &[(31_000.0, 0.3)], &mut noise);
    signal(&mut x, 0.5, &[], &mut noise);
    let out = run(x, base());
    // Nothing during the first 0.2 s of noise, then a value per update
    // for the remaining second (≈ 19.5).
    assert!(
        (18..=21).contains(&out.len()),
        "{} values: {out:?}",
        out.len()
    );
    for v in &out {
        assert!((v - 31_000.0).abs() < BIN, "{v}");
    }
    // During the last 0.5 s of noise the value is held exactly.
    let tail = &out[out.len() - 8..];
    assert!(tail.iter().all(|&v| v == tail[0]), "{tail:?}");
}

#[test]
fn negative_offsets_and_search_range() {
    let mut noise = Noise(2);
    let mut x = Vec::new();
    signal(
        &mut x,
        0.5,
        &[(50_000.0, 0.5), (-20_000.0, 0.1)],
        &mut noise,
    );
    let out = run(x.clone(), base());
    assert!((out.last().unwrap() - 50_000.0).abs() < BIN, "{out:?}");
    let mut p = base();
    p["min_offset"] = json!(-30_000.0);
    p["max_offset"] = json!(0.0);
    let out = run(x, p);
    assert!((out.last().unwrap() + 20_000.0).abs() < BIN, "{out:?}");
}

#[test]
fn hysteresis_keeps_the_current_signal_until_another_is_clearly_stronger() {
    let mut noise = Noise(3);
    let mut x = Vec::new();
    let (a, b) = (20_000.0, -40_000.0);
    signal(&mut x, 0.3, &[(a, 0.5)], &mut noise);
    // B now 1.6 dB stronger than A: within 3 dB of hysteresis.
    signal(&mut x, 0.3, &[(a, 0.5), (b, 0.6)], &mut noise);
    let stage2 = x.len();
    // B 6 dB stronger: switch.
    signal(&mut x, 0.3, &[(a, 0.5), (b, 1.0)], &mut noise);
    let per_s = 19.53;
    let at = |samples: usize| (samples as f64 / RATE * per_s) as usize;

    let out = run(x.clone(), base());
    let near = |v: f32, f: f64| (v - f as f32).abs() < BIN;
    assert!(out[..at(stage2)].iter().all(|&v| near(v, a)), "{out:?}");
    assert!(near(*out.last().unwrap(), b), "{out:?}");

    // Without hysteresis it jumps as soon as B is stronger.
    let mut p = base();
    p["hysteresis"] = json!(0.0);
    let out = run(x, p);
    assert!(near(out[at(stage2) - 2], b), "{out:?}");
}

#[test]
fn signal_width_finds_the_centre_of_a_wide_signal() {
    // A wide signal: several tones spread ±30 kHz around +10 kHz, the
    // strongest one off-centre. With the width set, the output is the centre
    // of the power, not the strongest bin.
    let mut noise = Noise(4);
    let mut x = Vec::new();
    let tones = [
        (-20_000.0, 0.2),
        (0.0, 0.3),
        (10_000.0, 0.3),
        (20_000.0, 0.3),
        (40_000.0, 0.2),
    ];
    signal(&mut x, 0.3, &tones, &mut noise);
    let mut p = base();
    p["bandwidth"] = json!(80_000.0);
    let out = run(x, p);
    let v = *out.last().unwrap();
    assert!((v - 10_000.0).abs() < 2_000.0, "{v}");
}
