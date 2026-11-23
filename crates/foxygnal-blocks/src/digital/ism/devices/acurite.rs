//! Acurite sensors.
//!
//! - **TXR family** (592TXR tower, 5n1, Atlas, 3n1, 899 rain gauge): OOK
//!   PWM, a 220 µs pulse for a 0 and a 408 µs pulse for a 1 (in a ~612 µs
//!   period), after four 620 µs sync pulses; each message is sent three
//!   times. Byte 0 is `CC` channel (11 = A, 10 = B, 00 = C) and the top of
//!   the ID, byte 1 the rest of the ID, byte 2 `pBMMMMMM` (parity, battery
//!   OK, message type), then data bytes whose top bit is a parity bit, and
//!   last the 8-bit sum of the other bytes. Bytes 2 up to the checksum have
//!   even parity (checked together).
//!   - 0x04 tower (7 bytes): 14-bit ID, humidity, 14-bit temperature
//!     `(°C + 100) × 10` from two 7-bit halves.
//!   - 0x31 / 0x38 5n1 (8 bytes): 12-bit ID with a 2-bit sequence number;
//!     wind speed (8 bits, ×0.8278 + 1 km/h), then either wind direction
//!     (a 4-bit code through a lookup table) and the rain counter (0.01 in),
//!     or temperature (`°F × 10 + 400`, 11 bits) and humidity.
//!   - 0x05–0x07 Atlas (8 bytes; 0x25–0x27 with the lightning sensor, 10):
//!     10-bit ID, 2-bit copy number, wind speed in mph, then temperature and
//!     humidity, wind direction and rain, or UV index and light; lightning
//!     strike count and distance in the extra bytes.
//!   - 0x20 3n1 (8 bytes, checksum only): temperature `(°F × 10) + 1480`
//!     in 12 bits, humidity, wind speed in mph.
//!   - 0x30 899 rain gauge (8 bytes, no channel check): rain counter in
//!     0.01 in tips.
//! - **606TX** (temperature): OOK PPM, 2000 / 4000 µs gaps, 32 bits (plus a
//!   final bit) repeated six times: ID, `BbCCTTTT` (battery OK, button,
//!   channel, top of a 12-bit signed temperature in 0.1 °C), the rest of the
//!   temperature, and an LFSR digest (generator 0x98, key 0xF1).
//! - **609TXC** (temperature / humidity): OOK PPM, 1000 / 2000 µs gaps,
//!   40 bits `II ST TT HH CC`: ID (new at each power-up), status (bit 3 =
//!   battery low), 12-bit signed temperature in 0.1 °C, humidity, and the
//!   sum of the first four bytes.
//! - **986** (refrigerator / freezer): OOK PPM, 520 / 880 µs gaps, 40 bits
//!   sent least significant bit first: signed-magnitude temperature in °F,
//!   16-bit ID, status (bit 0 = sensor 2, bit 1 = battery low), and a
//!   reflected CRC-8 (polynomial 0x07). Sent twice.

use super::super::bits::{BitBuffer, add_bytes, crc8le, lfsr_digest8, parity};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const TXR: Device = Device {
    key: "acurite_txr",
    label: "Acurite 592TXR / 5n1 / Atlas / 3n1 / 899",
    modulation: Modulation::OokPwm,
    timing: timing(220.0, 408.0, 620.0, 500.0, 4000.0, 0.0),
    decode: decode_acurite_txr,
};

/// Channel letter from the top two bits of byte 0 (`01` is invalid).
fn channel(b0: u8) -> Option<&'static str> {
    match b0 >> 6 {
        3 => Some("A"),
        2 => Some("B"),
        0 => Some("C"),
        _ => None,
    }
}

/// 5n1 wind direction code → direction in units of 22.5°.
const WIND_5N1: [u8; 16] = [14, 11, 13, 12, 15, 10, 0, 9, 3, 6, 4, 5, 2, 7, 1, 8];

