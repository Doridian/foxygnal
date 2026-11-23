//! LaCrosse sensors.
//!
//! - **TX141 family** (TX141TH-Bv2, TX141-Bv2/Bv3, TX141B, TX141W): OOK PWM
//!   with a 625 µs bit period, a 1 being a 417 µs pulse and a 0 a 208 µs one
//!   (so the bits are inverted against the slicer's "short = 1"). Each packet
//!   starts with four 833 µs sync pulses; packets repeat 4–12 times in a
//!   burst. 40 bits for the TX141TH-Bv2: `II FT TT HH DD` with ID, flags
//!   (battery low, test, 2-bit channel), temperature in 0.1 °C offset by 500,
//!   humidity, and an 8-bit LFSR digest (generator 0x31, key 0xF4, reflected)
//!   over the first four bytes. Temperature-only models send 32–37 bits and
//!   rely on repeats; the TX141W sends 65 bits closed by a CRC-8 (0x31).
//! - **IT+** (TX29-IT 17.24 kbit/s, TX35DTH-IT 9.58 kbit/s): FSK NRZ,
//!   preamble `AA…`, sync `2DD4`, then 40 bits `9I IN TT TH CC`: model nibble 9,
//!   6-bit ID, new-battery flag, BCD temperature (0.1 °C, offset 40),
//!   weak-battery flag and humidity (0x6A: no humidity sensor), and a CRC-8
//!   (poly 0x31, init 0) over the first four bytes.

use super::super::bits::{BitBuffer, crc8, lfsr_digest8_reflect};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const TX141: Device = Device {
    key: "tx141",
    label: "LaCrosse TX141-Bv2/Bv3, TX141TH-Bv2, TX141W",
    modulation: Modulation::OokPwm,
    timing: timing(208.0, 417.0, 833.0, 625.0, 1700.0, 0.0),
    decode: decode_tx141,
};

fn decode_tx141(bb: &BitBuffer, scratch: &mut BitBuffer, out: &mut Out) {
    // A long pulse is a 1: invert the slicer's "short = 1".
    scratch.clone_from(bb);
    scratch.invert();
    let bb = &*scratch;
    let rows = bb.num_rows();
    // rtl_433 asks for 5 repeats when there are more than 5 rows, but noise
    // ahead of a burst adds rows here: 3 identical rows are plenty.
    let mut r = bb.find_repeated_row(3, 32);
    if r.is_none() {
        r = bb.find_repeated_row(2, 64);
    }
    if r.is_none() && rows <= 4 {
        // The TX141TH has its own check: one good row is enough.
        r = (0..rows).find(|&i| {
            matches!(bb.bits(i), 40 | 41)
                && lfsr_digest8_reflect(&bb.row(i)[..4], 0x31, 0xF4) == bb.row(i)[4]
        });
    }
    let Some(r) = r else { return };
    let bits = bb.bits(r);
    let b = bb.row_full(r);
    if bits >= 64 {
        tx141w(b, out);
        return;
    }
    if bits > 41 || (bits == 41 && rows > 12) {
        return;
    }
    let th = bits >= 40;
    let id = b[0];
    // The battery bit means "low" on the TH, "ok" on the others.
    let battery_ok = if th { b[1] >> 7 == 0 } else { b[1] >> 7 == 1 };
    let test = if b[1] & 0x40 != 0 { "Yes" } else { "No" };
    let channel = (b[1] >> 4) & 3;
    let raw = i32::from(b[1] & 0x0F) << 8 | i32::from(b[2]);
    let temp = f64::from(raw - 500) * 0.1;
    let humidity = b[3];
    if id == 0 || (th && (humidity == 0 || humidity > 100)) || !(-40.0..=140.0).contains(&temp) {
        return;
    }
    if th {
        if lfsr_digest8_reflect(&b[..4], 0x31, 0xF4) != b[4] {
            return;
        }
        out.begin("LaCrosse-TX141THBv2")
            .kv("id", id)
            .kv("channel", channel)
            .kv("battery_ok", u8::from(battery_ok))
            .f("temperature_C", temp, 1)
            .kv("humidity", humidity)
            .kv("test", test)
            .mic("CRC");
        return;
    }
    let model = match bits {
        32 => "LaCrosse-TX141B",
        37..=39 => "LaCrosse-TX141Bv2",
        _ => "LaCrosse-TX141Bv3",
    };
    let mut line = out.begin(model).kv("id", id);
    if bits != 32 {
        line = line.kv("channel", channel);
    }
    drop(
        line.kv("battery_ok", u8::from(battery_ok))
            .f("temperature_C", temp, 1)
            .kv("test", test),
    );
}

