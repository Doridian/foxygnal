//! GOES DCS decoder: transmissions from the crate's own CS2 modulator, alone
//! and many at once, through noise and frequency offsets, decoded by the
//! whole-band receiver and by the block in a graph.
//!
//! These are loopback tests: the modulator and the demodulator were both
//! written from the certification standard, so they prove the two agree
//! with each other and with our reading of it, not with real platforms.

use foxygnal_blocks::digital::dcs::frame::{Baud, Format, bch_encode, channel_freq, flag_for};
use foxygnal_blocks::digital::dcs::modulator::{
    Burst, Mix, Sim, SimConfig, ascii, modulate, pseudo_binary,
};
use foxygnal_blocks::digital::dcs::receiver::{Config, Receiver};
use foxygnal_blocks::digital::dcs::{demod::End, format_line};
use foxygnal_blocks::sources::Rng;
use foxygnal_blocks::testing::{VectorSink, VectorSource};
use foxygnal_core::{Complex32, Flowgraph, StreamMeta};
use serde_json::json;

const CENTER: f64 = 1_679_900_000.0;

/// Offset from `CENTER` of a channel on the downlink.
fn chan(ch: u16) -> f64 {
    channel_freq(ch).unwrap() + 1_278_000_000.0 - CENTER
}

/// Complex Gaussian noise of total power `p` per sample.
fn add_noise(x: &mut [Complex32], p: f32, seed: u64) {
    let mut rng = Rng::new(seed);
    let s = (p / 2.0).sqrt();
    for v in x {
        let (a, b) = rng.gaussian_pair();
        *v += Complex32::new(a * s, b * s);
    }
}

/// Noise power per sample giving `esn0` dB for a carrier of amplitude `amp`.
fn noise_for(esn0: f32, amp: f32, baud: Baud, fs: f64) -> f32 {
    let cn0 = esn0 + 10.0 * (baud.symbol_rate() as f32).log10();
    amp * amp * fs as f32 / 10f32.powf(cn0 / 10.0)
}

struct Decoded {
    line: String,
    addr: Option<u32>,
    baud: Baud,
    ch: Option<u16>,
    data: Vec<u8>,
    end: End,
    snr: f32,
    freq: f64,
}

fn receive(x: &[Complex32], fs: f64) -> Vec<Decoded> {
    let mut rx = Receiver::new(Config {
        fs,
        center: Some(CENTER),
        level_db: 10.0,
        max_platforms: 32,
        b300: true,
        b1200: true,
    });
    let mut out = Vec::new();
    let mut used = 0;
    let grab = |rx: &mut Receiver, out: &mut Vec<Decoded>| {
        while rx.take(|m| {
            let mut line = String::new();
            format_line(m, &mut line);
            out.push(Decoded {
                line,
                addr: m.addr.map(|a| a.0),
                baud: m.baud,
                ch: m.channel.map(|c| c.0),
                data: m.data.to_vec(),
                end: m.end,
                snr: m.stats.snr_db,
                freq: m.freq,
            });
        }) {}
    };
    while used < x.len() {
        used += rx.push(&x[used..]);
        grab(&mut rx, &mut out);
    }
    rx.finish();
    grab(&mut rx, &mut out);
    for d in &out {
        eprintln!("{}", d.line);
    }
    out
}

fn burst(info: u32, baud: Baud, format: Format, data: Vec<u8>) -> Burst {
    Burst {
        addr: bch_encode(info),
        baud,
        flag: flag_for(format),
        data,
    }
}

/// `b` at `freq`, with `lead` seconds of silence before and 1 s after.
fn place(b: &Burst, fs: f64, freq: f64, amp: f32, lead: f64) -> Vec<Complex32> {
    let mut x = vec![Complex32::new(0.0, 0.0); (lead * fs) as usize];
    x.extend(modulate(b, fs, freq, amp, 0.7));
    x.resize(x.len() + fs as usize, Complex32::new(0.0, 0.0));
    x
}

