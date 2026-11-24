//! What a file is: the satellite, region, channel and time of an image, and
//! names for pictures and saved files.
//!
//! GOES-R puts most of it in the ancillary text (`Satellite`, `Region`,
//! `Channel`, `Time of frame start`) and the file name
//! (`OR_ABI-L2-CMIPF-M6C13_G18_s20263271200207_…`); the NOAA record gives
//! the product (16–19: that GOES's ABI, 13/15: the GOES-N imagers relayed,
//! 43: Himawari relayed, 6: NWS, 9: EMWIN). GK-2A and Elektro-L say it in
//! the file name (`IMG_FD_…_IR105_…`, `H-000-GOMS3-…`).

use super::header::{Headers, file_type};
use super::transport::File;
use crate::image::civil_from_days;

/// Where a file belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// A picture (or a segment of one).
    Image,
    /// An EMWIN file: an NWS text product, or a chart / zip.
    Emwin,
    /// Plain text (administrative messages).
    Text,
    /// DCS (data collection platforms) messages.
    Dcs,
    /// Anything else (saved as it is).
    Other,
}

/// What an image is.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    /// `G18`, `GK2A`, `ELEKTRO-L3`, `HIMAWARI`, …
    pub satellite: String,
    /// `FD`, `CONUS`, `M1`, `M2`, `NH`, `SH`, `SI`, … (`IMG` if not known).
    pub region: String,
    /// `CH13`, `IR105`, `VIS`, …
    pub channel: String,
    /// Seconds since 1970 (UTC), if the file says.
    pub time: Option<f64>,
}

impl ImageInfo {
    /// The picture's name, as folders and a file name:
    /// `G18/FD/CH13/2026-11-23/G18_FD_CH13_20261123T120020Z`.
    pub fn name(&self) -> String {
        let (date, stamp) = match self.time {
            Some(t) => {
                let (y, mo, d, h, mi, s) = civil(t);
                (
                    format!("{y:04}-{mo:02}-{d:02}"),
                    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z"),
                )
            }
            None => ("undated".into(), "undated".into()),
        };
        let (sat, region, ch) = (
            clean(&self.satellite),
            clean(&self.region),
            clean(&self.channel),
        );
        format!("{sat}/{region}/{ch}/{date}/{sat}_{region}_{ch}_{stamp}")
    }

    /// The same picture's other channels share this.
    pub fn scene(&self) -> (String, String, Option<i64>) {
        (
            self.satellite.clone(),
            self.region.clone(),
            self.time.map(|t| t.round() as i64),
        )
    }
}

/// Keep letters, digits, `.`, `_` and `-` (others become `-`), so a string
/// is safe as one path part. Never empty, `.` or `..`.
pub fn clean(s: &str) -> String {
    let mut out: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    out.truncate(80);
    if out.is_empty() || out.chars().all(|c| c == '.') {
        out = "x".into();
    }
    out
}

