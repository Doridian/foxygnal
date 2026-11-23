//! Two-line element sets: parsing (with checksums), from a file or pasted
//! text, with or without a name line.

use std::fmt;

/// One element set.
#[derive(Debug, Clone, PartialEq)]
pub struct Tle {
    /// The name line (title), trimmed, without a leading `0 `; empty if the
    /// set had none.
    pub name: String,
    /// NORAD catalog number (Alpha-5 numbers like `A0001` are decoded).
    pub norad: u32,
    pub classification: char,
    /// International designator (`98067A`), trimmed.
    pub intl: String,
    /// Four-digit year of the epoch.
    pub epoch_year: i32,
    /// Day of the year of the epoch, 1-based with fraction.
    pub epoch_day: f64,
    /// First derivative of the mean motion / 2 (rev/day²); unused by SGP4.
    pub ndot: f64,
    /// Second derivative / 6 (rev/day³); unused by SGP4.
    pub nddot: f64,
    /// Drag term (1/earth radii).
    pub bstar: f64,
    /// Degrees.
    pub inclination: f64,
    /// Right ascension of the ascending node, degrees.
    pub raan: f64,
    pub eccentricity: f64,
    /// Degrees.
    pub arg_perigee: f64,
    /// Degrees.
    pub mean_anomaly: f64,
    /// Revolutions per day.
    pub mean_motion: f64,
    pub rev_number: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TleError(pub String);

impl fmt::Display for TleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TleError {}

fn err(msg: impl Into<String>) -> TleError {
    TleError(msg.into())
}

/// The checksum digit of a TLE line: the digits of its first 68 columns
/// summed, `-` counting 1, modulo 10.
pub fn checksum(line: &str) -> u8 {
    let sum: u32 = line
        .bytes()
        .take(68)
        .map(|b| match b {
            b'0'..=b'9' => u32::from(b - b'0'),
            b'-' => 1,
            _ => 0,
        })
        .sum();
    (sum % 10) as u8
}

fn check_line(line: &str, n: u8) -> Result<(), TleError> {
    let Some(c) = line.as_bytes().get(68) else {
        return Err(err(format!("line {n} is shorter than 69 characters")));
    };
    let want = checksum(line);
    if *c != b'0' + want {
        return Err(err(format!(
            "line {n} checksum is {}, the line sums to {want}",
            *c as char
        )));
    }
    Ok(())
}

fn field<'a>(
    line: &'a str,
    n: u8,
    range: std::ops::Range<usize>,
    what: &str,
) -> Result<&'a str, TleError> {
    line.get(range)
        .map(str::trim)
        .ok_or_else(|| err(format!("line {n}: no {what}")))
}

fn num<T: std::str::FromStr>(s: &str, n: u8, what: &str) -> Result<T, TleError> {
    let s = s.trim();
    // `+.123` and `-.123` don't parse as f64 everywhere: add the zero.
    let fixed;
    let s = if let Some(r) = s.strip_prefix('.') {
        fixed = format!("0.{r}");
        fixed.as_str()
    } else if let Some(r) = s.strip_prefix("-.") {
        fixed = format!("-0.{r}");
        fixed.as_str()
    } else if let Some(r) = s.strip_prefix("+.") {
        fixed = format!("0.{r}");
        fixed.as_str()
    } else {
        s
    };
    s.parse()
        .map_err(|_| err(format!("line {n}: bad {what} `{s}`")))
}

/// An implied-decimal number with exponent: ` 12345-3` = 0.12345e-3.
fn implied(s: &str, n: u8, what: &str) -> Result<f64, TleError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0.0);
    }
    let (neg, rest) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let rest = rest.trim_start();
    let (mant, exp) = match rest.rfind(['-', '+']) {
        Some(p) if p > 0 => (&rest[..p], &rest[p..]),
        _ => (rest, "0"),
    };
    let mant = mant.trim();
    if mant.is_empty() || !mant.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err(format!("line {n}: bad {what} `{s}`")));
    }
    let m: f64 = format!("0.{mant}")
        .parse()
        .map_err(|_| err(format!("line {n}: bad {what} `{s}`")))?;
    let e: i32 = exp
        .trim()
        .trim_start_matches('+')
        .parse()
        .map_err(|_| err(format!("line {n}: bad {what} `{s}`")))?;
    let v = m * 10f64.powi(e);
    Ok(if neg { -v } else { v })
}

