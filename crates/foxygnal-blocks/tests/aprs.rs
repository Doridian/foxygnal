//! AX.25 / APRS end to end: a small independent frame builder (bitwise FCS,
//! bit stuffing, NRZI) and AFSK modulator make packets that go through
//! `hdlc_deframer` directly as bits, and through `afsk1200_demod` as audio,
//! flat, tilted, and through an FM modulator + noise + `fm_demod`.

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

// ---- Frame builder ------------------------------------------------------------------

fn address(call: &str, ssid: u8, last: bool, repeated: bool) -> [u8; 7] {
    let mut a = [b' ' << 1; 7];
    for (i, c) in call.bytes().enumerate() {
        a[i] = c << 1;
    }
    a[6] = 0x60 | (ssid << 1) | u8::from(last) | if repeated { 0x80 } else { 0 };
    a
}

/// A UI frame (PID F0) from N0CALL-9 to APRS via WIDE1-1, WIDE2-1.
fn ui_frame(info: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend(address("APRS", 0, false, false));
    f.extend(address("N0CALL", 9, false, false));
    f.extend(address("WIDE1", 1, false, false));
    f.extend(address("WIDE2", 1, true, false));
    f.extend([0x03, 0xF0]);
    f.extend_from_slice(info);
    f
}

/// CRC-16-CCITT (X.25) one bit at a time.
fn fcs(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        for i in 0..8 {
            let bit = (b >> i) & 1;
            let fb = (crc & 1) as u8 ^ bit;
            crc >>= 1;
            if fb == 1 {
                crc ^= 0x8408;
            }
        }
    }
    !crc
}

/// Line bits (NRZI, as tones: 1 = mark) of frames separated by flags.
fn line_bits(frames: &[Vec<u8>], preamble_flags: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let flag = |data: &mut Vec<u8>| data.extend((0..8).map(|i| (0x7Eu8 >> i) & 1));
    for _ in 0..preamble_flags {
        flag(&mut data);
    }
    for f in frames {
        let mut bytes = f.clone();
        let c = fcs(f);
        bytes.extend([c as u8, (c >> 8) as u8]);
        let mut ones = 0;
        for b in bytes {
            for i in 0..8 {
                let bit = (b >> i) & 1;
                data.push(bit);
                ones = if bit == 1 { ones + 1 } else { 0 };
                if ones == 5 {
                    data.push(0);
                    ones = 0;
                }
            }
        }
        for _ in 0..3 {
            flag(&mut data);
        }
    }
    // NRZI: a 0 changes the tone.
    let mut tone = 1u8;
    data.iter()
        .map(|&b| {
            if b == 0 {
                tone ^= 1;
            }
            tone
        })
        .collect()
}

/// Bell 202 AFSK at `rate`, continuous phase, with silence around it.
fn afsk(bits: &[u8], rate: f64, amplitude: f64) -> Vec<f32> {
    let sps = rate / 1200.0 * 1.0003; // a slightly slow clock
    let pad = (0.02 * rate) as usize;
    let n = (bits.len() as f64 * sps) as usize;
    let mut phase = 0.0f64;
    let mut out = vec![0.0f32; pad];
    for i in 0..n {
        let f = if bits[(i as f64 / sps) as usize] == 1 {
            1200.0
        } else {
            2200.0
        };
        phase += TAU * f / rate;
        out.push((amplitude * phase.sin()) as f32);
    }
    out.extend(std::iter::repeat_n(0.0, pad));
    out
}

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

