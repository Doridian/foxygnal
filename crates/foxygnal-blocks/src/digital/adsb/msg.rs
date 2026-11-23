//! Mode S messages: the CRC, the fields of each downlink format, the
//! altitude and identity codes, and the extended squitter (DF17/18) types.
//!
//! Bit numbers in comments are 1-based over the whole message, as in ICAO
//! Annex 10 Vol. IV; ME (and MB) field bits are 1-based within the 56-bit
//! field, as in DO-260B and ICAO Doc 9871.

// ---- CRC ---------------------------------------------------------------------

const POLY: u32 = 0xFF_F409;

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 16;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80_0000 != 0 {
                (c << 1) ^ POLY
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c & 0xFF_FFFF;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

fn crc24(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &b| {
        ((crc << 8) ^ CRC_TABLE[usize::from((crc >> 16) as u8 ^ b)]) & 0xFF_FFFF
    })
}

/// The CRC of a message's data bits XOR its last 24 bits (the parity
/// field). 0 for an intact DF11/17/18 message; the aircraft address for the
/// formats whose parity is address/parity (AP); the address XOR the
/// interrogator code for DF11 replies to a numbered interrogator.
pub fn residual(bytes: &[u8]) -> u32 {
    let n = bytes.len();
    let parity =
        u32::from(bytes[n - 3]) << 16 | u32::from(bytes[n - 2]) << 8 | u32::from(bytes[n - 1]);
    crc24(&bytes[..n - 3]) ^ parity
}

/// [`residual`] of a 112-bit message.
pub fn crc_residual(bytes: &[u8; 14]) -> u32 {
    residual(bytes)
}

// ---- The message ---------------------------------------------------------------

/// A Mode S downlink message, 56 or 112 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeS {
    /// The message, left-aligned; a 56-bit message uses the first 7 bytes.
    pub bytes: [u8; 14],
    /// 7 or 14.
    pub len: usize,
}

/// Bytes in a message of downlink format `df`: formats 0–15 are 56 bits.
pub fn len_for_df(df: u8) -> usize {
    if df < 16 { 7 } else { 14 }
}

impl ModeS {
    pub fn new(bytes: &[u8]) -> Self {
        let mut b = [0u8; 14];
        b[..bytes.len()].copy_from_slice(bytes);
        Self {
            bytes: b,
            len: bytes.len(),
        }
    }

