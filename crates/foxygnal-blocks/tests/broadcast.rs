//! Broadcast FM end to end: a synthesized MPX signal (L/R tones, pilot,
//! 38 kHz L−R, and a 57 kHz RDS stream built by the small encoder below) goes
//! through `fm_stereo` and `rds_decoder`, directly and through an FM
//! modulator + `fm_demod`.

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{Collected, VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, Instance, Sample, StreamMeta, Tag, TagValue};
use serde_json::json;

const RATE: f64 = 240_000.0;
/// Slightly off 19 kHz, so the PLL has to track.
const PILOT: f64 = 19_001.5;
const PI_CODE: u16 = 0xD3C3;
const PTY: u16 = 10;
const PS: &[u8; 8] = b"FOXYGNAL";
const RT: &str = "Hello from foxygnal! RDS test 123";

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

// ---- RDS encoder ----------------------------------------------------------------

/// Check word: data·x¹⁰ mod g(x), as a shift register (independent of the
/// decoder's long division).
fn checkword(data: u16) -> u16 {
    let mut reg: u16 = 0;
    for i in (0..16).rev() {
        let fb = ((reg >> 9) & 1) ^ ((data >> i) & 1);
        reg = (reg << 1) & 0x3FF;
        if fb == 1 {
            reg ^= 0x1B9; // g(x) without x¹⁰
        }
    }
    reg
}

const OFFSET_A: u16 = 0x0FC;
const OFFSET_B: u16 = 0x198;
const OFFSET_C: u16 = 0x168;
const OFFSET_D: u16 = 0x1B4;

fn push_group(bits: &mut Vec<u8>, blocks: [u16; 4]) {
    for (data, off) in blocks
        .into_iter()
        .zip([OFFSET_A, OFFSET_B, OFFSET_C, OFFSET_D])
    {
        let word = (u32::from(data) << 10) | u32::from(checkword(data) ^ off);
        bits.extend((0..26).rev().map(|i| ((word >> i) & 1) as u8));
    }
}

/// One cycle: PS (4 × 0A), RadioText (2A up to the CR), clock time (4A).
fn rds_cycle(bits: &mut Vec<u8>) {
    let b0 = (1 << 10) | (PTY << 5); // TP = 1
    for s in 0..4u16 {
        let d = u16::from_be_bytes([PS[2 * s as usize], PS[2 * s as usize + 1]]);
        push_group(bits, [PI_CODE, b0 | s, 0xE0CD, d]);
    }
    let mut rt = RT.as_bytes().to_vec();
    rt.push(0x0D);
    while !rt.len().is_multiple_of(4) {
        rt.push(b' ');
    }
    for (s, ch) in rt.chunks(4).enumerate() {
        let b = (2 << 12) | b0 | s as u16;
        let c = u16::from_be_bytes([ch[0], ch[1]]);
        let d = u16::from_be_bytes([ch[2], ch[3]]);
        push_group(bits, [PI_CODE, b, c, d]);
    }
    // 2026-11-22 13:45 UTC, +01:00: MJD 61366.
    let (mjd, hour, minute, offset) = (61366u32, 13u16, 45u16, 2u16);
    let b = (4 << 12) | b0 | ((mjd >> 15) & 3) as u16;
    let c = (((mjd & 0x7FFF) as u16) << 1) | (hour >> 4);
    let d = ((hour & 0xF) << 12) | (minute << 6) | offset;
    push_group(bits, [PI_CODE, b, c, d]);
}

/// Differentially coded biphase symbols, ±1 per bit.
fn rds_symbols(cycles: usize) -> Vec<f64> {
    let mut bits = Vec::new();
    for _ in 0..cycles {
        rds_cycle(&mut bits);
    }
    let mut e = 0u8;
    bits.iter()
        .map(|&d| {
            e ^= d;
            if e == 1 { 1.0 } else { -1.0 }
        })
        .collect()
}

// ---- MPX synthesis ----------------------------------------------------------------

struct Mpx {
    left: f64,
    right: f64,
    pilot: bool,
    rds: bool,
}

const LEFT_HZ: f64 = 1_000.0;
const RIGHT_HZ: f64 = 2_500.0;

