//! Metop AHRPT end to end: `ahrpt_encoder` (synthetic AVHRR/3 lines and an
//! MHS packet in CCSDS frames) → `ahrpt_modulator` (RS, randomizer, rate 3/4
//! punctured code, QPSK at 2.33 MSym/s) → a channel (Doppler-sized carrier
//! offset, symbol clock error, fractional timing offset, noise, a dropout)
//! → `ahrpt_demodulator` → `ccsds_packets` → `avhrr_decoder`, and the
//! decoded lines compared with the ones sent (the chain is lossless).

use std::f64::consts::TAU;

use foxygnal_blocks::polar::avhrr::avhrr_pattern;
use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::raster::{self, Next};
use foxygnal_core::{Complex32, Flowgraph, Instance, StreamMeta};
use serde_json::json;

const SYMBOL_RATE: f64 = 3_500_000.0 * 2.0 / 3.0;

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

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

/// The transmitter's samples for `lines` AVHRR lines (no fill frames).
fn transmit(lines: u64, sps: usize) -> Vec<Complex32> {
    let mut fg = Flowgraph::new();
    let enc = fg.add(
        "enc",
        block("ahrpt_encoder", json!({ "lines": lines, "pace": "packed" })),
    );
    let m = fg.add("mod", block("ahrpt_modulator", json!({ "sps": sps })));
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(enc, 0, m, 0).unwrap();
    fg.connect(m, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    out.lock().unwrap().clone()
}

struct Channel {
    rate: f64,
    offset_hz: f64,
    clock_ppm: f64,
    delay: f64,
    esn0_db: f64,
    sps: f64,
    /// Samples replaced by noise alone (a fade): where (a fraction of the
    /// transmission) and how many.
    fade: Option<(f64, usize)>,
}

/// Through the channel: resampled for the clock error and delay (cubic),
/// shifted by the offset, noise for Es/N0 added; noise before and after.
fn channel(tx: &[Complex32], ch: &Channel, seed: u64) -> Vec<Complex32> {
    let mut rng = Rng(seed);
    // The shaped signal has unit power a sample, so Es = sps.
    let sigma = (ch.sps / (2.0 * 10f64.powf(ch.esn0_db / 10.0))).sqrt();
    let ratio = 1.0 + ch.clock_ppm * 1e-6;
    let at = |t: f64| -> Complex32 {
        let i = t.floor() as isize;
        let mu = (t - i as f64) as f32;
        let p = |k: isize| {
            tx.get((i + k) as usize)
                .copied()
                .filter(|_| i + k >= 0)
                .unwrap_or_default()
        };
        let (a, b, c) = (mu + 1.0, mu - 1.0, mu - 2.0);
        p(-1) * (-mu * b * c / 6.0)
            + p(0) * (a * b * c / 2.0)
            + p(1) * (-a * mu * c / 2.0)
            + p(2) * (a * mu * b / 6.0)
    };
    let lead = 50_000usize;
    let n = (tx.len() as f64 / ratio) as usize + 2 * lead;
    let fade = ch
        .fade
        .map(|(f, l)| (lead + (f * tx.len() as f64) as usize, l));
    (0..n)
        .map(|k| {
            let t = (k as f64 - lead as f64) * ratio - ch.delay;
            let faded = fade.is_some_and(|(s, l)| (s..s + l).contains(&k));
            let s = if t < 0.0 || faded {
                Complex32::default()
            } else {
                at(t)
            };
            let ph = TAU * ch.offset_hz * k as f64 / ch.rate;
            s * Complex32::from_polar(1.0, ph as f32)
                + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32)
        })
        .collect()
}

struct Decoded {
    /// Composite rows (row index, RGB pixels) of picture 1, and pictures.
    image: Vec<(usize, Vec<u8>)>,
    pictures: usize,
    names: Vec<String>,
    /// Channel 4's rows.
    ch4: Vec<(usize, Vec<u8>)>,
    other: Vec<u8>,
    text: String,
    status: String,
}

type Rows = Vec<(u32, usize, Vec<u8>)>;

fn rows(bytes: &[u8]) -> (Rows, usize, Vec<String>) {
    let mut out = Vec::new();
    let mut names = Vec::new();
    let mut at = 0;
    let mut pictures = std::collections::BTreeSet::new();
    loop {
        match raster::next(&bytes[at..]) {
            Next::Row(r, len) => {
                pictures.insert(r.picture);
                if !r.is_last() {
                    out.push((
                        r.picture,
                        usize::from(r.row),
                        bytes[at + raster::HEADER..at + len].to_vec(),
                    ));
                }
                at += len;
            }
            Next::Name(_, len) => {
                let (_, name) = raster::parse_name(&bytes[at..at + len]).unwrap();
                names.push(name.to_string());
                at += len;
            }
            Next::Skip(k) => at += k,
            Next::More => break,
        }
    }
    (out, pictures.len(), names)
}

