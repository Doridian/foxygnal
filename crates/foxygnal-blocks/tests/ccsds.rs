//! The CCSDS layer end to end: packets → transfer frames → `Encoder` (RS,
//! randomizer, convolutional code, BPSK/QPSK) → a channel (Gaussian noise,
//! every phase symmetry a carrier loop can lock to, phase slips) →
//! `Deframer` → `PacketAssembler` → packets, compared byte for byte. Then
//! the four blocks in a graph.
//!
//! `threshold_sweep` (ignored) measures where frames start to fail:
//! `cargo test --release -p foxygnal-blocks --test ccsds -- --ignored
//! --nocapture`.

use std::f64::consts::TAU;

use foxygnal_blocks::ccsds::sync::Symmetry;
use foxygnal_blocks::ccsds::{
    Config, Deframer, Encoder, FrameBuilder, Layout, Modulation, PacketAssembler,
};
use foxygnal_blocks::testing::{VectorSink, VectorSource};
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

/// A space packet with APID 100, sequence `seq`, `len` random data bytes.
fn packet(seq: u16, len: usize, rng: &mut Rng) -> Vec<u8> {
    let mut p = vec![0x00, 100, 0xC0 | (seq >> 8) as u8 & 0x3F, seq as u8];
    p.extend_from_slice(&((len - 1) as u16).to_be_bytes());
    p.extend((0..len).map(|_| rng.next() as u8));
    p
}

/// Codeblocks (transfer frame + room for the parity) carrying random
/// packets; returns them and the packets.
fn frames(cfg: &Config, n: usize, seed: u64) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let parity = cfg.rs.map_or(0, |(i, _)| 32 * i);
    let layout = Layout {
        frame_len: cfg.frame_len - parity,
        insert_zone: 0,
        trailer: 0,
    };
    let mut fb = FrameBuilder::new(layout, 42, 3).unwrap();
    let mut rng = Rng(seed);
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut packets = Vec::new();
    let mut seq = 0;
    while out.len() < n {
        let len = 50 + (rng.next() % 2500) as usize;
        let p = packet(seq, len, &mut rng);
        seq = (seq + 1) & 0x3FFF;
        fb.push_packet(&p, &mut |f: &[u8]| {
            let mut cb = f.to_vec();
            cb.resize(cfg.frame_len, 0);
            out.push(cb);
        });
        packets.push(p);
    }
    (out, packets)
}

/// Encode, prefix `junk` noise-only symbols, apply `sym` (from symbol
/// `slip_at` on, `slip` instead) and noise for `ebn0_db` per bit into the
/// convolutional code (per channel bit for uncoded links).
#[allow(clippy::too_many_arguments)]
fn transmit(
    cfg: &Config,
    cbs: &[Vec<u8>],
    sym: Symmetry,
    slip: Option<(usize, Symmetry)>,
    ebn0_db: f64,
    junk: usize,
    seed: u64,
) -> Vec<Complex32> {
    let mut enc = Encoder::new(cfg.clone()).unwrap();
    let mut x = Vec::new();
    let mut rng = Rng(seed);
    for _ in 0..junk {
        x.push(Complex32::new(rng.gauss() as f32, rng.gauss() as f32) * 0.7);
    }
    for cb in cbs {
        enc.encode(cb, |s| x.push(s));
    }
    // Unit-energy symbols: Es/N0 = Eb/N0 · bits per symbol · code rate.
    let bits_per_symbol =
        cfg.modulation.bits_per_symbol() as f64 * if cfg.coded { 0.5 } else { 1.0 };
    let esn0 = 10f64.powf(ebn0_db / 10.0) * bits_per_symbol;
    let sigma = (1.0 / (2.0 * esn0)).sqrt();
    x.iter()
        .enumerate()
        .map(|(i, &s)| {
            let t = match slip {
                Some((at, other)) if i >= at => other,
                _ => sym,
            };
            t.apply_c(s)
                + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32)
        })
        .collect()
}

