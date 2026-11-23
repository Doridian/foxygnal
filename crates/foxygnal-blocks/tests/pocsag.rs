//! POCSAG end to end: a small independent encoder (BCH check bits from a
//! shift register, not the decoder's long division) builds transmissions
//! that go through `pocsag_decoder` as NRZ baseband, inverted, with bit
//! errors, at all three rates, and through an FM modulator + `fm_demod`.

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{BufferKind, Complex32, Flowgraph, Instance, Sample, StreamMeta};
use serde_json::json;

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

// ---- Encoder ----------------------------------------------------------------------

const SYNC: u32 = 0x7CD2_15D8;
const IDLE: u32 = 0x7A89_C197;

/// 21 data bits → 32-bit codeword: BCH(31,21) check bits from an LFSR
/// dividing by x¹⁰ + x⁹ + x⁸ + x⁶ + x⁵ + x³ + 1, then even parity.
fn codeword(data: u32) -> u32 {
    let mut reg: u32 = 0;
    for i in (0..21).rev() {
        let fb = ((reg >> 9) & 1) ^ ((data >> i) & 1);
        reg = (reg << 1) & 0x3FF;
        if fb == 1 {
            reg ^= 0x369; // g(x) without x¹⁰
        }
    }
    let cw = (data << 11) | (reg << 1);
    cw | (cw.count_ones() & 1)
}

enum Content<'a> {
    Tone,
    Numeric(&'a str),
    Alpha(&'a [u8]),
}

struct Page<'a> {
    ric: u32,
    func: u32,
    content: Content<'a>,
}

/// Message codeword data (20 bits each).
fn message_words(c: &Content<'_>) -> Vec<u32> {
    let mut bits: Vec<u8> = Vec::new();
    match c {
        Content::Tone => return vec![],
        Content::Numeric(s) => {
            const TABLE: &str = "0123456789.U -][";
            for ch in s.chars() {
                let v = TABLE.find(ch).unwrap() as u8;
                bits.extend((0..4).map(|i| (v >> i) & 1)); // LSB first
            }
            while !bits.len().is_multiple_of(20) {
                bits.extend([0, 0, 1, 1]); // space (0xC)
            }
        }
        Content::Alpha(s) => {
            for &ch in s.iter() {
                bits.extend((0..7).map(|i| (ch >> i) & 1));
            }
            while !bits.len().is_multiple_of(20) {
                bits.push(0);
            }
        }
    }
    bits.chunks(20)
        .map(|c| c.iter().fold(0u32, |w, &b| (w << 1) | u32::from(b)))
        .collect()
}

/// Codewords of one transmission (no sync words yet): each page's address
/// in its frame, its message right after, idle codewords elsewhere.
fn codewords(pages: &[Page<'_>]) -> Vec<u32> {
    let mut cws = Vec::new();
    for p in pages {
        let frame = (p.ric & 7) as usize;
        while cws.len() % 16 != 2 * frame {
            cws.push(IDLE);
        }
        cws.push(codeword(((p.ric >> 3) << 2) | p.func));
        for w in message_words(&p.content) {
            cws.push(codeword((1 << 20) | w));
        }
    }
    cws.push(IDLE);
    while !cws.len().is_multiple_of(16) {
        cws.push(IDLE);
    }
    cws
}

/// Bits on air: preamble, then batches (sync + 16 codewords), MSB first.
/// `flips` lists (codeword index, bit mask) errors to add.
fn transmission(pages: &[Page<'_>], flips: &[(usize, u32)]) -> Vec<u8> {
    let mut cws = codewords(pages);
    for &(i, mask) in flips {
        cws[i] ^= mask;
    }
    let mut bits: Vec<u8> = (0..576).map(|i| (i & 1 == 0) as u8).collect();
    for batch in cws.chunks(16) {
        for &w in std::iter::once(&SYNC).chain(batch) {
            bits.extend((0..32).rev().map(|i| ((w >> i) & 1) as u8));
        }
    }
    bits
}

/// Deterministic Gaussian noise.
struct Rng(u64);
impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
    }
}

