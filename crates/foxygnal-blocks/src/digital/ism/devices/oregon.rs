//! Oregon Scientific weather sensors, protocols v2.1 and v3.
//!
//! Both are OOK Manchester at ~1024 symbols/s (half-bit ~488 µs), sliced with
//! the leading-zero Manchester slicer. Messages are nibbles sent least
//! significant bit first: a 16-bit sensor type, the channel, an 8-bit rolling
//! ID, flags (bit 2: battery low), then readings in BCD, then a checksum: the
//! sum of the preceding nibbles, its low nibble sent first.
//!
//! - **v2.1** sends every data bit twice, inverted first, so the sliced bits
//!   are pairs to Manchester-decode once more. Preamble: 16 ones (sliced
//!   `55 55 55 55`), then the sync nibble 0xA (`99`). Sensors here:
//!   THGR122N / THGR228N / THGR968 (temperature, humidity), THN132N /
//!   THR228N (temperature), BHTR968 (plus pressure), WGR968 (wind).
//! - **v3** sends each bit once. Preamble: 24 zeros (as sliced), sync 0xA
//!   (`0101`). Sensors here: THGR810 (temperature, humidity), THN802
//!   (temperature), WGR800 (wind), PCR800 (rain), UV800 (UV index).
//!
//! Temperature nibbles 8–11: tenths, units, tens, sign (0x8); humidity
//! nibbles 12–13: units, tens.

use super::super::bits::BitBuffer;
use super::super::out::Out;
use super::super::slicer::Modulation;
use super::{Device, timing};

pub const OREGON: Device = Device {
    key: "oregon",
    label: "Oregon Scientific v2.1 / v3 (THGR122N, THN132N, THGR810, WGR800, PCR800, …)",
    modulation: Modulation::OokManchester,
    timing: timing(488.0, 0.0, 0.0, 0.0, 2400.0, 0.0),
    decode: decode_oregon,
};

/// Most message nibbles kept.
const MAX_NIBBLES: usize = 48;

/// A message as nibbles, in the order they were sent.
struct Msg {
    n: [u8; MAX_NIBBLES],
    len: usize,
    /// Message bits (v2.1: the number of decoded bits).
    bits: usize,
}

impl Msg {
    /// Read nibbles from `bits` bits of `src` starting at bit 0, each sent
    /// least significant bit first.
    fn from_bits(src: &[u8], start: usize, bits: usize) -> Self {
        let mut m = Msg {
            n: [0; MAX_NIBBLES],
            len: (bits / 4).min(MAX_NIBBLES),
            bits,
        };
        for i in 0..m.len {
            let mut v = 0;
            for k in 0..4 {
                if super::super::bits::bit(src, start + i * 4 + k) {
                    v |= 1 << k;
                }
            }
            m.n[i] = v;
        }
        m
    }

    fn id(&self) -> u16 {
        self.n[..4].iter().fold(0, |a, &x| a << 4 | u16::from(x))
    }

    /// Checksum in nibbles `at` (low) and `at + 1` (high) equals the sum of
    /// the nibbles before it.
    fn checksum_ok(&self, at: usize) -> bool {
        if at + 2 > self.len {
            return false;
        }
        let sum: u32 = self.n[..at].iter().map(|&x| u32::from(x)).sum();
        (sum & 0xFF) as u8 == self.n[at] | self.n[at + 1] << 4
    }

    fn bcd_ok(&self, range: std::ops::Range<usize>) -> bool {
        self.n[range].iter().all(|&d| d <= 9)
    }

    fn channel(&self) -> u8 {
        self.n[4]
    }

    fn rolling_id(&self) -> u8 {
        self.n[5] | self.n[6] << 4
    }

    fn battery_ok(&self) -> u8 {
        u8::from(self.n[7] & 4 == 0)
    }

    fn temperature(&self) -> f64 {
        let n = &self.n;
        let t = f64::from(n[10]) * 10.0
            + f64::from(n[9])
            + f64::from(n[8]) * 0.1
            + f64::from(n[11] & 7) * 100.0;
        if n[11] & 8 != 0 { -t } else { t }
    }

    fn humidity(&self) -> u8 {
        self.n[13] * 10 + self.n[12]
    }

    /// The common head of a line.
    fn begin<'a>(&self, out: &'a mut Out, model: &str) -> super::super::out::Line<'a> {
        out.begin(model)
            .kv("id", self.rolling_id())
            .kv("channel", self.channel())
            .kv("battery_ok", self.battery_ok())
    }
}

const V21_SYNC: [u8; 3] = [0x55, 0x55, 0x99];
const V3_SYNC: [u8; 2] = [0x00, 0x05];
const V3_SYNC_ONES: [u8; 2] = [0xFF, 0xF5];

fn decode_oregon(bb: &BitBuffer, scratch: &mut BitBuffer, out: &mut Out) {
    for r in 0..bb.num_rows() {
        let len = bb.bits(r);
        // v2.1: the preamble's last 16 sliced bits and the sync nibble.
        let p = bb.search(r, 0, &V21_SYNC, 24);
        if p < len && p <= 64 {
            bb.manchester_decode(r, p + 24, scratch, 173);
            let bits = scratch.bits(0);
            let m = Msg::from_bits(scratch.row(0), 0, bits);
            if v21(&m, out) {
                continue;
            }
        }
        // v3: the end of the preamble and the sync nibble. The preamble may
        // slice as ones or zeros (it is a run of equal half-bits), and the
        // slicer may break the row between the preamble and the sync, so a
        // row may also start right at the sync.
        let starts = [
            bb.search(r, 0, &V3_SYNC, 16).saturating_add(16),
            bb.search(r, 0, &V3_SYNC_ONES, 16).saturating_add(16),
            if bb.row(r).first().is_some_and(|b| b >> 4 == 0x5) {
                4
            } else {
                len
            },
        ];
        for s in starts {
            if s <= 80 && s + 7 * 8 <= len && v3(&Msg::from_bits(bb.row(r), s, len - s), out) {
                break;
            }
        }
    }
}

