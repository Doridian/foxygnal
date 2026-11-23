//! Tire pressure sensors (TPMS), 315 and 433.92 MHz.
//!
//! - **Schrader** (FCC MRXGG4, many US cars): OOK Manchester, 120 µs per
//!   half-bit, 68 bits: a sync nibble (`0111`, its leading 0 being the
//!   Manchester slicer's hard-coded first bit), then 8 bytes `PF FI II II II
//!   PP TT CC`: preamble nibble, 8 flag bits, 28-bit ID, pressure in 25 mbar
//!   steps, temperature + 50 °C, and a CRC-8 (poly 0x07, init 0xF0) over
//!   the first 7 bytes.
//! - **Schrader EG53MA4** (GM, Opel, Saab): the same line code (123 µs), 120
//!   bits: 40 sync bits, then `FF FF FF FF II II II PP TT CC`: flags, 24-bit
//!   ID, pressure in 25 mbar steps, temperature in °F, and the byte sum.
//! - **Toyota** (Pacific PMV-C210): FSK NRZ at 52 µs, preamble
//!   `01010101001111`, then 72 bits of differential Manchester (a level
//!   change at every bit boundary; no change mid-bit = 1): 32-bit ID, a
//!   status bit, 8-bit pressure (¼ PSI, offset −7 PSI), 8-bit temperature
//!   (+40 °C), 7 status bits, the pressure inverted, CRC-8 (0x07, init 0x80).
//! - **Ford** (VDO/Continental): FSK NRZ at 52 µs, preamble `55 55 55 56`,
//!   then 64 bits Manchester (`10` = 1): 32-bit ID, pressure (¼ PSI, its
//!   9th bit in the flags), temperature (+56 °C, invalid when bit 7 is set),
//!   flags (moving 0x44 / at rest 0x04 / learn 0x08), and the byte sum.
//! - **Citroen / Peugeot** (VDO): the same preamble and line code, 80 bits:
//!   state, 32-bit ID, flags / repeat nibbles, pressure (1.364 kPa steps),
//!   temperature (+50 °C), battery, and an XOR of bytes 1–9 that is zero.

use super::super::bits::{BitBuffer, add_bytes, bit, crc8};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

/// Decode Manchester pairs of `row` from bit `start` into `out` (packed,
/// most significant first), at most `max` bits. With `first`, a pair's
/// first half is the bit (`10` = 1), else its second (`01` = 1, IEEE
/// 802.3). Stops at an invalid pair; returns the bits decoded.
pub(super) fn mc_decode(
    row: &[u8],
    len: usize,
    start: usize,
    out: &mut [u8],
    max: usize,
    first: bool,
) -> usize {
    let max = max.min(out.len() * 8);
    out.fill(0);
    let mut n = 0;
    let mut i = start;
    while n < max && i + 1 < len {
        let (a, b) = (bit(row, i), bit(row, i + 1));
        if a == b {
            break;
        }
        if if first { a } else { b } {
            out[n / 8] |= 0x80 >> (n % 8);
        }
        n += 1;
        i += 2;
    }
    n
}

pub const SCHRADER: Device = Device {
    key: "schrader",
    label: "Schrader TPMS (and EG53MA4)",
    modulation: Modulation::OokManchester,
    timing: timing(120.0, 0.0, 0.0, 0.0, 480.0, 0.0),
    decode: decode_schrader,
};

