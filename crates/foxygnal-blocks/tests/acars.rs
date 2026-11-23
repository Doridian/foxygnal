//! ACARS end to end: a small independent transmitter (its own parity, a
//! bitwise CRC, the block layout of ARINC 618 and a phase-continuous MSK
//! tone generator: 2400 Hz when a bit repeats the one before, 1200 Hz when
//! it changes) makes blocks that go through `acars_decoder` as audio with
//! noise, a clock error and inverted polarity, through tiny buffers, and
//! through an AM modulator + carrier offset + noise + `am_demod`. The
//! sensitivity is measured against Eb/N0.

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{BufferKind, Complex32, Flowgraph, Instance, Sample, StreamMeta};
use serde_json::json;

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

// ---- Transmitter ------------------------------------------------------------------

fn parity(c: u8) -> u8 {
    if c.count_ones().is_multiple_of(2) {
        c | 0x80
    } else {
        c
    }
}

/// CRC-16 (CCITT polynomial, reflected, initial value 0), bit by bit.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        for i in 0..8 {
            let fb = (crc ^ u16::from(b >> i)) & 1;
            crc >>= 1;
            if fb == 1 {
                crc ^= 0x8408;
            }
        }
    }
    crc
}

struct Msg<'a> {
    mode: u8,
    addr: &'a [u8; 7],
    ack: u8,
    label: &'a [u8; 2],
    block: u8,
    /// Text after STX (for downlinks starting with message and flight
    /// number); none = no STX.
    text: Option<&'a [u8]>,
    more: bool,
}

/// The characters of a block, pre-key to the trailing DEL, and the bit
/// flips to add (character index from SOH + 1, mask).
fn block_bytes(m: &Msg<'_>, flips: &[(usize, u8)]) -> Vec<u8> {
    let mut body = vec![m.mode];
    body.extend(m.addr);
    body.push(m.ack);
    body.extend(m.label);
    body.push(m.block);
    if let Some(t) = m.text {
        body.push(0x02);
        body.extend(t);
    }
    body.push(if m.more { 0x17 } else { 0x03 });
    let mut body: Vec<u8> = body.into_iter().map(parity).collect();
    let c = crc16(&body);
    body.extend([c as u8, (c >> 8) as u8]);
    for &(i, mask) in flips {
        body[i] ^= mask;
    }
    let mut out = vec![0xFF; 16];
    out.extend([parity(b'+'), parity(b'*'), 0x16, 0x16, 0x01]);
    out.extend(body);
    out.push(0x7F);
    out
}

/// MSK audio of `blocks` (each after `gap` seconds of silence): bits least
/// significant first, 2400 Hz when a bit equals the previous one, 1200 Hz
/// when it changes. `clock` scales the bit rate.
fn msk(blocks: &[Vec<u8>], rate: f64, clock: f64, gap: f64, phase: f64) -> Vec<f32> {
    let mut out = Vec::new();
    for b in blocks {
        out.extend(std::iter::repeat_n(0.0f32, (gap * rate) as usize));
        let bits: Vec<u8> = b
            .iter()
            .flat_map(|&c| (0..8).map(move |i| (c >> i) & 1))
            .collect();
        // Tone of each bit, and the phase at its start: the signal is
        // computed exactly at each sample time, whatever the sample rate.
        // One oscillator makes both the bit clock and the tones.
        let tb = 1.0 / (2400.0 * clock);
        let mut starts = Vec::with_capacity(bits.len());
        let (mut ph, mut prev) = (phase, 1);
        for &b in &bits {
            let f = if b == prev { 2400.0 } else { 1200.0 } * clock;
            starts.push((ph, f));
            ph += TAU * f * tb;
            prev = b;
        }
        let n = (bits.len() as f64 * tb * rate) as usize;
        for i in 0..n {
            let t = i as f64 / rate;
            let k = ((t / tb) as usize).min(bits.len() - 1);
            let (p0, f) = starts[k];
            out.push((p0 + TAU * f * (t - k as f64 * tb)).sin() as f32);
        }
    }
    out.extend(std::iter::repeat_n(0.0f32, (gap * rate) as usize));
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

/// Add white noise for an Eb/N0 of `ebn0_db` (unit-amplitude tones).
fn noisy(x: &[f32], rate: f64, ebn0_db: f64, rng: &mut Rng) -> Vec<f32> {
    // Eb = (1/2)/2400, N0 = 2σ²/rate.
    let sigma = (rate / (4.0 * 2400.0 * 10f64.powf(ebn0_db / 10.0))).sqrt();
    x.iter()
        .map(|&v| (f64::from(v) + sigma * rng.gauss()) as f32)
        .collect()
}

fn run<T: Sample>(
    input: Vec<T>,
    rate: f64,
    front: Vec<Instance>,
    decoder: serde_json::Value,
    cap: Option<usize>,
) -> String {
    let mut fg = match cap {
        Some(c) => Flowgraph::new()
            .with_buffer_capacity(c)
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
    let dec = fg.add("acars", block("acars_decoder", decoder));
    fg.connect(prev, 0, dec, 0).unwrap();
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    String::from_utf8(out.lock().unwrap().clone()).unwrap()
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

const DOWN: Msg<'static> = Msg {
    mode: b'2',
    addr: b".N12345",
    ack: 0x15,
    label: b"H1",
    block: b'3',
    text: Some(b"M01AXX0123#DFB\"quoted\"\r\nPOS N4012.3W07401.2"),
    more: false,
};

const UP: Msg<'static> = Msg {
    mode: b'2',
    addr: b".G-ABCD",
    ack: b'3',
    label: b"_\x7f",
    block: b'A',
    text: None,
    more: false,
};

const MORE: Msg<'static> = Msg {
    mode: b'E',
    addr: b"..D-EFG",
    ack: 0x15,
    label: b"Q0",
    block: b'S',
    text: Some(b"PART ONE"),
    more: true,
};

