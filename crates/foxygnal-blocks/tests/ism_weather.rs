//! `ism_decoder` weather sensors: LaCrosse TX141 / IT+, Fine Offset WH24 /
//! WH65 / WH1080, Oregon Scientific v2.1 / v3, Nexus, Rubicson, Prologue.
//! Each has an encoder here, written from the formats' descriptions with its
//! own CRC / digest arithmetic (bit-serial division, a precomputed key
//! sequence), modulated with noise, a frequency offset and ±10 % jitter.

mod ism_common;

use ism_common::*;
use serde_json::json;

// ---- Shared encoder arithmetic -----------------------------------------------------

/// CRC-8 by long division over the message bits, most significant first.
fn crc8_div(bytes: &[u8], poly: u8, init: u8) -> u8 {
    // Register holds the remainder; feed bits one at a time.
    let mut reg = init;
    for &byte in bytes {
        for i in (0..8).rev() {
            let top = (reg >> 7) ^ ((byte >> i) & 1);
            reg <<= 1;
            if top == 1 {
                reg ^= poly;
            }
        }
    }
    reg
}

/// The LaCrosse reflected LFSR digest: the key sequence first, then the XOR
/// of the keys at the set message bits (last byte first, low bit first).
fn digest_reflect(bytes: &[u8], poly: u8, key: u8) -> u8 {
    let nbits = bytes.len() * 8;
    let mut keys = Vec::with_capacity(nbits);
    let mut k = key;
    for _ in 0..nbits {
        keys.push(k);
        k = (k << 1) ^ if k & 0x80 != 0 { poly } else { 0 };
    }
    let mut i = 0;
    let mut sum = 0;
    for &b in bytes.iter().rev() {
        for bit in 0..8 {
            if b >> bit & 1 == 1 {
                sum ^= keys[i];
            }
            i += 1;
        }
    }
    sum
}

/// Manchester as the leading-zero slicer reads it: a 0 rises mid-bit, a 1
/// falls mid-bit (levels of half-bits).
fn zerobit_levels(bits: &[u8]) -> Vec<u8> {
    bits.iter()
        .flat_map(|&b| if b == 1 { [1, 0] } else { [0, 1] })
        .collect()
}

/// NRZ timing jitter as transmitters have it: every edge moves by up to
/// ±`j` of a bit, independently (the channel's own jitter scales whole
/// runs, which no NRZ receiver could follow over 8-bit runs). The channel
/// returned has its jitter turned off.
fn edge_jitter(w: &Widths, bit_us: f64, j: f64, ch: &Channel) -> (Widths, Channel) {
    let mut rng = Rng(ch.seed ^ 0x5DEE_CE66);
    let mut t = 0.0;
    let mut edges = Vec::new();
    for &(p, g) in w {
        edges.push(t);
        t += p;
        edges.push(t);
        t += g;
    }
    let last = edges.len() - 1;
    for (i, e) in edges.iter_mut().enumerate() {
        if i > 0 && i < last {
            *e += j * bit_us * (2.0 * rng.uniform() - 1.0);
        }
    }
    let mut out = Vec::new();
    for (k, &(_, g)) in w.iter().enumerate() {
        let p = edges[2 * k + 1] - edges[2 * k];
        let g = if 2 * k + 2 < edges.len() {
            edges[2 * k + 2] - edges[2 * k + 1]
        } else {
            g
        };
        out.push((p.max(0.0), g.max(0.0)));
    }
    (out, Channel { jitter: 0.0, ..*ch })
}

/// FSK NRZ with per-edge jitter of ±10 % of a bit.
fn fsk_nrz(w: &Widths, bit_us: f64, dev: f64, ch: &Channel) -> Vec<foxygnal_core::Complex32> {
    let (w, ch) = edge_jitter(w, bit_us, 0.1, ch);
    fsk(&w, dev, &ch)
}

fn not(bits: &[u8]) -> Vec<u8> {
    bits.iter().map(|&b| b ^ 1).collect()
}

/// Run `make` under every test channel; `check` gets the lines.
fn each_channel(
    make: impl Fn(&Channel) -> Vec<foxygnal_core::Complex32>,
    device: &str,
    check: impl Fn(&[String]),
) {
    for ch in channels() {
        let lines = decode_one(make(&ch), ch.rate, device);
        check(&lines);
    }
}

