//! Finding the attached sync marker in soft symbols, through the phase
//! ambiguity of the carrier loop and (optionally) the convolutional code.
//!
//! A Costas loop locks to any of the constellation's symmetries: BPSK
//! upright or inverted, QPSK at any of 4 rotations, and with a mirrored
//! spectrum (I and Q swapped) at any of those again: 8. A [`Symmetry`] is one
//! such transform; applying the right one to every symbol undoes it.
//!
//! The **channel bits** are what was modulated: one per BPSK symbol (the real
//! part), two per QPSK symbol (I, then Q). Soft channel bits are `i8`,
//! positive meaning 0.
//!
//! The [`Pattern`] searched for in the channel bits is the ASM itself
//! (uncoded), or for the convolutionally coded link the code's output for
//! the ASM, without the first 6 pairs (they depend on the bits before the
//! ASM): 52 channel bits. NRZ-M, if used, is applied to the ASM first (the
//! complement it gives after a 1 is the inverted symmetry, also searched).
//!
//! [`search`] tries every symmetry at every channel-bit position of a buffer
//! holding two frames and wants the pattern twice, one frame apart, with few
//! wrong bits (hard decisions, 64 bits at a time with `popcount`).

use foxygnal_core::Complex32;

use super::ASM;
use super::conv::ConvEncoder;

/// The modulation of the channel bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modulation {
    /// One channel bit per symbol, on the real axis.
    Bpsk,
    /// Two per symbol: I then Q (Gray: 0 → +, 1 → − on each axis).
    Qpsk,
}

impl Modulation {
    pub fn bits_per_symbol(self) -> usize {
        match self {
            Modulation::Bpsk => 1,
            Modulation::Qpsk => 2,
        }
    }

    /// Symmetries a carrier loop can lock to.
    pub fn symmetries(self) -> &'static [Symmetry] {
        const BPSK: [Symmetry; 2] = [Symmetry::new(0, false), Symmetry::new(2, false)];
        const QPSK: [Symmetry; 8] = [
            Symmetry::new(0, false),
            Symmetry::new(1, false),
            Symmetry::new(2, false),
            Symmetry::new(3, false),
            Symmetry::new(0, true),
            Symmetry::new(1, true),
            Symmetry::new(2, true),
            Symmetry::new(3, true),
        ];
        match self {
            Modulation::Bpsk => &BPSK,
            Modulation::Qpsk => &QPSK,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Modulation::Bpsk => "bpsk",
            Modulation::Qpsk => "qpsk",
        }
    }
}

/// A rotation by `quarter` × 90° followed, if `conj`, by a complex conjugate
/// (which, combined with the rotations, gives every I/Q swap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Symmetry {
    pub quarter: u8,
    pub conj: bool,
}

impl Symmetry {
    pub const fn new(quarter: u8, conj: bool) -> Self {
        Self { quarter, conj }
    }

    /// Apply to a soft symbol (I, Q), each within ±127.
    #[inline]
    pub fn apply(self, s: [i8; 2]) -> [i8; 2] {
        let [i, q] = s;
        // ×j: (i, q) → (−q, i).
        let [i, q] = match self.quarter & 3 {
            0 => [i, q],
            1 => [-q, i],
            2 => [-i, -q],
            _ => [q, -i],
        };
        if self.conj { [i, -q] } else { [i, q] }
    }

    /// Apply to a complex symbol.
    #[inline]
    pub fn apply_c(self, z: Complex32) -> Complex32 {
        let r = match self.quarter & 3 {
            0 => z,
            1 => Complex32::new(-z.im, z.re),
            2 => -z,
            _ => Complex32::new(z.im, -z.re),
        };
        if self.conj { r.conj() } else { r }
    }

    /// The rotation in degrees (for status lines).
    pub fn degrees(self) -> u32 {
        u32::from(self.quarter & 3) * 90
    }
}

/// The bits searched for, in channel-bit order.
#[derive(Debug, Clone, Copy)]
pub struct Pattern {
    /// Bit j (LSB first) is channel bit j of the pattern.
    pub bits: u64,
    pub len: usize,
    /// Channel bits from the start of the ASM to the start of the pattern.
    pub offset: usize,
    /// The complement matches too: with NRZ-M, the ASM's polarity depends
    /// on the level before it.
    pub either_polarity: bool,
}

/// The ASM after NRZ-M encoding (a 0 before it).
pub fn nrzm_asm() -> u32 {
    let mut prev = 0u32;
    let mut out = 0u32;
    for i in (0..32).rev() {
        prev ^= (ASM >> i) & 1;
        out |= prev << i;
    }
    out
}

impl Pattern {
    /// The pattern for a link: `coded` with the CCSDS code (`invert`: G2
    /// inverted), `nrzm` applied to the data bits before coding.
    pub fn new(coded: bool, invert: bool, nrzm: bool) -> Self {
        let asm = if nrzm { nrzm_asm() } else { ASM };
        if !coded {
            let mut bits = 0u64;
            for j in 0..32 {
                bits |= u64::from((asm >> (31 - j)) & 1) << j;
            }
            return Self {
                bits,
                len: 32,
                offset: 0,
                either_polarity: nrzm,
            };
        }
        let mut enc = ConvEncoder::new(invert);
        let mut bits = 0u64;
        let mut n = 0;
        for j in 0..32 {
            let pair = enc.push(((asm >> (31 - j)) & 1) as u8);
            if j >= 6 {
                bits |= u64::from(pair[0]) << n;
                bits |= u64::from(pair[1]) << (n + 1);
                n += 2;
            }
        }
        Self {
            bits,
            len: n,
            offset: 12,
            either_polarity: nrzm,
        }
    }