/// A v2.1 message; true if it was one.
fn v21(m: &Msg, out: &mut Out) -> bool {
    if m.len < 8 {
        return false;
    }
    match m.id() {
        0x1D20 | 0x1D30 => {
            if !m.checksum_ok(15) {
                return false;
            }
            let model = match (m.id(), m.bits) {
                (0x1D30, _) => "Oregon-THGR968",
                (_, 76..) => "Oregon-THGR122N",
                _ => "Oregon-THGR228N",
            };
            m.begin(out, model)
                .f("temperature_C", m.temperature(), 1)
                .kv("humidity", m.humidity())
                .mic("CHECKSUM");
        }
        0xEC40 if m.bits >= 64 => {
            if !m.checksum_ok(12) || !m.bcd_ok(8..11) {
                return false;
            }
            let t = m.temperature();
            if !(-50.0..=70.0).contains(&t) {
                return false;
            }
            let model = if m.bits < 76 {
                "Oregon-THN132N"
            } else {
                "Oregon-THR228N"
            };
            m.begin(out, model).f("temperature_C", t, 1).mic("CHECKSUM");
        }
        0x5D60 => {
            if !m.checksum_ok(19) {
                return false;
            }
            let pressure = u32::from(m.n[15] | m.n[16] << 4) + 856;
            m.begin(out, "Oregon-BHTR968")
                .f("temperature_C", m.temperature(), 1)
                .kv("humidity", m.humidity())
                .kv("pressure_hPa", pressure)
                .mic("CHECKSUM");
        }
        0x3D00 => {
            if !m.checksum_ok(17) {
                return false;
            }
            let n = |i: usize| f64::from(m.n[i]);
            let dir = n(9) * 10.0 + n(8) + n(10) * 100.0;
            let gust = n(11) * 0.1 + n(12) + n(13) * 0.1;
            let avg = n(14) * 0.1 + n(15) + n(16) * 0.1;
            m.begin(out, "Oregon-WGR968")
                .f("wind_max_m_s", gust, 1)
                .f("wind_avg_m_s", avg, 1)
                .f("wind_dir_deg", dir, 1)
                .mic("CHECKSUM");
        }
        _ => return false,
    }
    true
}

/// A v3 message; true if it was one.
fn v3(m: &Msg, out: &mut Out) -> bool {
    let id = m.id();
    let n = |i: usize| f64::from(m.n[i]);
    if id & 0xF0FF == 0xF024 || id == 0xF8B4 {
        if !m.checksum_ok(15) || !m.bcd_ok(8..11) || !m.bcd_ok(12..14) {
            return false;
        }
        let t = m.temperature();
        if !(-50.0..=70.0).contains(&t) {
            return false;
        }
        let line = m
            .begin(out, "Oregon-THGR810")
            .f("temperature_C", t, 1)
            .kv("humidity", m.humidity());
        if m.n[1] & 1 != 0 {
            drop(line.kv("button", 1).mic("CHECKSUM"));
        } else {
            drop(line.mic("CHECKSUM"));
        }
    } else if id == 0xC844 {
        if !m.checksum_ok(12) {
            return false;
        }
        m.begin(out, "Oregon-THN802")
            .f("temperature_C", m.temperature(), 1)
            .mic("CHECKSUM");
    } else if id == 0xD874 {
        if !m.checksum_ok(13) {
            return false;
        }
        m.begin(out, "Oregon-UV800")
            .kv("uvi", m.n[9] * 10 + m.n[8])
            .mic("CHECKSUM");
    } else if id == 0x2914 || id == 0x2D14 {
        if !m.checksum_ok(18) || (id == 0x2914 && !m.bcd_ok(8..18)) {
            return false;
        }
        let rate = (n(11) * 1000.0 + n(10) * 100.0 + n(9) * 10.0 + n(8)) / 100.0;
        let total =
            n(17) * 100.0 + n(16) * 10.0 + n(15) + n(14) / 10.0 + n(13) / 100.0 + n(12) / 1000.0;
        let model = if id == 0x2914 {
            "Oregon-PCR800"
        } else {
            "Oregon-PCR800a"
        };
        m.begin(out, model)
            .f("rain_rate_in_h", rate, 2)
            .f("rain_in", total, 3)
            .mic("CHECKSUM");
    } else if id == 0x1984 || id == 0x1994 {
        if !m.checksum_ok(17) || !m.bcd_ok(11..17) {
            return false;
        }
        let gust = n(11) / 10.0 + n(12) + n(13) * 10.0;
        let avg = n(14) / 10.0 + n(15) + n(16) * 10.0;
        if gust > 56.0 || avg > 56.0 {
            return false;
        }
        m.begin(out, "Oregon-WGR800")
            .f("wind_max_m_s", gust, 1)
            .f("wind_avg_m_s", avg, 1)
            .f("wind_dir_deg", n(8) * 22.5, 1)
            .mic("CHECKSUM");
    } else {
        return false;
    }
    true
}