struct Received {
    frames: Vec<(bool, Vec<u8>)>,
    packets: Vec<(bool, Vec<u8>)>,
    unlocks: u64,
    ber: f64,
}

fn receive(cfg: &Config, x: &[Complex32]) -> Received {
    let mut d = Deframer::new(cfg.clone()).unwrap();
    let mut frames = Vec::new();
    // Feed in uneven pieces, as a graph would.
    let mut at = 0;
    let mut k = 0;
    while at < x.len() {
        let n = [1usize, 700, 4096, 13][k % 4].min(x.len() - at);
        d.push(&x[at..at + n], &mut |f| {
            frames.push((f.ok(), f.data.to_vec()))
        });
        at += n;
        k += 1;
    }
    d.flush(&mut |f| frames.push((f.ok(), f.data.to_vec())));
    let parity = cfg.rs.map_or(0, |(i, _)| 32 * i);
    let mut asm = PacketAssembler::new(Layout {
        frame_len: cfg.frame_len - parity,
        insert_zone: 0,
        trailer: 0,
    })
    .unwrap();
    let mut packets = Vec::new();
    for (ok, f) in &frames {
        if *ok {
            asm.push_frame(f, |p| packets.push((p.after_gap, p.bytes.to_vec())));
        }
    }
    let s = d.stats();
    Received {
        frames,
        packets,
        unlocks: s.unlocks,
        ber: s.viterbi_errors as f64 / s.viterbi_bits.max(1) as f64,
    }
}

/// Every packet received is one that was sent, in order; returns how many.
fn check_packets(sent: &[Vec<u8>], got: &[(bool, Vec<u8>)]) -> usize {
    let mut at = 0;
    for (_, p) in got {
        let k = sent[at..]
            .iter()
            .position(|s| s == p)
            .unwrap_or_else(|| panic!("a packet that wasn't sent (or out of order)"));
        at += k + 1;
    }
    got.len()
}

fn hrit_like() -> Config {
    Config {
        modulation: Modulation::Bpsk,
        ..Config::default()
    }
}

#[test]
fn qpsk_coded_through_every_symmetry_at_3_db() {
    let cfg = Config::default();
    let (cbs, packets) = frames(&cfg, 10, 1);
    for (k, &sym) in Modulation::Qpsk.symmetries().iter().enumerate() {
        let x = transmit(&cfg, &cbs, sym, None, 3.0, 3001, 10 + k as u64);
        let r = receive(&cfg, &x);
        assert_eq!(r.frames.len(), cbs.len(), "{sym:?}");
        assert!(r.frames.iter().all(|f| f.0), "{sym:?}: a frame failed RS");
        for ((_, got), want) in r.frames.iter().zip(&cbs) {
            // The parity differs (the encoder filled it in); the frame not.
            assert_eq!(got[..892], want[..892], "{sym:?}");
        }
        let n = check_packets(&packets, &r.packets);
        assert!(n + 6 >= packets.len(), "{sym:?}: {n} of {}", packets.len());
        assert!(r.ber > 0.02, "the channel had errors: {}", r.ber);
    }
}

#[test]
fn bpsk_coded_both_polarities() {
    let cfg = hrit_like();
    let (cbs, packets) = frames(&cfg, 8, 2);
    for &sym in Modulation::Bpsk.symmetries() {
        // Half a symbol's worth of junk: the pairs straddle the start.
        let x = transmit(&cfg, &cbs, sym, None, 3.0, 1001, 3);
        let r = receive(&cfg, &x);
        assert_eq!(r.frames.len(), cbs.len(), "{sym:?}");
        assert!(r.frames.iter().all(|f| f.0), "{sym:?}");
        assert!(check_packets(&packets, &r.packets) + 6 >= packets.len());
    }
}

