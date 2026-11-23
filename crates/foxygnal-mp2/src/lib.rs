//! A small MPEG audio decoder for Layers I and II (MP1 and MP2: ISO/IEC
//! 11172-3, and the lower sample rates of 13818-3 and "MPEG-2.5"): mono,
//! stereo, joint (intensity) stereo and dual channel, with the CRC checked
//! when a frame has one. Layer III and free format aren't decoded.
//!
//! It exists for the browser's Video instrument: MP2 is the sound of most
//! DVB services, and WebCodecs can't decode it. Built to WebAssembly
//! (`scripts/build-mpeg2-wasm.sh`) it imports nothing from its host; see the
//! `wasm` module for its C ABI. The decoder itself is `no_std` and never
//! allocates: [`Stream`] is one fixed-size struct.
//!
//! [`Decoder`] decodes whole frames. [`Stream`] takes elementary stream
//! bytes as they come (with a PES packet's PTS), finds frames, and gives
//! back PCM with a timestamp per frame.

#![cfg_attr(not(any(test, target_arch = "wasm32")), no_std)]

mod frame;
#[cfg(test)]
mod reference;
mod tables;
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use frame::{Decoded, Decoder, Error, Header, Mode, Version};

const PTS_MASK: u64 = (1 << 33) - 1;

/// The largest frame: Layer II at 160 kbit/s and 8 kHz.
pub const MAX_FRAME: usize = 2881;
/// The most values a frame decodes to: 1152 samples, two channels.
pub const MAX_PCM: usize = 2304;

const BUF: usize = 4 * MAX_FRAME;
const MARKS: usize = 16;

/// A decoded frame, valid until the next call on its [`Stream`].
#[derive(Debug)]
pub struct Frame<'a> {
    pub header: Header,
    /// Interleaved, in ±1 (full scale), `header.samples()` per channel.
    pub pcm: &'a [f32],
    /// 90 kHz, 33 bits: the PTS of the PES the frame starts in, or the
    /// previous frame's plus its duration.
    pub pts: Option<u64>,
}

/// Finds frames in an elementary stream and decodes them.
///
/// [`push`](Stream::push) bytes, then call [`decode`](Stream::decode) until
/// it returns `None`. Until the decoder has found its feet (at the start and
/// after a [`reset`](Stream::reset) or lost sync) a frame is only taken when
/// the next one follows where its header says; after that, every frame of
/// the same version, layer and sample rate is.
pub struct Stream {
    dec: Decoder,
    buf: [u8; BUF],
    start: usize,
    end: usize,
    /// Stream offset of `buf[0]`.
    base: u64,
    /// (stream offset, PTS) of PES starts not reached yet.
    marks: [(u64, u64); MARKS],
    nmarks: usize,
    /// The next frame's PTS if none is given, in 90 kHz ticks.
    next_pts: Option<f64>,
    /// Version, layer and sample rate of the frames being decoded.
    locked: Option<(Version, u8, u32)>,
    eof: bool,
    out: [f32; MAX_PCM],
    errors: u32,
    frames: u32,
    last: Option<Header>,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Self {
        Stream {
            dec: Decoder::new(),
            buf: [0; BUF],
            start: 0,
            end: 0,
            base: 0,
            marks: [(0, 0); MARKS],
            nmarks: 0,
            next_pts: None,
            locked: None,
            eof: false,
            out: [0.0; MAX_PCM],
            errors: 0,
            frames: 0,
            last: None,
        }
    }

