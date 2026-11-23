//! Cheap 36-bit PPM temperature sensors: Nexus, Rubicson and Prologue.
//!
//! All three send a ~500 µs pulse per bit and put the bit in the gap after
//! it; a long sync gap separates the 7–12 repeats of the message.
//!
//! - **Nexus** (FreeTec NC-7345, TFA 30.3209, Solight TE82S, …): gaps 1000 /
//!   2000 µs, sync 4000 µs. Nibbles `I I F T T T C H H`: ID, flags (battery
//!   ok, test, 2-bit channel), 12-bit signed temperature in 0.1 °C, a
//!   constant 0xF and humidity (0 on temperature-only sensors). No check
//!   beyond the constant nibble and the repeats.
//! - **Rubicson** (TFA 30.3197, InFactory PT-310): Nexus' timing and layout,
//!   but the last byte is a CRC-8 (poly 0x31, init 0x6C) over the first
//!   seven nibbles. A Nexus message whose humidity happens to satisfy that
//!   CRC is taken as Rubicson, so the two never both decode the same
//!   message.
//! - **Prologue** (FreeTec NC-7104, ThermoPro TX2): gaps 2000 / 4000 µs, sync
//!   9000 µs. Nibbles `Y I I F T T T H H`: type (9 or 5), ID, flags (battery
//!   ok, button, channel), temperature, humidity (0xCC: none).

use super::super::bits::{BitBuffer, crc8};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const NEXUS: Device = Device {
    key: "nexus",
    label: "Nexus / FreeTec NC-7345 / TFA 30.3209",
    modulation: Modulation::OokPpm,
    timing: timing(1000.0, 2000.0, 0.0, 3000.0, 5000.0, 0.0),
    decode: decode_nexus,
};

/// The Rubicson CRC over a Nexus-layout message: 0 if it checks.
fn rubicson_crc(b: &[u8]) -> u8 {
    let tmp = [
        b[0],
        b[1],
        b[2],
        b[3] & 0xF0,
        (b[3] & 0x0F) << 4 | b[4] >> 4,
    ];
    crc8(&tmp, 0x31, 0x6C)
}

/// 12-bit signed temperature from nibbles 3–5 (bytes 1–2), 0.1 °C.
fn temp12(b: &[u8]) -> f64 {
    let raw = i16::from_be_bytes([b[1] << 4 | b[2] >> 4, b[2] << 4]) >> 4;
    f64::from(raw) * 0.1
}

fn decode_nexus(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    let Some(r) = bb.find_repeated_row(3, 36) else {
        return;
    };
    if bb.bits(r) > 37 {
        return;
    }
    let b = bb.row_full(r);
    if b[3] & 0xF0 != 0xF0
        || (b[0] == 0 && b[2] == 0 && b[3] == 0)
        || (b[0] == 0xFF && b[2] == 0xFF && b[3] == 0xFF)
        || b[1] & 0x30 == 0x30
        || rubicson_crc(b) == 0
    {
        return;
    }
    let humidity = (b[3] & 0x0F) << 4 | b[4] >> 4;
    if humidity > 100 {
        return;
    }
    let line = out
        .begin(if humidity == 0 { "Nexus-T" } else { "Nexus-TH" })
        .kv("id", b[0])
        .kv("channel", ((b[1] >> 4) & 3) + 1)
        .kv("battery_ok", b[1] >> 7)
        .f("temperature_C", temp12(b), 1);
    let line = if humidity == 0 {
        line
    } else {
        line.kv("humidity", humidity)
    };
    if b[1] & 0x40 != 0 {
        drop(line.kv("test", 1));
    }
}

pub const RUBICSON: Device = Device {
    key: "rubicson",
    label: "Rubicson / TFA 30.3197 / InFactory PT-310",
    modulation: Modulation::OokPpm,
    timing: timing(1000.0, 2000.0, 0.0, 3000.0, 4800.0, 0.0),
    decode: decode_rubicson,
};

fn decode_rubicson(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    let Some(r) = bb.find_repeated_row(3, 36) else {
        return;
    };
    if !(36..=38).contains(&bb.bits(r)) {
        return;
    }
    let b = bb.row_full(r);
    if b[3] & 0xF0 != 0xF0 || rubicson_crc(b) != 0 {
        return;
    }
    out.begin("Rubicson-Temperature")
        .kv("id", b[0])
        .kv("channel", ((b[1] >> 4) & 3) + 1)
        .kv("battery_ok", b[1] >> 7)
        .f("temperature_C", temp12(b), 1)
        .mic("CRC");
}

pub const PROLOGUE: Device = Device {
    key: "prologue",
    label: "Prologue / FreeTec NC-7104 / ThermoPro TX2",
    modulation: Modulation::OokPpm,
    timing: timing(2000.0, 4000.0, 0.0, 7000.0, 10000.0, 0.0),
    decode: decode_prologue,
};

fn decode_prologue(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    // Alecto/Auriol sends 8 sync bits first: not one of these. (Shorter
    // first rows are noise blips before the packet.)
    if bb.num_rows() > 0 && (4..=8).contains(&bb.bits(0)) {
        return;
    }
    let Some(r) = bb.find_repeated_row(4, 36) else {
        return;
    };
    if bb.bits(r) > 37 {
        return;
    }
    let b = bb.row_full(r);
    let kind = b[0] >> 4;
    if kind != 9 && kind != 5 {
        return;
    }
    let raw = i16::from_be_bytes([b[2], b[3] & 0xF0]) >> 4;
    let humidity = (b[3] & 0x0F) << 4 | b[4] >> 4;
    let line = out
        .begin("Prologue-TH")
        .kv("subtype", kind)
        .kv("id", (b[0] & 0x0F) << 4 | b[1] >> 4)
        .kv("channel", (b[1] & 3) + 1)
        .kv("battery_ok", (b[1] >> 3) & 1)
        .f("temperature_C", f64::from(raw) * 0.1, 1);
    let line = if humidity == 0xCC {
        line
    } else {
        line.kv("humidity", humidity)
    };
    drop(line.kv("button", (b[1] >> 2) & 1));
}
