//! FLEX end to end: a small encoder written from the protocol description
//! (BCH check bits from a shift register in transmission order, sync words
//! built from the published A codes, its own interleaver and 4-level
//! mapping) builds frames that go through `flex_decoder` at every speed, with
//! bit errors, noise, a deviation and clock error, inverted, through tiny
//! buffers and through an FM modulator + `fm_demod`.

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

/// 21 data bits → the 32 bits of a FLEX word in transmission order (bit k is
/// the k-th bit sent): data bit 0 first, then the BCH(31,21) check bits
/// (g(x) = x¹⁰ + x⁹ + x⁸ + x⁶ + x⁵ + x³ + 1, the first bit sent being the
/// highest power), then even parity.
fn codeword(data: u32) -> u32 {
    let mut reg = 0u32;
    for i in 0..21 {
        let fb = ((reg >> 9) & 1) ^ ((data >> i) & 1);
        reg = (reg << 1) & 0x3FF;
        if fb == 1 {
            reg ^= 0x369;
        }
    }
    let mut w = data & 0x1F_FFFF;
    for m in 0..10 {
        w |= ((reg >> (9 - m)) & 1) << (21 + m);
    }
    w | ((w.count_ones() & 1) << 31)
}

/// Fill in the 4-bit checksum (bits 0–3) of an information word: all
/// nibbles plus bit 20 add up to 0xF.
fn with_checksum(w: u32) -> u32 {
    let w = w & !0xF;
    let sum: u32 = (1..5).map(|i| (w >> (4 * i)) & 0xF).sum::<u32>() + (w >> 20);
    w | (0xF_u32.wrapping_sub(sum) & 0xF)
}

#[derive(Clone, Copy)]
struct Speed {
    /// The A code as usually quoted (first half of the A word, 1 = lower
    /// frequency).
    code: u16,
    baud: u32,
    levels: u32,
}

const S1600: Speed = Speed {
    code: 0x870C,
    baud: 1600,
    levels: 2,
};
const S3200_4: Speed = Speed {
    code: 0xB068,
    baud: 1600,
    levels: 4,
};
const S3200_2: Speed = Speed {
    code: 0x7B18,
    baud: 3200,
    levels: 2,
};
const S6400: Speed = Speed {
    code: 0xDEA0,
    baud: 3200,
    levels: 4,
};

