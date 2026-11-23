//! Published messages (mostly from "The 1090 MHz Riddle", J. Sun, and the
//! pyModeS test suite that goes with it), and an independent PPM modulator
//! for end-to-end runs through a flowgraph.

use foxygnal_core::{BufferKind, Flowgraph, StreamMeta};
use serde_json::json;

use super::commb::{self, CommB, Reference};
use super::cpr::nl;
use super::msg::crc_residual;
use super::*;
use crate::testing::{VectorSink, VectorSource};

const KLM: &str = "8D4840D6202CC371C32CE0576098";
const EVEN: &str = "8D40621D58C382D690C8AC2863A7";
const ODD: &str = "8D40621D58C386435CC412692AD6";
const VELOCITY: &str = "8D485020994409940838175B284F";
const AIRSPEED: &str = "8DA05F219B06B6AF189400CBC33F";

fn msg(hex: &str) -> ModeS {
    ModeS::from_hex(hex).unwrap()
}

#[test]
fn crc_accepts_real_messages_and_rejects_damage() {
    for hex in [KLM, EVEN, ODD, VELOCITY, AIRSPEED] {
        assert_eq!(msg(hex).residual(), 0, "{hex}");
    }
    let mut m = msg(KLM);
    m.bytes[6] ^= 0x10;
    assert_ne!(crc_residual(&m.bytes), 0);
}

#[test]
fn decodes_identification() {
    let m = msg(KLM);
    assert_eq!((m.df(), m.icao(), m.tc()), (17, 0x4840D6, 4));
    assert_eq!(m.callsign().unwrap().as_str(), "KLM1023");
    assert_eq!(m.category(), Some(('A', 0)));
    assert_eq!(m.cpr(), None);
}

#[test]
fn decodes_all_call_reply() {
    // The Riddle, "All-call reply": DF11, capability 5, in reply to
    // interrogator code 22 (which the parity carries).
    let m = msg("5D484FDEA248F5");
    assert_eq!((m.df(), m.len, m.icao(), m.ca()), (11, 7, 0x484FDE, 5));
    assert_eq!(m.residual(), 22);
}

#[test]
fn recovers_addresses_from_address_parity() {
    // pyModeS test vectors.
    for (hex, icao) in [
        ("A0001839CA3800315800007448D9", 0x400940),
        ("A000139381951536E024D4CCF6B5", 0x3C4DD2),
        ("A000029CFFBAA11E2004727281F1", 0x4243D0),
    ] {
        assert_eq!(msg(hex).residual(), icao, "{hex}");
    }
}

#[test]
fn decodes_altitude_and_identity_codes() {
    // pyModeS: altitude 32300 ft (Q = 1), squawk 1346.
    assert_eq!(
        ac13_ft(msg("A02014B400000000000000F9D514").code13()),
        Some(32_300)
    );
    assert_eq!(
        id13_squawk(msg("A800292DFFBBA9BD8000005C3D1E").code13()),
        0x1346
    );
}

/// Gillham code for an altitude (ft), as squawk-layout bits, written from
/// the definition: a Gray-coded count of 500 ft steps in D2 D4 A1 A2 A4 B1 B2
/// B4 and a reflected 5-cycle code of the 100 ft steps in C1 C2 C4.
fn gillham_encode(alt: i32) -> u32 {
    let n = (alt + 1200) / 100;
    let (fives, mut hundreds) = ((n / 5) as u32, (n % 5) as u32);
    if fives % 2 == 1 {
        hundreds = 4 - hundreds;
    }
    let c = [0b001, 0b011, 0b010, 0b110, 0b100][hundreds as usize]; // C1 C2 C4
    let gray = fives ^ (fives >> 1);
    let mut code = 0;
    for (k, bit) in [
        0x0002, 0x0004, 0x1000, 0x2000, 0x4000, 0x0100, 0x0200, 0x0400,
    ]
    .into_iter()
    .enumerate()
    {
        if gray & (0x80 >> k) != 0 {
            code |= bit;
        }
    }
    for (k, bit) in [0x0010, 0x0020, 0x0040].into_iter().enumerate() {
        if c & (0b100 >> k) != 0 {
            code |= bit;
        }
    }
    code
}

