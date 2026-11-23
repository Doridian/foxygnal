//! DVB-T end to end: TS packets → `dvbt_modulator` → a channel (echo,
//! sample clock offset, carrier offset, noise) → `dvbt_demodulator` → TS
//! packets, compared byte for byte.
//!
//! The channel is applied in the test, between two graphs, so the offsets are
//! exact. Debug builds are slow for this much DSP, so the default tests stay
//! short; `threshold_sweep` (ignored) measures the decoding threshold of a few
//! modes: `cargo test --release -p foxygnal-blocks --test dvbt -- --ignored
//! --nocapture`.

use std::f64::consts::{PI, TAU};

use foxygnal_blocks::dvb::dvbt::{Constellation, Mode, elementary_rate};
use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{BufferKind, Complex32, Flowgraph, Instance, StreamMeta};
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
            p[0] = 0x47;
            p[1] = 0x01;
            p[2] = 0x00;
            p[3] = 0x10 | (i as u8 & 0x0F);
            for b in &mut p[4..] {
                *b = rng.next() as u8;
            }
            p
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Tx {
    mode: &'static str,
    cons: &'static str,
    rate: &'static str,
    guard: &'static str,
    bandwidth: &'static str,
}

impl Tx {
    fn new(
        mode: &'static str,
        cons: &'static str,
        rate: &'static str,
        guard: &'static str,
    ) -> Self {
        Self {
            mode,
            cons,
            rate,
            guard,
            bandwidth: "8",
        }
    }

    fn fs(&self) -> f64 {
        elementary_rate(self.bandwidth.parse().unwrap())
    }

    /// TS packets carried by `symbols` OFDM symbols.
    fn packets(&self, symbols: usize) -> usize {
        let mode = Mode::parse(self.mode).unwrap();
        let bits = Constellation::parse(self.cons).unwrap().bits();
        let (k, n) = match self.rate {
            "1/2" => (1, 2),
            "2/3" => (2, 3),
            "3/4" => (3, 4),
            "5/6" => (5, 6),
            _ => (7, 8),
        };
        symbols * mode.data_carriers() * bits * k / n / (204 * 8)
    }
}

struct Channel {
    /// C/N in dB, noise measured in the occupied band (None: no noise).
    cn_db: Option<f64>,
    /// Carrier offset in Hz.
    freq: f64,
    /// Receiver sample clock error in ppm (positive: it runs fast).
    ppm: f64,
    /// A second path: (delay in samples, complex gain).
    echo: Option<(usize, Complex32)>,
    /// Receiver sample rate (None: the elementary rate).
    rx_rate: Option<f64>,
}

const CLEAN: Channel = Channel {
    cn_db: None,
    freq: 0.0,
    ppm: 0.0,
    echo: None,
    rx_rate: None,
};

const GAIN: f64 = 0.25;

/// Modulate `packets`, then apply `ch`.
fn transmit(packets: &[[u8; TS]], tx: Tx, ch: &Channel, seed: u64) -> Vec<Complex32> {
    let bytes: Vec<u8> = packets.iter().flatten().copied().collect();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(bytes));
    let m = fg.add(
        "mod",
        block(
            "dvbt_modulator",
            json!({ "mode": tx.mode, "constellation": tx.cons, "rate": tx.rate,
                    "guard": tx.guard, "bandwidth": tx.bandwidth, "cell_id": 0x1234,
                    "gain": GAIN }),
        ),
    );
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, m, 0).unwrap();
    fg.connect(m, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let mut x = out.lock().unwrap().clone();

    if let Some((d, a)) = ch.echo {
        let orig = x.clone();
        for i in d..x.len() {
            x[i] += orig[i - d] * a;
        }
    }
    let fs = tx.fs();
    let rx_rate = ch.rx_rate.unwrap_or(fs);
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
    let mode = Mode::parse(tx.mode).unwrap();
    let in_band = mode.carriers() as f64 / mode.fft_len() as f64 * fs / rx_rate;
    let sigma = ch.cn_db.map_or(0.0, |db| {
        (GAIN * GAIN / in_band / 10f64.powf(db / 10.0) / 2.0).sqrt()
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

fn receive(x: Vec<Complex32>, rate: f64, bandwidth: &str, small: bool) -> Received {
    let mut fg = if small {
        Flowgraph::new()
            .with_buffer_capacity(64)
            .with_buffer_kind(BufferKind::Plain)
    } else {
        Flowgraph::new()
    };
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(x, StreamMeta::with_rate(rate)),
    );
    let d = fg.add(
        "demod",
        block(
            "dvbt_demodulator",
            json!({ "mode": "auto", "guard": "auto", "bandwidth": bandwidth }),
        ),
    );
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
        if sent[start + i][..] == p[..] {
            good += 1;
        } else {
            bad += 1;
        }
    }
    (good, bad)
}

/// Send `symbols` OFDM symbols' worth of packets through `ch`.
fn run(tx: Tx, ch: &Channel, symbols: usize, seed: u64, small: bool) -> (usize, usize, String) {
    let pkts = ts_packets(tx.packets(symbols), seed);
    let x = transmit(&pkts, tx, ch, seed + 1);
    let rate = ch.rx_rate.unwrap_or(tx.fs());
    let r = receive(x, rate, tx.bandwidth, small);
    let (good, bad) = compare(&pkts, &r.ts);
    (good, bad, r.status)
}

fn noise(n: usize, sigma: f64, seed: u64) -> Vec<Complex32> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|_| Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32))
        .collect()
}