#[test]
fn one_platform_each_rate_with_noise_and_offset() {
    let fs = 48_000.0;
    for (baud, ch, off) in [(Baud::B300, 130, 37.0), (Baud::B1200, 433, -61.0)] {
        let text = ascii("0123 STAGE 12.34 BATT 13.1");
        let b = burst(0x0ABCD, baud, Format::Ascii, text.clone());
        let mut x = place(&b, fs, chan(ch) + off, 0.1, 0.3);
        add_noise(&mut x, noise_for(14.0, 0.1, baud, fs), 5);
        let got = receive(&x, fs);
        assert_eq!(got.len(), 1, "{baud:?}");
        let d = &got[0];
        assert_eq!(d.addr, Some(bch_encode(0x0ABCD)));
        assert_eq!(d.baud, baud);
        assert_eq!(d.ch, Some(ch));
        assert_eq!(d.end, End::Eot);
        assert_eq!(d.data, text);
        assert!((d.snr - 14.0).abs() < 2.0, "snr {}", d.snr);
        assert!((d.freq - (chan(ch) + off)).abs() < 2.0, "freq {}", d.freq);
        assert!(
            d.line.contains(&format!("ch={ch} baud={}", baud.bits())),
            "{}",
            d.line
        );
        assert!(d.line.contains("offset_Hz="), "{}", d.line);
        assert!(
            d.line.contains("text=\"0123 STAGE 12.34 BATT 13.1\""),
            "{}",
            d.line
        );
    }
}

#[test]
fn pseudo_binary_and_binary_messages() {
    let fs = 32_000.0;
    let pb = pseudo_binary(3, &[(1234, 2), (65_000, 3)]);
    let b1 = burst(0x11111, Baud::B300, Format::PseudoBinary, pb.clone());
    let bin: Vec<u8> = (0..40u8).map(|i| i.wrapping_mul(37) ^ 0x5A).collect();
    let b2 = burst(0x22222, Baud::B1200, Format::Binary, bin.clone());
    let mut x = place(&b1, fs, chan(128), 0.1, 0.2);
    let y = place(&b2, fs, chan(436), 0.1, 0.9);
    if y.len() > x.len() {
        x.resize(y.len(), Complex32::new(0.0, 0.0));
    }
    for (a, b) in x.iter_mut().zip(&y) {
        *a += b;
    }
    add_noise(&mut x, noise_for(20.0, 0.1, Baud::B300, fs), 6);
    let got = receive(&x, fs);
    assert_eq!(got.len(), 2);
    let p = got
        .iter()
        .find(|d| d.addr == Some(bch_encode(0x11111)))
        .unwrap();
    assert_eq!(p.data, pb);
    assert!(p.line.contains("format=pseudo-binary"), "{}", p.line);
    let q = got
        .iter()
        .find(|d| d.addr == Some(bch_encode(0x22222)))
        .unwrap();
    assert_eq!(q.data, bin);
    assert_eq!(q.end, End::Length);
    assert!(q.line.contains("format=binary"), "{}", q.line);
    assert!(q.line.contains("hex=5A"), "{}", q.line);
}

#[test]
fn many_platforms_at_once() {
    // Twelve platforms whose transmissions overlap, on neighbouring
    // channels, 10 dB apart in level, each with its own frequency error.
    let fs = 64_000.0;
    let channels = [
        125u16, 425, 127, 430, 128, 129, 431, 132, 433, 137, 140, 144,
    ];
    let mut x = vec![Complex32::new(0.0, 0.0); (fs * 6.0) as usize];
    let mut expect = Vec::new();
    for (i, &ch) in channels.iter().enumerate() {
        let baud = if i % 4 == 3 { Baud::B1200 } else { Baud::B300 };
        let text = ascii(&format!("PLATFORM {i:02} ON CH {ch}"));
        let b = burst(0x1000 + i as u32, baud, Format::Ascii, text.clone());
        let amp = 0.1 * 10f32.powf(-(i as f32 % 5.0) / 10.0);
        let err = (i as f64 * 37.0) % 120.0 - 60.0;
        let start = (0.13 * i as f64 * fs) as usize;
        let s = modulate(&b, fs, chan(ch) + err, amp, i as f64);
        for (a, v) in x[start..].iter_mut().zip(&s) {
            *a += v;
        }
        expect.push((bch_encode(0x1000 + i as u32), ch, text));
    }
    add_noise(&mut x, noise_for(30.0, 0.1, Baud::B300, fs), 7);
    let got = receive(&x, fs);
    for (addr, ch, text) in &expect {
        let d = got
            .iter()
            .find(|d| d.addr == Some(*addr))
            .unwrap_or_else(|| panic!("{addr:08X} on {ch} not decoded"));
        assert_eq!(d.ch, Some(*ch));
        assert_eq!(&d.data, text, "{}", d.line);
    }
    assert_eq!(got.len(), expect.len());
}

