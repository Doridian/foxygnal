//! AX.25 frames and the APRS information field, as text.
//!
//! [`describe`] turns one frame (address field to the end of the info field,
//! FCS already checked and removed) into `key=value` fields:
//! `src=N0CALL-9 dst=APRS path=WIDE1-1,WIDE2-1* type=position lat=… lon=…
//! sym=/> comment="…"`. UI frames with PID F0 are APRS and their info field
//! is decoded (positions with or without timestamp, uncompressed or
//! compressed, Mic-E, status, message, object, item; anything else as
//! `type=other`); other frames show their control field, PID and info.
//! References: AX.25 v2.2, APRS Protocol Reference 1.0.1 (APRS101).

use std::fmt::Write;

use super::text::Escaped;

/// One address of the address field: call sign, SSID, and the C/H bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub call: [u8; 6],
    pub len: usize,
    pub ssid: u8,
    /// Command/response bit (destination, source) or has-been-repeated
    /// (digipeaters).
    pub bit7: bool,
}

impl Address {
    fn parse(b: &[u8]) -> Option<Self> {
        let mut call = [b' '; 6];
        let mut len = 0;
        for (i, &c) in b[..6].iter().enumerate() {
            if c & 1 != 0 {
                return None;
            }
            let c = c >> 1;
            if c == b' ' {
                continue;
            }
            // Letters and digits; padding only at the end.
            if !c.is_ascii_alphanumeric() || len != i {
                return None;
            }
            call[i] = c;
            len = i + 1;
        }
        if len == 0 {
            return None;
        }
        Some(Self {
            call,
            len,
            ssid: (b[6] >> 1) & 0x0F,
            bit7: b[6] & 0x80 != 0,
        })
    }

    pub fn call(&self) -> &str {
        std::str::from_utf8(&self.call[..self.len]).unwrap_or("")
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.call())?;
        if self.ssid != 0 {
            write!(f, "-{}", self.ssid)?;
        }
        Ok(())
    }
}

/// The parts of an AX.25 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub dst: Address,
    pub src: Address,
    pub digis: [Option<Address>; 8],
    pub control: u8,
    pub pid: Option<u8>,
    pub info: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Parse a frame without its FCS. `None` if the address field is
    /// malformed.
    pub fn parse(b: &'a [u8]) -> Option<Self> {
        let mut digis = [None; 8];
        let mut n = 0;
        let (mut dst, mut src) = (None, None);
        loop {
            let a = b.get(7 * n..7 * n + 7)?;
            let addr = Address::parse(a)?;
            match n {
                0 => dst = Some(addr),
                1 => src = Some(addr),
                _ => *digis.get_mut(n - 2)? = Some(addr),
            }
            n += 1;
            if a[6] & 1 != 0 {
                break;
            }
        }
        if n < 2 {
            return None;
        }
        let rest = &b[7 * n..];
        let (&control, rest) = rest.split_first()?;
        // I frames and UI frames carry a PID.
        let (pid, info) = if control & 1 == 0 || control & !0x10 == 0x03 {
            let (&pid, info) = rest.split_first()?;
            (Some(pid), info)
        } else {
            (None, rest)
        };
        Some(Self {
            dst: dst?,
            src: src?,
            digis,
            control,
            pid,
            info,
        })
    }

    pub fn is_ui(&self) -> bool {
        self.control & !0x10 == 0x03
    }

    fn control_name(&self) -> &'static str {
        let c = self.control;
        if c & 1 == 0 {
            return "I";
        }
        if c & 3 == 1 {
            return ["RR", "RNR", "REJ", "SREJ"][usize::from((c >> 2) & 3)];
        }
        match c & !0x10 {
            0x03 => "UI",
            0x2F => "SABM",
            0x6F => "SABME",
            0x43 => "DISC",
            0x0F => "DM",
            0x63 => "UA",
            0x87 => "FRMR",
            0xAF => "XID",
            0xE3 => "TEST",
            _ => "U",
        }
    }
}

