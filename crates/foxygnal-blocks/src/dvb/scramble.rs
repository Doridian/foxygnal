//! Energy dispersal (EN 300 421 §4.4.1, the same in DVB-T and DVB-C): the
//! transport stream is XORed with a PRBS (1 + x¹⁴ + x¹⁵) restarted every 8
//! packets, so the spectrum stays flat whatever the payload.
//!
//! - The register is loaded with `100101010000000` at the start of every
//!   group of 8 packets, whose first sync byte is sent inverted (`0xB8`).
//! - The PRBS is applied from the first byte after that inverted sync byte.
//! - The other 7 sync bytes are not scrambled, but the generator keeps running
//!   through them (their 8 PRBS bits are thrown away), so one group uses
//!   8 × 188 − 1 PRBS bytes.
//!
//! Scrambling and descrambling are the same XOR, so both use one table.

use super::{SYNC, SYNC_INV, TS_LEN};

/// Packets in one energy-dispersal group.
pub const GROUP: usize = 8;

/// The generator `1 + x¹⁴ + x¹⁵`. Register stage `i` (1…15) is bit `i − 1`.
#[derive(Debug, Clone, Copy)]
pub struct Prbs {
    reg: u16,
}

impl Prbs {
    /// `100101010000000`: stages 1, 4, 6 and 8 set.
    pub const INIT: u16 = 0b1010_1001;

    pub const fn new() -> Self {
        Self { reg: Self::INIT }
    }

    /// The next PRBS bit (stage 14 XOR stage 15, fed back into stage 1).
    pub const fn next_bit(&mut self) -> u8 {
        let b = ((self.reg >> 13) ^ (self.reg >> 14)) & 1;
        self.reg = ((self.reg << 1) | b) & 0x7FFF;
        b as u8
    }

    /// The next 8 bits, first one most significant.
    pub const fn next_byte(&mut self) -> u8 {
        let mut v = 0u8;
        let mut i = 0;
        while i < 8 {
            v = (v << 1) | self.next_bit();
            i += 1;
        }
        v
    }
}

impl Default for Prbs {
    fn default() -> Self {
        Self::new()
    }
}

/// What to XOR byte `i` of packet `p` (0…7) of a group with: `TABLE[p·188 + i]`.
/// Sync byte positions hold 0.
pub static TABLE: [u8; GROUP * TS_LEN] = build_table();

const fn build_table() -> [u8; GROUP * TS_LEN] {
    let mut t = [0u8; GROUP * TS_LEN];
    let mut prbs = Prbs::new();
    // Byte 0 of the group (the inverted sync byte) comes before the PRBS starts.
    let mut i = 1;
    while i < GROUP * TS_LEN {
        let b = prbs.next_byte();
        if i % TS_LEN != 0 {
            t[i] = b;
        }
        i += 1;
    }
    t
}

/// Scramble packet `index` (0…7) of a group in place and set its sync byte
/// (`0xB8` for packet 0, else `0x47`).
pub fn scramble(pkt: &mut [u8; TS_LEN], index: usize) {
    let t = &TABLE[index * TS_LEN..(index + 1) * TS_LEN];
    for (b, &k) in pkt.iter_mut().zip(t) {
        *b ^= k;
    }
    pkt[0] = if index == 0 { SYNC_INV } else { SYNC };
}

/// Undo [`scramble`]: XOR again and restore the sync byte to `0x47`.
pub fn descramble(pkt: &mut [u8; TS_LEN], index: usize) {
    let t = &TABLE[index * TS_LEN..(index + 1) * TS_LEN];
    for (b, &k) in pkt.iter_mut().zip(t) {
        *b ^= k;
    }
    pkt[0] = SYNC;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prbs_starts_as_in_the_standard() {
        // EN 300 421 figure 2: after loading 100101010000000 the output starts
        // 0000 0011 1111 0110 ...
        let mut p = Prbs::new();
        let bits: Vec<u8> = (0..16).map(|_| p.next_bit()).collect();
        assert_eq!(bits, [0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 0, 1, 1, 0]);
        assert_eq!(&TABLE[1..3], &[0x03, 0xF6]);
    }

    #[test]
    fn prbs_has_period_32767() {
        let mut p = Prbs::new();
        let first: Vec<u8> = (0..64).map(|_| p.next_bit()).collect();
        for _ in 64..32767 {
            p.next_bit();
        }
        let again: Vec<u8> = (0..64).map(|_| p.next_bit()).collect();
        assert_eq!(first, again);
    }

    #[test]
    fn group_uses_1503_prbs_bytes_with_sync_bytes_skipped() {
        let mut p = Prbs::new();
        let seq: Vec<u8> = (0..GROUP * TS_LEN - 1).map(|_| p.next_byte()).collect();
        for (i, &t) in TABLE.iter().enumerate() {
            if i % TS_LEN == 0 {
                assert_eq!(t, 0, "sync position {i}");
            } else {
                assert_eq!(t, seq[i - 1], "byte {i}");
            }
        }
    }

    #[test]
    fn scramble_round_trips() {
        let mut pkt = [0u8; TS_LEN];
        pkt[0] = SYNC;
        for (i, b) in pkt.iter_mut().enumerate().skip(1) {
            *b = (i * 7) as u8;
        }
        for index in 0..GROUP {
            let mut x = pkt;
            scramble(&mut x, index);
            assert_eq!(x[0], if index == 0 { 0xB8 } else { 0x47 });
            assert_ne!(x[1..], pkt[1..]);
            descramble(&mut x, index);
            assert_eq!(x, pkt);
        }
    }
}
