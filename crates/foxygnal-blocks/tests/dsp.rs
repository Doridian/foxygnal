//! Filters, resamplers, demodulators and level blocks, run through real
//! flowgraphs with synthetic signals.

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, GraphSpec, Params, Sample, StreamMeta};
use serde_json::json;

fn params(v: serde_json::Value) -> Params {
    v.as_object().unwrap().clone()
}

fn meta(rate: f64) -> StreamMeta {
    StreamMeta::with_rate(rate)
}

/// Run `input` through one block; returns the output and its metadata.
fn run<I: Sample, O: Sample>(
    kind: &str,
    p: serde_json::Value,
    input: Vec<I>,
    m: StreamMeta,
) -> (Vec<O>, StreamMeta) {
    let reg = foxygnal_blocks::registry();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance_with_meta(input, m));
    let blk = fg.add("blk", reg.build(kind, &params(p)).unwrap());
    let (sink, out) = VectorSink::<O>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    let edges = fg.configure().unwrap();
    let out_meta = edges.iter().find(|e| e.from == "blk.out").unwrap().meta;
    fg.start().unwrap().wait().unwrap();
    let v = out.lock().unwrap().clone();
    (v, out_meta)
}

/// The configure error a block reports for these params at this rate.
fn configure_err(kind: &str, p: serde_json::Value, ty_c32: bool, m: StreamMeta) -> String {
    let reg = foxygnal_blocks::registry();
    let mut fg = Flowgraph::new();
    let src = if ty_c32 {
        fg.add(
            "src",
            VectorSource::<Complex32>::instance_with_meta(vec![], m),
        )
    } else {
        fg.add("src", VectorSource::<f32>::instance_with_meta(vec![], m))
    };
    let blk = fg.add("blk", reg.build(kind, &params(p)).unwrap());
    fg.connect(src, 0, blk, 0).unwrap();
    let err = fg.configure().unwrap_err();
    assert_eq!(err.block(), Some("blk"), "{err}");
    err.to_string()
}

fn real_tones(rate: f64, n: usize, tones: &[(f64, f64)]) -> Vec<f32> {
    (0..n)
        .map(|i| {
            tones
                .iter()
                .map(|(f, a)| a * (TAU * f * i as f64 / rate).sin())
                .sum::<f64>() as f32
        })
        .collect()
}

fn complex_tones(rate: f64, n: usize, tones: &[(f64, f64)]) -> Vec<Complex32> {
    (0..n)
        .map(|i| {
            tones
                .iter()
                .map(|(f, a)| {
                    let (s, c) = (TAU * f * i as f64 / rate).sin_cos();
                    Complex32::new((a * c) as f32, (a * s) as f32)
                })
                .sum()
        })
        .collect()
}

/// Amplitude of a real tone at `f` (single DFT bin over a whole number of
/// cycles).
fn real_amp(x: &[f32], rate: f64, f: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let w = TAU * f * i as f64 / rate;
        re += v as f64 * w.cos();
        im -= v as f64 * w.sin();
    }
    2.0 * re.hypot(im) / x.len() as f64
}

/// Amplitude of a complex tone at `f` (negative = below 0 Hz).
fn complex_amp(x: &[Complex32], rate: f64, f: f64) -> f64 {
    let mut acc = num_complex::Complex64::new(0.0, 0.0);
    for (i, v) in x.iter().enumerate() {
        let w = -TAU * f * i as f64 / rate;
        acc += num_complex::Complex64::new(v.re as f64, v.im as f64)
            * num_complex::Complex64::from_polar(1.0, w);
    }
    acc.norm() / x.len() as f64
}

fn db(a: f64) -> f64 {
    20.0 * a.log10()
}

/// The last `n` samples (past the filter's start-up).
fn tail<T>(x: &[T], n: usize) -> &[T] {
    &x[x.len() - n..]
}

// ---- FIR -------------------------------------------------------------------

