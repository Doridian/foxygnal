//! `ism_decoder`: the pulse detectors, slicers and analysis on synthetic
//! signals, with an independent encoder for a simple device. Device formats
//! have their own test files (`ism_*.rs`).

mod ism_common;

use ism_common::*;
use serde_json::json;

/// Acurite 609TXC: 40 bits PPM, sum checksum.
fn acurite_609(id: u8, battery_low: bool, temp_c10: i16, hum: u8) -> Widths {
    let t = (temp_c10 as u16) & 0xFFF;
    let status = 0x2 | if battery_low { 8 } else { 0 };
    let mut b = [id, (status << 4) | (t >> 8) as u8, t as u8, hum, 0];
    b[4] = b[..4].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    let mut w = Vec::new();
    for _ in 0..3 {
        w.extend(ppm(&bits_of(&b, 40), 500.0, 1000.0, 2000.0));
        pause(&mut w, 4000.0);
    }
    w
}

#[test]
fn acurite_609_through_noise_offset_and_jitter() {
    for ch in channels() {
        let sig = ook(&acurite_609(0xA7, false, -123, 56), &ch);
        let lines = decode_one(sig, ch.rate, "acurite_609");
        assert_eq!(lines.len(), 1, "{lines:?}");
        check(
            &lines[0],
            &[
                ("model", "Acurite-609TXC"),
                ("id", "167"),
                ("battery_ok", "1"),
                ("temperature_C", "-12.3"),
                ("humidity", "56"),
                ("mic", "CHECKSUM"),
            ],
        );
        // The offset is measured.
        let off: f64 = field(&lines[0], "offset_kHz").unwrap().parse().unwrap();
        assert!((off - ch.offset_hz / 1e3).abs() < 3.0, "{off}");
    }
}

#[test]
fn two_messages_and_a_battery_flag() {
    let ch = Channel::new(1e6);
    let a = ook(&acurite_609(1, true, 215, 40), &ch);
    let b = ook(&acurite_609(2, false, 0, 99), &Channel { seed: 9, ..ch });
    let lines = decode(concat(&[a, b]), ch.rate, json!({}));
    let l = of_model(&lines, "Acurite-609TXC");
    assert_eq!(l.len(), 2, "{lines:?}");
    check(
        l[0],
        &[("id", "1"), ("battery_ok", "0"), ("temperature_C", "21.5")],
    );
    check(
        l[1],
        &[("id", "2"), ("temperature_C", "0.0"), ("humidity", "99")],
    );
    // t= is the time of the message: the second comes later.
    let t0: f64 = field(l[0], "t").unwrap().parse().unwrap();
    let t1: f64 = field(l[1], "t").unwrap().parse().unwrap();
    assert!(t0 > 0.02 && t1 > t0 + 0.1, "{t0} {t1}");
}

/// LaCrosse TX-3/4/6U/7U: 44 bits PWM (550 µs = 1, 1400 µs = 0, 1000 µs
/// gaps), nibble sum and value parity; `value` in tenths.
fn lacrosse_tx(kind: u8, id: u8, tenths: u32) -> Widths {
    let d = [tenths / 100 % 10, tenths / 10 % 10, tenths % 10].map(|x| x as u8);
    let par = d.iter().map(|x| x.count_ones()).sum::<u32>() as u8 & 1;
    let mut n = [
        0x0,
        0xA,
        kind,
        id >> 3,
        ((id & 7) << 1) | par,
        d[0],
        d[1],
        d[2],
        d[0],
        d[1],
        0,
    ];
    n[10] = n[..10].iter().sum::<u8>() & 0xF;
    let bits: Vec<u8> = n
        .iter()
        .flat_map(|&x| (0..4).rev().map(move |i| (x >> i) & 1))
        .collect();
    let mut w = pwm(&bits, 550.0, 1400.0, 0.0, 1000.0);
    pause(&mut w, 29_000.0);
    let again = w.clone();
    w.extend(again);
    w
}

#[test]
fn lacrosse_tx_temperature_and_humidity() {
    for ch in channels() {
        // 23.6 is -26.4 °C (offset by 50).
        let t = ook(&lacrosse_tx(0x0, 0x5B, 236), &ch);
        let h = ook(&lacrosse_tx(0xE, 0x5B, 470), &Channel { seed: 3, ..ch });
        let lines = decode_one(concat(&[t, h]), ch.rate, "lacrosse_tx");
        // The repeat comes 29 ms later, a packet of its own (as in rtl_433).
        assert_eq!(lines.len(), 4, "{lines:?}");
        for l in &lines[..2] {
            check(
                l,
                &[
                    ("model", "LaCrosse-TX"),
                    ("id", "91"),
                    ("temperature_C", "-26.4"),
                ],
            );
        }
        for l in &lines[2..] {
            check(
                l,
                &[("model", "LaCrosse-TX"), ("id", "91"), ("humidity", "47.0")],
            );
        }
    }
    // A wrong checksum nibble is rejected.
    let ch = Channel::new(1e6);
    let mut w = lacrosse_tx(0x0, 0x5B, 736);
    w[42].0 = if w[42].0 < 900.0 { 1400.0 } else { 550.0 };
    w[86].0 = w[42].0;
    assert!(decode_one(ook(&w, &ch), ch.rate, "lacrosse_tx").is_empty());
}

