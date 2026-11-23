//! Rows of bits sliced from a pulse train, and the integrity checks the
//! device formats use (CRCs, checksums, LFSR digests).
//!
//! Bits are stored most significant first within each byte: bit `i` of a row
//! is `row[i / 8] >> (7 - i % 8) & 1`, so the first bit received is the top
//! bit of byte 0.

/// Rows per buffer and bytes per row.
pub const ROWS: usize = 50;
pub const ROW_BYTES: usize = 128;
pub const ROW_BITS: usize = ROW_BYTES * 8;

/// Rows of bits (one per burst or repeat of a packet), fixed size so slicing
/// never allocates.
#[derive(Clone)]
pub struct BitBuffer {
    rows: [[u8; ROW_BYTES]; ROWS],
    lens: [u16; ROWS],
    /// Sync pulses seen before each row (PWM).
    syncs: [u16; ROWS],
    n: usize,
}

impl Default for BitBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl BitBuffer {
    pub const fn new() -> Self {
        Self {
            rows: [[0; ROW_BYTES]; ROWS],
            lens: [0; ROWS],
            syncs: [0; ROWS],
            n: 0,
        }
    }

    pub fn clear(&mut self) {
        // Only the rows in use can hold set bits.
        for r in 0..self.n.min(ROWS) {
            let used = usize::from(self.lens[r]).div_ceil(8);
            self.rows[r][..used].fill(0);
            self.lens[r] = 0;
            self.syncs[r] = 0;
        }
        self.n = 0;
    }

    /// Number of rows (an empty last row doesn't count).
    pub fn num_rows(&self) -> usize {
        if self.n > 0 && self.lens[self.n - 1] == 0 {
            self.n - 1
        } else {
            self.n
        }
    }

    /// Bits in row `r`.
    pub fn bits(&self, r: usize) -> usize {
        usize::from(self.lens[r])
    }

    /// The bytes of row `r` (trailing bits of the last byte are zero).
    pub fn row(&self, r: usize) -> &[u8] {
        &self.rows[r][..usize::from(self.lens[r]).div_ceil(8)]
    }

    /// The whole storage of row `r`, for reading a fixed number of bytes
    /// without a length check (bits past the end read as zero).
    pub fn row_full(&self, r: usize) -> &[u8; ROW_BYTES] {
        &self.rows[r]
    }

    pub fn syncs(&self, r: usize) -> usize {
        usize::from(self.syncs[r])
    }

    /// Append one bit to the current row (dropped once the row is full).
    pub fn push(&mut self, bit: bool) {
        if self.n == 0 {
            self.n = 1;
        }
        let r = self.n - 1;
        let len = usize::from(self.lens[r]);
        if len >= ROW_BITS {
            return;
        }
        if bit {
            self.rows[r][len / 8] |= 0x80 >> (len % 8);
        }
        self.lens[r] += 1;
    }

    /// Append `n` copies of `bit`.
    pub fn push_n(&mut self, bit: bool, n: usize) {
        for _ in 0..n.min(ROW_BITS) {
            self.push(bit);
        }
    }

    /// Start a new row (unless the current one is still empty).
    pub fn new_row(&mut self) {
        if self.n == 0 {
            self.n = 1;
        } else if self.lens[self.n - 1] > 0 && self.n < ROWS {
            self.n += 1;
        }
    }

    /// A sync pulse: starts a new row and counts it there.
    pub fn sync(&mut self) {
        self.new_row();
        if self.n == 0 {
            self.n = 1;
        }
        self.syncs[self.n - 1] += 1;
    }

    /// Append a whole row (for tests and derived buffers).
    pub fn push_row(&mut self, bytes: &[u8], bits: usize) {
        self.new_row();
        for i in 0..bits {
            self.push(bit(bytes, i));
        }
    }

    /// Invert every bit.
    pub fn invert(&mut self) {
        for r in 0..self.n {
            let len = usize::from(self.lens[r]);
            for (i, b) in self.rows[r][..len.div_ceil(8)].iter_mut().enumerate() {
                *b = !*b;
                if (i + 1) * 8 > len {
                    *b &= 0xFF << ((i + 1) * 8 - len);
                }
            }
        }
    }

