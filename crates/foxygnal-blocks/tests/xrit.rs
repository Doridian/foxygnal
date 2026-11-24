//! GOES HRIT end to end: a synthetic broadcast (`xrit_source`: segmented,
//! Rice-coded full disks, a mesoscale picture, EMWIN bulletins, an admin
//! message) → packets → CADUs → `ccsds_modulator` (BPSK, NRZ-M, the code
//! without G2 inversion, as GOES sends it) → a channel at an RTL-SDR's 2.4
//! MS/s (RRC pulses, a frequency offset, noise) → the receive chain of the
//! "GOES HRIT" template → `xrit_decoder` → pictures, messages and files,
//! compared with what was sent.
//!
//! `costas_sweep` (ignored) shows how the carrier loop's bandwidth copes
//! with tuning error and noise: `cargo test --release -p foxygnal-blocks
//! --test xrit -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};
use std::path::{Path, PathBuf};

use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_blocks::xrit::picture::false_colour;
use foxygnal_blocks::xrit::synth::{self, Synth};
use foxygnal_core::raster::{self, Next};
use foxygnal_core::{Complex32, Flowgraph, Instance, StreamMeta};
use serde_json::json;

const SYMBOL_RATE: f64 = 927_000.0;
const SAMPLE_RATE: f64 = 2_400_000.0;
const WIDTH: u16 = 192;
const SEGMENTS: u16 = 3;

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

/// The source's CADUs (`rounds` rounds), or its symbols through the
/// modulator.
fn transmit(rounds: u32, modulate: bool) -> (Vec<u8>, Vec<Complex32>) {
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        block(
            "xrit_source",
            json!({ "width": WIDTH, "segments": SEGMENTS, "rounds": rounds }),
        ),
    );
    if !modulate {
        let (sink, out) = VectorSink::<u8>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(src, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        return (out.lock().unwrap().clone(), Vec::new());
    }
    let m = fg.add(
        "mod",
        block(
            "ccsds_modulator",
            json!({ "modulation": "bpsk", "code": "conv_noninverted", "nrzm": true, "symbol_rate": SYMBOL_RATE }),
        ),
    );
    let (sink, out) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, m, 0).unwrap();
    fg.connect(m, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    (Vec::new(), out.lock().unwrap().clone())
}

/// The root-raised-cosine pulse (symbol period 1) at `t`.
fn rrc(t: f64, beta: f64) -> f64 {
    if t.abs() < 1e-9 {
        return 1.0 - beta + 4.0 * beta / PI;
    }
    if (t.abs() - 1.0 / (4.0 * beta)).abs() < 1e-9 {
        let a = PI / (4.0 * beta);
        return beta / 2f64.sqrt() * ((1.0 + 2.0 / PI) * a.sin() + (1.0 - 2.0 / PI) * a.cos());
    }
    let num = (PI * t * (1.0 - beta)).sin() + 4.0 * beta * t * (PI * t * (1.0 + beta)).cos();
    num / (PI * t * (1.0 - (4.0 * beta * t).powi(2)))
}

/// What an SDR at 2.4 MS/s would see: the symbols as RRC pulses (roll-off
/// 0.3, as GOES sends), `offset` Hz off, at Eb/N0 `ebn0_db` per data bit
/// (unit power per sample: Es = sps, and one channel bit is half a data
/// bit), with noise before and after.
fn channel(symbols: &[Complex32], offset: f64, ebn0_db: f64, seed: u64) -> Vec<Complex32> {
    let sps = SAMPLE_RATE / SYMBOL_RATE;
    let span = 8.0;
    let lead = 3000;
    let n = lead * 2 + (symbols.len() as f64 * sps) as usize;
    let mut x = vec![Complex32::new(0.0, 0.0); n];
    for (k, &s) in symbols.iter().enumerate() {
        let centre = lead as f64 + k as f64 * sps;
        let lo = (centre - span * sps).ceil().max(0.0) as usize;
        let hi = ((centre + span * sps).floor() as usize).min(n - 1);
        for (i, xi) in x.iter_mut().enumerate().take(hi + 1).skip(lo) {
            *xi += s * rrc((i as f64 - centre) / sps, 0.3) as f32;
        }
    }
    let power = x.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / (n - 2 * lead) as f64;
    let scale = (1.0 / power).sqrt() as f32;
    let ebn0 = 10f64.powf(ebn0_db / 10.0);
    // Eb = 2 Es = 2 sps (unit power); N0 = 2σ².
    let sigma = (sps / ebn0).sqrt();
    let mut rng = Rng(seed);
    x.iter()
        .enumerate()
        .map(|(i, &s)| {
            let ph = 0.7 + TAU * offset * i as f64 / SAMPLE_RATE;
            s * scale * Complex32::from_polar(1.0, ph as f32)
                + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32)
        })
        .collect()
}