#[test]
fn fir_low_pass_keeps_the_passband_and_cuts_the_stopband() {
    let rate = 48_000.0;
    let input = real_tones(rate, 24_000, &[(2_000.0, 1.0), (10_000.0, 1.0)]);
    let (out, m) = run::<f32, f32>(
        "fir_filter",
        json!({"type": "f32", "cutoff": 5000.0, "transition": 1000.0}),
        input,
        meta(rate),
    );
    assert_eq!(out.len(), 24_000);
    assert_eq!(m.sample_rate, Some(rate));
    let t = tail(&out, 4_800);
    let pass = real_amp(t, rate, 2_000.0);
    let stop = real_amp(t, rate, 10_000.0);
    assert!((pass - 1.0).abs() < 0.01, "passband amplitude {pass}");
    assert!(db(stop) < -50.0, "stopband {} dB", db(stop));
}

#[test]
fn fir_band_pass_on_complex_with_gain() {
    let rate = 48_000.0;
    let input = complex_tones(
        rate,
        24_000,
        &[(6_000.0, 1.0), (12_000.0, 1.0), (1_000.0, 1.0)],
    );
    let (out, _) = run::<Complex32, Complex32>(
        "fir_filter",
        json!({"type": "c32", "mode": "band_pass", "cutoff": 4000.0, "cutoff_high": 8000.0,
               "transition": 1000.0, "window": "blackman", "gain": 2.0}),
        input,
        meta(rate),
    );
    let t = tail(&out, 4_800);
    assert!((complex_amp(t, rate, 6_000.0) - 2.0).abs() < 0.01);
    assert!(db(complex_amp(t, rate, 12_000.0)) < -65.0);
    assert!(db(complex_amp(t, rate, 1_000.0)) < -65.0);
}

#[test]
fn fir_high_pass_and_band_stop_with_kaiser() {
    let rate = 48_000.0;
    let input = real_tones(rate, 24_000, &[(1_000.0, 1.0), (10_000.0, 1.0)]);
    let (out, _) = run::<f32, f32>(
        "fir_filter",
        json!({"type": "f32", "mode": "high_pass", "cutoff": 5000.0, "transition": 1000.0,
               "window": "kaiser", "attenuation": 70.0}),
        input.clone(),
        meta(rate),
    );
    let t = tail(&out, 4_800);
    assert!((real_amp(t, rate, 10_000.0) - 1.0).abs() < 0.01);
    assert!(db(real_amp(t, rate, 1_000.0)) < -65.0);

    let (out, _) = run::<f32, f32>(
        "fir_filter",
        json!({"type": "f32", "mode": "band_stop", "cutoff": 8000.0, "cutoff_high": 12000.0,
               "transition": 1000.0, "window": "kaiser", "attenuation": 70.0}),
        input,
        meta(rate),
    );
    let t = tail(&out, 4_800);
    assert!((real_amp(t, rate, 1_000.0) - 1.0).abs() < 0.01);
    assert!(db(real_amp(t, rate, 10_000.0)) < -65.0);
}

#[test]
fn fir_params_are_checked_against_the_input_rate() {
    let err = configure_err(
        "fir_filter",
        json!({"type": "f32", "cutoff": 30000.0}),
        false,
        meta(48_000.0),
    );
    assert!(
        err.contains("cutoff 30 kHz") && err.contains("24 kHz"),
        "{err}"
    );

    let err = configure_err(
        "fir_filter",
        json!({"type": "f32", "mode": "band_pass", "cutoff": 9000.0, "cutoff_high": 8000.0}),
        false,
        meta(48_000.0),
    );
    assert!(err.contains("must be below the high cutoff"), "{err}");

    let err = configure_err(
        "fir_filter",
        json!({"type": "f32", "transition": 1.0}),
        false,
        meta(48_000.0),
    );
    assert!(err.contains("taps"), "{err}");

    let err = configure_err(
        "fir_filter",
        json!({"type": "f32"}),
        false,
        StreamMeta::default(),
    );
    assert!(err.contains("sample rate"), "{err}");
}

