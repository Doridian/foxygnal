//! Alarm-system door / window / motion sensors.
//!
//! - **Honeywell 5800 series / 2Gig DW10, DW11 / RE208** (345 MHz): OOK,
//!   sliced as NRZ at 136 µs then Manchester-decoded here (`01` = 1). After
//!   the preamble `FF FE` come 48 bits: 4-bit channel, 20-bit device ID,
//!   8 event bits (contact, tamper, reed, alarm, battery low, heartbeat),
//!   and a CRC-16 over the first 4 bytes (poly 0x8050 for the 2Gig
//!   channels 2, 4, 9, A and C, else 0x8005; init 0).
//! - **Interlogix / GE / UTC** (319.5 MHz): OOK PPM, 122 / 244 µs gaps,
//!   57–64 bits: front porch, sync zeros and a start 1 (found as `0000
//!   0001`), then 46 bits sent least significant first: 20-bit ID, 4-bit
//!   device type, 3-bit trigger count (key-fob buttons), battery, latch and
//!   level pairs for five inputs, and two parity bits making the even- and
//!   odd-positioned bits each odd.

use super::super::bits::{BitBuffer, crc16, reverse8};
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::tpms::mc_decode;
use super::{Device, timing};

pub const HONEYWELL: Device = Device {
    key: "honeywell",
    label: "Honeywell 5800 / 2Gig DW10/DW11 door/window sensors",
    modulation: Modulation::OokPcm,
    timing: timing(136.0, 136.0, 0.0, 0.0, 408.0, 0.0),
    decode: decode_honeywell,
};

fn decode_honeywell(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    // The preamble FF FE's last 12 bits, Manchester coded.
    const PREAMBLE: [u8; 3] = [0x55, 0x55, 0x56];
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        if len < 120 {
            continue;
        }
        let mut pos = 0;
        loop {
            pos = bb.search(r, pos, &PREAMBLE, 24);
            if pos + 24 >= len {
                break;
            }
            let mut b = [0u8; 6];
            let n = mc_decode(bb.row(r), len, pos + 24, &mut b, 48, false);
            // A final 0 (`10`) loses its trailing low half when the
            // transmission ends: it reads as zero anyway.
            if n >= 47 && honeywell_emit(&b, out) {
                return;
            }
            pos += 1;
        }
    }
}

fn honeywell_emit(b: &[u8; 6], out: &mut Out) -> bool {
    let channel = b[0] >> 4;
    let id = u32::from(b[0] & 0x0F) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
    let crc = u16::from_be_bytes([b[4], b[5]]);
    if id == 0 && crc == 0 {
        return false;
    }
    let poly = if matches!(channel, 0x2 | 0x4 | 0x9 | 0xA | 0xC) {
        0x8050
    } else {
        0x8005
    };
    if crc16(&b[..4], poly, 0) != crc {
        return false;
    }
    let event = b[3];
    let flag = |m: u8| u8::from(event & m != 0);
    out.begin("Honeywell-Security")
        .kv("id", id)
        .kv("channel", channel)
        .kv("event", format_args!("{event:02x}"))
        .kv("state", if event & 0x80 != 0 { "open" } else { "closed" })
        .kv("contact_open", flag(0x80))
        .kv("reed_open", flag(0x20))
        .kv("alarm", flag(0x10))
        .kv("tamper", flag(0x40))
        .kv("battery_ok", 1 - flag(0x08))
        .kv("heartbeat", flag(0x04))
        .mic("CRC");
    true
}

pub const INTERLOGIX: Device = Device {
    key: "interlogix",
    label: "Interlogix / GE / UTC security devices",
    modulation: Modulation::OokPpm,
    // Gaps more than 60 µs off either width end the row: noise ahead of a
    // packet (or a burst of interference) can't run into one.
    timing: timing(122.0, 244.0, 0.0, 0.0, 500.0, 60.0),
    decode: decode_interlogix,
};

fn decode_interlogix(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        if !(57..=64).contains(&len) {
            continue;
        }
        // The message is the row's last 46 bits, after a run of 0s and a 1
        // (rtl_433 looks for the last 8 of them). Every capture ends right
        // after the message; anchoring it there keeps rows that noise or a
        // Manchester signal (whose runs of equal bits slice to PPM 0s) made
        // from matching somewhere inside.
        let mut pre = [0u8; 1];
        bb.extract(r, len - 54, &mut pre, 8);
        if pre[0] != 0x01 {
            continue;
        }
        let mut m = [0u8; 6];
        bb.extract(r, len - 46, &mut m, 46);
        if interlogix_emit(&m, out) {
            return;
        }
    }
}

fn interlogix_emit(m: &[u8; 6], out: &mut Out) -> bool {
    let all = |a: &[u8], v: u8| a.iter().all(|&x| x == v);
    if all(&m[..3], 0) || all(&m[..3], 0xFF) || all(&m[3..], 0) || all(&m[3..], 0xFF) {
        return false;
    }
    // Even-positioned bits (0, 2, … 40) and odd ones (1, … 41) must each
    // have odd parity: positions 40 and 41 are the top bits of byte 5.
    let mut even = 0u32;
    let mut odd = 0u32;
    for i in 0..42 {
        let v = u32::from(m[i / 8] >> (7 - i % 8) & 1);
        if i % 2 == 0 {
            even ^= v;
        } else {
            odd ^= v;
        }
    }
    if even != 1 || odd != 1 {
        return false;
    }
    let kind = reverse8(m[2]) >> 4;
    let subtype = match kind {
        0xA => "contact",
        0xF => "keyfob",
        0x4 => "motion",
        0x6 => "heat",
        0x9 | 0xD => "glass",
        0xE => "freeze",
        0x2 => "smoke",
        0x3 => "panic",
        _ => return false,
    };
    let state = |open: bool| if open { "OPEN" } else { "CLOSED" };
    let (battery_low, sw) = if kind == 0xF {
        // Key fobs send their button in the trigger count bits.
        let b = m[3] & 0x0E;
        (false, [b != 0x4, b != 0x8, b != 0xC, b != 0x2, b != 0xA])
    } else {
        (
            m[3] & 0x10 != 0,
            [
                m[3] & 0x04 != 0,
                m[3] & 0x01 != 0,
                m[4] & 0x40 != 0,
                m[4] & 0x10 != 0,
                m[4] & 0x04 != 0,
            ],
        )
    };
    out.begin("Interlogix-Security")
        .kv("subtype", subtype)
        .kv(
            "id",
            format_args!(
                "{:02x}{:02x}{:02x}",
                reverse8(m[2]),
                reverse8(m[1]),
                reverse8(m[0])
            ),
        )
        .kv("battery_ok", u8::from(!battery_low))
        .kv("switch1", state(sw[0]))
        .kv("switch2", state(sw[1]))
        .kv("switch3", state(sw[2]))
        .kv("switch4", state(sw[3]))
        .kv("switch5", state(sw[4]))
        .kv(
            "raw_message",
            format_args!("{:02x}{:02x}{:02x}", m[3], m[4], m[5]),
        )
        .mic("PARITY");
    true
}
