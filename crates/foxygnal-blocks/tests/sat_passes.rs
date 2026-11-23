//! Look angles and passes against Skyfield.
//!
//! `data/sat/weather-2026-11-23.txt` is CelesTrak's `weather` group as
//! fetched on 2026-11-23; `data/sat/skyfield-2026-11-23.txt` is what
//! Skyfield 1.55 (full IAU 2000 Earth orientation, its own SGP4) makes of it
//! for an observer in Berlin (`data/sat/skyfield.py`, run as
//! `python skyfield.py weather-2026-11-23.txt 1.55`): rise / culmination /
//! set times over 24 hours and look angles every 7 minutes for four polar
//! orbiters.

use foxygnal_blocks::sat::{Observer, Sgp4, find, parse_all, passes};

const TLES: &str = include_str!("data/sat/weather-2026-11-23.txt");
const SKYFIELD: &str = include_str!("data/sat/skyfield-2026-11-23.txt");

const OBS: Observer = Observer {
    lat: 52.52,
    lon: 13.405,
    alt_m: 34.0,
};
const START: f64 = 1_795_435_200.0; // 2026-11-23T12:00:00Z

fn sat(key: &str) -> Sgp4 {
    let (sets, errors) = parse_all(TLES);
    assert!(errors.is_empty(), "{errors:?}");
    Sgp4::new(find(&sets, &key.replace('_', " ")).unwrap()).unwrap()
}

fn rows(kind: &str) -> Vec<Vec<String>> {
    SKYFIELD
        .lines()
        .filter(|l| l.starts_with(kind))
        .map(|l| l.split_whitespace().skip(1).map(str::to_owned).collect())
        .collect()
}

fn angle_diff(a: f64, b: f64) -> f64 {
    ((a - b + 540.0) % 360.0 - 180.0).abs()
}

#[test]
fn look_angles_match_skyfield() {
    let (mut worst_el, mut worst_az, mut worst_range, mut worst_rr) =
        (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut n = 0;
    for r in rows("look") {
        let s = sat(&r[0]);
        let t: f64 = r[1].parse().unwrap();
        let want: Vec<f64> = r[2..].iter().map(|x| x.parse().unwrap()).collect();
        let l = OBS.look(&s.propagate_unix(t).unwrap(), t);
        worst_el = worst_el.max((l.el - want[0]).abs());
        if want[0] > -80.0 {
            worst_az = worst_az.max(angle_diff(l.az, want[1]) * want[0].to_radians().cos());
        }
        worst_range = worst_range.max((l.range - want[2]).abs());
        worst_rr = worst_rr.max((l.range_rate - want[3]).abs());
        n += 1;
    }
    eprintln!(
        "{n} looks: el {worst_el:.4}°, az·cos(el) {worst_az:.4}°, range {worst_range:.3} km, \
         range rate {:.2} m/s",
        worst_rr * 1e3
    );
    // GMST instead of the full Earth orientation, UTC for UT1 and no polar
    // motion: measured 0.001°, 24 m and 0.08 m/s.
    assert!(worst_el < 0.005, "{worst_el}");
    assert!(worst_az < 0.005, "{worst_az}");
    assert!(worst_range < 0.1, "{worst_range}");
    // 0.5 m/s is 3 Hz of Doppler at 1.7 GHz.
    assert!(worst_rr < 5e-4, "{worst_rr}");
    assert!(n > 800);
}

#[test]
fn passes_match_skyfield() {
    let events = rows("event");
    let mut keys: Vec<&str> = events.iter().map(|e| e[0].as_str()).collect();
    keys.dedup();
    let mut total = 0;
    let mut worst_t = 0.0f64;
    for key in keys {
        let s = sat(key);
        let ev: Vec<(u8, f64, f64)> = events
            .iter()
            .filter(|e| e[0] == key)
            .map(|e| {
                (
                    e[1].parse().unwrap(),
                    e[2].parse().unwrap(),
                    e[3].parse().unwrap(),
                )
            })
            .collect();
        // Skyfield's passes: rise, culmination(s), set.
        let mut want = Vec::new();
        let mut i = 0;
        while i < ev.len() {
            assert_eq!(ev[i].0, 0, "{key}: window starts mid-pass");
            let mut j = i + 1;
            let mut best = (0.0, f64::NEG_INFINITY);
            while j < ev.len() && ev[j].0 == 1 {
                if ev[j].2 > best.1 {
                    best = (ev[j].1, ev[j].2);
                }
                j += 1;
            }
            // A pass still going at the end of the window has no set.
            let set = ev.get(j).map_or(START + 86_400.0, |e| e.1);
            want.push((ev[i].1, best.0, best.1, set));
            i = j + 1;
        }
        let got = passes(&s, &OBS, 0.0, START, START + 86_400.0);
        assert_eq!(got.len(), want.len(), "{key}: {got:#?}\nwant {want:?}");
        for (g, w) in got.iter().zip(&want) {
            assert!((g.aos - w.0).abs() < 1.0, "{key} AOS {} vs {}", g.aos, w.0);
            assert!((g.los - w.3).abs() < 1.0, "{key} LOS {} vs {}", g.los, w.3);
            if w.2.is_finite() {
                assert!((g.tca - w.1).abs() < 5.0, "{key} TCA {} vs {}", g.tca, w.1);
                assert!(
                    (g.max_el - w.2).abs() < 0.02,
                    "{key} max el {} vs {}",
                    g.max_el,
                    w.2
                );
            }
            worst_t = worst_t.max((g.aos - w.0).abs()).max((g.los - w.3).abs());
            assert!(!g.aos_clipped);
            assert_eq!(g.los_clipped, w.3 == START + 86_400.0);
            total += 1;
        }
    }
    eprintln!("{total} passes, AOS/LOS within {worst_t:.2} s");
    assert!(total > 20, "{total}");
}

#[test]
fn doppler_is_the_derivative_of_range() {
    let s = sat("METEOR-M2_3");
    // Somewhere in a pass (high or low, it doesn't matter for the identity).
    for k in 0..50 {
        let t = START + f64::from(k) * 613.0;
        let l = OBS.look(&s.propagate_unix(t).unwrap(), t);
        let r = |t: f64| OBS.look(&s.propagate_unix(t).unwrap(), t).range;
        let numeric = (r(t + 0.05) - r(t - 0.05)) / 0.1;
        assert!(
            // SGP4's velocity is not exactly the derivative of its position
            // (a few cm/s), hence 10 cm/s.
            (numeric - l.range_rate).abs() < 1e-4,
            "{t}: {numeric} vs {}",
            l.range_rate
        );
        // And Doppler is its scaled negative.
        let d = l.doppler(1.7e9);
        assert!((d + 1.7e9 * l.range_rate / 299_792.458).abs() < 1e-6);
    }
}
