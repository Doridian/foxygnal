//! SSTV end to end: a small independent encoder (tone list per mode family,
//! written from the mode specifications rather than the decoder's tables)
//! makes audio that goes through `sstv_decoder`; the decoded rows are
//! compared with the picture that went in.

use std::f64::consts::TAU;

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::raster::{self, Next};
use foxygnal_core::{BufferKind, Flowgraph, Instance, StreamMeta};
use serde_json::json;

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

// ---- Encoder ------------------------------------------------------------------------

type Rgb = [u8; 3];

/// A smooth test picture: red across, green down, blue the other diagonal.
fn picture(w: usize, h: usize) -> Vec<Vec<Rgb>> {
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| {
                    let r = 255.0 * x as f64 / (w - 1) as f64;
                    let g = 255.0 * y as f64 / (h - 1) as f64;
                    let b = 255.0 - 0.5 * (r + g);
                    [r as u8, g as u8, b as u8]
                })
                .collect()
        })
        .collect()
}

fn ycc(p: Rgb) -> [f64; 3] {
    let [r, g, b] = p.map(f64::from);
    [
        0.299 * r + 0.587 * g + 0.114 * b,
        128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b,
        128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b,
    ]
}

fn freq(v: f64) -> f64 {
    1500.0 + v * 800.0 / 255.0
}

/// (frequency Hz, duration ms) tones.
#[derive(Default)]
struct Tones(Vec<(f64, f64)>);

impl Tones {
    fn tone(&mut self, f: f64, ms: f64) {
        self.0.push((f, ms));
    }

    /// One scan: `values.len()` pixels in `ms`.
    fn scan(&mut self, values: impl ExactSizeIterator<Item = f64>, ms: f64) {
        let px = ms / values.len() as f64;
        for v in values {
            self.tone(freq(v), px);
        }
    }

    fn vis(&mut self, code: u8) {
        self.tone(1900.0, 300.0);
        self.tone(1200.0, 10.0);
        self.tone(1900.0, 300.0);
        self.tone(1200.0, 30.0);
        let mut ones = 0;
        for i in 0..7 {
            let bit = (code >> i) & 1;
            ones += bit;
            self.tone(if bit == 1 { 1100.0 } else { 1300.0 }, 30.0);
        }
        self.tone(if ones % 2 == 1 { 1100.0 } else { 1300.0 }, 30.0);
        self.tone(1200.0, 30.0);
    }

    /// Audio at `rate`, with the sender's clock `ppm` slow (every tone that
    /// much longer), phase-continuous.
    fn audio(&self, rate: f64, ppm: f64, amp: f32) -> Vec<f32> {
        let stretch = 1.0 + ppm * 1e-6;
        let total: f64 = self.0.iter().map(|t| t.1).sum::<f64>() * stretch;
        let n = (total / 1000.0 * rate) as usize;
        let mut out = Vec::with_capacity(n);
        let (mut phase, mut seg, mut seg_end) = (0.0f64, 0, self.0[0].1 * stretch);
        for i in 0..n {
            let t = i as f64 / rate * 1000.0;
            while t >= seg_end && seg + 1 < self.0.len() {
                seg += 1;
                seg_end += self.0[seg].1 * stretch;
            }
            phase += TAU * self.0[seg].0 / rate;
            out.push(amp * phase.sin() as f32);
        }
        out
    }
}

fn chan(row: &[Rgb], c: usize) -> impl ExactSizeIterator<Item = f64> + '_ {
    row.iter().map(move |p| f64::from(p[c]))
}

fn luma(row: &[Rgb]) -> impl ExactSizeIterator<Item = f64> + '_ {
    row.iter().map(|p| ycc(*p)[0])
}

