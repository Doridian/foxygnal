//! Where a satellite is as seen from a place on the ground: azimuth,
//! elevation, range, range rate and Doppler, and the point below it.
//!
//! Approximations, all far below what matters for pointing an antenna or
//! correcting Doppler: TEME is turned into Earth-fixed coordinates by the
//! Greenwich mean sidereal time alone (UTC is used for UT1, which is within
//! 0.9 s; polar motion, a few metres, is ignored), and the observer sits on
//! the WGS-84 ellipsoid.

use super::sgp4::{State, gstime};

/// WGS-84.
const A: f64 = 6378.137; // km
const F: f64 = 1.0 / 298.257_223_563;
/// Earth's rotation rate, rad/s.
const OMEGA_E: f64 = 7.292_115_146_706_98e-5;
/// Speed of light, km/s.
pub const C_KM_S: f64 = 299_792.458;

/// A place on the ground.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observer {
    /// Geodetic latitude, degrees north.
    pub lat: f64,
    /// Longitude, degrees east.
    pub lon: f64,
    /// Height above the ellipsoid, metres.
    pub alt_m: f64,
}

/// The satellite as seen by an observer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Look {
    /// Degrees clockwise from true north, 0–360.
    pub az: f64,
    /// Degrees above the horizon.
    pub el: f64,
    /// km.
    pub range: f64,
    /// km/s, positive when moving away.
    pub range_rate: f64,
}

impl Look {
    /// Doppler shift (Hz) of a carrier at `freq` Hz sent by the satellite:
    /// received minus transmitted, positive while it approaches.
    pub fn doppler(&self, freq: f64) -> f64 {
        -freq * self.range_rate / C_KM_S
    }
}

/// Unix time (s) as a Julian date.
pub fn unix_to_jd(unix: f64) -> f64 {
    unix / 86_400.0 + 2_440_587.5
}

/// Earth-fixed position (km) and velocity (km/s) from TEME at a Unix time.
pub fn teme_to_ecef(s: &State, unix: f64) -> ([f64; 3], [f64; 3]) {
    let g = gstime(unix_to_jd(unix));
    let (sg, cg) = g.sin_cos();
    let r = [
        cg * s.r[0] + sg * s.r[1],
        -sg * s.r[0] + cg * s.r[1],
        s.r[2],
    ];
    let v = [
        cg * s.v[0] + sg * s.v[1] + OMEGA_E * r[1],
        -sg * s.v[0] + cg * s.v[1] - OMEGA_E * r[0],
        s.v[2],
    ];
    (r, v)
}

impl Observer {
    /// Earth-fixed position, km.
    pub fn ecef(&self) -> [f64; 3] {
        let (sl, cl) = self.lat.to_radians().sin_cos();
        let (so, co) = self.lon.to_radians().sin_cos();
        let e2 = F * (2.0 - F);
        let n = A / (1.0 - e2 * sl * sl).sqrt();
        let h = self.alt_m / 1000.0;
        [
            (n + h) * cl * co,
            (n + h) * cl * so,
            (n * (1.0 - e2) + h) * sl,
        ]
    }

    /// Look angles to a satellite at TEME state `s` at a Unix time.
    pub fn look(&self, s: &State, unix: f64) -> Look {
        let (r, v) = teme_to_ecef(s, unix);
        self.look_ecef(&r, &v)
    }

    /// Look angles to an Earth-fixed position and velocity.
    pub fn look_ecef(&self, r: &[f64; 3], v: &[f64; 3]) -> Look {
        let o = self.ecef();
        let d = [r[0] - o[0], r[1] - o[1], r[2] - o[2]];
        let (sl, cl) = self.lat.to_radians().sin_cos();
        let (so, co) = self.lon.to_radians().sin_cos();
        let east = -so * d[0] + co * d[1];
        let north = -sl * co * d[0] - sl * so * d[1] + cl * d[2];
        let up = cl * co * d[0] + cl * so * d[1] + sl * d[2];
        let range = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let mut az = east.atan2(north).to_degrees();
        if az < 0.0 {
            az += 360.0;
        }
        Look {
            az,
            el: (up / range).clamp(-1.0, 1.0).asin().to_degrees(),
            range,
            range_rate: (d[0] * v[0] + d[1] * v[1] + d[2] * v[2]) / range,
        }
    }
}

/// Geodetic latitude, longitude (degrees) and height (km) of an Earth-fixed
/// position (Bowring's method, iterated; sub-millimetre for orbits).
pub fn geodetic(r: &[f64; 3]) -> (f64, f64, f64) {
    let e2 = F * (2.0 - F);
    let p = (r[0] * r[0] + r[1] * r[1]).sqrt();
    let lon = r[1].atan2(r[0]);
    let mut lat = r[2].atan2(p * (1.0 - e2));
    let mut h = 0.0;
    for _ in 0..5 {
        let s = lat.sin();
        let n = A / (1.0 - e2 * s * s).sqrt();
        h = if lat.cos().abs() > 1e-9 {
            p / lat.cos() - n
        } else {
            r[2].abs() - n * (1.0 - e2)
        };
        lat = r[2].atan2(p * (1.0 - e2 * n / (n + h)));
    }
    (lat.to_degrees(), lon.to_degrees(), h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overhead_satellite_is_at_90_degrees() {
        let o = Observer {
            lat: 47.0,
            lon: 8.0,
            alt_m: 400.0,
        };
        let g = o.ecef();
        let n = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt();
        // 800 km straight up along the ellipsoid normal (close to radial).
        let (sl, cl) = 47f64.to_radians().sin_cos();
        let (so, co) = 8f64.to_radians().sin_cos();
        let up = [cl * co, cl * so, sl];
        let r = [
            g[0] + 800.0 * up[0],
            g[1] + 800.0 * up[1],
            g[2] + 800.0 * up[2],
        ];
        let l = o.look_ecef(&r, &[up[0], up[1], up[2]]);
        assert!(l.el > 89.999, "{l:?}");
        assert!((l.range - 800.0).abs() < 1e-9);
        assert!((l.range_rate - 1.0).abs() < 1e-12);
        // Receding at 1 km/s: 137 MHz comes down by ~457 Hz.
        assert!(
            (l.doppler(137e6) + 456.98).abs() < 0.01,
            "{}",
            l.doppler(137e6)
        );
        let (lat, lon, h) = geodetic(&r);
        assert!(
            (lat - 47.0).abs() < 1e-9 && (lon - 8.0).abs() < 1e-9,
            "{lat} {lon}"
        );
        assert!((h - 800.4).abs() < 1e-6, "{h}");
        assert!(n > 6000.0);
    }

    #[test]
    fn azimuth_points_north_and_east() {
        let o = Observer {
            lat: 0.0,
            lon: 0.0,
            alt_m: 0.0,
        };
        let north = o.look_ecef(&[A, 0.0, 100.0], &[0.0; 3]);
        assert!(north.az.abs() < 1e-9 && north.el.abs() < 1e-6, "{north:?}");
        let east = o.look_ecef(&[A, 100.0, 0.0], &[0.0; 3]);
        assert!((east.az - 90.0).abs() < 1e-9, "{east:?}");
    }
}