    /// From 14 or 28 hex digits.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 14 && s.len() != 28 {
            return None;
        }
        let mut bytes = [0u8; 14];
        for (i, b) in bytes.iter_mut().take(s.len() / 2).enumerate() {
            *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Self {
            bytes,
            len: s.len() / 2,
        })
    }

    pub fn data(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn residual(&self) -> u32 {
        residual(self.data())
    }

    /// Downlink format; 24 and up all start with `11` and are reported as 24.
    pub fn df(&self) -> u8 {
        let df = self.bytes[0] >> 3;
        if df >= 24 { 24 } else { df }
    }

    /// Bits `first..=last` of the message (1-based, as in Annex 10).
    pub fn field(&self, first: u32, last: u32) -> u32 {
        let v = self
            .bytes
            .iter()
            .fold(0u128, |v, &b| v << 8 | u128::from(b));
        ((v >> (112 - last)) & ((1u128 << (last - first + 1)) - 1)) as u32
    }

    /// The announced address (AA, bits 9–32) of DF11/17/18.
    pub fn icao(&self) -> u32 {
        u32::from(self.bytes[1]) << 16 | u32::from(self.bytes[2]) << 8 | u32::from(self.bytes[3])
    }

    /// The 56-bit ME / MB / MV field (bits 33–88) of a long message.
    pub fn me_field(&self) -> Mb {
        Mb(self.bytes[4..11]
            .iter()
            .fold(0u64, |v, &b| v << 8 | u64::from(b)))
    }

    /// `len` bits of the ME field starting at bit `start` (0-based).
    fn bits(&self, start: u32, len: u32) -> u32 {
        self.me_field().bits(start + 1, start + len)
    }

    /// Type code (DF17/18).
    pub fn tc(&self) -> u8 {
        self.bytes[4] >> 3
    }

    /// Capability (DF11/17) or control field (DF18), bits 6–8.
    pub fn ca(&self) -> u8 {
        self.bytes[0] & 7
    }

    /// The 13-bit altitude code (DF0/4/16/20) or identity code (DF5/21),
    /// bits 20–32.
    pub fn code13(&self) -> u32 {
        self.field(20, 32)
    }

    /// The call sign of an identification message (TC 1–4), spaces trimmed.
    pub fn callsign(&self) -> Option<CallSign> {
        if !(1..=4).contains(&self.tc()) {
            return None;
        }
        Some(self.me_field().chars())
    }

    /// Emitter category of an identification message, e.g. `A3`.
    pub fn category(&self) -> Option<(char, u8)> {
        let set = match self.tc() {
            4 => 'A',
            3 => 'B',
            2 => 'C',
            1 => 'D',
            _ => return None,
        };
        Some((set, self.bits(5, 3) as u8))
    }

    /// Barometric altitude of an airborne position (TC 9–18), in feet.
    /// `None` when unavailable.
    pub fn altitude_ft(&self) -> Option<i32> {
        if !(9..=18).contains(&self.tc()) {
            return None;
        }
        ac12_ft(self.bits(8, 12))
    }

    /// GNSS height of an airborne position with GNSS height (TC 20–22), in
    /// metres.
    pub fn gnss_height_m(&self) -> Option<i32> {
        if !(20..=22).contains(&self.tc()) {
            return None;
        }
        let v = self.bits(8, 12);
        (v != 0).then_some(v as i32)
    }

    /// The CPR position of an airborne (TC 9–18, 20–22) or surface (TC 5–8)
    /// position message.
    pub fn cpr(&self) -> Option<Cpr> {
        let tc = self.tc();
        let surface = (5..=8).contains(&tc);
        if !(surface || (9..=18).contains(&tc) || (20..=22).contains(&tc)) {
            return None;
        }
        Some(Cpr {
            odd: self.bits(21, 1) == 1,
            lat: self.bits(22, 17),
            lon: self.bits(39, 17),
            surface,
        })
    }

    /// Ground speed (kt) and track (degrees) of a surface position (TC 5–8).
    pub fn surface_movement(&self) -> (Option<f64>, Option<f64>) {
        if !(5..=8).contains(&self.tc()) {
            return (None, None);
        }
        let speed = movement_kt(self.bits(5, 7));
        let track = (self.bits(12, 1) == 1).then(|| f64::from(self.bits(13, 7)) * 360.0 / 128.0);
        (speed, track)
    }

    /// Velocity (TC 19, subtypes 1–4).
    pub fn velocity(&self) -> Option<Velocity> {
        if self.tc() != 19 {
            return None;
        }
        let st = self.bits(5, 3);
        let vr = self.bits(37, 9);
        let vrate_fpm = (vr != 0).then(|| {
            let v = (vr as i32 - 1) * 64;
            if self.bits(36, 1) == 1 { -v } else { v }
        });
        let scale = if st == 2 || st == 4 { 4.0 } else { 1.0 };
        match st {
            1 | 2 => {
                let (ew, ns) = (self.bits(14, 10), self.bits(25, 10));
                if ew == 0 || ns == 0 {
                    return None;
                }
                let sign = |neg: u32, v: u32| {
                    let v = f64::from(v - 1) * scale;
                    if neg == 1 { -v } else { v }
                };
                let vx = sign(self.bits(13, 1), ew);
                let vy = sign(self.bits(24, 1), ns);
                Some(Velocity::Ground {
                    speed_kt: vx.hypot(vy),
                    track_deg: vx.atan2(vy).to_degrees().rem_euclid(360.0),
                    vrate_fpm,
                })
            }
            3 | 4 => {
                let speed = self.bits(25, 10);
                if speed == 0 {
                    return None;
                }
                Some(Velocity::Air {
                    speed_kt: f64::from(speed - 1) * scale,
                    true_airspeed: self.bits(24, 1) == 1,
                    heading_deg: (self.bits(13, 1) == 1)
                        .then(|| f64::from(self.bits(14, 10)) * 360.0 / 1024.0),
                    vrate_fpm,
                })
            }
            _ => None,
        }
    }

    /// Velocity subtype (TC 19).
    pub fn velocity_subtype(&self) -> u32 {
        self.bits(5, 3)
    }

    /// Whether a velocity's vertical rate comes from the barometer (else
    /// GNSS).
    pub fn vrate_baro(&self) -> bool {
        self.bits(35, 1) == 1
    }

    /// GNSS minus barometric altitude of a velocity message, in feet.
    pub fn geo_minus_baro_ft(&self) -> Option<i32> {
        if self.tc() != 19 {
            return None;
        }
        let v = self.bits(49, 7);
        (v != 0).then(|| {
            let d = (v as i32 - 1) * 25;
            if self.bits(48, 1) == 1 { -d } else { d }
        })
    }

    /// Emergency/priority state and squawk of an aircraft status message
    /// (TC 28 subtype 1).
    pub fn emergency(&self) -> Option<(u32, u32)> {
        if self.tc() != 28 || self.bits(5, 3) != 1 {
            return None;
        }
        Some((self.bits(8, 3), id13_squawk(self.bits(11, 13))))
    }

    /// Target state and status (TC 29 subtype 1, ADS-B version 2).
    pub fn target_state(&self) -> Option<TargetState> {
        if self.tc() != 29 || self.bits(5, 2) != 1 {
            return None;
        }
        let alt = self.bits(9, 11);
        let baro = self.bits(20, 9);
        let modes = self.bits(46, 1) == 1;
        Some(TargetState {
            fms: self.bits(8, 1) == 1,
            selected_alt_ft: (alt != 0).then(|| (alt as i32 - 1) * 32),
            baro_hpa: (baro != 0).then(|| 800.0 + f64::from(baro - 1) * 0.8),
            heading_deg: (self.bits(29, 1) == 1)
                .then(|| f64::from(self.bits(30, 9)) * 180.0 / 256.0),
            autopilot: modes.then(|| self.bits(47, 1) == 1),
            vnav: modes.then(|| self.bits(48, 1) == 1),
            alt_hold: modes.then(|| self.bits(49, 1) == 1),
            approach: modes.then(|| self.bits(51, 1) == 1),
            lnav: modes.then(|| self.bits(53, 1) == 1),
        })
    }

    /// Operational status (TC 31, subtypes 0 airborne and 1 surface).
    pub fn op_status(&self) -> Option<OpStatus> {
        if self.tc() != 31 || self.bits(5, 3) > 1 {
            return None;
        }
        Some(OpStatus {
            surface: self.bits(5, 3) == 1,
            version: self.bits(40, 3),
            nic_a: self.bits(43, 1),
            nacp: self.bits(44, 4),
            sil: self.bits(50, 2),
        })
    }
}