/// Encode the first `lines` lines of `pic` in mode `id`.
fn encode(id: &str, pic: &[Vec<Rgb>], lines: usize) -> Tones {
    let mut t = Tones::default();
    t.tone(1500.0, 0.0001);
    let martin = |t: &mut Tones, vis, scan: f64| {
        t.vis(vis);
        for row in &pic[..lines] {
            t.tone(1200.0, 4.862);
            t.tone(1500.0, 0.572);
            for c in [1, 2, 0] {
                t.scan(chan(row, c), scan);
                t.tone(1500.0, 0.572);
            }
        }
    };
    let scottie = |t: &mut Tones, vis, scan: f64| {
        t.vis(vis);
        t.tone(1200.0, 9.0);
        for row in &pic[..lines] {
            t.tone(1500.0, 1.5);
            t.scan(chan(row, 1), scan);
            t.tone(1500.0, 1.5);
            t.scan(chan(row, 2), scan);
            t.tone(1200.0, 9.0);
            t.tone(1500.0, 1.5);
            t.scan(chan(row, 0), scan);
        }
    };
    let pd = |t: &mut Tones, vis, scan: f64| {
        t.vis(vis);
        for pair in pic[..2 * lines].chunks(2) {
            t.tone(1200.0, 20.0);
            t.tone(1500.0, 2.08);
            t.scan(luma(&pair[0]), scan);
            let avg = |c: usize| {
                (0..pair[0].len())
                    .map(move |x| (ycc(pair[0][x])[c] + ycc(pair[1][x])[c]) / 2.0)
                    .collect::<Vec<_>>()
            };
            t.scan(avg(1).into_iter(), scan);
            t.scan(avg(2).into_iter(), scan);
            t.scan(luma(&pair[1]), scan);
        }
    };
    match id {
        "martin1" => martin(&mut t, 44, 146.432),
        "martin2" => martin(&mut t, 40, 73.216),
        "scottie1" => scottie(&mut t, 60, 138.24),
        "scottie2" => scottie(&mut t, 56, 88.064),
        "scottie_dx" => scottie(&mut t, 76, 345.6),
        "pd50" => pd(&mut t, 93, 91.52),
        "pd90" => pd(&mut t, 99, 170.24),
        "pd120" => pd(&mut t, 95, 121.6),
        "pd160" => pd(&mut t, 98, 195.584),
        "pd180" => pd(&mut t, 96, 183.04),
        "pd240" => pd(&mut t, 97, 244.48),
        "pd290" => pd(&mut t, 94, 228.8),
        "wraase_sc2_180" => {
            t.vis(55);
            for row in &pic[..lines] {
                t.tone(1200.0, 5.5225);
                t.tone(1500.0, 0.5);
                for c in 0..3 {
                    t.scan(chan(row, c), 235.0);
                }
            }
        }
        "robot36" => {
            t.vis(8);
            for (y, row) in pic[..lines].iter().enumerate() {
                t.tone(1200.0, 9.0);
                t.tone(1500.0, 3.0);
                t.scan(luma(row), 88.0);
                let even = y % 2 == 0;
                t.tone(if even { 1500.0 } else { 2300.0 }, 4.5);
                t.tone(1900.0, 1.5);
                let c = if even { 1 } else { 2 };
                t.scan(row.iter().map(|p| ycc(*p)[c]), 44.0);
            }
        }
        "robot72" => {
            t.vis(12);
            for row in &pic[..lines] {
                t.tone(1200.0, 9.0);
                t.tone(1500.0, 3.0);
                t.scan(luma(row), 138.0);
                t.tone(1500.0, 4.5);
                t.tone(1900.0, 1.5);
                t.scan(row.iter().map(|p| ycc(*p)[1]), 69.0);
                t.tone(2300.0, 4.5);
                t.tone(1500.0, 1.5);
                t.scan(row.iter().map(|p| ycc(*p)[2]), 69.0);
            }
        }
        _ => panic!("no encoder for {id}"),
    }
    t
}

// ---- Running the decoder ------------------------------------------------------------

struct Decoded {
    /// (picture, row, last, width, pixels)
    rows: Vec<(u32, u16, bool, u16, Vec<u8>)>,
    text: String,
}

fn decode(audio: Vec<f32>, rate: f64, params: serde_json::Value, small: bool) -> Decoded {
    let mut fg = if small {
        Flowgraph::new()
            .with_buffer_capacity(64)
            .with_buffer_kind(BufferKind::Plain)
    } else {
        Flowgraph::new()
    };
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(audio, StreamMeta::with_rate(rate)),
    );
    let dec = fg.add("sstv", block("sstv_decoder", params));
    let (isink, image) = VectorSink::<u8>::instance();
    let (tsink, text) = VectorSink::<u8>::instance();
    let isink = fg.add("image", isink);
    let tsink = fg.add("text", tsink);
    fg.connect(src, 0, dec, 0).unwrap();
    fg.connect(dec, 0, isink, 0).unwrap();
    fg.connect(dec, 1, tsink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let bytes = image.lock().unwrap().clone();
    let mut rows = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match raster::next(&bytes[at..]) {
            Next::Row(r, len) => {
                rows.push((
                    r.picture,
                    r.row,
                    r.is_last(),
                    r.width,
                    bytes[at + raster::HEADER..at + len].to_vec(),
                ));
                at += len;
            }
            other => panic!("not a record at {at}: {other:?}"),
        }
    }
    let text = String::from_utf8(text.lock().unwrap().clone()).unwrap();
    eprint!("{text}");
    Decoded { rows, text }
}