struct Received {
    text: String,
    /// Pictures by number: (name, width, height, channels, pixels, rows).
    pictures: BTreeMap<u32, Picture>,
    status: String,
}

#[derive(Default)]
struct Picture {
    name: String,
    width: usize,
    height: usize,
    channels: usize,
    pixels: Vec<u8>,
    rows: usize,
    last: bool,
}

fn pictures(bytes: &[u8]) -> BTreeMap<u32, Picture> {
    let mut out: BTreeMap<u32, Picture> = BTreeMap::new();
    let mut at = 0;
    while at < bytes.len() {
        match raster::next(&bytes[at..]) {
            Next::Name(n, len) => {
                let name = raster::parse_name(&bytes[at..at + len])
                    .unwrap()
                    .1
                    .to_owned();
                out.entry(n).or_default().name = name;
                at += len;
            }
            Next::Row(r, len) => {
                let p = out.entry(r.picture).or_default();
                let (w, h, c) = (
                    usize::from(r.width),
                    usize::from(r.height),
                    usize::from(r.channels),
                );
                if p.pixels.is_empty() {
                    (p.width, p.height, p.channels) = (w, h, c);
                    p.pixels = vec![0; w * h * c];
                }
                let px = &bytes[at + raster::HEADER..at + len];
                let row = usize::from(r.row);
                p.pixels[row * w * c..(row + 1) * w * c].copy_from_slice(px);
                p.rows += 1;
                p.last |= r.is_last();
                at += len;
            }
            other => panic!("not a record at {at}: {other:?}"),
        }
    }
    out
}

