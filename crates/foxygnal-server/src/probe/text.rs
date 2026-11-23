//! Text: the lines of a `u8` wire that carries UTF-8 text, such as a
//! decoder's messages (one per line, see `foxygnal_blocks::digital`).
//!
//! Lossless while it keeps up, like audio: each frame carries every complete
//! line since the last one. It starts with whatever the ring still holds, so
//! opening the instrument late still shows recent messages. Only whole lines
//! are sent; after a gap the partial line is dropped and the frame says so.

use super::Instrument;
use super::source::{Frame, Source};

/// At most this many bytes wait for the next frame; past that the oldest
/// lines are dropped and the frame is marked as a gap.
const MAX_PENDING: usize = 1 << 18;

pub struct Text {
    /// Complete lines waiting for a frame.
    pending: Vec<u8>,
    /// The start of a line whose end hasn't arrived.
    partial: Vec<u8>,
    /// Drop bytes up to the next newline (we joined mid-line).
    skip_to_line: bool,
    gap: bool,
    started: bool,
}

impl Text {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            partial: Vec::new(),
            skip_to_line: false,
            gap: false,
            started: false,
        }
    }

    fn take(&mut self, mut bytes: &[u8]) {
        if self.skip_to_line {
            match bytes.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    bytes = &bytes[i + 1..];
                    self.skip_to_line = false;
                }
                None => return,
            }
        }
        match bytes.iter().rposition(|&b| b == b'\n') {
            Some(end) => {
                self.pending.append(&mut self.partial);
                self.pending.extend_from_slice(&bytes[..=end]);
                self.partial.extend_from_slice(&bytes[end + 1..]);
            }
            None => self.partial.extend_from_slice(bytes),
        }
        // A "line" this long isn't text: pass it on as it is.
        if self.partial.len() > MAX_PENDING / 4 {
            self.pending.append(&mut self.partial);
            self.pending.push(b'\n');
        }
        if self.pending.len() > MAX_PENDING {
            let cut = self.pending.len() - MAX_PENDING;
            let cut = self.pending[cut..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(self.pending.len(), |i| cut + i + 1);
            self.pending.drain(..cut);
            self.gap = true;
        }
    }
}

impl Instrument for Text {
    fn poll(&mut self, src: &mut Source) {
        if !self.started {
            self.started = true;
            // Joined mid-stream: the first line is probably cut.
            self.skip_to_line = !src.rewind();
        }
        let (contiguous, bytes) = src.fresh_bytes(MAX_PENDING);
        if !contiguous {
            self.partial.clear();
            self.skip_to_line = true;
            self.gap = true;
        }
        self.take(bytes);
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.pending.is_empty() && !self.gap {
            return false;
        }
        Frame(out)
            .u32(self.pending.len() as u32)
            .bytes4([u8::from(self.gap), 0, 0, 0]);
        out.append(&mut self.pending);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        self.gap = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_whole_lines_only() {
        let mut t = Text::new();
        t.take(b"one\ntw");
        assert_eq!(t.pending, b"one\n");
        t.take(b"o\nthr");
        assert_eq!(t.pending, b"one\ntwo\n");
        let mut out = Vec::new();
        assert!(t.frame(&mut out));
        assert_eq!(&out[..4], &8u32.to_le_bytes());
        assert_eq!(out[4], 0, "no gap");
        assert_eq!(&out[8..16], b"one\ntwo\n");
        out.clear();
        assert!(!t.frame(&mut out), "nothing new");
    }

    #[test]
    fn joining_mid_line_skips_to_the_next_line() {
        let mut t = Text::new();
        t.skip_to_line = true;
        t.take(b"ut off\nwhole\n");
        assert_eq!(t.pending, b"whole\n");
    }
}
