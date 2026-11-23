//! RRC, biquad, DC blocker and Hilbert blocks against closed forms, run
//! through real graphs (tiny plain buffers, so slices split at the wrap).

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{
    BufferKind, Complex32, Flowgraph, Instance, Sample, StreamMeta, Tag, TagValue,
};
use serde_json::json;

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

/// `input` → blocks → sink at `rate`; returns output, tags and the last
/// block's output metadata. `small` uses tiny plain buffers.
fn chain<I: Sample, O: Sample>(
    input: Vec<I>,
    tags: Vec<Tag>,
    rate: f64,
    blocks: Vec<Instance>,
    small: bool,
) -> (Vec<O>, Vec<Tag>, StreamMeta) {
    let mut fg = if small {
        Flowgraph::new()
            .with_buffer_capacity(37)
            .with_buffer_kind(BufferKind::Plain)
    } else {
        Flowgraph::new()
    };
    let src = if tags.is_empty() {
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(rate))
    } else {
        // Tagged sources don't carry metadata; the rate is checked separately.
        VectorSource::with_tags(input, tags)
    };
    let mut prev = fg.add("src", src);
    let n = blocks.len();
    for (i, b) in blocks.into_iter().enumerate() {
        let id = fg.add(format!("b{i}"), b);
        fg.connect(prev, 0, id, 0).unwrap();
        prev = id;
    }
    let (sink, out, got) = VectorSink::<O>::with_tags();
    let sink = fg.add("sink", sink);
    fg.connect(prev, 0, sink, 0).unwrap();
    let edges = fg.configure().unwrap();
    let last = format!("b{}.out", n - 1);
    let m = edges.iter().find(|e| e.from == last).unwrap().meta;
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap().clone();
    let got = got.lock().unwrap().clone();
    (out, got, m)
}

fn tone(f: f64, n: usize) -> Vec<f32> {
    (0..n).map(|i| (TAU * f * i as f64).cos() as f32).collect()
}

/// Amplitude of the second half of `y` (RMS × √2).
fn amplitude(y: &[f32]) -> f64 {
    let t = &y[y.len() / 2..];
    (2.0 * t.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / t.len() as f64).sqrt()
}

fn db(m: f64) -> f64 {
    20.0 * m.log10()
}

#[test]
fn biquad_gain_at_f0_matches_the_cookbook() {
    let rate = 48_000.0;
    let f0 = 1_000.0;
    let x = tone(f0 / rate, 24_000);
    for (shape, q, gain, want_db) in [
        ("low_pass", 2.0, 0.0, db(2.0)), // |H(f0)| = Q
        ("high_pass", 0.5, 0.0, db(0.5)),
        ("band_pass", 4.0, 0.0, 0.0),
        ("peaking", 1.0, 9.0, 9.0),
        ("low_shelf", 0.707, -12.0, -6.0),
        ("high_shelf", 0.707, 8.0, 4.0),
    ] {
        let b = block(
            "biquad",
            json!({"type": "f32", "shape": shape, "freq": f0, "q": q, "gain_db": gain}),
        );
        let (y, _, _) = chain::<f32, f32>(x.clone(), vec![], rate, vec![b], true);
        let got = db(amplitude(&y));
        assert!(
            (got - want_db).abs() < 0.05,
            "{shape}: {got:.3} dB, want {want_db:.3}"
        );
    }
    let b = block(
        "biquad",
        json!({"type": "f32", "shape": "notch", "freq": f0, "q": 2.0}),
    );
    let (y, _, _) = chain::<f32, f32>(x.clone(), vec![], rate, vec![b], true);
    assert!(db(amplitude(&y)) < -40.0);
}

#[test]
fn biquad_cascade_and_complex() {
    let rate = 48_000.0;
    // Two Butterworth stages: −6 dB at the cutoff, both I and Q.
    let x: Vec<Complex32> = (0..24_000)
        .map(|i| {
            let p = TAU * 1_000.0 / rate * i as f64;
            Complex32::new(p.cos() as f32, p.sin() as f32)
        })
        .collect();
    let b = block(
        "biquad",
        json!({"type": "c32", "shape": "low_pass", "freq": 1_000.0, "stages": 2}),
    );
    let (y, _, _) = chain::<Complex32, Complex32>(x, vec![], rate, vec![b], true);
    let re: Vec<f32> = y.iter().map(|v| v.re).collect();
    let im: Vec<f32> = y.iter().map(|v| v.im).collect();
    for part in [re, im] {
        let got = db(amplitude(&part));
        assert!((got + 6.02).abs() < 0.05, "{got}");
    }
}