#[test]
fn sensitivity() {
    // The weakest SNR (full band, 1 MS/s) at which all of a few messages
    // decode; printed for the docs, asserted loosely.
    let mut weakest = None;
    for snr in [20.0, 15.0, 12.0, 10.0, 8.0, 6.0, 4.0, 2.0, 0.0, -2.0] {
        let ok = (0..4).all(|seed| {
            let ch = Channel {
                snr_db: snr,
                seed: 100 + seed,
                ..Channel::new(1e6)
            };
            let lines = decode_one(
                ook(&acurite_609(0x55, false, 200, 50), &ch),
                ch.rate,
                "acurite_609",
            );
            lines.len() == 1
        });
        if !ok {
            break;
        }
        weakest = Some(snr);
    }
    eprintln!("609TXC decodes down to {weakest:?} dB SNR at 1 MS/s");
    assert!(weakest.is_some_and(|s| s <= 4.0), "{weakest:?}");
}

#[test]
fn analyze_summarises_unknown_ook() {
    // PWM, 300 / 900 µs in a 1200 µs period, 32 bits, no device knows it.
    let bits: Vec<u8> = (0..32)
        .map(|i| ((0xC3A5_0F96u32 >> (31 - i)) & 1) as u8)
        .collect();
    let ch = Channel::new(1e6);
    let sig = ook(&pwm(&bits, 300.0, 900.0, 1200.0, 0.0), &ch);
    let lines = decode(sig.clone(), ch.rate, json!({ "analyze": true }));
    assert_eq!(lines.len(), 1, "{lines:?}");
    check(
        &lines[0],
        &[
            ("model", "unknown"),
            ("mod", "OOK"),
            ("pulses", "32"),
            ("guess", "PWM"),
        ],
    );
    let short: f64 = field(&lines[0], "short_us").unwrap().parse().unwrap();
    let long: f64 = field(&lines[0], "long_us").unwrap().parse().unwrap();
    assert!(
        (short - 300.0).abs() < 30.0 && (long - 900.0).abs() < 60.0,
        "{short} {long}"
    );
    // Off by default.
    assert!(decode(sig, ch.rate, json!({})).is_empty());
}

#[test]
fn analyze_summarises_unknown_fsk() {
    // NRZ at 100 µs, 64 bits of 0xAA preamble + data, ±40 kHz.
    let mut bytes = vec![0xAA; 4];
    bytes.extend([0x2D, 0xD4, 0x12, 0x34]);
    let levels = nrz_levels(&bits_of(&bytes, 64));
    let ch = Channel {
        jitter: 0.0,
        ..Channel::new(1e6)
    };
    let w = runs(&levels, 100.0);
    let sig = fsk(&w, 40e3, &ch);
    let lines = decode(sig, ch.rate, json!({ "analyze": true }));
    let l: Vec<_> = lines
        .iter()
        .filter(|l| field(l, "mod") == Some("FSK"))
        .collect();
    assert_eq!(l.len(), 1, "{lines:?}");
    let f1: f64 = field(l[0], "f1_kHz").unwrap().parse().unwrap();
    let f2: f64 = field(l[0], "f2_kHz").unwrap().parse().unwrap();
    let off = ch.offset_hz / 1e3;
    assert!(
        (f1 - off - 40.0).abs() < 8.0 && (f2 - off + 40.0).abs() < 8.0,
        "{f1} {f2}"
    );
    let short: f64 = field(l[0], "short_us").unwrap().parse().unwrap();
    assert!((short - 100.0).abs() < 15.0, "{}", l[0]);
}

#[test]
fn the_examples_build_and_configure() {
    for toml in [
        include_str!("../../../examples/ism-433.fox.toml"),
        include_str!("../../../examples/ism-915.fox.toml"),
    ] {
        let spec = foxygnal_core::GraphSpec::from_toml(toml).unwrap();
        // The SDR source can't be opened here: check everything else.
        let reg = foxygnal_blocks::registry();
        for b in &spec.blocks {
            if b.kind != "sdr_source" {
                reg.build(&b.kind, &b.params).unwrap();
            }
        }
    }
}

#[test]
fn device_selection() {
    let reg = foxygnal_blocks::registry();
    for ok in ["all", "acurite_609,oregon", "-ev1527,-pt2262"] {
        reg.build("ism_decoder", json!({ "devices": ok }).as_object().unwrap())
            .unwrap();
    }
    let err = reg
        .build(
            "ism_decoder",
            json!({ "devices": "nope" }).as_object().unwrap(),
        )
        .err()
        .unwrap();
    assert!(err.to_string().contains("nope"), "{err}");
}