fn decode_schrader(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        if len == 120 && schrader_eg53(bb, r, out) {
            return;
        }
        // The packet is the row's last 67 bits: the sync nibble's three 1s
        // after its leading 0, the preamble nibble 1111, then 64 bits. Noise
        // before the packet can leave bits ahead of it.
        if len < 67 {
            continue;
        }
        let mut pre = [0u8; 1];
        bb.extract(r, len - 67, &mut pre, 7);
        if pre[0] != 0xFE {
            continue;
        }
        let mut b = [0u8; 8];
        bb.extract(r, len - 64, &mut b, 64);
        if crc8(&b[..7], 0x07, 0xF0) != b[7] {
            continue;
        }
        let id = u32::from(b[1] & 0x0F) << 24
            | u32::from(b[2]) << 16
            | u32::from(b[3]) << 8
            | u32::from(b[4]);
        if id == 0 {
            continue;
        }
        let flags = (b[0] & 0x0F) << 4 | b[1] >> 4;
        out.begin("Schrader")
            .kv("type", "TPMS")
            .kv("flags", format_args!("{flags:02x}"))
            .hex("id", id.into(), 7)
            .f("pressure_kPa", f64::from(b[5]) * 2.5, 1)
            .kv("temperature_C", i32::from(b[6]) - 50)
            .mic("CRC");
        return;
    }
}

fn schrader_eg53(bb: &BitBuffer, r: usize, out: &mut Out) -> bool {
    let mut b = [0u8; 10];
    bb.extract(r, 40, &mut b, 80);
    if b[1] == 0 && b[2] == 0 && b[4] == 0 && b[5] == 0 && b[7] == 0 && b[8] == 0 {
        return false;
    }
    if add_bytes(&b[..9]) & 0xFF != u32::from(b[9]) {
        return false;
    }
    let id = u32::from(b[4]) << 16 | u32::from(b[5]) << 8 | u32::from(b[6]);
    let flags = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    out.begin("Schrader-EG53MA4")
        .kv("type", "TPMS")
        .kv("flags", format_args!("{flags:08x}"))
        .hex("id", id.into(), 6)
        .f("pressure_kPa", f64::from(b[7]) * 2.5, 1)
        .f("temperature_F", f64::from(b[8]), 1)
        .mic("CHECKSUM");
    true
}

pub const TOYOTA: Device = Device {
    key: "toyota",
    label: "Toyota TPMS (PMV-C210)",
    modulation: Modulation::FskPcm,
    timing: timing(52.0, 52.0, 0.0, 0.0, 150.0, 0.0),
    decode: decode_toyota,
};

fn decode_toyota(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    // The preamble's last 11 bits and the first half-bit of the data.
    const PATTERN: [u8; 2] = [0xA9, 0xE0];
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        let row = bb.row(r);
        let mut pos = 0;
        loop {
            pos = bb.search(r, pos, &PATTERN, 12);
            if pos + 11 + 144 > len + MISSING_ZEROS {
                break;
            }
            if toyota_bits(row, pos + 11).is_some_and(|b| toyota_emit(&b, out)) {
                return;
            }
            pos += 1;
        }
    }
}

/// 72 differentially Manchester-coded bits from `start`; the half-bit
/// before `start` is a 1 (the preamble's end).
fn toyota_bits(row: &[u8], start: usize) -> Option<[u8; 9]> {
    let mut b = [0u8; 9];
    let mut prev = true;
    for n in 0..72 {
        let h1 = bit(row, start + 2 * n);
        let h2 = bit(row, start + 2 * n + 1);
        if h1 == prev {
            // No transition at the bit boundary.
            return None;
        }
        if h1 == h2 {
            b[n / 8] |= 0x80 >> (n % 8);
        }
        prev = h2;
    }
    Some(b)
}

fn toyota_emit(b: &[u8; 9], out: &mut Out) -> bool {
    if crc8(&b[..8], 0x07, 0x80) != b[8] {
        return false;
    }
    let pressure = (b[4] & 0x7F) << 1 | b[5] >> 7;
    let temp = (b[5] & 0x7F) << 1 | b[6] >> 7;
    if pressure != b[7] ^ 0xFF {
        return false;
    }
    let id = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    out.begin("Toyota")
        .kv("type", "TPMS")
        .kv("id", format_args!("{id:08x}"))
        .kv("status", (b[4] & 0x80) | (b[6] & 0x7F))
        .f("pressure_PSI", f64::from(pressure) * 0.25 - 7.0, 2)
        .kv("temperature_C", i32::from(temp) - 40)
        .mic("CRC");
    true
}

