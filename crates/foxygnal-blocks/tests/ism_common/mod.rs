//! Shared by the ISM decoder tests: line codes to pulse widths, an OOK and
//! an FSK modulator with noise, a frequency offset and timing jitter, and a
//! runner that puts the signal through `ism_decoder`.
#![allow(dead_code)]

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, StreamMeta};
use serde_json::json;

/// Deterministic noise.
pub struct Rng(pub u64);
impl Rng {
    pub fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    pub fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
    }
}

/// Bits from bytes, most significant first, `n` bits.
pub fn bits_of(bytes: &[u8], n: usize) -> Vec<u8> {
    (0..n).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1).collect()
}

/// (pulse µs, gap µs) pairs: the signal starts with a pulse.
pub type Widths = Vec<(f64, f64)>;

/// PWM: a 1 is a short pulse, a 0 a long one, each followed by the rest of
/// `period` (or by `gap` if `period` is 0).
pub fn pwm(bits: &[u8], short: f64, long: f64, period: f64, gap: f64) -> Widths {
    bits.iter()
        .map(|&b| {
            let p = if b == 1 { short } else { long };
            (p, if period > 0.0 { period - p } else { gap })
        })
        .collect()
}

/// PPM: fixed pulses, a short gap for 0, a long one for 1, and a final
/// pulse that ends the last gap (its own gap is 0: add one with [`pause`]).
pub fn ppm(bits: &[u8], pulse: f64, short: f64, long: f64) -> Widths {
    let mut w: Widths = bits
        .iter()
        .map(|&b| (pulse, if b == 1 { long } else { short }))
        .collect();
    w.push((pulse, 0.0));
    w
}

/// Levels (1 = carrier / upper tone) of `half`-µs half-bits to runs.
pub fn runs(levels: &[u8], half: f64) -> Widths {
    let mut w: Widths = Vec::new();
    let mut i = 0;
    // Leading low levels are dropped (before the first pulse).
    while i < levels.len() && levels[i] == 0 {
        i += 1;
    }
    while i < levels.len() {
        let mut p = 0;
        while i < levels.len() && levels[i] == 1 {
            p += 1;
            i += 1;
        }
        let mut g = 0;
        while i < levels.len() && levels[i] == 0 {
            g += 1;
            i += 1;
        }
        w.push((p as f64 * half, g as f64 * half));
    }
    w
}

/// Manchester as IEEE 802.3 (0 = high→low, 1 = low→high), as levels of
/// half-bits: what a PCM slicer and `BitBuffer::manchester_decode` read.
/// The Manchester (zero-bit) slicer reads the opposite: see
/// [`zerobit_levels`].
pub fn manchester_levels(bits: &[u8]) -> Vec<u8> {
    bits.iter()
        .flat_map(|&b| if b == 1 { [0, 1] } else { [1, 0] })
        .collect()
}

/// Manchester as the zero-bit slicer reads it (rtl_433's
/// OOK_PULSE_MANCHESTER_ZEROBIT): a 0 is a rising mid-bit edge (low→high),
/// a 1 a falling one.
pub fn zerobit_levels(bits: &[u8]) -> Vec<u8> {
    bits.iter()
        .flat_map(|&b| if b == 1 { [1, 0] } else { [0, 1] })
        .collect()
}

/// NRZ levels (one per bit).
pub fn nrz_levels(bits: &[u8]) -> Vec<u8> {
    bits.to_vec()
}

/// Append `extra` µs of silence to the last gap (between repeats).
pub fn pause(w: &mut Widths, extra: f64) {
    if let Some(last) = w.last_mut() {
        last.1 += extra;
    } else {
        w.push((0.0, extra));
    }
}

/// How the test signal is impaired.
#[derive(Clone, Copy)]
pub struct Channel {
    pub rate: f64,
    pub snr_db: f64,
    pub offset_hz: f64,
    /// Relative timing jitter of each width (0.1 = ±10 %).
    pub jitter: f64,
    pub seed: u64,
}

impl Channel {
    pub fn new(rate: f64) -> Self {
        Self {
            rate,
            snr_db: 20.0,
            offset_hz: 37e3,
            jitter: 0.1,
            seed: 1,
        }
    }
}

/// Jitter every width by up to ±`j` (relative).
fn jitter(w: &Widths, j: f64, rng: &mut Rng) -> Widths {
    w.iter()
        .map(|&(p, g)| {
            let k = |x: f64, rng: &mut Rng| x * (1.0 + j * (2.0 * rng.uniform() - 1.0));
            (k(p, rng), k(g, rng))
        })
        .collect()
}

/// Lead-in and tail of noise, µs.
const LEAD: f64 = 30_000.0;
const TAIL: f64 = 30_000.0;