/// The 13 bits of an AC field (C1 A1 C2 A2 C4 A4 M B1 Q B2 D2 B4 D4) from
/// squawk-layout bits (Q = 0, M = 0).
fn squawk_to_13(sq: u32) -> u32 {
    let order = [
        0x0010, 0x1000, 0x0020, 0x2000, 0x0040, 0x4000, 0, 0x0100, 0x0001, 0x0200, 0x0002, 0x0400,
        0x0004,
    ];
    order
        .iter()
        .fold(0, |acc, &b| acc << 1 | u32::from(b != 0 && sq & b != 0))
}

#[test]
fn gillham_round_trips_every_altitude() {
    for alt in (-1200..=126_700).step_by(100) {
        let code = gillham_encode(alt);
        assert_eq!(gillham_ft(code), Some(alt), "{alt} → {code:04X}");
        assert_eq!(ac13_ft(squawk_to_13(code)), Some(alt), "{alt}");
    }
    // Ten bits set at random aren't usually an altitude.
    assert_eq!(gillham_ft(0x0001), None);
    assert_eq!(gillham_ft(0x7700), None, "no C bit");
}

#[test]
fn decodes_comm_b_registers() {
    let mb = |hex: &str| msg(hex).me_field();
    let none = Reference::default();
    match commb::decode(mb("A000083E202CC371C31DE0AA1CCF"), none) {
        CommB::Ident(cs) => assert_eq!(cs.as_str(), "KLM1017"),
        c => panic!("{c:?}"),
    }
    assert_eq!(
        commb::decode(mb("A800178D10010080F50000D5893C"), none),
        CommB::DataLink
    );
    match commb::decode(mb("A0000638FA81C10000000081A92F"), none) {
        CommB::Gicb(caps) => {
            let names: Vec<_> = commb::GICB_REGISTERS
                .iter()
                .enumerate()
                .filter(|(i, _)| caps & (1 << (23 - i)) != 0)
                .map(|(_, n)| *n)
                .collect();
            assert!(
                names.contains(&"2,0") && names.contains(&"6,0"),
                "{names:?}"
            );
        }
        c => panic!("{c:?}"),
    }
    match commb::decode(mb("A000029C85E42F313000007047D3"), none) {
        CommB::Intent(i) => {
            assert_eq!(i.mcp_alt_ft, Some(3008));
            assert_eq!(i.fms_alt_ft, Some(3008));
            assert!((i.baro_hpa.unwrap() - 1020.0).abs() < 0.01);
        }
        c => panic!("{c:?}"),
    }
    match commb::decode(mb("A000139381951536E024D4CCF6B5"), none) {
        CommB::TrackTurn(t) => {
            assert!((t.roll_deg.unwrap() - 2.1).abs() < 0.05, "{t:?}");
            assert!((t.track_deg.unwrap() - 114.258).abs() < 0.01, "{t:?}");
            assert_eq!(t.gs_kt, Some(438.0));
            assert_eq!(t.track_rate, Some(0.125));
            assert_eq!(t.tas_kt, Some(424.0));
        }
        c => panic!("{c:?}"),
    }
    match commb::decode(mb("A00004128F39F91A7E27C46ADC21"), none) {
        CommB::HeadingSpeed(h) => {
            assert!((h.heading_deg.unwrap() - 42.715).abs() < 0.01, "{h:?}");
            assert_eq!(h.ias_kt, Some(252.0));
            assert!((h.mach.unwrap() - 0.42).abs() < 0.001);
            assert_eq!(h.baro_rate_fpm, Some(-1920));
            assert_eq!(h.ins_rate_fpm, Some(-1920));
        }
        c => panic!("{c:?}"),
    }
    assert_eq!(commb::decode(super::msg::Mb(0), none), CommB::Empty);
}

#[test]
fn mach_and_ias_agree_in_the_standard_atmosphere() {
    // M 0.78 at FL350 is about 264 kt CAS; at sea level Mach 0.3 is ~198 kt.
    assert!((commb::mach_to_cas_kt(0.78, 35_000.0) - 264.0).abs() < 2.0);
    assert!((commb::mach_to_cas_kt(0.3, 0.0) - 198.5).abs() < 1.0);
}

#[test]
fn decodes_target_state_and_status() {
    // The Riddle / pyModeS: selected altitude 16992 ft (MCP), 1012.8 hPa,
    // heading 66.8°.
    let s = msg("8DA05629EA21485CBF3F8CADAEEB").target_state().unwrap();
    assert_eq!(s.selected_alt_ft, Some(16_992));
    assert!(!s.fms);
    assert!((s.baro_hpa.unwrap() - 1012.8).abs() < 0.01);
    assert!((s.heading_deg.unwrap() - 66.8).abs() < 0.05, "{s:?}");
}

