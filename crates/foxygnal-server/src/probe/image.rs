//! Image: the picture rows on a `u8` wire that carries raster records (an
//! SSTV decoder's `image` output, see `foxygnal_core::raster`).
//!
//! Like Messages: each frame carries every whole row since the last one, and
//! it starts with whatever the ring still holds, so opening the instrument
//! late still shows the picture so far. Bytes that aren't records are
//! skipped; after a gap the reader finds the next record by its header.

use foxygnal_core::raster::{self, Next};

use super::Instrument;
use super::source::{Frame, Source};

/// At most this many bytes of rows wait for the next frame; past that the
/// oldest rows are dropped and the frame is marked as a gap.
const MAX_PENDING: usize = 1 << 22;

pub struct Image {
    /// Bytes read but not parsed yet (a record cut by the end of a read).
    buf: Vec<u8>,
    /// Whole records waiting for a frame, each padded to 4 bytes.
    pending: Vec<u8>,
    rows: u32,
    gap: bool,
    started: bool,
}

impl Image {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pending: Vec::new(),
            rows: 0,
            gap: false,
            started: false,
        }
    }

    fn take(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        let mut at = 0;
        loop {
            match raster::next(&self.buf[at..]) {
                Next::Row(_, len) => {
                    self.pending.extend_from_slice(&self.buf[at..at + len]);
                    while !self.pending.len().is_multiple_of(4) {
                        self.pending.push(0);
                    }
                    self.rows += 1;
                    at += len;
                }
                Next::Skip(k) => at += k,
                Next::More => break,
            }
        }
        self.buf.drain(..at);
        if self.pending.len() > MAX_PENDING {
            self.drop_oldest();
        }
    }

    /// Drop whole rows from the front until under the limit.
    fn drop_oldest(&mut self) {
        let mut at = 0;
        while self.pending.len() - at > MAX_PENDING / 2 {
            let Next::Row(_, len) = raster::next(&self.pending[at..]) else {
                break;
            };
            at += len.next_multiple_of(4);
            self.rows -= 1;
        }
        self.pending.drain(..at);
        self.gap = true;
    }
}

impl Instrument for Image {
    fn poll(&mut self, src: &mut Source) {
        if !self.started {
            self.started = true;
            src.rewind();
        }
        let (contiguous, bytes) = src.fresh_bytes(MAX_PENDING);
        if !contiguous {
            self.buf.clear();
            self.gap = true;
        }
        self.take(bytes);
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.rows == 0 && !self.gap {
            return false;
        }
        Frame(out)
            .u32(self.rows)
            .bytes4([u8::from(self.gap), 0, 0, 0]);
        out.append(&mut self.pending);
        self.rows = 0;
        self.gap = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foxygnal_core::raster::Row;

    fn record(row: u16, width: u16) -> Vec<u8> {
        let r = Row {
            picture: 1,
            row,
            height: 4,
            width,
            channels: 3,
            flags: 0,
        };
        let mut v = r.header().to_vec();
        v.extend(std::iter::repeat_n(row as u8, r.pixel_bytes()));
        v
    }

    #[test]
    fn sends_whole_padded_rows() {
        let mut img = Image::new();
        let mut bytes = b"noise".to_vec();
        bytes.extend(record(0, 3));
        bytes.extend(record(1, 3));
        img.take(&bytes[..28]);
        assert_eq!(img.rows, 0, "the first record isn't complete yet");
        img.take(&bytes[28..]);
        assert_eq!(img.rows, 2);
        let mut out = Vec::new();
        assert!(img.frame(&mut out));
        assert_eq!(&out[..4], &2u32.to_le_bytes());
        assert_eq!(out[4], 0, "no gap");
        // Each 25-byte record is padded to 28.
        assert_eq!(out.len(), 8 + 2 * 28);
        assert_eq!(&out[8..12], b"FXIM");
        assert_eq!(&out[36..40], b"FXIM");
        out.clear();
        assert!(!img.frame(&mut out), "nothing new");
    }
}