#[test]
fn noise_alone_never_locks_and_a_signal_after_it_does() {
    let fs = elementary_rate(8.0);
    let r = receive(noise(400_000, 0.1, 5), fs, "8", false);
    assert!(r.ts.is_empty());
    assert!(!r.status.contains("event="), "{}", r.status);

    // Noise, then a signal (starting mid-way through a frame).
    let tx = Tx::new("2k", "qpsk", "3/4", "1/8");
    let pkts = ts_packets(tx.packets(SETTLE + 100), 13);
    let sig = transmit(&pkts, tx, &CLEAN, 14);
    let mut x = noise(250_000, 0.01, 15);
    x.extend_from_slice(&sig[37 * 2304 + 999..]);
    let r = receive(x, fs, "8", false);
    let (good, bad) = compare(&pkts, &r.ts);
    assert_eq!(bad, 0, "{}", r.status);
    assert!(good >= 30, "only {good}\n{}", r.status);
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

/// Symbols for TPS lock (up to two frames), the channel estimate and the
/// FEC to settle.
const SETTLE: usize = 160;

fn check(tx: Tx, ch: &Channel, extra: usize, min_good: usize) -> String {
    let (good, bad, status) = run(tx, ch, SETTLE + extra, 7, false);
    assert_eq!(bad, 0, "{good} good\n{status}");
    assert!(good >= min_good, "only {good} packets\n{status}");
    let lock = lock_line(&status);
    for want in [
        format!("mode={}", tx.mode),
        format!("guard={}", tx.guard),
        format!("constellation={}", tx.cons),
        format!("rate={}", tx.rate),
    ] {
        assert!(lock.contains(&want), "{want} not in {lock}");
    }
    status
}

#[test]
fn loopback_2k_matrix_is_bit_exact() {
    for (cons, rate, guard) in [
        ("qpsk", "1/2", "1/32"),
        ("16qam", "3/4", "1/4"),
        ("64qam", "7/8", "1/8"),
        ("64qam", "5/6", "1/16"),
        ("qpsk", "2/3", "1/16"),
    ] {
        let tx = Tx::new("2k", cons, rate, guard);
        let status = check(tx, &CLEAN, 80, 40);
        // The cell id comes from the TPS of two frames.
        assert!(status.contains("cell_id=4660"), "{status}");
    }
}

#[test]
fn loopback_8k_matrix_is_bit_exact() {
    for (cons, rate, guard) in [("16qam", "5/6", "1/32"), ("qpsk", "1/2", "1/4")] {
        let tx = Tx::new("8k", cons, rate, guard);
        check(tx, &CLEAN, 12, 20);
    }
}

#[test]
fn frequency_offset_clock_offset_and_noise() {
    // 8k, 64-QAM 2/3: 3.4 carrier spacings off (3.8 kHz), the receiver's
    // clock 50 ppm fast, C/N 26 dB.
    let tx = Tx::new("8k", "64qam", "2/3", "1/8");
    let spacing = tx.fs() / 8192.0;
    let ch = Channel {
        cn_db: Some(26.0),
        freq: 3.4 * spacing,
        ppm: 50.0,
        ..CLEAN
    };
    let status = check(tx, &ch, 12, 50);
    let lock = lock_line(&status);
    let f = field(lock, "freq_hz");
    assert!((f - ch.freq).abs() < 50.0, "{f}\n{status}");
    let ppm = field(lock, "clock_ppm");
    assert!((ppm - 50.0).abs() < 3.0, "{ppm}\n{status}");
}

#[test]
fn clock_slow_and_offset_down_2k() {
    // 2k, 16-QAM 1/2: 7.6 carrier spacings below (−34 kHz), clock 50 ppm
    // slow, tiny buffers that split every slice.
    let tx = Tx::new("2k", "16qam", "1/2", "1/8");
    let spacing = tx.fs() / 2048.0;
    let ch = Channel {
        cn_db: Some(22.0),
        freq: -7.6 * spacing,
        ppm: -50.0,
        ..CLEAN
    };
    let (good, bad, status) = run(tx, &ch, SETTLE + 40, 9, true);
    assert_eq!(bad, 0, "{status}");
    assert!(good >= 20, "only {good}\n{status}");
}

#[test]
fn two_path_channel() {
    // An echo at −3 dB, 0.3 of the guard interval late, with a phase
    // that puts deep notches in the band.
    let tx = Tx::new("2k", "16qam", "2/3", "1/4");
    let ch = Channel {
        cn_db: Some(24.0),
        echo: Some((154, Complex32::from_polar(0.7, 2.0))),
        ..CLEAN
    };
    check(tx, &ch, 40, 30);
}

#[test]
fn higher_input_rate_7_mhz() {
    // A 7 MHz channel (8 MS/s elementary) received at 10 MS/s: resampled
    // inside the demodulator.
    let mut tx = Tx::new("2k", "64qam", "3/4", "1/32");
    tx.bandwidth = "7";
    let ch = Channel {
        cn_db: Some(28.0),
        freq: 1_200.0,
        rx_rate: Some(10e6),
        ..CLEAN
    };
    check(tx, &ch, 30, 20);
}

/// Near the standard's thresholds (EN 300 744 table A.1, Gaussian channel,
/// for a QEF result after RS: QPSK 1/2 3.1 dB, 16-QAM 1/2 8.8 dB, 64-QAM 2/3
/// 16.5 dB; real receivers need 2–3 dB more).
#[test]
fn awgn_near_threshold() {
    for (tx, cn) in [
        (Tx::new("2k", "qpsk", "1/2", "1/4"), 5.5),
        (Tx::new("2k", "16qam", "1/2", "1/32"), 11.0),
    ] {
        let ch = Channel {
            cn_db: Some(cn),
            freq: 500.0,
            ..CLEAN
        };
        check(tx, &ch, 60, 25);
    }
}

/// Packets decoded per C/N for a few modes. Slow in debug builds.
#[test]
#[ignore]
fn threshold_sweep() {
    for (tx, levels) in [
        (
            Tx::new("2k", "qpsk", "1/2", "1/4"),
            [3.0, 3.5, 4.0, 4.5, 5.0, 6.0],
        ),
        (
            Tx::new("2k", "16qam", "1/2", "1/32"),
            [8.5, 9.0, 9.5, 10.0, 10.5, 11.5],
        ),
        (
            Tx::new("8k", "16qam", "2/3", "1/8"),
            [10.5, 11.0, 11.5, 12.0, 12.5, 13.5],
        ),
        (
            Tx::new("8k", "64qam", "2/3", "1/32"),
            [14.5, 15.0, 15.5, 16.0, 16.5, 17.5],
        ),
        (
            Tx::new("2k", "64qam", "7/8", "1/8"),
            [19.5, 20.0, 20.5, 21.0, 22.0, 23.0],
        ),
    ] {
        for cn in levels {
            let ch = Channel {
                cn_db: Some(cn),
                freq: 1_000.0,
                ppm: 10.0,
                ..CLEAN
            };
            let symbols = SETTLE + if tx.mode == "2k" { 400 } else { 100 };
            let (good, bad, status) = run(tx, &ch, symbols, 21, false);
            if std::env::var("DVBT_STATUS").is_ok() {
                print!("{status}");
            }
            let last = status
                .lines()
                .rev()
                .find(|l| l.contains("state="))
                .unwrap_or("");
            println!(
                "{} {} {} {} C/N {cn:>4.1} dB: {good:>4} good, {bad:>4} bad | {last}",
                tx.mode, tx.cons, tx.rate, tx.guard
            );
        }
    }
}

/// Every mode, constellation, code rate and guard interval through a clean
/// channel (120 combinations). Slow in debug builds.
#[test]
#[ignore]
fn full_matrix() {
    let mut failed = Vec::new();
    for mode in ["2k", "8k"] {
        for cons in ["qpsk", "16qam", "64qam"] {
            for rate in ["1/2", "2/3", "3/4", "5/6", "7/8"] {
                for guard in ["1/4", "1/8", "1/16", "1/32"] {
                    let tx = Tx::new(mode, cons, rate, guard);
                    let extra = if mode == "2k" { 60 } else { 15 };
                    let (good, bad, status) = run(tx, &CLEAN, SETTLE + extra, 3, false);
                    println!("{mode} {cons} {rate} {guard}: {good} good, {bad} bad");
                    if bad > 0 || good < 20 {
                        failed.push(format!("{mode} {cons} {rate} {guard}\n{status}"));
                    }
                }
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
