//! Ambient Weather sensors.
//!
//! - **F007TH / F012TH** (433 MHz thermo-hygrometer, also TFA 30.3208.02 and
//!   SwitchDoc F016TH): OOK Manchester, ~1024 bit/s (488 µs half-bits),
//!   three copies back to back. Each starts with the preamble `0x00145`,
//!   whose last byte (0x45) is the first of six: `xxxxMMMM IIIIIIII
//!   BCCCTTTT TTTTTTTT HHHHHHHH DDDDDDDD`: model, ID (new at power-up),
//!   battery low, channel − 1, temperature `°F × 10 + 400`, humidity, and an
//!   LFSR digest of the first five bytes (generator 0x98, key 0x3E) XOR 0x64.
//! - **WH31E** (915 MHz thermo-hygrometer, also Ecowitt WH31, WH31B): FSK
//!   NRZ, 56 µs bits; a warm-up mark and space, `0xAAAA…` and the sync word
//!   0x2DD4, then `YY II CT TT HH XX AA`: type (0x30, or 0x37 for WH31B), ID,
//!   `1CCC0BTT` (channel − 1, battery low, top of a 10-bit temperature
//!   `°C × 10 + 400`), humidity, CRC-8 (polynomial 0x31) and the 8-bit sum of
//!   the bytes before it, then four more bytes reported as `data`.

use super::super::bits::{BitBuffer, add_bytes, crc8, lfsr_digest8};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const F007TH: Device = Device {
    key: "f007th",
    label: "Ambient Weather F007TH / F012TH",
    modulation: Modulation::OokManchester,
    timing: timing(500.0, 0.0, 0.0, 0.0, 2400.0, 0.0),
    decode: decode_f007th,
};

/// Check and write one F007TH message (six bytes); true if it was valid.
fn f007th_message(b: &[u8; 6], out: &mut Out) -> bool {
    if lfsr_digest8(&b[..5], 0x98, 0x3E) ^ 0x64 != b[5] {
        return false;
    }
    let raw = i32::from(b[2] & 0x0F) << 8 | i32::from(b[3]);
    let f = f64::from(raw - 400) * 0.1;
    let humidity = b[4];
    if humidity > 100 || !(-40.0..344.0).contains(&f) {
        return false;
    }
    out.begin("Ambientweather-F007TH")
        .kv("id", b[1])
        .kv("channel", ((b[2] >> 4) & 7) + 1)
        .kv("battery_ok", u8::from(b[2] & 0x80 == 0))
        .f("temperature_F", f, 1)
        .kv("humidity", humidity)
        .mic("CRC");
    true
}

fn decode_f007th(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    // The preamble's last 12 bits, as sent and as seen inverted.
    for pattern in [[0x01, 0x45], [0xFD, 0x45]] {
        for r in 0..bb.num_rows() {
            let len = bb.bits(r);
            let mut pos = 0;
            loop {
                pos = bb.search(r, pos, &pattern, 12);
                if pos + 8 + 48 > len {
                    break;
                }
                let mut b = [0u8; 6];
                bb.extract(r, pos + 8, &mut b, 48);
                if f007th_message(&b, out) {
                    // One message per packet, as the copies are identical.
                    return;
                }
                pos += 1;
            }
        }
    }
}

pub const WH31E: Device = Device {
    key: "wh31e",
    label: "Ambient Weather WH31E / Ecowitt WH31",
    modulation: Modulation::FskPcm,
    timing: timing(56.0, 56.0, 0.0, 1800.0, 1500.0, 0.0),
    decode: decode_wh31e,
};

fn decode_wh31e(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    const SYNC: [u8; 3] = [0xAA, 0x2D, 0xD4];
    for r in 0..bb.num_rows() {
        let pos = bb.search(r, 0, &SYNC, 24);
        // The row ends at the last mark: trailing zeros of the CRC and sum
        // merge into the space after the burst, so only require the data
        // up to the humidity; the rest reads as zeros and the checks decide.
        if pos + 24 + 5 * 8 > bb.bits(r) {
            continue;
        }
        let mut b = [0u8; 11];
        bb.extract(r, pos + 24, &mut b, 11 * 8);
        let model = match b[0] {
            0x30 => "AmbientWeather-WH31E",
            0x37 => "AmbientWeather-WH31B",
            _ => continue,
        };
        if crc8(&b[..6], 0x31, 0) != 0 || add_bytes(&b[..6]) & 0xFF != u32::from(b[6]) {
            continue;
        }
        let raw = i32::from(b[2] & 0x03) << 8 | i32::from(b[3]);
        out.begin(model)
            .kv("id", b[1])
            .kv("channel", ((b[2] >> 4) & 7) + 1)
            .kv("battery_ok", u8::from(b[2] & 0x04 == 0))
            .f("temperature_C", f64::from(raw - 400) * 0.1, 1)
            .kv("humidity", b[4])
            .bytes("data", &b[6..11])
            .mic("CRC");
    }
}
