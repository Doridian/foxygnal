//! `ism_decoder` devices: TPMS (Schrader, Toyota, Ford, Citroen), security
//! sensors (Honeywell / 2Gig, Interlogix), EV1527 / PT2262 remotes and ERT
//! meters (SCM, SCM+, IDM). Each has an independent encoder here: bytes and
//! checks computed bit by bit, line-coded, modulated with noise, an offset
//! and ±10 % timing jitter.

mod ism_common;

use ism_common::*;
use serde_json::json;

// ---- Checks (bit-serial polynomial division, independent of the decoder) ----

/// Remainder of `bits` (MSB first) × x^width divided by `poly` (with its
/// top term implied), after preloading `init`.
fn crc_bits(bits: &[u8], width: u32, poly: u32, init: u32) -> u32 {
    let top = 1u32 << (width - 1);
    let mask = (top << 1).wrapping_sub(1);
    let mut reg = init & mask;
    for &b in bits {
        let fb = (reg & top != 0) as u32 ^ u32::from(b);
        reg = (reg << 1) & mask;
        if fb == 1 {
            reg ^= poly;
        }
    }
    reg
}

fn crc8(bytes: &[u8], poly: u8, init: u8) -> u8 {
    crc_bits(
        &bits_of(bytes, bytes.len() * 8),
        8,
        poly.into(),
        init.into(),
    ) as u8
}

fn crc16(bytes: &[u8], poly: u16, init: u16) -> u16 {
    crc_bits(
        &bits_of(bytes, bytes.len() * 8),
        16,
        poly.into(),
        init.into(),
    ) as u16
}

fn not(bits: &[u8]) -> Vec<u8> {
    bits.iter().map(|b| b ^ 1).collect()
}

/// Leading-zero Manchester (rising mid-bit edge = 0) as OOK widths.
fn mc_zerobit(bits: &[u8], half: f64) -> Widths {
    runs(&manchester_levels(&not(bits)), half)
}

/// IEEE Manchester (0 → `10`, 1 → `01`) raw bits.
fn mc_ieee(bits: &[u8]) -> Vec<u8> {
    manchester_levels(bits)
}

fn repeat(w: &Widths, n: usize, gap: f64) -> Widths {
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend(w.iter().copied());
        pause(&mut out, gap);
    }
    out
}

fn one(lines: &[String], model: &str) -> String {
    let l = of_model(lines, model);
    assert_eq!(l.len(), 1, "{lines:?}");
    l[0].to_owned()
}

fn none_of(lines: &[String], models: &[&str]) {
    for m in models {
        assert!(of_model(lines, m).is_empty(), "{m} in {lines:?}");
    }
}

// ---- Schrader ----------------------------------------------------------------------

fn schrader(id: u32, flags: u8, pressure: u8, temp: u8, corrupt: bool) -> Widths {
    let mut b = [
        0xF0 | flags >> 4,
        (flags << 4) | (id >> 24) as u8 & 0x0F,
        (id >> 16) as u8,
        (id >> 8) as u8,
        id as u8,
        pressure,
        temp,
        0,
    ];
    b[7] = crc8(&b[..7], 0x07, 0xF0) ^ u8::from(corrupt);
    let mut bits = vec![0, 1, 1, 1];
    bits.extend(bits_of(&b, 64));
    repeat(&mc_zerobit(&bits, 120.0), 3, 5000.0)
}

