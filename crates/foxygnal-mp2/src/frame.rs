//! One frame: the header, and Layer I and II decoding into subband samples,
//! then the polyphase synthesis filter bank.

use crate::tables::{self, ALLOC, BITRATES, CODE_BITS, RATES, STEPS};

/// MPEG audio version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// ISO/IEC 11172-3: 32, 44.1 and 48 kHz.
    Mpeg1,
    /// ISO/IEC 13818-3 lower sample rates: 16, 22.05 and 24 kHz.
    Mpeg2,
    /// The unofficial extension to 8, 11.025 and 12 kHz.
    Mpeg25,
}

/// Channel mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Stereo,
    JointStereo,
    /// Two independent channels (two languages, say); decoded as left and right.
    DualChannel,
    Mono,
}

/// A frame header (the 32 bits after and including the sync word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: Version,
    /// 1, 2 or 3.
    pub layer: u8,
    /// A CRC-16 follows the header.
    pub crc: bool,
    /// Bits per second.
    pub bitrate: u32,
    pub sample_rate: u32,
    pub padding: bool,
    pub mode: Mode,
    /// For joint stereo in Layers I and II: subbands from 4·(ext + 1) up
    /// share their samples (intensity stereo).
    pub mode_ext: u8,
}

impl Header {
    /// Parse the first four bytes of `b`. `None` for anything but a valid
    /// header. Free format (bitrate index 0) isn't supported.
    pub fn parse(b: &[u8]) -> Option<Header> {
        let &[b0, b1, b2, b3, ..] = b else {
            return None;
        };
        if b0 != 0xFF || b1 & 0xE0 != 0xE0 {
            return None;
        }
        let version = match (b1 >> 3) & 3 {
            0 => Version::Mpeg25,
            2 => Version::Mpeg2,
            3 => Version::Mpeg1,
            _ => return None,
        };
        let layer = match (b1 >> 1) & 3 {
            1 => 3,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let bi = (b2 >> 4) as usize;
        let si = ((b2 >> 2) & 3) as usize;
        if bi == 0 || bi == 15 || si == 3 {
            return None;
        }
        let table = match (version, layer) {
            (Version::Mpeg1, l) => l as usize - 1,
            (_, 1) => 3,
            _ => 4,
        };
        let sample_rate = match version {
            Version::Mpeg1 => RATES[si],
            Version::Mpeg2 => RATES[si] / 2,
            Version::Mpeg25 => RATES[si] / 4,
        };
        Some(Header {
            version,
            layer,
            crc: b1 & 1 == 0,
            bitrate: u32::from(BITRATES[table][bi]) * 1000,
            sample_rate,
            padding: b2 & 2 != 0,
            mode: match b3 >> 6 {
                0 => Mode::Stereo,
                1 => Mode::JointStereo,
                2 => Mode::DualChannel,
                _ => Mode::Mono,
            },
            mode_ext: (b3 >> 4) & 3,
        })
    }

    /// Lower sample rates (MPEG-2 or 2.5).
    pub fn lsf(&self) -> bool {
        self.version != Version::Mpeg1
    }

    pub fn channels(&self) -> usize {
        if self.mode == Mode::Mono { 1 } else { 2 }
    }

    /// Samples per channel.
    pub fn samples(&self) -> usize {
        match self.layer {
            1 => 384,
            3 if self.lsf() => 576,
            _ => 1152,
        }
    }

    /// Bytes in the frame, header included.
    pub fn frame_len(&self) -> usize {
        let (br, fs, pad) = (
            self.bitrate as usize,
            self.sample_rate as usize,
            usize::from(self.padding),
        );
        match self.layer {
            1 => (12 * br / fs + pad) * 4,
            3 if self.lsf() => 72 * br / fs + pad,
            _ => 144 * br / fs + pad,
        }
    }

    /// The first subband that is coded jointly.
    fn bound(&self, sblimit: usize) -> usize {
        match self.mode {
            Mode::JointStereo => (4 * (self.mode_ext as usize + 1)).min(sblimit),
            _ => sblimit,
        }
    }
}

/// Why a frame couldn't be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a valid header.
    Header,
    /// Fewer bytes than the header says.
    Truncated,
    /// Layer III (or free format), which this decoder doesn't do.
    Unsupported,
}

