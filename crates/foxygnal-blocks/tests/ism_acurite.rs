//! `ism_decoder`: Acurite and Ambient Weather devices, each from an
//! independent encoder (bytes, checks and line code built here), through
//! noise, a frequency offset and ±10 % timing jitter at 250 kS/s and 1 MS/s.

mod ism_common;

use ism_common::*;

// ---- Helpers ----------------------------------------------------------------------

/// `x` (7 bits) with its top bit set for even parity.
fn par(x: u8) -> u8 {
    let x = x & 0x7F;
    x | ((x.count_ones() as u8 & 1) << 7)
}

fn sum8(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

/// Run `w` (OOK) under every test channel with only `device` enabled; the
/// lines of each run.
fn each_channel(w: &Widths, device: &str) -> Vec<Vec<String>> {
    channels()
        .iter()
        .map(|ch| decode_one(ook(w, ch), ch.rate, device))
        .collect()
}

/// Exactly one line under every channel, with these fields.
fn expect_one(w: &Widths, device: &str, fields: &[(&str, &str)]) {
    for lines in each_channel(w, device) {
        assert_eq!(lines.len(), 1, "{lines:?}");
        check(&lines[0], fields);
    }
}

/// Nothing decoded under any channel.
fn expect_none(w: &Widths, device: &str) {
    for lines in each_channel(w, device) {
        assert!(lines.is_empty(), "{lines:?}");
    }
}

// ---- TXR family --------------------------------------------------------------------

/// A TXR message: four 620 µs sync pulses, then the bytes as PWM (a 0 is a
/// 220 µs pulse, a 1 a 408 µs pulse, ~612 µs period), three times.
fn txr(bytes: &[u8]) -> Widths {
    // ism_common's pwm sends a 1 as the short pulse: invert.
    let bits: Vec<u8> = bits_of(bytes, bytes.len() * 8)
        .iter()
        .map(|b| b ^ 1)
        .collect();
    let mut w = Vec::new();
    for _ in 0..3 {
        for _ in 0..4 {
            w.push((620.0, 596.0));
        }
        w.extend(pwm(&bits, 220.0, 408.0, 612.0, 0.0));
        pause(&mut w, 2000.0);
    }
    w
}

/// Channel bits: A = 11, B = 10, C = 00.
fn ch_bits(ch: char) -> u8 {
    match ch {
        'A' => 3,
        'B' => 2,
        _ => 0,
    }
}

fn tower(ch: char, id: u16, battery_ok: bool, temp_c10: i32, hum: u8) -> Vec<u8> {
    let raw = (temp_c10 + 1000) as u16;
    let mut b = vec![
        ch_bits(ch) << 6 | (id >> 8) as u8 & 0x3F,
        id as u8,
        par(u8::from(battery_ok) << 6 | 0x04),
        par(hum),
        par((raw >> 7) as u8),
        par(raw as u8),
    ];
    b.push(sum8(&b));
    b
}

#[test]
fn acurite_tower() {
    expect_one(
        &txr(&tower('A', 0x1234, true, 213, 47)),
        "acurite_txr",
        &[
            ("model", "Acurite-Tower"),
            ("id", "4660"),
            ("channel", "A"),
            ("battery_ok", "1"),
            ("temperature_C", "21.3"),
            ("humidity", "47"),
            ("mic", "CHECKSUM"),
        ],
    );
    expect_one(
        &txr(&tower('C', 0x0042, false, -175, 88)),
        "acurite_txr",
        &[
            ("channel", "C"),
            ("id", "66"),
            ("battery_ok", "0"),
            ("temperature_C", "-17.5"),
        ],
    );
}

#[test]
fn acurite_tower_bad_checksum_and_parity_are_rejected() {
    let mut b = tower('B', 0x0777, true, 100, 50);
    b[6] ^= 0x01;
    expect_none(&txr(&b), "acurite_txr");
    // Flip a parity bit and fix the checksum: the parity check catches it.
    let mut b = tower('B', 0x0777, true, 100, 50);
    b[3] ^= 0x80;
    b[6] = sum8(&b[..6]);
    expect_none(&txr(&b), "acurite_txr");
}

fn five_in_one(msg: u8, ch: char, id: u16, seq: u8, wind_raw: u8, a: u16, b5: u16) -> Vec<u8> {
    // a: direction code (0x31) or temperature raw (0x38); b5: rain or humidity.
    let mut b = vec![
        ch_bits(ch) << 6 | (seq & 3) << 4 | (id >> 8) as u8 & 0x0F,
        id as u8,
        par(0x40 | msg),
        par(wind_raw >> 3),
    ];
    if msg == 0x31 {
        b.push(par((wind_raw & 7) << 4 | a as u8 & 0x0F));
        b.push(par((b5 >> 7) as u8));
        b.push(par(b5 as u8));
    } else {
        b.push(par((wind_raw & 7) << 4 | (a >> 7) as u8 & 0x0F));
        b.push(par(a as u8));
        b.push(par(b5 as u8));
    }
    b.push(sum8(&b));
    b
}

#[test]
fn acurite_5n1_both_messages() {
    // Wind raw 10 → 10 × 0.8278 + 1 = 9.3 km/h; direction code 0xA = E (90°).
    expect_one(
        &txr(&five_in_one(0x31, 'B', 0x0ABC, 2, 10, 0xA, 123)),
        "acurite_txr",
        &[
            ("model", "Acurite-5n1"),
            ("message_type", "49"),
            ("id", "2748"),
            ("channel", "B"),
            ("sequence_num", "2"),
            ("battery_ok", "1"),
            ("wind_avg_km_h", "9.3"),
            ("wind_dir_deg", "90.0"),
            ("rain_in", "1.23"),
        ],
    );
    // 68.5 °F → raw 1085.
    expect_one(
        &txr(&five_in_one(0x38, 'A', 0x0123, 0, 0, 1085, 61)),
        "acurite_txr",
        &[
            ("model", "Acurite-5n1"),
            ("message_type", "56"),
            ("id", "291"),
            ("wind_avg_km_h", "0.0"),
            ("temperature_F", "68.5"),
            ("humidity", "61"),
        ],
    );
}

#[test]
fn acurite_atlas_with_and_without_lightning() {
    let id: u16 = 0x2D5;
    let head = |seq: u8, msg: u8, mph: u8| {
        vec![
            ch_bits('A') << 6 | seq << 2 | (id >> 8) as u8,
            id as u8,
            par(0x40 | msg),
            par(mph >> 1),
        ]
    };
    // 0x05: 13 mph, 72.4 °F (raw 1124), 33 %.
    let mut b = head(1, 0x05, 13);
    b.extend([
        par(((13 & 1) << 6) | (1124 >> 7) as u8),
        par(1124u16 as u8),
        par(33),
    ]);
    b.push(sum8(&b));
    expect_one(
        &txr(&b),
        "acurite_txr",
        &[
            ("model", "Acurite-Atlas"),
            ("id", "725"),
            ("channel", "A"),
            ("sequence_num", "1"),
            ("message_type", "5"),
            ("wind_avg_mi_h", "13.0"),
            ("temperature_F", "72.4"),
            ("humidity", "33"),
        ],
    );
    // 0x06: 7 mph, 270°, rain 0.42 in.
    let (dir, rain) = (270u16, 42u16);
    let mut b = head(0, 0x06, 7);
    b.extend([
        par(((7 & 1) << 6) | (dir >> 5) as u8),
        par(((dir as u8 & 0x1F) << 2) | (rain >> 7) as u8),
        par(rain as u8),
    ]);
    b.push(sum8(&b));
    expect_one(
        &txr(&b),
        "acurite_txr",
        &[
            ("wind_dir_deg", "270.0"),
            ("rain_in", "0.42"),
            ("wind_avg_mi_h", "7.0"),
        ],
    );
    // 0x27 with lightning: UV 6, 5432 ×10 lux, 300 strikes at 12.
    let (lux, strikes) = (5432u16, 300u16);
    let mut b = head(2, 0x27, 3);
    b.extend([
        par(((3 & 1) << 6) | 6),
        par((lux >> 7) as u8),
        par(lux as u8),
        par((strikes >> 2) as u8),
        par(((strikes & 3) as u8) << 5 | 12),
    ]);
    b.push(sum8(&b));
    expect_one(
        &txr(&b),
        "acurite_txr",
        &[
            ("message_type", "39"),
            ("uvi", "6"),
            ("lux", "54320"),
            ("strike_count", "300"),
            ("strike_distance", "12"),
        ],
    );
}

#[test]
fn acurite_3n1_and_899() {
    // 3n1: 45.0 °F → raw 1930, 9 mph, 70 %.
    let id: u16 = 0x1A2B;
    let raw = 1930u16;
    let mut b = vec![
        ch_bits('C') << 6 | (id >> 8) as u8 & 0x3F,
        id as u8,
        0x40 | 0x20,
        70,
        (raw >> 7) as u8 & 0x1F,
        raw as u8 & 0x7F,
        9,
    ];
    b.push(sum8(&b));
    expect_one(
        &txr(&b),
        "acurite_txr",
        &[
            ("model", "Acurite-3n1"),
            ("id", "6699"),
            ("channel", "C"),
            ("temperature_F", "45.0"),
            ("humidity", "70"),
            ("wind_avg_mi_h", "9.0"),
        ],
    );
    // 899: 157 tips = 39.88 mm, channel bits 01.
    let id: u16 = 0x0321;
    let rain = 157u16;
    let mut b = vec![
        1 << 6 | (id >> 8) as u8,
        id as u8,
        par(0x40 | 0x30),
        par(0),
        par(0),
        par((rain >> 7) as u8),
        par(rain as u8),
    ];
    b.push(sum8(&b));
    expect_one(
        &txr(&b),
        "acurite_txr",
        &[
            ("model", "Acurite-Rain899"),
            ("id", "801"),
            ("channel", "1"),
            ("rain_mm", "39.88"),
        ],
    );
}

// ---- 606TX --------------------------------------------------------------------------

/// The 606TX's LFSR digest, computed with an explicit key schedule.
fn digest_606(b: &[u8; 3]) -> u8 {
    let mut keys = [0u8; 24];
    let mut k = 0xF1u8;
    for slot in keys.iter_mut() {
        *slot = k;
        k = (k >> 1) ^ if k & 1 == 1 { 0x98 } else { 0 };
    }
    (0..24)
        .filter(|&i| b[i / 8] & (0x80 >> (i % 8)) != 0)
        .fold(0, |s, i| s ^ keys[i])
}

fn acurite_606(id: u8, battery_ok: bool, button: bool, channel: u8, temp_c10: i16) -> Widths {
    let t = (temp_c10 as u16) & 0xFFF;
    let head = [
        id,
        u8::from(battery_ok) << 7 | u8::from(button) << 6 | (channel - 1) << 4 | (t >> 8) as u8,
        t as u8,
    ];
    let b = [head[0], head[1], head[2], digest_606(&head)];
    let mut bits = bits_of(&b, 32);
    bits.push(0);
    let mut w = Vec::new();
    for _ in 0..6 {
        w.extend(ppm(&bits, 500.0, 2000.0, 4000.0));
        pause(&mut w, 9000.0);
    }
    w
}

#[test]
fn acurite_606tx_repeats() {
    expect_one(
        &acurite_606(0x9C, true, false, 2, -54),
        "acurite_606",
        &[
            ("model", "Acurite-606TX"),
            ("id", "156"),
            ("channel", "2"),
            ("battery_ok", "1"),
            ("button", "0"),
            ("temperature_C", "-5.4"),
            ("mic", "CHECKSUM"),
        ],
    );
}

// ---- 609TXC (also in ism.rs): a bad checksum -------------------------------------------

#[test]
fn acurite_609_bad_checksum_is_rejected() {
    let mut b = [0x21u8, 0x20, 0xD7, 45, 0];
    b[4] = sum8(&b[..4]) ^ 0x10;
    let mut w = Vec::new();
    for _ in 0..3 {
        w.extend(ppm(&bits_of(&b, 40), 500.0, 1000.0, 2000.0));
        pause(&mut w, 4000.0);
    }
    expect_none(&w, "acurite_609");
    b[4] ^= 0x10;
    let mut w = Vec::new();
    for _ in 0..3 {
        w.extend(ppm(&bits_of(&b, 40), 500.0, 1000.0, 2000.0));
        pause(&mut w, 4000.0);
    }
    expect_one(
        &w,
        "acurite_609",
        &[("temperature_C", "21.5"), ("humidity", "45")],
    );
}

// ---- 986 ------------------------------------------------------------------------------

/// CRC-8 (poly 0x07) of the bit-reversed bytes, reversed: the reflected CRC.
fn crc_986(b: &[u8]) -> u8 {
    let mut c = 0u8;
    for &x in b {
        c ^= x.reverse_bits();
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c.reverse_bits()
}

fn acurite_986(id: u16, sensor2: bool, battery_low: bool, temp_f: i8) -> Widths {
    let t = if temp_f < 0 {
        0x80 | temp_f.unsigned_abs()
    } else {
        temp_f as u8
    };
    let mut b = [
        t,
        (id >> 8) as u8,
        id as u8,
        u8::from(battery_low) << 1 | u8::from(sensor2),
        0,
    ];
    b[4] = crc_986(&b[..4]);
    // Least significant bit first.
    let air: Vec<u8> = b.iter().map(|x| x.reverse_bits()).collect();
    let mut w = Vec::new();
    for _ in 0..2 {
        for _ in 0..4 {
            w.push((1600.0, 1600.0));
        }
        w.extend(ppm(&bits_of(&air, 40), 400.0, 520.0, 880.0));
        pause(&mut w, 1600.0);
    }
    w
}

#[test]
fn acurite_986_fridge_and_freezer() {
    expect_one(
        &acurite_986(0xBEEF, true, false, -4),
        "acurite_986",
        &[
            ("model", "Acurite-986"),
            ("id", "48879"),
            ("channel", "2F"),
            ("battery_ok", "1"),
            ("temperature_F", "-4.0"),
            ("mic", "CRC"),
        ],
    );
    expect_one(
        &acurite_986(0x1234, false, true, 38),
        "acurite_986",
        &[
            ("channel", "1R"),
            ("battery_ok", "0"),
            ("temperature_F", "38.0"),
        ],
    );
}

// ---- Ambient Weather F007TH ---------------------------------------------------------------

/// The F007TH digest: an LFSR (generator 0x98) keyed from 0x3E, XOR 0x64.
fn digest_f007(b: &[u8]) -> u8 {
    let mut key = 0x3Eu8;
    let mut sum = 0u8;
    for &x in b {
        for i in (0..8).rev() {
            if x >> i & 1 == 1 {
                sum ^= key;
            }
            key = if key & 1 == 1 {
                (key >> 1) ^ 0x98
            } else {
                key >> 1
            };
        }
    }
    sum ^ 0x64
}

/// Manchester as the zero-bit slicer reads it: a 0 rises mid-bit, a 1 falls.
fn mc_levels(bits: &[u8]) -> Vec<u8> {
    bits.iter()
        .flat_map(|&b| if b == 0 { [0, 1] } else { [1, 0] })
        .collect()
}

fn f007th(id: u8, channel: u8, battery_low: bool, temp_f10: i32, hum: u8) -> Widths {
    let raw = (temp_f10 + 400) as u16;
    let mut b = [
        0x45,
        id,
        u8::from(battery_low) << 7 | (channel - 1) << 4 | (raw >> 8) as u8,
        raw as u8,
        hum,
        0,
    ];
    b[5] = digest_f007(&b[..5]);
    // Preamble 0x00145: 12 bits 0x001, then the six bytes; three copies.
    let mut bits = Vec::new();
    for _ in 0..3 {
        bits.extend(bits_of(&[0x00, 0x10], 12));
        bits.extend(bits_of(&b, 48));
    }
    runs(&mc_levels(&bits), 488.0)
}

#[test]
fn ambient_f007th() {
    expect_one(
        &f007th(0xB2, 3, false, 723, 41),
        "f007th",
        &[
            ("model", "Ambientweather-F007TH"),
            ("id", "178"),
            ("channel", "3"),
            ("battery_ok", "1"),
            ("temperature_F", "72.3"),
            ("humidity", "41"),
            ("mic", "CRC"),
        ],
    );
    expect_one(
        &f007th(0x07, 8, true, -125, 90),
        "f007th",
        &[
            ("channel", "8"),
            ("battery_ok", "0"),
            ("temperature_F", "-12.5"),
        ],
    );
}

#[test]
fn ambient_f007th_bad_digest_is_rejected() {
    // A message whose digest is off by one bit.
    let mut bits = Vec::new();
    let mut b = [0x45, 0xB2, 0x21, 0xF3, 41, 0];
    b[5] = digest_f007(&b[..5]) ^ 0x04;
    for _ in 0..3 {
        bits.extend(bits_of(&[0x00, 0x10], 12));
        bits.extend(bits_of(&b, 48));
    }
    expect_none(&runs(&mc_levels(&bits), 488.0), "f007th");
}

#[test]
fn all_devices_on_decode_only_what_was_sent() {
    let ch = Channel::new(1e6);
    let sig = concat(&[
        ook(&txr(&tower('B', 0x0222, true, 55, 30)), &ch),
        ook(&f007th(0x10, 1, false, 650, 35), &Channel { seed: 3, ..ch }),
        ook(
            &acurite_606(0x33, true, false, 1, 222),
            &Channel { seed: 4, ..ch },
        ),
        ook(
            &acurite_986(0x4321, false, false, 10),
            &Channel { seed: 5, ..ch },
        ),
    ]);
    let lines = decode(sig, ch.rate, serde_json::json!({}));
    let models: Vec<_> = lines.iter().map(|l| field(l, "model").unwrap()).collect();
    assert_eq!(
        models,
        [
            "Acurite-Tower",
            "Ambientweather-F007TH",
            "Acurite-606TX",
            "Acurite-986"
        ],
        "{lines:?}"
    );
}

// ---- Ambient Weather WH31E (FSK) ----------------------------------------------------------

/// CRC-8, polynomial 0x31, from a table built here.
fn crc_31(b: &[u8]) -> u8 {
    let table: Vec<u8> = (0..=255u8)
        .map(|i| {
            (0..8).fold(i, |c, _| {
                if c & 0x80 != 0 {
                    (c << 1) ^ 0x31
                } else {
                    c << 1
                }
            })
        })
        .collect();
    b.iter().fold(0, |c, &x| table[usize::from(c ^ x)])
}

fn wh31e(id: u8, channel: u8, battery_low: bool, temp_c10: i32, hum: u8) -> Widths {
    let raw = (temp_c10 + 400) as u16;
    let mut p = vec![
        0x30,
        id,
        0x80 | (channel - 1) << 4 | u8::from(battery_low) << 2 | (raw >> 8) as u8,
        raw as u8,
        hum,
    ];
    p.push(crc_31(&p));
    p.push(sum8(&p));
    p.extend([0x56, 0xA7, 0x5A, 0xB3]);
    let mut bytes = vec![0xAA; 6];
    bytes.extend([0x2D, 0xD4]);
    bytes.extend(&p);
    // Warm-up: 1336 µs mark, 1996 µs space (in 56 µs bits), then the data.
    let mut levels = vec![1u8; 24];
    levels.extend([0u8; 36]);
    levels.extend(nrz_levels(&bits_of(&bytes, bytes.len() * 8)));
    levels.extend([1, 0, 1, 0]);
    runs(&levels, 56.0)
}

#[test]
fn ambient_wh31e() {
    for ch in channels() {
        let w = wh31e(0x44, 2, false, 231, 57);
        let lines = decode_one(fsk(&w, 35e3, &ch), ch.rate, "wh31e");
        assert_eq!(lines.len(), 1, "{lines:?}");
        check(
            &lines[0],
            &[
                ("model", "AmbientWeather-WH31E"),
                ("id", "68"),
                ("channel", "2"),
                ("battery_ok", "1"),
                ("temperature_C", "23.1"),
                ("humidity", "57"),
                ("mic", "CRC"),
            ],
        );
        // A bad CRC is rejected.
        let mut w = wh31e(0x44, 2, false, 231, 57);
        // Lengthen one data run by a bit (~56 µs) somewhere in the payload.
        let i = w.len() - 20;
        w[i].0 += 56.0;
        let lines = decode_one(fsk(&w, 35e3, &ch), ch.rate, "wh31e");
        assert!(lines.is_empty(), "{lines:?}");
    }
}
