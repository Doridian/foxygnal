//! LaCrosse TX-3 / TX-4 / TX-6U / TX-7U thermo-hygrometers and the TX5U
//! rain gauge (433 MHz, OOK): PWM, a 550 µs pulse for 1 and a 1400 µs pulse
//! for 0, each followed by a ~1000 µs gap; 44 bits (11 nibbles), sent twice
//! 30 ms apart:
//!
//! ```text
//! 0  A  T  II  IP  V1 V2 V3  R1 R2  S
//! ```
//!
//! `0A` start the message, `T` is the type (0 = temperature, E = humidity,
//! A = rain), the 7-bit ID spans `II` and the top of `IP`, whose low bit is
//! the parity of the three value nibbles. The value is BCD with one decimal
//! (temperature + 50 °C, or humidity in %), or a 12-bit tip counter (rain);
//! `R1 R2` repeat its first two digits, and `S` is the sum of the other ten
//! nibbles. Temperature and humidity come in separate messages.

use super::super::bits::BitBuffer;
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const LACROSSE_TX: Device = Device {
    key: "lacrosse_tx",
    label: "LaCrosse TX-3 / TX-4 / TX-6U / TX-7U, TX5U rain",
    modulation: Modulation::OokPwm,
    timing: timing(550.0, 1400.0, 0.0, 3000.0, 8000.0, 0.0),
    decode,
};

fn decode(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        // Noise ahead of the message can join its row: look for the `0A`.
        let len = bb.bits(r);
        if !(44..=56).contains(&len) {
            continue;
        }
        for p in 0..=len - 44 {
            let mut row = [0u8; 6];
            bb.extract(r, p, &mut row, 44);
            if row[0] == 0x0A && message(&row, out) {
                break;
            }
        }
    }
}

/// A 44-bit message; true if it was one.
fn message(row: &[u8; 6], out: &mut Out) -> bool {
    let mut n = [0u8; 11];
    for (i, x) in n.iter_mut().enumerate() {
        *x = (row[i / 2] >> (4 * (1 - i % 2))) & 0xF;
    }
    let sum = n[..10].iter().fold(0u8, |a, &x| a.wrapping_add(x)) & 0xF;
    let ones: u32 = n[5..8].iter().map(|x| x.count_ones()).sum::<u32>() + u32::from(n[4] & 1);
    if sum != n[10] || !ones.is_multiple_of(2) || n[5] != n[8] || n[6] != n[9] {
        return false;
    }
    let id = (n[3] << 3) | (n[4] >> 1);
    let raw = u16::from(n[5]) << 8 | u16::from(n[6]) << 4 | u16::from(n[7]);
    let value = f64::from(n[5]) * 10.0 + f64::from(n[6]) + f64::from(n[7]) * 0.1;
    match n[2] {
        0x0 => {
            out.begin("LaCrosse-TX")
                .kv("id", id)
                .f("temperature_C", value - 50.0, 1)
                .mic("PARITY");
        }
        0xE if raw != 0xFF => {
            out.begin("LaCrosse-TX")
                .kv("id", id)
                .f("humidity", value, 1)
                .mic("PARITY");
        }
        0xA => {
            out.begin("LaCrosse-TX5U")
                .kv("id", id)
                .f("rain_in", f64::from(raw) * 0.0105, 4)
                .kv("rain_raw", raw)
                .mic("PARITY");
        }
        _ => return false,
    }
    true
}