fn run<T: Sample>(
    input: Vec<T>,
    rate: f64,
    blocks: Vec<Instance>,
    buffers: Option<usize>,
) -> (Vec<u8>, Vec<Tag>) {
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
    for (i, b) in blocks.into_iter().enumerate() {
        let id = fg.add(format!("b{i}"), b);
        fg.connect(prev, 0, id, 0).unwrap();
        prev = id;
    }
    let (sink, out, tags) = VectorSink::<u8>::with_tags();
    let sink = fg.add("sink", sink);
    fg.connect(prev, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let out = out.lock().unwrap().clone();
    let tags = tags.lock().unwrap().clone();
    (out, tags)
}

fn text_lines(out: Vec<u8>) -> Vec<String> {
    let text = String::from_utf8(out).unwrap();
    eprintln!("{text}");
    text.lines()
        .map(|l| {
            assert!(l.starts_with("t="), "{l}");
            l.split_once(' ').unwrap().1.to_string()
        })
        .collect()
}

// Example reports from the APRS Protocol Reference 1.0.1.
const POSITION: &[u8] = b"!4903.50N/07201.75W-Test 001234";
const POSITION_LINE: &str = "src=N0CALL-9 dst=APRS path=WIDE1-1,WIDE2-1 type=position \
                             lat=49.058333 lon=-72.029167 sym=/- comment=\"Test 001234\"";
const COMPRESSED: &[u8] = b"@092345z/5L!!<*e7>7P[";
const COMPRESSED_LINE: &str = "src=N0CALL-9 dst=APRS path=WIDE1-1,WIDE2-1 type=position \
                               time=092345z lat=49.500000 lon=-72.750004 sym=/> course=88 \
                               speed_kt=36.2";
const MESSAGE: &[u8] = b":WU2Z     :Testing{003";
const MESSAGE_LINE: &str = "src=N0CALL-9 dst=APRS path=WIDE1-1,WIDE2-1 type=message \
                            to=\"WU2Z\" msg=\"Testing\" id=\"003\"";

fn frames() -> Vec<Vec<u8>> {
    vec![ui_frame(POSITION), ui_frame(COMPRESSED), ui_frame(MESSAGE)]
}

fn expected() -> Vec<&'static str> {
    vec![POSITION_LINE, COMPRESSED_LINE, MESSAGE_LINE]
}

fn deframer(format: &str) -> Instance {
    block("hdlc_deframer", json!({"format": format}))
}

#[test]
fn deframer_finds_frames_in_bits() {
    let bits = line_bits(&frames(), 4);
    for buffers in [None, Some(5)] {
        let (out, _) = run(bits.clone(), 1200.0, vec![deframer("ax25")], buffers);
        assert_eq!(text_lines(out), expected());
    }
}

#[test]
fn deframer_hex_and_raw_and_bad_fcs() {
    let frame = b"\x12\x34\x7e\xff\xfc\x00".to_vec(); // needs stuffing
    let mut bits = line_bits(std::slice::from_ref(&frame), 2);
    let (out, _) = run(bits.clone(), 1200.0, vec![deframer("hex")], Some(7));
    assert_eq!(text_lines(out), vec!["len=6 data=12347efffc00"]);
    // Not AX.25: the ax25 format falls back to hex.
    let (out, _) = run(bits.clone(), 1200.0, vec![deframer("ax25")], None);
    assert_eq!(text_lines(out), vec!["type=hdlc len=6 data=12347efffc00"]);
    let mut two = line_bits(&[frame.clone(), b"abc".to_vec()], 2);
    let (raw, tags) = run(two.clone(), 1200.0, vec![deframer("raw")], Some(5));
    assert_eq!(raw, b"\x12\x34\x7e\xff\xfc\x00abc");
    let at: Vec<(u64, &TagValue)> = tags
        .iter()
        .filter(|t| &*t.key == "packet")
        .map(|t| (t.offset, &t.value))
        .collect();
    assert_eq!(at, vec![(0, &TagValue::Int(6)), (6, &TagValue::Int(3))]);
    // A flipped bit in the middle of the frame (an extra tone change and
    // the one after it, so the stuffing is unaffected) breaks the FCS.
    let mid = 16 + 3; // bit 3 of 0x12: a 0 becomes 1, no run of 1s
    bits[mid] ^= 1;
    for b in &mut bits[mid + 1..] {
        *b ^= 1;
    }
    let (out, _) = run(bits.clone(), 1200.0, vec![deframer("hex")], None);
    assert!(out.is_empty());
    let (out, _) = run(
        bits,
        1200.0,
        vec![block(
            "hdlc_deframer",
            json!({"format": "hex", "check_fcs": false}),
        )],
        None,
    );
    let l = text_lines(out);
    assert_eq!(l.len(), 1);
    assert!(l[0].starts_with("fcs=bad len=6 data="), "{}", l[0]);
    // Without NRZI the same frames come out of the plain data bits.
    let mut tone = 1u8;
    for b in &mut two {
        let v = u8::from(*b == tone);
        tone = *b;
        *b = v;
    }
    let (out, _) = run(
        two,
        1200.0,
        vec![block(
            "hdlc_deframer",
            json!({"format": "hex", "nrzi": false}),
        )],
        None,
    );
    assert_eq!(
        text_lines(out),
        vec!["len=6 data=12347efffc00", "len=3 data=616263"]
    );
}

fn afsk_chain(format: &str) -> Vec<Instance> {
    vec![block("afsk1200_demod", json!({})), deframer(format)]
}