#[test]
fn coded_with_nrzm_and_g2_not_inverted() {
    for (modulation, invert) in [(Modulation::Qpsk, false), (Modulation::Bpsk, true)] {
        let cfg = Config {
            modulation,
            conv_invert: invert,
            nrzm: true,
            ..Config::default()
        };
        let (cbs, packets) = frames(&cfg, 8, 30);
        for (k, &sym) in modulation.symmetries().iter().enumerate().step_by(3) {
            let x = transmit(&cfg, &cbs, sym, None, 3.5, 2000, 40 + k as u64);
            let r = receive(&cfg, &x);
            let label = format!("{modulation:?} invert={invert} {sym:?}");
            assert_eq!(r.frames.len(), cbs.len(), "{label}");
            assert!(r.frames.iter().all(|f| f.0), "{label}");
            assert!(
                check_packets(&packets, &r.packets) + 6 >= packets.len(),
                "{label}"
            );
        }
    }
}

#[test]
fn uncoded_links_with_and_without_nrzm() {
    for (modulation, nrzm, rs) in [
        (Modulation::Bpsk, false, Some((4, true))),
        (Modulation::Bpsk, true, Some((5, false))),
        (Modulation::Qpsk, false, Some((1, true))),
        (Modulation::Qpsk, true, None),
    ] {
        let cfg = Config {
            modulation,
            coded: false,
            conv_invert: false,
            nrzm,
            randomized: true,
            rs,
            frame_len: rs.map_or(892, |(i, _)| 255 * i),
        };
        // Without RS nothing may go wrong: no noise then.
        let ebn0 = if rs.is_some() { 7.0 } else { 30.0 };
        let (cbs, packets) = frames(&cfg, 8, 4);
        for (k, &sym) in modulation.symmetries().iter().enumerate() {
            let x = transmit(&cfg, &cbs, sym, None, ebn0, 517, 20 + k as u64);
            let r = receive(&cfg, &x);
            let label = format!("{modulation:?} nrzm={nrzm} rs={rs:?} {sym:?}");
            // NRZ-M can't decode the very first bit after the junk.
            assert!(
                r.frames.len() >= cbs.len() - 1,
                "{label}: {}",
                r.frames.len()
            );
            assert!(r.frames.iter().all(|f| f.0), "{label}");
            assert!(
                check_packets(&packets, &r.packets) + 6 >= packets.len(),
                "{label}"
            );
        }
    }
}

#[test]
fn relocks_after_a_carrier_phase_slip() {
    let cfg = Config::default();
    let (cbs, packets) = frames(&cfg, 24, 5);
    let syms = Modulation::Qpsk.symmetries();
    let slip_at = 3001 + 8 * 8224; // eight frames in
    let x = transmit(&cfg, &cbs, syms[0], Some((slip_at, syms[1])), 4.0, 3001, 6);
    let r = receive(&cfg, &x);
    assert_eq!(r.unlocks, 1);
    let ok = r.frames.iter().filter(|f| f.0).count();
    // A few frames go while the lock is lost and found again.
    assert!(ok >= 24 - 7, "{ok}");
    let n = check_packets(&packets, &r.packets);
    assert!(n + 20 >= packets.len(), "{n} of {}", packets.len());
    // The packet after the outage says so.
    assert!(r.packets.iter().skip(1).any(|p| p.0));
}

#[test]
fn uncoded_180_degree_slip_is_followed_without_losing_lock() {
    let cfg = Config {
        modulation: Modulation::Bpsk,
        coded: false,
        conv_invert: false,
        nrzm: false,
        randomized: true,
        rs: Some((4, true)),
        frame_len: 1020,
    };
    let (cbs, _) = frames(&cfg, 10, 7);
    let syms = Modulation::Bpsk.symmetries();
    let slip_at = 300 + 4 * 8224 + 5000;
    let x = transmit(&cfg, &cbs, syms[0], Some((slip_at, syms[1])), 8.0, 300, 8);
    let r = receive(&cfg, &x);
    assert_eq!(r.unlocks, 0);
    // The frame the slip lands in is lost; the rest are fine.
    let ok = r.frames.iter().filter(|f| f.0).count();
    assert_eq!(ok, cbs.len() - 1);
}

fn block(kind: &str, p: serde_json::Value) -> Instance {
    foxygnal_blocks::registry()
        .build(kind, p.as_object().unwrap())
        .unwrap()
}

