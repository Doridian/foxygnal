//! Itron ERT utility meter messages (910–920 MHz, mostly ~912.6 MHz):
//! OOK Manchester at 32768 chips/s (30.5 µs per half-bit), sliced with the
//! leading-zero Manchester slicer. Wants 1 MS/s or more.
//!
//! - **SCM** (Standard Consumption Message): 96 bits — 21-bit preamble
//!   (including the slicer's leading 0), 2 ID high bits, reserved bit,
//!   physical tamper (2), ERT type (4), encoder tamper (2), 24-bit
//!   consumption, 24 ID low bits, and a 16-bit BCH code (generator 0x6F63)
//!   over bytes 2–11, which leaves a zero remainder.
//! - **SCM+**: frame sync `16 A3 1E` (the last byte is the protocol ID),
//!   endpoint type, 32-bit endpoint ID, 32-bit consumption, 16-bit tamper
//!   flags, and a CRC-16 (CCITT poly 0x1021, init 0x0971) over bytes 2–13.
//! - **IDM** (Interval Data Message): frame sync `16 A3 1C`, 92 bytes: packet
//!   type, length, Hamming code, application version, ERT type, 32-bit
//!   serial, interval count, programming state, tamper counters, async
//!   counters, power outage flags, 32-bit last consumption, 47 9-bit
//!   interval differences, transmit time offset, a serial-number CRC and a
//!   CRC-16 (0x1021, init 0xD895) over bytes 2–87.

use super::super::bits::{BitBuffer, crc16};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

/// Utility of a meter from the low nibble of its ERT / endpoint type.
fn meter_type(t: u8) -> &'static str {
    match t & 0x0F {
        4 | 5 | 7 | 8 => "Electric",
        0 | 1 | 2 | 9 | 12 => "Gas",
        3 | 11 | 13 => "Water",
        _ => "unknown",
    }
}

pub const SCM: Device = Device {
    key: "scm",
    label: "ERT Standard Consumption Message (SCM)",
    modulation: Modulation::OokManchester,
    timing: timing(30.0, 0.0, 0.0, 0.0, 90.0, 0.0),
    decode: decode_scm,
};

fn decode_scm(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        if bb.bits(r) != 96 {
            continue;
        }
        let b = bb.row(r);
        if b[..4].iter().all(|&x| x == 0) || crc16(&b[2..12], 0x6F63, 0) != 0 {
            continue;
        }
        let id = u32::from(b[2] & 0x06) << 23
            | u32::from(b[7]) << 16
            | u32::from(b[8]) << 8
            | u32::from(b[9]);
        let consumption = u32::from(b[4]) << 16 | u32::from(b[5]) << 8 | u32::from(b[6]);
        let ert_type = (b[3] >> 2) & 0x0F;
        out.begin("ERT-SCM")
            .kv("id", id)
            .kv("physical_tamper", b[3] >> 6)
            .kv("ert_type", ert_type)
            .kv("encoder_tamper", b[3] & 3)
            .kv("consumption_data", consumption)
            .kv("MeterType", meter_type(ert_type))
            .mic("CRC");
        return;
    }
}

pub const SCMPLUS: Device = Device {
    key: "scmplus",
    label: "ERT SCM+",
    modulation: Modulation::OokManchester,
    timing: timing(30.0, 0.0, 0.0, 0.0, 90.0, 0.0),
    decode: decode_scmplus,
};

fn decode_scmplus(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    const SYNC: [u8; 3] = [0x16, 0xA3, 0x1E];
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        if len < 128 {
            continue;
        }
        let pos = bb.search(r, 0, &SYNC, 24);
        if pos + 128 > len {
            continue;
        }
        let mut b = [0u8; 16];
        bb.extract(r, pos, &mut b, 128);
        let crc = u16::from_be_bytes([b[14], b[15]]);
        if crc16(&b[2..14], 0x1021, 0x0971) != crc {
            continue;
        }
        let id = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        out.begin("SCMplus")
            .kv("id", id)
            .kv("ProtocolID", format_args!("0x{:02X}", b[2]))
            .kv("EndpointType", format_args!("0x{:02X}", b[3]))
            .kv("EndpointID", id)
            .kv(
                "Consumption",
                u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            )
            .kv("Tamper", format_args!("0x{:02X}{:02X}", b[12], b[13]))
            .kv("PacketCRC", format_args!("0x{crc:04X}"))
            .kv("MeterType", meter_type(b[3]))
            .mic("CRC");
        return;
    }
}

pub const IDM: Device = Device {
    key: "idm",
    label: "ERT Interval Data Message (IDM)",
    modulation: Modulation::OokManchester,
    timing: timing(30.0, 0.0, 0.0, 0.0, 90.0, 0.0),
    decode: decode_idm,
};

const IDM_BYTES: usize = 92;

fn decode_idm(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    const SYNC: [u8; 3] = [0x16, 0xA3, 0x1C];
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        if len < IDM_BYTES * 8 {
            continue;
        }
        let pos = bb.search(r, 0, &SYNC, 24);
        if pos + IDM_BYTES * 8 > len {
            continue;
        }
        let mut b = [0u8; IDM_BYTES];
        bb.extract(r, pos, &mut b, IDM_BYTES * 8);
        if crc16(&b[2..88], 0x1021, 0xD895) != u16::from_be_bytes([b[88], b[89]]) {
            continue;
        }
        let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        out.begin("IDM")
            .kv("id", be32(7))
            .kv("PacketTypeID", format_args!("0x{:02X}", b[2]))
            .kv("PacketLength", b[3])
            .kv("ApplicationVersion", b[5])
            .kv("ERTType", format_args!("0x{:02X}", b[6]))
            .kv("ConsumptionIntervalCount", b[11])
            .kv("ModuleProgrammingState", format_args!("0x{:02X}", b[12]))
            .kv("AsynchronousCounters", u16::from_be_bytes([b[19], b[20]]))
            .kv("LastConsumptionCount", be32(27))
            .kv("TransmitTimeOffset", u16::from_be_bytes([b[84], b[85]]))
            .kv("MeterIdCRC", u16::from_be_bytes([b[86], b[87]]))
            .kv("MeterType", meter_type(b[6]))
            .mic("CRC");
        return;
    }
}
