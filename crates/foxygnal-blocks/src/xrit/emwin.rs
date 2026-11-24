//! EMWIN over HRIT: NWS bulletins (text) and charts.
//!
//! Each EMWIN file is an xRIT text file (file type 2, NOAA product 9), its
//! annotation the EMWIN file name of the NWS "EMWIN GOES-R Filename
//! Convention":
//!
//! `A_WFUS54KBMX260104_C_KWIN_20160126010535_000179-1-TORBMXAL.TXT`
//!
//! — `A_` and the WMO abbreviated heading (TTAAii CCCC YYGGgg, then BBB if
//! any), `_C_KWIN_`, the time it was sent, then sequence number, priority
//! (1 highest … 4) and the AWIPS identifier (category NNN, originator xxx,
//! state qq) with the file type. Big files come zipped (NOAA compression 10);
//! the zip holds one file.
//!
//! Text products become one message each (see [`Product::message`]); every
//! file is saved.

use std::fmt::Write as _;

use crate::digital::text::Escaped;

/// What a product's name says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Product {
    /// The file name inside the zip, or the annotation.
    pub file_name: String,
    /// `WFUS54 KBMX 260104` (and ` BBB` if any).
    pub wmo: Option<String>,
    /// `KBMX`.
    pub office: Option<String>,
    /// `TORBMX` (category and originator).
    pub awips: Option<String>,
    /// Two-letter state code, if any.
    pub state: Option<String>,
    pub priority: Option<u8>,
    pub sequence: Option<u32>,
    /// When it was sent (seconds since 1970).
    pub sent: Option<f64>,
}

impl Product {
    /// Read an EMWIN file name (`A_…_C_KWIN_…`); unknown parts stay `None`.
    pub fn from_name(name: &str) -> Self {
        let mut p = Product {
            file_name: name.to_owned(),
            ..Default::default()
        };
        let parts: Vec<&str> = name.split('_').collect();
        if parts.len() < 6 || !(parts[0] == "A" || parts[0] == "Z") {
            return p;
        }
        let h = parts[1];
        if parts[0] == "A" && h.len() >= 16 && h.is_ascii() {
            let (tt, cccc, time, bbb) = (&h[..6], &h[6..10], &h[10..16], &h[16..]);
            p.wmo = Some(if bbb.is_empty() {
                format!("{tt} {cccc} {time}")
            } else {
                format!("{tt} {cccc} {time} {bbb}")
            });
            p.office = Some(cccc.to_owned());
        }
        p.sent = parse_stamp(parts[4]);
        let tail = parts[5].split('.').next().unwrap_or("");
        let mut f = tail.splitn(3, '-');
        p.sequence = f.next().and_then(|s| s.parse().ok());
        p.priority = f.next().and_then(|s| s.parse().ok());
        if let Some(id) = f.next().filter(|id| id.len() >= 4 && id.is_ascii()) {
            let (awips, state) = if id.len() == 8 {
                (&id[..6], Some(&id[6..]))
            } else {
                (id, None)
            };
            p.awips = Some(awips.trim_end_matches('-').to_owned());
            p.state = state.map(str::to_owned);
        }
        p
    }