/// A big-endian bit reader; past the end it reads zeros.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    /// The next `n` (0..=16) bits.
    #[inline]
    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let i = self.pos >> 3;
        let byte = |k: usize| u32::from(self.data.get(i + k).copied().unwrap_or(0));
        let w = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        let v = (w << (8 + (self.pos & 7))) >> (32 - n);
        self.pos += n as usize;
        v
    }
}

/// CRC-16 (x¹⁶ + x¹⁵ + x² + 1) over `n` bits of `data` from bit `from`.
pub(crate) fn crc16(mut crc: u16, data: &[u8], from: usize, n: usize) -> u16 {
    for p in from..from + n {
        let bit = (data.get(p >> 3).copied().unwrap_or(0) >> (7 - (p & 7))) & 1;
        let msb = (crc >> 15) as u8 ^ bit;
        crc <<= 1;
        if msb != 0 {
            crc ^= 0x8005;
        }
    }
    crc
}

/// What [`Decoder::decode`] found.
#[derive(Debug, Clone, Copy)]
pub struct Decoded {
    pub header: Header,
    /// The frame's CRC didn't match: the output is silence.
    pub crc_error: bool,
}

/// Decodes whole frames to PCM. Keeps the synthesis filter's history
/// between frames, so feed it consecutive frames of one stream.
pub struct Decoder {
    // Tables, computed once.
    n: [[f32; 32]; 64],
    d: [f32; 512],
    scale: [f32; 64],
    // Per channel: the synthesis FIFO V (a ring of 1024) and its start.
    v: [[f32; 1024]; 2],
    v_at: usize,
    // Subband samples of the frame being decoded: [channel][time][subband].
    sb: [[[f32; 32]; 36]; 2],
    alloc: [[u8; 32]; 2],
    scfsi: [[u8; 32]; 2],
    scf: [[[u8; 3]; 32]; 2],
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            n: tables::matrix(),
            d: tables::window(),
            scale: tables::scalefactors(),
            v: [[0.0; 1024]; 2],
            v_at: 0,
            sb: [[[0.0; 32]; 36]; 2],
            alloc: [[0; 32]; 2],
            scfsi: [[0; 32]; 2],
            scf: [[[0; 3]; 32]; 2],
        }
    }

    /// Forget the filter history (after a gap).
    pub fn reset(&mut self) {
        self.v = [[0.0; 1024]; 2];
    }

    /// Decode one whole frame (`frame` starts with its header) into `out`,
    /// interleaved, `samples() × channels()` values in ±1. `out` must hold
    /// at least 2304. A frame whose CRC fails comes out as silence.
    pub fn decode(&mut self, frame: &[u8], out: &mut [f32]) -> Result<Decoded, Error> {
        let h = Header::parse(frame).ok_or(Error::Header)?;
        if h.layer == 3 {
            return Err(Error::Unsupported);
        }
        if frame.len() < h.frame_len() {
            return Err(Error::Truncated);
        }
        let frame = &frame[..h.frame_len()];
        let ok = match h.layer {
            1 => self.layer1(&h, frame),
            _ => self.layer2(&h, frame),
        };
        if !ok {
            for ch in &mut self.sb {
                for t in ch.iter_mut() {
                    *t = [0.0; 32];
                }
            }
        }
        let nch = h.channels();
        let slots = h.samples() / 32;
        for t in 0..slots {
            // Both channels move the ring together.
            self.v_at = (self.v_at + 1024 - 64) & 1023;
            for ch in 0..nch {
                let o = &mut out[(t * 32 * nch)..][..32 * nch];
                self.synth(ch, t, o, nch);
            }
        }
        Ok(Decoded {
            header: h,
            crc_error: !ok,
        })
    }

    /// Check the CRC (if any) over the first `bits` bits after the CRC word.
    fn crc_ok(h: &Header, frame: &[u8], bits: usize) -> bool {
        if !h.crc {
            return true;
        }
        let c = crc16(0xFFFF, frame, 16, 16);
        let c = crc16(c, frame, 48, bits);
        c == u16::from_be_bytes([frame[4], frame[5]])
    }

    fn layer1(&mut self, h: &Header, frame: &[u8]) -> bool {
        let nch = h.channels();
        let bound = h.bound(32);
        let mut b = Bits {
            data: frame,
            pos: if h.crc { 48 } else { 32 },
        };
        for sb in 0..32 {
            for ch in 0..nch {
                self.alloc[ch][sb] = if sb < bound || ch == 0 {
                    b.read(4) as u8
                } else {
                    self.alloc[0][sb]
                };
                if self.alloc[ch][sb] == 15 {
                    return false;
                }
            }
        }
        if !Self::crc_ok(h, frame, 4 * (nch * bound + (32 - bound))) {
            return false;
        }
        for sb in 0..32 {
            for ch in 0..nch {
                if self.alloc[ch][sb] != 0 {
                    self.scf[ch][sb][0] = b.read(6) as u8;
                }
            }
        }
        for t in 0..12 {
            for sb in 0..32 {
                let mut x = 0.0;
                for ch in 0..nch {
                    let a = u32::from(self.alloc[ch][sb]);
                    if a == 0 {
                        self.sb[ch][t][sb] = 0.0;
                        continue;
                    }
                    if sb < bound || ch == 0 {
                        let nb = a + 1;
                        let steps = ((1u32 << nb) - 1) as f32;
                        let v = b.read(nb) as f32;
                        x = (2.0 * v - (steps - 1.0)) / steps;
                    }
                    self.sb[ch][t][sb] = x * self.scale[self.scf[ch][sb][0] as usize];
                }
            }
        }
        true
    }

    fn layer2(&mut self, h: &Header, frame: &[u8]) -> bool {
        let nch = h.channels();
        let kbps = h.bitrate / 1000 / nch as u32;
        let table = ALLOC[tables::alloc_table(h.lsf(), h.sample_rate, kbps)];
        // The classes per subband, and nbal.
        let mut classes: [&[u8]; 32] = [&[]; 32];
        let mut sblimit = 0;
        for &(count, c) in table {
            for _ in 0..count {
                classes[sblimit] = c;
                sblimit += 1;
            }
        }
        let nbal = |c: &[u8]| (c.len() + 1).trailing_zeros();
        let bound = h.bound(sblimit);
        let mut b = Bits {
            data: frame,
            pos: if h.crc { 48 } else { 32 },
        };
        for sb in 0..sblimit {
            for ch in 0..nch {
                self.alloc[ch][sb] = if sb < bound || ch == 0 {
                    b.read(nbal(classes[sb])) as u8
                } else {
                    self.alloc[0][sb]
                };
            }
        }
        for sb in 0..sblimit {
            for ch in 0..nch {
                if self.alloc[ch][sb] != 0 {
                    self.scfsi[ch][sb] = b.read(2) as u8;
                }
            }
        }
        if !Self::crc_ok(h, frame, b.pos - if h.crc { 48 } else { 32 }) {
            return false;
        }
        for sb in 0..sblimit {
            for ch in 0..nch {
                if self.alloc[ch][sb] == 0 {
                    continue;
                }
                let s = &mut self.scf[ch][sb];
                match self.scfsi[ch][sb] {
                    0 => {
                        s[0] = b.read(6) as u8;
                        s[1] = b.read(6) as u8;
                        s[2] = b.read(6) as u8;
                    }
                    1 => {
                        s[0] = b.read(6) as u8;
                        s[1] = s[0];
                        s[2] = b.read(6) as u8;
                    }
                    2 => {
                        s[0] = b.read(6) as u8;
                        s[1] = s[0];
                        s[2] = s[0];
                    }
                    _ => {
                        s[0] = b.read(6) as u8;
                        s[1] = b.read(6) as u8;
                        s[2] = s[1];
                    }
                }
            }
        }
        for gr in 0..12 {
            for sb in 0..32 {
                let mut x = [0f32; 3];
                for ch in 0..nch {
                    let a = if sb < sblimit {
                        self.alloc[ch][sb] as usize
                    } else {
                        0
                    };
                    if a == 0 {
                        for s in 0..3 {
                            self.sb[ch][gr * 3 + s][sb] = 0.0;
                        }
                        continue;
                    }
                    if sb < bound || ch == 0 {
                        let q = classes[sb][a - 1] as usize;
                        let steps = u32::from(STEPS[q]);
                        let bits = CODE_BITS[q];
                        let mut v = [0u32; 3];
                        if bits < 0 {
                            let mut c = b.read(-bits as u32);
                            for v in &mut v {
                                *v = c % steps;
                                c /= steps;
                            }
                        } else {
                            for v in &mut v {
                                *v = b.read(bits as u32);
                            }
                        }
                        let steps = steps as f32;
                        for s in 0..3 {
                            x[s] = (2.0 * v[s] as f32 - (steps - 1.0)) / steps;
                        }
                    }
                    let f = self.scale[self.scf[ch][sb][gr / 4] as usize];
                    for s in 0..3 {
                        self.sb[ch][gr * 3 + s][sb] = x[s] * f;
                    }
                }
            }
        }
        true
    }

    /// Synthesise 32 samples of channel `ch` from time slot `t` into every
    /// `nch`-th value of `out` (11172-3 Figure A.2).
    fn synth(&mut self, ch: usize, t: usize, out: &mut [f32], nch: usize) {
        let s = &self.sb[ch][t];
        let v = &mut self.v[ch];
        let at = self.v_at;
        for (i, row) in self.n.iter().enumerate() {
            let mut acc = 0.0f32;
            for k in 0..32 {
                acc += row[k] * s[k];
            }
            v[(at + i) & 1023] = acc;
        }
        for j in 0..32 {
            let mut acc = 0.0f32;
            for i in 0..8 {
                acc += v[(at + i * 128 + j) & 1023] * self.d[j + 64 * i];
                acc += v[(at + i * 128 + 96 + j) & 1023] * self.d[j + 64 * i + 32];
            }
            out[j * nch + ch] = acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        // MPEG-1 Layer II, 192 kbit/s, 48 kHz, stereo, no CRC.
        let h = Header::parse(&[0xFF, 0xFD, 0xA4, 0x00]).unwrap();
        assert_eq!(h.version, Version::Mpeg1);
        assert_eq!((h.layer, h.bitrate, h.sample_rate), (2, 192_000, 48000));
        assert_eq!((h.frame_len(), h.samples(), h.channels()), (576, 1152, 2));
        assert!(!h.crc);
        // MPEG-2 Layer II, 64 kbit/s, 22.05 kHz, mono, CRC, padded.
        let h = Header::parse(&[0xFF, 0xF4, 0x82, 0xC0]).unwrap();
        assert_eq!(h.version, Version::Mpeg2);
        assert_eq!(
            (h.bitrate, h.sample_rate, h.mode),
            (64_000, 22050, Mode::Mono)
        );
        assert!(h.crc && h.padding);
        assert_eq!(h.frame_len(), 144 * 64_000 / 22050 + 1);
        // Layer I, 448 kbit/s at 32 kHz.
        let h = Header::parse(&[0xFF, 0xFF, 0xE8, 0x00]).unwrap();
        assert_eq!((h.layer, h.bitrate, h.sample_rate), (1, 448_000, 32000));
        assert_eq!((h.frame_len(), h.samples()), (672, 384));
        // Free format, bad index, reserved layer and version.
        for bad in [
            [0xFF, 0xFD, 0x04, 0],
            [0xFF, 0xFD, 0xF4, 0],
            [0xFF, 0xFD, 0xAC, 0],
            [0xFF, 0xF9, 0xA4, 0],
            [0xFF, 0xED, 0xA4, 0],
            [0xFE, 0xFD, 0xA4, 0],
        ] {
            assert_eq!(Header::parse(&bad), None, "{bad:x?}");
        }
    }

    #[test]
    fn crc_of_known_bits() {
        // CRC-16/IBM-3740 style register (init 0xFFFF, poly 0x8005, no
        // reflection) over "123456789" is 0xAEE7.
        let c = crc16(0xFFFF, b"123456789", 0, 72);
        assert_eq!(c, 0xAEE7);
    }

    #[test]
    fn garbage_never_panics() {
        let mut d = Decoder::new();
        let mut out = [0f32; 2304];
        let mut x = 0x1234_5678u32;
        let mut buf = [0u8; 3000];
        for _ in 0..300 {
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
            // A valid header on random contents, of every kind.
            buf[0] = 0xFF;
            buf[1] |= 0xE0;
            let _ = d.decode(&buf, &mut out);
            assert!(out.iter().all(|v| v.is_finite()));
        }
    }
}