// ---- LaCrosse TX141TH-Bv2 ----------------------------------------------------------

fn tx141th(id: u8, battery_low: bool, ch: u8, temp_c10: i32, hum: u8, corrupt: bool) -> Widths {
    let t = (temp_c10 + 500) as u16;
    let mut b = [
        id,
        u8::from(battery_low) << 7 | ch << 4 | (t >> 8) as u8,
        t as u8,
        hum,
        0,
    ];
    b[4] = digest_reflect(&b[..4], 0x31, 0xF4) ^ u8::from(corrupt);
    let data = bits_of(&b, 40);
    let mut w: Widths = Vec::new();
    for _ in 0..12 {
        w.extend(std::iter::repeat_n((833.0, 833.0), 4));
        // A 1 is the long pulse: the helper's "short = 1", inverted.
        w.extend(pwm(&not(&data), 208.0, 417.0, 625.0, 0.0));
    }
    w.extend([(833.0, 833.0), (833.0, 833.0)]);
    w
}

#[test]
fn lacrosse_tx141th() {
    each_channel(
        |ch| ook(&tx141th(0x5B, false, 1, -57, 63, false), ch),
        "tx141",
        |lines| {
            assert_eq!(lines.len(), 1, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "LaCrosse-TX141THBv2"),
                    ("id", "91"),
                    ("channel", "1"),
                    ("battery_ok", "1"),
                    ("temperature_C", "-5.7"),
                    ("humidity", "63"),
                    ("mic", "CRC"),
                ],
            );
        },
    );
}

#[test]
fn lacrosse_tx141th_bad_digest_is_rejected() {
    let ch = Channel::new(1e6);
    let lines = decode(
        ook(&tx141th(0x5B, true, 0, 215, 40, true), &ch),
        ch.rate,
        json!({}),
    );
    assert!(lines.is_empty(), "{lines:?}");
}

// ---- LaCrosse IT+ (TX29-IT, TX35DTH-IT) ----------------------------------------------

fn lacrosse_it(id: u8, temp_c10: i32, hum: u8, weak: bool, bit_us: f64) -> Widths {
    let t = (temp_c10 + 400) as u32;
    let (d0, d1, d2) = ((t / 100) as u8, (t / 10 % 10) as u8, (t % 10) as u8);
    let mut b = vec![0xAA, 0xAA, 0xAA, 0x2D, 0xD4];
    let mut p = [
        0x90 | id >> 2,
        (id & 3) << 6 | d0,
        d1 << 4 | d2,
        u8::from(weak) << 7 | hum,
        0,
    ];
    p[4] = crc8_div(&p[..4], 0x31, 0);
    b.extend(p);
    b.push(0xA0);
    runs(&nrz_levels(&bits_of(&b, b.len() * 8)), bit_us)
}