/// NRZ at `rate`: 1 → +1 (or −1 when `invert`), with a DC offset and noise,
/// a slightly fast bit clock, and silence (noise) around the bits.
fn nrz(bits: &[u8], baud: f64, rate: f64, invert: bool, noise: f64, rng: &mut Rng) -> Vec<f32> {
    let sps = rate / baud * 0.9998;
    let pad = (0.05 * rate) as usize;
    let n = (bits.len() as f64 * sps) as usize;
    let mut out = Vec::with_capacity(n + 2 * pad);
    for i in 0..n + 2 * pad {
        let v = if i < pad || i >= pad + n {
            0.0
        } else {
            let b = bits[((i - pad) as f64 / sps) as usize];
            if (b == 1) != invert { 1.0 } else { -1.0 }
        };
        out.push((0.1 + v + noise * rng.gauss()) as f32);
    }
    out
}

fn run<T: Sample>(
    input: Vec<T>,
    rate: f64,
    front: Vec<Instance>,
    decoder: serde_json::Value,
    buffers: Option<usize>,
) -> String {
    let mut fg = match buffers {
        Some(cap) => Flowgraph::new()
            .with_buffer_capacity(cap)
            .with_buffer_kind(BufferKind::Plain),
        None => Flowgraph::new(),
    };
    let mut prev = fg.add(
        "src",
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(rate)),
    );
    for (i, b) in front.into_iter().enumerate() {
        let id = fg.add(format!("f{i}"), b);
        fg.connect(prev, 0, id, 0).unwrap();
        prev = id;
    }
    let dec = fg.add("pocsag", block("pocsag_decoder", decoder));
    fg.connect(prev, 0, dec, 0).unwrap();
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    eprintln!("{text}");
    text
}

/// The lines without their `t=` field.
fn lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(|l| {
            assert!(l.starts_with("t="), "{l}");
            l.split_once(' ').unwrap().1
        })
        .collect()
}

const ALPHA: &[u8] = b"Hello, World! 123 \"quoted\" back\\slash\nline two";

fn pages() -> Vec<Page<'static>> {
    vec![
        Page {
            ric: 1_234_567,
            func: 3,
            content: Content::Alpha(ALPHA),
        },
        Page {
            ric: 1_999_998,
            func: 0,
            content: Content::Numeric("555-0123 [2] U"),
        },
        Page {
            ric: 42,
            func: 1,
            content: Content::Tone,
        },
    ]
}

fn expected(baud: u32) -> Vec<String> {
    vec![
        format!(
            "baud={baud} ric=1234567 func=3 type=alpha msg=\"Hello, World! 123 \\\"quoted\\\" \
             back\\\\slash\\nline two\""
        ),
        format!("baud={baud} ric=1999998 func=0 type=numeric msg=\"555-0123 [2] U\""),
        format!("baud={baud} ric=42 func=1 type=tone"),
    ]
}

#[test]
fn encoder_makes_valid_codewords() {
    // The sync and idle words are codewords of this encoder too.
    assert_eq!(codeword(SYNC >> 11), SYNC);
    assert_eq!(codeword(IDLE >> 11), IDLE);
}

#[test]
fn all_three_rates_decode_in_auto() {
    let mut rng = Rng(0x5eed);
    for baud in [512u32, 1200, 2400] {
        let bits = transmission(&pages(), &[]);
        let x = nrz(&bits, f64::from(baud), 24_000.0, false, 0.3, &mut rng);
        let text = run(x, 24_000.0, vec![], json!({}), None);
        assert_eq!(lines(&text), expected(baud), "{baud} Bd");
    }
}

#[test]
fn inverted_polarity_and_a_fixed_rate() {
    let mut rng = Rng(11);
    let bits = transmission(&pages(), &[]);
    let x = nrz(&bits, 1200.0, 24_000.0, true, 0.3, &mut rng);
    let text = run(x.clone(), 24_000.0, vec![], json!({"baud": "1200"}), None);
    assert_eq!(lines(&text), expected(1200));
    // Listening at the wrong rate finds nothing.
    let text = run(x, 24_000.0, vec![], json!({"baud": "2400"}), None);
    assert!(text.is_empty(), "{text}");
}

