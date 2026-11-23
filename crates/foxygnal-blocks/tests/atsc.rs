//! ATSC 1.0 (8-VSB).
//!
//! - Against GNU Radio's gr-dtv, with fixtures `scripts/atsc-fixtures.py`
//!   made: our symbols must equal gr-dtv's transmitter's, and our decoder
//!   must decode gr-dtv's symbols.
//! - End to end: TS packets → `atsc_modulator` → a channel (echoes, sample
//!   clock offset, carrier offset, noise) → `atsc_demodulator` → TS packets,
//!   compared byte for byte. The channel is applied in the test, between two
//!   graphs, so the offsets are exact. `threshold_sweep` (ignored) measures
//!   the AWGN threshold: `cargo test --release -p foxygnal-blocks --test atsc
//!   -- --ignored --nocapture`.

use std::f64::consts::{PI, TAU};

use foxygnal_blocks::dvb::atsc::SYMBOL_RATE;
use foxygnal_blocks::dvb::atsc::fec::{self, Encoder, OuterDecoder, SEGMENT, TrellisDecoder};
use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, Instance, StreamMeta};
use serde_json::json;

const TS: usize = 188;

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform().max(1e-300), self.uniform());
        (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    }
}

/// `n` TS packets: PID 0x100, continuity counter, random payload.
fn ts_packets(n: usize, seed: u64) -> Vec<[u8; TS]> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|i| {
            let mut p = [0u8; TS];
            p[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10 | (i as u8 & 0x0F)]);
            for b in &mut p[4..] {
                *b = rng.next() as u8;
            }
            p
        })
        .collect()
}

struct Channel {
    /// SNR in dB: total signal power over the noise in 6 MHz (None: none).
    snr_db: Option<f64>,
    /// Carrier offset in Hz.
    freq: f64,
    /// Receiver sample clock error in ppm (positive: it runs fast).
    ppm: f64,
    /// Echoes: (delay in symbols, negative for a pre-echo; complex gain).
    echoes: &'static [(isize, (f32, f32))],
    /// Receiver sample rate.
    rx_rate: f64,
}

const CLEAN: Channel = Channel {
    snr_db: None,
    freq: 0.0,
    ppm: 0.0,
    echoes: &[],
    rx_rate: SYMBOL_RATE,
};

const GAIN: f64 = 0.25;

/// Modulate `packets` at the symbol rate, then apply `ch`.
fn transmit(packets: &[[u8; TS]], ch: &Channel, seed: u64) -> Vec<Complex32> {
    let bytes: Vec<u8> = packets.iter().flatten().copied().collect();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(bytes));
    let m = fg.add(
        "mod",
        block(
            "atsc_modulator",
            json!({ "sample_rate": SYMBOL_RATE, "gain": GAIN }),
        ),
    );
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, m, 0).unwrap();
    fg.connect(m, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let mut x = out.lock().unwrap().clone();

    if !ch.echoes.is_empty() {
        let orig = x.clone();
        for &(d, (re, im)) in ch.echoes {
            let a = Complex32::new(re, im);
            for (i, v) in x.iter_mut().enumerate() {
                let j = i as isize - d;
                if j >= 0 && (j as usize) < orig.len() {
                    *v += orig[j as usize] * a;
                }
            }
        }
    }
    let fs = SYMBOL_RATE;
    let rx_rate = ch.rx_rate;
    // The receiver's samples, at its (wrong) clock: windowed-sinc
    // interpolation of the transmitted signal.
    let step = fs / (rx_rate * (1.0 + ch.ppm * 1e-6));
    let exact = (step - 1.0).abs() < 1e-15;
    let fc = 0.5 * (1.0f64 / step).min(1.0);
    let half = 24isize;
    let mut y = Vec::with_capacity((x.len() as f64 / step) as usize + 1);
    let mut t = half as f64;
    while t + (half as f64) < x.len() as f64 {
        let s = if exact {
            x[t as usize]
        } else {
            let i = t.floor() as isize;
            let mu = t - i as f64;
            let mut acc = Complex32::default();
            for j in -half + 1..=half {
                let u = j as f64 - mu;
                let a = PI * 2.0 * fc * u;
                let sinc = if a.abs() < 1e-12 { 1.0 } else { a.sin() / a };
                let w = 0.5 + 0.5 * (PI * u / half as f64).cos();
                acc += x[(i + j) as usize] * (2.0 * fc * sinc * w) as f32;
            }
            acc
        };
        y.push(s);
        t += step;
    }
    let mut rng = Rng(seed);
    let sigma = ch.snr_db.map_or(0.0, |db| {
        (GAIN * GAIN / 10f64.powf(db / 10.0) * rx_rate / 6e6 / 2.0).sqrt()
    });
    for (n, v) in y.iter_mut().enumerate() {
        let ph = TAU * ch.freq * n as f64 / rx_rate + 0.4;
        *v *= Complex32::from_polar(1.0, ph as f32);
        if sigma > 0.0 {
            *v += Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
        }
    }
    y
}