/// Write `frame` (without FCS) as text fields into `out`. Returns false (and
/// writes nothing) if it isn't an AX.25 frame.
pub fn describe(frame: &[u8], out: &mut String) -> bool {
    let Some(f) = Frame::parse(frame) else {
        return false;
    };
    let _ = write!(out, "src={} dst={}", f.src, f.dst);
    for (i, d) in f.digis.iter().flatten().enumerate() {
        let sep = if i == 0 { " path=" } else { "," };
        let star = if d.bit7 { "*" } else { "" };
        let _ = write!(out, "{sep}{d}{star}");
    }
    if f.is_ui() && f.pid == Some(0xF0) && !f.info.is_empty() {
        aprs(&f.dst, f.info, out);
    } else {
        let _ = write!(out, " type=ax25 ctl={}", f.control_name());
        if let Some(pid) = f.pid {
            let _ = write!(out, " pid={pid:02X}");
        }
        if !f.info.is_empty() {
            let _ = write!(out, " info=\"{}\"", Escaped(f.info));
        }
    }
    true
}

// ---- APRS -----------------------------------------------------------------------

/// A decoded position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub lat: f64,
    pub lon: f64,
    pub table: u8,
    pub code: u8,
    pub course: Option<u32>,
    pub speed_kt: Option<f64>,
    pub alt_ft: Option<f64>,
}

impl Position {
    fn write(&self, out: &mut String) {
        let _ = write!(out, " lat={:.6} lon={:.6}", self.lat, self.lon);
        if self.table.is_ascii_graphic() && self.code.is_ascii_graphic() {
            let _ = write!(out, " sym={}{}", self.table as char, self.code as char);
        }
        if let Some(c) = self.course {
            let _ = write!(out, " course={c}");
        }
        if let Some(s) = self.speed_kt {
            let _ = write!(out, " speed_kt={s:.1}");
        }
        if let Some(a) = self.alt_ft {
            let _ = write!(out, " alt_ft={a:.0}");
        }
    }
}

fn digit(c: u8) -> Option<f64> {
    match c {
        b'0'..=b'9' => Some(f64::from(c - b'0')),
        b' ' => Some(0.0), // position ambiguity
        _ => None,
    }
}

/// Digits `s` as a number (spaces count as 0).
fn number(s: &[u8]) -> Option<f64> {
    s.iter().try_fold(0.0, |v, &c| Some(v * 10.0 + digit(c)?))
}

fn base91(s: &[u8]) -> Option<f64> {
    s.iter().try_fold(0.0, |v, &c| {
        (33..=123)
            .contains(&c)
            .then(|| v * 91.0 + f64::from(c - 33))
    })
}

/// An uncompressed (`4903.50N/07201.75W-`) or compressed (`/5L!!<*e7>7P[`)
/// position at the start of `s`, and what follows it.
pub fn parse_position(s: &[u8]) -> Option<(Position, &[u8])> {
    let first = *s.first()?;
    if first.is_ascii_digit() || first == b' ' {
        if s.len() < 19 || s[4] != b'.' || s[14] != b'.' {
            return None;
        }
        let lat = number(&s[0..2])? + (number(&s[2..4])? + number(&s[5..7])? / 100.0) / 60.0;
        let lon = number(&s[9..12])? + (number(&s[12..14])? + number(&s[15..17])? / 100.0) / 60.0;
        let lat = match s[7] {
            b'N' | b'n' => lat,
            b'S' | b's' => -lat,
            _ => return None,
        };
        let lon = match s[17] {
            b'E' | b'e' => lon,
            b'W' | b'w' => -lon,
            _ => return None,
        };
        if lat.abs() > 90.0 || lon.abs() > 180.0 {
            return None;
        }
        let mut p = Position {
            lat,
            lon,
            table: s[8],
            code: s[18],
            course: None,
            speed_kt: None,
            alt_ft: None,
        };
        let mut rest = &s[19..];
        // Course/speed data extension: `ddd/ddd`.
        if rest.len() >= 7 && rest[3] == b'/' {
            let (c, v) = (&rest[0..3], &rest[4..7]);
            if c.iter().chain(v).all(u8::is_ascii_digit) {
                p.course = number(c).map(|c| c as u32);
                p.speed_kt = number(v);
                rest = &rest[7..];
            }
        }
        p.alt_ft = altitude(rest);
        Some((p, rest))
    } else {
        if s.len() < 13 {
            return None;
        }
        let table = match s[0] {
            b'/' | b'\\' | b'A'..=b'Z' => s[0],
            b'a'..=b'j' => s[0] - b'a' + b'0',
            _ => return None,
        };
        let y = base91(&s[1..5])?;
        let x = base91(&s[5..9])?;
        let mut p = Position {
            lat: 90.0 - y / 380_926.0,
            lon: -180.0 + x / 190_463.0,
            table,
            code: s[9],
            course: None,
            speed_kt: None,
            alt_ft: None,
        };
        let (c, sp, t) = (s[10], s[11], s[12]);
        if c != b' ' && (33..=123).contains(&c) && (33..=123).contains(&sp) {
            let (c, sp) = (c - 33, sp - 33);
            let gga = t >= 33 && ((t - 33) >> 3) & 3 == 2;
            if gga {
                p.alt_ft = Some(1.002f64.powi(i32::from(c) * 91 + i32::from(sp)));
            } else if c <= 89 {
                p.course = Some(u32::from(c) * 4);
                p.speed_kt = Some(1.08f64.powi(i32::from(sp)) - 1.0);
            }
        }
        let rest = &s[13..];
        if p.alt_ft.is_none() {
            p.alt_ft = altitude(rest);
        }
        Some((p, rest))
    }
}