/// Checksum (last of `len` bytes) and the joint even parity of bytes 2 up
/// to the checksum.
fn txr_valid(b: &[u8], len: usize, check_parity: bool) -> bool {
    if b.len() < len || add_bytes(&b[..len - 1]) & 0xFF != u32::from(b[len - 1]) {
        return false;
    }
    !check_parity || parity(&b[2..len - 1]) == 0
}

fn decode_acurite_txr(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let n = bb.bits(r) / 8;
        if !(6..=10).contains(&n) {
            continue;
        }
        // The PWM slicer reads a short pulse as 1; Acurite's short pulse is a 0.
        let mut b = [0u8; 10];
        for (d, s) in b.iter_mut().zip(&bb.row(r)[..n]) {
            *d = !s;
        }
        let b = &b[..n];
        if b[0] == 0 && b[1] == 0 && b[2] == 0 && b[n - 1] == 0 {
            continue;
        }
        let battery_ok = u8::from(b[2] & 0x40 != 0);
        match b[2] & 0x3F {
            0x04 => {
                let Some(ch) = channel(b[0]).filter(|_| txr_valid(b, 7, true)) else {
                    continue;
                };
                let id = u16::from(b[0] & 0x3F) << 8 | u16::from(b[1]);
                let humidity = b[3] & 0x7F;
                let raw = i32::from(b[4] & 0x7F) << 7 | i32::from(b[5] & 0x7F);
                let t = f64::from(raw - 1000) * 0.1;
                if (humidity > 100 && humidity != 127) || !(-40.0..=70.0).contains(&t) {
                    continue;
                }
                let line = out
                    .begin("Acurite-Tower")
                    .kv("id", id)
                    .kv("channel", ch)
                    .kv("battery_ok", battery_ok)
                    .f("temperature_C", t, 1);
                let line = if humidity != 127 {
                    line.kv("humidity", humidity)
                } else {
                    line
                };
                line.mic("CHECKSUM");
            }
            t @ (0x31 | 0x38) => {
                let Some(ch) = channel(b[0]).filter(|_| txr_valid(b, 8, true)) else {
                    continue;
                };
                let id = u16::from(b[0] & 0x0F) << 8 | u16::from(b[1]);
                let seq = (b[0] >> 4) & 3;
                let raw = u16::from(b[3] & 0x1F) << 3 | u16::from((b[4] & 0x70) >> 4);
                let kmh = if raw > 0 {
                    f64::from(raw) * 0.8278 + 1.0
                } else {
                    0.0
                };
                let raw_t = i32::from(b[4] & 0x0F) << 7 | i32::from(b[5] & 0x7F);
                let f = f64::from(raw_t - 400) * 0.1;
                let humidity = b[6] & 0x7F;
                if t == 0x38 && (!(-40.0..=158.0).contains(&f) || humidity > 100) {
                    continue;
                }
                let line = out
                    .begin("Acurite-5n1")
                    .kv("message_type", t)
                    .kv("id", id)
                    .kv("channel", ch)
                    .kv("sequence_num", seq)
                    .kv("battery_ok", battery_ok)
                    .f("wind_avg_km_h", kmh, 1);
                if t == 0x31 {
                    let dir = f64::from(WIND_5N1[usize::from(b[4] & 0x0F)]) * 22.5;
                    let rain = u16::from(b[5] & 0x7F) << 7 | u16::from(b[6] & 0x7F);
                    line.f("wind_dir_deg", dir, 1)
                        .f("rain_in", f64::from(rain) * 0.01, 2)
                        .mic("CHECKSUM");
                } else {
                    line.f("temperature_F", f, 1)
                        .kv("humidity", humidity)
                        .mic("CHECKSUM");
                }
            }
            t @ (0x05..=0x07 | 0x25..=0x27) => {
                let len = if t & 0x20 != 0 { 10 } else { 8 };
                let Some(ch) = channel(b[0]).filter(|_| txr_valid(b, len, true)) else {
                    continue;
                };
                atlas(b, t, ch, battery_ok, out);
            }
            0x20 => {
                let Some(ch) = channel(b[0]).filter(|_| txr_valid(b, 8, false)) else {
                    continue;
                };
                let id = u16::from(b[0] & 0x3F) << 8 | u16::from(b[1]);
                let humidity = b[3] & 0x7F;
                let raw = i32::from(b[4] & 0x1F) << 7 | i32::from(b[5] & 0x7F);
                let f = f64::from(raw - 1480) * 0.1;
                if humidity > 100 || !(-40.0..=158.0).contains(&f) {
                    continue;
                }
                out.begin("Acurite-3n1")
                    .kv("message_type", 0x20)
                    .kv("id", id)
                    .kv("channel", ch)
                    .kv("sequence_num", (b[0] >> 4) & 3)
                    .kv("battery_ok", battery_ok)
                    .f("wind_avg_mi_h", f64::from(b[6] & 0x7F), 1)
                    .f("temperature_F", f, 1)
                    .kv("humidity", humidity)
                    .mic("CHECKSUM");
            }
            0x30 => {
                if !txr_valid(b, 8, true) {
                    continue;
                }
                let id = u16::from(b[0] & 0x3F) << 8 | u16::from(b[1]);
                let rain = u16::from(b[5] & 0x7F) << 7 | u16::from(b[6] & 0x7F);
                out.begin("Acurite-Rain899")
                    .kv("id", id)
                    .kv("channel", b[0] >> 6)
                    .kv("battery_ok", battery_ok)
                    .f("rain_mm", f64::from(rain) * 0.254, 2)
                    .mic("CHECKSUM");
            }
            _ => {}
        }
    }
}