/// A catalog number, five digits or Alpha-5 (a letter for the first digit,
/// I and O skipped: A = 10 … Z = 33).
fn catalog(s: &str, n: u8) -> Result<u32, TleError> {
    let s = s.trim();
    let bad = || err(format!("line {n}: bad catalog number `{s}`"));
    let mut chars = s.chars();
    let first = chars.next().ok_or_else(bad)?;
    if first.is_ascii_uppercase() {
        let v = match first {
            'A'..='H' => first as u32 - 'A' as u32 + 10,
            'J'..='N' => first as u32 - 'J' as u32 + 18,
            'P'..='Z' => first as u32 - 'P' as u32 + 23,
            _ => return Err(bad()),
        };
        let rest: u32 = chars.as_str().parse().map_err(|_| bad())?;
        Ok(v * 10_000 + rest)
    } else {
        s.parse().map_err(|_| bad())
    }
}

impl Tle {
    /// Parse a set from its two lines (and optional name). With `strict`,
    /// both checksums must match.
    pub fn from_lines(
        name: &str,
        line1: &str,
        line2: &str,
        strict: bool,
    ) -> Result<Self, TleError> {
        let line1 = line1.trim_end();
        let line2 = line2.trim_end();
        if !line1.starts_with("1 ") {
            return Err(err("line 1 must start with `1 `"));
        }
        if !line2.starts_with("2 ") {
            return Err(err("line 2 must start with `2 `"));
        }
        if !line1.is_ascii() || !line2.is_ascii() {
            return Err(err("TLE lines must be ASCII"));
        }
        if line1.len() < 64 || line2.len() < 63 {
            return Err(err("TLE lines are 69 characters long"));
        }
        if strict {
            check_line(line1, 1)?;
            check_line(line2, 2)?;
        }
        let norad = catalog(field(line1, 1, 2..7, "catalog number")?, 1)?;
        let norad2 = catalog(field(line2, 2, 2..7, "catalog number")?, 2)?;
        if norad != norad2 {
            return Err(err(format!(
                "the lines are of different satellites ({norad} and {norad2})"
            )));
        }
        let yy: i32 = num(field(line1, 1, 18..20, "epoch year")?, 1, "epoch year")?;
        let name = name.trim();
        let name = name.strip_prefix("0 ").unwrap_or(name).trim();
        Ok(Tle {
            name: name.to_owned(),
            norad,
            classification: line1.as_bytes()[7] as char,
            intl: field(line1, 1, 9..17, "designator")?.to_owned(),
            epoch_year: if yy < 57 { 2000 + yy } else { 1900 + yy },
            epoch_day: num(field(line1, 1, 20..32, "epoch day")?, 1, "epoch day")?,
            ndot: num(field(line1, 1, 33..43, "ndot")?, 1, "ndot")?,
            nddot: implied(field(line1, 1, 44..52, "nddot")?, 1, "nddot")?,
            bstar: implied(field(line1, 1, 53..61, "B*")?, 1, "B*")?,
            inclination: num(field(line2, 2, 8..16, "inclination")?, 2, "inclination")?,
            raan: num(field(line2, 2, 17..25, "RAAN")?, 2, "RAAN")?,
            eccentricity: num::<f64>(
                &format!("0.{}", field(line2, 2, 26..33, "eccentricity")?),
                2,
                "eccentricity",
            )?,
            arg_perigee: num(
                field(line2, 2, 34..42, "argument of perigee")?,
                2,
                "argument of perigee",
            )?,
            mean_anomaly: num(field(line2, 2, 43..51, "mean anomaly")?, 2, "mean anomaly")?,
            mean_motion: num(field(line2, 2, 52..63, "mean motion")?, 2, "mean motion")?,
            rev_number: line2
                .get(63..68)
                .map(str::trim)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        })
    }

    /// The name, or `NORAD n` if the set had none.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            format!("NORAD {}", self.norad)
        } else {
            self.name.clone()
        }
    }

    /// Epoch in days since 1949 December 31 00:00 UT (SGP4's time base),
    /// computed the way Vallado's `twoline2rv` does (via month, day, hour,
    /// minute, second) so results match the reference output.
    pub fn epoch_1950(&self) -> f64 {
        let (mon, day, hr, minute, sec) = days2mdhms(self.epoch_year, self.epoch_day);
        let (jd, frac) = jday(self.epoch_year, mon, day, hr, minute, sec);
        (jd - 2_433_281.5) + frac
    }

    /// Epoch as a Unix time (seconds).
    pub fn epoch_unix(&self) -> f64 {
        (self.epoch_1950() - UNIX_EPOCH_1950) * 86_400.0
    }

    /// Does `query` pick this set: its catalog number, or its name ignoring
    /// case (whole name first; see [`find`]).
    fn matches_number(&self, query: &str) -> bool {
        query.trim().parse::<u32>().is_ok_and(|n| n == self.norad)
    }
}

