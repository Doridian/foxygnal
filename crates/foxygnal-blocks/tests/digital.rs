//! Digital modes end to end: synthesized PSK and FSK signals with frequency,
//! phase and timing offsets go through the modem chains and must come back
//! bit-exact; access codes are found despite bit errors.

use std::f64::consts::{PI, TAU};

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

/// Run `input` through a chain of blocks (each output 0 into the next input
/// 0). Returns the output, its tags and the last block's output metadata.
fn chain<I: Sample, O: Sample>(
    input: Vec<I>,
    meta: StreamMeta,
    blocks: Vec<Instance>,
    buffers: Option<usize>,
) -> (Vec<O>, Vec<Tag>, StreamMeta) {
    let mut fg = match buffers {
        Some(cap) => Flowgraph::new()
            .with_buffer_capacity(cap)
            .with_buffer_kind(BufferKind::Plain),
        None => Flowgraph::new(),
    };
    let mut prev = fg.add("src", VectorSource::instance_with_meta(input, meta));
    let n = blocks.len();
    for (i, b) in blocks.into_iter().enumerate() {
        let id = fg.add(format!("b{i}"), b);
        fg.connect(prev, 0, id, 0).unwrap();
        prev = id;
    }
    let (sink, out, tags) = VectorSink::<O>::with_tags();
    let sink = fg.add("sink", sink);
    fg.connect(prev, 0, sink, 0).unwrap();
    let edges = fg.configure().unwrap();
    let last = format!("b{}.out", n - 1);
    let m = edges.iter().find(|e| e.from == last).unwrap().meta;
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap().clone();
    let tags = tags.lock().unwrap().clone();
    (out, tags, m)
}

/// Deterministic noise and bits (xorshift + Box-Muller).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
    fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 32) as u8 & 1).collect()
    }
}

/// Matched-filtered linear modulation: `symbols` with triangular pulses
/// (rectangular pulse ∗ its matched filter) every `sps` samples, starting
/// `delay` samples in, on a carrier offset by `freq` cycles/sample with
/// phase `phase`, plus complex noise of `sigma` per axis.
fn linear(
    symbols: &[Complex32],
    sps: f64,
    delay: f64,
    freq: f64,
    phase: f64,
    sigma: f64,
    rng: &mut Rng,
) -> Vec<Complex32> {
    let n = ((symbols.len() as f64 + 2.0) * sps + delay) as usize;
    (0..n)
        .map(|i| {
            let t = (i as f64 - delay) / sps;
            let k = t.floor() as i64;
            let mut s = num_complex::Complex64::new(0.0, 0.0);
            for j in [k, k + 1] {
                if j >= 0 && (j as usize) < symbols.len() {
                    let w = 1.0 - (t - j as f64).abs();
                    let a = symbols[j as usize];
                    s += num_complex::Complex64::new(a.re as f64, a.im as f64) * w.max(0.0);
                }
            }
            let rot = num_complex::Complex64::from_polar(1.0, TAU * freq * i as f64 + phase);
            let y = s * rot + num_complex::Complex64::new(sigma * rng.gauss(), sigma * rng.gauss());
            Complex32::new(y.re as f32, y.im as f32)
        })
        .collect()
}

/// Smallest number of bit errors of `got[skip..]` against `want` (or its
/// inverse when `invert`), over small lags. Returns (errors, compared bits).
fn best_match(got: &[u8], want: &[u8], skip: usize, invert: bool) -> (usize, usize) {
    let mut best = (usize::MAX, 0);
    for lag in -8i64..=8 {
        for flip in [0u8, u8::from(invert)] {
            let mut errors = 0;
            let mut n = 0;
            for (i, &g) in got.iter().enumerate().skip(skip) {
                let j = i as i64 + lag;
                if j < 0 || j as usize >= want.len() {
                    continue;
                }
                errors += usize::from((g ^ flip) != want[j as usize]);
                n += 1;
            }
            if n > 0 && errors < best.0 {
                best = (errors, n);
            }
        }
    }
    best
}

