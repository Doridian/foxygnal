//! Comm-B (DF20/21): the 56-bit MB field holds one of many registers ("BDS
//! codes"), but which one isn't sent: the ground station knows what it asked
//! for, a passive receiver has to guess. As dump1090 and pyModeS do, each
//! candidate register is checked for reserved bits, status bits that agree
//! with their fields (a field marked unavailable must be all zeros) and
//! values in a plausible range; a message is reported as the register that
//! fits, as ambiguous when several fit equally well, and as unknown when none
//! does.
//!
//! Registers 1,0, 2,0 and 3,0 carry their own number in the first byte;
//! 1,7 is a capability bitmap with a fixed shape. 4,0, 5,0 and 6,0 have no
//! marker, so 5,0 and 6,0 in particular are told apart by plausibility and,
//! when the aircraft also sends ADS-B, by agreement with its ADS-B speed and
//! track.

use super::msg::{CallSign, Mb};

/// What the MB field of a DF20/21 reply was decoded as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommB {
    /// All zeros: nothing to decode.
    Empty,
    /// 1,0: data link capability.
    DataLink,
    /// 1,7: common-usage GICB capability; bit i (from the most significant
    /// of 24) set when register `GICB_REGISTERS[i]` is supported.
    Gicb(u32),
    /// 2,0: aircraft identification.
    Ident(CallSign),
    /// 3,0: ACAS active resolution advisory.
    Acas(Acas),
    /// 4,0: selected vertical intention.
    Intent(Intent),
    /// 5,0: track and turn.
    TrackTurn(TrackTurn),
    /// 6,0: heading and speed.
    HeadingSpeed(HeadingSpeed),
    /// More than one register fits.
    Ambiguous,
    /// No register fits.
    Unknown,
}

