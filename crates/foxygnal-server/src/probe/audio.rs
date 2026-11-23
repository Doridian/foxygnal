//! Audio: every sample of a stream, for the browser to play.
//!
//! Unlike the other instruments this one is lossless while it keeps up: each
//! frame carries all samples since the last frame. If the tap fell behind,
//! the frame says so and the player re-syncs.
//!
//! A real wire is mono. A complex (c32) wire is stereo: real part left,
//! imaginary part right; the frame sets flag 2 and interleaves L, R.

use foxygnal_core::Complex32;

use super::Instrument;
use super::source::{Frame, Source};

/// At most this many frames (sample instants) per frame (about 1.4 s at
/// 48 kS/s). More than that between frames means the tab or the probe thread
/// stalled; the oldest are dropped and the frame is marked as a gap.
const MAX_PENDING: usize = 1 << 16;

/// Frame flags.
const GAP: u8 = 1;
const STEREO: u8 = 2;

#[derive(Default)]
pub struct Audio {
    stereo: bool,
    /// Mono samples, or interleaved L, R.
    pending: Vec<f32>,
    gap: bool,
    buf: Vec<Complex32>,
}

impl Audio {
    pub fn new(stereo: bool) -> Self {
        Self {
            stereo,
            ..Self::default()
        }
    }

    fn channels(&self) -> usize {
        if self.stereo { 2 } else { 1 }
    }
}

impl Instrument for Audio {
    fn poll(&mut self, src: &mut Source) {
        self.buf.clear();
        if !src.fresh(MAX_PENDING, &mut self.buf) {
            self.gap = true;
        }
        if self.stereo {
            self.pending
                .extend(self.buf.iter().flat_map(|c| [c.re, c.im]));
        } else {
            self.pending.extend(self.buf.iter().map(|c| c.re));
        }
        let max = MAX_PENDING * self.channels();
        if self.pending.len() > max {
            let extra = self.pending.len() - max;
            self.pending.drain(..extra);
            self.gap = true;
        }
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let flags = (if self.gap { GAP } else { 0 }) | (if self.stereo { STEREO } else { 0 });
        Frame(out)
            .u32((self.pending.len() / self.channels()) as u32)
            .bytes4([flags, 0, 0, 0])
            .f32s(self.pending.drain(..));
        self.gap = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_frames_interleave_and_count_frames() {
        let mut a = Audio::new(true);
        a.pending.extend([0.1, -0.1, 0.2, -0.2]);
        let mut out = Vec::new();
        assert!(a.frame(&mut out));
        assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 2);
        assert_eq!(out[4], STEREO);
        let f = |i: usize| f32::from_le_bytes(out[8 + 4 * i..12 + 4 * i].try_into().unwrap());
        assert_eq!((f(0), f(1), f(2), f(3)), (0.1, -0.1, 0.2, -0.2));
    }
}
