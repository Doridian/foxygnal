//! The CCSDS pseudo-randomizer (CCSDS 131.0-B §10): h(x) = x⁸ + x⁷ + x⁵ +
//! x³ + 1, all ones at the start of every codeblock, a 255-bit period. The
//! sequence is XORed onto everything after the attached sync marker (the
//! whole codeblock, Reed-Solomon parity included); applying it again undoes
//! it.

/// The first 255 bytes of the sequence, first bit in the MSB: 8 periods of
/// 255 bits, after which it starts over on a byte boundary.
const fn build() -> [u8; 255] {
    // x[n + 8] = x[n] ⊕ x[n + 3] ⊕ x[n + 5] ⊕ x[n + 7], x[0..8] = 1.
    let mut bits = [0u8; 255 * 8];
    let mut i = 0;
    while i < 8 {
        bits[i] = 1;
        i += 1;
    }
    while i < bits.len() {
        bits[i] = bits[i - 8] ^ bits[i - 5] ^ bits[i - 3] ^ bits[i - 1];
        i += 1;
    }
    let mut out = [0u8; 255];
    let mut b = 0;
    while b < 255 {
        let mut v = 0u8;
        let mut k = 0;
        while k < 8 {
            v = (v << 1) | bits[b * 8 + k];
            k += 1;
        }
        out[b] = v;
        b += 1;
    }
    out
}

/// 255 bytes of the sequence (8 periods), so byte `i` is `SEQUENCE[i % 255]`
/// for any `i`.
pub const SEQUENCE: [u8; 255] = build();

/// Byte `i` of the sequence.
#[inline]
pub fn byte(i: usize) -> u8 {
    SEQUENCE[i % 255]
}

/// XOR the sequence onto `data` (from its start): randomizes and
/// derandomizes.
pub fn apply(data: &mut [u8]) {
    for chunk in data.chunks_mut(255) {
        for (b, &r) in chunk.iter_mut().zip(&SEQUENCE) {
            *b ^= r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_the_published_bits() {
        // CCSDS 131.0-B-5 §10.4.1: "the first 40 bits … are
        // 1111 1111 0100 1000 0000 1110 1100 0000 1001 1010".
        assert_eq!(SEQUENCE[..5], [0xFF, 0x48, 0x0E, 0xC0, 0x9A]);
    }

    #[test]
    fn period_is_255_bits() {
        // 255 bytes are 8 periods: the next byte starts over.
        let mut d = vec![0u8; 600];
        apply(&mut d);
        assert_eq!(d[255..510], d[..255]);
        assert_eq!(d[510], 0xFF);
        // No shorter period (the polynomial is primitive).
        let bit = |i: usize| (SEQUENCE[(i / 8) % 255] >> (7 - i % 8)) & 1;
        for p in [3usize, 5, 15, 17, 51, 85] {
            assert!((0..255).any(|i| bit(i) != bit(i + p)), "period {p}");
        }
        let mut again = d.clone();
        apply(&mut again);
        assert!(again.iter().all(|&b| b == 0));
        assert_eq!(byte(256), SEQUENCE[1]);
    }
}