#[test]
fn decodes_altitude_and_global_cpr_position() {
    let (e, o) = (msg(EVEN), msg(ODD));
    assert_eq!(e.altitude_ft(), Some(38_000));
    let (ce, co) = (e.cpr().unwrap(), o.cpr().unwrap());
    assert!(!ce.odd && co.odd && !ce.surface);
    // "The 1090 MHz Riddle": the even frame is the newer one.
    let (lat, lon) = cpr_global(ce, co, false).unwrap();
    assert!((lat - 52.25720).abs() < 1e-4, "{lat}");
    assert!((lon - 3.91937).abs() < 1e-4, "{lon}");
    let (lat, _) = cpr_global(ce, co, true).unwrap();
    assert!((lat - 52.26578).abs() < 1e-4, "{lat}");
}

#[test]
fn decodes_local_cpr_positions() {
    // The Riddle, locally unambiguous decoding with reference (52.258, 3.918).
    let (lat, lon) = cpr_local(msg(EVEN).cpr().unwrap(), (52.258, 3.918));
    assert!((lat - 52.25720).abs() < 1e-4, "{lat}");
    assert!((lon - 3.91937).abs() < 1e-4, "{lon}");
    // The same frame read from 150 NM away still lands there.
    let (lat, lon) = cpr_local(msg(EVEN).cpr().unwrap(), (50.0, 2.0));
    assert!((lat - 52.25720).abs() < 1e-4 && (lon - 3.91937).abs() < 1e-4);
}

#[test]
fn decodes_surface_positions() {
    // The Riddle, surface position, receiver at (51.990, 4.375): an aircraft
    // taxiing at Schiphol (the book gives 52.32061, 4.73473 for the odd
    // frame; formula and book differ in the third decimal of longitude).
    let (e, o) = (
        msg("8C4841753AAB238733C8CD4020B1"),
        msg("8C4841753A9A153237AEF0F275BE"),
    );
    assert_eq!(e.tc(), 7);
    let (ce, co) = (e.cpr().unwrap(), o.cpr().unwrap());
    assert!(ce.surface && !ce.odd && co.odd);
    let rx = (51.990, 4.375);
    let (lat, lon) = cpr_global_surface(ce, co, true, rx).unwrap();
    assert!((lat - 52.32061).abs() < 2e-4, "{lat}");
    assert!((lon - 4.73473).abs() < 2e-3, "{lon}");
    // Local decoding of the same frame agrees with the global one.
    let (llat, llon) = cpr_local(co, rx);
    assert!((llat - lat).abs() < 1e-6 && (llon - lon).abs() < 1e-6);
    // From the far side of the reference quadrant, still the same place.
    let (glat, glon) = cpr_global_surface(ce, co, true, (50.0, 3.0)).unwrap();
    assert!((glat - lat).abs() < 1e-6 && (glon - lon).abs() < 1e-6);
    // The book: 17 kt, track 92.8°.
    let (gs, trk) = o.surface_movement();
    assert_eq!(gs, Some(17.0));
    assert_eq!(trk, Some(92.8125));
}

#[test]
fn decodes_velocities() {
    match msg(VELOCITY).velocity().unwrap() {
        Velocity::Ground {
            speed_kt,
            track_deg,
            vrate_fpm,
        } => {
            assert!((speed_kt - 159.2).abs() < 0.1, "{speed_kt}");
            assert!((track_deg - 182.88).abs() < 0.01, "{track_deg}");
            assert_eq!(vrate_fpm, Some(-832));
        }
        v => panic!("{v:?}"),
    }
    assert_eq!(msg(VELOCITY).geo_minus_baro_ft(), Some(550));
    match msg(AIRSPEED).velocity().unwrap() {
        Velocity::Air {
            speed_kt,
            true_airspeed,
            heading_deg,
            vrate_fpm,
        } => {
            assert_eq!(speed_kt, 375.0);
            assert!(true_airspeed);
            assert!((heading_deg.unwrap() - 243.98).abs() < 0.01);
            assert_eq!(vrate_fpm, Some(-2304));
        }
        v => panic!("{v:?}"),
    }
}

