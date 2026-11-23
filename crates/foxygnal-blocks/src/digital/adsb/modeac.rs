//! Mode A/C replies: two framing pulses F1 and F2 20.3 µs apart, with up to
//! 13 code pulses between them every 1.45 µs (C1 A1 C2 A2 C4 A4 X B1 D1 B2
//! D2 B4 D4, X always empty) and an optional SPI pulse 4.35 µs after F2.
//! Each pulse is 0.45 µs long; levels are read by integrating over each
//! pulse slot (see [`super::demod`]) after finding F1's start to a tenth of a
//! sample.
//!
//! The reply carries a squawk (Mode A) or a Gillham altitude (Mode C) and
//! nothing says which, nor who sent it. With no CRC, the only defence
//! against noise and Mode S fragments is the shape: both framing pulses well
//! above the noise and of similar height, every gap between pulse slots
//! empty, X empty, and silence just before F1 and after F2.

use super::demod::Signal;

/// Pulse length, µs.
const W: f64 = 0.45;
/// Pulse spacing, µs.
const STEP: f64 = 1.45;

/// Samples needed from the start position at `s` samples per µs.
pub fn span(s: f64) -> usize {
    (27.0 * s).ceil() as usize + 2
}

/// A Mode A/C reply whose F1 pulse starts within half a sample of `n`: the
/// 13 code bits in time order (C1 first, as [`super::msg::id13_squawk`] wants
/// them) and the SPI flag. `noise` is the mean magnitude around. The caller
/// makes sure `n >= 1.5 µs` into the buffer and [`span`] samples follow.
pub fn detect(sig: &Signal<'_>, n: usize, s: f64, noise: f64) -> Option<(u32, bool)> {
    let floor = 4.0 * noise;
    if f64::from(sig.m[n]) < floor {
        return None;
    }
    let w = W * s;
    // F1's start: the window holding the most energy.
    let (f1, t) = (-5..=5)
        .map(|k| {
            let t = n as f64 + f64::from(k) * 0.1;
            (sig.level(t, t + w), t)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))?;
    let slot = |k: f64| {
        let a = t + k * STEP * s;
        sig.level(a, a + w)
    };
    let f2 = slot(14.0);
    if f1 < floor || f2 < floor || f2 * 2.0 < f1 || f1 * 2.0 < f2 {
        return None;
    }
    let thr = 0.5 * f1.min(f2);
    // Silence before F1, in every gap, and in the two slots after F2.
    let gap = |k: f64| {
        let a = t + (k * STEP + W + 0.25) * s;
        sig.level(a, a + 0.5 * s)
    };
    if sig.level(t - 1.25 * s, t - 0.25 * s) >= thr
        || (0..=15).any(|k| gap(f64::from(k)) >= thr)
        || slot(15.0) >= thr
        || slot(16.0) >= thr
    {
        return None;
    }
    let mut code = 0u32;
    for k in 1..=13 {
        code = code << 1 | u32::from(slot(f64::from(k)) >= thr);
    }
    // X (the 7th) is never sent.
    if code & 0x40 != 0 {
        return None;
    }
    let spi = slot(17.0) >= thr;
    Some((code, spi))
}