/// Mean absolute error per channel of the decoded rows against `pic`,
/// leaving out a few pixels at each edge (the filters smear the jumps there).
fn error(d: &Decoded, pic: &[Vec<Rgb>]) -> f64 {
    error_rows(d, pic, usize::MAX)
}

/// [`error`] over rows before `rows` only.
fn error_rows(d: &Decoded, pic: &[Vec<Rgb>], rows: usize) -> f64 {
    let (mut sum, mut n) = (0.0, 0.0);
    for (_, row, _, w, px) in d.rows.iter().filter(|r| usize::from(r.1) < rows) {
        let want = &pic[usize::from(*row)];
        let w = usize::from(*w);
        for x in 4..w - 4 {
            for c in 0..3 {
                sum += (f64::from(px[3 * x + c]) - f64::from(want[x][c])).abs();
                n += 1.0;
            }
        }
    }
    sum / n
}

fn mode(id: &str) -> &'static foxygnal_blocks::image::SstvMode {
    foxygnal_blocks::image::SSTV_MODES
        .iter()
        .find(|m| m.id == id)
        .unwrap()
}

fn noise(audio: &mut [f32], amp: f32, seed: u64) {
    let mut s = seed;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
    };
    for x in audio {
        // Sum of four uniforms: roughly Gaussian.
        *x += amp * (rnd() + rnd() + rnd() + rnd());
    }
}

// ---- Tests --------------------------------------------------------------------------

#[test]
fn every_mode_decodes_its_first_lines() {
    for m in foxygnal_blocks::image::SSTV_MODES {
        let pic = picture(usize::from(m.width), usize::from(m.height));
        // A dozen lines are enough to check timing, colour and the VIS code.
        let lines = 12.min(usize::from(m.lines));
        let mut audio = vec![0.0; 4000];
        audio.extend(encode(m.id, &pic, lines).audio(11_025.0, 0.0, 0.5));
        let d = decode(audio, 11_025.0, json!({}), false);
        assert!(
            d.text
                .contains(&format!("event=start mode=\"{}\" vis={}", m.name, m.vis)),
            "{}: {}",
            m.id,
            d.text
        );
        assert!(d.text.contains("event=cut"), "{}: {}", m.id, d.text);
        let rows_per_line = usize::from(m.height / m.lines);
        assert!(
            d.rows.len() >= (lines - 1) * rows_per_line,
            "{}: {} rows",
            m.id,
            d.rows.len()
        );
        assert!(
            d.rows.last().unwrap().2,
            "{}: the last row is flagged",
            m.id
        );
        let e = error(&d, &pic);
        eprintln!("{}: mean error {e:.2}", m.id);
        assert!(e < 6.0, "{}: mean error {e:.2}", m.id);
    }
}

#[test]
fn a_whole_picture_with_noise_and_a_slow_clock() {
    let m = mode("robot36");
    let pic = picture(320, 240);
    let mut audio = vec![0.0; 20_000];
    // The sender's sound card runs 400 ppm slow: without slant correction the
    // last line would start 14 ms late, a tenth of a line.
    audio.extend(encode(m.id, &pic, 240).audio(48_000.0, 400.0, 0.4));
    audio.extend(vec![0.0; 20_000]);
    // About 30 dB SNR in the 2 kHz the decoder listens to: the VIS header
    // and sync pulses are found easily; pixels get a little grainy.
    noise(&mut audio, 0.05, 7);
    let d = decode(audio, 48_000.0, json!({}), false);
    assert!(
        d.text.contains("event=end mode=\"Robot 36\" lines=240"),
        "{}",
        d.text
    );
    let clock: f64 = d
        .text
        .split("clock_ppm=")
        .nth(1)
        .and_then(|s| s.trim().parse().ok())
        .unwrap();
    assert!((clock - 400.0).abs() < 60.0, "clock {clock} ppm");
    assert_eq!(d.rows.len(), 240);
    assert!(
        d.rows
            .iter()
            .enumerate()
            .all(|(i, r)| usize::from(r.1) == i)
    );
    assert!(
        d.rows[239].2 && !d.rows[238].2,
        "only the last row is flagged"
    );
    let e = error(&d, &pic);
    assert!(e < 8.0, "mean error {e:.2}");
    // Without correction the slant shows.
    let mut audio = encode(m.id, &pic, 240).audio(11_025.0, 400.0, 0.4);
    audio.extend(vec![0.0; 5000]);
    let fixed = decode(audio, 11_025.0, json!({"slant": false}), false);
    assert_eq!(fixed.rows.len(), 240);
    let ef = error(&fixed, &pic);
    assert!(ef > e + 2.0, "slant off: {ef:.2}, on: {e:.2}");
}