#[test]
fn nl_matches_the_table() {
    assert_eq!(nl(0.0), 59);
    assert_eq!(nl(52.2572), 36);
    assert_eq!(nl(-52.2572), 36);
    assert_eq!(nl(87.0), 2);
    assert_eq!(nl(89.0), 1);
}

#[test]
fn two_bit_syndromes_are_unambiguous() {
    let one = syndromes();
    let two = two_bit_syndromes(&one);
    // The Mode S code has distance 6 over 112 bits: every pair is unique.
    assert_eq!(two.len(), 107 * 106 / 2);
}

// ---- End to end -------------------------------------------------------------------

/// Small deterministic Gaussian noise (xorshift + Box-Muller).
pub(super) struct Noise(pub u64);
impl Noise {
    pub fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    pub fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// Something on the air.
#[derive(Clone)]
enum Tx {
    ModeS(Vec<u8>),
    /// 13 code bits (C1 first) and SPI, F1 starting `offset` µs late.
    ModeAc(u32, bool, f64),
}

/// A message with its parity computed: plain CRC (DF11/17/18) or the CRC
/// XOR the address (address/parity formats).
fn with_parity(body: &[u8], address: Option<u32>) -> Vec<u8> {
    let mut m = body.to_vec();
    m.extend([0, 0, 0]);
    let p = super::msg::residual(&m) ^ address.unwrap_or(0);
    let n = m.len();
    m[n - 3] = (p >> 16) as u8;
    m[n - 2] = (p >> 8) as u8;
    m[n - 1] = p as u8;
    m
}

/// The envelope of `tx` (each after `gap` µs of silence) at 2 MS/s, as a
/// receiver would see it at amplitude `amp` over complex noise of standard
/// deviation `sigma` per axis. Pulses are integrated over each 0.5 µs sample
/// (a pulse that straddles two samples lands half in each).
fn modulate(tx: &[Tx], gap: f64, amp: f64, sigma: f64, seed: u64) -> Vec<f32> {
    modulate_at(tx, gap, amp, sigma, seed, 2.0, 0.0)
}

/// [`modulate`] at `s` samples per µs, each transmission starting
/// `phase_step` µs later (modulo one sample) than the last relative to the
/// sample clock.
fn modulate_at(
    tx: &[Tx],
    gap: f64,
    amp: f64,
    sigma: f64,
    seed: u64,
    s: f64,
    phase_step: f64,
) -> Vec<f32> {
    let mut pulses: Vec<(f64, f64)> = Vec::new(); // (start µs, length µs)
    let mut t = 0.0;
    for (k, x) in tx.iter().enumerate() {
        t += gap + (k as f64 * phase_step) % (1.0 / s);
        match x {
            Tx::ModeS(bytes) => {
                for p in [0.0, 1.0, 3.5, 4.5] {
                    pulses.push((t + p, 0.5));
                }
                for i in 0..bytes.len() * 8 {
                    let one = bytes[i / 8] & (0x80 >> (i % 8)) != 0;
                    let at = t + 8.0 + i as f64 + if one { 0.0 } else { 0.5 };
                    pulses.push((at, 0.5));
                }
                t += 8.0 + bytes.len() as f64 * 8.0;
            }
            Tx::ModeAc(code, spi, offset) => {
                let f1 = t + offset;
                pulses.push((f1, 0.45));
                pulses.push((f1 + 20.3, 0.45));
                for k in 1..=13 {
                    if code & (1 << (13 - k)) != 0 {
                        pulses.push((f1 + 1.45 * k as f64, 0.45));
                    }
                }
                if *spi {
                    pulses.push((f1 + 24.65, 0.45));
                }
                t += 30.0;
            }
        }
    }
    t += gap;
    let n = (t * s).ceil() as usize;
    let mut clean = vec![0.0f64; n];
    for (start, len) in pulses {
        // Sample k covers [k/s, (k+1)/s) µs.
        let (a, b) = (start * s, (start + len) * s);
        for (k, c) in clean
            .iter_mut()
            .enumerate()
            .take(b.ceil() as usize)
            .skip(a.floor() as usize)
        {
            let lo = a.max(k as f64);
            let hi = b.min(k as f64 + 1.0);
            *c += amp * (hi - lo).max(0.0);
        }
    }
    let mut noise = Noise(seed);
    clean
        .iter()
        .map(|&s| {
            let (i, q) = (s + sigma * noise.gauss(), sigma * noise.gauss());
            i.hypot(q) as f32
        })
        .collect()
}

fn decode_with(
    input: Vec<f32>,
    params: serde_json::Value,
    kind: BufferKind,
    cap: usize,
) -> Vec<String> {
    decode_at(input, params, kind, cap, RATE)
}

fn decode_at(
    input: Vec<f32>,
    params: serde_json::Value,
    kind: BufferKind,
    cap: usize,
    rate: f64,
) -> Vec<String> {
    let reg = crate::registry();
    let mut fg = Flowgraph::new()
        .with_buffer_capacity(cap)
        .with_buffer_kind(kind);
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(rate)),
    );
    let dec = fg.add(
        "adsb",
        reg.build("adsb_decoder", params.as_object().unwrap())
            .unwrap(),
    );
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, dec, 0).unwrap();
    fg.connect(dec, 0, sink, 0).unwrap();
    let edges = fg.configure().unwrap();
    let m = edges.iter().find(|e| e.from == "adsb.out").unwrap().meta;
    assert_eq!(m.sample_rate, None, "text has no sample rate");
    fg.start().unwrap().wait().unwrap();
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    text.lines().map(str::to_owned).collect()
}