/// `seconds` of MPX at [`RATE`], ±1 = full deviation.
fn mpx(cfg: &Mpx, seconds: f64) -> Vec<f32> {
    let n = (seconds * RATE) as usize;
    let bit_rate = 3.0 * PILOT / 48.0;
    let symbols = rds_symbols((seconds * bit_rate / 1456.0).ceil() as usize + 1);
    (0..n)
        .map(|i| {
            let t = i as f64 / RATE;
            let l = cfg.left * (TAU * LEFT_HZ * t).sin();
            let r = cfg.right * (TAU * RIGHT_HZ * t).sin();
            let w = TAU * PILOT * t;
            let mut m = 0.9 * ((l + r) / 2.0 + (l - r) / 2.0 * (2.0 * w).sin());
            if cfg.pilot {
                m += 0.09 * w.sin();
            }
            if cfg.rds {
                let pos = t * bit_rate;
                let k = pos as usize;
                // Biphase: a sine period per bit, + first half, − second.
                let shape = (TAU * pos.fract()).sin();
                m += 0.04 * symbols[k] * shape * (3.0 * w + 0.7).sin();
            }
            m as f32
        })
        .collect()
}

/// FM-modulate at 75 kHz deviation, adding complex noise of this RMS.
fn fm_modulate(m: &[f32], noise: f64) -> Vec<Complex32> {
    let mut phase = 0.0f64;
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut uniform = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    };
    m.iter()
        .map(|&x| {
            phase = (phase + TAU * 75_000.0 * f64::from(x) / RATE) % TAU;
            let r = (-2.0 * uniform().ln()).sqrt() * noise / 2f64.sqrt();
            let a = TAU * uniform();
            Complex32::new(
                (phase.cos() + r * a.cos()) as f32,
                (phase.sin() + r * a.sin()) as f32,
            )
        })
        .collect()
}

// ---- Running ----------------------------------------------------------------------

struct Out {
    left: Vec<f32>,
    right: Vec<f32>,
    tags: Vec<Tag>,
    text: String,
    rate: Option<f64>,
}

fn run<T: Sample>(input: Vec<T>, front: Option<Instance>, stereo: serde_json::Value) -> Out {
    let mut fg = Flowgraph::new();
    let mut src = fg.add(
        "src",
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(RATE)),
    );
    if let Some(f) = front {
        let id = fg.add("demod", f);
        fg.connect(src, 0, id, 0).unwrap();
        src = id;
    }
    let st = fg.add("stereo", block("fm_stereo", stereo));
    let rds = fg.add("rds", block("rds_decoder", json!({})));
    let (ls, left, tags): (_, Collected<f32>, _) = VectorSink::with_tags();
    let (rs, right) = VectorSink::<f32>::instance();
    let (ts, text) = VectorSink::<u8>::instance();
    let (ls, rs, ts) = (fg.add("l", ls), fg.add("r", rs), fg.add("t", ts));
    fg.connect(src, 0, st, 0).unwrap();
    fg.connect(src, 0, rds, 0).unwrap();
    fg.connect(st, 0, ls, 0).unwrap();
    fg.connect(st, 1, rs, 0).unwrap();
    fg.connect(rds, 0, ts, 0).unwrap();
    let edges = fg.configure().unwrap();
    let rate = edges
        .iter()
        .find(|e| e.from == "stereo.left")
        .unwrap()
        .meta
        .sample_rate;
    fg.start().unwrap().wait().unwrap();
    Out {
        left: left.lock().unwrap().clone(),
        right: right.lock().unwrap().clone(),
        tags: tags.lock().unwrap().clone(),
        text: String::from_utf8(text.lock().unwrap().clone()).unwrap(),
        rate,
    }
}

/// Amplitude of a tone at `hz` (whole cycles over the window) at 48 kS/s.
fn tone(x: &[f32], hz: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let p = TAU * hz * i as f64 / 48_000.0;
        re += f64::from(v) * p.cos();
        im += f64::from(v) * p.sin();
    }
    2.0 * (re * re + im * im).sqrt() / x.len() as f64
}

fn stereo_tags(tags: &[Tag]) -> Vec<bool> {
    tags.iter()
        .filter(|t| &*t.key == "stereo")
        .map(|t| t.value == TagValue::Bool(true))
        .collect()
}