/// `/A=001234` (feet) anywhere in a comment.
fn altitude(s: &[u8]) -> Option<f64> {
    let i = s.windows(3).position(|w| w == b"/A=")?;
    let v = s.get(i + 3..i + 9)?;
    let (neg, digits) = match v[0] {
        b'-' => (true, &v[1..]),
        _ => (false, v),
    };
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let a = number(digits)?;
    Some(if neg { -a } else { a })
}

/// A 7-character timestamp (`092345z`, `092345/`, `234517h`).
fn timestamp(s: &[u8]) -> Option<&str> {
    let t = s.get(..7)?;
    (t[..6].iter().all(u8::is_ascii_digit) && matches!(t[6], b'z' | b'/' | b'h'))
        .then(|| std::str::from_utf8(t).unwrap_or(""))
}

/// Comment text, trailing spaces and line ends removed.
fn comment(s: &[u8], out: &mut String) {
    let end = s
        .iter()
        .rposition(|c| !matches!(c, b' ' | b'\r' | b'\n' | 0))
        .map_or(0, |i| i + 1);
    if end > 0 {
        let _ = write!(out, " comment=\"{}\"", Escaped(&s[..end]));
    }
}

fn other(info: &[u8], out: &mut String) {
    let _ = write!(out, " type=other info=\"{}\"", Escaped(info));
}

/// Decode an APRS information field. `dst` is the frame's destination
/// (Mic-E keeps the latitude there).
pub fn aprs(dst: &Address, info: &[u8], out: &mut String) {
    let start = out.len();
    if !aprs_known(dst, info, out) {
        out.truncate(start);
        other(info, out);
    }
}

