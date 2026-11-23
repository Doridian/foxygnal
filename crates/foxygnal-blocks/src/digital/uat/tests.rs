//! An independent UAT encoder (payload packing, Reed-Solomon, interleaving,
//! CPFSK with noise and a frequency offset) run through `fm_demod` and the
//! decoder in a flowgraph.

use foxygnal_core::{Complex32, Flowgraph, StreamMeta};
use serde_json::json;

use super::*;
use crate::testing::{VectorSink, VectorSource};

// ---- Payloads ------------------------------------------------------------------

/// Write `value` into `len` bits starting at bit `first` (0 = MSB of byte 0).
fn put(buf: &mut [u8], first: usize, len: usize, value: u32) {
    for i in 0..len {
        let bit = (value >> (len - 1 - i)) & 1;
        let at = first + i;
        if bit == 1 {
            buf[at / 8] |= 0x80 >> (at % 8);
        } else {
            buf[at / 8] &= !(0x80 >> (at % 8));
        }
    }
}

/// Latitude: 23 bits of 360/2^24 degrees, two's complement.
fn lat23(deg: f64) -> u32 {
    ((deg.rem_euclid(180.0) / 360.0 * 16_777_216.0).round() as u32) & 0x7F_FFFF
}

/// 24-bit angular weighted binary.
fn awb(deg: f64) -> u32 {
    ((deg.rem_euclid(360.0) / 360.0 * 16_777_216.0).round() as u32) & 0xFF_FFFF
}

struct Aircraft {
    mdb: u32,
    address: u32,
    lat: f64,
    lon: f64,
    alt_ft: i32,
    north_kt: i32,
    east_kt: i32,
    vrate_fpm: i32,
    callsign: &'static str,
    category: u32,
}

const BASE40: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ  ..";

/// An ADS-B payload as DO-282B lays it out (written from the standard's
/// field table, independently of the decoder).
fn adsb_payload(a: &Aircraft) -> Vec<u8> {
    let long = a.mdb != 0;
    let mut p = vec![0u8; if long { 34 } else { 18 }];
    put(&mut p, 0, 5, a.mdb);
    put(&mut p, 5, 3, 0); // ICAO address
    put(&mut p, 8, 24, a.address);
    put(&mut p, 32, 23, lat23(a.lat));
    put(&mut p, 55, 24, awb(a.lon));
    put(&mut p, 79, 1, 0); // altitude type: barometric
    put(&mut p, 80, 12, ((a.alt_ft + 1000) / 25 + 1) as u32);
    put(&mut p, 92, 4, 8); // NIC
    put(&mut p, 96, 2, 0); // airborne subsonic
    let vel = |v: i32| (u32::from(v < 0) << 10) | (v.unsigned_abs() + 1);
    put(&mut p, 99, 11, vel(a.north_kt));
    put(&mut p, 110, 11, vel(a.east_kt));
    put(&mut p, 121, 1, 1); // vertical rate source: barometric
    put(&mut p, 122, 1, u32::from(a.vrate_fpm < 0));
    put(&mut p, 123, 9, a.vrate_fpm.unsigned_abs() / 64 + 1);
    if a.mdb == 1 {
        let cs: Vec<u32> = format!("{:<8}", a.callsign)
            .bytes()
            .map(|c| BASE40.bytes().position(|b| b == c).unwrap() as u32)
            .collect();
        let w1 = a.category * 1600 + cs[0] * 40 + cs[1];
        let w2 = cs[2] * 1600 + cs[3] * 40 + cs[4];
        let w3 = cs[5] * 1600 + cs[6] * 40 + cs[7];
        put(&mut p, 136, 16, w1);
        put(&mut p, 152, 16, w2);
        put(&mut p, 168, 16, w3);
        put(&mut p, 184, 3, 0); // no emergency
        put(&mut p, 187, 3, 2); // UAT version 2
        put(&mut p, 190, 2, 3); // SIL
        put(&mut p, 200, 4, 9); // NACp
        put(&mut p, 214, 1, 1); // call sign, not squawk
        // Auxiliary state vector: geometric altitude 150 ft above baro.
        put(&mut p, 232, 12, ((a.alt_ft + 150 + 1000) / 25 + 1) as u32);
    }
    p
}

/// DLAC-encode text (4 characters in 3 bytes).
fn dlac_encode(text: &str) -> Vec<u8> {
    const ALPHA: &[u8] =
        b"\x03ABCDEFGHIJKLMNOPQRSTUVWXYZ\x1A\t\x1E\n| !\"#$%&'()*+,-./0123456789:;<=>?";
    let codes: Vec<u32> = text
        .bytes()
        .map(|c| ALPHA.iter().position(|&a| a == c).unwrap() as u32)
        .collect();
    let mut out = vec![0u8; (codes.len() * 6).div_ceil(8)];
    for (i, &c) in codes.iter().enumerate() {
        put(&mut out, i * 6, 6, c);
    }
    out
}