/// (year, month, day, hour, minute, second) of a Unix time.
pub fn civil(t: f64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t.floor() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    (
        y,
        m,
        d,
        (s / 3600) as u32,
        (s / 60 % 60) as u32,
        (s % 60) as u32,
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-11-23T12:00:20.7Z` (or with a space, or without the `Z`).
pub fn parse_iso(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once(['T', ' '])?;
    let mut d = date.split('-').map(str::parse::<i64>);
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':');
    let h: f64 = t.next()?.parse().ok()?;
    let mi: f64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let days = days_from_civil(y, mo as u32, da as u32);
    Some(days as f64 * 86_400.0 + h * 3600.0 + mi * 60.0 + sec)
}

/// GOES-R file names carry `_sYYYYDDDHHMMSSs` (year, day of year, time,
/// tenths).
fn goesr_start(name: &str) -> Option<f64> {
    let s = name.split('_').find_map(|p| p.strip_prefix('s'))?;
    if s.len() < 13 || !s.bytes().take(13).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = |a: usize, b: usize| s[a..b].parse::<i64>().ok();
    let (y, doy, h, mi, sec) = (n(0, 4)?, n(4, 7)?, n(7, 9)?, n(9, 11)?, n(11, 13)?);
    let tenths = if s.len() > 13 {
        n(13, 14).unwrap_or(0)
    } else {
        0
    };
    let days = days_from_civil(y, 1, 1) + doy - 1;
    Some((days * 86_400 + h * 3600 + mi * 60 + sec) as f64 + tenths as f64 / 10.0)
}

/// `YYYYMMDD` and `HHMMSS` (or `HHMM`) parts of a file name.
fn date_time_parts(parts: &[&str]) -> Option<f64> {
    let i = parts
        .iter()
        .position(|p| p.len() == 8 && p.bytes().all(|c| c.is_ascii_digit()))?;
    let d = parts[i];
    let t = parts
        .get(i + 1)
        .filter(|t| (t.len() == 6 || t.len() == 4) && t.bytes().all(|c| c.is_ascii_digit()));
    let n = |s: &str, a: usize, b: usize| s[a..b].parse::<i64>().ok();
    let days = days_from_civil(n(d, 0, 4)?, n(d, 4, 6)? as u32, n(d, 6, 8)? as u32);
    let secs = match t {
        Some(t) if t.len() == 6 => n(t, 0, 2)? * 3600 + n(t, 2, 4)? * 60 + n(t, 4, 6)?,
        Some(t) => n(t, 0, 2)? * 3600 + n(t, 2, 4)? * 60,
        None => 0,
    };
    Some((days * 86_400 + secs) as f64)
}

/// The file name without its extension.
pub fn stem(h: &Headers) -> Option<&str> {
    let a = h.annotation.as_deref()?.trim();
    let a = a.rsplit('/').next().unwrap_or(a);
    Some(
        a.strip_suffix(".lrit")
            .or(a.strip_suffix(".LRIT"))
            .unwrap_or(a),
    )
}

pub fn classify(f: &File) -> Class {
    let h = &f.headers;
    let product = h.noaa.map(|n| n.product);
    let name = h.annotation.as_deref().unwrap_or("");
    match h.file_type {
        file_type::DCS => Class::Dcs,
        file_type::IMAGE if h.image.is_some() => {
            // NWS charts come as GIF files with an image structure record.
            match h.noaa.map(|n| n.compression) {
                Some(super::header::noaa_compression::GIF)
                | Some(super::header::noaa_compression::ZIP) => Class::Other,
                _ => Class::Image,
            }
        }
        file_type::TEXT | file_type::GTS_MESSAGE
            if product == Some(9) || name.starts_with("A_") || name.starts_with("Z_") =>
        {
            Class::Emwin
        }
        file_type::TEXT | file_type::GTS_MESSAGE => Class::Text,
        _ => Class::Other,
    }
}

/// The satellite a file comes from.
pub fn satellite(f: &File) -> String {
    let h = &f.headers;
    if let Some(s) = h.ancillary("Satellite")
        && !s.is_empty()
    {
        return s.to_owned();
    }
    let name = stem(h).unwrap_or("");
    if let Some(n) = h.noaa {
        match n.product {
            13..=19 => return format!("G{}", n.product),
            43 => return "HIMAWARI".into(),
            _ => {}
        }
    }
    // GOES-R file names: …_G18_s….
    if let Some(g) = name
        .split('_')
        .find(|p| p.len() == 3 && p.starts_with('G') && p[1..].bytes().all(|c| c.is_ascii_digit()))
    {
        return g.to_owned();
    }
    if let Some(rest) = name.split('-').find(|p| p.starts_with("GOMS")) {
        return format!("ELEKTRO-L{}", &rest[4..]);
    }
    if name.starts_with("IMG_") || name.starts_with("ADD_") || name.contains("GK2A") {
        return "GK2A".into();
    }
    format!("SC{}", f.spacecraft)
}

/// What an image file is.
pub fn image_info(f: &File) -> ImageInfo {
    let h = &f.headers;
    let name = stem(h).unwrap_or("");
    let satellite = satellite(f);
    let noaa = h.noaa;
    let mut region = h.ancillary("Region").map(|r| match r {
        "Full Disk" => "FD".to_owned(),
        "Mesoscale" => {
            if name.contains("M2C") || name.contains("CMIPM2") {
                "M2".into()
            } else {
                "M1".into()
            }
        }
        r if r.eq_ignore_ascii_case("conus") || r.contains("CONUS") => "CONUS".into(),
        r => r.to_owned(),
    });
    let mut channel = h
        .ancillary("Channel")
        .and_then(|c| c.parse::<u32>().ok())
        .map(|c| format!("CH{c:02}"));
    let mut time = h.ancillary("Time of frame start").and_then(parse_iso);

    // GOES-R names: OR_ABI-L2-CMIPF-M6C13_G18_s….
    if name.starts_with("OR_") {
        let parts: Vec<&str> = name.split('_').collect();
        if let Some(p) = parts.get(1) {
            let mut seg = p.split('-');
            let product = seg.nth(2).unwrap_or("");
            if region.is_none() {
                region = Some(
                    if product.ends_with("M1") {
                        "M1"
                    } else if product.ends_with("M2") {
                        "M2"
                    } else if product.ends_with('C') {
                        "CONUS"
                    } else {
                        "FD"
                    }
                    .into(),
                );
            }
            if channel.is_none()
                && let Some(mode) = seg.next()
                && let Some(c) = mode.split('C').nth(1)
                && let Ok(c) = c.parse::<u32>()
            {
                channel = Some(format!("CH{c:02}"));
            }
            if channel.is_none() && !product.is_empty() {
                channel = Some(product.trim_end_matches(['F', 'C', '1', '2', 'M']).into());
            }
        }
        time = time.or_else(|| goesr_start(name));
    }
    if let Some(n) = noaa {
        match n.product {
            13 | 15 if channel.is_none() => {
                channel = Some(
                    match n.sub_product {
                        0..=10 => "CH4",
                        11..=20 => "CH1",
                        _ => "CH3",
                    }
                    .into(),
                );
                region.get_or_insert_with(|| {
                    match n.sub_product % 10 {
                        1 => "FD",
                        2 => "NH",
                        3 => "SH",
                        4 => "US",
                        _ => "SI",
                    }
                    .into()
                });
            }
            43 if channel.is_none() => {
                channel = Some(
                    match n.sub_product {
                        1 => "VIS",
                        3 => "IR",
                        7 => "WV",
                        _ => "X",
                    }
                    .into(),
                );
                region.get_or_insert_with(|| "FD".into());
            }
            _ => {}
        }
    }
    // GK-2A: IMG_FD_047_IR105_20190907_181006_01; Elektro: H-000-GOMS3-…
    if (channel.is_none() || region.is_none()) && name.starts_with("IMG_") {
        let parts: Vec<&str> = name.split('_').collect();
        region.get_or_insert_with(|| parts.get(1).copied().unwrap_or("IMG").into());
        channel.get_or_insert_with(|| parts.get(3).copied().unwrap_or("X").into());
        time = time.or_else(|| date_time_parts(&parts));
    }
    if (channel.is_none() || region.is_none()) && name.contains("GOMS") {
        let parts: Vec<&str> = name.split('-').filter(|p| !p.is_empty()).collect();
        channel.get_or_insert_with(|| parts.get(3).map_or("X", |p| p.trim_matches('_')).into());
        region.get_or_insert_with(|| "FD".into());
        if time.is_none()
            && let Some(t) = parts
                .iter()
                .find(|p| p.len() == 12 && p.bytes().all(|c| c.is_ascii_digit()))
        {
            time = date_time_parts(&[&t[..8], &t[8..]]);
        }
    }
    if time.is_none() {
        time = h.time.map(|c| c.unix());
    }
    ImageInfo {
        satellite,
        region: region.unwrap_or_else(|| "IMG".into()),
        channel: channel
            .unwrap_or_else(|| name.split(['_', '-']).next_back().unwrap_or("X").into()),
        time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xrit::header::{ImageStructure, Noaa};

    fn file(h: Headers) -> File {
        File {
            spacecraft: 0,
            vcid: 13,
            apid: 1,
            headers: h,
            header_bytes: Vec::new(),
            data: Vec::new(),
            complete: true,
            rice_decoded: false,
            lines_lost: 0,
        }
    }

    fn image(annotation: &str, ancillary: Option<&str>, noaa: Option<(u16, u16)>) -> File {
        file(Headers {
            file_type: file_type::IMAGE,
            image: Some(ImageStructure {
                bits_per_pixel: 8,
                columns: 10,
                lines: 10,
                compression: 0,
            }),
            annotation: Some(annotation.into()),
            ancillary: ancillary.map(Into::into),
            noaa: noaa.map(|(p, s)| Noaa {
                agency: *b"NOAA",
                product: p,
                sub_product: s,
                parameter: 0,
                compression: 0,
            }),
            ..Default::default()
        })
    }

    #[test]
    fn dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil(1_795_435_220.7), (2026, 11, 23, 12, 0, 20));
        assert_eq!(parse_iso("2026-11-23T12:00:20.7Z"), Some(1_795_435_220.7));
        assert_eq!(
            goesr_start("OR_ABI-L2-CMIPF-M6C13_G18_s20263271200207_e2026"),
            Some(1_795_435_220.7)
        );
    }

    #[test]
    fn goes_r_cmip() {
        let f = image(
            "OR_ABI-L2-CMIPF-M6C13_G18_s20263271200207_e20263271209527_c20263271209583.lrit",
            Some(
                "Time of frame start = 2026-11-23T12:00:20.7Z;Satellite = G18;Instrument = ABI;Channel = 13;Imaging Mode = ABI Mode 6;Region = Full Disk;Resolution = 2km at nadir;Segmented = yes",
            ),
            Some((18, 0)),
        );
        assert_eq!(classify(&f), Class::Image);
        let i = image_info(&f);
        assert_eq!(
            (i.satellite.as_str(), i.region.as_str(), i.channel.as_str()),
            ("G18", "FD", "CH13")
        );
        assert_eq!(
            i.name(),
            "G18/FD/CH13/2026-11-23/G18_FD_CH13_20261123T120020Z"
        );

        // Without ancillary text: the file name says it all.
        let f = image(
            "OR_ABI-L2-CMIPM2-M6C02_G19_s20263271200207_e2026.lrit",
            None,
            Some((19, 0)),
        );
        let i = image_info(&f);
        assert_eq!(
            (i.satellite.as_str(), i.region.as_str(), i.channel.as_str()),
            ("G19", "M2", "CH02")
        );
        assert_eq!(i.time, Some(1_795_435_220.7));
    }

    #[test]
    fn relays_and_others() {
        let i = image_info(&image("IMG_HIMAWARI", None, Some((43, 3))));
        assert_eq!(
            (i.satellite.as_str(), i.region.as_str(), i.channel.as_str()),
            ("HIMAWARI", "FD", "IR")
        );
        let i = image_info(&image(
            "IMG_FD_047_IR105_20190907_181006_01.lrit",
            None,
            None,
        ));
        assert_eq!(
            (i.satellite.as_str(), i.region.as_str(), i.channel.as_str()),
            ("GK2A", "FD", "IR105")
        );
        assert_eq!(i.time, Some(1_567_879_806.0));
        let i = image_info(&image(
            "H-000-GOMS3_-GOMS3_4_____-00_9_0700___-201911251130-C_",
            None,
            None,
        ));
        assert_eq!(i.satellite, "ELEKTRO-L3_");
        assert_eq!(clean("../a b"), "..-a-b");
        assert_eq!(clean(".."), "x");
    }
}
