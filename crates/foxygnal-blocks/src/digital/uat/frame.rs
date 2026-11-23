//! UAT payloads (DO-282B): the ADS-B message from aircraft (header, state
//! vector, mode status, auxiliary state vector) and the ground uplink (its
//! header, the information frames, and the FIS-B products in them).
//!
//! Field positions follow DO-282B §2.2.4; bytes are numbered from 0 here.

use std::fmt::Write;

use super::super::text::Escaped;

/// An ADS-B payload: 18 bytes (Basic) or 34 (Long).
pub struct AdsbMessage<'a>(pub &'a [u8]);

/// Latitude/longitude from 24-bit angular weighted binary fields.
fn latlon(raw_lat: u32, raw_lon: u32) -> (f64, f64) {
    let mut lat = f64::from(raw_lat) * 360.0 / 16_777_216.0;
    if lat > 90.0 {
        lat -= 180.0;
    }
    let mut lon = f64::from(raw_lon) * 360.0 / 16_777_216.0;
    if lon > 180.0 {
        lon -= 360.0;
    }
    (lat, lon)
}

/// Base-40 characters of the call sign field.
const BASE40: &[u8; 40] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ  ..";

impl AdsbMessage<'_> {
    pub fn payload_type(&self) -> u8 {
        self.0[0] >> 3
    }

    pub fn address_qualifier(&self) -> u8 {
        self.0[0] & 7
    }

    pub fn address(&self) -> u32 {
        u32::from(self.0[1]) << 16 | u32::from(self.0[2]) << 8 | u32::from(self.0[3])
    }

    /// Whether this payload type carries a mode status element.
    pub fn has_mode_status(&self) -> bool {
        self.0.len() >= 34 && matches!(self.payload_type(), 1 | 3)
    }

    /// Whether this payload type carries an auxiliary state vector.
    pub fn has_aux(&self) -> bool {
        self.0.len() >= 34 && matches!(self.payload_type(), 1 | 2 | 5 | 6)
    }

    /// Payload types 0–10 carry a state vector.
    pub fn has_state_vector(&self) -> bool {
        self.payload_type() <= 10
    }

    pub fn nic(&self) -> u8 {
        self.0[11] & 0x0F
    }

    pub fn position(&self) -> Option<(f64, f64)> {
        let f = self.0;
        let raw_lat = u32::from(f[4]) << 15 | u32::from(f[5]) << 7 | u32::from(f[6]) >> 1;
        let raw_lon = u32::from(f[6] & 1) << 23
            | u32::from(f[7]) << 15
            | u32::from(f[8]) << 7
            | u32::from(f[9]) >> 1;
        (self.nic() != 0 || raw_lat != 0 || raw_lon != 0).then(|| latlon(raw_lat, raw_lon))
    }

    /// Altitude in feet and whether it is geometric (else barometric).
    pub fn altitude(&self) -> Option<(i32, bool)> {
        let f = self.0;
        let raw = i32::from(f[10]) << 4 | i32::from(f[11] >> 4);
        (raw != 0).then(|| ((raw - 1) * 25 - 1000, f[9] & 1 == 1))
    }

    /// Air/ground state: 0 subsonic, 1 supersonic, 2 on the ground.
    pub fn air_ground(&self) -> u8 {
        self.0[12] >> 6
    }

    /// North and east velocity (kt) when airborne.
    pub fn velocity(&self) -> (Option<i32>, Option<i32>) {
        let f = self.0;
        if self.air_ground() > 1 {
            return (None, None);
        }
        let scale = if self.air_ground() == 1 { 4 } else { 1 };
        let field = |raw: u32| {
            (raw & 0x3FF != 0).then(|| {
                let v = (raw & 0x3FF) as i32 - 1;
                scale * if raw & 0x400 != 0 { -v } else { v }
            })
        };
        let ns = u32::from(f[12] & 0x1F) << 6 | u32::from(f[13]) >> 2;
        let ew = u32::from(f[13] & 3) << 9 | u32::from(f[14]) << 1 | u32::from(f[15]) >> 7;
        (field(ns), field(ew))
    }

    /// Vertical rate (ft/min) and whether it is barometric.
    pub fn vertical_rate(&self) -> Option<(i32, bool)> {
        let f = self.0;
        if self.air_ground() > 1 {
            return None;
        }
        let raw = u32::from(f[15] & 0x7F) << 4 | u32::from(f[16]) >> 4;
        (raw & 0x1FF != 0).then(|| {
            let v = ((raw & 0x1FF) as i32 - 1) * 64;
            (if raw & 0x200 != 0 { -v } else { v }, raw & 0x400 != 0)
        })
    }

    /// Ground speed (kt) and track/heading (degrees) when on the ground.
    pub fn ground_movement(&self) -> (Option<i32>, Option<f64>) {
        let f = self.0;
        if self.air_ground() != 2 {
            return (None, None);
        }
        let gs = u32::from(f[12] & 0x1F) << 6 | u32::from(f[13]) >> 2;
        let trk = u32::from(f[13] & 3) << 9 | u32::from(f[14]) << 1 | u32::from(f[15]) >> 7;
        (
            (gs & 0x3FF != 0).then(|| (gs & 0x3FF) as i32 - 1),
            (trk & 0x600 != 0).then(|| f64::from(trk & 0x1FF) * 360.0 / 512.0),
        )
    }

    /// The TIS-B site (address qualifiers 2 and 3) or the UTC-coupled flag
    /// and uplink feedback (others).
    pub fn tisb_site(&self) -> Option<u8> {
        matches!(self.address_qualifier(), 2 | 3).then(|| self.0[16] & 0x0F)
    }

    /// Emitter category and the 8-character call sign or squawk (trailing
    /// spaces trimmed), and whether it is a call sign (else a squawk).
    pub fn mode_status(&self, out: &mut [u8; 8]) -> Option<(u8, usize, bool)> {
        if !self.has_mode_status() {
            return None;
        }
        let f = self.0;
        let w = |i: usize| u16::from(f[i]) << 8 | u16::from(f[i + 1]);
        let (a, b, c) = (w(17), w(19), w(21));
        let cat = ((a / 1600) % 40) as u8;
        let chars = [a / 40, a, b / 1600, b / 40, b, c / 1600, c / 40, c];
        for (o, v) in out.iter_mut().zip(chars) {
            *o = BASE40[usize::from(v % 40)];
        }
        let len = out.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
        Some((cat, len, f[26] & 0x02 != 0))
    }

    /// Emergency/priority status, UAT version, SIL, NACp from the mode status.
    pub fn status_fields(&self) -> Option<(u8, u8, u8, u8)> {
        self.has_mode_status().then(|| {
            let f = self.0;
            (f[23] >> 5, (f[23] >> 2) & 7, f[23] & 3, f[25] >> 4)
        })
    }

    /// The secondary altitude (the other kind than the state vector's).
    pub fn aux_altitude(&self) -> Option<i32> {
        if !self.has_aux() {
            return None;
        }
        let f = self.0;
        let raw = i32::from(f[29]) << 4 | i32::from(f[30] >> 4);
        (raw != 0).then(|| (raw - 1) * 25 - 1000)
    }
}