#[test]
fn lacrosse_tx29_and_tx35() {
    for (bit_us, key, model) in [
        (58.0, "tx29", "LaCrosse-TX29IT"),
        (104.4, "tx35", "LaCrosse-TX35DTHIT"),
    ] {
        each_channel(
            |ch| fsk_nrz(&lacrosse_it(45, 237, 52, false, bit_us), bit_us, 30e3, ch),
            key,
            |lines| {
                assert_eq!(lines.len(), 1, "{lines:?}");
                check(
                    &lines[0],
                    &[
                        ("model", model),
                        ("id", "45"),
                        ("battery_ok", "1"),
                        ("temperature_C", "23.7"),
                        ("humidity", "52"),
                        ("mic", "CRC"),
                    ],
                );
            },
        );
    }
    // No humidity sensor (0x6A), weak battery, below zero.
    let ch = Channel::new(1e6);
    let lines = decode_one(
        fsk_nrz(&lacrosse_it(3, -112, 0x6A, true, 58.0), 58.0, 30e3, &ch),
        ch.rate,
        "tx29",
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    check(
        &lines[0],
        &[("id", "3"), ("battery_ok", "0"), ("temperature_C", "-11.2")],
    );
    assert!(field(&lines[0], "humidity").is_none());
}

// ---- Fine Offset WH24 / WH65 -------------------------------------------------------

struct Wh24 {
    id: u8,
    temp_c10: i32,
    hum: u8,
    dir: u16,
    wind: u16,
    gust: u8,
    rain: u16,
    uv: u16,
    light: u32,
}

fn wh24_bytes(w: &Wh24, corrupt: bool) -> [u8; 17] {
    let t = (w.temp_c10 + 400) as u16;
    let mut b = [0u8; 17];
    b[0] = 0x24;
    b[1] = w.id;
    b[2] = w.dir as u8;
    b[3] = ((w.dir >> 8) as u8) << 7 | ((w.wind >> 8) as u8) << 4 | (t >> 8) as u8;
    b[4] = t as u8;
    b[5] = w.hum;
    b[6] = w.wind as u8;
    b[7] = w.gust;
    b[8..10].copy_from_slice(&w.rain.to_be_bytes());
    b[10..12].copy_from_slice(&w.uv.to_be_bytes());
    b[12..15].copy_from_slice(&w.light.to_be_bytes()[1..]);
    b[15] = crc8_div(&b[..15], 0x31, 0);
    b[16] = b[..16].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    if corrupt {
        b[6] ^= 0x10;
    }
    b
}

/// `pre` alternating preamble bits (ending in 0) before `2DD4`, `post`
/// postamble bits.
fn wh24_widths(payload: &[u8], pre: usize, post: usize) -> Widths {
    let mut bits: Vec<u8> = (0..pre).map(|i| ((pre - 1 - i) % 2) as u8).collect();
    bits.extend(bits_of(&[0x2D, 0xD4], 16));
    bits.extend(bits_of(payload, payload.len() * 8));
    bits.extend((0..post).map(|i| (i % 2 == 0) as u8));
    runs(&nrz_levels(&bits), 58.0)
}

fn sample_wh24() -> Wh24 {
    Wh24 {
        id: 191,
        temp_c10: 118,
        hum: 78,
        dir: 266,
        wind: 8,
        gust: 2,
        rain: 74,
        uv: 1500,
        light: 123_456,
    }
}

#[test]
fn fineoffset_wh24_and_wh65() {
    let b = wh24_bytes(&sample_wh24(), false);
    each_channel(
        |ch| fsk_nrz(&wh24_widths(&b, 41, 3), 58.0, 35e3, ch),
        "wh24",
        |lines| {
            assert_eq!(lines.len(), 1, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "Fineoffset-WH24"),
                    ("id", "191"),
                    ("battery_ok", "1"),
                    ("temperature_C", "11.8"),
                    ("humidity", "78"),
                    ("wind_dir_deg", "266"),
                    ("wind_avg_m_s", "1.1"),
                    ("wind_max_m_s", "2.2"),
                    ("rain_mm", "22.2"),
                    ("uv", "1500"),
                    ("uvi", "3"),
                    ("light_lux", "12345.6"),
                    ("mic", "CRC"),
                ],
            );
        },
    );
    // The WH65: longer preamble and postamble, other scales.
    each_channel(
        |ch| fsk_nrz(&wh24_widths(&b, 45, 12), 58.0, 35e3, ch),
        "wh24",
        |lines| {
            assert_eq!(lines.len(), 1, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "Fineoffset-WH65B"),
                    ("rain_mm", "18.8"),
                    ("wind_avg_m_s", "0.5"),
                ],
            );
        },
    );
    // A flipped bit fails the CRC.
    let ch = Channel::new(1e6);
    let bad = wh24_bytes(&sample_wh24(), true);
    let lines = decode(
        fsk_nrz(&wh24_widths(&bad, 41, 3), 58.0, 35e3, &ch),
        ch.rate,
        json!({}),
    );
    assert!(lines.is_empty(), "{lines:?}");
}

// ---- Fine Offset WH1080 ----------------------------------------------------------

fn wh1080(id: u8, temp_c10: i32, hum: u8, speed: u8, gust: u8, rain: u16, dir: u8) -> Widths {
    let t = (temp_c10 + 400) as u16;
    let mut b = [
        0xFF,
        0xA0 | id >> 4,
        (id & 0x0F) << 4 | (t >> 8) as u8,
        t as u8,
        hum,
        speed,
        gust,
        (rain >> 8) as u8,
        rain as u8,
        dir,
        0,
    ];
    b[10] = crc8_div(&b[..10], 0x31, 0xFF);
    let mut w = Vec::new();
    // Two packets, 31 ms apart.
    for _ in 0..2 {
        w.extend(pwm(&bits_of(&b, 88), 544.0, 1524.0, 0.0, 1036.0));
        pause(&mut w, 30_000.0);
    }
    w
}