/// The Unix epoch in days since 1949 December 31 00:00 UT.
pub const UNIX_EPOCH_1950: f64 = 2_440_587.5 - 2_433_281.5;

/// Month, day, hour, minute, second of a day of the year (Vallado's
/// `days2mdhms`).
fn days2mdhms(year: i32, days: f64) -> (i32, i32, i32, i32, f64) {
    let mut lmonth = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if year % 4 == 0 {
        lmonth[1] = 29;
    }
    let dayofyr = days.floor() as i32;
    let mut i = 1;
    let mut inttemp = 0;
    while dayofyr > inttemp + lmonth[i - 1] && i < 12 {
        inttemp += lmonth[i - 1];
        i += 1;
    }
    let mon = i as i32;
    let day = dayofyr - inttemp;
    let mut temp = (days - f64::from(dayofyr)) * 24.0;
    let hr = temp.floor() as i32;
    temp = (temp - f64::from(hr)) * 60.0;
    let minute = temp.floor() as i32;
    let sec = (temp - f64::from(minute)) * 60.0;
    (mon, day, hr, minute, sec)
}

/// Julian date of a calendar date, split into the date at 0h and the day
/// fraction (Vallado's `jday`).
pub fn jday(year: i32, mon: i32, day: i32, hr: i32, minute: i32, sec: f64) -> (f64, f64) {
    let (y, m) = (f64::from(year), f64::from(mon));
    let mut jd = 367.0 * y - (7.0 * (y + ((m + 9.0) / 12.0).floor()) * 0.25).floor()
        + (275.0 * m / 9.0).floor()
        + f64::from(day)
        + 1_721_013.5;
    let mut frac = (sec + f64::from(minute) * 60.0 + f64::from(hr) * 3600.0) / 86_400.0;
    if frac.abs() > 1.0 {
        let d = frac.floor();
        jd += d;
        frac -= d;
    }
    (jd, frac)
}

/// Every element set in `text`: a file's contents or pasted text, two- or
/// three-line sets, blank lines and `#` comments ignored. Pasted text whose
/// line breaks were turned into spaces (a one-line text field) works too.
///
/// Sets that fail to parse (bad checksum, bad field) are skipped and
/// reported in the second list, as `name: reason`.
pub fn parse_all(text: &str) -> (Vec<Tle>, Vec<String>) {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect();
    if lines.len() == 1 || lines.iter().any(|l| l.len() > 80) {
        return parse_flat(&text.replace(['\r', '\n', '\t'], " "));
    }
    let (mut sets, mut errors) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i].trim_start();
        let (name, l1, l2, next) = if l.starts_with("1 ")
            && lines
                .get(i + 1)
                .is_some_and(|n| n.trim_start().starts_with("2 "))
        {
            ("", l, lines[i + 1].trim_start(), i + 2)
        } else if lines
            .get(i + 1)
            .is_some_and(|n| n.trim_start().starts_with("1 "))
            && lines
                .get(i + 2)
                .is_some_and(|n| n.trim_start().starts_with("2 "))
        {
            (
                l,
                lines[i + 1].trim_start(),
                lines[i + 2].trim_start(),
                i + 3,
            )
        } else {
            errors.push(format!(
                "`{}`: not part of a TLE",
                l.chars().take(40).collect::<String>()
            ));
            i += 1;
            continue;
        };
        match Tle::from_lines(name, l1, l2, true) {
            Ok(t) => sets.push(t),
            Err(e) => errors.push(format!(
                "{}: {e}",
                if name.is_empty() {
                    l1.get(2..7).unwrap_or("?").trim()
                } else {
                    name.trim()
                }
            )),
        }
        i = next;
    }
    (sets, errors)
}