fn check_separation(o: &Out) {
    assert_eq!(o.rate, Some(48_000.0));
    // One second after the PLL has settled.
    let (l, r) = (&o.left[48_000..96_000], &o.right[48_000..96_000]);
    let (ll, lr) = (tone(l, LEFT_HZ), tone(l, RIGHT_HZ));
    let (rr, rl) = (tone(r, RIGHT_HZ), tone(r, LEFT_HZ));
    assert!((ll - 0.45).abs() < 0.02, "left level {ll}");
    assert!((rr - 0.45).abs() < 0.02, "right level {rr}");
    let sep_l = 20.0 * (rr / lr).log10();
    let sep_r = 20.0 * (ll / rl).log10();
    eprintln!("separation: right→left {sep_l:.1} dB, left→right {sep_r:.1} dB");
    assert!(sep_l > 30.0 && sep_r > 30.0, "{sep_l} {sep_r}");
    assert_eq!(stereo_tags(&o.tags), vec![false, true], "{:?}", o.tags);
}

fn check_rds(text: &str) {
    eprintln!("{text}");
    let pi = format!("pi={PI_CODE:04X}");
    let has = |kind: &str, field: &str| {
        text.lines()
            .any(|l| l.contains(&pi) && l.contains(kind) && l.contains(field))
    };
    assert!(
        has("type=station", "pty=10 pty_name=\"Pop music\" tp=1"),
        "{text}"
    );
    assert!(has("type=ps", "ps=\"FOXYGNAL\""), "{text}");
    assert!(has("type=rt", &format!("rt=\"{RT}\"")), "{text}");
    assert!(
        has("type=ct", "utc=2026-11-22T13:45Z offset=+01:00"),
        "{text}"
    );
    // Each piece of news once, not once per group.
    assert_eq!(text.lines().filter(|l| l.contains("type=ps")).count(), 1);
    assert_eq!(text.lines().filter(|l| l.contains("type=rt")).count(), 1);
}

#[test]
fn stereo_separates_and_rds_decodes() {
    let m = mpx(
        &Mpx {
            left: 0.5,
            right: 0.5,
            pilot: true,
            rds: true,
        },
        4.0,
    );
    let o = run(m, None, json!({ "deemphasis": "off" }));
    check_separation(&o);
    check_rds(&o.text);
}

#[test]
fn whole_chain_through_fm_demod() {
    let m = mpx(
        &Mpx {
            left: 0.5,
            right: 0.5,
            pilot: true,
            rds: true,
        },
        4.0,
    );
    let iq = fm_modulate(&m, 0.15);
    let demod = block(
        "fm_demod",
        json!({ "deviation": 75_000.0, "deemphasis": "off" }),
    );
    let o = run(iq, Some(demod), json!({ "deemphasis": "off" }));
    check_separation(&o);
    check_rds(&o.text);
}

#[test]
fn no_pilot_falls_back_to_mono() {
    let m = mpx(
        &Mpx {
            left: 0.5,
            right: 0.0,
            pilot: false,
            rds: false,
        },
        1.5,
    );
    let o = run(m, None, json!({ "deemphasis": "50us" }));
    assert_eq!(stereo_tags(&o.tags), vec![false]);
    assert!(o.left.len() > 60_000);
    assert_eq!(o.left, o.right);
    // Mono: (L+R)/2 = 0.9 · 0.25, less the 50 µs de-emphasis at 1 kHz.
    let a = tone(&o.left[24_000..72_000], LEFT_HZ);
    let deemph = 1.0 / (1.0 + (TAU * LEFT_HZ * 50e-6).powi(2)).sqrt();
    assert!((a - 0.225 * deemph).abs() < 0.01, "{a}");
    assert!(o.text.is_empty(), "{}", o.text);
}

#[test]
fn forced_mono_ignores_the_pilot() {
    let m = mpx(
        &Mpx {
            left: 0.5,
            right: 0.0,
            pilot: true,
            rds: false,
        },
        1.0,
    );
    let o = run(m, None, json!({ "mode": "mono", "audio_rate": 0.0 }));
    assert_eq!(o.rate, Some(RATE));
    assert_eq!(stereo_tags(&o.tags), vec![false]);
    assert_eq!(o.left, o.right);
}

#[test]
fn too_low_a_rate_is_explained() {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(96_000.0)),
    );
    let st = fg.add("stereo", block("fm_stereo", json!({})));
    fg.connect(src, 0, st, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(err.contains("106 kS/s") && err.contains("96 kS/s"), "{err}");
}