const WANT: [&str; 3] = [
    "freq=131.550 mode=2 reg=N12345 ack=NAK label=H1 block=3 msgno=M01A flight=XX0123 \
     text=\"#DFB\\\"quoted\\\"\\r\\nPOS N4012.3W07401.2\"",
    "freq=131.550 mode=2 reg=G-ABCD ack=3 label=_d block=A",
    "freq=131.550 mode=E reg=D-EFG ack=NAK label=Q0 block=S text=\"PART ONE\" more=1",
];

fn three() -> Vec<Vec<u8>> {
    vec![
        block_bytes(&DOWN, &[]),
        block_bytes(&UP, &[]),
        block_bytes(&MORE, &[]),
    ]
}

#[test]
fn blocks_decode_at_both_rates_and_polarities() {
    let mut rng = Rng(77);
    for (rate, clock, polarity) in [(12_500.0, 1.0, 1.0), (48_000.0, 1.0002, -1.0)] {
        let x: Vec<f32> = msk(&three(), rate, clock, 0.05, 0.4)
            .iter()
            .map(|&v| v * polarity)
            .collect();
        let x = noisy(&x, rate, 15.0, &mut rng);
        for cap in [None, Some(61)] {
            let text = run(x.clone(), rate, vec![], json!({"frequency": 131.55e6}), cap);
            assert_eq!(lines(&text), WANT, "{rate} {cap:?}\n{text}");
        }
    }
}

#[test]
fn any_start_phase_decodes() {
    for i in 0..12 {
        for rate in [12_500.0, 48_000.0] {
            let x = msk(
                &[block_bytes(&DOWN, &[])],
                rate,
                1.0,
                0.03,
                f64::from(i) * 0.53,
            );
            let text = run(x, rate, vec![], json!({"frequency": 131.55e6}), None);
            assert_eq!(lines(&text), WANT[..1], "start phase {i} at {rate}");
        }
    }
}

#[test]
fn repairs_bit_errors() {
    // After SOH: mode is character 0; the text starts at 13.
    let x = msk(
        &[
            block_bytes(&DOWN, &[(20, 0x04)]),
            block_bytes(&DOWN, &[(14, 0x01), (30, 0x20)]),
            block_bytes(&DOWN, &[(14, 0x01), (30, 0x20), (31, 0x02)]),
        ],
        12_500.0,
        1.0,
        0.05,
        1.0,
    );
    let text = run(
        x.clone(),
        12_500.0,
        vec![],
        json!({"frequency": 131.55e6}),
        None,
    );
    let got = lines(&text);
    assert_eq!(
        got,
        [
            format!("{} fixed=1", WANT[0]),
            format!("{} fixed=2", WANT[0])
        ]
    );
    // Repairs off: only intact blocks.
    let text = run(x, 12_500.0, vec![], json!({"fix_errors": 0}), None);
    assert_eq!(text, "");
}

#[test]
fn am_receiver_chain() {
    // An AM carrier (80 % modulation) 1.5 kHz off centre, complex noise,
    // then an AM demodulator, all at 48 kS/s.
    let rate = 48_000.0;
    let audio = msk(&three(), rate, 1.0, 0.1, 2.0);
    let mut rng = Rng(3);
    let iq: Vec<Complex32> = audio
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            let ph = TAU * 1500.0 * i as f64 / rate + 0.3;
            let env = 1.0 + 0.8 * f64::from(a);
            Complex32::new(
                (env * ph.cos() + 0.15 * rng.gauss()) as f32,
                (env * ph.sin() + 0.15 * rng.gauss()) as f32,
            )
        })
        .collect();
    let front = vec![
        block(
            "freq_xlating_fir",
            json!({"offset": 0.0, "cutoff": 6000.0, "transition": 3000.0}),
        ),
        block("am_demod", json!({})),
    ];
    let text = run(iq, rate, front, json!({"frequency": 131.55e6}), None);
    assert_eq!(lines(&text), WANT, "{text}");
}

#[test]
fn noise_alone_decodes_nothing() {
    let mut rng = Rng(9);
    let x: Vec<f32> = (0..12_500 * 20).map(|_| rng.gauss() as f32).collect();
    assert_eq!(run(x, 12_500.0, vec![], json!({}), None), "");
}

/// Blocks decoded out of `n` at an Eb/N0.
fn decode_rate(ebn0: f64, n: usize) -> usize {
    let rate = 12_500.0;
    let x = msk(&vec![block_bytes(&DOWN, &[]); n], rate, 1.0, 0.02, 0.7);
    let mut rng = Rng(400 + ebn0 as u64);
    let x = noisy(&x, rate, ebn0, &mut rng);
    run(x, rate, vec![], json!({}), None).lines().count()
}

#[test]
fn decodes_most_blocks_at_8_db() {
    let n = 40;
    let got = decode_rate(8.0, n);
    assert!(got >= 36, "{got}/{n}");
}

/// Run with `--ignored --nocapture` to print the curve.
#[test]
#[ignore]
fn sensitivity_curve() {
    let n = 200;
    for ebn0 in [4.0, 5.0, 6.0, 7.0, 8.0, 10.0] {
        println!("Eb/N0 {ebn0:4.1} dB: {}/{n}", decode_rate(ebn0, n));
    }
}