fn aprs_known(dst: &Address, info: &[u8], out: &mut String) -> bool {
    let body = &info[1..];
    match info[0] {
        b'!' | b'=' => {
            let Some((p, rest)) = parse_position(body) else {
                return false;
            };
            out.push_str(" type=position");
            p.write(out);
            comment(rest, out);
        }
        b'/' | b'@' => {
            let Some(ts) = timestamp(body) else {
                return false;
            };
            let Some((p, rest)) = parse_position(&body[7..]) else {
                return false;
            };
            let _ = write!(out, " type=position time={ts}");
            p.write(out);
            comment(rest, out);
        }
        b'>' => {
            out.push_str(" type=status");
            let mut text = body;
            if let Some(ts) = timestamp(body).filter(|t| t.ends_with('z')) {
                let _ = write!(out, " time={ts}");
                text = &body[7..];
            }
            let _ = write!(out, " status=\"{}\"", Escaped(trim(text)));
        }
        b':' => {
            if body.len() < 10 || body[9] != b':' {
                return false;
            }
            let to = trim(&body[..9]);
            let text = &body[10..];
            let _ = write!(out, " type=message to=\"{}\"", Escaped(to));
            if let Some(id) = text
                .strip_prefix(b"ack")
                .or_else(|| text.strip_prefix(b"rej"))
                .filter(|id| id.len() <= 5 && !id.contains(&b' '))
            {
                let _ = write!(
                    out,
                    " {}=\"{}\"",
                    if text[0] == b'a' { "ack" } else { "rej" },
                    Escaped(trim(id))
                );
            } else {
                let (msg, id) = match text.iter().rposition(|&c| c == b'{') {
                    Some(i) if text.len() - i <= 6 => (&text[..i], Some(&text[i + 1..])),
                    _ => (text, None),
                };
                let _ = write!(out, " msg=\"{}\"", Escaped(trim(msg)));
                if let Some(id) = id {
                    let _ = write!(out, " id=\"{}\"", Escaped(trim(id)));
                }
            }
        }
        b';' => {
            if body.len() < 17 || !matches!(body[9], b'*' | b'_') {
                return false;
            }
            let Some(ts) = timestamp(&body[10..]) else {
                return false;
            };
            let Some((p, rest)) = parse_position(&body[17..]) else {
                return false;
            };
            let _ = write!(
                out,
                " type=object name=\"{}\" state={} time={ts}",
                Escaped(trim(&body[..9])),
                if body[9] == b'*' { "live" } else { "killed" }
            );
            p.write(out);
            comment(rest, out);
        }
        b')' => {
            let Some(end) = body
                .iter()
                .take(10)
                .position(|&c| c == b'!' || c == b'_')
                .filter(|&i| i >= 3)
            else {
                return false;
            };
            let Some((p, rest)) = parse_position(&body[end + 1..]) else {
                return false;
            };
            let _ = write!(
                out,
                " type=item name=\"{}\" state={}",
                Escaped(&body[..end]),
                if body[end] == b'!' { "live" } else { "killed" }
            );
            p.write(out);
            comment(rest, out);
        }
        b'`' | b'\'' | 0x1C | 0x1D => {
            let Some(m) = mic_e(dst, body) else {
                return false;
            };
            out.push_str(" type=mic-e");
            m.pos.write(out);
            let _ = write!(out, " status=\"{}\"", m.status);
            comment(m.comment, out);
        }
        _ => return false,
    }
    true
}

fn trim(s: &[u8]) -> &[u8] {
    let end = s
        .iter()
        .rposition(|c| !matches!(c, b' ' | b'\r' | b'\n'))
        .map_or(0, |i| i + 1);
    &s[..end]
}

/// A decoded Mic-E report.
#[derive(Debug, Clone, PartialEq)]
pub struct MicE<'a> {
    pub pos: Position,
    pub status: &'static str,
    pub comment: &'a [u8],
}