#[test]
fn dc_blocker_removes_the_offset() {
    let rate = 48_000.0;
    let x: Vec<f32> = tone(2_000.0 / rate, 48_000)
        .into_iter()
        .map(|v| 0.5 * v + 0.8)
        .collect();
    let b = block("dc_blocker", json!({"type": "f32", "cutoff": 20.0}));
    let (y, _, _) = chain::<f32, f32>(x, vec![], rate, vec![b], true);
    let tail = &y[y.len() - 12_000..];
    let mean = tail.iter().map(|&v| v as f64).sum::<f64>() / tail.len() as f64;
    assert!(mean.abs() < 1e-3, "DC left: {mean}");
    assert!((amplitude(&y) - 0.5).abs() < 0.005);

    // Complex: a constant I/Q offset decays away.
    let x = vec![Complex32::new(0.3, -0.7); 20_000];
    let b = block("dc_blocker", json!({"type": "c32", "cutoff": 100.0}));
    let (y, _, _) = chain::<Complex32, Complex32>(x, vec![], rate, vec![b], true);
    assert!(y.last().unwrap().norm() < 1e-4, "{}", y.last().unwrap());

    // The cutoff must stay below a quarter of the rate.
    let reg = foxygnal_blocks::registry();
    let p = json!({"cutoff": 20_000.0});
    let e = reg
        .response("dc_blocker", p.as_object().unwrap(), rate)
        .unwrap_err();
    assert!(e.to_string().contains("quarter"), "{e}");
}

#[test]
fn rrc_pair_has_no_isi_at_symbol_instants() {
    let sps = 4usize;
    let span = 12;
    // Pseudo-random QPSK symbols.
    let mut s = 0x1234_5678u32;
    let syms: Vec<Complex32> = (0..400)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let (i, q) = (s & 1, (s >> 1) & 1);
            Complex32::new(1.0 - 2.0 * i as f32, 1.0 - 2.0 * q as f32)
        })
        .collect();
    let blocks = vec![
        block(
            "rrc_filter",
            json!({"mode": "interpolate", "sps": sps, "span": span, "rolloff": 0.35}),
        ),
        block(
            "rrc_filter",
            json!({"sps": sps, "span": span, "rolloff": 0.35}),
        ),
    ];
    let (y, _, meta) = chain::<Complex32, Complex32>(syms.clone(), vec![], 1_000.0, blocks, true);
    assert_eq!(meta.sample_rate, Some(4_000.0));
    assert_eq!(y.len(), syms.len() * sps);
    // Each filter delays by (taps − 1)/2 = span·sps/2 samples.
    let delay = span * sps;
    let mut worst = 0f32;
    for (k, &want) in syms
        .iter()
        .enumerate()
        .skip(span)
        .take(syms.len() - 2 * span)
    {
        let got = y[k * sps + delay];
        worst = worst.max((got - want).norm());
    }
    assert!(worst < 0.03, "worst symbol error {worst}");
}

#[test]
fn rrc_interpolation_scales_tags_and_checks_sps() {
    let tags = vec![Tag::new(3, "burst", true)];
    let b = block(
        "rrc_filter",
        json!({"type": "f32", "mode": "interpolate", "sps": 8}),
    );
    let (y, got, _) = chain::<f32, f32>(vec![1.0; 20], tags, 1.0, vec![b], true);
    assert_eq!(y.len(), 160);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].offset, &got[0].value), (24, &TagValue::Bool(true)));

    let reg = foxygnal_blocks::registry();
    let p = json!({"mode": "interpolate", "sps": 2.5});
    let e = reg
        .build("rrc_filter", p.as_object().unwrap())
        .err()
        .unwrap();
    assert!(e.to_string().contains("whole number"), "{e}");
    // Fractional sps is fine for the matched filter.
    let p = json!({"sps": 2.5});
    assert!(reg.build("rrc_filter", p.as_object().unwrap()).is_ok());
}

#[test]
fn hilbert_shifts_by_90_degrees_with_unit_magnitude() {
    let taps = 101;
    let d = (taps - 1) / 2;
    for f in [0.03, 0.1, 0.25, 0.4] {
        let x = tone(f, 3_000);
        let b = block("hilbert", json!({"taps": taps, "window": "blackman"}));
        let (y, _, _) = chain::<f32, Complex32>(x, vec![], 48_000.0, vec![b], true);
        assert_eq!(y.len(), 3_000);
        for (n, v) in y.iter().enumerate().skip(2 * taps) {
            // Analytic signal of cos(ωn), delayed by D: e^{jω(n−D)}.
            let want = TAU * f * (n - d) as f64;
            let err = (v.arg() as f64 - want).rem_euclid(TAU);
            let err = err.min(TAU - err);
            assert!(err < 0.01, "f {f} n {n}: phase off by {err}");
            assert!(
                (v.norm() - 1.0).abs() < 0.01,
                "f {f} n {n}: |y| = {}",
                v.norm()
            );
        }
    }
    // The real part is exactly the delayed input.
    let x = tone(0.1, 500);
    let b = block("hilbert", json!({"taps": 31}));
    let (y, _, _) = chain::<f32, Complex32>(x.clone(), vec![], 1.0, vec![b], true);
    for n in 15..500 {
        assert_eq!(y[n].re, x[n - 15]);
    }
}