/// Atlas (7-in-1) messages, checksum and parity already checked.
fn atlas(b: &[u8], t: u8, ch: &str, battery_ok: u8, out: &mut Out) {
    let id = u16::from(b[0] & 0x03) << 8 | u16::from(b[1]);
    let seq = (b[0] >> 2) & 3;
    let mph = u16::from(b[3] & 0x7F) << 1 | u16::from((b[4] & 0x40) >> 6);
    if mph > 200 {
        return;
    }
    let mut exception = 0;
    // Check the data before writing anything.
    match t & 0x1F {
        0x05 => {
            let raw = i32::from(b[4] & 0x0F) << 7 | i32::from(b[5] & 0x7F);
            let f = f64::from(raw - 400) * 0.1;
            if !(-40.0..=158.0).contains(&f) || b[6] & 0x7F > 100 {
                return;
            }
        }
        0x06 => {
            let dir = u16::from(b[4] & 0x1F) << 5 | u16::from((b[5] & 0x7C) >> 2);
            if dir > 360 {
                return;
            }
        }
        _ => {
            let lux = u16::from(b[5] & 0x7F) << 7 | u16::from(b[6] & 0x7F);
            if lux > 12000 {
                return;
            }
        }
    }
    if t & 0x1F != 0x07 && b[4] & 0x30 != 0 {
        exception += 1;
    }
    let mut line = out
        .begin("Acurite-Atlas")
        .kv("id", id)
        .kv("channel", ch)
        .kv("sequence_num", seq)
        .kv("battery_ok", battery_ok)
        .kv("message_type", t)
        .f("wind_avg_mi_h", f64::from(mph), 1);
    match t & 0x1F {
        0x05 => {
            let raw = i32::from(b[4] & 0x0F) << 7 | i32::from(b[5] & 0x7F);
            let humidity = b[6] & 0x7F;
            if humidity == 0 {
                exception += 1;
            }
            line = line
                .f("temperature_F", f64::from(raw - 400) * 0.1, 1)
                .kv("humidity", humidity);
        }
        0x06 => {
            let dir = u16::from(b[4] & 0x1F) << 5 | u16::from((b[5] & 0x7C) >> 2);
            let rain = u16::from(b[5] & 0x03) << 7 | u16::from(b[6] & 0x7F);
            line =
                line.f("wind_dir_deg", f64::from(dir), 1)
                    .f("rain_in", f64::from(rain) * 0.01, 2);
        }
        _ => {
            let lux = u32::from(b[5] & 0x7F) << 7 | u32::from(b[6] & 0x7F);
            line = line.kv("uvi", b[4] & 0x0F).kv("lux", lux * 10);
        }
    }
    if t & 0x20 != 0 {
        let count = u16::from(b[7] & 0x7F) << 2 | u16::from((b[8] & 0x60) >> 5);
        line = line
            .kv("strike_count", count)
            .kv("strike_distance", b[8] & 0x1F);
    }
    line.kv("exception", exception).mic("CHECKSUM");
}