#[test]
fn steady_carriers_are_ignored() {
    // A pilot-like tone that never carries a preamble, and a DC spike: no
    // messages, and a platform next to them still decodes.
    let fs = 32_000.0;
    let text = ascii("NEXT TO THE PILOT");
    let b = burst(0x33333, Baud::B300, Format::Ascii, text.clone());
    let mut x = place(&b, fs, chan(140), 0.05, 2.5);
    let pilot = chan(131) + 210.0;
    for (i, v) in x.iter_mut().enumerate() {
        let a = std::f64::consts::TAU * pilot * i as f64 / fs;
        *v += Complex32::new(a.cos() as f32, a.sin() as f32) * 0.2 + Complex32::new(0.05, 0.0);
    }
    add_noise(&mut x, noise_for(20.0, 0.05, Baud::B300, fs), 8);
    let got = receive(&x, fs);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].data, text);
}

#[test]
fn threshold() {
    // Es/N0 where messages of 32 characters still come through: report the
    // rate over a sweep, assert on a comfortable point.
    let fs = 24_000.0;
    for baud in [Baud::B300, Baud::B1200] {
        let mut summary = Vec::new();
        for esn0 in [8.0f32, 10.0, 12.0, 14.0] {
            let mut ok = 0;
            let runs = 12;
            for r in 0..runs {
                let text = ascii(&format!("{r:02} THRESHOLD TEST 0123456789 ABCD"));
                let b = burst(0x44444 + r, baud, Format::Ascii, text.clone());
                let mut x = place(&b, fs, chan(433) + 20.0 * r as f64 - 50.0, 0.1, 0.2);
                add_noise(&mut x, noise_for(esn0, 0.1, baud, fs), 100 + u64::from(r));
                if receive(&x, fs).iter().any(|d| d.data == text) {
                    ok += 1;
                }
            }
            summary.push(format!("{esn0} dB: {ok}/{runs}"));
            if esn0 >= 14.0 {
                assert_eq!(ok, runs, "{baud:?} at {esn0} dB");
            }
        }
        eprintln!("{baud:?}: {}", summary.join(", "));
    }
}

#[test]
fn simulator_through_the_blocks() {
    // goes_dcs_sim → noise → goes_dcs_decoder in a graph: every platform's
    // first messages arrive, with the channel the simulator put it on.
    let fs = 50_000.0;
    let reg = foxygnal_blocks::registry();
    let params = json!({
        "sample_rate": fs, "center": CENTER, "platforms": 6, "interval": 1.0,
        "rates": "both", "amplitude": 0.05, "freq_error": 100.0, "seed": 9
    });
    let sim = Sim::new(SimConfig {
        fs,
        center: CENTER,
        platforms: 6,
        interval: 1.0,
        mix: Mix::Both,
        amplitude: 0.05,
        freq_error: 100.0,
        seed: 9,
    });
    // Render ~8 s with the block itself, so the test covers it.
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "sim",
        reg.build("goes_dcs_sim", params.as_object().unwrap())
            .unwrap(),
    );
    let head = fg.add(
        "head",
        reg.build(
            "head",
            json!({"type": "c32", "count": (fs * 8.0) as u64})
                .as_object()
                .unwrap(),
        )
        .unwrap(),
    );
    let (sink, samples) = VectorSink::<Complex32>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, head, 0).unwrap();
    fg.connect(head, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let mut x = samples.lock().unwrap().clone();
    add_noise(&mut x, noise_for(28.0, 0.05, Baud::B300, fs), 10);

    let mut fg = Flowgraph::new();
    let mut meta = StreamMeta::with_rate(fs);
    meta.center_freq = Some(CENTER);
    let src = fg.add("src", VectorSource::instance_with_meta(x, meta));
    let dec = fg.add(
        "dcs",
        reg.build("goes_dcs_decoder", json!({}).as_object().unwrap())
            .unwrap(),
    );
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, dec, 0).unwrap();
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    eprintln!("{text}");
    for (addr, ch, baud) in sim.platforms() {
        let first = sim.expected(addr, 0).unwrap();
        let first: String = first.iter().map(|&b| (b & 0x7F) as char).collect();
        let line = text
            .lines()
            .find(|l| l.contains(&format!("addr={addr:08X} ")))
            .unwrap_or_else(|| panic!("nothing from {addr:08X}"));
        assert!(
            line.contains(&format!("ch={ch} baud={}", baud.bits())),
            "{line}"
        );
        assert!(
            text.contains(&format!("text=\"{}\"", first.replace('"', "\\\""))),
            "first message of {addr:08X} missing"
        );
    }
}
