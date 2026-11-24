//! Meteor-M LRPT end to end: `lrpt_encoder` (a synthetic MSU-MR picture,
//! JPEG-coded into packets and frames) → `lrpt_modulator` → a channel
//! (carrier offset, symbol clock error, fractional timing offset, noise,
//! a dropout) → `lrpt_demodulator` → `ccsds_packets` → `lrpt_decoder`, in
//! every mode, and the decoded picture compared with the one sent.

use std::f64::consts::TAU;

use foxygnal_blocks::polar::lrpt::test_pattern;
use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::raster::{self, Next};
use foxygnal_core::{Complex32, Flowgraph, Instance, StreamMeta};
use serde_json::json;

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

/// The transmitter's samples for `scans` scans.
fn transmit(mode: &str, channels: &str, scans: u64, sps: usize) -> Vec<Complex32> {
    let mut fg = Flowgraph::new();
    let enc = fg.add(
        "enc",
        block(
            "lrpt_encoder",
            json!({ "channels": channels, "quality": 80, "scans": scans }),
        ),
    );
    let m = fg.add(
        "mod",
        block("lrpt_modulator", json!({ "mode": mode, "sps": sps })),
    );
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
    let lead = 20_000usize;
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
    /// Composite rows (row index, RGB pixels) of picture 1, and pictures seen.
    image: Vec<(usize, Vec<u8>)>,
    pictures: usize,
    /// Channel 1's rows.
    ch1: Vec<(usize, Vec<u8>)>,
    text: String,
    status: String,
}

fn rows(bytes: &[u8]) -> (Vec<(u32, usize, Vec<u8>)>, usize) {
    let mut out = Vec::new();
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
                assert!(name.starts_with("METEOR-M/"), "{name}");
                at += len;
            }
            Next::Skip(k) => at += k,
            Next::More => break,
        }
    }
    (out, pictures.len())
}

fn receive(x: Vec<Complex32>, rate: f64, mode: &str) -> Decoded {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(x, StreamMeta::with_rate(rate)),
    );
    let demod = fg.add("demod", block("lrpt_demodulator", json!({ "mode": mode })));
    let packets = fg.add(
        "packets",
        block(
            "ccsds_packets",
            json!({ "insert_zone": 2, "format": "records" }),
        ),
    );
    let dec = fg.add("dec", block("lrpt_decoder", json!({})));
    let (s_img, img) = VectorSink::<u8>::instance();
    let (s_ch1, ch1) = VectorSink::<u8>::instance();
    let (s_txt, txt) = VectorSink::<u8>::instance();
    let (s_st, st) = VectorSink::<u8>::instance();
    let s_img = fg.add("img", s_img);
    let s_ch1 = fg.add("ch1", s_ch1);
    let s_txt = fg.add("txt", s_txt);
    let s_st = fg.add("st", s_st);
    fg.connect(src, 0, demod, 0).unwrap();
    fg.connect(demod, 0, packets, 0).unwrap();
    fg.connect(demod, 1, s_st, 0).unwrap();
    fg.connect(packets, 0, dec, 0).unwrap();
    fg.connect(dec, 0, s_img, 0).unwrap();
    fg.connect(dec, 1, s_ch1, 0).unwrap();
    fg.connect(dec, 7, s_txt, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let (image, pictures) = rows(&img.lock().unwrap());
    let (ch1, _) = rows(&ch1.lock().unwrap());
    let first = |v: Vec<(u32, usize, Vec<u8>)>| {
        v.into_iter()
            .filter(|r| r.0 == 1)
            .map(|r| (r.1, r.2))
            .collect()
    };
    Decoded {
        image: first(image),
        pictures,
        ch1: first(ch1),
        text: String::from_utf8(txt.lock().unwrap().clone()).unwrap(),
        status: String::from_utf8(st.lock().unwrap().clone()).unwrap(),
    }
}

/// The scan the picture starts at (the receiver misses the first frames
/// while it locks): the offset that matches the first composite row with
/// green (channel 2) best.
fn first_scan(image: &[(usize, Vec<u8>)], scans: usize) -> usize {
    let (row, px) = image
        .iter()
        .find(|(_, px)| px.chunks(3).any(|p| p[1] != 0))
        .expect("a row with channel 2");
    (0..scans)
        .min_by_key(|&s| {
            px.chunks(3)
                .enumerate()
                .filter(|(_, p)| p[1] != 0)
                .map(|(x, p)| (i32::from(p[1]) - i32::from(test_pattern(1, x, s * 8 + row))).abs())
                .sum::<i32>()
        })
        .unwrap()
}