    pub fn mask(&self) -> u64 {
        if self.len == 64 {
            u64::MAX
        } else {
            (1u64 << self.len) - 1
        }
    }
}

/// Hard channel bits packed LSB first into words.
pub struct BitBuf {
    words: Vec<u64>,
    len: usize,
}

impl BitBuf {
    pub fn with_capacity(bits: usize) -> Self {
        Self {
            words: Vec::with_capacity(bits / 64 + 2),
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.words.clear();
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn push(&mut self, bit: bool) {
        if self.len.is_multiple_of(64) {
            self.words.push(0);
        }
        if bit {
            self.words[self.len / 64] |= 1 << (self.len % 64);
        }
        self.len += 1;
    }

    /// The 64 bits from `pos` (bits past the end read as 0).
    #[inline]
    pub fn window(&self, pos: usize) -> u64 {
        let (w, s) = (pos / 64, pos % 64);
        let lo = self.words.get(w).copied().unwrap_or(0) >> s;
        if s == 0 {
            lo
        } else {
            lo | (self.words.get(w + 1).copied().unwrap_or(0) << (64 - s))
        }
    }

    /// Wrong bits of `pat` at `pos` (or of its complement, if that matches
    /// too and is closer).
    #[inline]
    pub fn errors(&self, pos: usize, pat: &Pattern) -> u32 {
        let e = ((self.window(pos) ^ pat.bits) & pat.mask()).count_ones();
        if pat.either_polarity {
            e.min(pat.len as u32 - e)
        } else {
            e
        }
    }
}

/// Where [`search`] found the frame grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub symmetry: Symmetry,
    /// Channel bit (of the buffer, counting from its first symbol) where the
    /// ASM starts.
    pub asm_bit: usize,
    /// Wrong pattern bits at the two places.
    pub errors: u32,
}

/// Search `soft` (soft symbols) for the pattern at positions
/// `first..first + frame_bits` (channel bits, of the pattern's start) and
/// again `frame_bits` later, under every symmetry of `modulation`; best
/// match with at most `max_errors` wrong bits at each place. `bits` is
/// scratch space.
pub fn search(
    soft: &[[i8; 2]],
    modulation: Modulation,
    pat: &Pattern,
    first: usize,
    frame_bits: usize,
    max_errors: u32,
    bits: &mut BitBuf,
) -> Option<Found> {
    let bps = modulation.bits_per_symbol();
    let total = soft.len() * bps;
    if total < first + 2 * frame_bits + pat.len {
        return None;
    }
    let mut best: Option<Found> = None;
    for &sym in modulation.symmetries() {
        bits.clear();
        for &s in soft {
            let t = sym.apply(s);
            bits.push(t[0] < 0);
            if bps == 2 {
                bits.push(t[1] < 0);
            }
        }
        for p in first..first + frame_bits {
            let e1 = bits.errors(p, pat);
            if e1 > max_errors {
                continue;
            }
            let e2 = bits.errors(p + frame_bits, pat);
            if e2 > max_errors {
                continue;
            }
            let e = e1 + e2;
            if best.is_none_or(|b| e < b.errors) {
                best = Some(Found {
                    symmetry: sym,
                    asm_bit: p - pat.offset,
                    errors: e,
                });
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetries_are_a_group_with_inverses() {
        let z = [37i8, -90];
        for &a in Modulation::Qpsk.symmetries() {
            let t = a.apply(z);
            let inv = Modulation::Qpsk
                .symmetries()
                .iter()
                .filter(|b| b.apply(t) == z)
                .count();
            assert_eq!(inv, 1, "{a:?}");
            let c = a.apply_c(Complex32::new(37.0, -90.0));
            assert_eq!([c.re as i8, c.im as i8], t);
        }
        // I/Q swap is one of them.
        assert!(
            Modulation::Qpsk
                .symmetries()
                .iter()
                .any(|s| s.apply([1, 2]) == [2, 1])
        );
    }

    #[test]
    fn patterns() {
        let p = Pattern::new(false, false, false);
        assert_eq!(p.len, 32);
        // First channel bit is the ASM's MSB (0x1A = 0001 1010).
        assert_eq!(p.bits & 0xFF, 0b0101_1000);
        let c = Pattern::new(true, true, false);
        assert_eq!((c.len, c.offset), (52, 12));
        // GNU Radio's encode_ccsds_27_bb gives 0x03AE86C18FF19497 for the
        // ASM from the zero state: the same code, but G2 first and not
        // inverted. Swap its pairs, invert G2, drop the first 12 bits.
        let gr = 0x03AE_86C1_8FF1_9497u64;
        let gr_bit = |k: usize| (gr >> (63 - k)) & 1;
        let mut want = 0u64;
        for j in 0..52 {
            let k = 12 + j;
            let (pair, second) = (k / 2, k % 2);
            let g = if second == 0 {
                gr_bit(2 * pair + 1)
            } else {
                gr_bit(2 * pair) ^ 1
            };
            want |= g << j;
        }
        assert_eq!(c.bits, want);
        // NRZ-M of 0x1ACFFC1D.
        assert_eq!(nrzm_asm(), 0x1375_57E9);
    }
}
