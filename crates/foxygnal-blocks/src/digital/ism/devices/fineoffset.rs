//! Fine Offset Electronics weather stations (sold as Ambient Weather, Ecowitt,
//! Misol, Froggit, …).
//!
//! - **WH24 / WH65 / WS69** outdoor units: FSK NRZ at 58 µs per bit,
//!   preamble `AA…`, sync `2DD4`, then 17 bytes
//!   `FF II DD VT TT HH WW GG RR RR UU UU LL LL LL CC BB`: family 0x24, ID,
//!   wind direction (9 bits), flags (battery low, wind speed bit 8),
//!   temperature (11 bits, 0.1 °C offset 40), humidity, wind speed (1/8
//!   units), gust, rain tip counter, UV, light (0.1 lux), a CRC-8 (poly 0x31)
//!   over the first 15 bytes and their sum. The WH24 and WH65 differ in
//!   wind and rain scale; rtl_433 tells them apart by the preamble and
//!   postamble lengths, and so does this. The WS69 adds 8 bytes with
//!   pressure and a second CRC and sum.
//! - **WH1080 / WH3080**: OOK PWM, a 1 is a 544 µs pulse, a 0 a 1524 µs
//!   one, each followed by a ~1 ms gap. 88 bits: preamble `FF`, then
//!   `FI IT TT HH SS GG ?R RR BD CC`: message type (0xA weather, 0xB radio
//!   clock), station ID, temperature (10 bits, 0.1 °C offset 40), humidity,
//!   wind speed and gust (0.34 m/s), rain counter (0.3 mm), battery and wind
//!   direction (16 points), and a CRC-8 (poly 0x31, init 0xFF over the
//!   preamble too). The WH3080's UV / light sensor sends 64-bit messages of
//!   type 0x7: `F7 SU XX LL LL LL CC` with sensor ID, UV index and light in
//!   0.1 lux, with the same CRC.

use super::super::bits::{BitBuffer, add_bytes, crc8};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const WH24: Device = Device {
    key: "wh24",
    label: "Fine Offset WH24 / WH65 / WS69 (Ambient WS-2902 outdoor unit, Misol WS2320)",
    modulation: Modulation::FskPcm,
    timing: timing(58.0, 58.0, 0.0, 0.0, 20000.0, 0.0),
    decode: decode_wh24,
};

const SYNC: [u8; 3] = [0xAA, 0x2D, 0xD4];

/// UV raw value → UV index upper bounds.
const UVI_UPPER: [u16; 13] = [
    432, 851, 1210, 1570, 2017, 2450, 2761, 3100, 3512, 3918, 4277, 4650, 5029,
];

fn decode_wh24(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let bits = bb.bits(r);
        if !(190..=320).contains(&bits) {
            continue;
        }
        let off = bb.search(r, 0, &SYNC, 24) + 24;
        // Trailing zeros are the idle level and the slicer drops them, so
        // the sum byte may come up to 7 bits short.
        if off + 17 * 8 > bits + 7 {
            continue;
        }
        let mut b = [0u8; 25];
        bb.extract(r, off, &mut b, 25 * 8);
        if b[0] != 0x24
            || crc8(&b[..16], 0x31, 0) != 0
            || add_bytes(&b[..16]) & 0xFF != u32::from(b[16])
        {
            continue;
        }
        let tail = bits.saturating_sub(off + 17 * 8);
        let ws69 = tail >= 8 * 8
            && crc8(&b[..24], 0x31, 0) == 0
            && add_bytes(&b[..24]) & 0xFF == u32::from(b[24]);
        // WH24: ~57 bits before the payload and 3 after; WH65: 61 and 12.
        let wh24 = !ws69 && tail < 10 && off < 64;
        let (model, wind_k, rain_k) = if ws69 {
            ("Fineoffset-WS69", 0.51, 0.254)
        } else if wh24 {
            ("Fineoffset-WH24", 1.12, 0.3)
        } else {
            ("Fineoffset-WH65B", 0.51, 0.254)
        };
        let wind_dir = u16::from(b[2]) | u16::from(b[3] & 0x80) << 1;
        let battery_ok = u8::from(b[3] & 0x08 == 0);
        let temp_raw = u16::from(b[3] & 0x07) << 8 | u16::from(b[4]);
        let humidity = b[5];
        let wind_raw = u16::from(b[6]) | u16::from(b[3] & 0x10) << 4;
        let gust_raw = b[7];
        let rain = u16::from_be_bytes([b[8], b[9]]);
        let uv = u16::from_be_bytes([b[10], b[11]]);
        let light = u32::from(b[12]) << 16 | u32::from(b[13]) << 8 | u32::from(b[14]);
        let mut line = out.begin(model).kv("id", b[1]).kv("battery_ok", battery_ok);
        if temp_raw != 0x7FF {
            line = line.f("temperature_C", (f64::from(temp_raw) - 400.0) * 0.1, 1);
        }
        if humidity != 0xFF {
            line = line.kv("humidity", humidity);
        }
        if ws69 {
            let p = u32::from(b[17]) << 16 | u32::from(b[18]) << 8 | u32::from(b[19]);
            if p < 0x1FFFF {
                line = line.f("pressure_hPa", f64::from(p) * 0.01, 2);
            }
        }
        if wind_dir != 0x1FF {
            line = line.kv("wind_dir_deg", wind_dir);
        }
        if wind_raw != 0x1FF {
            line = line.f("wind_avg_m_s", f64::from(wind_raw) * 0.125 * wind_k, 1);
        }
        if gust_raw != 0xFF {
            line = line.f("wind_max_m_s", f64::from(gust_raw) * wind_k, 1);
        }
        line = line.f("rain_mm", f64::from(rain) * rain_k, 1);
        if uv != 0xFFFF {
            let uvi = UVI_UPPER.iter().take_while(|&&u| u < uv).count();
            line = line.kv("uv", uv).kv("uvi", uvi);
        }
        if light != 0xFF_FFFF {
            line = line.f("light_lux", f64::from(light) * 0.1, 1);
        }
        drop(line.mic("CRC"));
    }
}