pub const T606: Device = Device {
    key: "acurite_606",
    label: "Acurite 606TX",
    modulation: Modulation::OokPpm,
    timing: timing(2000.0, 4000.0, 0.0, 7000.0, 10000.0, 0.0),
    decode: decode_acurite_606,
};

fn decode_acurite_606(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    let Some(r) = bb.find_repeated_row(3, 32) else {
        return;
    };
    if bb.bits(r) > 33 {
        return;
    }
    let b = bb.row_full(r);
    if b[..4] == [0; 4] || lfsr_digest8(&b[..3], 0x98, 0xF1) != b[3] {
        return;
    }
    let raw = i16::from_be_bytes([b[1] << 4 | b[2] >> 4, b[2] << 4]) >> 4;
    out.begin("Acurite-606TX")
        .kv("id", b[0])
        .kv("channel", ((b[1] >> 4) & 3) + 1)
        .kv("battery_ok", b[1] >> 7)
        .kv("button", (b[1] >> 6) & 1)
        .f("temperature_C", f64::from(raw) * 0.1, 1)
        .mic("CHECKSUM");
}

pub const T609: Device = Device {
    key: "acurite_609",
    label: "Acurite 609TXC",
    modulation: Modulation::OokPpm,
    timing: timing(1000.0, 2000.0, 0.0, 3000.0, 10000.0, 0.0),
    decode: decode_acurite_609,
};

fn decode_acurite_609(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        if bb.bits(r) != 40 {
            continue;
        }
        let b = bb.row(r);
        let sum = add_bytes(&b[..4]);
        if sum == 0 || sum & 0xFF != u32::from(b[4]) {
            continue;
        }
        let humidity = b[3];
        if humidity > 100 {
            continue;
        }
        // 12-bit two's complement, tenths of a degree.
        let raw = i16::from_be_bytes([(b[1] & 0x0F) << 4 | b[2] >> 4, b[2] << 4]) >> 4;
        let status = b[1] >> 4;
        out.begin("Acurite-609TXC")
            .kv("id", b[0])
            .kv("battery_ok", u8::from(status & 8 == 0))
            .f("temperature_C", f64::from(raw) * 0.1, 1)
            .kv("humidity", humidity)
            .kv("status", status)
            .mic("CHECKSUM");
    }
}

pub const T986: Device = Device {
    key: "acurite_986",
    label: "Acurite 986 refrigerator / freezer",
    modulation: Modulation::OokPpm,
    timing: timing(520.0, 880.0, 0.0, 1280.0, 4000.0, 0.0),
    decode: decode_acurite_986,
};

fn decode_acurite_986(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        if !(39..=43).contains(&bb.bits(r)) {
            continue;
        }
        let row = bb.row_full(r);
        if row[..3] == [0xFF; 3] || row[..3] == [0; 3] {
            continue;
        }
        // Sent least significant bit first.
        let mut b = [0u8; 5];
        for (d, s) in b.iter_mut().zip(row) {
            *d = s.reverse_bits();
        }
        // The last bit is sometimes lost (a trailing 1 read as missing).
        let crc = crc8le(&b[..4], 0xE0, 0);
        if crc != b[4] && crc != b[4] | 0x80 {
            continue;
        }
        let t = if b[0] & 0x80 != 0 {
            -i16::from(b[0] & 0x7F)
        } else {
            i16::from(b[0])
        };
        let id = u16::from(b[1]) << 8 | u16::from(b[2]);
        let sensor2 = b[3] & 1 != 0;
        let status = b[3] >> 1;
        out.begin("Acurite-986")
            .kv("id", id)
            .kv("channel", if sensor2 { "2F" } else { "1R" })
            .kv("battery_ok", u8::from(status & 1 == 0))
            .f("temperature_F", f64::from(t), 1)
            .kv("status", status)
            .mic("CRC");
    }
}