/// Mean absolute error of composite row `row` (picture rows from `scan0`)
/// against the 2-2-1 composite of the pattern, over the pixels that came
/// (the pattern is never 0; missing packets are); None if none did (a gap).
fn row_error(px: &[u8], row: usize, scan0: usize) -> Option<f64> {
    let y = scan0 * 8 + row;
    let (mut e, mut n) = (0.0, 0);
    for x in 0..1568 {
        let want = [
            test_pattern(1, x, y),
            test_pattern(1, x, y),
            test_pattern(0, x, y),
        ];
        for k in 0..3 {
            if px[3 * x + k] != 0 {
                e += (f64::from(px[3 * x + k]) - f64::from(want[k])).abs();
                n += 1;
            }
        }
    }
    (n > 0).then(|| e / f64::from(n))
}

fn run(mode: &str, scans: u64, ch: Channel, seed: u64) -> (Decoded, usize, usize) {
    let tx = transmit(mode, "1,2,3", scans, ch.sps as usize);
    let rate = ch.rate;
    let rx = channel(&tx, &ch, seed);
    let d = receive(rx, rate, mode);
    assert!(
        !d.ch1.is_empty(),
        "nothing decoded\n{}\n{}",
        d.status,
        d.text
    );
    let scan0 = first_scan(&d.image, scans as usize);
    let mut good = 0;
    for (row, px) in &d.image {
        if let Some(e) = row_error(px, *row, scan0) {
            assert!(e < 5.0, "row {row}: mean error {e}\n{}", d.text);
            good += 1;
        }
    }
    (d, scan0, good)
}

#[test]
fn oqpsk72_through_a_noisy_channel() {
    let sps = 4.0;
    let (d, scan0, good) = run(
        "oqpsk72",
        8,
        Channel {
            rate: 72_000.0 * sps,
            offset_hz: 4_200.0,
            clock_ppm: 60.0,
            delay: 1.3,
            esn0_db: 9.0,
            sps,
            fade: None,
        },
        1,
    );
    assert!(d.status.contains("event=lock mode=oqpsk72"), "{}", d.status);
    assert_eq!(d.pictures, 1, "{}", d.text);
    // Locking costs the first scan or two.
    assert!(scan0 <= 2, "{scan0}");
    assert!(good >= (8 - scan0 - 2) * 8, "{good} rows\n{}", d.text);
    assert!(d.text.contains("event=start picture=1"), "{}", d.text);
    assert!(d.text.contains("channels=1,2,3 missing=0"), "{}", d.text);
    assert!(d.text.contains("event=end picture=1"), "{}", d.text);
}

#[test]
fn qpsk72_with_a_dropout_keeps_one_picture() {
    let sps = 4.0;
    let rate = 72_000.0 * sps;
    // A 3 s fade in the middle: the rows of those scans are black, the
    // picture goes on.
    let (d, scan0, good) = run(
        "qpsk72",
        12,
        Channel {
            rate,
            offset_hz: -2_500.0,
            clock_ppm: -40.0,
            delay: 0.6,
            esn0_db: 9.0,
            sps,
            fade: Some((0.45, (3.0 * rate) as usize)),
        },
        2,
    );
    assert_eq!(d.pictures, 1, "{}", d.text);
    let gaps = d
        .image
        .iter()
        .filter(|(_, px)| px.iter().all(|&v| v == 0))
        .count();
    assert!(gaps >= 16, "{gaps} black rows\n{}\n{}", d.text, d.status);
    assert!(good >= 5 * 8, "{good} rows (from scan {scan0})\n{}", d.text);
    // Rows keep their place after the gap: they matched the pattern above.
    let last = d.image.last().unwrap().0;
    assert!(last + 1 >= (12 - scan0 - 1) * 8, "last row {last}");
}

#[test]
fn oqpsk80_interleaved() {
    // The deinterleaver holds 16 s, so the first 13 scans fill it.
    let sps = 2.0;
    let (d, scan0, good) = run(
        "oqpsk80",
        20,
        Channel {
            rate: 80_000.0 * sps,
            offset_hz: 6_000.0,
            clock_ppm: 30.0,
            delay: 0.4,
            esn0_db: 10.0,
            sps,
            fade: None,
        },
        3,
    );
    assert!(d.status.contains("marker=lock"), "{}", d.status);
    assert_eq!(d.pictures, 1, "{}", d.text);
    assert!(good >= 3 * 8, "{good} rows from scan {scan0}\n{}", d.text);
}