#[test]
fn fir_cutoff_retunes_while_running() {
    // A 4 kHz tone: blocked at cutoff 2 kHz, passed after a live retune to 8 kHz.
    let rate = 48_000.0;
    let reg = foxygnal_blocks::registry();
    let spec = GraphSpec::from_toml(
        r#"
        [[block]]
        id = "src"
        kind = "signal_source"
        params = { type = "f32", frequency = 4000.0, sample_rate = 48000.0 }
        [[block]]
        id = "thr"
        kind = "throttle"
        params = { type = "f32" }
        [[block]]
        id = "fir"
        kind = "fir_filter"
        params = { type = "f32", cutoff = 2000.0, transition = 1000.0 }
        [[block]]
        id = "null"
        kind = "null_sink"
        params = { type = "f32" }
        [[connect]]
        from = "src.out"
        to = "thr.in"
        [[connect]]
        from = "thr.out"
        to = "fir.in"
        [[connect]]
        from = "fir.out"
        to = "null.in"
        "#,
    )
    .unwrap();
    let fg = Flowgraph::from_spec(&spec, &reg).unwrap();
    let running = fg.start().unwrap();
    let mon = running.monitor();
    let (_, tap) = mon.tap("fir", "out").unwrap();
    let peak = |tap: &foxygnal_core::Tap| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut buf = vec![0u8; 4 * 2400];
        let got = tap.read(tap.written().saturating_sub(2400), &mut buf);
        buf[..got.items * 4]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()).abs())
            .fold(0.0f32, f32::max)
    };
    assert!(peak(&tap) < 0.01, "stopband before retune");
    // An out-of-range value is ignored (keeps the old taps, keeps running).
    running
        .param_sender()
        .send("fir", "cutoff", json!(rate))
        .unwrap();
    running
        .param_sender()
        .send("fir", "cutoff", json!(8000.0))
        .unwrap();
    let p = peak(&tap);
    assert!((p - 1.0).abs() < 0.02, "passband after retune: {p}");
    running.stop();
    running.wait().unwrap();
}

// ---- Rate changes ---------------------------------------------------------------

#[test]
fn decimator_divides_the_rate_and_removes_aliases() {
    let rate = 48_000.0;
    let input = complex_tones(rate, 48_000, &[(1_000.0, 1.0), (10_000.0, 1.0)]);
    let (out, m) = run::<Complex32, Complex32>(
        "decimator",
        json!({"type": "c32", "decimation": 4}),
        input,
        StreamMeta {
            sample_rate: Some(rate),
            center_freq: Some(100e6),
        },
    );
    assert_eq!(m.sample_rate, Some(12_000.0));
    assert_eq!(m.center_freq, Some(100e6));
    assert_eq!(out.len(), 12_000);
    let t = tail(&out, 6_000);
    assert!((complex_amp(t, 12_000.0, 1_000.0) - 1.0).abs() < 0.01);
    // 10 kHz would alias to −2 kHz.
    let alias = complex_amp(t, 12_000.0, -2_000.0);
    assert!(db(alias) < -55.0, "alias {} dB", db(alias));
}

#[test]
fn decimator_works_without_a_known_rate() {
    let (out, m) = run::<f32, f32>(
        "decimator",
        json!({"type": "f32", "decimation": 3}),
        vec![1.0; 300],
        StreamMeta::default(),
    );
    assert_eq!(m.sample_rate, None);
    assert_eq!(out.len(), 100);
    assert!((out[99] - 1.0).abs() < 1e-3, "DC gain 1: {}", out[99]);
}

