//! Satellites: SGP4/SDP4 orbit propagation from two-line element sets, look
//! angles and Doppler from a place on the ground, pass prediction, and the
//! `sat_tracker` block that turns all of it into control streams (Doppler
//! correction for `freq_xlating_fir`, azimuth and elevation) and a ground
//! track for the Map instrument.
//!
//! Nothing here fetches element sets from the network: they come from a file
//! or pasted text.

pub mod observer;
pub mod passes;
pub mod sgp4;
pub mod tle;
mod tracker;

use foxygnal_core::Registry;

pub use observer::{Look, Observer};
pub use passes::{Pass, passes};
pub use sgp4::{Sgp4, Sgp4Error, State};
pub use tle::{Tle, find, parse_all};
pub use tracker::SatTracker;

pub fn register(r: &mut Registry) {
    tracker::register(r);
}

/// The current time as Unix seconds.
pub fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Parse a UTC time: `2026-11-23T14:05:00Z`, `2026-11-23 14:05:00`,
/// `2026-11-23T14:05:00.25`, `2026-11-23T14:05Z` or a date alone. An offset
/// other than `Z` (`+01:00`) is applied. Returns Unix seconds.
pub fn parse_utc(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = match s.find(['T', ' ', 't']) {
        Some(p) => (&s[..p], s[p + 1..].trim()),
        None => (s, ""),
    };
    let mut d = date.split('-');
    let year: i32 = d.next()?.parse().ok()?;
    let mon: i32 = d.next()?.parse().ok()?;
    let day: i32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&mon) || !(1..=31).contains(&day) {
        return None;
    }
    // Time zone suffix.
    let (time, offset_s) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0.0)
    } else if let Some(p) = rest.rfind(['+', '-']) {
        let (h, m) = rest[p + 1..]
            .split_once(':')
            .unwrap_or((&rest[p + 1..], "0"));
        let off = h.parse::<f64>().ok()? * 3600.0 + m.parse::<f64>().ok()? * 60.0;
        (&rest[..p], if &rest[p..p + 1] == "-" { -off } else { off })
    } else {
        (rest, 0.0)
    };
    let mut t = time.split(':');
    let hr: i32 = t
        .next()
        .filter(|x| !x.is_empty())
        .map_or(Some(0), |x| x.parse().ok())?;
    let minute: i32 = t.next().map_or(Some(0), |x| x.parse().ok())?;
    let sec: f64 = t.next().map_or(Some(0.0), |x| x.parse().ok())?;
    if t.next().is_some()
        || !(0..24).contains(&hr)
        || !(0..60).contains(&minute)
        || !(0.0..61.0).contains(&sec)
    {
        return None;
    }
    let (jd, frac) = tle::jday(year, mon, day, hr, minute, sec);
    Some(((jd - 2_440_587.5) + frac) * 86_400.0 - offset_s)
}

/// Format Unix seconds as `2026-11-23T14:05:00Z` (whole seconds).
pub struct Utc(pub f64);

impl std::fmt::Display for Utc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self.0.floor() as i64;
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        // Civil from days (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        write!(
            f,
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            sod / 3600,
            sod / 60 % 60,
            sod % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_round_trips() {
        let t = parse_utc("2026-11-23T14:05:07Z").unwrap();
        assert_eq!(t, 1_795_442_707.0);
        assert_eq!(Utc(t).to_string(), "2026-11-23T14:05:07Z");
        assert_eq!(parse_utc("2026-11-23 14:05:07"), Some(t));
        assert_eq!(parse_utc("2026-11-23T15:05:07+01:00"), Some(t));
        assert_eq!(
            parse_utc("2026-11-23"),
            Some(t - 14.0 * 3600.0 - 5.0 * 60.0 - 7.0)
        );
        assert_eq!(Utc(0.0).to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(Utc(951_782_400.0).to_string(), "2000-02-29T00:00:00Z");
        assert!(parse_utc("yesterday").is_none());
        assert!(parse_utc("2026-13-01").is_none());
    }
}