fn decode(input: Vec<f32>, buffers: BufferKind, cap: usize) -> Vec<String> {
    decode_with(input, json!({}), buffers, cap)
}

fn frames(hex: &[&str]) -> Vec<Tx> {
    hex.iter()
        .map(|h| Tx::ModeS(msg(h).data().to_vec()))
        .collect()
}

#[test]
fn decodes_modulated_messages_end_to_end() {
    let tx = frames(&[KLM, ODD, EVEN, VELOCITY]);
    // 20 dB SNR (amplitude 1, noise 0.1 per axis).
    let input = modulate(&tx, 1500.0, 1.0, 0.1, 7);
    for (kind, cap) in [(BufferKind::Auto, 64 * 1024), (BufferKind::Plain, 100)] {
        let lines = decode(input.clone(), kind, cap);
        assert_eq!(lines.len(), 4, "{lines:#?}");
        assert!(lines[0].starts_with("t=0.001500 icao=4840D6 df=17 tc=4 type=ident"));
        assert!(
            lines[0].contains("callsign=KLM1023 category=A0"),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(&format!("raw={KLM}")), "{}", lines[0]);
        assert!(lines[1].contains("icao=40621D") && lines[1].contains("cpr=odd"));
        assert!(lines[1].contains("alt_ft=38000"), "{}", lines[1]);
        assert!(
            lines[2].contains("lat=52.25720 lon=3.91937 pos=global"),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].contains("type=velocity gs_kt=159.2 track_deg=182.9 vrate_fpm=-832"),
            "{}",
            lines[3]
        );
    }
}

#[test]
fn local_cpr_needs_a_receiver_position() {
    let input = modulate(&frames(&[EVEN]), 500.0, 1.0, 0.05, 5);
    let lines = decode(input.clone(), BufferKind::Auto, 64 * 1024);
    assert!(lines[0].contains("cpr=even"), "{lines:?}");
    let lines = decode_with(
        input,
        json!({"receiver": "52.0, 4.0"}),
        BufferKind::Auto,
        64 * 1024,
    );
    assert!(
        lines[0].contains("lat=52.25720 lon=3.91937 pos=local"),
        "{lines:?}"
    );
}