#[test]
fn fineoffset_wh1080() {
    each_channel(
        |ch| ook(&wh1080(0x3C, 215, 41, 5, 9, 1234, 6), ch),
        "wh1080",
        |lines| {
            assert_eq!(lines.len(), 2, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "Fineoffset-WHx080"),
                    ("subtype", "0"),
                    ("id", "60"),
                    ("battery_ok", "1"),
                    ("temperature_C", "21.5"),
                    ("humidity", "41"),
                    ("wind_dir_deg", "135"),
                    ("wind_avg_km_h", "6.12"),
                    ("wind_max_km_h", "11.02"),
                    ("rain_mm", "370.2"),
                    ("mic", "CRC"),
                ],
            );
        },
    );
}

#[test]
fn fineoffset_wh3080_uv_light() {
    // UV sensor 0x5E, UV index 3, 12345.6 lux; sent with a 7-bit preamble.
    let mut b = [0xFF, 0x75, 0xE3, 0x55, 0x01, 0xE2, 0x40, 0];
    b[7] = crc8_div(&b[..7], 0x31, 0xFF);
    let mut w = Vec::new();
    for _ in 0..2 {
        w.extend(pwm(&bits_of(&b, 64)[1..], 544.0, 1524.0, 0.0, 1036.0));
        pause(&mut w, 30_000.0);
    }
    each_channel(
        |ch| ook(&w, ch),
        "wh1080",
        |lines| {
            assert_eq!(lines.len(), 2, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "Fineoffset-WHx080"),
                    ("subtype", "2"),
                    ("uv_sensor_id", "94"),
                    ("uv_status", "OK"),
                    ("uv_index", "3"),
                    ("lux", "12345.6"),
                    ("mic", "CRC"),
                ],
            );
        },
    );
}

// ---- Oregon Scientific ----------------------------------------------------------

/// Nibbles with the checksum (sum of the nibbles before it, low nibble
/// first) inserted at `at`, then `extra` nibbles.
fn os_nibbles(head: &[u8], at: usize, extra: &[u8]) -> Vec<u8> {
    assert_eq!(head.len(), at);
    let sum: u32 = head.iter().map(|&n| u32::from(n)).sum();
    let mut n = head.to_vec();
    n.push((sum & 0xF) as u8);
    n.push((sum >> 4 & 0xF) as u8);
    n.extend(extra);
    n
}

/// Nibbles → bits, each nibble least significant bit first.
fn lsb_bits(nibbles: &[u8]) -> Vec<u8> {
    nibbles
        .iter()
        .flat_map(|&n| (0..4).map(move |i| (n >> i) & 1))
        .collect()
}

/// v2.1: 16 preamble ones and the sync nibble 0xA, every bit sent as its
/// inverse then itself; the message twice.
fn os_v21(nibbles: &[u8]) -> Widths {
    let mut data = vec![1u8; 16];
    data.extend(lsb_bits(&[0xA]));
    data.extend(lsb_bits(nibbles));
    let doubled: Vec<u8> = data.iter().flat_map(|&d| [d ^ 1, d]).collect();
    let mut w = Vec::new();
    for _ in 0..2 {
        w.extend(runs(&zerobit_levels(&doubled), 488.0));
        pause(&mut w, 8000.0);
    }
    w
}

/// v3: 24 preamble bits (zeros as sliced), sync 0xA, the message.
fn os_v3(nibbles: &[u8]) -> Widths {
    let mut data = vec![0u8; 24];
    data.extend(lsb_bits(&[0xA]));
    data.extend(lsb_bits(nibbles));
    runs(&zerobit_levels(&data), 488.0)
}

/// Temperature as nibbles 8–11 (tenths, units, tens, sign).
fn os_temp(t10: i32) -> [u8; 4] {
    let a = t10.unsigned_abs();
    [
        (a % 10) as u8,
        (a / 10 % 10) as u8,
        (a / 100 % 10) as u8,
        if t10 < 0 { 8 } else { 0 },
    ]
}