#[derive(Clone, Copy)]
enum Msg<'a> {
    Alpha(&'a [u8]),
    /// Part of a longer alphanumeric message: fragment number (3 = the
    /// first, which starts with a signature character) and whether more
    /// follow.
    Fragment(&'a [u8], u32, bool),
    Numeric(&'a str),
    /// Tone only, with up to three (short address) or eight (long) digits.
    Tone(&'a str),
    Binary(&'a [u32]),
    /// A secure page (vector type 0) of alphanumeric text.
    Secure(&'a [u8]),
}

#[derive(Clone, Copy)]
struct Page<'a> {
    phase: usize,
    capcode: u64,
    msg: Msg<'a>,
}

fn digit(c: char) -> u32 {
    "0123456789.U -][".find(c).unwrap() as u32
}

/// Address words for a capcode.
fn address(capcode: u64) -> Vec<u32> {
    if capcode < 2_101_249 {
        return vec![(capcode + 0x8000) as u32];
    }
    let x = capcode - 0x1F_9000;
    let a1 = (x - 1) % 0x8000 + 1;
    let k = (x - a1) >> 15;
    vec![a1 as u32, !(k as u32) & 0x1F_FFFF]
}

/// A message's words (header first for alpha and binary), its vector type
/// and the vector's length field.
fn message(msg: Msg<'_>, number: u32) -> (Vec<u32>, u32) {
    match msg {
        Msg::Alpha(text) => message(Msg::Fragment(text, 3, false), number),
        Msg::Secure(text) => (message(Msg::Alpha(text), number).0, 0),
        Msg::Fragment(text, frag, more) => {
            // Header: message number, fragment number, continuation flag.
            let mut words = vec![(number << 13) | (frag << 11) | (u32::from(more) << 10)];
            let mut chars = if frag == 3 { vec![0x55u32] } else { vec![] };
            chars.extend(text.iter().map(|&c| u32::from(c)));
            while !chars.len().is_multiple_of(3) {
                chars.push(0x03);
            }
            for c in chars.chunks(3) {
                words.push(c[0] | (c[1] << 7) | (c[2] << 14));
            }
            (words, 5)
        }
        Msg::Binary(data) => {
            let mut words = vec![(number << 13) | (3 << 11)];
            words.extend_from_slice(data);
            (words, 6)
        }
        Msg::Numeric(s) => {
            let mut bits: Vec<u32> = vec![0, 0];
            for c in s.chars() {
                let d = digit(c);
                bits.extend((0..4).map(|i| (d >> i) & 1));
            }
            let mut i = 0;
            while !bits.len().is_multiple_of(21) {
                bits.push((0xC >> (i % 4)) & 1);
                i += 1;
            }
            let words = bits
                .chunks(21)
                .map(|c| c.iter().enumerate().map(|(i, &b)| b << i).sum())
                .collect();
            (words, 3)
        }
        Msg::Tone(_) => (vec![], 2),
    }
}

/// The 88 words of one phase carrying these pages.
fn phase_words(pages: &[Page<'_>]) -> [u32; 88] {
    let mut data = [0u32; 88];
    for (i, w) in data.iter_mut().enumerate() {
        *w = if i % 2 == 0 { 0 } else { 0x1F_FFFF };
    }
    let naddr: usize = pages.iter().map(|p| address(p.capcode).len()).sum();
    let (aoff, voff) = (1, 1 + naddr);
    data[0] = with_checksum((voff as u32) << 10);
    let (mut a, mut next) = (aoff, voff + naddr);
    for (n, p) in pages.iter().enumerate() {
        let addr = address(p.capcode);
        let long = addr.len() == 2;
        let v = voff + (a - aoff);
        data[a..a + addr.len()].copy_from_slice(&addr);
        a += addr.len();
        let (mut words, kind) = message(p.msg, n as u32 + 1);
        let total = words.len();
        let vector = match p.msg {
            Msg::Tone(digits) => {
                let mut d = [0xC; 8];
                for (i, c) in digits.chars().enumerate() {
                    d[i] = digit(c);
                }
                if long {
                    data[v + 1] = (0..5).map(|i| d[3 + i] << (4 * i)).sum();
                }
                (d[0] << 9) | (d[1] << 13) | (d[2] << 17) | (kind << 4)
            }
            _ => {
                if long {
                    // The vector field's second word holds the first message word.
                    data[v + 1] = words.remove(0);
                }
                let len = if kind == 3 {
                    total as u32 - 1
                } else {
                    total as u32
                };
                (len << 14) | ((next as u32) << 7) | (kind << 4)
            }
        };
        data[v] = with_checksum(vector);
        data[next..next + words.len()].copy_from_slice(&words);
        next += words.len();
    }
    data.map(codeword)
}

/// One phase's 2816 bits in transmission order: per block, bit 0 of its 8
/// words, then bit 1, …
fn interleave(words: &[u32; 88]) -> Vec<u32> {
    let mut bits = Vec::with_capacity(2816);
    for block in words.chunks(8) {
        for bit in 0..32 {
            bits.extend(block.iter().map(|w| (w >> bit) & 1));
        }
    }
    bits
}

/// A symbol: frequency (±1 = ±4.8 kHz) for 1/baud seconds.
type Sym = (f64, u32);

fn two(b: u32) -> f64 {
    if b == 1 { 1.0 } else { -1.0 }
}

/// 4-FSK: the first bit picks the sign, the second the inner level.
fn four(a: u32, b: u32) -> f64 {
    match (a, b) {
        (1, 0) => 1.0,
        (1, 1) => 1.0 / 3.0,
        (0, 1) => -1.0 / 3.0,
        _ => -1.0,
    }
}

/// `flips` lists (phase, word, bit mask) errors to add.
fn frame(
    speed: Speed,
    cycle: u32,
    number: u32,
    pages: &[Page<'_>],
    flips: &[(usize, usize, u32)],
) -> Vec<Sym> {
    let mut out = Vec::new();
    let bits16 = |v: u64, n: u32, out: &mut Vec<Sym>| {
        for i in (0..n).rev() {
            out.push((two(((v >> i) & 1) as u32), 1600));
        }
    };
    // Sync 1 (1 = higher frequency): dotting, A, B, A inverted.
    let a = !((u32::from(speed.code) << 16) | 0xA6C6);
    bits16(0xAAAA_AAAA, 32, &mut out);
    bits16(u64::from(a), 32, &mut out);
    bits16(0x5555, 16, &mut out);
    bits16(u64::from(!a), 32, &mut out);
    let fiw = codeword(with_checksum((number << 8) | (cycle << 4)));
    for i in 0..32 {
        out.push((two((fiw >> i) & 1), 1600));
    }
    // Sync 2: 25 ms at the frame's speed (content not checked).
    for i in 0..speed.baud / 40 {
        out.push((if i % 2 == 0 { 1.0 } else { -1.0 }, speed.baud));
    }
    let phases: Vec<Vec<u32>> = (0..4)
        .map(|p| {
            let mine: Vec<Page<'_>> = pages.iter().filter(|g| g.phase == p).copied().collect();
            let mut words = if mine.is_empty() {
                let mut idle = [0u32; 88];
                for (i, w) in idle.iter_mut().enumerate() {
                    *w = codeword(if i % 2 == 0 { 0 } else { 0x1F_FFFF });
                }
                idle
            } else {
                phase_words(&mine)
            };
            for &(fp, i, mask) in flips {
                if fp == p {
                    words[i] ^= mask;
                }
            }
            interleave(&words)
        })
        .collect();
    let b = speed.baud;
    let columns = phases[0]
        .iter()
        .zip(&phases[1])
        .zip(&phases[2])
        .zip(&phases[3]);
    for (((&pa, &pb), &pc), &pd) in columns {
        match (speed.baud, speed.levels) {
            (1600, 2) => out.push((two(pa), b)),
            // 3200 bps has phases A and C, however it is modulated.
            (1600, _) => out.push((four(pa, pc), b)),
            (_, 2) => out.extend([(two(pa), b), (two(pc), b)]),
            _ => out.extend([(four(pa, pb), b), (four(pc, pd), b)]),
        }
    }
    out
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

struct Air {
    rate: f64,
    /// Deviation relative to nominal, and its sign.
    gain: f64,
    /// Transmitter clock error.
    ppm: f64,
    noise: f64,
    dc: f64,
}

const AIR: Air = Air {
    rate: 48_000.0,
    gain: 1.0,
    ppm: 0.0,
    noise: 0.2,
    dc: 0.05,
};

/// Discriminator output: the symbols, with noise (and silence) around them.
fn render(syms: &[Sym], air: &Air, rng: &mut Rng) -> Vec<f32> {
    let scale = 1.0 + air.ppm * 1e-6;
    let pad = 0.1;
    let mut ends = Vec::with_capacity(syms.len());
    let mut t = pad;
    for &(v, baud) in syms {
        t += 1.0 / (f64::from(baud) * scale);
        ends.push((t, v));
    }
    let n = ((t + pad) * air.rate) as usize;
    let mut k = 0;
    (0..n)
        .map(|i| {
            let ti = i as f64 / air.rate;
            while k < ends.len() && ends[k].0 <= ti {
                k += 1;
            }
            let v = if ti < pad || k == ends.len() {
                0.0
            } else {
                ends[k].1
            };
            if v.is_nan() {
                return (2.0 * rng.gauss()) as f32;
            }
            (air.dc + air.gain * v + air.noise * rng.gauss()) as f32
        })
        .collect()
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
    let dec = fg.add("flex", block("flex_decoder", decoder));
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

/// Pages spread over the phases a speed has.
fn pages(speed: Speed) -> Vec<Page<'static>> {
    let phases: &[usize] = match (speed.baud, speed.levels) {
        (1600, 2) => &[0],
        (1600, _) => &[0, 2],
        (_, 2) => &[0, 2],
        _ => &[0, 1, 2, 3],
    };
    let ph = |i: usize| phases[i % phases.len()];
    vec![
        Page {
            phase: ph(0),
            capcode: 1_234_567,
            msg: Msg::Alpha(ALPHA),
        },
        Page {
            phase: ph(1),
            capcode: 200_001,
            msg: Msg::Numeric("555-0123 [2] U"),
        },
        Page {
            phase: ph(2),
            capcode: 42,
            msg: Msg::Tone(""),
        },
        Page {
            phase: ph(3),
            capcode: 1_000_000,
            msg: Msg::Tone("911"),
        },
    ]
}

fn expected(speed: Speed) -> Vec<String> {
    let mut ps = pages(speed);
    // The decoder prints phase by phase, in address order.
    ps.sort_by_key(|p| p.phase);
    ps.iter()
        .map(|p| {
            let head = format!(
                "baud={} levels={} phase={} cycle=3 frame=45 capcode={:09}",
                speed.baud,
                speed.levels,
                char::from(b'A' + p.phase as u8),
                p.capcode
            );
            match p.msg {
                Msg::Alpha(_) => format!(
                    "{head} type=alpha msg=\"Hello, World! 123 \\\"quoted\\\" \
                     back\\\\slash\\nline two\""
                ),
                Msg::Numeric(s) => format!("{head} type=numeric msg=\"{s}\""),
                Msg::Tone("") => format!("{head} type=tone"),
                Msg::Tone(d) => format!("{head} type=tone msg=\"{d}\""),
                Msg::Binary(_) | Msg::Fragment(..) | Msg::Secure(_) => unreachable!(),
            }
        })
        .collect()
}

const SPEEDS: [Speed; 4] = [S1600, S3200_4, S3200_2, S6400];

#[test]
fn encoder_words_are_codewords() {
    // All zeros and all ones are FLEX's idle words; both are codewords.
    assert_eq!(codeword(0), 0);
    assert_eq!(codeword(0x1F_FFFF), 0xFFFF_FFFF);
    // The check bits are the remainder of the data (first bit = highest
    // power) times x¹⁰, by polynomial long division.
    for data in [1u32, 0x12345, 0x1F0F0F] {
        let w = codeword(data);
        let mut poly: u64 = 0;
        for k in 0..31 {
            poly = (poly << 1) | u64::from((w >> k) & 1);
        }
        for i in (10..31).rev() {
            if poly & (1 << i) != 0 {
                poly ^= 0x769 << (i - 10);
            }
        }
        assert_eq!(poly, 0, "{data:x}");
    }
}

#[test]
fn every_speed_decodes() {
    let mut rng = Rng(0x5eed);
    for speed in SPEEDS {
        let syms = frame(speed, 3, 45, &pages(speed), &[]);
        let x = render(&syms, &AIR, &mut rng);
        let text = run(x, AIR.rate, vec![], json!({}), None);
        assert_eq!(
            lines(&text),
            expected(speed),
            "{} {}",
            speed.baud,
            speed.levels
        );
    }
}

#[test]
fn long_addresses_and_binary() {
    let mut rng = Rng(7);
    let words = [0x0ABCDE, 0x1F0001];
    let pages = [
        Page {
            phase: 0,
            capcode: 2_101_249,
            msg: Msg::Alpha(b"long one"),
        },
        Page {
            phase: 0,
            capcode: 123_456_789,
            msg: Msg::Numeric("0123456789"),
        },
        Page {
            phase: 0,
            capcode: 2_500_000,
            msg: Msg::Tone("12345678"),
        },
        Page {
            phase: 0,
            capcode: 77,
            msg: Msg::Binary(&words),
        },
    ];
    let syms = frame(S1600, 0, 0, &pages, &[]);
    let x = render(&syms, &AIR, &mut rng);
    let text = run(x, AIR.rate, vec![], json!({}), None);
    let head = "baud=1600 levels=2 phase=A cycle=0 frame=0";
    assert_eq!(
        lines(&text),
        [
            format!("{head} capcode=002101249 type=alpha msg=\"long one\""),
            format!("{head} capcode=123456789 type=numeric msg=\"0123456789\""),
            format!("{head} capcode=002500000 type=tone msg=\"12345678\""),
            format!("{head} capcode=000000077 type=binary hex=\"0abcde 1f0001\""),
        ]
    );
}

#[test]
fn fragments_are_marked() {
    let mut rng = Rng(12);
    let page = |capcode, msg| Page {
        phase: 0,
        capcode,
        msg,
    };
    let pages = [
        page(100_001, Msg::Fragment(b"first part, ", 3, true)),
        page(100_002, Msg::Fragment(b"middle part", 0, true)),
        page(100_003, Msg::Fragment(b"the end.", 1, false)),
    ];
    let syms = frame(S3200_2, 1, 2, &pages, &[]);
    let x = render(&syms, &AIR, &mut rng);
    let text = run(x, AIR.rate, vec![], json!({}), None);
    let head = "baud=3200 levels=2 phase=A cycle=1 frame=2";
    assert_eq!(
        lines(&text),
        [
            format!("{head} capcode=000100001 type=alpha msg=\"first part, \" part=first"),
            format!("{head} capcode=000100002 type=alpha msg=\"middle part\" part=more"),
            format!("{head} capcode=000100003 type=alpha msg=\"the end.\" part=last"),
        ]
    );
}

#[test]
fn bit_errors_are_repaired_up_to_the_limit() {
    let mut rng = Rng(3);
    let pages = pages(S1600);
    // Phase A: BIW at 0, four addresses at 1–4, vectors 5–8, the alpha
    // message at 9.
    let flips = [
        (0, 0, 1 << 5),               // BIW: one bit
        (0, 1, (1 << 3) | (1 << 30)), // first address: two
        (0, 10, 1 << 31),             // a message word's parity bit
    ];
    let syms = frame(S1600, 3, 45, &pages, &flips);
    let x = render(&syms, &AIR, &mut rng);
    let text = run(x.clone(), AIR.rate, vec![], json!({}), None);
    let got = lines(&text);
    assert_eq!(got[0], format!("{} fixed=3", expected(S1600)[0]));
    assert_eq!(&got[1..], &expected(S1600)[1..]);
    // One bit per word: the two-bit address is lost, the rest decodes.
    let text = run(x, AIR.rate, vec![], json!({"fix_errors": 1}), None);
    assert_eq!(lines(&text), &expected(S1600)[1..]);
}

#[test]
fn inverted_weak_and_off_clock() {
    let mut rng = Rng(11);
    for (speed, ppm) in [(S1600, 300.0), (S6400, -300.0), (S3200_4, 250.0)] {
        let air = Air {
            gain: -0.6,
            ppm,
            noise: 0.15,
            dc: -0.2,
            ..AIR
        };
        let syms = frame(speed, 3, 45, &pages(speed), &[]);
        let x = render(&syms, &air, &mut rng);
        let text = run(x, air.rate, vec![], json!({}), None);
        assert_eq!(
            lines(&text),
            expected(speed),
            "{} {}",
            speed.baud,
            speed.levels
        );
    }
}

#[test]
fn secure_pages_on_short_addresses_in_phase_b() {
    // A layout seen on a real 6400 bps channel (made-up capcodes): phase B mixing short addresses
    // (secure, alpha) with long ones, so vectors sit at shifted positions.
    let mut rng = Rng(13);
    let page = |capcode, msg| Page {
        phase: 1,
        capcode,
        msg,
    };
    let pages = [
        page(205, Msg::Alpha(b"short")),
        page(8_300_000, Msg::Binary(&[0x012345])),
        page(70_001, Msg::Secure(b"one")),
        page(8_312_345, Msg::Alpha(b"long")),
        page(600_123, Msg::Secure(b"two")),
    ];
    let syms = frame(S6400, 6, 32, &pages, &[]);
    let x = render(&syms, &AIR, &mut rng);
    let text = run(x, AIR.rate, vec![], json!({}), None);
    let head = "baud=3200 levels=4 phase=B cycle=6 frame=32";
    assert_eq!(
        lines(&text),
        [
            format!("{head} capcode=000000205 type=alpha msg=\"short\""),
            format!("{head} capcode=008300000 type=binary hex=\"012345\""),
            format!("{head} capcode=000070001 type=secure msg=\"one\""),
            format!("{head} capcode=008312345 type=alpha msg=\"long\""),
            format!("{head} capcode=000600123 type=secure msg=\"two\""),
        ]
    );
}

#[test]
fn shortened_frames() {
    // Transmitters key off once the blocks in use are sent; the receiver
    // hears noise for the rest of the frame, then the next frame on time.
    let mut rng = Rng(14);
    let pages = &pages(S6400)[..2];
    let mut syms = frame(S6400, 3, 45, pages, &[]);
    // Sync 1 + FIW + Sync 2 + 3 blocks of 160 ms (2 × 512 symbols each at 3200 Bd).
    let used = 144 + 80 + 3 * 512;
    for s in &mut syms[used..] {
        s.0 = f64::NAN;
    }
    syms.extend(frame(S6400, 3, 46, pages, &[]));
    let x = render(&syms, &AIR, &mut rng);
    let text = run(x, AIR.rate, vec![], json!({}), None);
    let want: Vec<String> = expected(S6400)[..2].to_vec();
    let mut both = want.clone();
    both.extend(want.iter().map(|l| l.replace("frame=45", "frame=46")));
    assert_eq!(lines(&text), both);
}

#[test]
fn frames_back_to_back() {
    // Continuous transmission: a 6400 bps frame right after a 1600 bps one.
    let mut rng = Rng(5);
    let mut syms = frame(S1600, 3, 45, &pages(S1600), &[]);
    syms.extend(frame(S6400, 3, 45, &pages(S6400), &[]));
    syms.extend(frame(S3200_2, 3, 45, &pages(S3200_2), &[]));
    let x = render(&syms, &AIR, &mut rng);
    let text = run(x, AIR.rate, vec![], json!({}), None);
    let mut want = expected(S1600);
    want.extend(expected(S6400));
    want.extend(expected(S3200_2));
    assert_eq!(lines(&text), want);
}

#[test]
fn tiny_buffers() {
    let mut rng = Rng(6);
    let air = Air {
        rate: 16_000.0,
        ..AIR
    };
    let syms = frame(S6400, 3, 45, &pages(S6400), &[]);
    let x = render(&syms, &air, &mut rng);
    let text = run(x, air.rate, vec![], json!({}), Some(64));
    assert_eq!(lines(&text), expected(S6400));
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
    let mut rng = Rng(8);
    for speed in SPEEDS {
        // 4.8 kHz deviation, a 600 Hz carrier offset, noisy.
        let air = Air {
            noise: 0.0,
            dc: 600.0 / 4800.0,
            ..AIR
        };
        let syms = frame(speed, 3, 45, &pages(speed), &[]);
        let x = render(&syms, &air, &mut rng);
        let iq = fm_modulate(&x, air.rate, 4800.0, 0.3, &mut rng);
        // The example's channel filter (its ISI included), then the demodulator.
        let channel = block(
            "freq_xlating_fir",
            json!({"offset": 0.0, "cutoff": 9000.0, "transition": 6000.0,
                   "decimation": 1, "window": "hamming"}),
        );
        let demod = block(
            "fm_demod",
            json!({"deviation": 4800.0, "deemphasis": "off"}),
        );
        let text = run(iq, air.rate, vec![channel, demod], json!({}), None);
        assert_eq!(
            lines(&text),
            expected(speed),
            "{} {}",
            speed.baud,
            speed.levels
        );
    }
}

#[test]
fn noise_alone_decodes_nothing() {
    let mut rng = Rng(9);
    let x: Vec<f32> = (0..480_000).map(|_| rng.gauss() as f32).collect();
    let text = run(x, 48_000.0, vec![], json!({}), None);
    assert!(text.is_empty(), "{text}");
}

#[test]
fn too_low_a_rate_is_explained() {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(8_000.0)),
    );
    let d = fg.add("f", block("flex_decoder", json!({})));
    fg.connect(src, 0, d, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(
        err.contains("12.8 kS/s") && err.contains("3200 Bd"),
        "{err}"
    );
}

#[test]
fn the_flex_example_builds_and_configures() {
    let spec = foxygnal_core::GraphSpec::from_toml(include_str!("../../../examples/flex.fox.toml"))
        .unwrap();
    let mut fg = Flowgraph::from_spec(&spec, &foxygnal_blocks::registry()).unwrap();
    fg.configure().unwrap();
}