/// Decode Mic-E: latitude, message bits and flags from the destination call
/// sign, longitude, speed, course and symbol from the info field (`body` is
/// the info field without its type byte).
pub fn mic_e<'a>(dst: &Address, body: &'a [u8]) -> Option<MicE<'a>> {
    if dst.len != 6 || body.len() < 8 {
        return None;
    }
    let d = &dst.call;
    let mut lat_digits = [0u8; 6];
    // Message bits A, B, C: Some(true) standard 1, Some(false) 0; custom
    // tracked separately.
    let (mut bits, mut custom, mut standard) = (0u8, false, false);
    for (i, &c) in d.iter().enumerate() {
        let (v, one, cust) = match c {
            b'0'..=b'9' => (c - b'0', false, false),
            b'A'..=b'J' => (c - b'A', true, true),
            b'K' => (0, true, true),
            b'L' => (0, false, false),
            b'P'..=b'Y' => (c - b'P', true, false),
            b'Z' => (0, true, false),
            _ => return None,
        };
        lat_digits[i] = v;
        if i < 3 {
            bits = (bits << 1) | u8::from(one);
            if one {
                if cust {
                    custom = true;
                } else {
                    standard = true;
                }
            }
        } else if cust {
            return None; // A–K only appear in the message bits
        }
    }
    let high = |c: u8| c >= b'P';
    let north = high(d[3]);
    let lon_offset = high(d[4]);
    let west = high(d[5]);
    let ld = |i: usize| f64::from(lat_digits[i]);
    let mut lat =
        ld(0) * 10.0 + ld(1) + (ld(2) * 10.0 + ld(3) + (ld(4) * 10.0 + ld(5)) / 100.0) / 60.0;
    if !north {
        lat = -lat;
    }
    let b = |i: usize| i32::from(body[i]) - 28;
    let mut deg = b(0);
    if lon_offset {
        deg += 100;
    }
    if (180..=189).contains(&deg) {
        deg -= 80;
    } else if (190..=199).contains(&deg) {
        deg -= 190;
    }
    let mut min = b(1);
    if min >= 60 {
        min -= 60;
    }
    let hun = b(2);
    if !(0..=179).contains(&deg) || !(0..60).contains(&min) || !(0..100).contains(&hun) {
        return None;
    }
    let mut lon = f64::from(deg) + (f64::from(min) + f64::from(hun) / 100.0) / 60.0;
    if west {
        lon = -lon;
    }
    let (sp, dc, se) = (b(3), b(4), b(5));
    if sp < 0 || dc < 0 || se < 0 {
        return None;
    }
    let mut speed = sp * 10 + dc / 10;
    if speed >= 800 {
        speed -= 800;
    }
    let mut course = (dc % 10) * 100 + se;
    if course >= 400 {
        course -= 400;
    }
    let status = match (custom, standard) {
        (true, true) => "Unknown",
        (true, false) => CUSTOM[usize::from(bits)],
        _ => STANDARD[usize::from(bits)],
    };
    let comment = &body[8..];
    // Altitude: three base-91 digits and `}` (metres above −10 km), right at
    // the start or after a one-character radio type.
    let alt_ft = [0usize, 1].iter().find_map(|&o| {
        let a = comment.get(o..o + 4)?;
        (a[3] == b'}')
            .then(|| base91(&a[..3]))
            .flatten()
            .map(|m| (m - 10_000.0) * 3.28084)
    });
    Some(MicE {
        pos: Position {
            lat,
            lon,
            table: body[7],
            code: body[6],
            course: Some(course as u32),
            speed_kt: Some(f64::from(speed)),
            alt_ft,
        },
        status,
        comment,
    })
}

