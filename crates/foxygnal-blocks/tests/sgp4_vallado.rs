//! SGP4/SDP4 against Vallado's verification set.
//!
//! `data/sgp4/SGP4-VER.TLE` (the test element sets, with start / stop /
//! step minutes after line 2) and `data/sgp4/tcppver.out` (the expected TEME
//! positions and velocities from Vallado's C++ code, WGS-72, opsmode 'i')
//! come from the AIAA 2006-6753 "Revisiting Spacetrack Report #3"
//! distribution (CelesTrak), as vendored by python-sgp4
//! (github.com/brandon-rhodes/python-sgp4, `sgp4/SGP4-VER.TLE` and
//! `sgp4/tcppver.out`), unmodified.
//!
//! Every time step in the expected output is propagated and compared. One
//! case is left out: 33334 (mean motion 0.00001 rev/day) fails in
//! `sgp4init`, and the reference driver then prints the previous
//! satellite's state, which is not a result of SGP4.

use foxygnal_blocks::sat::{Sgp4, Tle};

const TLES: &str = include_str!("data/sgp4/SGP4-VER.TLE");
const EXPECTED: &str = include_str!("data/sgp4/tcppver.out");

/// Position (km) and velocity (km/s) tolerances: 0.1 mm and 1 µm/s. The
/// reference prints 8 and 9 decimals.
const POS_TOL: f64 = 1e-7;
const VEL_TOL: f64 = 3e-9;
/// For the two most eccentric orbits (WIND, e = 0.97; SL-12 R/B after 3.5
/// years, e = 0.79) rounding differences grow: 1 cm.
const POS_TOL_ECCENTRIC: f64 = 1e-5;

struct Case {
    norad: u32,
    steps: Vec<(f64, [f64; 6])>,
}

fn expected() -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    for line in EXPECTED.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() == 2 && f[1] == "xx" {
            cases.push(Case {
                norad: f[0].parse().unwrap(),
                steps: Vec::new(),
            });
        } else if f.len() >= 7 {
            let v: Vec<f64> = f[..7].iter().map(|x| x.parse().unwrap()).collect();
            cases
                .last_mut()
                .unwrap()
                .steps
                .push((v[0], [v[1], v[2], v[3], v[4], v[5], v[6]]));
        }
    }
    cases
}

fn tles() -> Vec<Tle> {
    let lines: Vec<&str> = TLES.lines().filter(|l| !l.starts_with('#')).collect();
    lines
        .chunks(2)
        .map(|p| Tle::from_lines("", p[0], &p[1][..69.min(p[1].len())], false).unwrap())
        .collect()
}

#[test]
fn matches_vallado_verification_output() {
    let tles = tles();
    let cases = expected();
    assert_eq!(tles.len(), cases.len());
    let (mut worst_r, mut worst_v) = (0.0f64, 0.0f64);
    let mut compared = 0;
    for (tle, case) in tles.iter().zip(&cases) {
        assert_eq!(tle.norad, case.norad);
        if case.norad == 33334 {
            assert!(Sgp4::new(tle).is_err(), "33334 should fail to initialise");
            continue;
        }
        let sat = Sgp4::new(tle).unwrap_or_else(|e| panic!("{}: {e}", case.norad));
        for (t, want) in &case.steps {
            let s = sat
                .propagate(*t)
                .unwrap_or_else(|e| panic!("{} at {t} min: {e}", case.norad));
            let dr = (0..3).map(|i| (s.r[i] - want[i]).abs()).fold(0.0, f64::max);
            let dv = (0..3)
                .map(|i| (s.v[i] - want[3 + i]).abs())
                .fold(0.0, f64::max);
            let pos_tol = if matches!(case.norad, 23333 | 20413) {
                POS_TOL_ECCENTRIC
            } else {
                POS_TOL
            };
            assert!(
                dr < pos_tol && dv < VEL_TOL,
                "{} at {t} min: position off by {dr:e} km, velocity by {dv:e} km/s\n got {:?} {:?}\nwant {want:?}",
                case.norad,
                s.r,
                s.v
            );
            worst_r = worst_r.max(dr);
            worst_v = worst_v.max(dv);
            compared += 1;
        }
    }
    eprintln!("{compared} states compared; worst {worst_r:e} km, {worst_v:e} km/s");
    assert!(compared > 500);
}

/// The reference output stops where SGP4 reports an error; check that we
/// report one at the next step too.
#[test]
fn errors_where_the_reference_stops() {
    let tles = tles();
    let cases = expected();
    let lines: Vec<&str> = TLES.lines().filter(|l| !l.starts_with('#')).collect();
    let mut checked = 0;
    for ((tle, case), pair) in tles.iter().zip(&cases).zip(lines.chunks(2)) {
        if case.norad == 33334 {
            continue;
        }
        // Start, stop and step after line 2's 69 columns.
        let extra: Vec<f64> = pair[1][69..]
            .split_whitespace()
            .map(|x| x.parse().unwrap())
            .collect();
        let (stop, step) = (extra[1], extra[2]);
        let last = case.steps.last().unwrap().0;
        if last + step <= stop && last < stop {
            let sat = Sgp4::new(tle).unwrap();
            let next = last + step;
            assert!(
                sat.propagate(next).is_err(),
                "{}: the reference stops at {last} min, we propagate past it",
                case.norad
            );
            checked += 1;
        }
    }
    assert!(checked >= 3, "{checked}");
}
