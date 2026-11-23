//! A big-endian bit reader. Reading past the end yields zero bits, so a
//! truncated slice ends in a VLC error instead of a panic.

pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The next `n` bits (1..=32) without consuming them.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!((1..=32).contains(&n));
        let byte = self.pos >> 3;
        let word = match self.data.get(byte..byte + 8) {
            Some(b) => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            None => {
                let mut b = [0u8; 8];
                if let Some(rest) = self.data.get(byte..) {
                    b[..rest.len()].copy_from_slice(rest);
                }
                u64::from_be_bytes(b)
            }
        };
        ((word << (self.pos & 7)) >> (64 - n)) as u32
    }

    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    #[inline]
    pub fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    /// Read past the end of the data.
    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_across_bytes_and_past_the_end() {
        let mut b = Bits::new(&[0b1010_1100, 0xFF]);
        assert_eq!(b.read(3), 0b101);
        assert_eq!(b.read(7), 0b011_0011);
        assert_eq!(b.read(6), 0b111111);
        assert!(!b.overrun());
        assert_eq!(b.read(32), 0);
        assert!(b.overrun());
    }
}