/// A 56-bit ME, MB or MV field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mb(pub u64);

impl Mb {
    /// Bits `first..=last` (1-based, as in Doc 9871).
    pub fn bits(self, first: u32, last: u32) -> u32 {
        ((self.0 >> (56 - last)) & ((1u64 << (last - first + 1)) - 1)) as u32
    }

    pub fn bit(self, n: u32) -> bool {
        self.bits(n, n) == 1
    }

    /// The eight 6-bit characters in bits 9–56 (identification, BDS 2,0).
    pub fn chars(self) -> CallSign {
        const CHARS: &[u8; 64] =
            b"?ABCDEFGHIJKLMNOPQRSTUVWXYZ????? ???????????????0123456789??????";
        let mut cs = CallSign([b' '; 8]);
        for (i, c) in cs.0.iter_mut().enumerate() {
            let first = 9 + 6 * i as u32;
            *c = CHARS[self.bits(first, first + 5) as usize];
        }
        cs
    }
}

/// A CPR-encoded position (17-bit latitude and longitude).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cpr {
    pub odd: bool,
    pub lat: u32,
    pub lon: u32,
    /// A surface position (90° zones instead of 360°).
    pub surface: bool,
}

/// Ground velocity or airspeed (TC 19).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Velocity {
    Ground {
        speed_kt: f64,
        track_deg: f64,
        vrate_fpm: Option<i32>,
    },
    Air {
        speed_kt: f64,
        true_airspeed: bool,
        heading_deg: Option<f64>,
        vrate_fpm: Option<i32>,
    },
}

/// Target state and status (TC 29).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetState {
    /// The selected altitude is the FMS's (else the MCP/FCU's).
    pub fms: bool,
    pub selected_alt_ft: Option<i32>,
    pub baro_hpa: Option<f64>,
    pub heading_deg: Option<f64>,
    pub autopilot: Option<bool>,
    pub vnav: Option<bool>,
    pub alt_hold: Option<bool>,
    pub approach: Option<bool>,
    pub lnav: Option<bool>,
}

/// Operational status (TC 31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpStatus {
    pub surface: bool,
    /// ADS-B version: 0 (DO-260), 1 (DO-260A) or 2 (DO-260B).
    pub version: u32,
    pub nic_a: u32,
    pub nacp: u32,
    pub sil: u32,
}

/// An eight-character call sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSign(pub [u8; 8]);

