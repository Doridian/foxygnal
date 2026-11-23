//! Constellation: the latest I/Q points. Persistence is drawn by the UI.

use foxygnal_core::Complex32;
use serde::Deserialize;

use super::Instrument;
use super::source::{Frame, Source};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ConstellationOptions {
    pub points: u32,
}

impl Default for ConstellationOptions {
    fn default() -> Self {
        Self { points: 1024 }
    }
}

pub struct Constellation {
    points: usize,
    latest: Vec<Complex32>,
}

impl Constellation {
    pub fn new(o: &ConstellationOptions) -> Self {
        Self {
            points: (o.points as usize).clamp(16, 16_384),
            latest: Vec::new(),
        }
    }
}

impl Instrument for Constellation {
    fn poll(&mut self, src: &mut Source) {
        src.fresh(self.points, &mut self.latest);
        if self.latest.len() > self.points {
            let drop = self.latest.len() - self.points;
            self.latest.drain(..drop);
        }
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.latest.is_empty() {
            return false;
        }
        Frame(out)
            .u32(self.latest.len() as u32)
            .f32s(self.latest.drain(..).flat_map(|x| [x.re, x.im]));
        true
    }
}