pub const WH1080: Device = Device {
    key: "wh1080",
    label: "Fine Offset WH1080 / WH3080 weather station",
    modulation: Modulation::OokPwm,
    timing: timing(544.0, 1524.0, 0.0, 0.0, 2800.0, 0.0),
    decode: decode_wh1080,
};

/// Wind direction by 16-point code.
const DIRECTION: [u16; 16] = [
    0, 23, 45, 68, 90, 113, 135, 158, 180, 203, 225, 248, 270, 293, 315, 338,
];

fn decode_wh1080(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    // The preamble is 7 or 8 ones, sometimes more, and noise ahead of the
    // packet may join its row: try every place where 7 ones are followed by
    // a known message type.
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        if len < 63 {
            continue;
        }
        for p in 7..=len - 56 {
            let mut h = [0u8; 2];
            bb.extract(r, p - 7, &mut h, 11);
            if h[0] >> 1 != 0x7F {
                continue;
            }
            let n = match (h[0] & 1) << 3 | h[1] >> 5 {
                0xA | 0xB => 88,
                0x7 => 64,
                _ => continue,
            };
            if p + n - 8 > len {
                continue;
            }
            let mut b = [0u8; 11];
            b[0] = 0xFF;
            bb.extract(r, p, &mut b[1..], n - 8);
            if crc8(&b[..n / 8], 0x31, 0xFF) == 0 && wh1080_fields(&b, out) {
                return;
            }
        }
    }
}

/// Write a message's line; false if its fields are out of range.
fn wh1080_fields(b: &[u8; 11], out: &mut Out) -> bool {
    let id = (b[1] << 4) | (b[2] >> 4);
    match b[1] >> 4 {
        0x0A => {
            if b[4] > 100 {
                return false;
            }
            let temp_raw = i32::from(b[2] & 0x03) << 8 | i32::from(b[3]);
            let rain = u16::from(b[7] & 0x0F) << 8 | u16::from(b[8]);
            out.begin("Fineoffset-WHx080")
                .kv("subtype", 0)
                .kv("id", id)
                .kv("battery_ok", u8::from(b[9] >> 4 != 1))
                .f("temperature_C", f64::from(temp_raw - 400) * 0.1, 1)
                .kv("humidity", b[4])
                .kv("wind_dir_deg", DIRECTION[usize::from(b[9] & 0x0F)])
                .f("wind_avg_km_h", f64::from(b[5]) * 0.34 * 3.6, 2)
                .f("wind_max_km_h", f64::from(b[6]) * 0.34 * 3.6, 2)
                .f("rain_mm", f64::from(rain) * 0.3, 1)
                .mic("CRC");
        }
        0x0B => {
            let digits_ok = [b[3] & 0x3F, b[4], b[5], b[6], b[7] & 0x1F, b[8]]
                .iter()
                .all(|&x| x >> 4 <= 9 && x & 0x0F <= 9);
            if !digits_ok {
                return false;
            }
            let bcd = |x: u8, mask: u8| u32::from((x & mask) >> 4) * 10 + u32::from(x & 0x0F);
            let signal = if b[2] & 0x0F == 10 {
                "DCF77"
            } else {
                "WWVB/MSF"
            };
            out.begin("Fineoffset-WHx080")
                .kv("subtype", 1)
                .kv("id", id)
                .kv("signal", signal)
                .kv(
                    "radio_clock",
                    format_args!(
                        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                        2000 + bcd(b[6], 0xF0),
                        bcd(b[7], 0x10),
                        bcd(b[8], 0xF0),
                        bcd(b[3], 0x30),
                        bcd(b[4], 0xF0),
                        bcd(b[5], 0xF0)
                    ),
                )
                .mic("CRC");
        }
        0x07 => {
            // UV / light sensor (WH3080).
            let lux =
                f64::from(u32::from(b[4]) << 16 | u32::from(b[5]) << 8 | u32::from(b[6])) * 0.1;
            out.begin("Fineoffset-WHx080")
                .kv("subtype", 2)
                .kv("uv_sensor_id", id)
                .kv("uv_status", if b[3] == 0x55 { "OK" } else { "ERROR" })
                .kv("uv_index", b[2] & 0x0F)
                .f("lux", lux, 1)
                .f("wm", lux / 683.0, 3)
                .mic("CRC");
        }
        _ => return false,
    }
    true
}
