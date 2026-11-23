//! FFT window functions.

use serde::{Deserialize, Serialize};

/// A window applied before an FFT to reduce spectral leakage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    /// No window: best frequency resolution, worst leakage.
    Rectangular,
    /// A good default.
    #[default]
    Hann,
    Hamming,
    Blackman,
    /// Very low sidelobes (−92 dB), wide main lobe.
    BlackmanHarris,
    /// Accurate amplitudes for tones between bins, very wide main lobe.
    FlatTop,
}

impl Window {
    /// `n` window coefficients (periodic form, as used for spectral analysis).
    pub fn coefficients(self, n: usize) -> Vec<f32> {
        // Generalized cosine windows: w[i] = Σ (−1)^k a_k cos(2πki/n).
        let a: &[f64] = match self {
            Self::Rectangular => &[1.0],
            Self::Hann => &[0.5, 0.5],
            Self::Hamming => &[0.54, 0.46],
            Self::Blackman => &[0.42, 0.5, 0.08],
            Self::BlackmanHarris => &[0.35875, 0.48829, 0.14128, 0.01168],
            Self::FlatTop => &[
                0.215_578_95,
                0.416_631_58,
                0.277_263_158,
                0.083_578_947,
                0.006_947_368,
            ],
        };
        (0..n)
            .map(|i| {
                let x = std::f64::consts::TAU * i as f64 / n as f64;
                a.iter()
                    .enumerate()
                    .map(|(k, ak)| {
                        let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
                        sign * ak * (k as f64 * x).cos()
                    })
                    .sum::<f64>() as f32
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hann_is_zero_at_the_edge_and_one_in_the_middle() {
        let w = Window::Hann.coefficients(8);
        assert!(w[0].abs() < 1e-6);
        assert!((w[4] - 1.0).abs() < 1e-6);
        assert!(
            Window::Rectangular
                .coefficients(4)
                .iter()
                .all(|&v| v == 1.0)
        );
    }
}