/// A FIS-B APDU: product, hours and minutes, payload.
fn apdu(product: u32, hour: u32, minute: u32, payload: &[u8]) -> Vec<u8> {
    let mut a = vec![0u8; 4];
    put(&mut a, 0, 3, 0); // A, G, P flags
    put(&mut a, 3, 11, product);
    put(&mut a, 14, 1, 0); // not segmented
    put(&mut a, 15, 2, 0); // time option 0: hours, minutes
    put(&mut a, 17, 5, hour);
    put(&mut a, 22, 6, minute);
    a.extend_from_slice(payload);
    a
}

/// An information frame: 9-bit length, 4 reserved bits... and the type.
fn info_frame(kind: u32, body: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; 2];
    put(&mut f, 0, 9, body.len() as u32);
    put(&mut f, 12, 4, kind);
    f.extend_from_slice(body);
    f
}

/// A 432-byte uplink payload from a ground station at (`lat`, `lon`).
fn uplink_payload(lat: f64, lon: f64, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut p = vec![0u8; 432];
    put(&mut p, 0, 23, lat23(lat));
    put(&mut p, 23, 24, awb(lon));
    put(&mut p, 47, 1, 1); // position valid
    put(&mut p, 48, 1, 1); // UTC coupled
    put(&mut p, 50, 1, 1); // application data valid
    put(&mut p, 51, 5, 7); // slot
    put(&mut p, 56, 4, 3); // TIS-B site
    let mut at = 8;
    for f in frames {
        p[at..at + f.len()].copy_from_slice(f);
        at += f.len();
    }
    p
}

// ---- Channel coding and modulation --------------------------------------------------

fn adsb_frame_bits(payload: &[u8]) -> Vec<u8> {
    let (code, n) = if payload.len() == 34 {
        (&rs::LONG, LONG_BYTES)
    } else {
        (&rs::SHORT, SHORT_BYTES)
    };
    let mut cw = payload.to_vec();
    cw.resize(n, 0);
    code.encode(&mut cw);
    with_sync(ADSB_SYNC, &cw)
}

fn uplink_frame_bits(payload: &[u8]) -> Vec<u8> {
    let mut raw = vec![0u8; UPLINK_BYTES];
    for block in 0..6 {
        let mut cw = payload[block * 72..(block + 1) * 72].to_vec();
        cw.resize(UPLINK_BLOCK, 0);
        rs::UPLINK.encode(&mut cw);
        for (i, &b) in cw.iter().enumerate() {
            raw[i * 6 + block] = b;
        }
    }
    with_sync(UPLINK_SYNC, &raw)
}

fn with_sync(sync: u64, bytes: &[u8]) -> Vec<u8> {
    let mut bits: Vec<u8> = (0..36).map(|k| (sync >> (35 - k) & 1) as u8).collect();
    for b in bytes {
        bits.extend((0..8).map(|i| (b >> (7 - i)) & 1));
    }
    bits
}