/// The NRZ slicer ends a row at a gap longer than the reset limit without
/// adding that gap's zeros, so a packet's last few bits may be missing from
/// the row when they are 0: up to this many are read as zeros. (The
/// Manchester decoders read one missing half-bit the same way.)
const MISSING_ZEROS: usize = 3;

/// `55 55 55 56`: the end of the Ford / Citroen preamble.
const VDO_PREAMBLE: [u8; 2] = [0x55, 0x56];

pub const FORD: Device = Device {
    key: "ford",
    label: "Ford TPMS",
    modulation: Modulation::FskPcm,
    timing: timing(52.0, 52.0, 0.0, 0.0, 150.0, 0.0),
    decode: decode_ford,
};

fn decode_ford(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        let mut pos = 0;
        loop {
            pos = bb.search(r, pos, &VDO_PREAMBLE, 16);
            if pos + 16 + 128 > len + 1 {
                break;
            }
            let mut b = [0u8; 8];
            if mc_decode(bb.row(r), len + 1, pos + 16, &mut b, 64, true) == 64 && ford_emit(&b, out)
            {
                return;
            }
            pos += 1;
        }
    }
}

fn ford_emit(b: &[u8; 8], out: &mut Out) -> bool {
    if add_bytes(&b[..7]) & 0xFF != u32::from(b[7]) {
        return false;
    }
    // Moving, at rest and learn are the only known flag patterns.
    let (moving, learn) = match b[6] & 0x4C {
        0x08 => (0, 1),
        0x04 => (0, 0),
        0x44 => (1, 0),
        _ => return false,
    };
    if b[6] & 0x90 != 0 {
        return false;
    }
    let id = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let psi = f64::from(u16::from(b[6] & 0x20) << 3 | u16::from(b[4])) * 0.25;
    let code = u32::from(b[4]) << 16 | u32::from(b[5]) << 8 | u32::from(b[6]);
    let line = out
        .begin("Ford")
        .kv("type", "TPMS")
        .kv("id", format_args!("{id:08x}"))
        .f("pressure_PSI", psi, 2);
    let line = if b[5] & 0x80 == 0 {
        line.f("temperature_C", f64::from(b[5] & 0x7F) - 56.0, 1)
    } else {
        line
    };
    line.kv("moving", moving)
        .kv("learn", learn)
        .kv("code", format_args!("{code:06x}"))
        .mic("CHECKSUM");
    true
}

pub const CITROEN: Device = Device {
    key: "citroen",
    label: "Citroen / Peugeot TPMS (VDO)",
    modulation: Modulation::FskPcm,
    timing: timing(52.0, 52.0, 0.0, 0.0, 150.0, 0.0),
    decode: decode_citroen,
};

fn decode_citroen(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        let mut pos = 0;
        loop {
            pos = bb.search(r, pos, &VDO_PREAMBLE, 16);
            if pos + 16 + 160 > len + 1 {
                break;
            }
            let mut b = [0u8; 10];
            if mc_decode(bb.row(r), len + 1, pos + 16, &mut b, 80, true) == 80
                && citroen_emit(&b, out)
            {
                return;
            }
            pos += 1;
        }
    }
}

fn citroen_emit(b: &[u8; 10], out: &mut Out) -> bool {
    if b[6] == 0 || b[7] == 0 || b[1..].iter().fold(0, |a, &x| a ^ x) != 0 {
        return false;
    }
    let id = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
    out.begin("Citroen")
        .kv("type", "TPMS")
        .kv("id", format_args!("{id:08x}"))
        .kv("state", format_args!("{:02x}", b[0]))
        .kv("flags", b[5] >> 4)
        .kv("repeat", b[5] & 0x0F)
        .f("pressure_kPa", f64::from(b[6]) * 1.364, 0)
        .kv("temperature_C", i32::from(b[7]) - 50)
        .kv("maybe_battery", b[8])
        .mic("CHECKSUM");
    true
}