/// OOK: a carrier at `offset_hz` while a pulse is on, noise throughout.
/// SNR is the carrier power over the noise power in the full band.
pub fn ook(w: &Widths, ch: &Channel) -> Vec<Complex32> {
    let mut rng = Rng(ch.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let w = jitter(w, ch.jitter, &mut rng);
    let mut levels: Vec<(bool, f64)> = vec![(false, LEAD)];
    for &(p, g) in &w {
        levels.push((true, p));
        levels.push((false, g));
    }
    levels.push((false, TAIL));
    modulate(
        &levels,
        ch,
        &mut rng,
        |on, _| if on { Some(0.0) } else { None },
    )
}

/// FSK: a carrier on throughout the widths, at `offset_hz + dev` during a
/// pulse (mark) and `offset_hz - dev` during a gap; silence before and after.
pub fn fsk(w: &Widths, dev_hz: f64, ch: &Channel) -> Vec<Complex32> {
    let mut rng = Rng(ch.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let w = jitter(w, ch.jitter, &mut rng);
    // (mark, µs), all keyed: an "off" period marks silence.
    let mut levels: Vec<(bool, f64)> = Vec::new();
    for &(p, g) in &w {
        levels.push((true, p));
        levels.push((false, g));
    }
    let mut out = modulate(&[(false, LEAD)], ch, &mut rng, |_, _| None);
    // A short lead of space tone, as transmitters key up before data.
    let mut keyed = vec![(false, 200.0)];
    keyed.extend(levels);
    keyed.push((false, 200.0));
    out.extend(modulate(&keyed, ch, &mut rng, |mark, _| {
        Some(if mark { dev_hz } else { -dev_hz })
    }));
    out.extend(modulate(&[(false, TAIL)], ch, &mut rng, |_, _| None));
    out
}

/// Generate samples for (level, µs) segments; `tone` gives the carrier's
/// frequency (relative to the offset) or None for silence.
fn modulate(
    segs: &[(bool, f64)],
    ch: &Channel,
    rng: &mut Rng,
    tone: impl Fn(bool, f64) -> Option<f64>,
) -> Vec<Complex32> {
    let sigma = (10f64.powf(-ch.snr_db / 10.0) / 2.0).sqrt() * 0.1;
    let amp = 0.1;
    let mut out = Vec::new();
    let mut t = 0.0f64;
    let mut phase = rng.uniform() * TAU;
    let mut acc = 0.0f64;
    for &(lvl, us) in segs {
        acc += us * ch.rate / 1e6;
        let n = acc.round() as usize - out.len().min(acc.round() as usize);
        let f = tone(lvl, t);
        for _ in 0..n {
            let mut s = Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
            if let Some(df) = f {
                phase += TAU * (ch.offset_hz + df) / ch.rate;
                s += Complex32::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32);
            }
            out.push(s);
        }
        t += us;
    }
    let _ = t;
    out
}

/// Concatenate signals.
pub fn concat(parts: &[Vec<Complex32>]) -> Vec<Complex32> {
    parts.iter().flatten().copied().collect()
}

/// Run `sig` through `ism_decoder` with `params`; the decoded lines.
pub fn decode(sig: Vec<Complex32>, rate: f64, params: serde_json::Value) -> Vec<String> {
    dump(&sig, rate);
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(sig, StreamMeta::with_rate(rate)),
    );
    let dec = fg.add(
        "ism",
        foxygnal_blocks::registry()
            .build("ism_decoder", params.as_object().unwrap())
            .unwrap(),
    );
    fg.connect(src, 0, dec, 0).unwrap();
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    eprintln!("{text}");
    text.lines().map(str::to_owned).collect()
}

/// With `ISM_DUMP=dir` set, write each test signal to
/// `dir/<test>-<n>_<rate>.cf32`, to cross-check the encoders with another
/// decoder (`rtl_433 -r file.cf32 -s <rate>`).
fn dump(sig: &[Complex32], rate: f64) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let Some(dir) = std::env::var_os("ISM_DUMP") else {
        return;
    };
    let name = std::thread::current()
        .name()
        .unwrap_or("signal")
        .replace("::", "-");
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::path::Path::new(&dir).join(format!("{name}-{n}_{}k.cf32", rate / 1e3));
    let bytes: Vec<u8> = sig
        .iter()
        .flat_map(|c| [c.re.to_le_bytes(), c.im.to_le_bytes()])
        .flatten()
        .collect();
    std::fs::write(path, bytes).unwrap();
}

/// Decode with only `device` enabled.
pub fn decode_one(sig: Vec<Complex32>, rate: f64, device: &str) -> Vec<String> {
    decode(sig, rate, json!({ "devices": device }))
}

/// The value of `key` in a line.
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ').find_map(|kv| {
        kv.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

/// Assert that `line` has every `key=value` of `expect`.
pub fn check(line: &str, expect: &[(&str, &str)]) {
    for (k, v) in expect {
        assert_eq!(field(line, k), Some(*v), "{k} in {line}");
    }
}

/// The lines of model `model`.
pub fn of_model<'a>(lines: &'a [String], model: &str) -> Vec<&'a str> {
    lines
        .iter()
        .filter(|l| field(l, "model") == Some(model))
        .map(String::as_str)
        .collect()
}

/// The channel conditions every device test is run under: 250 kS/s and
/// 1 MS/s, ±10 % jitter, a frequency offset, 15 dB SNR in the full band.
pub fn channels() -> Vec<Channel> {
    let mut v = Vec::new();
    for (i, &rate) in [250e3, 1e6].iter().enumerate() {
        let mut ch = Channel::new(rate);
        ch.snr_db = 15.0;
        ch.offset_hz = if i == 0 { -23e3 } else { 61e3 };
        ch.seed = 7 + i as u64;
        v.push(ch);
    }
    v
}