/// Format an ADS-B payload as `key=value` fields (no newline).
pub fn format_adsb(m: &AdsbMessage<'_>, out: &mut impl Write) -> std::fmt::Result {
    write!(
        out,
        " type=adsb mdb={} addr={:06X} aq={}",
        m.payload_type(),
        m.address(),
        m.address_qualifier()
    )?;
    if !m.has_state_vector() {
        return Ok(());
    }
    if let Some((lat, lon)) = m.position() {
        write!(out, " lat={lat:.5} lon={lon:.5}")?;
    }
    write!(out, " nic={}", m.nic())?;
    let geo = m.altitude().map(|(a, geo)| {
        let kind = if geo { "geo" } else { "baro" };
        let _ = write!(out, " alt_ft={a} alt_type={kind}");
        geo
    });
    match m.air_ground() {
        2 => {
            out.write_str(" ground=1")?;
            let (gs, trk) = m.ground_movement();
            if let Some(v) = gs {
                write!(out, " gs_kt={v}")?;
            }
            if let Some(v) = trk {
                write!(out, " track_deg={v:.1}")?;
            }
        }
        3 => {}
        _ => {
            let (ns, ew) = m.velocity();
            if let (Some(n), Some(e)) = (ns, ew) {
                let gs = f64::from(n).hypot(f64::from(e));
                write!(out, " ns_kt={n} ew_kt={e} gs_kt={gs:.0}")?;
                if n != 0 || e != 0 {
                    let trk = f64::from(e)
                        .atan2(f64::from(n))
                        .to_degrees()
                        .rem_euclid(360.0);
                    write!(out, " track_deg={trk:.1}")?;
                }
            }
            if let Some((v, baro)) = m.vertical_rate() {
                let src = if baro { "baro" } else { "geo" };
                write!(out, " vrate_fpm={v} vrate_src={src}")?;
            }
            if m.air_ground() == 1 {
                out.write_str(" supersonic=1")?;
            }
        }
    }
    if let Some(site) = m.tisb_site() {
        write!(out, " tisb_site={site}")?;
    }
    let mut cs = [0u8; 8];
    if let Some((cat, len, is_callsign)) = m.mode_status(&mut cs) {
        let s = std::str::from_utf8(&cs[..len]).unwrap_or("");
        if is_callsign {
            write!(out, " callsign={s}")?;
        } else {
            write!(out, " squawk={s}")?;
        }
        write!(out, " category={cat}")?;
        if let Some((emerg, version, sil, nacp)) = m.status_fields() {
            write!(
                out,
                " emergency={emerg} uat_version={version} sil={sil} nacp={nacp}"
            )?;
        }
    }
    if let Some(a) = m.aux_altitude() {
        let kind = if geo == Some(false) { "geo" } else { "baro" };
        write!(out, " alt2_ft={a} alt2_type={kind}")?;
    }
    Ok(())
}