#[test]
fn bpsk_with_frequency_phase_and_timing_offsets_is_error_free() {
    let mut rng = Rng(0x5eed);
    let bits = rng.bits(3000);
    let syms: Vec<Complex32> = bits
        .iter()
        .map(|&b| Complex32::new(if b == 0 { 1.0 } else { -1.0 }, 0.0))
        .collect();
    // 8.006 true samples/symbol against a nominal 8: the clock drifts.
    let x = linear(&syms, 8.006, 3.3, 4e-4, 1.0, 0.08, &mut rng);
    for buffers in [None, Some(7)] {
        let (got, _, meta) = chain::<Complex32, u8>(
            x.clone(),
            StreamMeta::with_rate(80_000.0),
            vec![
                block(
                    "clock_recovery",
                    json!({"type": "c32", "sps": 8.0, "loop_bw": 0.02}),
                ),
                block("costas_loop", json!({"order": "2", "loop_bw": 0.05})),
                block("psk_demod", json!({"modulation": "bpsk", "output": "bits"})),
            ],
            buffers,
        );
        assert_eq!(meta.sample_rate, Some(10_000.0));
        assert!(
            (got.len() as i64 - 3000).abs() < 10,
            "{} symbols out",
            got.len()
        );
        let (errors, n) = best_match(&got, &bits, 300, true);
        assert!(n > 2500, "compared {n}");
        assert_eq!(errors, 0, "BER {}/{n}", errors);
    }
}

#[test]
fn qpsk_with_offsets_is_error_free_up_to_rotation() {
    let mut rng = Rng(42);
    let n = 3000;
    let idx: Vec<u32> = (0..n).map(|_| (rng.next() >> 40) as u32 & 3).collect();
    let syms: Vec<Complex32> = idx
        .iter()
        .map(|&k| {
            let a = PI / 4.0 + PI / 2.0 * k as f64;
            Complex32::new(a.cos() as f32, a.sin() as f32)
        })
        .collect();
    let x = linear(&syms, 6.0, 1.7, -3e-4, 2.0, 0.05, &mut rng);
    let (got, _, _) = chain::<Complex32, u8>(
        x,
        StreamMeta::default(),
        vec![
            block(
                "clock_recovery",
                json!({"type": "c32", "sps": 6.003, "loop_bw": 0.02}),
            ),
            block("costas_loop", json!({"order": "4", "loop_bw": 0.05})),
            block(
                "psk_demod",
                json!({"modulation": "qpsk", "output": "symbols"}),
            ),
        ],
        None,
    );
    let gray = |k: u32| (k ^ (k >> 1)) as u8;
    // The Costas loop may lock at any multiple of 90°.
    let best = (0..4)
        .map(|r| {
            let want: Vec<u8> = idx.iter().map(|&k| gray((k + r) % 4)).collect();
            let mut b = (usize::MAX, 0);
            for lag in -8i64..=8 {
                let (mut e, mut c) = (0, 0);
                for (i, &g) in got.iter().enumerate().skip(300) {
                    let j = i as i64 + lag;
                    if j >= 0 && (j as usize) < want.len() {
                        e += usize::from(g != want[j as usize]);
                        c += 1;
                    }
                }
                if c > 0 && e < b.0 {
                    b = (e, c);
                }
            }
            b
        })
        .min()
        .unwrap();
    assert!(best.1 > 2500, "compared {}", best.1);
    assert_eq!(best.0, 0, "symbol errors {}/{}", best.0, best.1);
}

#[test]
fn mueller_muller_recovers_real_symbols() {
    let mut rng = Rng(7);
    let bits = rng.bits(2000);
    let syms: Vec<Complex32> = bits
        .iter()
        .map(|&b| Complex32::new(if b == 1 { 1.0 } else { -1.0 }, 0.0))
        .collect();
    let x: Vec<f32> = linear(&syms, 10.004, 4.6, 0.0, 0.0, 0.05, &mut rng)
        .iter()
        .map(|c| c.re)
        .collect();
    let (got, _, _) = chain::<f32, u8>(
        x,
        StreamMeta::default(),
        vec![
            block(
                "clock_recovery",
                json!({"type": "f32", "sps": 10.0, "loop_bw": 0.02, "ted": "mueller_muller"}),
            ),
            block("binary_slicer", json!({})),
        ],
        None,
    );
    let (errors, n) = best_match(&got, &bits, 200, false);
    assert!(n > 1700, "compared {n}");
    assert_eq!(errors, 0, "BER {errors}/{n}");
}