/// TLEs whose line breaks became spaces or vanished: find each `1 NNNNN…` line
/// (69 characters with a good checksum) followed by its `2 NNNNN…` line; the
/// text before is the name.
fn parse_flat(text: &str) -> (Vec<Tle>, Vec<String>) {
    let b = text.as_bytes();
    let (mut sets, mut errors) = (Vec::new(), Vec::new());
    let mut name_start = 0;
    let mut i = 0;
    while i + 69 <= b.len() {
        if b[i] == b'1' && b[i + 1] == b' ' {
            let Some(l1) = text.get(i..i + 69) else {
                i += 1;
                continue;
            };
            // The line break became a space, or vanished (Chrome drops line
            // breaks pasted into a one-line field).
            let j = if b.get(i + 69) == Some(&b' ') {
                i + 70
            } else {
                i + 69
            };
            if j + 69 <= b.len() && b[j] == b'2' && b[j + 1] == b' ' {
                let Some(l2) = text.get(j..j + 69) else {
                    i += 1;
                    continue;
                };
                if checksum(l1) == b[i + 68].wrapping_sub(b'0') && l1[2..7] == l2[2..7] {
                    let name = &text[name_start..i];
                    match Tle::from_lines(name, l1, l2, true) {
                        Ok(t) => sets.push(t),
                        Err(e) => errors.push(format!("{}: {e}", name.trim())),
                    }
                    i = j + 69;
                    name_start = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    if sets.is_empty() && errors.is_empty() && !text.trim().is_empty() {
        errors.push("no two-line element set found".into());
    }
    (sets, errors)
}

/// Pick a set by catalog number or name (case-insensitive; an exact name
/// beats a name that merely starts with `query`). An empty query picks the
/// first.
pub fn find<'a>(sets: &'a [Tle], query: &str) -> Option<&'a Tle> {
    let q = query.trim();
    if q.is_empty() {
        return sets.first();
    }
    if let Some(t) = sets.iter().find(|t| t.matches_number(q)) {
        return Some(t);
    }
    let ql = q.to_lowercase();
    sets.iter()
        .find(|t| t.name.to_lowercase() == ql)
        .or_else(|| sets.iter().find(|t| t.name.to_lowercase().starts_with(&ql)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISS: &str = "ISS (ZARYA)
1 25544U 98067A   08264.51782528 -.00002182  00000-0 -11606-4 0  2927
2 25544  51.6416 247.4627 0006703 130.5360 325.0288 15.72125391563537";

    #[test]
    fn parses_the_wikipedia_example() {
        let (sets, errors) = parse_all(ISS);
        assert!(errors.is_empty(), "{errors:?}");
        let t = &sets[0];
        assert_eq!(t.name, "ISS (ZARYA)");
        assert_eq!(t.norad, 25544);
        assert_eq!(t.intl, "98067A");
        assert_eq!(t.epoch_year, 2008);
        assert!((t.epoch_day - 264.51782528).abs() < 1e-12);
        assert!((t.ndot + 0.00002182).abs() < 1e-15);
        assert!((t.bstar + 0.11606e-4).abs() < 1e-15);
        assert!((t.eccentricity - 0.0006703).abs() < 1e-15);
        assert!((t.mean_motion - 15.72125391).abs() < 1e-12);
        assert_eq!(t.rev_number, 56353);
    }

    #[test]
    fn checksum_errors_are_reported() {
        let bad = ISS.replace("2927", "2928");
        let (sets, errors) = parse_all(&bad);
        assert!(sets.is_empty());
        assert!(errors[0].contains("checksum"), "{errors:?}");
    }

    #[test]
    fn flattened_paste_and_two_line_sets() {
        for sep in [" ", ""] {
            let flat = ISS.replace('\n', sep);
            let (sets, errors) = parse_all(&format!("{flat} {flat}"));
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(sets.len(), 2);
            assert_eq!(sets[1].name, "ISS (ZARYA)");
        }
        let two = ISS.lines().skip(1).collect::<Vec<_>>().join("\n");
        let (sets, _) = parse_all(&format!("{two}\n\n{ISS}\n"));
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].label(), "NORAD 25544");
        assert_eq!(find(&sets, "iss").unwrap().name, "ISS (ZARYA)");
        assert_eq!(find(&sets, "25544").unwrap().name, "");
    }

    #[test]
    fn alpha5_and_implied_decimals() {
        assert_eq!(catalog("A0001", 1).unwrap(), 100_001);
        assert_eq!(catalog("Z9999", 1).unwrap(), 339_999);
        assert!((implied("-30915-6", 1, "x").unwrap() + 0.30915e-6).abs() < 1e-20);
        assert!((implied("14311-1", 1, "x").unwrap() - 0.014311).abs() < 1e-15);
        assert_eq!(implied(" 00000+0", 1, "x").unwrap(), 0.0);
    }

    #[test]
    fn epoch_matches_the_calendar() {
        let (sets, _) = parse_all(ISS);
        // 2008 day 264.51782528 = 2008-09-20 12:25:40.104 UTC.
        let want = 1_221_913_540.104;
        assert!(
            (sets[0].epoch_unix() - want).abs() < 1e-3,
            "{}",
            sets[0].epoch_unix()
        );
    }
}