// ---- Uplink ------------------------------------------------------------------

/// The 432-byte payload of a ground uplink.
pub struct Uplink<'a>(pub &'a [u8]);

/// What one information frame held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoFrame<'a> {
    /// A FIS-B APDU (frame type 0).
    Fisb(Apdu<'a>),
    /// Another frame type, with its length.
    Other(u8, usize),
}

/// A FIS-B application data unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Apdu<'a> {
    pub product: u16,
    /// Segmented (a product split over several APDUs).
    pub segmented: bool,
    pub month: Option<u8>,
    pub day: Option<u8>,
    pub hour: u8,
    pub minute: u8,
    pub second: Option<u8>,
    pub data: &'a [u8],
}

impl<'a> Uplink<'a> {
    pub fn position(&self) -> Option<(f64, f64)> {
        let f = self.0;
        let raw_lat = u32::from(f[0]) << 15 | u32::from(f[1]) << 7 | u32::from(f[2]) >> 1;
        let raw_lon = u32::from(f[2] & 1) << 23
            | u32::from(f[3]) << 15
            | u32::from(f[4]) << 7
            | u32::from(f[5]) >> 1;
        (f[5] & 1 == 1).then(|| latlon(raw_lat, raw_lon))
    }

    pub fn utc_coupled(&self) -> bool {
        self.0[6] & 0x80 != 0
    }

    pub fn app_data_valid(&self) -> bool {
        self.0[6] & 0x20 != 0
    }

    pub fn slot(&self) -> u8 {
        self.0[6] & 0x1F
    }

    pub fn tisb_site(&self) -> u8 {
        self.0[7] >> 4
    }

    /// The information frames of the application data (bytes 8–431).
    pub fn frames(&self) -> impl Iterator<Item = InfoFrame<'a>> {
        let data: &'a [u8] = &self.0[8..];
        let mut at = 0usize;
        std::iter::from_fn(move || {
            if at + 2 > data.len() {
                return None;
            }
            let len = usize::from(data[at]) << 1 | usize::from(data[at + 1] >> 7);
            let kind = data[at + 1] & 0x0F;
            if len == 0 || at + 2 + len > data.len() {
                return None;
            }
            let body = &data[at + 2..at + 2 + len];
            at += 2 + len;
            Some(match kind {
                0 => apdu(body).map_or(InfoFrame::Other(0, len), InfoFrame::Fisb),
                k => InfoFrame::Other(k, len),
            })
        })
    }
}