    /// Bit offset of the first match of the top `pattern_bits` bits of
    /// `pattern` in row `r`, at or after `start`; the row length if none.
    pub fn search(&self, r: usize, start: usize, pattern: &[u8], pattern_bits: usize) -> usize {
        let len = self.bits(r);
        let row = &self.rows[r];
        let mut i = start;
        while i + pattern_bits <= len {
            if (0..pattern_bits).all(|k| bit(row, i + k) == bit(pattern, k)) {
                return i;
            }
            i += 1;
        }
        len
    }

    /// Copy `nbits` bits of row `r` from bit `pos` to `out`, packed from its
    /// first byte (bits past the row's end read as zero).
    pub fn extract(&self, r: usize, pos: usize, out: &mut [u8], nbits: usize) {
        extract_bits(&self.rows[r], pos, out, nbits);
    }

    /// The row that repeats most often with at least `min_bits` bits and
    /// `min_repeats` identical copies, or `None`.
    pub fn find_repeated_row(&self, min_repeats: usize, min_bits: usize) -> Option<usize> {
        let n = self.num_rows();
        for i in 0..n {
            if self.bits(i) < min_bits {
                continue;
            }
            let count = (i..n)
                .filter(|&j| self.bits(j) == self.bits(i) && self.row(j) == self.row(i))
                .count();
            if count >= min_repeats {
                return Some(i);
            }
        }
        None
    }

    /// Manchester-decode row `r` from bit `start` into row 0 of `out`
    /// (IEEE 802.3: `01` → 1, `10` → 0), stopping
    /// at the first invalid pair or after `max` bits. Returns the bit
    /// position where decoding stopped.
    pub fn manchester_decode(
        &self,
        r: usize,
        start: usize,
        out: &mut BitBuffer,
        max: usize,
    ) -> usize {
        let len = self.bits(r);
        let row = &self.rows[r];
        out.clear();
        out.new_row();
        let mut i = start;
        let mut n = 0;
        while i + 1 < len && n < max {
            match (bit(row, i), bit(row, i + 1)) {
                (false, true) => out.push(true),
                (true, false) => out.push(false),
                _ => break,
            }
            i += 2;
            n += 1;
        }
        i
    }
}

/// Bit `i` of `b` (most significant first).
#[inline]
pub fn bit(b: &[u8], i: usize) -> bool {
    b.get(i / 8).is_some_and(|&x| x >> (7 - i % 8) & 1 == 1)
}

/// Copy `nbits` bits of `src` from bit `pos` to `out`, packed from its first
/// byte; bits past the end of `src` read as zero.
pub fn extract_bits(src: &[u8], pos: usize, out: &mut [u8], nbits: usize) {
    let nbytes = nbits.div_ceil(8).min(out.len());
    out[..nbytes].fill(0);
    for k in 0..nbits.min(nbytes * 8) {
        if bit(src, pos + k) {
            out[k / 8] |= 0x80 >> (k % 8);
        }
    }
}

/// Reverse the bits of a byte.
#[inline]
pub fn reverse8(x: u8) -> u8 {
    x.reverse_bits()
}

// ---- Checks ----------------------------------------------------------------------