struct Received {
    ts: Vec<u8>,
    status: String,
}

fn receive(x: Vec<Complex32>, rate: f64) -> Received {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(x, StreamMeta::with_rate(rate)),
    );
    let d = fg.add("demod", block("atsc_demodulator", json!({})));
    let (ts_sink, ts) = VectorSink::<u8>::instance();
    let (st_sink, st) = VectorSink::<u8>::instance();
    let ts_sink = fg.add("ts", ts_sink);
    let st_sink = fg.add("status", st_sink);
    fg.connect(src, 0, d, 0).unwrap();
    fg.connect(d, 0, ts_sink, 0).unwrap();
    fg.connect(d, 1, st_sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let ts = ts.lock().unwrap().clone();
    let status = String::from_utf8(st.lock().unwrap().clone()).unwrap();
    Received { ts, status }
}

/// Checks the output is whole packets that match a contiguous run of the
/// input; returns (packets matched exactly, packets with errors).
fn compare(sent: &[[u8; TS]], got: &[u8]) -> (usize, usize) {
    assert_eq!(got.len() % TS, 0, "output isn't whole packets");
    let out: Vec<&[u8]> = got.chunks(TS).collect();
    if out.is_empty() {
        return (0, 0);
    }
    for p in &out {
        assert_eq!(p[0], 0x47);
    }
    let Some((k, at)) = out
        .iter()
        .enumerate()
        .find_map(|(k, p)| sent.iter().position(|s| s[..] == p[..]).map(|at| (k, at)))
    else {
        return (0, out.len());
    };
    assert!(at >= k, "output runs ahead of the input");
    let start = at - k;
    let mut good = 0;
    let mut bad = 0;
    for (i, p) in out.iter().enumerate() {
        if sent.get(start + i).is_some_and(|s| s[..] == p[..]) {
            good += 1;
        } else {
            bad += 1;
        }
    }
    (good, bad)
}

/// Packets for lock (pilot, segment and field sync, the deinterleaver) and
/// then `extra` more.
fn run(ch: &Channel, extra: usize, seed: u64) -> (usize, usize, String) {
    let pkts = ts_packets(2 * 312 + extra, seed);
    let x = transmit(&pkts, ch, seed + 1);
    let r = receive(x, ch.rx_rate);
    let (good, bad) = compare(&pkts, &r.ts);
    (good, bad, r.status)
}

fn check(ch: &Channel, extra: usize, min_good: usize) -> String {
    let (good, bad, status) = run(ch, extra, 7);
    assert_eq!(bad, 0, "{good} good\n{status}");
    assert!(good >= min_good, "only {good} packets\n{status}");
    status
}

fn lock_line(status: &str) -> &str {
    status
        .lines()
        .find(|l| l.contains("event=lock"))
        .unwrap_or_else(|| panic!("never locked:\n{status}"))
}

fn field(line: &str, key: &str) -> f64 {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {key} in {line}"))
}

#[test]
fn loopback_is_bit_exact() {
    let status = check(&CLEAN, 400, 300);
    lock_line(&status);
}

#[test]
fn frequency_offset_at_12_msps() {
    // 40 kHz off, received at 12 MS/s (resampled inside), SNR 25 dB.
    let ch = Channel {
        snr_db: Some(25.0),
        freq: 40e3,
        rx_rate: 12e6,
        ..CLEAN
    };
    let status = check(&ch, 400, 300);
    let lock = lock_line(&status);
    let f = field(lock, "freq_hz");
    assert!((f - 40e3).abs() < 100.0, "{f}\n{status}");
}

#[test]
fn clock_offset_50_ppm() {
    for ppm in [50.0, -50.0] {
        let ch = Channel {
            snr_db: Some(25.0),
            freq: -12e3,
            ppm,
            rx_rate: 12e6,
            ..CLEAN
        };
        let status = check(&ch, 400, 300);
        let lock = lock_line(&status);
        let got = field(lock, "clock_ppm");
        assert!((got - ppm).abs() < 5.0, "{got}\n{status}");
    }
}

