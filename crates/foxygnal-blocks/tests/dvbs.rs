//! DVB-S end to end: TS packets → `dvbs_modulator` → a channel (noise,
//! carrier phase and frequency offset, sample clock offset) →
//! `dvbs_demodulator` → TS packets, compared byte for byte.
//!
//! The channel is applied in the test, between two graphs, so the offsets are
//! exact. Debug builds are slow for this much DSP, so the default tests stay
//! short; `threshold_sweep` (ignored) measures the decoding threshold per code
//! rate: `cargo test --release -p foxygnal-blocks --test dvbs -- --ignored
//! --nocapture`.

use std::f64::consts::TAU;

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

struct Channel {
    /// Es/N0 in dB (None: no noise).
    esn0_db: Option<f64>,
    /// Carrier offset in Hz.
    freq: f64,
    /// Carrier phase in radians.
    phase: f64,
    /// Receiver sample clock error in ppm (positive: it runs fast).
    ppm: f64,
}

/// Modulate `packets`, then apply `ch`.
fn transmit(
    packets: &[[u8; TS]],
    rate: &str,
    sps: usize,
    symbol_rate: f64,
    ch: &Channel,
    seed: u64,
) -> Vec<Complex32> {
    let bytes: Vec<u8> = packets.iter().flatten().copied().collect();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(bytes));
    let gain = 0.5;
    let m = fg.add(
        "mod",
        block(
            "dvbs_modulator",
            json!({ "rate": rate, "sps": sps, "symbol_rate": symbol_rate, "gain": gain }),
        ),
    );
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, m, 0).unwrap();
    fg.connect(m, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let x = out.lock().unwrap().clone();

    // Resample for the clock error (cubic interpolation), then rotate and
    // add noise.
    let fs = symbol_rate * sps as f64;
    let step = 1.0 / (1.0 + ch.ppm * 1e-6);
    let mut rng = Rng(seed);
    let sigma = ch.esn0_db.map_or(0.0, |db| {
        let p = gain * gain;
        (p * sps as f64 / 10f64.powf(db / 10.0) / 2.0).sqrt()
    });
    let mut y = Vec::with_capacity(x.len());
    let mut t = 1.0f64;
    let mut n = 0usize;
    while t + 2.0 < x.len() as f64 {
        let i = t.floor() as usize;
        let mu = (t - i as f64) as f32;
        let p = [x[i - 1], x[i], x[i + 1], x[i + 2]];
        let (a, b, c) = (mu + 1.0, mu - 1.0, mu - 2.0);
        let s = p[0] * (-mu * b * c / 6.0)
            + p[1] * (a * b * c / 2.0)
            + p[2] * (-a * mu * c / 2.0)
            + p[3] * (a * mu * b / 6.0);
        let ph = ch.phase + TAU * ch.freq * n as f64 / fs;
        let rot = Complex32::from_polar(1.0, ph as f32);
        let noise = Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
        y.push(s * rot + noise);
        t += step;
        n += 1;
    }
    y
}

struct Received {
    ts: Vec<u8>,
    status: String,
}