fn tx141w(b: &[u8], out: &mut Out) {
    if b[0] >> 3 != 1 || crc8(&b[..8], 0x31, 0) != 0 {
        return;
    }
    let id = u32::from(b[0] & 7) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
    let battery_ok = u8::from(b[3] >> 7 == 0);
    let test = (b[3] >> 6) & 1;
    let channel = (b[3] >> 4) & 3;
    let kind = b[3] & 0x0F;
    if kind != 1 && kind != 2 {
        return;
    }
    let a = u16::from(b[4]) << 4 | u16::from(b[5] >> 4);
    let c = u16::from(b[5] & 0x0F) << 8 | u16::from(b[6]);
    let line = out
        .begin("LaCrosse-TX141W")
        .kv("id", id)
        .kv("channel", channel)
        .kv("battery_ok", battery_ok);
    match kind {
        1 => drop(
            line.f("temperature_C", (f64::from(a) - 500.0) * 0.1, 1)
                .kv("humidity", c)
                .kv("test", test)
                .mic("CRC"),
        ),
        _ => drop(
            line.f("wind_avg_km_h", f64::from(a) * 0.1, 1)
                .kv("wind_dir_deg", c)
                .kv("test", test)
                .mic("CRC"),
        ),
    }
}

pub const TX29: Device = Device {
    key: "tx29",
    label: "LaCrosse TX29-IT / TFA 30.3159.IT (IT+, 17.24 kbit/s)",
    modulation: Modulation::FskPcm,
    timing: timing(58.0, 58.0, 0.0, 0.0, 4000.0, 0.0),
    decode: decode_tx29,
};

pub const TX35: Device = Device {
    key: "tx35",
    label: "LaCrosse TX35DTH-IT / TFA 30.3155 (IT+, 9.58 kbit/s)",
    modulation: Modulation::FskPcm,
    timing: timing(105.0, 105.0, 0.0, 0.0, 4000.0, 0.0),
    decode: decode_tx35,
};

fn decode_tx29(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    lacrosse_it(bb, out, "LaCrosse-TX29IT");
}

fn decode_tx35(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    lacrosse_it(bb, out, "LaCrosse-TX35DTHIT");
}

/// Last preamble nibble, sync word 2DD4, model nibble 9.
const IT_SYNC: [u8; 3] = [0xA2, 0xDD, 0x49];

fn lacrosse_it(bb: &BitBuffer, out: &mut Out, model: &str) {
    for r in 0..bb.num_rows() {
        let pos = bb.search(r, 0, &IT_SYNC, 24);
        // Trailing zeros are the idle level and the slicer drops them, so
        // the CRC byte may come up to 7 bits short.
        if pos + 20 + 40 > bb.bits(r) + 7 {
            continue;
        }
        let mut b = [0u8; 5];
        bb.extract(r, pos + 20, &mut b, 40);
        if crc8(&b[..4], 0x31, 0) != b[4] {
            continue;
        }
        let bcd = [b[1] & 0x0F, b[2] >> 4, b[2] & 0x0F];
        if bcd.iter().any(|&d| d > 9) {
            continue;
        }
        let mut id = (b[0] & 0x0F) << 2 | b[1] >> 6;
        let new_battery = (b[1] >> 5) & 1;
        let battery_ok = u8::from(b[3] >> 7 == 0);
        let humidity = b[3] & 0x7F;
        let temp = f64::from(bcd[0]) * 10.0 + f64::from(bcd[1]) + f64::from(bcd[2]) * 0.1 - 40.0;
        // 0x6A: no humidity sensor; 0x7D: a second probe (TX25U).
        let no_hum = humidity == 0x6A || humidity == 0x7D;
        if humidity == 0x7D {
            id += 0x40;
        }
        let line = out
            .begin(model)
            .kv("id", id)
            .kv("battery_ok", battery_ok)
            .kv("newbattery", new_battery)
            .f("temperature_C", temp, 1);
        if no_hum {
            drop(line.mic("CRC"));
        } else {
            drop(line.kv("humidity", humidity).mic("CRC"));
        }
    }
}