/// Small deterministic Gaussian noise (xorshift + Box-Muller).
struct Noise(u64);
impl Noise {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// CPFSK (±312.5 kHz, 1.041667 Mbit/s) at `rate`, each frame after `gap` µs
/// of noise, `offset_hz` off frequency, unit amplitude over complex noise of
/// `sigma` per axis. Frames start at a different fraction of a sample each.
fn modulate(
    frames: &[Vec<u8>],
    rate: f64,
    gap: f64,
    offset_hz: f64,
    sigma: f64,
    seed: u64,
) -> Vec<Complex32> {
    let dev = 312_500.0;
    let mut out = Vec::new();
    let mut noise = Noise(seed);
    let mut phase = 0.0f64;
    let mut t = 0.0f64; // µs
    let mut emit = |out: &mut Vec<Complex32>, freq: f64, on: bool, noise: &mut Noise| {
        phase += std::f64::consts::TAU * (freq + offset_hz) / rate;
        let a = if on { 1.0 } else { 0.0 };
        out.push(Complex32::new(
            (a * phase.cos() + sigma * noise.gauss()) as f32,
            (a * phase.sin() + sigma * noise.gauss()) as f32,
        ));
    };
    let us_per_sample = 1e6 / rate;
    let bit_us = 1e6 / BIT_RATE;
    for (k, bits) in frames.iter().enumerate() {
        let start = t + gap + k as f64 * 0.137;
        while t < start {
            emit(&mut out, 0.0, false, &mut noise);
            t += us_per_sample;
        }
        let end = start + bits.len() as f64 * bit_us;
        while t < end {
            let i = ((t - start) / bit_us) as usize;
            let f = if bits[i.min(bits.len() - 1)] == 1 {
                dev
            } else {
                -dev
            };
            emit(&mut out, f, true, &mut noise);
            t += us_per_sample;
        }
    }
    let end = t + gap;
    while t < end {
        emit(&mut out, 0.0, false, &mut noise);
        t += us_per_sample;
    }
    out
}

fn decode(input: Vec<Complex32>, rate: f64) -> Vec<String> {
    let reg = crate::registry();
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::instance_with_meta(input, StreamMeta::with_rate(rate)),
    );
    let fm = fg.add(
        "fm",
        reg.build(
            "fm_demod",
            json!({"deviation": 312_500.0, "deemphasis": "off"})
                .as_object()
                .unwrap(),
        )
        .unwrap(),
    );
    let dec = fg.add(
        "uat",
        reg.build("uat_decoder", json!({}).as_object().unwrap())
            .unwrap(),
    );
    let (sink, out) = VectorSink::<u8>::instance();
    let sink = fg.add("sink", sink);
    fg.connect(src, 0, fm, 0).unwrap();
    fg.connect(fm, 0, dec, 0).unwrap();
    fg.connect(dec, 0, sink, 0).unwrap();
    fg.configure().unwrap();
    fg.start().unwrap().wait().unwrap();
    let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
    text.lines().map(str::to_owned).collect()
}

fn long_aircraft() -> Aircraft {
    Aircraft {
        mdb: 1,
        address: 0xA1B2C3,
        lat: 47.4502,
        lon: -122.3088,
        alt_ft: 4500,
        north_kt: -120,
        east_kt: 85,
        vrate_fpm: -640,
        callsign: "N123AB",
        category: 1,
    }
}

fn short_aircraft() -> Aircraft {
    Aircraft {
        mdb: 0,
        address: 0xABCDEF,
        lat: -33.9,
        lon: 151.2,
        alt_ft: 12_025,
        north_kt: 3,
        east_kt: -250,
        vrate_fpm: 1024,
        callsign: "",
        category: 0,
    }
}

const METAR: &str = "METAR KSEA 231953Z 18012KT 10SM FEW040 BKN250 09/03 A3012 RMK AO2=";
const TAF: &str = "TAF KBFI 231730Z 2318/2418 19010KT P6SM SCT050=";

fn test_uplink() -> Vec<u8> {
    let text = format!("{METAR}\x1E{TAF}\x1E\x03");
    let f1 = info_frame(0, &apdu(413, 19, 53, &dlac_encode(&text)));
    let f2 = info_frame(0, &apdu(63, 19, 50, &[0x55; 40]));
    uplink_payload(47.45, -122.31, &[f1, f2])
}

#[test]
fn decodes_adsb_and_uplink_frames_end_to_end() {
    let frames = vec![
        adsb_frame_bits(&adsb_payload(&long_aircraft())),
        uplink_frame_bits(&test_uplink()),
        adsb_frame_bits(&adsb_payload(&short_aircraft())),
    ];
    for rate in [MIN_RATE, 3e6, 4_166_666.666_666_7] {
        // 20 dB SNR, 25 kHz off frequency.
        let input = modulate(&frames, rate, 300.0, 25_000.0, 0.07, 3);
        let lines = decode(input, rate);
        assert_eq!(lines.len(), 5, "{rate}: {lines:#?}");
        let l = &lines[0];
        assert!(l.contains("type=adsb mdb=1 addr=A1B2C3 aq=0"), "{l}");
        assert!(
            l.contains("lat=47.4502") && l.contains("lon=-122.3088"),
            "{l}"
        );
        assert!(l.contains("alt_ft=4500 alt_type=baro"), "{l}");
        assert!(l.contains("ns_kt=-120 ew_kt=85 gs_kt=147"), "{l}");
        assert!(l.contains("vrate_fpm=-640 vrate_src=baro"), "{l}");
        assert!(l.contains("callsign=N123AB category=1"), "{l}");
        assert!(l.contains("uat_version=2 sil=3 nacp=9"), "{l}");
        assert!(l.contains("alt2_ft=4650 alt2_type=geo"), "{l}");
        let u = &lines[1];
        assert!(
            u.contains("type=uplink lat=47.4") && u.contains("lon=-122.3"),
            "{u}"
        );
        assert!(u.contains("slot=7 tisb_site=3 utc=1"), "{u}");
        assert!(u.contains("apdus=2 text=1 nexrad=1 other=0"), "{u}");
        assert!(
            lines[2].contains(&format!(
                "type=fisb product=413 time=19:53 text=\"{METAR}\""
            )),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].contains(&format!("text=\"{TAF}\"")),
            "{}",
            lines[3]
        );
        let s = &lines[4];
        assert!(s.contains("type=adsb mdb=0 addr=ABCDEF"), "{s}");
        assert!(s.contains("lat=-33.9") && s.contains("lon=151.2"), "{s}");
        assert!(
            s.contains("alt_ft=12025") && s.contains("vrate_fpm=1024"),
            "{s}"
        );
        assert!(s.contains("ns_kt=3 ew_kt=-250"), "{s}");
    }
}