fn receive(x: Vec<Complex32>, rate: &str, sps: usize, symbol_rate: f64, small: bool) -> Received {
    let mut fg = if small {
        Flowgraph::new()
            .with_buffer_capacity(64)
            .with_buffer_kind(BufferKind::Plain)
    } else {
        Flowgraph::new()
    };
    let fs = symbol_rate * sps as f64;
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(x, StreamMeta::with_rate(fs)),
    );
    let d = fg.add(
        "demod",
        block(
            "dvbs_demodulator",
            json!({ "rate": rate, "symbol_rate": symbol_rate }),
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
    // Locate by the first packet that came through clean.
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

#[test]
fn loopback_clean_channel_is_bit_exact() {
    let pkts = ts_packets(70, 1);
    let ch = Channel {
        esn0_db: None,
        freq: 0.0,
        phase: 0.0,
        ppm: 0.0,
    };
    let x = transmit(&pkts, "3/4", 2, 250_000.0, &ch, 2);
    let r = receive(x, "3/4", 2, 250_000.0, false);
    let (good, bad) = compare(&pkts, &r.ts);
    assert_eq!(bad, 0, "{}", r.status);
    assert!(good >= 25, "only {good} packets\n{}", r.status);
    assert!(r.status.contains("event=lock rate=3/4"), "{}", r.status);
}

#[test]
fn loopback_through_noise_offsets_and_clock_error() {
    // 1/2 at 7 dB Es/N0 (≈ 2.5 dB above its threshold), 3 kHz off at
    // 250 kBd, a carrier phase that isn't a multiple of 90°, and the
    // receiver's clock 100 ppm fast. Rate auto, so it also has to find the
    // rate.
    let pkts = ts_packets(55, 3);
    let ch = Channel {
        esn0_db: Some(7.0),
        freq: 3_000.0,
        phase: 2.0,
        ppm: 100.0,
    };
    let x = transmit(&pkts, "1/2", 2, 250_000.0, &ch, 4);
    let r = receive(x, "auto", 2, 250_000.0, false);
    let (good, bad) = compare(&pkts, &r.ts);
    assert_eq!(bad, 0, "{}", r.status);
    assert!(good >= 10, "only {good} packets\n{}", r.status);
    assert!(r.status.contains("event=lock rate=1/2"), "{}", r.status);
    // The status reports the offset it found.
    let f: f64 = r
        .status
        .lines()
        .find(|l| l.contains("event=lock"))
        .and_then(|l| l.split("freq_hz=").nth(1))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap();
    assert!((f - 3_000.0).abs() < 100.0, "{f}");
}

#[test]
fn loopback_with_tiny_buffers_spectrum_inverted_7_8() {
    // Small plain rings split every slice at the wrap; the conjugated
    // channel (a mirrored spectrum, as some receivers deliver) must be found
    // too. 4 samples per symbol and the highest rate.
    let pkts = ts_packets(50, 5);
    let ch = Channel {
        esn0_db: Some(14.0),
        freq: -1_000.0,
        phase: 0.7,
        ppm: -50.0,
    };
    let x: Vec<Complex32> = transmit(&pkts, "7/8", 4, 125_000.0, &ch, 6)
        .into_iter()
        .map(|v| v.conj())
        .collect();
    let r = receive(x, "7/8", 4, 125_000.0, true);
    let (good, bad) = compare(&pkts, &r.ts);
    assert_eq!(bad, 0, "{}", r.status);
    assert!(good >= 10, "only {good} packets\n{}", r.status);
    assert!(r.status.contains("spectrum=inverted"), "{}", r.status);
}

/// Packet error rate per code rate and Es/N0, and where each rate stops
/// decoding. Slow in debug builds.
#[test]
#[ignore]
fn threshold_sweep() {
    let symbol_rate = 250_000.0;
    for (rate, levels) in [
        ("1/2", [2.0, 2.5, 3.0, 3.5, 4.0, 5.0]),
        ("2/3", [3.5, 4.0, 4.5, 5.0, 5.5, 6.5]),
        ("3/4", [4.5, 5.0, 5.5, 6.0, 6.5, 7.5]),
        ("5/6", [5.5, 6.0, 6.5, 7.0, 7.5, 8.5]),
        ("7/8", [6.0, 6.5, 7.0, 7.5, 8.0, 9.0]),
    ] {
        for esn0 in levels {
            let pkts = ts_packets(400, 11);
            let ch = Channel {
                esn0_db: Some(esn0),
                freq: 1_500.0,
                phase: 0.3,
                ppm: 30.0,
            };
            let x = transmit(&pkts, rate, 2, symbol_rate, &ch, 12);
            let r = receive(x, rate, 2, symbol_rate, false);
            let (good, bad) = if r.ts.is_empty() {
                (0, 0)
            } else {
                compare(&pkts, &r.ts)
            };
            if std::env::var("DVBS_STATUS").is_ok() {
                print!("{}", r.status);
            }
            let last = r
                .status
                .lines()
                .rev()
                .find(|l| l.contains("state="))
                .unwrap_or("");
            println!(
                "rate {rate} Es/N0 {esn0:>4.1} dB: {good:>3} good, {bad:>3} bad packets | {last}"
            );
        }
    }
}
