//! foxygnal-dsp: signal-processing math shared by blocks and probes.
//! Pure functions and small stateful helpers; no threads, no I/O.

pub mod filter;
pub mod fir;
pub mod iir;
pub mod kernels;
pub mod spectrum;
pub mod taps;
pub mod window;

pub use filter::{FilterSample, OnePole, Resampler, Rotator};
pub use fir::{Band, DesignError, DesignWindow, FirSpec};
pub use iir::{Biquad, BiquadCascade, BiquadKind, DcBlocker};
pub use kernels::{Kernels, kernels};
pub use num_complex::Complex32;
pub use spectrum::PowerSpectrum;
pub use window::Window;
