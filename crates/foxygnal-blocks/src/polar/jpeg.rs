//! The JPEG-like codec of Meteor-M's MSU-MR imager on LRPT: each image
//! packet carries a strip of 14 blocks of 8 × 8 pixels (8 rows × 112
//! pixels), coded as baseline JPEG without markers: an 8 × 8 DCT, the
//! standard luminance quantization table scaled by a quality factor carried
//! in the packet (the IJG formula), zigzag order, and the standard luminance
//! DC and AC Huffman tables (ITU-T T.81 Annex K). The DC prediction starts
//! at 0 in every packet; bits are read MSB first with no byte stuffing, and
//! whatever follows the 14th block is padding.
//!
//! [`decode_strip`] is the receive side; [`encode_strip`] the transmit side
//! for loopback tests. Checked against libjpeg (through Pillow): see
//! `scripts/lrpt-jpeg-vectors.py` and the tests below.

use std::sync::OnceLock;

/// Blocks per packet, and the strip's size in pixels.
pub const BLOCKS: usize = 14;
pub const STRIP_W: usize = BLOCKS * 8;
pub const STRIP_PIXELS: usize = STRIP_W * 8;

/// Zigzag position → natural (row-major) index.
pub const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The luminance quantization table of T.81 Annex K (natural order).
pub const STD_QUANT: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Huffman table specs of T.81 Annex K: code counts per length 1–16, then
/// the symbols in code order.
const DC_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];
const AC_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];

/// A canonical Huffman table: for decoding, the first code and the index of
/// its symbol per length; for encoding, each symbol's code and length.
struct Huffman {
    /// Per length 1–16: the largest code + 1 of that length (exclusive end),
    /// the first code, and the index of its symbol in `vals`.
    end: [u32; 17],
    first: [u32; 17],
    index: [u16; 17],
    vals: &'static [u8],
    code: [u16; 256],
    len: [u8; 256],
}

impl Huffman {
    fn new(bits: &[u8; 16], vals: &'static [u8]) -> Self {
        let mut h = Self {
            end: [0; 17],
            first: [0; 17],
            index: [0; 17],
            vals,
            code: [0; 256],
            len: [0; 256],
        };
        let mut code = 0u32;
        let mut k = 0usize;
        for l in 1..=16 {
            let n = usize::from(bits[l - 1]);
            h.first[l] = code;
            h.index[l] = k as u16;
            for _ in 0..n {
                let v = usize::from(vals[k]);
                h.code[v] = code as u16;
                h.len[v] = l as u8;
                code += 1;
                k += 1;
            }
            h.end[l] = code;
            code <<= 1;
        }
        h
    }
}

struct Tables {
    dc: Huffman,
    ac: Huffman,
    /// `cos((2x + 1)uπ/16) · c(u) / 2`, index `[u][x]`.
    basis: [[f32; 8]; 8],
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut basis = [[0.0f32; 8]; 8];
        for (u, row) in basis.iter_mut().enumerate() {
            let c = if u == 0 {
                std::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            for (x, v) in row.iter_mut().enumerate() {
                let a = (2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0;
                *v = (a.cos() * c / 2.0) as f32;
            }
        }
        Tables {
            dc: Huffman::new(&DC_BITS, &DC_VALS),
            ac: Huffman::new(&AC_BITS, &AC_VALS),
            basis,
        }
    })
}

/// The quantization table for quality `q` (1–100), natural order: the
/// standard table scaled with the IJG formula, at least 1.
pub fn quant_table(q: u8) -> [u16; 64] {
    let q = u32::from(q.clamp(1, 100));
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    STD_QUANT.map(|s| ((u32::from(s) * scale + 50) / 100).max(1) as u16)
}

/// MSB-first bit reader over a byte slice.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    #[inline]
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos >> 3)?;
        let b = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Some(u32::from(b))
    }

    #[inline]
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn symbol(&mut self, h: &Huffman) -> Option<u8> {
        let mut code = 0u32;
        for l in 1..=16 {
            code = (code << 1) | self.bit()?;
            if code < h.end[l] && code >= h.first[l] && h.end[l] > h.first[l] {
                return Some(h.vals[usize::from(h.index[l]) + (code - h.first[l]) as usize]);
            }
        }
        None
    }
}

/// JPEG's sign extension of an `n`-bit magnitude category value.
#[inline]
fn extend(v: u32, n: u32) -> i32 {
    if n == 0 {
        0
    } else if v < (1 << (n - 1)) {
        v as i32 - (1 << n) + 1
    } else {
        v as i32
    }
}