#[test]
fn interpolator_keeps_amplitude() {
    let rate = 12_000.0;
    let input = real_tones(rate, 12_000, &[(1_000.0, 1.0)]);
    let (out, m) = run::<f32, f32>(
        "interpolator",
        json!({"type": "f32", "interpolation": 4}),
        input,
        meta(rate),
    );
    assert_eq!(m.sample_rate, Some(48_000.0));
    assert_eq!(out.len(), 48_000);
    let t = tail(&out, 24_000);
    assert!((real_amp(t, 48_000.0, 1_000.0) - 1.0).abs() < 0.01);
    // Images at 11 kHz and 13 kHz are gone.
    assert!(db(real_amp(t, 48_000.0, 11_000.0)) < -55.0);
    assert!(db(real_amp(t, 48_000.0, 13_000.0)) < -55.0);
}

#[test]
fn rational_resampler_to_a_target_rate() {
    let rate = 44_100.0;
    let input = real_tones(rate, 44_100, &[(1_000.0, 0.5)]);
    let (out, m) = run::<f32, f32>(
        "rational_resampler",
        json!({"type": "f32", "output_rate": 48000.0}),
        input,
        meta(rate),
    );
    assert_eq!(m.sample_rate, Some(48_000.0));
    assert!((out.len() as i64 - 48_000).abs() <= 1, "{}", out.len());
    let t = &out[out.len() - 24_000..];
    assert!((real_amp(t, 48_000.0, 1_000.0) - 0.5).abs() < 0.005);

    // By integers: 3/2, reduced from 6/4.
    let (out, m) = run::<f32, f32>(
        "rational_resampler",
        json!({"type": "f32", "output_rate": 0.0, "interpolation": 6, "decimation": 4}),
        vec![0.0; 1000],
        meta(1000.0),
    );
    assert_eq!(m.sample_rate, Some(1500.0));
    assert_eq!(out.len(), 1500);
}

#[test]
fn rational_resampler_errors_are_actionable() {
    let err = configure_err(
        "rational_resampler",
        json!({"type": "f32", "output_rate": 48000.0}),
        false,
        StreamMeta::default(),
    );
    assert!(err.contains("input sample rate"), "{err}");
    let err = configure_err(
        "rational_resampler",
        json!({"type": "f32", "output_rate": 48000.5}),
        false,
        meta(44_100.0),
    );
    assert!(err.contains("whole-number"), "{err}");
    let err = configure_err(
        "rational_resampler",
        json!({"type": "f32", "output_rate": 48001.0}),
        false,
        meta(44_100.0),
    );
    assert!(err.contains("too complex"), "{err}");
}

// ---- Frequency-translating FIR ---------------------------------------------------

#[test]
fn xlating_fir_picks_a_channel_and_moves_the_centre() {
    let rate = 240_000.0;
    let input = complex_tones(rate, 240_000, &[(51_000.0, 1.0), (0.0, 1.0)]);
    let (out, m) = run::<Complex32, Complex32>(
        "freq_xlating_fir",
        json!({"offset": 50000.0, "cutoff": 10000.0, "transition": 5000.0, "decimation": 4}),
        input,
        StreamMeta {
            sample_rate: Some(rate),
            center_freq: Some(100e6),
        },
    );
    assert_eq!(m.sample_rate, Some(60_000.0));
    assert_eq!(m.center_freq, Some(100.05e6));
    assert_eq!(out.len(), 60_000);
    let t = tail(&out, 30_000);
    assert!((complex_amp(t, 60_000.0, 1_000.0) - 1.0).abs() < 0.01);
    // The DC tone would sit at −50 kHz, which aliases to +10 kHz at 60 kS/s.
    let rejected = complex_amp(t, 60_000.0, 10_000.0);
    assert!(db(rejected) < -50.0, "{} dB", db(rejected));

    let err = configure_err(
        "freq_xlating_fir",
        json!({"offset": 130000.0}),
        true,
        meta(rate),
    );
    assert!(err.contains("offset 130 kHz"), "{err}");
    let err = configure_err(
        "freq_xlating_fir",
        json!({"cutoff": 40000.0, "decimation": 4}),
        true,
        meta(rate),
    );
    assert!(err.contains("Nyquist") && err.contains("30 kHz"), "{err}");
}