/// Mic-E message types by bits A B C (index = ABC as a binary number).
const STANDARD: [&str; 8] = [
    "Emergency",
    "Priority",
    "Special",
    "Committed",
    "Returning",
    "In Service",
    "En Route",
    "Off Duty",
];
const CUSTOM: [&str; 8] = [
    "Emergency",
    "Custom-6",
    "Custom-5",
    "Custom-4",
    "Custom-3",
    "Custom-2",
    "Custom-1",
    "Custom-0",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(call: &str, ssid: u8, last: bool) -> [u8; 7] {
        let mut a = [b' ' << 1; 7];
        for (i, c) in call.bytes().enumerate() {
            a[i] = c << 1;
        }
        a[6] = 0x60 | (ssid << 1) | u8::from(last);
        a
    }

    fn info(dst: &str, text: &[u8]) -> String {
        let mut f = Vec::new();
        f.extend(addr(dst, 0, false));
        f.extend(addr("N0CALL", 9, true));
        f.extend([0x03, 0xF0]);
        f.extend_from_slice(text);
        let mut s = String::new();
        assert!(describe(&f, &mut s));
        s
    }

    // Examples from the APRS Protocol Reference 1.0.1.
    #[test]
    fn uncompressed_position() {
        assert_eq!(
            info("APRS", b"!4903.50N/07201.75W-Test 001234"),
            "src=N0CALL-9 dst=APRS type=position lat=49.058333 lon=-72.029167 sym=/- \
             comment=\"Test 001234\""
        );
        let s = info("APRS", b"/092345z4903.50N/07201.75W>088/036/A=001234");
        assert!(
            s.ends_with(
                "type=position time=092345z lat=49.058333 lon=-72.029167 sym=/> course=88 \
                 speed_kt=36.0 alt_ft=1234 comment=\"/A=001234\""
            ),
            "{s}"
        );
        // Ambiguity: spaces count as zeros.
        let s = info("APRS", b"!4903.  N/07201.  W-");
        assert!(s.contains("lat=49.050000 lon=-72.016667"), "{s}");
    }

    #[test]
    fn compressed_position() {
        // The spec rounds: 20427156 / 190463 is 107.249996.
        let s = info("APRS", b"@092345z/5L!!<*e7>7P[");
        assert!(
            s.ends_with(
                "type=position time=092345z lat=49.500000 lon=-72.750004 sym=/> course=88 \
                 speed_kt=36.2"
            ),
            "{s}"
        );
        let s = info("APRS", b"=/5L!!<*e7> sTComment");
        assert!(
            s.ends_with("lat=49.500000 lon=-72.750004 sym=/> comment=\"Comment\""),
            "{s}"
        );
    }

    #[test]
    fn status_message_object_item() {
        assert!(
            info("APRS", b">Net Control Center")
                .ends_with("type=status status=\"Net Control Center\"")
        );
        assert!(
            info("APRS", b">092345zNet Control Center")
                .ends_with("type=status time=092345z status=\"Net Control Center\"")
        );
        assert!(
            info("APRS", b":WU2Z     :Testing{003")
                .ends_with("type=message to=\"WU2Z\" msg=\"Testing\" id=\"003\"")
        );
        assert!(
            info("APRS", b":KB2ICI-14:ack003")
                .ends_with("type=message to=\"KB2ICI-14\" ack=\"003\"")
        );
        let s = info("APRS", b";LEADER   *092345z4903.50N/07201.75W>088/036");
        assert!(
            s.ends_with(
                "type=object name=\"LEADER\" state=live time=092345z lat=49.058333 \
                 lon=-72.029167 sym=/> course=88 speed_kt=36.0"
            ),
            "{s}"
        );
        let s = info("APRS", b")AID #2!4903.50N/07201.75WA");
        assert!(
            s.ends_with("type=item name=\"AID #2\" state=live lat=49.058333 lon=-72.029167 sym=/A"),
            "{s}"
        );
    }

    #[test]
    fn mic_e_destination_example() {
        // APRS101 chapter 10: destination S32U6T is 33°25.64' N, message
        // bits 1 0 0 (standard: Returning), no longitude offset, West.
        // Longitude 72°07.74': d+28 = 'd', m+28 = '#', h+28 = 'f'. Speed
        // 20 kt, course 251°, written with the +800 / +400 offsets that keep
        // the bytes printable: SP = 82 ('n'), DC = 6 ('"'), SE = 51 ('O').
        let s = info("S32U6T", b"`d#fn\"O>/Comment");
        assert_eq!(
            s,
            "src=N0CALL-9 dst=S32U6T type=mic-e lat=33.427333 lon=-72.129000 sym=/> \
             course=251 speed_kt=20.0 status=\"Returning\" comment=\"Comment\""
        );
        // Altitude after a radio type byte: `"4T}` is 1·91² + 19·91 + 51 m
        // above −10 km.
        let m = mic_e(
            &Address::parse(&addr("S32U6T", 0, true)).unwrap(),
            b"d#fn\"O>/]\"4T}",
        )
        .unwrap();
        let metres = 1.0 * 8281.0 + 19.0 * 91.0 + 51.0 - 10_000.0;
        assert!((m.pos.alt_ft.unwrap() - metres * 3.28084).abs() < 1e-6);
    }

    #[test]
    fn non_aprs_and_malformed() {
        let mut f = Vec::new();
        f.extend(addr("DEST", 0, false));
        f.extend(addr("SRC", 1, false));
        f.extend(addr("RELAY", 0, true));
        f[20] |= 0x80; // repeated
        f.extend([0x00, 0xCF]);
        f.extend_from_slice(b"hi\n");
        let mut s = String::new();
        assert!(describe(&f, &mut s));
        assert_eq!(
            s,
            "src=SRC-1 dst=DEST path=RELAY* type=ax25 ctl=I pid=CF info=\"hi\\n\""
        );
        assert!(info("APRS", b"!garbage").ends_with("type=other info=\"!garbage\""));
        let mut s = String::new();
        assert!(!describe(b"\x01\x02\x03", &mut s));
        assert!(s.is_empty());
    }
}