/// The registers of a 1,7 capability report, most significant bit first.
pub const GICB_REGISTERS: [&str; 24] = [
    "0,5", "0,6", "0,7", "0,8", "0,9", "0,A", "2,0", "2,1", "4,0", "4,1", "4,2", "4,3", "4,4",
    "4,5", "4,8", "5,0", "5,1", "5,2", "5,3", "5,4", "5,5", "5,6", "5,F", "6,0",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acas {
    /// Active resolution advisories (14 bits).
    pub ara: u32,
    /// RA terminated.
    pub rat: bool,
    /// Multiple threat encounter.
    pub mte: bool,
    /// Threat type: 0 none, 1 Mode S address, 2 altitude/range/bearing.
    pub tti: u32,
    /// The threat's address when `tti` is 1.
    pub threat: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Intent {
    pub mcp_alt_ft: Option<i32>,
    pub fms_alt_ft: Option<i32>,
    pub baro_hpa: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackTurn {
    pub roll_deg: Option<f64>,
    pub track_deg: Option<f64>,
    pub gs_kt: Option<f64>,
    pub track_rate: Option<f64>,
    pub tas_kt: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadingSpeed {
    pub heading_deg: Option<f64>,
    pub ias_kt: Option<f64>,
    pub mach: Option<f64>,
    pub baro_rate_fpm: Option<i32>,
    pub ins_rate_fpm: Option<i32>,
}

/// What else is known about the aircraft, for telling 5,0 from 6,0.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reference {
    /// Barometric altitude from the same reply (DF20) or recent messages.
    pub alt_ft: Option<i32>,
    /// ADS-B ground speed and track from the last few seconds.
    pub gs_kt: Option<f64>,
    pub track_deg: Option<f64>,
}

/// A field with a status bit at `status` and a value in `first..=last`:
/// `Err` if unavailable but not zero, `Ok(None)` if unavailable.
fn field(mb: Mb, status: u32, first: u32, last: u32) -> Result<Option<u32>, ()> {
    let v = mb.bits(first, last);
    match (mb.bit(status), v) {
        (false, 0) => Ok(None),
        (false, _) => Err(()),
        (true, v) => Ok(Some(v)),
    }
}

/// A two's complement value in `first..=last` whose sign bit is `first`.
fn signed(mb: Mb, first: u32, last: u32) -> i32 {
    let n = last - first + 1;
    let v = mb.bits(first, last) as i32;
    if v & (1 << (n - 1)) != 0 {
        v - (1 << n)
    } else {
        v
    }
}

fn bds10(mb: Mb) -> bool {
    mb.bits(1, 8) == 0x10 && mb.bits(10, 14) == 0
}

fn bds17(mb: Mb) -> Option<u32> {
    let caps = mb.bits(1, 24);
    // 2,0 is always supported; the rest of the field is reserved.
    (caps & (1 << 17) != 0 && mb.bits(25, 56) == 0).then_some(caps)
}

fn bds20(mb: Mb) -> Option<CallSign> {
    let cs = mb.chars();
    (mb.bits(1, 8) == 0x20 && cs.plausible()).then_some(cs)
}

fn bds30(mb: Mb) -> Option<Acas> {
    if mb.bits(1, 8) != 0x30 {
        return None;
    }
    let tti = mb.bits(29, 30);
    // Threat type 3 is unassigned; the top ARA codes are reserved (ACAS III).
    if tti == 3 || mb.bits(16, 22) >= 48 {
        return None;
    }
    Some(Acas {
        ara: mb.bits(9, 22),
        rat: mb.bit(27),
        mte: mb.bit(28),
        tti,
        threat: (tti == 1).then(|| mb.bits(31, 54)),
    })
}

fn bds40(mb: Mb) -> Option<Intent> {
    let mcp = field(mb, 1, 2, 13).ok()?;
    let fms = field(mb, 14, 15, 26).ok()?;
    let baro = field(mb, 27, 28, 39).ok()?;
    field(mb, 48, 49, 51).ok()?;
    field(mb, 54, 55, 56).ok()?;
    if mb.bits(40, 47) != 0 || mb.bits(52, 53) != 0 {
        return None;
    }
    // Selected altitudes are in 100s of feet, sent in 16 ft steps.
    let alt = |v: Option<u32>| -> Result<Option<i32>, ()> {
        match v {
            None => Ok(None),
            Some(v) => {
                let ft = v as i32 * 16;
                let off = ft.rem_euclid(100);
                if ft > 50_000 || (off > 16 && off < 84) {
                    Err(())
                } else {
                    Ok(Some(ft))
                }
            }
        }
    };
    let baro_hpa = baro.map(|v| 800.0 + f64::from(v) * 0.1);
    if baro_hpa.is_some_and(|b| !(900.0..=1100.0).contains(&b)) {
        return None;
    }
    let out = Intent {
        mcp_alt_ft: alt(mcp).ok()?,
        fms_alt_ft: alt(fms).ok()?,
        baro_hpa,
    };
    (out.mcp_alt_ft.is_some() || out.fms_alt_ft.is_some() || out.baro_hpa.is_some()).then_some(out)
}

fn bds50(mb: Mb) -> Option<TrackTurn> {
    let roll = field(mb, 1, 2, 11)
        .ok()?
        .map(|_| f64::from(signed(mb, 2, 11)) * 45.0 / 256.0);
    let track = field(mb, 12, 13, 23)
        .ok()?
        .map(|_| (f64::from(signed(mb, 13, 23)) * 90.0 / 512.0).rem_euclid(360.0));
    let gs = field(mb, 24, 25, 34).ok()?.map(|v| f64::from(v) * 2.0);
    let rate = field(mb, 35, 36, 45)
        .ok()?
        .map(|_| f64::from(signed(mb, 36, 45)) * 8.0 / 256.0);
    let tas = field(mb, 46, 47, 56).ok()?.map(|v| f64::from(v) * 2.0);
    if roll.is_some_and(|r| r.abs() > 50.0)
        || gs.is_some_and(|g| g > 600.0)
        || tas.is_some_and(|t| t > 600.0)
        || rate.is_some_and(|r| r.abs() > 10.0)
    {
        return None;
    }
    if let (Some(g), Some(t)) = (gs, tas)
        && (g - t).abs() > 200.0
    {
        return None;
    }
    Some(TrackTurn {
        roll_deg: roll,
        track_deg: track,
        gs_kt: gs,
        track_rate: rate,
        tas_kt: tas,
    })
}

fn bds60(mb: Mb, alt_ft: Option<i32>) -> Option<HeadingSpeed> {
    let heading = field(mb, 1, 2, 12)
        .ok()?
        .map(|_| (f64::from(signed(mb, 2, 12)) * 90.0 / 512.0).rem_euclid(360.0));
    let ias = field(mb, 13, 14, 23).ok()?.map(f64::from);
    let mach = field(mb, 24, 25, 34)
        .ok()?
        .map(|v| f64::from(v) * 2.048 / 512.0);
    let baro = field(mb, 35, 36, 45).ok()?.map(|_| signed(mb, 36, 45) * 32);
    let ins = field(mb, 46, 47, 56).ok()?.map(|_| signed(mb, 47, 56) * 32);
    if ias.is_some_and(|v| v > 500.0 || v == 0.0)
        || mach.is_some_and(|m| m > 1.0 || m == 0.0)
        || baro.is_some_and(|v| v.abs() > 6000)
        || ins.is_some_and(|v| v.abs() > 6000)
    {
        return None;
    }
    if let (Some(b), Some(i)) = (baro, ins)
        && (b - i).abs() > 2000
    {
        return None;
    }
    // At altitude, the Mach number and the indicated airspeed must agree.
    if let (Some(m), Some(ias), Some(alt)) = (mach, ias, alt_ft)
        && alt > 10_000
        && (mach_to_cas_kt(m, f64::from(alt)) - ias).abs() > 20.0
    {
        return None;
    }
    Some(HeadingSpeed {
        heading_deg: heading,
        ias_kt: ias,
        mach,
        baro_rate_fpm: baro,
        ins_rate_fpm: ins,
    })
}

/// Calibrated airspeed (kt) for a Mach number at a pressure altitude (ft),
/// in the ISA atmosphere.
pub fn mach_to_cas_kt(mach: f64, alt_ft: f64) -> f64 {
    let h = alt_ft * 0.3048;
    let (t, p) = if h < 11_000.0 {
        let t = 288.15 - 0.0065 * h;
        (t, 101_325.0 * (t / 288.15).powf(5.255_88))
    } else {
        (216.65, 22_632.06 * (-(h - 11_000.0) / 6341.62).exp())
    };
    let a = (1.4 * 287.052_87 * t).sqrt();
    let tas = mach * a;
    let qc = p * ((1.0 + 0.2 * (tas / a).powi(2)).powf(3.5) - 1.0);
    let (a0, p0) = (340.294, 101_325.0);
    let cas = a0 * (5.0 * ((qc / p0 + 1.0).powf(2.0 / 7.0) - 1.0)).sqrt();
    cas / 0.514_444
}

fn angle_diff(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Decode a Comm-B MB field.
pub fn decode(mb: Mb, r: Reference) -> CommB {
    if mb.0 == 0 {
        return CommB::Empty;
    }
    // Registers that announce themselves win outright.
    if let Some(cs) = bds20(mb) {
        return CommB::Ident(cs);
    }
    if let Some(a) = bds30(mb) {
        return CommB::Acas(a);
    }
    if bds10(mb) {
        return CommB::DataLink;
    }
    if let Some(c) = bds17(mb) {
        return CommB::Gicb(c);
    }
    let b40 = bds40(mb);
    let mut b50 = bds50(mb);
    let mut b60 = bds60(mb, r.alt_ft);
    // With ADS-B speed and track to compare against, 5,0 and 6,0 rarely
    // both agree.
    if b50.is_some() && b60.is_some() {
        if let (Some(t50), Some(gs)) = (b50, r.gs_kt) {
            let ok = t50.gs_kt.is_none_or(|g| (g - gs).abs() < 30.0)
                && t50
                    .track_deg
                    .zip(r.track_deg)
                    .is_none_or(|(a, b)| angle_diff(a, b) < 15.0);
            if !ok {
                b50 = None;
            }
        }
        if let (Some(h60), Some(trk)) = (b60, r.track_deg)
            && h60.heading_deg.is_some_and(|h| angle_diff(h, trk) > 45.0)
        {
            b60 = None;
        }
    }
    match (b40, b50, b60) {
        (Some(i), None, None) => CommB::Intent(i),
        (None, Some(t), None) => CommB::TrackTurn(t),
        (None, None, Some(h)) => CommB::HeadingSpeed(h),
        (None, None, None) => CommB::Unknown,
        _ => CommB::Ambiguous,
    }
}