/// CRC-8, most significant bit first.
pub fn crc8(data: &[u8], poly: u8, init: u8) -> u8 {
    let mut crc = init;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ poly
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-8, least significant bit first (`poly` given reflected).
pub fn crc8le(data: &[u8], poly: u8, init: u8) -> u8 {
    let mut crc = init;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ poly
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// CRC-16, most significant bit first.
pub fn crc16(data: &[u8], poly: u16, init: u16) -> u16 {
    let mut crc = init;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ poly
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// CRC-16 over the first `nbits` bits of `data`, most significant first.
pub fn crc16_bits(data: &[u8], nbits: usize, poly: u16, init: u16) -> u16 {
    let mut crc = init;
    for i in 0..nbits {
        let fb = (crc >> 15 != 0) ^ bit(data, i);
        crc <<= 1;
        if fb {
            crc ^= poly;
        }
    }
    crc
}

/// CRC-16, least significant bit first (`poly` given reflected).
pub fn crc16le(data: &[u8], poly: u16, init: u16) -> u16 {
    let mut crc = init;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ poly
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Sum of the bytes, modulo 256.
pub fn add_bytes(data: &[u8]) -> u32 {
    data.iter().map(|&b| u32::from(b)).sum()
}

/// Sum of the nibbles.
pub fn add_nibbles(data: &[u8]) -> u32 {
    data.iter()
        .map(|&b| u32::from(b >> 4) + u32::from(b & 15))
        .sum()
}

/// XOR of the bytes.
pub fn xor_bytes(data: &[u8]) -> u8 {
    data.iter().fold(0, |a, &b| a ^ b)
}

/// Parity (1 if odd) of the bits of `data`.
pub fn parity(data: &[u8]) -> u32 {
    data.iter().map(|b| b.count_ones()).sum::<u32>() & 1
}

/// The 8-bit "LFSR digest" (a keyed XOR of a Galois LFSR's states, one per
/// set message bit), message bits taken most significant first. Used by
/// LaCrosse and Fine Offset sensors.
pub fn lfsr_digest8(data: &[u8], poly: u8, key: u8) -> u8 {
    let mut sum = 0;
    let mut key = key;
    for &b in data {
        for i in (0..8).rev() {
            if b >> i & 1 != 0 {
                sum ^= key;
            }
            key = if key & 1 != 0 {
                (key >> 1) ^ poly
            } else {
                key >> 1
            };
        }
    }
    sum
}

/// [`lfsr_digest8`] with the bytes taken in reverse order and their bits
/// least significant first, the LFSR shifting left.
pub fn lfsr_digest8_reflect(data: &[u8], poly: u8, key: u8) -> u8 {
    let mut sum = 0;
    let mut key = key;
    for &b in data.iter().rev() {
        for i in 0..8 {
            if b >> i & 1 != 0 {
                sum ^= key;
            }
            key = if key & 0x80 != 0 {
                (key << 1) ^ poly
            } else {
                key << 1
            };
        }
    }
    sum
}

/// 16-bit LFSR digest, message bits most significant first.
pub fn lfsr_digest16(data: &[u8], poly: u16, key: u16) -> u16 {
    let mut sum = 0;
    let mut key = key;
    for &b in data {
        for i in (0..8).rev() {
            if b >> i & 1 != 0 {
                sum ^= key;
            }
            key = if key & 1 != 0 {
                (key >> 1) ^ poly
            } else {
                key >> 1
            };
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_push_search_extract() {
        let mut b = BitBuffer::new();
        b.push_row(&[0xA5, 0xF0], 12);
        b.new_row();
        b.push_row(&[0x12], 8);
        assert_eq!(b.num_rows(), 2);
        assert_eq!(b.bits(0), 12);
        assert_eq!(b.row(0), &[0xA5, 0xF0]);
        assert_eq!(b.search(0, 0, &[0xF0], 4), 7);
        assert_eq!(b.search(0, 0, &[0x5F], 8), 4);
        let mut out = [0u8; 1];
        b.extract(0, 4, &mut out, 8);
        assert_eq!(out, [0x5F]);
        b.invert();
        assert_eq!(b.row(0), &[0x5A, 0x00]);
        b.clear();
        assert_eq!(b.num_rows(), 0);
        assert_eq!(b.row_full(0)[0], 0);
    }

    #[test]
    fn known_crcs() {
        // CRC-16/CCITT-FALSE of "123456789" is 0x29B1; CRC-8 (poly 7) is 0xF4.
        assert_eq!(crc16(b"123456789", 0x1021, 0xFFFF), 0x29B1);
        assert_eq!(crc16_bits(b"123456789", 72, 0x1021, 0xFFFF), 0x29B1);
        assert_eq!(crc8(b"123456789", 0x07, 0), 0xF4);
        // CRC-8/MAXIM (reflected 0x31 = 0x8C) is 0xA1; CRC-16/ARC 0xBB3D.
        assert_eq!(crc8le(b"123456789", 0x8C, 0), 0xA1);
        assert_eq!(crc16le(b"123456789", 0xA001, 0), 0xBB3D);
    }

    #[test]
    fn manchester() {
        let mut b = BitBuffer::new();
        b.push_row(&[0b1001_1010], 8);
        let mut out = BitBuffer::new();
        let end = b.manchester_decode(0, 0, &mut out, 100);
        assert_eq!(end, 8);
        assert_eq!(out.bits(0), 4);
        assert_eq!(out.row(0), &[0b0100_0000]);
    }
}
