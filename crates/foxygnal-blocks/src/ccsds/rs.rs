//! CCSDS Reed-Solomon (CCSDS 131.0-B §4): RS(255, 223), E = 16, over a
//! codeblock of `I` interleaved codewords (I = 1…5), optionally shortened
//! (virtual fill) and in Berlekamp's dual basis.
//!
//! - **Code**: [`crate::dvb::rs::CCSDS`], the generic decoder over the field
//!   `0x187` with roots β^112 … β^143, β = α¹¹.
//! - **Dual basis**: the symbols on the link are in Berlekamp's dual basis;
//!   the code is defined in the conventional one. [`TO_DUAL`] and
//!   [`FROM_DUAL`] convert (the matrix of CCSDS 131.0-B Annex F; the same
//!   tables as Phil Karn's libfec `Taltab` / `Tal1tab`).
//! - **Interleaving**: byte j of the codeblock belongs to codeword j mod I,
//!   so the I codewords' data come first, interleaved, then their parity,
//!   interleaved: the last 32·I bytes.
//! - **Virtual fill**: a codeblock shorter than 255·I bytes has the missing
//!   leading bytes of every codeword taken as zero (not sent).

use crate::dvb::rs::{CCSDS, Uncorrectable};

/// Parity bytes per codeword (2E).
pub const PARITY: usize = 32;
/// The largest interleaving depth.
pub const MAX_INTERLEAVE: usize = 8;

const TAL: [u8; 8] = [0x8d, 0xef, 0xec, 0x86, 0xfa, 0x99, 0xaf, 0x7b];

const fn dual_tables() -> ([u8; 256], [u8; 256]) {
    let mut to = [0u8; 256];
    let mut from = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut v = 0u8;
        let mut k = 0;
        while k < 8 {
            if i & (1 << k) != 0 {
                v ^= TAL[7 - k];
            }
            k += 1;
        }
        to[i] = v;
        from[v as usize] = i as u8;
        i += 1;
    }
    (to, from)
}

/// Conventional basis → Berlekamp's dual basis.
pub static TO_DUAL: [u8; 256] = dual_tables().0;
/// Dual basis → conventional.
pub static FROM_DUAL: [u8; 256] = dual_tables().1;

/// What [`Codec::decode`] did to a codeblock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Corrections {
    /// Bytes corrected, over all codewords that could be corrected.
    pub bytes: usize,
    /// Codewords that could not be corrected (left as received).
    pub failed: usize,
}

impl Corrections {
    /// Every codeword is (now) a codeword.
    pub fn ok(&self) -> bool {
        self.failed == 0
    }
}

/// RS(255, 223) over a whole codeblock.
#[derive(Debug, Clone)]
pub struct Codec {
    interleave: usize,
    /// Bytes of each codeword that are sent (255 − virtual fill).
    n: usize,
    dual: bool,
}

impl Codec {
    /// A codec for codeblocks of `len` bytes (the bytes after the sync
    /// marker), `interleave` codewords deep. `len` must be a multiple of
    /// `interleave` of at most 255 · `interleave` bytes, with room for data.
    pub fn new(len: usize, interleave: usize, dual: bool) -> Result<Self, String> {
        if !(1..=MAX_INTERLEAVE).contains(&interleave) {
            return Err(format!(
                "the Reed-Solomon interleaving depth must be 1 to {MAX_INTERLEAVE} (got {interleave})"
            ));
        }
        if !len.is_multiple_of(interleave) {
            return Err(format!(
                "a {len}-byte frame can't hold {interleave} interleaved Reed-Solomon codewords \
                 (not a multiple of {interleave})"
            ));
        }
        let n = len / interleave;
        if n > 255 || n <= PARITY {
            return Err(format!(
                "a {len}-byte frame at interleaving depth {interleave} gives {n}-byte \
                 Reed-Solomon codewords; they must be {} to 255",
                PARITY + 1
            ));
        }
        Ok(Self {
            interleave,
            n,
            dual,
        })
    }

    /// Codeblock length in bytes.
    pub fn len(&self) -> usize {
        self.n * self.interleave
    }

    /// Always false: a codeblock has data.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Interleaving depth I.
    pub fn interleave(&self) -> usize {
        self.interleave
    }

    /// Data bytes in a codeblock (the transfer frame).
    pub fn data_len(&self) -> usize {
        (self.n - PARITY) * self.interleave
    }