impl CallSign {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("").trim_matches(' ')
    }

    /// Only letters, digits and trailing spaces, and not empty.
    pub fn plausible(&self) -> bool {
        let s = self.as_str();
        !s.is_empty()
            && self.0.starts_with(s.as_bytes())
            && s.bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    }
}

// ---- Altitude and identity codes ---------------------------------------------------

/// Surface movement (ground speed) code → knots (DO-260B 2.2.3.2.4.2).
fn movement_kt(m: u32) -> Option<f64> {
    let m = f64::from(m);
    Some(match m as u32 {
        1 => 0.0,
        2..=8 => 0.125 + (m - 2.0) * 0.125,
        9..=12 => 1.0 + (m - 9.0) * 0.25,
        13..=38 => 2.0 + (m - 13.0) * 0.5,
        39..=93 => 15.0 + (m - 39.0),
        94..=108 => 70.0 + (m - 94.0) * 2.0,
        109..=123 => 100.0 + (m - 109.0) * 5.0,
        124 => 175.0,
        _ => return None,
    })
}

/// The 13 identity bits C1 A1 C2 A2 C4 A4 X B1 D1 B2 D2 B4 D4 (DF5/21, and
/// a Mode A/C reply in time order) as the four octal digits `ABCD` packed
/// in hex nibbles, so `{:04X}` prints the squawk.
pub fn id13_squawk(id: u32) -> u32 {
    const MAP: [(u32, u32); 12] = [
        (0x1000, 0x0010), // C1
        (0x0800, 0x1000), // A1
        (0x0400, 0x0020), // C2
        (0x0200, 0x2000), // A2
        (0x0100, 0x0040), // C4
        (0x0080, 0x4000), // A4
        (0x0020, 0x0100), // B1
        (0x0010, 0x0001), // D1
        (0x0008, 0x0200), // B2
        (0x0004, 0x0002), // D2
        (0x0002, 0x0400), // B4
        (0x0001, 0x0004), // D4
    ];
    MAP.iter()
        .filter(|(from, _)| id & from != 0)
        .fold(0, |acc, (_, to)| acc | to)
}

/// A Mode C (Gillham) altitude from a squawk-layout code (see
/// [`id13_squawk`]), in feet; `None` for codes that aren't altitudes.
pub fn gillham_ft(code: u32) -> Option<i32> {
    // D1 is never used for altitude; some C bit must be set.
    if code & 0xFFFF_8889 != 0 || code & 0x0070 == 0 {
        return None;
    }
    // The 100 ft digit, a 5-cycle reflected code in C1 C2 C4.
    let mut hundreds = 0u32;
    if code & 0x0010 != 0 {
        hundreds ^= 7;
    }
    if code & 0x0020 != 0 {
        hundreds ^= 3;
    }
    if code & 0x0040 != 0 {
        hundreds ^= 1;
    }
    if hundreds & 5 == 5 {
        hundreds ^= 2;
    }
    if hundreds > 5 {
        return None;
    }
    // The 500 ft count, Gray coded in D2 D4 A1 A2 A4 B1 B2 B4.
    let mut fives = 0u32;
    for (bit, x) in [
        (0x0002, 0xFF),
        (0x0004, 0x7F),
        (0x1000, 0x3F),
        (0x2000, 0x1F),
        (0x4000, 0x0F),
        (0x0100, 0x07),
        (0x0200, 0x03),
        (0x0400, 0x01),
    ] {
        if code & bit != 0 {
            fives ^= x;
        }
    }
    if fives & 1 != 0 {
        hundreds = 6 - hundreds;
    }
    Some((fives as i32 * 5 + hundreds as i32 - 13) * 100)
}

/// The 13-bit altitude code (AC) of DF0/4/16/20, in feet. M = 1 (metres)
/// is not decoded.
pub fn ac13_ft(ac: u32) -> Option<i32> {
    if ac == 0 || ac & 0x40 != 0 {
        return None;
    }
    if ac & 0x10 != 0 {
        // Q = 1: 25 ft steps.
        let n = ((ac & 0x1F80) >> 2) | ((ac & 0x20) >> 1) | (ac & 0xF);
        return Some(n as i32 * 25 - 1000);
    }
    gillham_ft(id13_squawk(ac))
}

/// The 12-bit altitude of an airborne position (the AC code without M).
pub fn ac12_ft(a: u32) -> Option<i32> {
    ac13_ft(((a & 0xFC0) << 1) | (a & 0x3F))
}