#[test]
fn blocks_loop_back_bytes_to_packets() {
    // Bytes → framer → modulator → (noise) → deframer → packets (records and
    // text), in a graph.
    let mut rng = Rng(9);
    let payload: Vec<u8> = (0..30_000).map(|_| rng.next() as u8).collect();
    let mut fg = Flowgraph::new();
    let src = fg.add("src", VectorSource::instance(payload.clone()));
    let framer = fg.add(
        "framer",
        block(
            "ccsds_framer",
            json!({ "spacecraft": 12, "vcid": 5, "apid": 321, "packet_len": 1000 }),
        ),
    );
    let modulator = fg.add(
        "mod",
        block(
            "ccsds_modulator",
            json!({ "modulation": "bpsk", "symbol_rate": 100000.0 }),
        ),
    );
    let (sink, syms) = VectorSink::<Complex32>::instance();
    let sink = fg.add("syms", sink);
    fg.connect(src, 0, framer, 0).unwrap();
    fg.connect(framer, 0, modulator, 0).unwrap();
    fg.connect(modulator, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    let cfg = hrit_like();
    let clean = syms.lock().unwrap().clone();
    let mut x = transmit(&cfg, &[], Symmetry::default(), None, 0.0, 1500, 1);
    let mut rng = Rng(10);
    x.extend(
        clean
            .iter()
            .map(|&s| -s + Complex32::new(0.4 * rng.gauss() as f32, 0.4 * rng.gauss() as f32)),
    );

    for format in ["records", "text"] {
        let mut fg = Flowgraph::new();
        let src = fg.add(
            "src",
            VectorSource::instance_with_meta(x.clone(), StreamMeta::with_rate(100_000.0)),
        );
        let d = fg.add(
            "deframer",
            block("ccsds_deframer", json!({ "modulation": "bpsk" })),
        );
        let p = fg.add(
            "packets",
            block("ccsds_packets", json!({ "format": format })),
        );
        let (psink, pk) = VectorSink::<u8>::instance();
        let (ssink, st) = VectorSink::<u8>::instance();
        let psink = fg.add("pk", psink);
        let ssink = fg.add("st", ssink);
        fg.connect(src, 0, d, 0).unwrap();
        fg.connect(d, 0, p, 0).unwrap();
        fg.connect(p, 0, psink, 0).unwrap();
        fg.connect(d, 1, ssink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        let out = pk.lock().unwrap().clone();
        let status = String::from_utf8(st.lock().unwrap().clone()).unwrap();
        assert!(
            status.contains("event=lock modulation=bpsk rotation=180"),
            "{status}"
        );
        assert!(status.contains("state=lock"), "{status}");
        if format == "text" {
            let text = String::from_utf8(out).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 30, "{text}");
            assert!(lines[0].starts_with("scid=12 vcid=5 apid=321 seq=0 len=1000 data="));
            let hex: String = payload[..16].iter().map(|b| format!("{b:02x}")).collect();
            assert!(lines[0].contains(&hex), "{}", lines[0]);
            continue;
        }
        let mut reader = foxygnal_blocks::ccsds::wire::RecordReader::new();
        let mut data = Vec::new();
        let mut n = 0;
        reader.push(&out, |r| {
            assert_eq!((r.spacecraft, r.vcid, r.header.apid), (12, 5, 321));
            assert_eq!(usize::from(r.header.seq), n);
            data.extend_from_slice(&r.packet[6..]);
            n += 1;
        });
        assert_eq!(reader.skipped, 0);
        assert_eq!(data, payload);
    }
}

/// Frame error rate against Eb/N0 for the usual configurations.
#[test]
#[ignore]
fn threshold_sweep() {
    for (name, cfg) in [
        ("QPSK r=1/2 + RS I=4", Config::default()),
        ("BPSK r=1/2 + RS I=4", hrit_like()),
    ] {
        let (cbs, _) = frames(&cfg, 200, 11);
        for tenths in (10..=40).step_by(5) {
            let ebn0 = f64::from(tenths) / 10.0;
            let sym = *cfg.modulation.symmetries().last().unwrap();
            let x = transmit(&cfg, &cbs, sym, None, ebn0, 3001, 12);
            let r = receive(&cfg, &x);
            let ok = r.frames.iter().filter(|f| f.0).count();
            println!(
                "{name}: Eb/N0 {ebn0:.1} dB: {ok}/{} frames ok, channel BER {:.3}",
                cbs.len(),
                r.ber
            );
        }
    }
}

/// Run `blocks` (kind, params) in a chain from `input` to a sink.
fn chain<I: foxygnal_core::Sample, O: foxygnal_core::Sample + Clone>(
    input: Vec<I>,
    rate: Option<f64>,
    blocks: &[(&str, serde_json::Value)],
) -> Vec<O> {
    let mut fg = Flowgraph::new();
    let src = match rate {
        Some(r) => VectorSource::instance_with_meta(input, StreamMeta::with_rate(r)),
        None => VectorSource::instance(input),
    };
    let mut prev = fg.add("src", src);
    for (i, (kind, p)) in blocks.iter().enumerate() {
        let b = fg.add(format!("b{i}"), block(kind, p.clone()));
        fg.connect(prev, 0, b, 0).unwrap();
        prev = b;
    }
    let (sink, out) = VectorSink::<O>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(prev, 0, sink, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    out.lock().unwrap().clone()
}

#[test]
fn rf_loopback_through_the_standard_demodulator_blocks() {
    const EBN0_DB: f64 = 4.0;
    // The loopback template's chain: framer → modulator → RRC (×4) →
    // channel (300 Hz off, a carrier phase, noise at Eb/N0 4 dB) → RRC
    // matched filter → Clock Recovery → Costas Loop (order 4) → deframer →
    // packets.
    let symbol_rate = 72_000.0;
    let sps = 4.0;
    let mut rng = Rng(77);
    let payload: Vec<u8> = (0..20_000).map(|_| rng.next() as u8).collect();
    let tx: Vec<Complex32> = chain(
        payload.clone(),
        None,
        &[
            ("ccsds_framer", json!({ "packet_len": 500 })),
            ("ccsds_modulator", json!({ "symbol_rate": symbol_rate })),
            (
                "rrc_filter",
                json!({ "mode": "interpolate", "sps": sps, "rolloff": 0.5, "span": 11 }),
            ),
        ],
    );
    let fs = symbol_rate * sps;
    // Eb/N0 per bit into the code: Es = 2 · ½ · Eb. The interpolator keeps
    // the amplitude (unit power per sample), so Es = sps.
    let esn0 = 10f64.powf(EBN0_DB / 10.0);
    let sigma = (sps / (2.0 * esn0)).sqrt();
    let rx: Vec<Complex32> = std::iter::repeat_n(Complex32::new(0.0, 0.0), 4000)
        .chain(tx)
        .chain(std::iter::repeat_n(Complex32::new(0.0, 0.0), 4000))
        .enumerate()
        .map(|(n, s)| {
            let ph = 1.0 + TAU * 300.0 * n as f64 / fs;
            s * Complex32::from_polar(1.0, ph as f32)
                + Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32)
        })
        .collect();
    let text: Vec<u8> = chain(
        rx,
        Some(fs),
        &[
            (
                "rrc_filter",
                json!({ "sps": sps, "rolloff": 0.5, "span": 11 }),
            ),
            ("clock_recovery", json!({ "sps": sps, "loop_bw": 0.01 })),
            ("costas_loop", json!({ "order": "4", "loop_bw": 0.005 })),
            ("ccsds_deframer", json!({})),
            ("ccsds_packets", json!({ "format": "text" })),
        ],
    );
    let text = String::from_utf8(text).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // The carrier and clock loops settle within the first frame or two.
    assert!(lines.len() >= 34, "{} packets:\n{text}", lines.len());
    let last = lines.last().unwrap();
    assert!(
        last.starts_with("scid=0 vcid=1 apid=100 seq=39 len=500"),
        "{last}"
    );
}