/// Decode a packet's coded data into an 8 × 112 strip (`out`, row-major).
/// Returns the number of blocks decoded (14 if all went well); the blocks
/// after a coding error are left untouched.
pub fn decode_strip(data: &[u8], quality: u8, out: &mut [u8; STRIP_PIXELS]) -> usize {
    let t = tables();
    let quant = quant_table(quality);
    let mut bits = Bits { data, pos: 0 };
    let mut dc = 0i32;
    for block in 0..BLOCKS {
        let mut coef = [0f32; 64];
        let Some(cat) = bits.symbol(&t.dc) else {
            return block;
        };
        if cat > 11 {
            return block;
        }
        let Some(v) = bits.bits(u32::from(cat)) else {
            return block;
        };
        dc += extend(v, u32::from(cat));
        coef[0] = dc as f32 * f32::from(quant[0]);
        let mut k = 1;
        while k < 64 {
            let Some(rs) = bits.symbol(&t.ac) else {
                return block;
            };
            let (run, size) = (usize::from(rs >> 4), u32::from(rs & 15));
            if size == 0 {
                if run == 15 {
                    k += 16;
                    continue;
                }
                break; // EOB
            }
            k += run;
            if k > 63 {
                return block;
            }
            let Some(v) = bits.bits(size) else {
                return block;
            };
            let z = usize::from(ZIGZAG[k]);
            coef[z] = extend(v, size) as f32 * f32::from(quant[z]);
            k += 1;
        }
        if k > 64 {
            return block;
        }
        idct_block(&coef, &t.basis, out, block * 8);
    }
    BLOCKS
}