/// Continuous-phase 2-FSK at `rate`: bit 1 = +dev, 0 = −dev, with a carrier
/// offset of `offset` Hz.
fn fsk(bits: &[u8], rate: f64, baud: f64, dev: f64, offset: f64, rng: &mut Rng) -> Vec<Complex32> {
    let sps = rate / baud;
    let n = (bits.len() as f64 * sps) as usize;
    let mut phase = 0.3;
    (0..n)
        .map(|i| {
            let b = bits[((i as f64 / sps) as usize).min(bits.len() - 1)];
            let f = if b == 1 { dev } else { -dev } + offset;
            phase += TAU * f / rate;
            Complex32::new(
                (phase.cos() + 0.05 * rng.gauss()) as f32,
                (phase.sin() + 0.05 * rng.gauss()) as f32,
            )
        })
        .collect()
}

#[test]
fn fsk_bits_come_back() {
    let mut rng = Rng(99);
    let bits = rng.bits(1500);
    let rate = 48_000.0;
    let x = fsk(&bits, rate, 1200.0, 2400.0, 150.0, &mut rng);
    for buffers in [None, Some(9)] {
        let (got, _, meta) = chain::<Complex32, u8>(
            x.clone(),
            StreamMeta::with_rate(rate),
            vec![
                block(
                    "fsk_demod",
                    json!({"deviation": 2400.0, "baud": 1200.0, "dc_symbols": 50.0}),
                ),
                block("clock_recovery", json!({"type": "f32", "sps": 40.0})),
                block("binary_slicer", json!({})),
            ],
            buffers,
        );
        assert_eq!(meta.sample_rate, Some(1200.0));
        let (errors, n) = best_match(&got, &bits, 100, false);
        assert!(n > 1300, "compared {n}");
        assert_eq!(errors, 0, "BER {errors}/{n}");
    }
}

#[test]
fn fsk_demod_checks_its_parameters() {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<Complex32>::instance_with_meta(vec![], StreamMeta::with_rate(8000.0)),
    );
    let d = fg.add("fsk", block("fsk_demod", json!({"deviation": 5000.0})));
    fg.connect(src, 0, d, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(err.contains("deviation 5 kHz"), "{err}");
}

fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
        .collect()
}

/// Random bits, then the access code 0x1ACFFC1D with bit errors at
/// `flips`, then `payload`, then more random bits. Returns (bits, index of
/// the first payload bit).
fn framed(payload: &[u8], flips: &[usize], rng: &mut Rng) -> (Vec<u8>, usize) {
    let mut bits = rng.bits(137);
    let mut code = bytes_to_bits(&[0x1A, 0xCF, 0xFC, 0x1D]);
    for &f in flips {
        code[f] ^= 1;
    }
    bits.extend(code);
    let start = bits.len();
    bits.extend(bytes_to_bits(payload));
    bits.extend(rng.bits(77));
    (bits, start)
}

#[test]
fn access_code_is_found_despite_bit_errors() {
    let mut rng = Rng(1234);
    let (bits, start) = framed(b"fox!", &[3, 30], &mut rng);
    let code = json!({"access_code": "0x1ACF FC1D", "threshold": 2});
    let (out, tags, _) = chain::<u8, u8>(
        bits.clone(),
        StreamMeta::default(),
        vec![block("correlate_access_code", code.clone())],
        Some(5),
    );
    assert_eq!(out, bits, "bits pass through");
    let packets: Vec<&Tag> = tags.iter().filter(|t| &*t.key == "packet").collect();
    assert_eq!(packets.len(), 1, "{tags:?}");
    assert_eq!(packets[0].offset, start as u64);
    assert_eq!(packets[0].value, TagValue::Int(2));

    // Two errors are one too many at threshold 1.
    let (_, tags, _) = chain::<u8, u8>(
        bits,
        StreamMeta::default(),
        vec![block(
            "correlate_access_code",
            json!({"access_code": "0x1ACFFC1D", "threshold": 1}),
        )],
        None,
    );
    assert!(tags.is_empty(), "{tags:?}");
}

#[test]
fn packet_decoder_collects_payloads_as_text_or_bytes() {
    let mut rng = Rng(5);
    let (mut bits, _) = framed(b"fox!", &[], &mut rng);
    let (more, _) = framed(&[0xDE, 0xAD, 0xBE, 0xEF], &[17], &mut rng);
    bits.extend(more);
    let blocks = |format: &str| {
        vec![
            block(
                "correlate_access_code",
                json!({"access_code": "0x1ACFFC1D"}),
            ),
            block("packet_decoder", json!({"length": 4, "format": format})),
        ]
    };
    for buffers in [None, Some(5)] {
        let (text, _, meta) = chain::<u8, u8>(
            bits.clone(),
            StreamMeta::with_rate(9600.0),
            blocks("hex"),
            buffers,
        );
        assert_eq!(meta.sample_rate, None);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "len=4 errors=0 data=666f7821\nlen=4 errors=1 data=deadbeef\n"
        );
        let (raw, tags, _) =
            chain::<u8, u8>(bits.clone(), StreamMeta::default(), blocks("raw"), buffers);
        assert_eq!(raw, b"fox!\xDE\xAD\xBE\xEF");
        let at: Vec<(u64, &TagValue)> = tags
            .iter()
            .filter(|t| &*t.key == "packet")
            .map(|t| (t.offset, &t.value))
            .collect();
        assert_eq!(at, vec![(0, &TagValue::Int(4)), (4, &TagValue::Int(4))]);
    }
}