#[test]
fn address_parity_replies_need_a_known_address() {
    let icao = 0x4840D6;
    let df4 = with_parity(&[0x20, 0x00, 0x03, 0xB0], Some(icao)); // Q=1, 5000 ft
    let df5 = with_parity(&[0x28, 0x00, 0x0E, 0x6D], Some(icao));
    let df11 = with_parity(&[0x5D, 0x48, 0x40, 0xD6], None);
    // A DF20 carrying BDS 2,0 and a DF21 carrying BDS 6,0 from the Riddle.
    let mut b20 = vec![0xA0, 0x00, 0x03, 0xB0];
    b20.extend(
        msg("A000083E202CC371C31DE0AA1CCF")
            .me_field()
            .0
            .to_be_bytes()[1..]
            .iter(),
    );
    let df20 = with_parity(&b20, Some(icao));
    let mut b21 = vec![0xA8, 0x00, 0x0E, 0x6D];
    b21.extend(
        msg("A00004128F39F91A7E27C46ADC21")
            .me_field()
            .0
            .to_be_bytes()[1..]
            .iter(),
    );
    let df21 = with_parity(&b21, Some(icao));
    let tx: Vec<Tx> = [&df4, &df11, &df4, &df5, &df20, &df21]
        .into_iter()
        .map(|m| Tx::ModeS(m.clone()))
        .collect();
    let lines = decode(
        modulate(&tx, 300.0, 1.0, 0.05, 9),
        BufferKind::Auto,
        64 * 1024,
    );
    // The first DF4 comes before the address is known and is dropped.
    assert_eq!(lines.len(), 5, "{lines:#?}");
    assert!(
        lines[0].contains("df=11 type=all_call ca=5"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("icao=4840D6 df=4 type=altitude alt_ft=5000"),
        "{}",
        lines[1]
    );
    let sq = id13_squawk(0x0E6D);
    assert!(
        lines[2].contains(&format!("df=5 type=identity squawk={sq:04X}")),
        "{}",
        lines[2]
    );
    assert!(lines[3].contains("df=20 type=commb"), "{}", lines[3]);
    assert!(
        lines[3].contains("bds=2,0 callsign=KLM1017"),
        "{}",
        lines[3]
    );
    assert!(
        lines[4].contains("bds=6,0 heading_deg=42.7 ias_kt=252"),
        "{}",
        lines[4]
    );
}

#[test]
fn repairs_one_bit_and_drops_two() {
    let mut one = msg(KLM).bytes.to_vec();
    one[7] ^= 0x04; // bit 61
    let mut two = msg(KLM).bytes.to_vec();
    two[7] ^= 0x04;
    two[9] ^= 0x01;
    let tx = [Tx::ModeS(one), Tx::ModeS(two.clone())];
    let input = modulate(&tx, 250.0, 1.0, 0.02, 3);
    let lines = decode(input, BufferKind::Auto, 64 * 1024);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].contains("callsign=KLM1023 category=A0 fixed_bit=61"),
        "{}",
        lines[0]
    );
    // With 2-bit repair, but only once the address is known.
    let tx = [
        Tx::ModeS(two.clone()),
        Tx::ModeS(msg(KLM).data().to_vec()),
        Tx::ModeS(two),
    ];
    let input = modulate(&tx, 250.0, 1.0, 0.02, 3);
    let lines = decode_with(
        input,
        json!({"fix_two_bits": true}),
        BufferKind::Auto,
        64 * 1024,
    );
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[1].contains("fixed_bits=61,79"), "{}", lines[1]);
}

#[test]
fn extended_squitter_only_mode_ignores_other_formats() {
    let df11 = with_parity(&[0x5D, 0x48, 0x40, 0xD6], None);
    let tx = vec![Tx::ModeS(df11), Tx::ModeS(msg(KLM).data().to_vec())];
    let input = modulate(&tx, 300.0, 1.0, 0.05, 2);
    let lines = decode_with(
        input,
        json!({"formats": "extended_squitter"}),
        BufferKind::Auto,
        64 * 1024,
    );
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains("df=17"));
}

#[test]
fn decodes_mode_ac_replies_when_asked() {
    // Squawk 7000 (A4 A2 A1 = 7): C1 A1 C2 A2 C4 A4 X B1 D1 B2 D2 B4 D4.
    let sq7000 = 0b0101010000000;
    let alt = gillham_encode(12_300);
    let alt_bits = {
        // Squawk layout → time order.
        let order = [
            0x0010, 0x1000, 0x0020, 0x2000, 0x0040, 0x4000, 0, 0x0100, 0x0001, 0x0200, 0x0002,
            0x0400, 0x0004,
        ];
        order
            .iter()
            .fold(0u32, |acc, &b| acc << 1 | u32::from(b != 0 && alt & b != 0))
    };
    let tx = vec![
        Tx::ModeAc(sq7000, false, 0.0),
        Tx::ModeAc(alt_bits, true, 0.27),
        Tx::ModeAc(sq7000, false, 0.13),
    ];
    let input = modulate_at(&tx, 200.0, 1.0, 0.05, 4, 2.0, 0.17);
    assert!(decode(input.clone(), BufferKind::Auto, 64 * 1024).is_empty());
    let lines = decode_with(input, json!({"mode_ac": true}), BufferKind::Auto, 64 * 1024);
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(
        lines[0].contains("type=mode_ac squawk=7000"),
        "{}",
        lines[0]
    );
    assert!(lines[1].contains("alt_ft=12300 spi=1"), "{}", lines[1]);
    assert!(lines[2].contains("squawk=7000"), "{}", lines[2]);
}