/// Run `input` through one block with a given buffer capacity.
fn run_with_capacity<I: Sample, O: Sample>(
    kind: &str,
    p: serde_json::Value,
    input: Vec<I>,
    m: StreamMeta,
    capacity: usize,
) -> Vec<O> {
    let reg = foxygnal_blocks::registry();
    let mut fg = Flowgraph::new().with_buffer_capacity(capacity);
    let src = fg.add("src", VectorSource::instance_with_meta(input, m));
    let blk = fg.add("blk", reg.build(kind, &params(p)).unwrap());
    let (sink, out) = VectorSink::<O>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, blk, 0).unwrap();
    fg.connect(blk, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    out.lock().unwrap().clone()
}

#[test]
fn xlating_fir_output_does_not_depend_on_buffer_sizes() {
    // The mixer runs in blocks ahead of the filter; whatever the buffer
    // sizes, it must stay in step with the samples the filter takes.
    let rate = 240_000.0;
    let input = complex_tones(rate, 30_000, &[(51_000.0, 1.0), (47_500.0, 0.5)]);
    let p = json!({"offset": 50000.0, "cutoff": 10000.0, "transition": 5000.0, "decimation": 4});
    let want: Vec<Complex32> = run_with_capacity(
        "freq_xlating_fir",
        p.clone(),
        input.clone(),
        meta(rate),
        1 << 16,
    );
    assert_eq!(want.len(), 7_500);
    for cap in [5, 37, 1000, 4099] {
        let got: Vec<Complex32> = run_with_capacity(
            "freq_xlating_fir",
            p.clone(),
            input.clone(),
            meta(rate),
            cap,
        );
        assert_eq!(got.len(), want.len(), "capacity {cap}");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert!(
                (g - w).norm() < 1e-4,
                "capacity {cap}, item {i}: {g} vs {w}"
            );
        }
    }
}

// ---- Demodulators ----------------------------------------------------------------

#[test]
fn fm_demod_matches_an_exact_discriminator() {
    // The demodulator uses a fast atan2; compare with f64 atan2 on the same
    // samples. Output scale: ±deviation → ±1.
    let rate = 240_000.0;
    let input = fm_signal(rate, 20_000, 75_000.0, 3_000.0, 0.9);
    let out: Vec<f32> = run_with_capacity(
        "fm_demod",
        json!({"deviation": 75000.0, "deemphasis": "off"}),
        input.clone(),
        meta(rate),
        777,
    );
    let gain = rate / (TAU * 75_000.0);
    let mut prev = Complex32::new(0.0, 0.0);
    let mut worst = 0.0f64;
    for (o, &x) in out.iter().zip(&input) {
        let z = x * prev.conj();
        let want = (z.im as f64).atan2(z.re as f64) * gain;
        worst = worst.max((*o as f64 - want).abs());
        prev = x;
    }
    // ~1e-7 rad of atan2 error × gain 0.51, plus f32 rounding: about
    // −130 dB below full deviation.
    assert!(worst < 1e-6, "worst error {worst}");
}

/// FM: phase = 2π·dev/rate·Σ m.
fn fm_signal(rate: f64, n: usize, dev: f64, tone: f64, amp: f64) -> Vec<Complex32> {
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let m = amp * (TAU * tone * i as f64 / rate).sin();
            phase += TAU * dev * m / rate;
            Complex32::from_polar(1.0, phase as f32)
        })
        .collect()
}