#[test]
fn response_previews() {
    let reg = foxygnal_blocks::registry();
    let rate = 48_000.0;
    let resp = |kind: &str, p: serde_json::Value| {
        reg.response(kind, p.as_object().unwrap(), rate)
            .unwrap()
            .unwrap_or_else(|| panic!("{kind} has no preview"))
    };
    let at = |r: &foxygnal_core::FrequencyResponse, f: f64| {
        let i = r
            .freqs
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1 - f).abs().total_cmp(&(b.1 - f).abs()))
            .unwrap()
            .0;
        r.db[i] as f64
    };

    let r = resp(
        "biquad",
        json!({"shape": "peaking", "freq": 6_000.0, "q": 2.0, "gain_db": 6.0}),
    );
    assert_eq!(r.taps, 0);
    // 6 kHz is exactly one of the 512 points? Close enough either way.
    assert!((at(&r, 6_000.0) - 6.0).abs() < 0.2, "{}", at(&r, 6_000.0));
    assert!(at(&r, 0.0).abs() < 1e-6);
    let r = resp("biquad", json!({"freq": 1_000.0, "stages": 3}));
    assert!(at(&r, 20_000.0) < -100.0);

    let r = resp("dc_blocker", json!({"cutoff": 200.0}));
    assert!(at(&r, 0.0) < -150.0 && at(&r, 24_000.0).abs() < 1e-6);

    // RRC at 4 sps: 0 dB at DC, −6 dB (half amplitude) for the raised cosine
    // pair = −3 dB for one RRC at half the symbol rate, stopband past (1+β)/2.
    let r = resp(
        "rrc_filter",
        json!({"sps": 4.0, "span": 16, "rolloff": 0.35}),
    );
    assert_eq!(r.taps, 65);
    assert!(at(&r, 0.0).abs() < 0.1);
    assert!((at(&r, 6_000.0) + 3.0).abs() < 0.3, "{}", at(&r, 6_000.0));
    assert!(at(&r, 12_000.0) < -30.0);
    let r = resp(
        "rrc_filter",
        json!({"mode": "interpolate", "sps": 4, "span": 16}),
    );
    assert_eq!(*r.freqs.last().unwrap(), rate * 2.0);
    assert!(at(&r, 0.0).abs() < 0.1);

    // Hilbert: ≈ +6 dB for positive frequencies, a null for negative ones.
    let r = resp("hilbert", json!({"taps": 101, "window": "blackman"}));
    assert!(r.freqs[0] < 0.0);
    assert!((at(&r, 5_000.0) - 6.02).abs() < 0.1);
    assert!(at(&r, -5_000.0) < -40.0);

    let cat = serde_json::to_value(reg.catalog().collect::<Vec<_>>()).unwrap();
    for kind in ["rrc_filter", "biquad", "dc_blocker", "hilbert"] {
        let d = cat
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["kind"] == kind)
            .unwrap();
        assert_eq!(d["response"], true, "{kind}");
    }
}

#[test]
fn filters_reject_wrong_port_types_naming_both_ends() {
    let mut fg = Flowgraph::new();
    let a = fg.add("a", VectorSource::instance(vec![Complex32::default()]));
    let h = fg.add("h", block("hilbert", json!({})));
    let e = fg.connect(a, 0, h, 0).err().unwrap().to_string();
    assert!(e.contains("a.out") && e.contains("h.in"), "{e}");
}

/// Default tag propagation moves tags by the configured rate ratio, not by
/// one `work` call's produced ÷ consumed (which a filter's history skews).
#[test]
fn resamplers_move_tags_by_the_exact_rate_ratio() {
    let cases = [
        (
            "interpolator",
            json!({"type": "f32", "interpolation": 8}),
            3,
            24,
        ),
        ("decimator", json!({"type": "f32", "decimation": 4}), 11, 2),
        (
            "rational_resampler",
            json!({"type": "f32", "output_rate": 0, "interpolation": 3, "decimation": 2}),
            4,
            6,
        ),
        (
            "rational_resampler",
            json!({"type": "f32", "output_rate": 0, "interpolation": 3, "decimation": 2}),
            301,
            451,
        ),
    ];
    for (kind, p, at, want) in cases {
        for tiny in [false, true] {
            let b = block(kind, p.clone());
            let tags = vec![Tag::new(at, "mark", true)];
            let (_, got, _) = chain::<f32, f32>(vec![1.0; 2_000], tags, 48_000.0, vec![b], tiny);
            assert_eq!(got.len(), 1, "{kind} {p}");
            assert_eq!(
                got[0].offset, want,
                "{kind} {p} tag at {at} (tiny buffers: {tiny})"
            );
        }
    }
}
