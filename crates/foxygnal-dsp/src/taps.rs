//! Special-purpose FIR tap sets: root-raised-cosine pulse shapes and Hilbert
//! transformers. Run them with [`crate::Resampler`] (SIMD dot products).

use std::f64::consts::PI;

use crate::fir::{DesignWindow, MAX_TAPS};

/// Taps of a root-raised-cosine filter `span` symbols long at `sps` samples
/// per symbol: `round(span·sps)`, made odd so the peak is a tap.
pub fn rrc_len(sps: f64, span: usize) -> usize {
    ((span as f64 * sps).round().max(1.0) as usize) | 1
}

/// Root-raised-cosine taps, `sps` samples per symbol, roll-off `beta`
/// (0..=1), `span` symbols ([`rrc_len`] taps), scaled so they sum to `gain`
/// (the DC gain).
///
/// Two of these in cascade form a raised-cosine pulse, which has no
/// inter-symbol interference at the symbol instants. With `gain = 1` on the
/// receive side and `gain = sps` on an interpolate-by-`sps` transmit side,
/// the cascade's peak (at the symbol instants) is 1.
pub fn rrc(sps: f64, beta: f64, span: usize, gain: f64) -> Vec<f32> {
    let mut taps = vec![0.0; rrc_len(sps, span).min(MAX_TAPS)];
    rrc_into(&mut taps, sps, beta, gain);
    taps
}

/// [`rrc`] into an existing buffer (any length; odd keeps the peak on a tap).
pub fn rrc_into(taps: &mut [f32], sps: f64, beta: f64, gain: f64) {
    let mid = (taps.len() - 1) as f64 / 2.0;
    let beta = beta.clamp(0.0, 1.0);
    let mut sum = 0.0;
    for (i, h) in taps.iter_mut().enumerate() {
        let v = rrc_at((i as f64 - mid) / sps, beta);
        sum += v;
        *h = v as f32;
    }
    let scale = if sum.abs() > 1e-12 { gain / sum } else { 0.0 };
    taps.iter_mut()
        .for_each(|h| *h = (*h as f64 * scale) as f32);
}

/// The (unnormalized) RRC impulse response at `t` symbols from the peak.
fn rrc_at(t: f64, beta: f64) -> f64 {
    if t.abs() < 1e-9 {
        return 1.0 - beta + 4.0 * beta / PI;
    }
    if beta > 0.0 && ((4.0 * beta * t).abs() - 1.0).abs() < 1e-9 {
        let a = PI / (4.0 * beta);
        return beta / 2f64.sqrt() * ((1.0 + 2.0 / PI) * a.sin() + (1.0 - 2.0 / PI) * a.cos());
    }
    let x = 4.0 * beta * t;
    ((PI * t * (1.0 - beta)).sin() + x * (PI * t * (1.0 + beta)).cos()) / (PI * t * (1.0 - x * x))
}

/// A windowed Hilbert transformer of `n` taps (`n` made odd): `2/(πk)` for
/// odd `k` from the centre, 0 for even `k`. Its group delay is `(n−1)/2`
/// samples; it passes positive frequencies shifted by −90° and negative ones
/// by +90° with gain about 1, except near 0 and Nyquist.
pub fn hilbert(n: usize, window: DesignWindow) -> Vec<f32> {
    let n = (n.max(3) | 1).min(MAX_TAPS);
    let mid = (n / 2) as i64;
    (0..n)
        .map(|i| {
            let k = i as i64 - mid;
            if k % 2 == 0 {
                0.0
            } else {
                (2.0 / (PI * k as f64) * window.value(i, n)) as f32
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fir::magnitude_at;

    #[test]
    fn rrc_cascade_has_no_isi() {
        for (sps, beta) in [(4.0, 0.35), (8.0, 0.2), (2.0, 0.5), (5.0, 1.0)] {
            let span = 16;
            let rx = rrc(sps, beta, span, 1.0);
            let tx = rrc(sps, beta, span, sps);
            let n = rx.len();
            // Full convolution; the peak is at n − 1.
            let conv: Vec<f64> = (0..2 * n - 1)
                .map(|k| {
                    (0..n)
                        .filter(|&i| k >= i && k - i < n)
                        .map(|i| tx[i] as f64 * rx[k - i] as f64)
                        .sum()
                })
                .collect();
            let peak = conv[n - 1];
            assert!((peak - 1.0).abs() < 0.01, "sps {sps} β {beta}: peak {peak}");
            let s = sps as usize;
            for m in 1..span {
                for k in [n - 1 + m * s, n - 1 - m * s] {
                    assert!(
                        conv[k].abs() < 0.01,
                        "sps {sps} β {beta}: ISI {} at {m}",
                        conv[k]
                    );
                }
            }
        }
    }

    #[test]
    fn rrc_unity_dc_and_odd_length() {
        let t = rrc(4.0, 0.35, 11, 1.0);
        assert_eq!(t.len(), 45);
        assert!((t.iter().map(|&h| h as f64).sum::<f64>() - 1.0).abs() < 1e-5);
        // Symmetric, peak in the middle.
        for i in 0..t.len() {
            assert!((t[i] - t[t.len() - 1 - i]).abs() < 1e-7);
        }
        // The β = 0.25 singularity (t = ±1 symbol) is finite.
        assert!(rrc(4.0, 0.25, 6, 1.0).iter().all(|h| h.is_finite()));
    }

    #[test]
    fn hilbert_unity_in_band() {
        let h = hilbert(101, DesignWindow::Blackman);
        assert_eq!(h.len(), 101);
        for f in [0.05, 0.1, 0.25, 0.4, 0.45] {
            let m = magnitude_at(&h, f);
            assert!((m - 1.0).abs() < 0.01, "|H({f})| = {m}");
        }
        assert!(magnitude_at(&h, 0.0) < 1e-6);
    }
}