#[test]
fn afsk_audio_decodes() {
    let bits = line_bits(&frames(), 20);
    let mut rng = Rng(1);
    for rate in [48_000.0, 22_050.0, 9_600.0] {
        let x: Vec<f32> = afsk(&bits, rate, 0.5)
            .iter()
            .map(|&v| v + (0.1 * rng.gauss()) as f32)
            .collect();
        let (out, _) = run(x, rate, afsk_chain("ax25"), None);
        assert_eq!(text_lines(out), expected(), "{rate}");
    }
}

#[test]
fn afsk_with_tiny_buffers() {
    let bits = line_bits(&frames()[..1], 20);
    let x = afsk(&bits, 12_000.0, 0.5);
    let (out, _) = run(x, 12_000.0, afsk_chain("ax25"), Some(7));
    assert_eq!(text_lines(out), vec![POSITION_LINE]);
}

/// First-order tilt: `pre` boosts highs (pre-emphasis, +6 dB/octave above
/// ~2 kHz), otherwise cuts them (de-emphasis-like low-pass at ~1 kHz).
fn tilt(x: &[f32], rate: f64, pre: bool) -> Vec<f32> {
    let mut prev = 0.0f32;
    let mut y = 0.0f32;
    let a = (-TAU * 1_000.0 / rate).exp() as f32;
    x.iter()
        .map(|&v| {
            let out = if pre {
                // x[n] − 0.8·x[n−1], normalised at 1200 Hz roughly.
                2.0 * (v - 0.8 * prev)
            } else {
                y = a * y + (1.0 - a) * v;
                3.0 * y
            };
            prev = v;
            out
        })
        .collect()
}

#[test]
fn afsk_tolerates_emphasis_tilt() {
    let bits = line_bits(&frames(), 20);
    let rate = 24_000.0;
    let x = afsk(&bits, rate, 0.5);
    for pre in [true, false] {
        let (out, _) = run(tilt(&x, rate, pre), rate, afsk_chain("ax25"), None);
        assert_eq!(text_lines(out), expected(), "pre-emphasis {pre}");
    }
}

#[test]
fn afsk_through_fm_and_noise() {
    let bits = line_bits(&frames(), 30);
    let rate = 48_000.0;
    let audio = afsk(&bits, rate, 1.0);
    let mut rng = Rng(7);
    // 3 kHz deviation, a 400 Hz carrier offset, and noise (about 11 dB SNR
    // in the 15 kHz channel).
    let mut phase = 0.0f64;
    let iq: Vec<Complex32> = audio
        .iter()
        .map(|&a| {
            phase = (phase + TAU * (3_000.0 * f64::from(a) + 400.0) / rate) % TAU;
            let s = 0.5 / 2f64.sqrt();
            Complex32::new(
                (phase.cos() + s * rng.gauss()) as f32,
                (phase.sin() + s * rng.gauss()) as f32,
            )
        })
        .collect();
    for deemphasis in ["75us", "off"] {
        let mut chain = vec![
            block(
                "freq_xlating_fir",
                json!({"offset": 0.0, "cutoff": 7_500.0, "transition": 3_000.0, "decimation": 1}),
            ),
            block(
                "fm_demod",
                json!({"deviation": 5_000.0, "deemphasis": deemphasis}),
            ),
        ];
        chain.extend(afsk_chain("ax25"));
        let (out, _) = run(iq.clone(), rate, chain, None);
        assert_eq!(text_lines(out), expected(), "de-emphasis {deemphasis}");
    }
}

#[test]
fn afsk_soft_output_and_rate_checks() {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(48_000.0)),
    );
    let d = fg.add("afsk", block("afsk1200_demod", json!({"output": "soft"})));
    let (sink, _) = VectorSink::<f32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, d, 0).unwrap();
    fg.connect(d, 0, sink, 0).unwrap();
    let edges = fg.configure().unwrap();
    assert_eq!(edges[0].meta.sample_rate, Some(48_000.0));
    let out = edges.iter().find(|e| e.from == "afsk.out").unwrap();
    assert_eq!(out.meta.sample_rate, Some(1200.0));

    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(4_000.0)),
    );
    let d = fg.add("afsk", block("afsk1200_demod", json!({})));
    fg.connect(src, 0, d, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(err.contains("4 kS/s") && err.contains("too slow"), "{err}");
}

#[test]
fn the_aprs_example_builds_and_configures() {
    let spec = foxygnal_core::GraphSpec::from_toml(include_str!("../../../examples/aprs.fox.toml"))
        .unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    let edges = fg.configure().unwrap();
    let into = edges.iter().find(|e| e.to == "afsk.in").unwrap();
    assert_eq!(into.meta.sample_rate, Some(48_000.0));
}