#[test]
fn two_pictures_in_a_row_with_tiny_buffers() {
    let pic = picture(320, 256);
    let mut tones = encode("martin2", &pic, 20);
    tones.tone(1500.0, 500.0);
    let more = encode("scottie2", &pic, 20);
    tones.0.extend(more.0);
    let audio = tones.audio(8_000.0, 0.0, 0.5);
    let d = decode(audio, 8_000.0, json!({}), true);
    assert!(d.text.contains("mode=\"Martin M2\""), "{}", d.text);
    assert!(
        d.text.contains("event=lost mode=\"Martin M2\""),
        "{}",
        d.text
    );
    assert!(
        d.text.contains("event=start mode=\"Scottie S2\""),
        "{}",
        d.text
    );
    let pictures: Vec<u32> = d.rows.iter().map(|r| r.0).collect();
    assert_eq!(pictures.first(), Some(&1));
    assert_eq!(pictures.last(), Some(&2));
    // The first picture's last lines are decoded from the second one's VIS
    // header (until that is recognised); leave those out.
    assert!(error_rows(&d, &pic, 20) < 6.0);
}

#[test]
fn a_forced_mode_ignores_the_vis_code() {
    let pic = picture(320, 256);
    // Martin M1 timing, but the header says Martin M2.
    let mut t = encode("martin1", &pic, 6);
    // VIS starts after the 0.0001 ms lead-in: data bits are tones 5..12.
    for (i, bit) in [0, 0, 0, 1, 0, 1, 0].iter().enumerate() {
        t.0[5 + i].0 = if *bit == 1 { 1100.0 } else { 1300.0 };
    }
    t.0[12].0 = 1300.0; // parity of 40 (two ones): even
    let audio = t.audio(11_025.0, 0.0, 0.5);
    let auto = decode(audio.clone(), 11_025.0, json!({}), false);
    assert!(auto.text.contains("mode=\"Martin M2\""), "{}", auto.text);
    let forced = decode(audio, 11_025.0, json!({"mode": "martin1"}), false);
    assert!(
        forced.text.contains("mode=\"Martin M1\""),
        "{}",
        forced.text
    );
    assert!(error(&forced, &pic) < 6.0);
}

#[test]
fn image_sink_saves_each_picture_as_png() {
    let dir = std::env::temp_dir().join(format!("foxygnal-sstv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pic = picture(320, 256);
    let mut tones = encode("pd50", &pic, 128);
    tones.tone(1500.0, 300.0);
    tones.0.extend(encode("martin2", &pic, 30).0);
    let audio = tones.audio(11_025.0, 0.0, 0.5);

    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(audio, StreamMeta::with_rate(11_025.0)),
    );
    let dec = fg.add("sstv", block("sstv_decoder", json!({})));
    let sink = fg.add(
        "save",
        block(
            "image_sink",
            json!({"dir": dir.to_str().unwrap(), "prefix": "sstv", "min_rows": 16}),
        ),
    );
    fg.connect(src, 0, dec, 0).unwrap();
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2, "{files:?}");
    for (path, rows) in files.iter().zip([256, 30]) {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("sstv-") && name.ends_with(".png"),
            "{name}"
        );
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (320, 256), "{name}");
        assert_eq!(info.color_type, png::ColorType::Rgb);
        // The rows that were sent match the picture; the rest are black.
        let (mut sum, mut n) = (0.0, 0.0);
        for y in 0..rows {
            for x in 4..316 {
                for c in 0..3 {
                    let got = f64::from(buf[(y * 320 + x) * 3 + c]);
                    sum += (got - f64::from(pic[y][x][c])).abs();
                    n += 1.0;
                }
            }
        }
        assert!(sum / n < 6.0, "{name}: mean error {:.2}", sum / n);
        if rows < 256 {
            assert!(buf[(255 * 320) * 3..].iter().all(|&v| v == 0));
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn robot36_right_edge_has_no_colour_fringe() {
    // The chroma scan ends right before the next line's sync pulse; the
    // decoder's filter must not smear that into the last pixels (a green
    // stripe down the right edge).
    let w = 320;
    let pic: Vec<Vec<Rgb>> = (0..240).map(|_| vec![[0, 0, 0]; w]).collect();
    let audio = encode("robot36", &pic, 40).audio(11_025.0, 0.0, 0.5);
    let d = decode(audio, 11_025.0, json!({}), false);
    for (_, row, _, _, px) in &d.rows {
        for x in w - 3..w {
            let g = px[3 * x + 1];
            assert!(g < 24, "row {row} column {x}: green {g}");
        }
    }
}
