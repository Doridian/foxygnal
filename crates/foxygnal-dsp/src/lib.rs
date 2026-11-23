//! foxygnal-dsp: signal-processing math shared by blocks and probes.
//! Pure functions and small stateful helpers; no threads, no I/O.

pub mod spectrum;
pub mod window;

pub use num_complex::Complex32;
pub use spectrum::PowerSpectrum;
pub use window::Window;