/// CADUs straight into the decoder, or (`cadus` None) the channel's
/// samples through the template's demodulator.
fn receive(
    cadus: Option<Vec<u8>>,
    samples: Vec<Complex32>,
    dir: &Path,
    costas_bw: f64,
) -> Received {
    let mut fg = Flowgraph::new();
    let dec = fg.add(
        "dec",
        block(
            "xrit_decoder",
            json!({ "dir": dir.to_string_lossy(), "image_rate": 0 }),
        ),
    );
    let mut status = None;
    match cadus {
        Some(c) => {
            let src = fg.add("src", VectorSource::instance(c));
            fg.connect(src, 0, dec, 0).unwrap();
        }
        None => {
            let (ssink, st) = VectorSink::<u8>::instance();
            let ssink = fg.add("status", ssink);
            status = Some(st);
            let sps = SAMPLE_RATE / SYMBOL_RATE;
            let src = fg.add(
                "src",
                VectorSource::instance_with_meta(samples, StreamMeta::with_rate(SAMPLE_RATE)),
            );
            let chain = [
                ("agc", json!({ "type": "c32" })),
                (
                    "rrc_filter",
                    json!({ "sps": sps, "rolloff": 0.3, "span": 31 }),
                ),
                ("clock_recovery", json!({ "sps": sps, "loop_bw": 0.005 })),
                ("costas_loop", json!({ "order": "2", "loop_bw": costas_bw })),
            ];
            let mut prev = src;
            for (i, (kind, p)) in chain.into_iter().enumerate() {
                let b = fg.add(format!("b{i}"), block(kind, p));
                fg.connect(prev, 0, b, 0).unwrap();
                prev = b;
            }
            let d = fg.add(
                "deframer",
                block(
                    "ccsds_deframer",
                    json!({ "modulation": "bpsk", "code": "conv_noninverted", "nrzm": true }),
                ),
            );
            fg.connect(prev, 0, d, 0).unwrap();
            fg.connect(d, 0, dec, 0).unwrap();
            fg.connect(d, 1, ssink, 0).unwrap();
        }
    }
    let (tsink, text) = VectorSink::<u8>::instance();
    let (isink, image) = VectorSink::<u8>::instance();
    let tsink = fg.add("text", tsink);
    let isink = fg.add("image", isink);
    fg.connect(dec, 0, tsink, 0).unwrap();
    fg.connect(dec, 1, isink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let text = String::from_utf8(text.lock().unwrap().clone()).unwrap();
    let status = status.map_or_else(String::new, |s| {
        String::from_utf8(s.lock().unwrap().clone()).unwrap()
    });
    Received {
        text,
        pictures: pictures(&image.lock().unwrap()),
        status,
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("foxygnal-xrit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Everything of `rounds` arrived and matches what was sent.
fn check(r: &Received, rounds: std::ops::Range<u32>, dir: &Path) {
    let s = Synth {
        width: WIDTH,
        segments: SEGMENTS,
        ..Synth::default()
    };
    let by_name: BTreeMap<&str, &Picture> =
        r.pictures.values().map(|p| (p.name.as_str(), p)).collect();
    for round in rounds.clone() {
        let t = s.time(round);
        let stamp = |t: f64, ch: &str, region: &str| {
            let (y, m, d, hh, mm, ss) = foxygnal_blocks::xrit::products::civil(t);
            format!(
                "G19/{region}/{ch}/{y:04}-{m:02}-{d:02}/G19_{region}_{ch}_{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"
            )
        };
        let ir = by_name
            .get(stamp(t, "CH13", "FD").as_str())
            .unwrap_or_else(|| panic!("no CH13 in {:?}\n{}", by_name.keys(), r.text));
        let vis = by_name[stamp(t, "CH02", "FD").as_str()];
        let meso = by_name[stamp(t + 30.0, "CH07", "M1").as_str()];
        let fc = by_name[stamp(t, "FC", "FD").as_str()];
        let (want_ir, want_vis) = (
            synth::shown(13, WIDTH, WIDTH, round),
            synth::shown(2, WIDTH, WIDTH, round),
        );
        assert_eq!((ir.width, ir.height, ir.channels), (192, 192, 1));
        assert!(ir.pixels == want_ir, "channel 13 differs");
        assert!(vis.pixels == want_vis, "channel 2 differs");
        assert!(ir.last && vis.last && meso.last && fc.last);
        assert_eq!(meso.width, 96);
        assert!(
            meso.pixels == synth::picture(7, 96, 96, round),
            "mesoscale differs"
        );
        assert_eq!(fc.channels, 3);
        assert!(
            fc.pixels == false_colour(&want_vis, &want_ir),
            "false colour differs"
        );

        // Messages: the warning (zipped), the discussion, the admin text.
        let (wname, _) = synth::warning(round, t + 60.0);
        let want = format!(
            "product=TORBMX category=TOR event=\"Tornado Warning\" office=KBMX state=AL wmo=\"WFUS54 KBMX {}\" priority=1",
            &wname[12..18]
        );
        assert!(r.text.contains(&want), "{want}\n{}", r.text);
        assert!(
            r.text
                .contains(&format!("THIS IS A FOXYGNAL LOOPBACK TEST, ROUND {round}."))
        );
        assert!(
            r.text
                .contains("product=AFDOKX category=AFD office=KOKX state=NY")
        );
        assert!(r.text.contains(&format!(
            "This is round {round} of the synthetic broadcast."
        )));
        // Files: the bulletins (unzipped) and the admin message.
        let (y, m, d, ..) = foxygnal_blocks::xrit::products::civil(t);
        let day = format!("{y:04}-{m:02}-{d:02}");
        let warning = std::fs::read_to_string(dir.join(format!(
            "emwin/{day}/{}.TXT",
            wname.trim_end_matches(".ZIS")
        )))
        .unwrap();
        assert!(warning.contains("TORBMX"));
        let admin =
            std::fs::read_to_string(dir.join(format!("text/{day}/FOXYGNAL_ADMIN_{round:04}.TXT")))
                .unwrap();
        assert_eq!(admin, synth::admin(round));
    }
    if rounds.start == 0 {
        assert!(!r.text.contains("event=incomplete"), "{}", r.text);
        assert!(!r.text.contains("complete=0"), "{}", r.text);
    }
}

#[test]
fn cadus_straight_into_the_decoder() {
    let (cadus, _) = transmit(2, false);
    assert_eq!(cadus.len() % 1024, 0);
    let dir = temp_dir("cadus");
    let r = receive(Some(cadus), Vec::new(), &dir, 0.0);
    check(&r, 0..2, &dir);
    // 2 rounds × (2 full disks + mesoscale + false colour).
    assert_eq!(r.pictures.len(), 8, "{}", r.text);
    // One picture at a time: numbers in order, each ending with its last row.
    for p in r.pictures.values() {
        assert_eq!(p.rows, p.height, "{}", p.name);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rf_loopback_with_offset_and_noise() {
    // The template's loop settings, 15 kHz off (9 ppm at 1694 MHz), Eb/N0
    // 4 dB. The first round's first segment or so goes by while the loops
    // lock; the second must arrive whole.
    let (_, symbols) = transmit(2, true);
    let x = channel(&symbols, 15_000.0, 4.0, 1);
    let dir = temp_dir("rf");
    let r = receive(None, x, &dir, 0.01);
    assert!(
        r.status.contains("event=lock modulation=bpsk"),
        "{}",
        r.status
    );
    assert!(!r.status.contains("event=unlock"), "{}", r.status);
    check(&r, 1..2, &dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// How the Costas loop bandwidth copes with tuning error and noise: files
/// decoded (of 2 rounds: 14 picture segments, 4 bulletins) and carrier locks
/// for each (bandwidth, offset, Eb/N0).
#[test]
#[ignore]
fn costas_sweep() {
    let (_, symbols) = transmit(2, true);
    for bw in [0.002, 0.005, 0.01, 0.02] {
        for offset in [0.0, 5_000.0, 10_000.0, 20_000.0, 40_000.0] {
            for ebn0 in [2.5, 3.0, 4.0, 6.0] {
                let x = channel(&symbols, offset, ebn0, 3);
                let dir = temp_dir("sweep");
                let r = receive(None, x, &dir, bw);
                println!(
                    "bw {bw:.3} offset {offset:>6.0} Hz Eb/N0 {ebn0:.1} dB: segments {:>2}/14 bulletins {}/4 locks {} unlocks {}",
                    r.text.matches("type=image").count(),
                    r.text.matches("type=emwin").count(),
                    r.status.matches("event=lock").count(),
                    r.status.matches("event=unlock").count(),
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

/// NOAA's sample GOES-16 HRIT files (from the NOAASIS "HRIT sample imagery"
/// page; not in the repository, they are 8 MB): set
/// `FOXYGNAL_GOES_SAMPLES` to their folder. Each is sent the way the
/// satellite does (its pixels Rice-coded line by line with the file's own
/// szip parameters, in packets with CRCs), put back together, decoded, and
/// its picture saved as PNG in the same folder's `decoded/`.
#[test]
#[ignore]
fn noaa_samples() {
    use foxygnal_blocks::xrit::decoder::{Config, Decoder};
    use foxygnal_blocks::xrit::header::Headers;
    use foxygnal_blocks::xrit::transport::{FileAssembler, FileEncoder};

    let Some(dir) = std::env::var_os("FOXYGNAL_GOES_SAMPLES") else {
        eprintln!("FOXYGNAL_GOES_SAMPLES not set");
        return;
    };
    let dir = PathBuf::from(dir);
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "lrit"))
        .collect();
    names.sort();
    for path in names {
        let bytes = std::fs::read(&path).unwrap();
        let h = Headers::parse(&bytes).unwrap();
        let data = &bytes[h.header_len as usize..];
        assert_eq!(data.len(), h.data_len());
        let mut packets = Vec::new();
        FileEncoder::new(5).encode(&h, data, &mut |p: &[u8]| packets.push(p.to_vec()));
        let compressed: usize = packets.iter().map(Vec::len).sum();
        let mut files = Vec::new();
        let mut fa = FileAssembler::new();
        for p in &packets {
            fa.push(0, 13, p, |f| files.push(f));
        }
        assert_eq!(files.len(), 1);
        let f = files.remove(0);
        assert!(f.complete && f.rice_decoded);
        assert!(f.data == data, "{}", path.display());
        let mut dec = Decoder::new(Config {
            dir: Some(dir.join("decoded")),
            ..Config::default()
        });
        dec.file(f);
        dec.finish();
        let pics = pictures(&dec.pictures.drain_records());
        let p = pics.values().next().unwrap();
        println!(
            "{}: {} packets, {:.0}% of raw, {} → {}×{} ({} rows)",
            path.file_name().unwrap().to_string_lossy(),
            packets.len(),
            100.0 * compressed as f64 / data.len() as f64,
            p.name,
            p.width,
            p.height,
            p.rows
        );
        let out = dir
            .join("decoded")
            .join(format!("{}.png", p.name.replace('/', "_")));
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&out).unwrap();
        let mut enc = png::Encoder::new(
            std::io::BufWriter::new(file),
            p.width as u32,
            p.height as u32,
        );
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&p.pixels)
            .unwrap();
    }
}