    /// Fill in what the name didn't say from the bulletin's own heading: the
    /// first line that looks like `TTAAii CCCC YYGGgg [BBB]`, and the AWIPS
    /// identifier on the line after it.
    pub fn from_text(&mut self, text: &str) {
        let mut lines = text
            .lines()
            .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c.is_control()))
            .filter(|l| !l.is_empty());
        while let Some(l) = lines.next() {
            let f: Vec<&str> = l.split_whitespace().collect();
            let heading = (3..=4).contains(&f.len())
                && f[0].len() == 6
                && f[1].len() == 4
                && f[2].len() == 6
                && f[0].bytes().all(|c| c.is_ascii_alphanumeric())
                && f[1].bytes().all(|c| c.is_ascii_uppercase())
                && f[2].bytes().all(|c| c.is_ascii_digit());
            if !heading {
                continue;
            }
            self.wmo.get_or_insert_with(|| f.join(" "));
            self.office.get_or_insert_with(|| f[1].to_owned());
            if let Some(next) = lines.next()
                && (4..=6).contains(&next.len())
                && next.bytes().all(|c| c.is_ascii_alphanumeric())
            {
                self.awips.get_or_insert_with(|| next.to_owned());
            }
            return;
        }
    }

    /// The product category (`TOR`), the first three letters of the AWIPS id.
    pub fn category(&self) -> Option<&str> {
        self.awips.as_deref().and_then(|a| a.get(..3))
    }

    /// One message line (no newline) for a text product: what it is, then
    /// its text (cut after `max_text` bytes), quoted and escaped.
    pub fn message(&self, path: &str, text: &[u8], max_text: usize) -> String {
        let mut m = String::from("type=emwin");
        if let Some(t) = self.sent {
            let _ = write!(m, " t={t:.0}");
        }
        if let Some(a) = &self.awips {
            let _ = write!(m, " product={a}");
        }
        if let Some(c) = self.category() {
            let _ = write!(m, " category={c}");
        }
        if let Some(e) = self.category().and_then(event) {
            let _ = write!(m, " event=\"{e}\"");
        }
        if let Some(o) = &self.office {
            let _ = write!(m, " office={o}");
        }
        if let Some(s) = &self.state {
            let _ = write!(m, " state={s}");
        }
        if let Some(w) = &self.wmo {
            let _ = write!(m, " wmo=\"{w}\"");
        }
        if let Some(p) = self.priority {
            let _ = write!(m, " priority={p}");
        }
        if let Some(t) = self.sent {
            let (y, mo, d, h, mi, s) = super::products::civil(t);
            let _ = write!(m, " issued={y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        }
        let _ = write!(m, " file={path}");
        // The text, without the transmission framing (SOH, ETX, CRs).
        let body: Vec<u8> = text
            .iter()
            .copied()
            .filter(|&c| c != b'\r' && c != 0x01 && c != 0x03)
            .collect();
        let start = body
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(0);
        let mut end = body.len();
        while end > start && body[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut body = &body[start..end];
        let cut = body.len() > max_text;
        if cut {
            let mut n = max_text;
            while n > 0 && body[n] & 0xC0 == 0x80 {
                n -= 1;
            }
            body = &body[..n];
        }
        let _ = write!(
            m,
            " text=\"{}{}\"",
            Escaped(body),
            if cut { "…" } else { "" }
        );
        m
    }
}

/// `yyyyMMddhhmmss` to seconds since 1970.
fn parse_stamp(s: &str) -> Option<f64> {
    if s.len() != 14 || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = |a: usize, b: usize| s[a..b].parse::<i64>().ok();
    let days = super::products::days_from_civil(n(0, 4)?, n(4, 6)? as u32, n(6, 8)? as u32);
    Some((days * 86_400 + n(8, 10)? * 3600 + n(10, 12)? * 60 + n(12, 14)?) as f64)
}

/// Warnings and other urgent products by category (NWS product categories,
/// EMWIN priority 1).
pub fn event(category: &str) -> Option<&'static str> {
    Some(match category {
        "TOR" => "Tornado Warning",
        "SVR" => "Severe Thunderstorm Warning",
        "SVS" => "Severe Weather Statement",
        "FFW" => "Flash Flood Warning",
        "FFS" => "Flash Flood Statement",
        "FLW" => "Flood Warning",
        "FLS" => "Flood Statement",
        "EWW" => "Extreme Wind Warning",
        "SMW" => "Special Marine Warning",
        "MWS" => "Marine Weather Statement",
        "SQW" => "Snow Squall Warning",
        "DSW" => "Dust Storm Warning",
        "WSW" => "Winter Weather Message",
        "NPW" => "Non-Precipitation Weather Message",
        "RFW" => "Red Flag Warning",
        "CFW" => "Coastal Flood Message",
        "HLS" => "Hurricane Local Statement",
        "TCV" => "Tropical Cyclone Watch/Warning",
        "TCP" => "Tropical Cyclone Public Advisory",
        "TSU" => "Tsunami Message",
        "TOE" => "911 Telephone Outage",
        "CEM" => "Civil Emergency Message",
        "CDW" => "Civil Danger Warning",
        "EQW" => "Earthquake Warning",
        "EVI" => "Evacuation Immediate",
        "LAE" => "Local Area Emergency",
        "SPS" => "Special Weather Statement",
        "WOU" => "Watch Outline Update",
        "SEL" => "Severe Watch",
        _ => return None,
    })
}

const fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32: [u32; 256] = crc32_table();

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        CRC32[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

/// The largest file we unzip.
const MAX_UNZIPPED: usize = 64 << 20;

/// The files in a zip archive: (name, contents). Stored and deflated
/// entries, checked against their CRC; others (and damaged ones) are left
/// out.
pub fn unzip(z: &[u8]) -> Vec<(String, Vec<u8>)> {
    let u16_at = |i: usize| z.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |i: usize| {
        z.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    // The central directory has the sizes even when the local headers
    // don't (bit 3: sizes after the data).
    let Some(eocd) = (0..z.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(i) == Some(0x0605_4B50))
    else {
        return Vec::new();
    };
    let (Some(count), Some(cd)) = (u16_at(eocd + 10), u32_at(eocd + 16)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut at = cd as usize;
    for _ in 0..count {
        if u32_at(at) != Some(0x0201_4B50) {
            break;
        }
        let (
            Some(method),
            Some(crc),
            Some(csize),
            Some(usize_),
            Some(nlen),
            Some(xlen),
            Some(clen),
            Some(local),
        ) = (
            u16_at(at + 10),
            u32_at(at + 16),
            u32_at(at + 20),
            u32_at(at + 24),
            u16_at(at + 28),
            u16_at(at + 30),
            u16_at(at + 32),
            u32_at(at + 42),
        )
        else {
            break;
        };
        let name = z
            .get(at + 46..at + 46 + usize::from(nlen))
            .map(|n| String::from_utf8_lossy(n).into_owned());
        at += 46 + usize::from(nlen) + usize::from(xlen) + usize::from(clen);
        let local = local as usize;
        let (Some(name), Some(lnlen), Some(lxlen)) = (name, u16_at(local + 26), u16_at(local + 28))
        else {
            continue;
        };
        if u32_at(local) != Some(0x0403_4B50) || usize_ as usize > MAX_UNZIPPED {
            continue;
        }
        let start = local + 30 + usize::from(lnlen) + usize::from(lxlen);
        let Some(data) = z.get(start..start + csize as usize) else {
            continue;
        };
        let contents = match method {
            0 => data.to_vec(),
            8 => match miniz_oxide::inflate::decompress_to_vec_with_limit(data, MAX_UNZIPPED) {
                Ok(v) => v,
                Err(_) => continue,
            },
            _ => continue,
        };
        if crc32(&contents) == crc && contents.len() == usize_ as usize {
            out.push((name, contents));
        }
    }
    out
}

/// A zip archive of one file, deflated (for test data and the transmit
/// side).
pub fn zip_one(name: &str, data: &[u8]) -> Vec<u8> {
    let comp = miniz_oxide::deflate::compress_to_vec(data, 6);
    let crc = crc32(data);
    let mut z = Vec::new();
    let header = |sig: u32, central: bool, z: &mut Vec<u8>| {
        z.extend_from_slice(&sig.to_le_bytes());
        if central {
            z.extend_from_slice(&20u16.to_le_bytes());
        }
        z.extend_from_slice(&20u16.to_le_bytes()); // version needed
        z.extend_from_slice(&0u16.to_le_bytes()); // flags
        z.extend_from_slice(&8u16.to_le_bytes()); // deflate
        z.extend_from_slice(&0u32.to_le_bytes()); // time, date
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes()); // extra
        if central {
            z.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
            z.extend_from_slice(&0u32.to_le_bytes()); // external attributes
            z.extend_from_slice(&0u32.to_le_bytes()); // local header at 0
        }
        z.extend_from_slice(name.as_bytes());
    };
    header(0x0403_4B50, false, &mut z);
    z.extend_from_slice(&comp);
    let cd = z.len();
    header(0x0201_4B50, true, &mut z);
    let cd_len = z.len() - cd;
    z.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
    z.extend_from_slice(&[0; 4]);
    z.extend_from_slice(&1u16.to_le_bytes());
    z.extend_from_slice(&1u16.to_le_bytes());
    z.extend_from_slice(&(cd_len as u32).to_le_bytes());
    z.extend_from_slice(&(cd as u32).to_le_bytes());
    z.extend_from_slice(&0u16.to_le_bytes());
    z
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "A_WFUS54KBMX260104_C_KWIN_20160126010535_000179-1-TORBMXAL.TXT";

    #[test]
    fn file_names() {
        let p = Product::from_name(NAME);
        assert_eq!(p.wmo.as_deref(), Some("WFUS54 KBMX 260104"));
        assert_eq!(p.office.as_deref(), Some("KBMX"));
        assert_eq!(p.awips.as_deref(), Some("TORBMX"));
        assert_eq!(p.state.as_deref(), Some("AL"));
        assert_eq!(p.category(), Some("TOR"));
        assert_eq!((p.priority, p.sequence), (Some(1), Some(179)));
        assert_eq!(p.sent, Some(1_453_770_335.0));
        let p =
            Product::from_name("A_FXUS61KOKX231800AAA_C_KWIN_20261123180001_123456-3-AFDOKX.TXT");
        assert_eq!(p.wmo.as_deref(), Some("FXUS61 KOKX 231800 AAA"));
        assert_eq!(
            (p.awips.as_deref(), p.state.as_deref()),
            (Some("AFDOKX"), None)
        );
    }

    #[test]
    fn headings_and_messages() {
        let text = "\x01\r\r\n000 \r\r\nWFUS54 KBMX 260104\r\r\nTORBMX\r\r\n\r\r\nBULLETIN - \"EAS\" ACTIVATION REQUESTED\r\r\nTornado Warning\r\r\n\x03";
        let mut p = Product::from_name("SOMETHING.TXT");
        p.from_text(text);
        assert_eq!(p.wmo.as_deref(), Some("WFUS54 KBMX 260104"));
        assert_eq!(p.awips.as_deref(), Some("TORBMX"));
        let m = Product::from_name(NAME).message("emwin/x.TXT", text.as_bytes(), 4000);
        assert!(m.starts_with("type=emwin t=1453770335 product=TORBMX category=TOR event=\"Tornado Warning\" office=KBMX state=AL wmo=\"WFUS54 KBMX 260104\" priority=1 issued=2016-01-26T01:05:35Z file=emwin/x.TXT text=\"000 \\n"), "{m}");
        assert!(m.contains("\\\"EAS\\\""));
        assert!(!m.contains('\n'));
        let m = Product::from_name(NAME).message("x", text.as_bytes(), 10);
        assert!(m.ends_with("text=\"000 \\nWFUS5…\""), "{m}");
    }

    #[test]
    fn zips_round_trip() {
        let data = "THE QUICK BROWN FOX ".repeat(500);
        let z = zip_one("TORBMXAL.TXT", data.as_bytes());
        assert!(z.len() < data.len() / 4);
        let files = unzip(&z);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "TORBMXAL.TXT");
        assert_eq!(files[0].1, data.as_bytes());
        let mut bad = z.clone();
        bad[50] ^= 1; // in the deflated data
        assert!(unzip(&bad).is_empty());
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