#[test]
fn oregon_v21_thgr122n_and_thn132n() {
    // THGR122N: 1D20, channel 2, rolling id 0xB7, battery low.
    let mut head = vec![1, 0xD, 2, 0, 2, 0x7, 0xB, 0x4];
    head.extend(os_temp(-153));
    head.extend([7, 4, 0]); // humidity 47, then an unused nibble
    let thgr = os_nibbles(&head, 15, &[0x3, 0x9]);
    each_channel(
        |ch| ook(&os_v21(&thgr), ch),
        "oregon",
        |lines| {
            assert_eq!(lines.len(), 1, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "Oregon-THGR122N"),
                    ("id", "183"),
                    ("channel", "2"),
                    ("battery_ok", "0"),
                    ("temperature_C", "-15.3"),
                    ("humidity", "47"),
                    ("mic", "CHECKSUM"),
                ],
            );
        },
    );
    // THN132N: EC40, 64 bits.
    let mut head = vec![0xE, 0xC, 4, 0, 1, 0x2, 0x1, 0x0];
    head.extend(os_temp(264));
    let thn = os_nibbles(&head, 12, &[0x5, 0x6]);
    let ch = Channel::new(1e6);
    let lines = decode_one(ook(&os_v21(&thn), &ch), ch.rate, "oregon");
    assert_eq!(lines.len(), 1, "{lines:?}");
    check(
        &lines[0],
        &[
            ("model", "Oregon-THN132N"),
            ("id", "18"),
            ("channel", "1"),
            ("battery_ok", "1"),
            ("temperature_C", "26.4"),
        ],
    );
    // A wrong checksum is rejected.
    let mut bad = thgr.clone();
    bad[15] ^= 1;
    assert!(decode_one(ook(&os_v21(&bad), &ch), ch.rate, "oregon").is_empty());
}

#[test]
fn oregon_v3_thgr810_wgr800_pcr800() {
    // THGR810: F824, channel 3, rolling id 0x5A.
    let mut head = vec![0xF, 0x8, 2, 4, 3, 0xA, 0x5, 0x0];
    head.extend(os_temp(87));
    head.extend([1, 9, 0]);
    let thgr = os_nibbles(&head, 15, &[0x1, 0x2]);
    each_channel(
        |ch| ook(&os_v3(&thgr), ch),
        "oregon",
        |lines| {
            assert_eq!(lines.len(), 1, "{lines:?}");
            check(
                &lines[0],
                &[
                    ("model", "Oregon-THGR810"),
                    ("id", "90"),
                    ("channel", "3"),
                    ("battery_ok", "1"),
                    ("temperature_C", "8.7"),
                    ("humidity", "91"),
                    ("mic", "CHECKSUM"),
                ],
            );
        },
    );
    // WGR800: 1984, direction 6 (135°), gust 12.3 m/s, average 4.5 m/s.
    let head = vec![1, 9, 8, 4, 1, 0x3, 0x2, 0x0, 6, 0, 0, 3, 2, 1, 5, 4, 0];
    let wgr = os_nibbles(&head, 17, &[0x0, 0x0]);
    // PCR800: 2914, rate 1.23 in/h, total 45.678 in.
    let head = vec![2, 9, 1, 4, 1, 0x7, 0x7, 0x0, 3, 2, 1, 0, 8, 7, 6, 5, 4, 0];
    let pcr = os_nibbles(&head, 18, &[0x0, 0x0]);
    let ch = Channel::new(1e6);
    let lines = decode_one(ook(&os_v3(&wgr), &ch), ch.rate, "oregon");
    assert_eq!(lines.len(), 1, "{lines:?}");
    check(
        &lines[0],
        &[
            ("model", "Oregon-WGR800"),
            ("wind_max_m_s", "12.3"),
            ("wind_avg_m_s", "4.5"),
            ("wind_dir_deg", "135.0"),
        ],
    );
    let lines = decode_one(ook(&os_v3(&pcr), &ch), ch.rate, "oregon");
    assert_eq!(lines.len(), 1, "{lines:?}");
    check(
        &lines[0],
        &[
            ("model", "Oregon-PCR800"),
            ("rain_rate_in_h", "1.23"),
            ("rain_in", "45.678"),
        ],
    );
}