/// Inverse DCT of one block into the strip at column `x0`, +128, clamped.
fn idct_block(coef: &[f32; 64], basis: &[[f32; 8]; 8], out: &mut [u8; STRIP_PIXELS], x0: usize) {
    // Rows of coefficients (v) → columns transformed first.
    let mut tmp = [0f32; 64];
    for v in 0..8 {
        for x in 0..8 {
            let mut s = 0.0;
            for u in 0..8 {
                s += coef[v * 8 + u] * basis[u][x];
            }
            tmp[v * 8 + x] = s;
        }
    }
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0.0;
            for v in 0..8 {
                s += tmp[v * 8 + x] * basis[v][y];
            }
            out[y * STRIP_W + x0 + x] = (s + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// MSB-first bit writer.
struct Writer<'a> {
    out: &'a mut Vec<u8>,
    acc: u32,
    n: u32,
}

impl Writer<'_> {
    fn put(&mut self, v: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((v >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// Pad the last byte with ones.
    fn finish(&mut self) {
        while self.n != 0 {
            self.put(1, 1);
        }
    }
}

/// The magnitude category of a coefficient and its JPEG bits.
fn category(v: i32) -> (u32, u32) {
    let n = 32 - v.unsigned_abs().leading_zeros();
    let bits = if v < 0 { (v - 1) as u32 } else { v as u32 };
    (n, bits & ((1u32 << n) - 1))
}

/// Encode an 8 × 112 strip (row-major) at quality `quality`, appending the
/// coded bytes to `out` (the transmit side, for loopback tests).
pub fn encode_strip(pixels: &[u8; STRIP_PIXELS], quality: u8, out: &mut Vec<u8>) {
    let t = tables();
    let quant = quant_table(quality);
    let mut w = Writer { out, acc: 0, n: 0 };
    let mut prev_dc = 0i32;
    for block in 0..BLOCKS {
        // Forward DCT: F(v,u) = Σ f(y,x) b[u][x] b[v][y].
        let mut tmp = [0f32; 64];
        for y in 0..8 {
            for u in 0..8 {
                let mut s = 0.0;
                for x in 0..8 {
                    let p = f32::from(pixels[y * STRIP_W + block * 8 + x]) - 128.0;
                    s += p * t.basis[u][x];
                }
                tmp[y * 8 + u] = s;
            }
        }
        let mut q = [0i32; 64];
        for (v, bv) in t.basis.iter().enumerate() {
            for u in 0..8 {
                let mut s = 0.0;
                for (y, b) in bv.iter().enumerate() {
                    s += tmp[y * 8 + u] * b;
                }
                let i = v * 8 + u;
                q[i] = (s / f32::from(quant[i])).round() as i32;
            }
        }
        let dc = q[0];
        let (n, b) = category(dc - prev_dc);
        prev_dc = dc;
        let h = &t.dc;
        w.put(u32::from(h.code[n as usize]), u32::from(h.len[n as usize]));
        w.put(b, n);
        let h = &t.ac;
        let mut run = 0;
        for k in 1..64 {
            let v = q[usize::from(ZIGZAG[k])];
            if v == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                w.put(u32::from(h.code[0xF0]), u32::from(h.len[0xF0]));
                run -= 16;
            }
            let (n, b) = category(v);
            let rs = (run << 4) | n as usize;
            w.put(u32::from(h.code[rs]), u32::from(h.len[rs]));
            w.put(b, n);
            run = 0;
        }
        if run > 0 {
            w.put(u32::from(h.code[0]), u32::from(h.len[0]));
        }
    }
    w.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_complete() {
        assert_eq!(AC_BITS.iter().map(|&b| usize::from(b)).sum::<usize>(), 162);
        assert_eq!(DC_BITS.iter().map(|&b| usize::from(b)).sum::<usize>(), 12);
        let t = tables();
        assert_eq!((t.dc.code[0], t.dc.len[0]), (0b00, 2));
        assert_eq!((t.ac.code[0], t.ac.len[0]), (0b1010, 4));
        assert_eq!((t.ac.code[0xF0], t.ac.len[0xF0]), (0b111_1111_1001, 11));
        assert_eq!(quant_table(50), STD_QUANT);
        assert_eq!(quant_table(100), [1; 64]);
        assert_eq!(quant_table(25)[0], 32);
    }

    fn pattern(seed: u32) -> [u8; STRIP_PIXELS] {
        let mut p = [0u8; STRIP_PIXELS];
        for (i, v) in p.iter_mut().enumerate() {
            let (x, y) = ((i % STRIP_W) as f32, (i / STRIP_W) as f32);
            let f =
                128.0 + 90.0 * (x * 0.07 + seed as f32).sin() + 30.0 * (y * 0.6 + x * 0.01).cos();
            *v = f.clamp(0.0, 255.0) as u8;
        }
        p
    }

    #[test]
    fn round_trip() {
        for q in [20u8, 50, 80, 95] {
            let img = pattern(u32::from(q));
            let mut coded = Vec::new();
            encode_strip(&img, q, &mut coded);
            let mut out = [0u8; STRIP_PIXELS];
            assert_eq!(decode_strip(&coded, q, &mut out), BLOCKS);
            let err: f64 = img
                .iter()
                .zip(&out)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).abs())
                .sum::<f64>()
                / STRIP_PIXELS as f64;
            assert!(err < 6.0 - f64::from(q) / 20.0, "q {q}: mean error {err}");
        }
    }

    #[test]
    fn truncated_data_stops_at_the_bad_block() {
        let img = pattern(3);
        let mut coded = Vec::new();
        encode_strip(&img, 80, &mut coded);
        let mut out = [0u8; STRIP_PIXELS];
        let n = decode_strip(&coded[..coded.len() / 2], 80, &mut out);
        assert!(n > 0 && n < BLOCKS, "{n}");
    }

    /// Strips coded by libjpeg (Pillow) as 112 × 8 baseline greyscale JPEGs
    /// with the standard tables, their entropy-coded segment unstuffed: the
    /// same bitstream an MSU-MR packet carries. libjpeg's own decode of each
    /// is the reference (its integer IDCT rounds a little differently).
    #[test]
    fn decodes_libjpeg_strips() {
        let raw = include_bytes!("../../tests/fixtures/lrpt/jpeg-strips.bin");
        let mut at = 0;
        let mut n = 0;
        while at < raw.len() {
            let q = raw[at];
            let len = usize::from(u16::from_le_bytes([raw[at + 1], raw[at + 2]]));
            at += 3;
            let coded = &raw[at..at + len];
            at += len;
            let want = &raw[at..at + STRIP_PIXELS];
            at += STRIP_PIXELS;
            let mut out = [0u8; STRIP_PIXELS];
            assert_eq!(decode_strip(coded, q, &mut out), BLOCKS, "strip {n}");
            let worst = want
                .iter()
                .zip(&out)
                .map(|(&a, &b)| (i32::from(a) - i32::from(b)).abs())
                .max()
                .unwrap();
            assert!(worst <= 2, "strip {n} (q {q}): off by up to {worst}");
            n += 1;
        }
        assert!(n >= 8, "{n} strips");
    }
}