fn apdu(d: &[u8]) -> Option<Apdu<'_>> {
    if d.len() < 4 {
        return None;
    }
    let product = u16::from(d[0] & 0x1F) << 6 | u16::from(d[1] >> 2);
    let segmented = d[1] & 0x02 != 0;
    let t_opt = (d[1] & 1) << 1 | d[2] >> 7;
    let (month, day, hour, minute, second, header) = match t_opt {
        0 => (
            None,
            None,
            (d[2] & 0x7C) >> 2,
            (d[2] & 3) << 4 | d[3] >> 4,
            None,
            4,
        ),
        1 => {
            if d.len() < 5 {
                return None;
            }
            (
                None,
                None,
                (d[2] & 0x7C) >> 2,
                (d[2] & 3) << 4 | d[3] >> 4,
                Some((d[3] & 0x0F) << 2 | d[4] >> 6),
                5,
            )
        }
        2 => {
            if d.len() < 5 {
                return None;
            }
            (
                Some((d[2] & 0x78) >> 3),
                Some((d[2] & 7) << 2 | d[3] >> 6),
                (d[3] & 0x3E) >> 1,
                (d[3] & 1) << 5 | d[4] >> 3,
                None,
                5,
            )
        }
        _ => {
            if d.len() < 6 {
                return None;
            }
            (
                Some((d[2] & 0x78) >> 3),
                Some((d[2] & 7) << 2 | d[3] >> 6),
                (d[3] & 0x3E) >> 1,
                (d[3] & 1) << 5 | d[4] >> 3,
                Some((d[4] & 7) << 3 | d[5] >> 5),
                6,
            )
        }
    };
    Some(Apdu {
        product,
        segmented,
        month,
        day,
        hour,
        minute,
        second,
        data: &d[header..],
    })
}

/// The DLAC alphabet (6-bit characters of FIS-B text): 0 is ETX, 28 a tab
/// (the next character counts spaces), 29 the record separator.
const DLAC: &[u8; 64] =
    b"\x03ABCDEFGHIJKLMNOPQRSTUVWXYZ\x1A\t\x1E\n| !\"#$%&'()*+,-./0123456789:;<=>?";

/// Decode DLAC text (four characters in three bytes) into `out`, expanding
/// tabs. Stops at an ETX.
pub fn dlac(data: &[u8], out: &mut Vec<u8>) {
    let mut tab = false;
    for i in 0..data.len() * 4 / 3 {
        let bit = i * 6;
        let (byte, shift) = (bit / 8, bit % 8);
        let word = u16::from(data[byte]) << 8 | u16::from(data.get(byte + 1).copied().unwrap_or(0));
        let ch = ((word >> (10 - shift)) & 0x3F) as usize;
        if tab {
            out.extend(std::iter::repeat_n(b' ', ch));
            tab = false;
            continue;
        }
        match ch {
            0 => break,
            28 => tab = true,
            c => out.push(DLAC[c]),
        }
    }
}

/// The text records of a FIS-B product, if it is text: product 413
/// (generic text: METAR, TAF, PIREP, winds aloft, …) is DLAC text with
/// records separated by RS; products 8–16 (NOTAMs, AIRMET, SIGMET, …) in the
/// text-with-graphics format carry a text record when the record format is
/// 2. Each record is passed to `each`.
pub fn text_records(a: &Apdu<'_>, buf: &mut Vec<u8>, mut each: impl FnMut(&[u8])) -> bool {
    buf.clear();
    match a.product {
        413 => dlac(a.data, buf),
        8..=17 => {
            let d = a.data;
            if d.len() < 11 || d[0] >> 4 != 2 {
                return false;
            }
            let rec_len = usize::from(d[6]) << 8 | usize::from(d[7]);
            if rec_len < 5 || 6 + rec_len > d.len() {
                return false;
            }
            dlac(&d[11..6 + rec_len], buf);
        }
        _ => return false,
    }
    for rec in buf.split(|&c| c == 0x1E) {
        let rec = rec.trim_ascii();
        if !rec.is_empty() {
            each(rec);
        }
    }
    true
}

/// A short name for a FIS-B product, for counting.
pub fn product_kind(p: u16) -> &'static str {
    match p {
        63 | 64 => "nexrad",
        8..=17 | 413 => "text",
        _ => "other",
    }
}

/// Escaped text for a record.
pub fn escaped(rec: &[u8]) -> Escaped<'_> {
    Escaped(rec)
}