// ---- Nexus / Rubicson / Prologue -------------------------------------------------

/// 36-bit PPM messages, `repeats` times, each ended by a sync gap.
fn ppm36(b: &[u8; 5], short: f64, long: f64, sync: f64, repeats: usize) -> Widths {
    let mut w = Vec::new();
    for _ in 0..repeats {
        w.extend(ppm(&bits_of(b, 36), 500.0, short, long));
        pause(&mut w, sync);
    }
    w
}

fn nexus_bytes(id: u8, ch: u8, temp_c10: i32, hum: u8) -> [u8; 5] {
    let t = (temp_c10 as u16) & 0xFFF;
    [
        id,
        0x80 | (ch - 1) << 4 | (t >> 8) as u8,
        t as u8,
        0xF0 | hum >> 4,
        (hum & 0x0F) << 4,
    ]
}

fn rubicson_bytes(id: u8, ch: u8, temp_c10: i32, corrupt: bool) -> [u8; 5] {
    let t = (temp_c10 as u16) & 0xFFF;
    let head = [id, 0x80 | (ch - 1) << 4 | (t >> 8) as u8, t as u8, 0xF0];
    let crc = crc8_div(&head, 0x31, 0x6C) ^ u8::from(corrupt);
    [
        head[0],
        head[1],
        head[2],
        0xF0 | crc >> 4,
        (crc & 0x0F) << 4,
    ]
}

#[test]
fn nexus_only_as_nexus() {
    let b = nexus_bytes(0xD3, 2, -42, 55);
    // The humidity must not happen to be a valid Rubicson CRC.
    let head = [b[0], b[1], b[2], 0xF0];
    assert_ne!(crc8_div(&head, 0x31, 0x6C), 55);
    for ch in channels() {
        let lines = decode(
            ook(&ppm36(&b, 1000.0, 2000.0, 4000.0, 12), &ch),
            ch.rate,
            json!({}),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        check(
            &lines[0],
            &[
                ("model", "Nexus-TH"),
                ("id", "211"),
                ("channel", "2"),
                ("battery_ok", "1"),
                ("temperature_C", "-4.2"),
                ("humidity", "55"),
            ],
        );
    }
}

#[test]
fn rubicson_only_as_rubicson() {
    let b = rubicson_bytes(0x6E, 3, 193, false);
    for ch in channels() {
        let lines = decode(
            ook(&ppm36(&b, 1000.0, 2000.0, 4000.0, 12), &ch),
            ch.rate,
            json!({}),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        check(
            &lines[0],
            &[
                ("model", "Rubicson-Temperature"),
                ("id", "110"),
                ("channel", "3"),
                ("battery_ok", "1"),
                ("temperature_C", "19.3"),
                ("mic", "CRC"),
            ],
        );
    }
    // A bad CRC is no Rubicson.
    let ch = Channel::new(1e6);
    let b = rubicson_bytes(0x6E, 3, 193, true);
    let lines = decode_one(
        ook(&ppm36(&b, 1000.0, 2000.0, 4000.0, 12), &ch),
        ch.rate,
        "rubicson",
    );
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn prologue_only_as_prologue() {
    let t = (-87i32 as u16) & 0xFFF;
    let (id, hum) = (0xA5u8, 0x2Bu8);
    let b = [
        0x90 | id >> 4,
        (id & 0x0F) << 4 | 0x08 | 1,
        (t >> 4) as u8,
        ((t & 0x0F) as u8) << 4 | hum >> 4,
        (hum & 0x0F) << 4,
    ];
    for ch in channels() {
        // A sync pulse before the first of seven messages.
        let mut w = vec![(500.0, 9000.0)];
        w.extend(ppm36(&b, 2000.0, 4000.0, 9000.0, 7));
        let lines = decode(ook(&w, &ch), ch.rate, json!({}));
        assert_eq!(lines.len(), 1, "{lines:?}");
        check(
            &lines[0],
            &[
                ("model", "Prologue-TH"),
                ("subtype", "9"),
                ("id", "165"),
                ("channel", "2"),
                ("battery_ok", "1"),
                ("temperature_C", "-8.7"),
                ("humidity", "43"),
                ("button", "0"),
            ],
        );
    }
}
