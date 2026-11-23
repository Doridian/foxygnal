//! Compact Position Reporting: positions from the 17-bit latitude and
//! longitude of ADS-B position messages, either globally from an even and an
//! odd frame, or locally from one frame and a reference position nearby.
//!
//! Airborne positions divide the globe into 60 (even) or 59 (odd) latitude
//! zones over 360°, surface positions over 90°, so a surface position decoded
//! globally is known only up to a quadrant and needs a reference to pick one.

use super::msg::Cpr;

const SCALE: f64 = 131_072.0; // 2^17

/// Number of longitude zones at a latitude (the CPR `NL` function).
pub fn nl(lat: f64) -> u32 {
    let lat = lat.abs();
    if lat < 1e-9 {
        return 59;
    }
    if lat >= 87.0 {
        return if lat > 87.0 { 1 } else { 2 };
    }
    let nz = 15.0;
    let a = 1.0 - (std::f64::consts::PI / (2.0 * nz)).cos();
    let b = lat.to_radians().cos().powi(2);
    (std::f64::consts::TAU / (1.0 - a / b).acos()).floor() as u32
}

fn span(surface: bool) -> f64 {
    if surface { 90.0 } else { 360.0 }
}

/// Globally unambiguous airborne position from an even and an odd CPR frame
/// (sent within ~10 s of each other). `odd_newest` picks which one the
/// position is for. `None` if the two straddle a longitude-zone boundary.
pub fn cpr_global(even: Cpr, odd: Cpr, odd_newest: bool) -> Option<(f64, f64)> {
    let (lat, lon) = global(even, odd, odd_newest, false)?;
    Some((lat, wrap180(lon)))
}

/// Surface position from an even and an odd frame, in the quadrant nearest
/// the reference `(lat, lon)` (the receiver, or the aircraft's last position).
pub fn cpr_global_surface(
    even: Cpr,
    odd: Cpr,
    odd_newest: bool,
    reference: (f64, f64),
) -> Option<(f64, f64)> {
    let (lat, lon) = global(even, odd, odd_newest, true)?;
    // Latitude: the solution in the northern hemisphere, or 90° south of it.
    let lat = [lat, lat - 90.0]
        .into_iter()
        .min_by(|a, b| (a - reference.0).abs().total_cmp(&(b - reference.0).abs()))?;
    // Longitude: one of four, 90° apart.
    let lon = (0..4)
        .map(|k| wrap180(lon + 90.0 * f64::from(k)))
        .min_by(|a, b| angle_diff(*a, reference.1).total_cmp(&angle_diff(*b, reference.1)))?;
    Some((lat, lon))
}

fn global(even: Cpr, odd: Cpr, odd_newest: bool, surface: bool) -> Option<(f64, f64)> {
    let span = span(surface);
    let (lat_e, lon_e) = (f64::from(even.lat) / SCALE, f64::from(even.lon) / SCALE);
    let (lat_o, lon_o) = (f64::from(odd.lat) / SCALE, f64::from(odd.lon) / SCALE);
    let j = (59.0 * lat_e - 60.0 * lat_o + 0.5).floor();
    let fix = |lat: f64| {
        if !surface && lat >= 270.0 {
            lat - 360.0
        } else {
            lat
        }
    };
    let lat_even = fix(span / 60.0 * (j.rem_euclid(60.0) + lat_e));
    let lat_odd = fix(span / 59.0 * (j.rem_euclid(59.0) + lat_o));
    if !(-90.0..=90.0).contains(&lat_even) || !(-90.0..=90.0).contains(&lat_odd) {
        return None;
    }
    if nl(lat_even) != nl(lat_odd) {
        return None;
    }
    let (lat, l) = if odd_newest {
        (lat_odd, nl(lat_odd))
    } else {
        (lat_even, nl(lat_even))
    };
    let ni = if odd_newest { l.saturating_sub(1) } else { l }.max(1);
    let m = (lon_e * f64::from(l - 1) - lon_o * f64::from(l) + 0.5).floor();
    let own = if odd_newest { lon_o } else { lon_e };
    let lon = span / f64::from(ni) * (m.rem_euclid(f64::from(ni)) + own);
    Some((lat, lon))
}

/// Position from one frame and a reference `(lat, lon)` closer than half a
/// zone (about 180 NM airborne, 45 NM on the surface).
pub fn cpr_local(cpr: Cpr, reference: (f64, f64)) -> (f64, f64) {
    let span = span(cpr.surface);
    let i = f64::from(u8::from(cpr.odd));
    let (y, x) = (f64::from(cpr.lat) / SCALE, f64::from(cpr.lon) / SCALE);
    let dlat = span / (60.0 - i);
    let (rlat, rlon) = reference;
    let j = (rlat / dlat).floor() + (0.5 + rlat.rem_euclid(dlat) / dlat - y).floor();
    let lat = dlat * (j + y);
    let ni = (f64::from(nl(lat)) - i).max(1.0);
    let dlon = span / ni;
    let m = (rlon / dlon).floor() + (0.5 + rlon.rem_euclid(dlon) / dlon - x).floor();
    (lat, wrap180(dlon * (m + x)))
}

fn wrap180(lon: f64) -> f64 {
    let l = lon.rem_euclid(360.0);
    if l >= 180.0 { l - 360.0 } else { l }
}

fn angle_diff(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Great-circle distance in nautical miles.
pub fn distance_nm(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, la2) = (a.0.to_radians(), b.0.to_radians());
    let dla = la2 - la1;
    let dlo = (b.1 - a.1).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 3440.065 * h.sqrt().min(1.0).asin()
}
