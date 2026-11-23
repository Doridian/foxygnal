//! Reading a tap as complex samples, and writing binary frames.

use foxygnal_core::{Complex32, StreamType, Tap};

/// A probe's view of one output: a lossy tap plus a read cursor. Every type is
/// read as complex samples (real types have `im = 0`); `i16` and `u8` are
/// scaled so that full scale is ±1.
pub struct Source {
    tap: Tap,
    ty: StreamType,
    cursor: usize,
    bytes: Vec<u8>,
}

impl Source {
    /// Start reading at the stream's current end.
    pub fn new(tap: Tap, ty: StreamType) -> Self {
        let cursor = tap.written();
        Self::new_at(tap, ty, cursor)
    }

    /// A second reader of the same tap, at the same position.
    pub fn detach(&self) -> Self {
        Self::new_at(self.tap.clone(), self.ty, self.cursor)
    }

    fn new_at(tap: Tap, ty: StreamType, cursor: usize) -> Self {
        Self {
            cursor,
            tap,
            ty,
            bytes: Vec::new(),
        }
    }

    /// The ring's capacity in items.
    pub fn capacity(&self) -> usize {
        self.tap.capacity()
    }

    /// Start from the oldest item the ring still holds instead of the end,
    /// so a probe opened late shows recent history. True if that is the
    /// very start of the stream.
    pub fn rewind(&mut self) -> bool {
        self.cursor = self.tap.written().saturating_sub(self.tap.capacity());
        self.cursor == 0
    }

    /// The raw bytes of the items written since the last call, at most `max`
    /// items (the newest), and whether they follow on from the last call
    /// without a gap.
    pub fn fresh_bytes(&mut self, max: usize) -> (bool, &[u8]) {
        let written = self.tap.written();
        let from = self.cursor.max(written.saturating_sub(max));
        let size = self.tap.item_size();
        self.bytes
            .resize((written - from).min(self.tap.capacity()) * size, 0);
        let got = self.tap.read(from, &mut self.bytes);
        let contiguous = from == self.cursor && got.start == from;
        self.cursor = got.start + got.items;
        (contiguous, &self.bytes[..got.items * size])
    }

    /// Append the items written since the last call to `out`, at most `max`
    /// of them (the newest). Returns false if items were skipped since the
    /// last call: the probe fell behind, or the writer overwrote them.
    pub fn fresh(&mut self, max: usize, out: &mut Vec<Complex32>) -> bool {
        let written = self.tap.written();
        let from = self.cursor.max(written.saturating_sub(max));
        let mut contiguous = from == self.cursor;
        let size = self.tap.item_size();
        self.bytes
            .resize((written - from).min(self.tap.capacity()) * size, 0);
        let got = self.tap.read(from, &mut self.bytes);
        if got.start != from {
            contiguous = false;
        }
        convert(self.ty, &self.bytes[..got.items * size], out);
        self.cursor = got.start + got.items;
        contiguous
    }
}

fn convert(ty: StreamType, b: &[u8], out: &mut Vec<Complex32>) {
    let words = |n: usize| b.chunks_exact(n);
    match ty {
        StreamType::C32 => out.extend(words(8).map(|c| {
            Complex32::new(
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })),
        StreamType::F32 => out.extend(
            words(4).map(|c| Complex32::new(f32::from_le_bytes([c[0], c[1], c[2], c[3]]), 0.0)),
        ),
        StreamType::I16 => {
            out.extend(words(2).map(|c| {
                Complex32::new(f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0, 0.0)
            }))
        }
        StreamType::U8 => out.extend(
            b.iter()
                .map(|&v| Complex32::new((f32::from(v) - 127.5) / 127.5, 0.0)),
        ),
    }
}

/// Little-endian frame writer. Every section is padded to 4 bytes so the
/// browser can view `f32` arrays in place.
pub struct Frame<'a>(pub &'a mut Vec<u8>);

impl Frame<'_> {
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Four bytes, for flags and small fields.
    pub fn bytes4(&mut self, v: [u8; 4]) -> &mut Self {
        self.0.extend_from_slice(&v);
        self
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn f32s(&mut self, vs: impl IntoIterator<Item = f32>) -> &mut Self {
        for v in vs {
            self.f32(v);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_types_are_scaled_to_full_scale() {
        let mut out = Vec::new();
        convert(StreamType::I16, &(-32768i16).to_le_bytes(), &mut out);
        convert(StreamType::U8, &[255, 0], &mut out);
        let re: Vec<f32> = out.iter().map(|c| c.re).collect();
        assert_eq!(re, vec![-1.0, 1.0, -1.0]);
    }
}