#[test]
fn multipath() {
    // A strong post-echo (−3 dB, 8 µs) and a weaker pre-echo (−10 dB,
    // 1.5 µs early), with phases that put deep notches in the band.
    const ECHOES: &[(isize, (f32, f32))] = &[(86, (-0.5, 0.5)), (-16, (0.2, -0.22))];
    let ch = Channel {
        snr_db: Some(28.0),
        freq: 5e3,
        echoes: ECHOES,
        rx_rate: 12e6,
        ..CLEAN
    };
    check(&ch, 600, 400);
}

/// Near the 15.2 dB threshold of A/53 (white noise, the ideal receiver
/// needs 14.9 dB for a segment error rate of 1.93·10⁻⁴).
#[test]
fn awgn_near_threshold() {
    let ch = Channel {
        snr_db: Some(16.0),
        freq: 2e3,
        rx_rate: 12e6,
        ..CLEAN
    };
    check(&ch, 600, 400);
}

/// Packets decoded per SNR. Slow in debug builds.
#[test]
#[ignore]
fn threshold_sweep() {
    for snr in [14.0, 14.5, 15.0, 15.5, 16.0, 16.5, 17.0] {
        let ch = Channel {
            snr_db: Some(snr),
            freq: 2e3,
            ppm: 10.0,
            rx_rate: 12e6,
            ..CLEAN
        };
        let (good, bad, status) = run(&ch, 2000, 21);
        let last = status
            .lines()
            .rev()
            .find(|l| l.contains("state="))
            .unwrap_or("");
        println!("SNR {snr:>4.1} dB: {good:>4} good, {bad:>4} bad | {last}");
    }
}

/// The transport stream the fixture script feeds gr-dtv.
fn fixture_packets(n: usize) -> Vec<[u8; TS]> {
    (0..n)
        .map(|i| {
            let mut p = [0u8; TS];
            p[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10 | (i as u8 & 15)]);
            for (k, b) in p.iter_mut().enumerate().skip(4) {
                *b = ((i * 31) ^ (k * 97)) as u8;
            }
            p
        })
        .collect()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// gr-dtv's first 97 segments of symbol indices.
fn grdtv_symbols() -> Vec<u8> {
    let raw = fixture("atsc_grdtv_symbols.bin");
    let n = raw.len() * 8 / 3;
    (0..n)
        .map(|i| {
            let bit = |b: usize| (raw[b / 8] >> (7 - b % 8)) & 1;
            (bit(3 * i) << 2) | (bit(3 * i + 1) << 1) | bit(3 * i + 2)
        })
        .collect()
}

fn fnv64(data: &[u8]) -> u64 {
    data.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

#[test]
fn symbols_match_gr_dtv() {
    let info = String::from_utf8(fixture("atsc_grdtv.txt")).unwrap();
    let field = |k: &str| {
        info.split_whitespace()
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
            .unwrap()
            .to_owned()
    };
    let segments: usize = field("segments").parse().unwrap();
    let hash = u64::from_str_radix(&field("fnv64"), 16).unwrap();

    let mut enc = Encoder::new();
    let mut ours = Vec::new();
    for p in fixture_packets(2 * 312 + 24) {
        enc.push(&p, &mut ours);
    }
    let theirs = grdtv_symbols();
    let n = theirs.len() / SEGMENT * SEGMENT;
    if let Some(i) = (0..n).find(|&i| ours[i] != theirs[i]) {
        panic!(
            "first difference at symbol {i} (segment {}, symbol {}): ours {} gr-dtv {}",
            i / SEGMENT,
            i % SEGMENT,
            ours[i],
            theirs[i]
        );
    }
    // Two whole fields, the second field sync's inverted PN63 included.
    assert_eq!(fnv64(&ours[..segments * SEGMENT]), hash);
}

#[test]
fn decodes_gr_dtv_symbols() {
    let sym = grdtv_symbols();
    let segs = sym.len() / SEGMENT;
    assert_eq!(&sym[4..8], &[1, 1, 1, 1], "starts with a field sync");
    let mut tdec = TrellisDecoder::new();
    let mut outer = OuterDecoder::new();
    let mut got = Vec::new();
    for s in 1..segs {
        for &x in &sym[s * SEGMENT + 4..(s + 1) * SEGMENT] {
            if tdec.push(fec::level(x)) {
                outer.push(tdec.round(), |p| got.push(*p));
            }
        }
    }
    let want = fixture_packets(got.len());
    assert!(got.len() >= 30, "{}", got.len());
    assert_eq!(outer.failed, 0);
    for (k, p) in got.iter().enumerate() {
        assert_eq!(p[..], want[k][..], "packet {k}");
    }
}