fn receive(x: Vec<Complex32>, rate: f64) -> Decoded {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(x, StreamMeta::with_rate(rate)),
    );
    let demod = fg.add("demod", block("ahrpt_demodulator", json!({})));
    let packets = fg.add(
        "packets",
        block(
            "ccsds_packets",
            json!({ "insert_zone": 2, "format": "records" }),
        ),
    );
    let dec = fg.add("dec", block("avhrr_decoder", json!({})));
    let (s_img, img) = VectorSink::<u8>::instance();
    let (s_ch4, ch4) = VectorSink::<u8>::instance();
    let (s_txt, txt) = VectorSink::<u8>::instance();
    let (s_oth, oth) = VectorSink::<u8>::instance();
    let (s_st, st) = VectorSink::<u8>::instance();
    let s_img = fg.add("img", s_img);
    let s_ch4 = fg.add("ch4", s_ch4);
    let s_txt = fg.add("txt", s_txt);
    let s_oth = fg.add("oth", s_oth);
    let s_st = fg.add("st", s_st);
    fg.connect(src, 0, demod, 0).unwrap();
    fg.connect(demod, 0, packets, 0).unwrap();
    fg.connect(demod, 1, s_st, 0).unwrap();
    fg.connect(packets, 0, dec, 0).unwrap();
    fg.connect(dec, 0, s_img, 0).unwrap();
    fg.connect(dec, 4, s_ch4, 0).unwrap();
    fg.connect(dec, 6, s_txt, 0).unwrap();
    fg.connect(dec, 7, s_oth, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let (image, pictures, names) = rows(&img.lock().unwrap());
    let (ch4, _, _) = rows(&ch4.lock().unwrap());
    let first = |v: Rows| {
        v.into_iter()
            .filter(|r| r.0 == 1)
            .map(|r| (r.1, r.2))
            .collect()
    };
    let other = oth.lock().unwrap().clone();
    Decoded {
        image: first(image),
        pictures,
        names,
        ch4: first(ch4),
        other,
        text: String::from_utf8(txt.lock().unwrap().clone()).unwrap(),
        status: String::from_utf8(st.lock().unwrap().clone()).unwrap(),
    }
}

/// The pattern's 2-2-1 composite of line `y`.
fn want_row(y: usize) -> Vec<u8> {
    let top = |c: usize, x: usize| (avhrr_pattern(c, x, y) >> 2) as u8;
    (0..2048)
        .flat_map(|x| [top(1, x), top(1, x), top(0, x)])
        .collect()
}

/// The line the picture starts at (the receiver misses the first frames
/// while it locks).
fn first_line(image: &[(usize, Vec<u8>)], lines: usize) -> usize {
    let (row, px) = image
        .iter()
        .find(|(_, px)| px.iter().any(|&v| v != 0))
        .expect("a row that isn't black");
    (0..lines)
        .find(|&l| l >= *row && want_row(l)[..] == px[..])
        .map(|l| l - row)
        .expect("a row that matches a line sent")
}

/// The picture's first line, rows equal to the lines sent, rows that are
/// black; panics on any other.
fn check(d: &Decoded, lines: usize) -> (usize, usize, usize) {
    let line0 = first_line(&d.image, lines);
    let (mut good, mut black) = (0, 0);
    for (row, px) in &d.image {
        if px.iter().all(|&v| v == 0) {
            black += 1;
            continue;
        }
        assert!(
            want_row(line0 + row)[..] == px[..],
            "row {row} (line {}) differs\n{}",
            line0 + row,
            d.text
        );
        good += 1;
    }
    (line0, good, black)
}

#[test]
fn through_a_noisy_channel_with_doppler() {
    let sps = 2.0;
    let lines = 16;
    let tx = transmit(lines, 2);
    let ch = Channel {
        rate: SYMBOL_RATE * sps,
        offset_hz: 37_000.0,
        clock_ppm: 40.0,
        delay: 0.7,
        esn0_db: 8.0,
        sps,
        fade: None,
    };
    let d = receive(channel(&tx, &ch, 1), ch.rate);
    assert!(d.status.contains("event=lock"), "{}", d.status);
    assert_eq!(d.pictures, 1, "{}\n{}", d.text, d.status);
    let (line0, good, black) = check(&d, lines as usize);
    assert!(line0 <= 2, "{line0}\n{}", d.status);
    assert_eq!(black, 0, "{}", d.text);
    assert!(
        good + line0 >= lines as usize - 1,
        "{good} rows\n{}\n{}",
        d.text,
        d.status
    );
    assert_eq!(d.ch4.len(), d.image.len());
    // Named from the time stamp: day 9000 is 2024-08-22, 10:00:00 UTC.
    assert!(
        d.names
            .iter()
            .any(|n| n.starts_with("METOP-B/2024-08-22/AVHRR_rgb221_20240822T1000")),
        "{:?}",
        d.names
    );
    assert!(
        d.text.contains("event=start picture=1 satellite=METOP-B"),
        "{}",
        d.text
    );
    assert!(d.text.contains("ch3=3a missing=0"), "{}", d.text);
    // The MHS packet goes out of `other` as a record.
    assert!(d.other.starts_with(b"FXSP"), "{} bytes", d.other.len());
}

#[test]
fn a_dropout_leaves_black_rows_in_one_picture() {
    let sps = 2.0;
    let lines = 24;
    let tx = transmit(lines, 2);
    let rate = SYMBOL_RATE * sps;
    // An eighth of the (packed) link lost in the middle: three lines.
    let ch = Channel {
        rate,
        offset_hz: -21_000.0,
        clock_ppm: -30.0,
        delay: 0.3,
        esn0_db: 9.0,
        sps,
        fade: Some((0.5, tx.len() / 8)),
    };
    let d = receive(channel(&tx, &ch, 2), rate);
    assert_eq!(d.pictures, 1, "{}\n{}", d.text, d.status);
    let (line0, good, black) = check(&d, lines as usize);
    assert!(black >= 2, "{black} black rows\n{}\n{}", d.text, d.status);
    assert!(good >= 14, "{good} rows from line {line0}\n{}", d.text);
    let last = d.image.last().unwrap().0;
    assert!(
        line0 + last + 2 >= lines as usize,
        "last row {last} from line {line0}"
    );
}
