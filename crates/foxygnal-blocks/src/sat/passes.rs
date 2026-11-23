//! Upcoming passes: when a satellite rises above a minimum elevation (AOS),
//! culminates (TCA) and sets (LOS).

use super::observer::{Look, Observer};
use super::sgp4::Sgp4;

/// One pass over the observer. Times are Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pass {
    /// Acquisition of signal: the satellite rises above the minimum
    /// elevation (or the search window's start if it was already up).
    pub aos: f64,
    /// Time of closest approach: highest elevation.
    pub tca: f64,
    /// Loss of signal (or the window's end if it is still up).
    pub los: f64,
    pub max_el: f64,
    pub aos_az: f64,
    pub tca_az: f64,
    pub los_az: f64,
    /// The satellite was already up at the start of the window (and for an
    /// hour before it): `aos` is not a real rise.
    pub aos_clipped: bool,
    /// Still up at the end of the window.
    pub los_clipped: bool,
}

/// Coarse step for finding horizon crossings. A LEO pass above 0° lasts
/// several minutes; one that peaks within a fraction of a degree of the
/// minimum elevation can slip between two steps.
const STEP_S: f64 = 20.0;
/// How far back to look for the rise of a satellite that is up at the start.
const LOOKBACK_S: f64 = 3600.0;

fn look(sat: &Sgp4, obs: &Observer, t: f64) -> Option<Look> {
    sat.propagate_unix(t).ok().map(|s| obs.look(&s, t))
}

fn el(sat: &Sgp4, obs: &Observer, t: f64) -> f64 {
    look(sat, obs, t).map_or(f64::NEG_INFINITY, |l| l.el)
}

/// The time in `[a, b]` where the elevation crosses `min_el` (one side is
/// above, the other below), to 0.1 s.
fn crossing(sat: &Sgp4, obs: &Observer, min_el: f64, mut a: f64, mut b: f64) -> f64 {
    let up_a = el(sat, obs, a) >= min_el;
    while b - a > 0.1 {
        let m = 0.5 * (a + b);
        if (el(sat, obs, m) >= min_el) == up_a {
            a = m;
        } else {
            b = m;
        }
    }
    0.5 * (a + b)
}

/// The highest elevation in `[a, b]` (golden-section search, refined from a
/// coarse scan so a long pass with a flat top isn't fooled).
fn culmination(sat: &Sgp4, obs: &Observer, a: f64, b: f64) -> f64 {
    let n = (((b - a) / STEP_S).ceil() as usize).max(2);
    let dt = (b - a) / n as f64;
    let mut best = a;
    let mut best_el = f64::NEG_INFINITY;
    for i in 0..=n {
        let t = a + i as f64 * dt;
        let e = el(sat, obs, t);
        if e > best_el {
            best_el = e;
            best = t;
        }
    }
    let (mut lo, mut hi) = ((best - dt).max(a), (best + dt).min(b));
    let g = 0.5 * (5f64.sqrt() - 1.0);
    let mut x1 = hi - g * (hi - lo);
    let mut x2 = lo + g * (hi - lo);
    let (mut f1, mut f2) = (el(sat, obs, x1), el(sat, obs, x2));
    while hi - lo > 0.1 {
        if f1 < f2 {
            lo = x1;
            x1 = x2;
            f1 = f2;
            x2 = lo + g * (hi - lo);
            f2 = el(sat, obs, x2);
        } else {
            hi = x2;
            x2 = x1;
            f2 = f1;
            x1 = hi - g * (hi - lo);
            f1 = el(sat, obs, x1);
        }
    }
    0.5 * (lo + hi)
}

fn make_pass(
    sat: &Sgp4,
    obs: &Observer,
    aos: f64,
    los: f64,
    clipped: (bool, bool),
) -> Option<Pass> {
    let tca = culmination(sat, obs, aos, los);
    let (a, t, l) = (
        look(sat, obs, aos)?,
        look(sat, obs, tca)?,
        look(sat, obs, los)?,
    );
    Some(Pass {
        aos,
        tca,
        los,
        max_el: t.el,
        aos_az: a.az,
        tca_az: t.az,
        los_az: l.az,
        aos_clipped: clipped.0,
        los_clipped: clipped.1,
    })
}

/// Passes of `sat` over `obs` above `min_el` degrees between Unix times
/// `start` and `end`. A pass in progress at `start` is included with its real
/// AOS (up to an hour back).
pub fn passes(sat: &Sgp4, obs: &Observer, min_el: f64, start: f64, end: f64) -> Vec<Pass> {
    let mut out = Vec::new();
    if end <= start {
        return out;
    }
    let mut t = start;
    let mut up = el(sat, obs, t) >= min_el;
    let mut aos = None;
    let mut aos_clipped = false;
    if up {
        // Find where this pass began.
        let mut b = start;
        let mut found = false;
        while start - b < LOOKBACK_S {
            let a = b - STEP_S;
            if el(sat, obs, a) < min_el {
                aos = Some(crossing(sat, obs, min_el, a, b));
                found = true;
                break;
            }
            b = a;
        }
        if !found {
            aos = Some(start);
            aos_clipped = true;
        }
    }
    while t < end {
        let next = (t + STEP_S).min(end);
        let up_next = el(sat, obs, next) >= min_el;
        if !up && up_next {
            aos = Some(crossing(sat, obs, min_el, t, next));
            aos_clipped = false;
        } else if up && !up_next {
            let los = crossing(sat, obs, min_el, t, next);
            if let Some(a) = aos.take()
                && let Some(p) = make_pass(sat, obs, a, los, (aos_clipped, false))
            {
                out.push(p);
            }
        }
        up = up_next;
        t = next;
    }
    if up
        && let Some(a) = aos
        && let Some(p) = make_pass(sat, obs, a, end, (aos_clipped, true))
    {
        out.push(p);
    }
    out
}