    /// Parity bytes at the end of a codeblock.
    pub fn parity_len(&self) -> usize {
        PARITY * self.interleave
    }

    /// Virtual fill: leading zero bytes per codeword that aren't sent.
    pub fn virtual_fill(&self) -> usize {
        255 - self.n
    }

    /// Whether the symbols are in the dual basis.
    pub fn dual(&self) -> bool {
        self.dual
    }

    #[inline]
    fn to_code(&self, b: u8) -> u8 {
        if self.dual { FROM_DUAL[b as usize] } else { b }
    }

    #[inline]
    fn to_link(&self, b: u8) -> u8 {
        if self.dual { TO_DUAL[b as usize] } else { b }
    }

    /// Fill in the parity of `block` (`len` bytes; the first
    /// [`Self::data_len`] are the data).
    pub fn encode(&self, block: &mut [u8]) {
        assert_eq!(block.len(), self.len());
        let i_depth = self.interleave;
        let k = self.n - PARITY;
        let mut parity = [0u8; PARITY];
        for i in 0..i_depth {
            let data = (0..k).map(|j| self.to_code(block[j * i_depth + i]));
            CCSDS.encode_parts(data, &mut parity);
            for (j, &p) in parity.iter().enumerate() {
                block[(k + j) * i_depth + i] = self.to_link(p);
            }
        }
    }

    /// Correct `block` in place, codeword by codeword. A codeword that can't
    /// be corrected is left as received and counted in
    /// [`Corrections::failed`].
    pub fn decode(&self, block: &mut [u8]) -> Corrections {
        assert_eq!(block.len(), self.len());
        let i_depth = self.interleave;
        let n = self.n;
        let mut cw = [0u8; 255];
        let mut res = Corrections::default();
        for i in 0..i_depth {
            for (j, c) in cw[..n].iter_mut().enumerate() {
                *c = self.to_code(block[j * i_depth + i]);
            }
            match CCSDS.decode_shortened(&mut cw[..n]) {
                Ok(0) => {}
                Ok(fixed) => {
                    res.bytes += fixed;
                    for (j, &c) in cw[..n].iter().enumerate() {
                        block[j * i_depth + i] = self.to_link(c);
                    }
                }
                Err(Uncorrectable) => res.failed += 1,
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_basis_tables_are_inverse() {
        for v in 0..=255u8 {
            assert_eq!(FROM_DUAL[TO_DUAL[v as usize] as usize], v);
        }
        // libfec's Taltab starts 0x00, 0x7b, 0xaf, 0xd4, 0x99, 0xe2, …
        assert_eq!(TO_DUAL[..6], [0x00, 0x7b, 0xaf, 0xd4, 0x99, 0xe2]);
    }

    #[test]
    fn rejects_impossible_layouts() {
        assert!(Codec::new(1020, 4, true).is_ok());
        assert!(Codec::new(1020, 3, true).is_err());
        assert!(Codec::new(1275, 5, true).is_ok());
        assert!(Codec::new(1280, 5, true).is_err());
        assert!(Codec::new(160, 5, true).is_err());
        assert!(Codec::new(255, 0, true).is_err());
    }

    #[test]
    fn round_trip_with_errors_in_every_codeword() {
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        for (len, depth, dual) in [
            (255, 1, true),
            (1020, 4, true),
            (1275, 5, false),
            (500, 2, true),
        ] {
            let c = Codec::new(len, depth, dual).unwrap();
            let mut block = vec![0u8; len];
            for b in &mut block[..c.data_len()] {
                *b = next();
            }
            c.encode(&mut block);
            let orig = block.clone();
            assert_eq!(c.decode(&mut block), Corrections::default());
            // 16 errors per codeword: correctable.
            for j in 0..16 * depth {
                let at = (j * 37 + 5) % len;
                block[at] ^= next() | 1;
            }
            let fixed = c.decode(&mut block);
            assert!(fixed.ok(), "{len}/{depth}: {fixed:?}");
            assert_eq!(block, orig);
            // 17 in codeword 0 only: that one fails, the rest are fine.
            for j in 0..17 {
                block[j * depth * 3] ^= 0x81;
            }
            if depth > 1 {
                block[1] ^= 0x01;
            }
            let bad = c.decode(&mut block);
            assert_eq!(bad.failed, 1, "{len}/{depth}");
        }
    }
}