#[test]
fn fm_demod_recovers_the_modulating_tone() {
    let rate = 240_000.0;
    let input = fm_signal(rate, 48_000, 75_000.0, 1_000.0, 0.5);
    let (out, m) = run::<Complex32, f32>(
        "fm_demod",
        json!({"deviation": 75000.0, "deemphasis": "off"}),
        input.clone(),
        StreamMeta {
            sample_rate: Some(rate),
            center_freq: Some(100e6),
        },
    );
    assert_eq!(m.sample_rate, Some(rate));
    assert_eq!(m.center_freq, None, "audio has no RF centre");
    let t = tail(&out, 24_000);
    let a = real_amp(t, rate, 1_000.0);
    assert!((a - 0.5).abs() < 0.005, "amplitude {a}");

    // 50 µs de-emphasis: |H(1 kHz)| = 1/√(1 + (2π·1k·50µ)²) ≈ 0.954.
    let (out, _) = run::<Complex32, f32>(
        "fm_demod",
        json!({"deviation": 75000.0, "deemphasis": "50us"}),
        input,
        meta(rate),
    );
    let a = real_amp(tail(&out, 24_000), rate, 1_000.0);
    let want = 0.5 / (1.0 + (TAU * 1000.0 * 50e-6f64).powi(2)).sqrt();
    assert!((a - want).abs() < 0.005, "{a} vs {want}");

    let err = configure_err(
        "fm_demod",
        json!({"deviation": 75000.0}),
        true,
        meta(48_000.0),
    );
    assert!(err.contains("deviation 75 kHz"), "{err}");
}

#[test]
fn am_demod_recovers_the_envelope() {
    let rate = 48_000.0;
    let input: Vec<Complex32> = (0..48_000)
        .map(|i| {
            let env = 1.0 + 0.5 * (TAU * 1000.0 * i as f64 / rate).sin();
            Complex32::from_polar(env as f32, 0.7)
        })
        .collect();
    let (out, _) = run::<Complex32, f32>("am_demod", json!({}), input, meta(rate));
    let t = tail(&out, 24_000);
    assert!((real_amp(t, rate, 1_000.0) - 0.5).abs() < 0.01);
    let mean = t.iter().map(|&v| v as f64).sum::<f64>() / t.len() as f64;
    assert!(mean.abs() < 0.01, "DC removed: {mean}");
}

#[test]
fn ssb_demod_keeps_one_sideband() {
    let rate = 48_000.0;
    let input = complex_tones(rate, 48_000, &[(1_000.0, 1.0), (-2_000.0, 1.0)]);
    for (sideband, keep, drop) in [("usb", 1_000.0, 2_000.0), ("lsb", 2_000.0, 1_000.0)] {
        let (out, _) = run::<Complex32, f32>(
            "ssb_demod",
            json!({"sideband": sideband}),
            input.clone(),
            meta(rate),
        );
        assert_eq!(out.len(), 48_000);
        let t = tail(&out, 24_000);
        let k = real_amp(t, rate, keep);
        let d = real_amp(t, rate, drop);
        assert!((k - 1.0).abs() < 0.01, "{sideband}: kept {k}");
        assert!(db(d) < -50.0, "{sideband}: other side {} dB", db(d));
    }
}

// ---- Level -----------------------------------------------------------------------

#[test]
fn agc_brings_quiet_signals_to_the_target() {
    let rate = 48_000.0;
    let input = complex_tones(rate, 48_000, &[(1_000.0, 0.01)]);
    let (out, _) = run::<Complex32, Complex32>(
        "agc",
        json!({"type": "c32", "target": 0.5}),
        input,
        meta(rate),
    );
    for v in tail(&out, 1000) {
        assert!((v.norm() - 0.5).abs() < 0.01, "{}", v.norm());
    }

    let input = real_tones(rate, 48_000, &[(1_000.0, 0.01)]);
    let (out, _) = run::<f32, f32>("agc", json!({"type": "f32"}), input, meta(rate));
    let peak = tail(&out, 4800).iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((0.95..1.1).contains(&peak), "peak {peak}");
}