    /// Take as much of `data` as fits (all of it, if frames are decoded in
    /// between) and return how much that was. `pts` (90 kHz) belongs to the
    /// first frame that starts in `data`.
    pub fn push(&mut self, data: &[u8], pts: Option<u64>) -> usize {
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.base += self.start as u64;
            self.end -= self.start;
            self.start = 0;
        }
        let n = data.len().min(BUF - self.end);
        if n == 0 {
            return 0;
        }
        if let Some(pts) = pts {
            if self.nmarks == MARKS {
                self.marks.copy_within(1.., 0);
                self.nmarks -= 1;
            }
            self.marks[self.nmarks] = (self.base + self.end as u64, pts & PTS_MASK);
            self.nmarks += 1;
        }
        self.buf[self.end..self.end + n].copy_from_slice(&data[..n]);
        self.end += n;
        self.eof = false;
        n
    }

    /// No more input is coming: the last frame needn't be followed by another.
    pub fn flush(&mut self) {
        self.eof = true;
    }

    /// After a gap: drop buffered input and filter history, and find the
    /// frame grid again.
    pub fn reset(&mut self) {
        self.base += self.end as u64;
        self.start = 0;
        self.end = 0;
        self.nmarks = 0;
        self.next_pts = None;
        self.locked = None;
        self.dec.reset();
    }

    /// Frames that failed their CRC (decoded as silence), and times sync
    /// was lost.
    pub fn errors(&self) -> u32 {
        self.errors
    }

    /// Frames decoded.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The header of the last frame seen, decoded or not (Layer III shows
    /// up here, too).
    pub fn last_header(&self) -> Option<Header> {
        self.last
    }

    /// The next frame, or `None` if more input is needed.
    pub fn decode(&mut self) -> Option<Frame<'_>> {
        loop {
            let avail = &self.buf[self.start..self.end];
            if avail.len() < 4 {
                return None;
            }
            let h = Header::parse(avail).filter(|h| {
                self.locked
                    .is_none_or(|l| l == (h.version, h.layer, h.sample_rate))
            });
            let Some(h) = h else {
                if self.locked.take().is_some() {
                    self.errors += 1;
                }
                self.start += 1;
                continue;
            };
            let len = h.frame_len();
            if avail.len() < len {
                return None;
            }
            if self.locked.is_none() {
                // Check that the next frame is where this one says it is.
                if avail.len() < len + 4 {
                    if !self.eof {
                        return None;
                    }
                } else if Header::parse(&avail[len..]).is_none_or(|n| {
                    (n.version, n.layer, n.sample_rate) != (h.version, h.layer, h.sample_rate)
                }) {
                    self.start += 1;
                    continue;
                }
            }
            self.locked = Some((h.version, h.layer, h.sample_rate));
            self.last = Some(h);
            let at = self.base + self.start as u64;
            self.start += len;
            // The PTS of the last PES that started at or before this frame.
            let mut pts = None;
            while self.nmarks > 0 && self.marks[0].0 <= at {
                pts = Some(self.marks[0].1 as f64);
                self.marks.copy_within(1..self.nmarks, 0);
                self.nmarks -= 1;
            }
            let pts = pts.or(self.next_pts);
            let frame = &self.buf[self.start - len..self.start];
            match self.dec.decode(frame, &mut self.out) {
                Ok(d) => {
                    if d.crc_error {
                        self.errors += 1;
                    }
                    self.frames += 1;
                    let n = h.samples();
                    self.next_pts = pts.map(|p| p + (n * 90000) as f64 / h.sample_rate as f64);
                    return Some(Frame {
                        header: h,
                        pcm: &self.out[..n * h.channels()],
                        pts: pts.map(|p| (p + 0.5) as u64 & PTS_MASK),
                    });
                }
                // Layer III: skipped (whole frames), its timing kept.
                Err(_) => {
                    self.next_pts =
                        pts.map(|p| p + (h.samples() * 90000) as f64 / h.sample_rate as f64);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_is_small_and_never_panics_on_garbage() {
        assert!(core::mem::size_of::<Stream>() < 64 * 1024);
        let mut s = Stream::new();
        let mut x = 0x9E37_79B9u32;
        for round in 0..200 {
            let mut chunk = [0u8; 700];
            for b in chunk.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
                // Plenty of sync-like bytes.
                if x.is_multiple_of(7) {
                    *b = 0xFF;
                }
            }
            let mut at = 0;
            while at < chunk.len() {
                at += s.push(&chunk[at..], Some(round * 1000));
                while let Some(f) = s.decode() {
                    assert!(f.pcm.iter().all(|v| v.is_finite()));
                }
            }
        }
    }
}