#[test]
fn schrader_tpms() {
    for ch in channels() {
        let sig = ook(&schrader(0x0ABC_DEF1, 0x67, 88, 72, false), &ch);
        let lines = decode_one(sig, ch.rate, "schrader");
        check(
            &one(&lines, "Schrader"),
            &[
                ("type", "TPMS"),
                ("flags", "67"),
                ("id", "ABCDEF1"),
                ("pressure_kPa", "220.0"),
                ("temperature_C", "22"),
                ("mic", "CRC"),
            ],
        );
    }
    let ch = Channel::new(1e6);
    let lines = decode_one(
        ook(&schrader(0x0ABC_DEF1, 0x67, 88, 72, true), &ch),
        ch.rate,
        "schrader",
    );
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn schrader_eg53ma4() {
    let mut b = [0x01, 0x23, 0x45, 0x67, 0x9A, 0xBC, 0xDE, 96, 68, 0];
    b[9] = b[..9].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    let mut bytes = vec![0x00, 0x00, 0x00, 0x00, 0x3F];
    bytes.extend(b);
    let w = repeat(&mc_zerobit(&bits_of(&bytes, 120), 123.0), 2, 5000.0);
    for ch in channels() {
        let lines = decode_one(ook(&w, &ch), ch.rate, "schrader");
        check(
            &one(&lines, "Schrader-EG53MA4"),
            &[
                ("flags", "01234567"),
                ("id", "9ABCDE"),
                ("pressure_kPa", "240.0"),
                ("temperature_F", "68.0"),
                ("mic", "CHECKSUM"),
            ],
        );
    }
}

// ---- FSK TPMS ----------------------------------------------------------------------

/// Toyota: preamble, 72 bits differential Manchester, trailer, as NRZ.
fn toyota(id: u32, pressure: u8, temp: u8) -> Widths {
    let mut b = [0u8; 9];
    b[..4].copy_from_slice(&id.to_be_bytes());
    b[4] = 0x80 | pressure >> 1;
    b[5] = (pressure << 7) | temp >> 1;
    b[6] = (temp << 7) | 0x05;
    b[7] = !pressure;
    b[8] = crc8(&b[..8], 0x07, 0x80);
    let mut raw = vec![0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 1, 1];
    let mut level = 1u8;
    for d in bits_of(&b, 72) {
        level ^= 1; // a change at every bit boundary
        raw.push(level);
        if d == 0 {
            level ^= 1; // and mid-bit for a 0
        }
        raw.push(level);
    }
    raw.extend([1, 1, 1]);
    runs(&raw, 52.0)
}

#[test]
fn toyota_tpms() {
    for ch in channels() {
        let w = toyota(0x1234_ABCD, 156, 62);
        let lines = decode_one(fsk(&w, 25e3, &ch), ch.rate, "toyota");
        check(
            &one(&lines, "Toyota"),
            &[
                ("id", "1234abcd"),
                ("status", "133"),
                ("pressure_PSI", "32.00"),
                ("temperature_C", "22"),
                ("mic", "CRC"),
            ],
        );
    }
}

/// Ford / Citroen raw bits: `55 55 55 56`, then the data Manchester coded
/// with `10` = 1, then a trailer.
fn vdo(data: &[u8]) -> Widths {
    let mut raw = bits_of(&[0x55, 0x55, 0x55, 0x56], 32);
    raw.extend(not(&mc_ieee(&bits_of(data, data.len() * 8))));
    raw.extend(bits_of(&[0x7E], 8));
    runs(&raw, 52.0)
}

#[test]
fn ford_tpms() {
    let mut b = [0xDE, 0xAD, 0xBE, 0xEF, 130, 20 + 56, 0x45, 0];
    b[7] = b[..7].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    for ch in channels() {
        let lines = decode_one(fsk(&vdo(&b), 30e3, &ch), ch.rate, "ford");
        check(
            &one(&lines, "Ford"),
            &[
                ("id", "deadbeef"),
                ("pressure_PSI", "32.50"),
                ("temperature_C", "20.0"),
                ("moving", "1"),
                ("learn", "0"),
                ("code", "824c45"),
                ("mic", "CHECKSUM"),
            ],
        );
    }
}

#[test]
fn citroen_tpms() {
    let mut b = [0x0A, 0x8E, 0x01, 0x22, 0x33, 0xC2, 176, 70, 0x0F, 0];
    b[9] = b[1..9].iter().fold(0, |a, &x| a ^ x);
    for ch in channels() {
        let lines = decode_one(fsk(&vdo(&b), 30e3, &ch), ch.rate, "citroen");
        check(
            &one(&lines, "Citroen"),
            &[
                ("id", "8e012233"),
                ("state", "0a"),
                ("flags", "12"),
                ("repeat", "2"),
                ("pressure_kPa", "240"),
                ("temperature_C", "20"),
                ("maybe_battery", "15"),
                ("mic", "CHECKSUM"),
            ],
        );
    }
}

// ---- Security ----------------------------------------------------------------------

fn honeywell(channel: u8, id: u32, event: u8, corrupt: bool) -> Widths {
    let mut b = [
        channel << 4 | (id >> 16) as u8 & 0x0F,
        (id >> 8) as u8,
        id as u8,
        event,
        0,
        0,
    ];
    let poly = if [0x2, 0x4, 0x9, 0xA, 0xC].contains(&channel) {
        0x8050
    } else {
        0x8005
    };
    let crc = crc16(&b[..4], poly, 0) ^ u16::from(corrupt);
    b[4..].copy_from_slice(&crc.to_be_bytes());
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(b);
    let w = runs(&mc_ieee(&bits_of(&bytes, 64)), 136.0);
    repeat(&w, 3, 3000.0)
}

#[test]
fn honeywell_and_2gig() {
    for ch in channels() {
        let sig = concat(&[
            ook(&honeywell(0x8, 0x5_1234, 0x80, false), &ch),
            ook(
                &honeywell(0xA, 0xB_CDEF, 0x0C, false),
                &Channel { seed: 3, ..ch },
            ),
        ]);
        let lines = decode_one(sig, ch.rate, "honeywell");
        let l = of_model(&lines, "Honeywell-Security");
        assert_eq!(l.len(), 2, "{lines:?}");
        check(
            l[0],
            &[
                ("id", "332340"),
                ("channel", "8"),
                ("event", "80"),
                ("state", "open"),
                ("contact_open", "1"),
                ("battery_ok", "1"),
                ("mic", "CRC"),
            ],
        );
        check(
            l[1],
            &[
                ("id", "773615"),
                ("channel", "10"),
                ("state", "closed"),
                ("battery_ok", "0"),
                ("heartbeat", "1"),
            ],
        );
    }
    let ch = Channel::new(1e6);
    let lines = decode_one(
        ook(&honeywell(0x8, 0x5_1234, 0x80, true), &ch),
        ch.rate,
        "honeywell",
    );
    assert!(lines.is_empty(), "{lines:?}");
}

/// Interlogix: `serial` is the 24-bit value rtl_433 prints as the ID
/// (device type in its top nibble), sent least significant bit first.
fn interlogix(serial: u32, flags: &[(usize, u8)]) -> Widths {
    // 46 bits: the rows in rtl_433's captures end right after them.
    let mut m = [0u8; 46];
    for (k, v) in m.iter_mut().enumerate().take(24) {
        *v = (serial >> k & 1) as u8;
    }
    for &(pos, v) in flags {
        m[pos] = v;
    }
    let even = (0..40).step_by(2).fold(0, |a, i| a ^ m[i]);
    let odd = (1..40).step_by(2).fold(0, |a, i| a ^ m[i]);
    m[40] = even ^ 1;
    m[41] = odd ^ 1;
    let mut bits = vec![0; 12];
    bits.push(1);
    bits.extend(m);
    let mut w = ppm(&bits, 122.0, 122.0, 244.0);
    w[0].0 = 976.0;
    repeat(&w, 3, 2000.0)
}

#[test]
fn interlogix_contact() {
    for ch in channels() {
        // Contact sensor (type A), switch 1 open (bit 29), battery low (27).
        let w = interlogix(0xA1_2345, &[(29, 1), (27, 1), (24, 1)]);
        let lines = decode_one(ook(&w, &ch), ch.rate, "interlogix");
        check(
            &one(&lines, "Interlogix-Security"),
            &[
                ("subtype", "contact"),
                ("id", "a12345"),
                ("battery_ok", "0"),
                ("switch1", "OPEN"),
                ("switch2", "CLOSED"),
                ("switch5", "CLOSED"),
            ],
        );
    }
}

// ---- Remotes -----------------------------------------------------------------------

/// A 24-bit code, PWM with base period `a`: sync first, then 4 words, each
/// followed by its sync.
fn remote(code: u32, a: f64) -> Widths {
    let sync = (a, 31.0 * a);
    let mut w = vec![sync];
    for _ in 0..4 {
        for i in (0..24).rev() {
            w.push(if code >> i & 1 == 1 {
                (3.0 * a, a)
            } else {
                (a, 3.0 * a)
            });
        }
        w.push(sync);
    }
    w
}

#[test]
fn ev1527_remote() {
    for ch in channels() {
        for a in [300.0, 340.0, 380.0] {
            let lines = decode(ook(&remote(0x5A3C79, a), &ch), ch.rate, json!({}));
            check(
                &one(&lines, "EV1527"),
                &[
                    ("id", "5a3c7"),
                    ("button", "9"),
                    ("code", "5a3c79"),
                    ("mic", "REPEAT"),
                ],
            );
            none_of(&lines, &["PT2262"]);
        }
    }
}

#[test]
fn pt2262_remote() {
    let trits = "1F0F01FF0110";
    let code = trits.chars().fold(0u32, |c, t| {
        (c << 2)
            | match t {
                '0' => 0,
                '1' => 3,
                _ => 1,
            }
    });
    for ch in channels() {
        let lines = decode(ook(&remote(code, 320.0), &ch), ch.rate, json!({}));
        check(
            &one(&lines, "PT2262"),
            &[("id", "1F0F01FF"), ("data", "0110"), ("tristate", trits)],
        );
        none_of(&lines, &["EV1527"]);
    }
}

#[test]
fn remotes_need_two_words() {
    let ch = Channel::new(1e6);
    let mut w = remote(0x5A3C79, 320.0);
    w.truncate(26); // one sync, one word and its sync
    let lines = decode(ook(&w, &ch), ch.rate, json!({}));
    none_of(&lines, &["EV1527", "PT2262"]);
}

#[test]
fn remotes_ignore_other_devices() {
    for ch in channels() {
        let mut b = [0xDE, 0xAD, 0xBE, 0xEF, 130, 76, 0x45, 0];
        b[7] = b[..7].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        let sig = concat(&[
            ook(&schrader(0x0ABC_DEF1, 0x67, 88, 72, false), &ch),
            ook(&honeywell(0x8, 0x5_1234, 0x80, false), &ch),
            ook(&interlogix(0xA1_2345, &[(29, 1)]), &ch),
            fsk(&toyota(0x1234_ABCD, 156, 62), 25e3, &ch),
            fsk(&vdo(&b), 30e3, &ch),
        ]);
        let lines = decode(sig, ch.rate, json!({}));
        none_of(&lines, &["EV1527", "PT2262"]);
        for m in [
            "Schrader",
            "Honeywell-Security",
            "Interlogix-Security",
            "Toyota",
            "Ford",
        ] {
            assert_eq!(of_model(&lines, m).len(), 1, "{m} in {lines:?}");
        }
    }
}

// ---- ERT ---------------------------------------------------------------------------

/// 30.5 µs half-bits (32768 chips/s).
const ERT_HALF: f64 = 1e6 / 32768.0;

fn ert_rates() -> Vec<Channel> {
    [1e6, 2.4e6]
        .iter()
        .enumerate()
        .map(|(i, &rate)| Channel {
            snr_db: 15.0,
            seed: 20 + i as u64,
            ..Channel::new(rate)
        })
        .collect()
}

fn scm(id: u32, ert_type: u8, consumption: u32, corrupt: bool) -> Widths {
    let pre: u32 = 0x0F_2A60; // 21 bits, the first (0) being the zero bit
    let mut b = [0u8; 12];
    b[0] = (pre >> 13) as u8;
    b[1] = (pre >> 5) as u8;
    b[2] = ((pre & 0x1F) as u8) << 3 | ((id >> 24) as u8 & 3) << 1;
    b[3] = 1 << 6 | (ert_type & 0xF) << 2 | 2;
    b[4..7].copy_from_slice(&consumption.to_be_bytes()[1..]);
    b[7..10].copy_from_slice(&id.to_be_bytes()[1..]);
    let bch = crc16(&b[2..10], 0x6F63, 0) ^ u16::from(corrupt);
    b[10..].copy_from_slice(&bch.to_be_bytes());
    mc_zerobit(&bits_of(&b, 96), ERT_HALF)
}

#[test]
fn ert_scm() {
    for ch in ert_rates() {
        let lines = decode_one(
            ook(&scm(0x2AB_CDEF, 7, 123_456, false), &ch),
            ch.rate,
            "scm",
        );
        check(
            &one(&lines, "ERT-SCM"),
            &[
                ("id", &0x2AB_CDEF.to_string()),
                ("physical_tamper", "1"),
                ("ert_type", "7"),
                ("encoder_tamper", "2"),
                ("consumption_data", "123456"),
                ("MeterType", "Electric"),
                ("mic", "CRC"),
            ],
        );
    }
    let ch = Channel::new(1e6);
    let lines = decode_one(ook(&scm(0x2AB_CDEF, 7, 123_456, true), &ch), ch.rate, "scm");
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn ert_scmplus() {
    let mut f = vec![0x55, 0x55, 0x16, 0xA3, 0x1E, 0xAB];
    f.extend(68_211_547u32.to_be_bytes());
    f.extend(6_883u32.to_be_bytes());
    f.extend([0x49, 0x00]);
    let crc = crc16(&f[4..16], 0x1021, 0x0971);
    f.extend(crc.to_be_bytes());
    let w = mc_zerobit(&bits_of(&f, f.len() * 8), ERT_HALF);
    for ch in ert_rates() {
        let lines = decode_one(ook(&w, &ch), ch.rate, "scmplus");
        check(
            &one(&lines, "SCMplus"),
            &[
                ("id", "68211547"),
                ("ProtocolID", "0x1E"),
                ("EndpointType", "0xAB"),
                ("Consumption", "6883"),
                ("Tamper", "0x4900"),
                ("MeterType", "Water"),
                ("mic", "CRC"),
            ],
        );
    }
}

#[test]
fn ert_idm() {
    let mut b = [0u8; 92];
    b[..3].copy_from_slice(&[0x16, 0xA3, 0x1C]);
    b[3] = 92;
    b[4] = 0xC6;
    b[5] = 4;
    b[6] = 7;
    b[7..11].copy_from_slice(&11_278_109u32.to_be_bytes());
    b[11] = 246;
    b[12] = 0xBC;
    for (i, x) in b[13..84].iter_mut().enumerate() {
        *x = (i * 37 % 251) as u8;
    }
    b[27..31].copy_from_slice(&339_972u32.to_be_bytes());
    b[84..86].copy_from_slice(&476u16.to_be_bytes());
    let crc = crc16(&b[2..88], 0x1021, 0xD895);
    b[88..90].copy_from_slice(&crc.to_be_bytes());
    let mut f = vec![0x55, 0x55];
    f.extend(b);
    let w = mc_zerobit(&bits_of(&f, f.len() * 8), ERT_HALF);
    for ch in ert_rates() {
        let lines = decode_one(ook(&w, &ch), ch.rate, "idm");
        check(
            &one(&lines, "IDM"),
            &[
                ("id", "11278109"),
                ("PacketTypeID", "0x1C"),
                ("ApplicationVersion", "4"),
                ("ERTType", "0x07"),
                ("ConsumptionIntervalCount", "246"),
                ("LastConsumptionCount", "339972"),
                ("TransmitTimeOffset", "476"),
                ("MeterType", "Electric"),
                ("mic", "CRC"),
            ],
        );
    }
}

// ---- Junk ----------------------------------------------------------------------------

/// Bursts of interference, pulses and gaps anywhere from 20 to 800 µs,
/// sliced and decoded with every device: nothing may come out.
#[test]
fn junk_decodes_as_nothing() {
    use foxygnal_blocks::digital::ism::{
        bits::BitBuffer, devices, out::Out, pulse::Pulses, slicer,
    };
    let mut rng = Rng(0x0BAD_5EED);
    let mut bb = BitBuffer::new();
    let mut scratch = BitBuffer::new();
    for _ in 0..3000 {
        let n = 20 + (rng.uniform() * 280.0) as usize;
        let w: Widths = (0..n)
            .map(|_| (20.0 + 780.0 * rng.uniform(), 20.0 + 780.0 * rng.uniform()))
            .collect();
        for fsk in [false, true] {
            let p = Pulses::from_us(250e3, &w, fsk);
            for dev in devices::all()
                .iter()
                .filter(|d| d.modulation.is_fsk() == fsk)
            {
                let mut out = Out::new();
                slicer::slice(&p, dev.modulation, &dev.timing, &mut bb);
                (dev.decode)(&bb, &mut scratch, &mut out);
                let text = String::from_utf8_lossy(out.text()).into_owned();
                assert!(text.is_empty(), "{}: {text} from {w:?}", dev.key);
            }
        }
    }
}