#[test]
fn bit_errors_are_repaired_up_to_the_limit() {
    let mut rng = Rng(3);
    // Codeword 1 (first page's first message word, frame 7 → index 15) etc.
    // Indices: page 1's address sits at 14 (frame 7), its message at 15…
    let flips = [
        (14, 1 << 20),              // address: one wrong bit
        (15, (1 << 3) | (1 << 30)), // message: two
        (16, 1),                    // parity bit only
    ];
    let bits = transmission(&pages(), &flips);
    let x = nrz(&bits, 1200.0, 24_000.0, false, 0.3, &mut rng);
    let text = run(x.clone(), 24_000.0, vec![], json!({}), None);
    let got = lines(&text);
    assert_eq!(got[0], format!("{} fixed=4", expected(1200)[0]));
    assert_eq!(&got[1..], &expected(1200)[1..]);
    // Repairing only one bit: the two-bit codeword is kept as received and
    // counted bad; the address (one bit) is still repaired.
    let text = run(x, 24_000.0, vec![], json!({"fix_errors": 1}), None);
    let first = lines(&text)[0];
    assert!(
        first.starts_with("baud=1200 ric=1234567 func=3 type=alpha"),
        "{first}"
    );
    assert!(first.ends_with("fixed=2 bad=1"), "{first}");
}

#[test]
fn formats_can_be_forced() {
    let mut rng = Rng(4);
    let bits = transmission(&pages()[1..2], &[]);
    let x = nrz(&bits, 2400.0, 24_000.0, false, 0.2, &mut rng);
    let text = run(x, 24_000.0, vec![], json!({"format": "both"}), None);
    let l = lines(&text);
    assert_eq!(l.len(), 1);
    assert!(
        l[0].starts_with("baud=2400 ric=1999998 func=0 type=both num=\"555-0123 [2] U\" alpha=\""),
        "{}",
        l[0]
    );
}

#[test]
fn tiny_buffers() {
    let mut rng = Rng(5);
    let bits = transmission(&pages(), &[]);
    let x = nrz(&bits, 2400.0, 12_000.0, false, 0.2, &mut rng);
    let text = run(x, 12_000.0, vec![], json!({"baud": "2400"}), Some(7));
    assert_eq!(lines(&text), expected(2400));
}

/// FM-modulate at `dev` Hz per unit, with complex noise of this RMS.
fn fm_modulate(m: &[f32], rate: f64, dev: f64, noise: f64, rng: &mut Rng) -> Vec<Complex32> {
    let mut phase = 0.0f64;
    m.iter()
        .map(|&x| {
            phase = (phase + TAU * dev * f64::from(x) / rate) % TAU;
            let s = noise / 2f64.sqrt();
            Complex32::new(
                (phase.cos() + s * rng.gauss()) as f32,
                (phase.sin() + s * rng.gauss()) as f32,
            )
        })
        .collect()
}

#[test]
fn through_an_fm_modulator_and_fm_demod() {
    let mut rng = Rng(6);
    let rate = 48_000.0;
    let bits = transmission(&pages(), &[]);
    let x = nrz(&bits, 1200.0, rate, false, 0.0, &mut rng);
    // 4.5 kHz deviation, a 600 Hz carrier offset, noisy.
    let x: Vec<f32> = x.iter().map(|v| v - 0.1 + 600.0 / 4500.0).collect();
    let iq = fm_modulate(&x, rate, 4500.0, 0.4, &mut rng);
    let demod = block(
        "fm_demod",
        json!({"deviation": 4500.0, "deemphasis": "off"}),
    );
    let text = run(iq, rate, vec![demod], json!({}), None);
    assert_eq!(lines(&text), expected(1200));
}

#[test]
fn too_low_a_rate_is_explained() {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(8_000.0)),
    );
    let d = fg.add("p", block("pocsag_decoder", json!({})));
    fg.connect(src, 0, d, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(err.contains("9.6 kS/s") && err.contains("2400 Bd"), "{err}");
    // 512 Bd alone is fine at 8 kS/s.
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(8_000.0)),
    );
    let d = fg.add("p", block("pocsag_decoder", json!({"baud": "512"})));
    fg.connect(src, 0, d, 0).unwrap();
    fg.configure().unwrap();
}

#[test]
fn the_pocsag_example_builds_and_configures() {
    let spec =
        foxygnal_core::GraphSpec::from_toml(include_str!("../../../examples/pocsag.fox.toml"))
            .unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    fg.configure().unwrap();
}