#[test]
fn message_log_consumes_text() {
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(b"one\ntwo\n".to_vec()));
    let log = fg.add("log", block("message_log", json!({})));
    fg.connect(src, 0, log, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
}

#[test]
fn message_log_takes_several_inputs() {
    let mut fg = Flowgraph::new();
    let a = fg.add("a", VectorSource::instance(b"one\ntwo\n".to_vec()));
    let b = fg.add("b", VectorSource::instance(b"three\n".to_vec()));
    let log = fg.add("log", block("message_log", json!({"inputs": 2})));
    fg.connect(a, 0, log, 0).unwrap();
    fg.connect(b, 0, log, 1).unwrap();
    fg.start().unwrap().wait().unwrap();
    let err = foxygnal_blocks::registry()
        .build("message_log", json!({"inputs": 5}).as_object().unwrap())
        .err()
        .unwrap();
    assert!(err.to_string().contains("1 to 4"), "{err}");
}

#[test]
fn the_ais_and_acars_examples_build_and_configure() {
    let reg = foxygnal_blocks::registry();
    for (toml, into, rate) in [
        (
            include_str!("../../../examples/ais.fox.toml"),
            "ais_a.in",
            48e3,
        ),
        (
            include_str!("../../../examples/acars.fox.toml"),
            "acars.in",
            48e3,
        ),
    ] {
        let spec = foxygnal_core::GraphSpec::from_toml(toml).unwrap();
        let mut fg = Flowgraph::from_spec(&spec, &reg).unwrap();
        let edges = fg.configure().unwrap();
        let e = edges.iter().find(|e| e.to == into).unwrap();
        assert_eq!(e.meta.sample_rate, Some(rate), "{into}");
    }
}

#[test]
fn the_adsb_example_builds_and_configures() {
    // Configuring opens no device, so this touches no hardware.
    let spec = foxygnal_core::GraphSpec::from_toml(include_str!("../../../examples/adsb.fox.toml"))
        .unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let edges = fg.configure().unwrap();
    let into = edges.iter().find(|e| e.to == "adsb.in").unwrap();
    assert_eq!(into.meta.sample_rate, Some(2.4e6));
}

#[test]
fn the_uat_example_builds_and_configures() {
    let spec = foxygnal_core::GraphSpec::from_toml(include_str!("../../../examples/uat.fox.toml"))
        .unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let edges = fg.configure().unwrap();
    let into = edges.iter().find(|e| e.to == "uat.in").unwrap();
    assert!((into.meta.sample_rate.unwrap() - 2_083_333.333).abs() < 1.0);
}

#[test]
fn psk_bits_split_across_buffer_wraps_stay_in_order() {
    let mut rng = Rng(77);
    let idx: Vec<u32> = (0..500).map(|_| (rng.next() >> 40) as u32 & 7).collect();
    let syms: Vec<Complex32> = idx
        .iter()
        .map(|&k| {
            let a = PI / 8.0 + PI / 4.0 * k as f64;
            Complex32::new(a.cos() as f32, a.sin() as f32)
        })
        .collect();
    let want: Vec<u8> = idx
        .iter()
        .flat_map(|&k| {
            let g = (k ^ (k >> 1)) as u8;
            [(g >> 2) & 1, (g >> 1) & 1, g & 1]
        })
        .collect();
    for buffers in [None, Some(5)] {
        let (got, _, meta) = chain::<Complex32, u8>(
            syms.clone(),
            StreamMeta::with_rate(1000.0),
            vec![block(
                "psk_demod",
                json!({"modulation": "8psk", "output": "bits"}),
            )],
            buffers,
        );
        assert_eq!(meta.sample_rate, Some(3000.0));
        assert_eq!(got, want);
    }
}