#[test]
fn squelch_mutes_weak_signals_and_passes_strong_ones() {
    let rate = 48_000.0;
    // −63 dB average power: muted.
    let weak = real_tones(rate, 9_600, &[(1_000.0, 0.001)]);
    let (out, _) = run::<f32, f32>(
        "squelch",
        json!({"type": "f32", "threshold": -40.0}),
        weak,
        meta(rate),
    );
    assert_eq!(out.len(), 9_600, "never drops samples");
    assert!(out.iter().all(|&v| v == 0.0));

    // −23 dB: passes unchanged once open.
    let strong = complex_tones(rate, 9_600, &[(1_000.0, 0.1)]);
    let (out, _) = run::<Complex32, Complex32>(
        "squelch",
        json!({"type": "c32", "threshold": -40.0}),
        strong.clone(),
        meta(rate),
    );
    assert_eq!(out.len(), 9_600);
    assert_eq!(out[0], Complex32::new(0.0, 0.0), "opens after averaging");
    assert_eq!(tail(&out, 4800), tail(&strong, 4800));
}

// ---- Frequency-response previews --------------------------------------------------

#[test]
fn response_previews() {
    let reg = foxygnal_blocks::registry();
    let r = reg
        .response(
            "fir_filter",
            &params(json!({"cutoff": 5000.0, "transition": 1000.0})),
            48_000.0,
        )
        .unwrap()
        .expect("fir_filter has a preview");
    assert_eq!(r.freqs.len(), r.db.len());
    assert_eq!(r.freqs[0], 0.0);
    assert_eq!(*r.freqs.last().unwrap(), 24_000.0);
    assert!(r.db[0].abs() < 0.01);
    assert_eq!(r.taps % 2, 1);
    assert!(r.taps > 100);

    // Two-sided, centred on the offset.
    let r = reg
        .response(
            "freq_xlating_fir",
            &params(json!({"offset": 50000.0, "cutoff": 10000.0})),
            240_000.0,
        )
        .unwrap()
        .unwrap();
    assert_eq!(r.freqs[0], -120_000.0);
    let at = |f: f64| {
        let i = r.freqs.iter().position(|&x| x >= f).expect("in range");
        r.db[i]
    };
    assert!(at(50_000.0).abs() < 0.1);
    assert!(at(-50_000.0) < -50.0);

    for kind in [
        "decimator",
        "interpolator",
        "rational_resampler",
        "ssb_demod",
    ] {
        let r = reg.response(kind, &Params::new(), 48_000.0).unwrap();
        // Unity gain somewhere in the passband (0 dB peak).
        let peak = r.unwrap().db.iter().cloned().fold(f32::MIN, f32::max);
        assert!(peak.abs() < 0.1, "{kind}: {peak}");
    }

    assert!(
        reg.response("add", &Params::new(), 48_000.0)
            .unwrap()
            .is_none()
    );
    let err = reg
        .response("fir_filter", &params(json!({"cutoff": 30000.0})), 48_000.0)
        .unwrap_err();
    assert!(err.to_string().contains("24 kHz"), "{err}");

    let cat = serde_json::to_value(reg.catalog().collect::<Vec<_>>()).unwrap();
    let entry = |kind: &str| {
        cat.as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == kind)
            .unwrap()
            .clone()
    };
    assert_eq!(entry("fir_filter")["response"], json!(true));
    assert_eq!(entry("fir_filter")["category"], json!("Filters"));
    assert!(entry("add").get("response").is_none());
}

// ---- Examples ----------------------------------------------------------------------

#[test]
fn fm_receiver_example_configures() {
    let spec =
        GraphSpec::from_toml(include_str!("../../../examples/fm-receiver.fox.toml")).unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let edges = fg.configure().unwrap();
    let rate_into = |to: &str| {
        edges
            .iter()
            .find(|e| e.to == to)
            .unwrap_or_else(|| panic!("no edge into {to}"))
            .meta
            .sample_rate
    };
    assert_eq!(rate_into("demod.in"), Some(240_000.0));
    assert_eq!(rate_into("audio.in"), Some(48_000.0));
}
