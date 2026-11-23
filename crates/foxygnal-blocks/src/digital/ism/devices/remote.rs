//! Generic remotes, doorbells and cheap sensors (EV1527, PT2262 and their
//! clones), 315 / 433.92 MHz.
//!
//! Both send 24 bits as OOK PWM with a base period α (about 250–500 µs, set
//! by a resistor): a 0 is α on, 3α off; a 1 is 3α on, α off. Each word ends
//! with a sync: α on, 31α off. Sliced with PWM (short = 1) a word is a row
//! of 25 bits: the 24 bits inverted, then the sync pulse as a 1.
//!
//! - **EV1527** (learning code): 20-bit ID and 4 data bits (the button).
//! - **PT2262 / SC2262** (fixed code): 12 tri-state digits, two bits each:
//!   `00` = 0, `11` = 1, `01` = F (floating); `10` is not a PT2262 digit.
//!
//! These codes have no checksum and match noise and other devices easily,
//! so both decoders are strict: widths within the PWM tolerance, rows of
//! exactly 25 bits ending in the sync, at least two identical words in a
//! packet, and no all-0 / all-1 code. A code that is valid tri-state is
//! reported as PT2262 only (an EV1527 code is valid tri-state about 3 % of
//! the time; its `code=` field reads the same either way).

use super::super::bits::BitBuffer;
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

/// Short pulses 20–740 µs, long 780–1500 µs: α from about 290 to 450 µs with
/// ±10 % jitter. (A longer α also makes the sync gap longer than the gap
/// that ends a packet, so the repeats would not be seen together.)
const TIMING: super::super::slicer::Timing = timing(380.0, 1140.0, 0.0, 2000.0, 2000.0, 360.0);

pub const EV1527: Device = Device {
    key: "ev1527",
    label: "EV1527 remotes and doorbells (learning code)",
    modulation: Modulation::OokPwm,
    timing: TIMING,
    decode: decode_ev1527,
};

pub const PT2262: Device = Device {
    key: "pt2262",
    label: "PT2262 / SC2262 remotes and doorbells (tri-state)",
    modulation: Modulation::OokPwm,
    timing: TIMING,
    decode: decode_pt2262,
};

/// The 24-bit code repeated in at least two 25-bit rows, if any.
fn repeated_code(bb: &BitBuffer) -> Option<u32> {
    let n = bb.num_rows();
    for i in 0..n {
        let Some(code) = word(bb, i) else { continue };
        if (i + 1..n).any(|j| word(bb, j) == Some(code)) {
            return Some(code);
        }
    }
    None
}

fn word(bb: &BitBuffer, r: usize) -> Option<u32> {
    if bb.bits(r) != 25 {
        return None;
    }
    let b = bb.row(r);
    // The 25th bit: the sync pulse, short.
    if b[3] & 0x80 == 0 {
        return None;
    }
    // Short pulses slice as 1; on air a short pulse is a 0.
    let code = !(u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2])) & 0xFF_FFFF;
    (code != 0 && code != 0xFF_FFFF).then_some(code)
}

/// The code as tri-state digits, if every pair is one.
fn tristate(code: u32) -> Option<[u8; 12]> {
    let mut t = [0u8; 12];
    for (i, d) in t.iter_mut().enumerate() {
        *d = match code >> (22 - 2 * i) & 3 {
            0 => b'0',
            3 => b'1',
            1 => b'F',
            _ => return None,
        };
    }
    Some(t)
}

fn decode_ev1527(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    let Some(code) = repeated_code(bb) else {
        return;
    };
    if tristate(code).is_some() {
        return;
    }
    out.begin("EV1527")
        .kv("id", format_args!("{:05x}", code >> 4))
        .kv("button", code & 0xF)
        .kv("code", format_args!("{code:06x}"))
        .mic("REPEAT");
}

fn decode_pt2262(bb: &BitBuffer, _scratch: &mut BitBuffer, out: &mut Out) {
    let Some(code) = repeated_code(bb) else {
        return;
    };
    let Some(t) = tristate(code) else { return };
    fn s(b: &[u8]) -> &str {
        std::str::from_utf8(b).unwrap_or("")
    }
    out.begin("PT2262")
        .kv("id", s(&t[..8]))
        .kv("data", s(&t[8..]))
        .kv("tristate", s(&t))
        .kv("code", format_args!("{code:06x}"))
        .mic("REPEAT");
}