#[test]
fn decodes_at_any_phase_and_rate() {
    let tx: Vec<Tx> = (0..40)
        .map(|k| Tx::ModeS(msg([KLM, VELOCITY, AIRSPEED, EVEN][k % 4]).data().to_vec()))
        .collect();
    // Noiseless, every phase decodes at every rate, 2 MS/s included (where
    // a frame half a sample off needs the decision feedback in `bits`).
    for (rate, step) in [(2e6, 0.0613), (2.4e6, 0.0517), (3e6, 0.0371), (4e6, 0.0277)] {
        let input = modulate_at(&tx, 300.0, 1.0, 1e-4, 21, rate / 1e6, step);
        let lines = decode_at(input, json!({}), BufferKind::Auto, 1 << 16, rate);
        assert_eq!(lines.len(), 40, "{rate}: {lines:#?}");
    }
    // At 14 dB SNR, from 2.4 MS/s up nearly all do. (At 2 MS/s about half:
    // see `sensitivity`.)
    let sigma = (1.0f64 / 10f64.powf(1.4) / 2.0).sqrt();
    for (rate, step) in [(2.4e6, 0.0517), (3e6, 0.0371), (4e6, 0.0277)] {
        let input = modulate_at(&tx, 300.0, 1.0, sigma, 21, rate / 1e6, step);
        let lines = decode_at(input, json!({}), BufferKind::Auto, 1 << 16, rate);
        assert!(lines.len() >= 37, "{rate}: {}", lines.len());
    }
}

#[test]
fn noise_alone_decodes_nothing() {
    let input = modulate(&[], 200_000.0, 1.0, 0.3, 11);
    assert!(decode(input.clone(), BufferKind::Auto, 64 * 1024).is_empty());
    let lines = decode_with(
        input,
        json!({"mode_ac": true, "fix_two_bits": true}),
        BufferKind::Auto,
        64 * 1024,
    );
    assert!(lines.is_empty(), "{lines:#?}");
}

#[test]
fn rejects_bad_parameters_and_the_wrong_sample_rate() {
    let reg = crate::registry();
    let err = reg
        .build(
            "adsb_decoder",
            json!({"receiver": "north"}).as_object().unwrap(),
        )
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("lat, lon"), "{err}");
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(1.5e6)),
    );
    let dec = fg.add(
        "adsb",
        reg.build("adsb_decoder", json!({}).as_object().unwrap())
            .unwrap(),
    );
    fg.connect(src, 0, dec, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(
        err.contains("needs at least 2 MS/s") && err.contains("1.5 MS/s"),
        "{err}"
    );
}

/// Messages decoded out of 1000 DF17 frames at each SNR, at sample rates
/// of 2, 2.4 and 4 MS/s with the frames at every phase relative to the
/// sample clock. Run with
/// `cargo test -p foxygnal-blocks --release sensitivity -- --ignored --nocapture`.
#[test]
#[ignore]
fn sensitivity() {
    let tx: Vec<Tx> = (0..1000)
        .map(|k| Tx::ModeS(msg([KLM, VELOCITY, AIRSPEED, EVEN][k % 4]).data().to_vec()))
        .collect();
    for rate in [2e6, 2.4e6, 4e6] {
        for snr_db in [6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 20.0] {
            // SNR as pulse power over noise power (both axes).
            let sigma = (1.0f64 / 10f64.powf(snr_db / 10.0) / 2.0).sqrt();
            let input = modulate_at(&tx, 200.0, 1.0, sigma, 17, rate / 1e6, 0.0731);
            let run = |p| decode_at(input.clone(), p, BufferKind::Auto, 1 << 16, rate).len();
            println!(
                "{:.1} MS/s, SNR {snr_db:>4} dB: no repair {:>4}, 1-bit {:>4}, 2-bit {:>4} / 1000",
                rate / 1e6,
                run(json!({"fix_errors": false})),
                run(json!({})),
                run(json!({"fix_two_bits": true}))
            );
        }
    }
}
