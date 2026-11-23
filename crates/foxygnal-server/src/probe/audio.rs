//! Audio: every sample of a real stream, for the browser to play.
//!
//! Unlike the other instruments this one is lossless while it keeps up: each
//! frame carries all samples since the last frame. If the tap fell behind
//! (or the stream is complex), the frame says so and the player re-syncs.

use foxygnal_core::Complex32;

use super::Instrument;
use super::source::{Frame, Source};

/// At most this many samples per frame (about 1.4 s at 48 kS/s). More than
/// that between frames means the tab or the probe thread stalled; the oldest
/// are dropped and the frame is marked as a gap.
const MAX_PENDING: usize = 1 << 16;

#[derive(Default)]
pub struct Audio {
    pending: Vec<f32>,
    gap: bool,
    buf: Vec<Complex32>,
}

impl Instrument for Audio {
    fn poll(&mut self, src: &mut Source) {
        self.buf.clear();
        if !src.fresh(MAX_PENDING, &mut self.buf) {
            self.gap = true;
        }
        self.pending.extend(self.buf.iter().map(|c| c.re));
        if self.pending.len() > MAX_PENDING {
            let extra = self.pending.len() - MAX_PENDING;
            self.pending.drain(..extra);
            self.gap = true;
        }
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        Frame(out)
            .u32(self.pending.len() as u32)
            .bytes4([u8::from(self.gap), 0, 0, 0])
            .f32s(self.pending.drain(..));
        self.gap = false;
        true
    }
}