#[test]
fn corrects_byte_errors() {
    let payload = adsb_payload(&long_aircraft());
    let mut bits = adsb_frame_bits(&payload);
    // Flip bits in six different bytes after the sync word.
    for b in [0, 9, 17, 30, 41, 47] {
        bits[36 + 8 * b + 3] ^= 1;
    }
    let input = modulate(&[bits], MIN_RATE, 200.0, 0.0, 0.02, 5);
    let lines = decode(input, MIN_RATE);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(lines[0].contains("rs_errors=6"), "{}", lines[0]);
}

#[test]
fn noise_alone_decodes_nothing() {
    let input = modulate(&[], MIN_RATE, 200_000.0, 0.0, 0.5, 9);
    assert!(decode(input, MIN_RATE).is_empty());
}

#[test]
fn rejects_too_low_a_sample_rate() {
    let reg = crate::registry();
    let mut fg = Flowgraph::new();
    let src = fg.add(
        "src",
        VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(2e6)),
    );
    let dec = fg.add(
        "uat",
        reg.build("uat_decoder", json!({}).as_object().unwrap())
            .unwrap(),
    );
    fg.connect(src, 0, dec, 0).unwrap();
    let err = fg.configure().unwrap_err().to_string();
    assert!(err.contains("2.083 MS/s"), "{err}");
}

#[test]
fn dlac_round_trips() {
    let mut out = Vec::new();
    frame::dlac(&dlac_encode("KSEA 18012KT\x03"), &mut out);
    assert_eq!(out, b"KSEA 18012KT");
}

/// Frames decoded out of 200 long ADS-B frames at each SNR. Run with
/// `cargo test -p foxygnal-blocks --release uat_sensitivity -- --ignored --nocapture`.
#[test]
#[ignore]
fn uat_sensitivity() {
    let frames: Vec<_> = (0..200)
        .map(|_| adsb_frame_bits(&adsb_payload(&long_aircraft())))
        .collect();
    let up: Vec<_> = (0..50).map(|_| uplink_frame_bits(&test_uplink())).collect();
    for snr_db in [4.0, 6.0, 8.0, 10.0, 12.0, 14.0] {
        // SNR as carrier power over the noise power in the sample bandwidth.
        let sigma = (10f64.powf(-snr_db / 10.0) / 2.0).sqrt();
        let n = decode(
            modulate(&frames, MIN_RATE, 100.0, 10_000.0, sigma, 13),
            MIN_RATE,
        )
        .len();
        let u = decode(
            modulate(&up, MIN_RATE, 100.0, 10_000.0, sigma, 14),
            MIN_RATE,
        )
        .iter()
        .filter(|l| l.contains("type=uplink"))
        .count();
        println!("SNR {snr_db:>4} dB: ADS-B {n:>3} / 200, uplink {u:>2} / 50");
    }
}

/// Write a test signal as interleaved f32 I/Q (for checking against
/// dump978-fa: `dump978-fa --file FILE --format CF32H --raw-stdout`).
#[test]
#[ignore]
fn write_test_signal() {
    let frames = vec![
        adsb_frame_bits(&adsb_payload(&long_aircraft())),
        uplink_frame_bits(&test_uplink()),
        adsb_frame_bits(&adsb_payload(&short_aircraft())),
    ];
    let input = modulate(&frames, MIN_RATE, 300.0, 25_000.0, 0.07, 3);
    let path = std::env::var("UAT_TEST_SIGNAL").unwrap_or_else(|_| "uat-test.cf32".into());
    let bytes: Vec<u8> = input
        .iter()
        .flat_map(|c| [c.re.to_le_bytes(), c.im.to_le_bytes()].concat())
        .collect();
    std::fs::write(path, bytes).unwrap();
}
