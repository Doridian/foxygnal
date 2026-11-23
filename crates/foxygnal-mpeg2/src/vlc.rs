//! The variable-length code tables of ISO/IEC 13818-2 annex B (shared with
//! MPEG-1), as lookup tables built once per decoder.

use crate::bits::Bits;

#[derive(Clone, Copy, Default)]
struct Entry {
    val: i16,
    len: u8,
}

/// A code table decoded with one lookup of its longest code's length.
pub struct Vlc {
    bits: u32,
    table: Vec<Entry>,
}

fn parse(code: &str) -> (u32, u32) {
    let mut v = 0;
    let mut n = 0;
    for c in code.bytes() {
        match c {
            b'0' | b'1' => {
                v = v << 1 | u32::from(c - b'0');
                n += 1;
            }
            _ => {}
        }
    }
    (v, n)
}

impl Vlc {
    /// Fails if the codes aren't prefix-free.
    pub fn try_new(codes: &[(&str, i16)]) -> Result<Self, String> {
        let bits = codes.iter().map(|(c, _)| parse(c).1).max().unwrap_or(1);
        let mut table = vec![Entry::default(); 1 << bits];
        for &(c, val) in codes {
            let (v, n) = parse(c);
            let shift = bits - n;
            for i in (v << shift)..((v + 1) << shift) {
                let e = &mut table[i as usize];
                if e.len != 0 {
                    return Err(format!("`{c}` overlaps another code"));
                }
                *e = Entry { val, len: n as u8 };
            }
        }
        Ok(Self { bits, table })
    }

    fn new(codes: &[(&str, i16)]) -> Self {
        Self::try_new(codes).expect("annex B tables are prefix-free")
    }

    #[inline]
    pub fn decode(&self, b: &mut Bits) -> Option<i16> {
        let e = self.table[b.peek(self.bits) as usize];
        if e.len == 0 {
            return None;
        }
        b.skip(u32::from(e.len));
        Some(e.val)
    }
}

pub const MB_ESCAPE: i16 = 34;
pub const MB_STUFFING: i16 = 35;

/// Table B-1: macroblock_address_increment.
const MB_ADDR: &[(&str, i16)] = &[
    ("1", 1),
    ("011", 2),
    ("010", 3),
    ("0011", 4),
    ("0010", 5),
    ("0001 1", 6),
    ("0001 0", 7),
    ("0000 111", 8),
    ("0000 110", 9),
    ("0000 1011", 10),
    ("0000 1010", 11),
    ("0000 1001", 12),
    ("0000 1000", 13),
    ("0000 0111", 14),
    ("0000 0110", 15),
    ("0000 0101 11", 16),
    ("0000 0101 10", 17),
    ("0000 0101 01", 18),
    ("0000 0101 00", 19),
    ("0000 0100 11", 20),
    ("0000 0100 10", 21),
    ("0000 0100 011", 22),
    ("0000 0100 010", 23),
    ("0000 0100 001", 24),
    ("0000 0100 000", 25),
    ("0000 0011 111", 26),
    ("0000 0011 110", 27),
    ("0000 0011 101", 28),
    ("0000 0011 100", 29),
    ("0000 0011 011", 30),
    ("0000 0011 010", 31),
    ("0000 0011 001", 32),
    ("0000 0011 000", 33),
    ("0000 0001 000", MB_ESCAPE),
    // MPEG-1 only.
    ("0000 0001 111", MB_STUFFING),
];

pub const QUANT: u8 = 1;
pub const FWD: u8 = 2;
pub const BWD: u8 = 4;
pub const PATTERN: u8 = 8;
pub const INTRA: u8 = 16;

const I_TYPES: &[(&str, i16)] = &[("1", INTRA as i16), ("01", (INTRA | QUANT) as i16)];

const P_TYPES: &[(&str, i16)] = &[
    ("1", (FWD | PATTERN) as i16),
    ("01", PATTERN as i16),
    ("001", FWD as i16),
    ("0001 1", INTRA as i16),
    ("0001 0", (QUANT | FWD | PATTERN) as i16),
    ("0000 1", (QUANT | PATTERN) as i16),
    ("0000 01", (QUANT | INTRA) as i16),
];

const B_TYPES: &[(&str, i16)] = &[
    ("10", (FWD | BWD) as i16),
    ("11", (FWD | BWD | PATTERN) as i16),
    ("010", BWD as i16),
    ("011", (BWD | PATTERN) as i16),
    ("0010", FWD as i16),
    ("0011", (FWD | PATTERN) as i16),
    ("0001 1", INTRA as i16),
    ("0001 0", (QUANT | FWD | BWD | PATTERN) as i16),
    ("0000 11", (QUANT | FWD | PATTERN) as i16),
    ("0000 10", (QUANT | BWD | PATTERN) as i16),
    ("0000 01", (QUANT | INTRA) as i16),
];

/// Table B-9: coded_block_pattern.
const CBP: &[(&str, i16)] = &[
    ("111", 60),
    ("1101", 4),
    ("1100", 8),
    ("1011", 16),
    ("1010", 32),
    ("1001 1", 12),
    ("1001 0", 48),
    ("1000 1", 20),
    ("1000 0", 40),
    ("0111 1", 28),
    ("0111 0", 44),
    ("0110 1", 52),
    ("0110 0", 56),
    ("0101 1", 1),
    ("0101 0", 61),
    ("0100 1", 2),
    ("0100 0", 62),
    ("0011 11", 24),
    ("0011 10", 36),
    ("0011 01", 3),
    ("0011 00", 63),
    ("0010 111", 5),
    ("0010 110", 9),
    ("0010 101", 17),
    ("0010 100", 33),
    ("0010 011", 6),
    ("0010 010", 10),
    ("0010 001", 18),
    ("0010 000", 34),
    ("0001 1111", 7),
    ("0001 1110", 11),
    ("0001 1101", 19),
    ("0001 1100", 35),
    ("0001 1011", 13),
    ("0001 1010", 49),
    ("0001 1001", 21),
    ("0001 1000", 41),
    ("0001 0111", 14),
    ("0001 0110", 50),
    ("0001 0101", 22),
    ("0001 0100", 42),
    ("0001 0011", 15),
    ("0001 0010", 51),
    ("0001 0001", 23),
    ("0001 0000", 43),
    ("0000 1111", 25),
    ("0000 1110", 37),
    ("0000 1101", 26),
    ("0000 1100", 38),
    ("0000 1011", 29),
    ("0000 1010", 45),
    ("0000 1001", 53),
    ("0000 1000", 57),
    ("0000 0111", 30),
    ("0000 0110", 46),
    ("0000 0101", 54),
    ("0000 0100", 58),
    ("0000 0011 1", 31),
    ("0000 0011 0", 47),
    ("0000 0010 1", 55),
    ("0000 0010 0", 59),
    ("0000 0001 1", 27),
    ("0000 0001 0", 39),
    ("0000 0000 1", 0),
];

/// Table B-10: motion_code, sign bit included.
const MOTION: &[(&str, i16)] = &[
    ("1", 0),
    ("010", 1),
    ("011", -1),
    ("0010", 2),
    ("0011", -2),
    ("00010", 3),
    ("00011", -3),
    ("0000 110", 4),
    ("0000 111", -4),
    ("0000 1010", 5),
    ("0000 1011", -5),
    ("0000 1000", 6),
    ("0000 1001", -6),
    ("0000 0110", 7),
    ("0000 0111", -7),
    ("0000 0101 10", 8),
    ("0000 0101 11", -8),
    ("0000 0101 00", 9),
    ("0000 0101 01", -9),
    ("0000 0100 10", 10),
    ("0000 0100 11", -10),
    ("0000 0100 010", 11),
    ("0000 0100 011", -11),
    ("0000 0100 000", 12),
    ("0000 0100 001", -12),
    ("0000 0011 110", 13),
    ("0000 0011 111", -13),
    ("0000 0011 100", 14),
    ("0000 0011 101", -14),
    ("0000 0011 010", 15),
    ("0000 0011 011", -15),
    ("0000 0011 000", 16),
    ("0000 0011 001", -16),
];

/// Table B-11: dmvector.
const DMV: &[(&str, i16)] = &[("0", 0), ("10", 1), ("11", -1)];

/// Table B-12: dct_dc_size_luminance.
const DC_LUMA: &[(&str, i16)] = &[
    ("100", 0),
    ("00", 1),
    ("01", 2),
    ("101", 3),
    ("110", 4),
    ("1110", 5),
    ("1111 0", 6),
    ("1111 10", 7),
    ("1111 110", 8),
    ("1111 1110", 9),
    ("1111 1111 0", 10),
    ("1111 1111 1", 11),
];

/// Table B-13: dct_dc_size_chrominance.
const DC_CHROMA: &[(&str, i16)] = &[
    ("00", 0),
    ("01", 1),
    ("10", 2),
    ("110", 3),
    ("1110", 4),
    ("1111 0", 5),
    ("1111 10", 6),
    ("1111 110", 7),
    ("1111 1110", 8),
    ("1111 1111 0", 9),
    ("1111 1111 10", 10),
    ("1111 1111 11", 11),
];

pub const EOB: u8 = 64;
pub const ESCAPE: u8 = 65;

/// Codes of both DCT coefficient tables that are 12 bits or longer and the
/// same in both (run, level; the sign bit follows).
const DCT_LONG: &[(&str, u8, u8)] = &[
    ("0000 0001 1100", 3, 3),
    ("0000 0001 0010", 4, 3),
    ("0000 0001 1110", 6, 2),
    ("0000 0001 0101", 7, 2),
    ("0000 0001 0001", 8, 2),
    ("0000 0001 1111", 17, 1),
    ("0000 0001 1010", 18, 1),
    ("0000 0001 1001", 19, 1),
    ("0000 0001 0111", 20, 1),
    ("0000 0001 0110", 21, 1),
    ("0000 0000 1011 0", 1, 6),
    ("0000 0000 1010 1", 1, 7),
    ("0000 0000 1010 0", 2, 5),
    ("0000 0000 1001 1", 3, 4),
    ("0000 0000 1001 0", 5, 3),
    ("0000 0000 1000 1", 9, 2),
    ("0000 0000 1000 0", 10, 2),
    ("0000 0000 1111 1", 22, 1),
    ("0000 0000 1111 0", 23, 1),
    ("0000 0000 1110 1", 24, 1),
    ("0000 0000 1110 0", 25, 1),
    ("0000 0000 1101 1", 26, 1),
    ("0000 0000 0111 11", 0, 16),
    ("0000 0000 0111 10", 0, 17),
    ("0000 0000 0111 01", 0, 18),
    ("0000 0000 0111 00", 0, 19),
    ("0000 0000 0110 11", 0, 20),
    ("0000 0000 0110 10", 0, 21),
    ("0000 0000 0110 01", 0, 22),
    ("0000 0000 0110 00", 0, 23),
    ("0000 0000 0101 11", 0, 24),
    ("0000 0000 0101 10", 0, 25),
    ("0000 0000 0101 01", 0, 26),
    ("0000 0000 0101 00", 0, 27),
    ("0000 0000 0100 11", 0, 28),
    ("0000 0000 0100 10", 0, 29),
    ("0000 0000 0100 01", 0, 30),
    ("0000 0000 0100 00", 0, 31),
    ("0000 0000 0011 000", 0, 32),
    ("0000 0000 0010 111", 0, 33),
    ("0000 0000 0010 110", 0, 34),
    ("0000 0000 0010 101", 0, 35),
    ("0000 0000 0010 100", 0, 36),
    ("0000 0000 0010 011", 0, 37),
    ("0000 0000 0010 010", 0, 38),
    ("0000 0000 0010 001", 0, 39),
    ("0000 0000 0010 000", 0, 40),
    ("0000 0000 0011 111", 1, 8),
    ("0000 0000 0011 110", 1, 9),
    ("0000 0000 0011 101", 1, 10),
    ("0000 0000 0011 100", 1, 11),
    ("0000 0000 0011 011", 1, 12),
    ("0000 0000 0011 010", 1, 13),
    ("0000 0000 0011 001", 1, 14),
    ("0000 0000 0001 0011", 1, 15),
    ("0000 0000 0001 0010", 1, 16),
    ("0000 0000 0001 0001", 1, 17),
    ("0000 0000 0001 0000", 1, 18),
    ("0000 0000 0001 0100", 6, 3),
    ("0000 0000 0001 1010", 11, 2),
    ("0000 0000 0001 1001", 12, 2),
    ("0000 0000 0001 1000", 13, 2),
    ("0000 0000 0001 0111", 14, 2),
    ("0000 0000 0001 0110", 15, 2),
    ("0000 0000 0001 0101", 16, 2),
    ("0000 0000 0001 1111", 27, 1),
    ("0000 0000 0001 1110", 28, 1),
    ("0000 0000 0001 1101", 29, 1),
    ("0000 0000 0001 1100", 30, 1),
    ("0000 0000 0001 1011", 31, 1),
];

/// Table B-14 (DCT coefficients table zero), up to 12 bits. For the first
/// coefficient of a non-intra block, `1s` is run 0 level 1 (handled by the
/// caller); elsewhere `10` ends the block.
const B14: &[(&str, u8, u8)] = &[
    ("10", EOB, 0),
    ("11", 0, 1),
    ("011", 1, 1),
    ("0100", 0, 2),
    ("0101", 2, 1),
    ("0010 1", 0, 3),
    ("0011 1", 3, 1),
    ("0011 0", 4, 1),
    ("0001 10", 1, 2),
    ("0001 11", 5, 1),
    ("0001 01", 6, 1),
    ("0001 00", 7, 1),
    ("0000 110", 0, 4),
    ("0000 100", 2, 2),
    ("0000 111", 8, 1),
    ("0000 101", 9, 1),
    ("0000 01", ESCAPE, 0),
    ("0010 0110", 0, 5),
    ("0010 0001", 0, 6),
    ("0010 0101", 1, 3),
    ("0010 0100", 3, 2),
    ("0010 0111", 10, 1),
    ("0010 0011", 11, 1),
    ("0010 0010", 12, 1),
    ("0010 0000", 13, 1),
    ("0000 0010 10", 0, 7),
    ("0000 0011 00", 1, 4),
    ("0000 0010 11", 2, 3),
    ("0000 0011 11", 4, 2),
    ("0000 0010 01", 5, 2),
    ("0000 0011 10", 14, 1),
    ("0000 0011 01", 15, 1),
    ("0000 0010 00", 16, 1),
    ("0000 0001 1101", 0, 8),
    ("0000 0001 1000", 0, 9),
    ("0000 0001 0011", 0, 10),
    ("0000 0001 0000", 0, 11),
    ("0000 0001 1011", 1, 5),
    ("0000 0001 0100", 2, 4),
    ("0000 0000 1101 0", 0, 12),
    ("0000 0000 1100 1", 0, 13),
    ("0000 0000 1100 0", 0, 14),
    ("0000 0000 1011 1", 0, 15),
];

/// Table B-15 (DCT coefficients table one, `intra_vlc_format`), codes that
/// differ from table zero.
const B15: &[(&str, u8, u8)] = &[
    ("0110", EOB, 0),
    ("10", 0, 1),
    ("110", 0, 2),
    ("0111", 0, 3),
    ("1110 0", 0, 4),
    ("1110 1", 0, 5),
    ("0001 01", 0, 6),
    ("0001 00", 0, 7),
    ("1111 011", 0, 8),
    ("1111 100", 0, 9),
    ("0010 0011", 0, 10),
    ("0010 0010", 0, 11),
    ("1111 1010", 0, 12),
    ("1111 1011", 0, 13),
    ("1111 1110", 0, 14),
    ("1111 1111", 0, 15),
    ("010", 1, 1),
    ("0011 0", 1, 2),
    ("1111 001", 1, 3),
    ("0010 0111", 1, 4),
    ("0010 0000", 1, 5),
    ("0010 1", 2, 1),
    ("0000 111", 2, 2),
    ("1111 1100", 2, 3),
    ("0000 0011 00", 2, 4),
    ("0011 1", 3, 1),
    ("0010 0110", 3, 2),
    ("0001 10", 4, 1),
    ("1111 1101", 4, 2),
    ("0001 11", 5, 1),
    ("0000 0010 0", 5, 2),
    ("0000 110", 6, 1),
    ("0000 100", 7, 1),
    ("0000 101", 8, 1),
    ("1111 000", 9, 1),
    ("1111 010", 10, 1),
    ("0010 0001", 11, 1),
    ("0010 0101", 12, 1),
    ("0010 0100", 13, 1),
    ("0000 0010 1", 14, 1),
    ("0000 0011 1", 15, 1),
    ("0000 0011 01", 16, 1),
    ("0000 01", ESCAPE, 0),
];

#[derive(Clone, Copy, Default)]
pub struct Dct {
    pub run: u8,
    pub level: u8,
    /// Without the sign bit; 0 = not a code.
    pub len: u8,
}

/// A DCT coefficient table: codes that don't start with six zeros are at most
/// 10 bits long and are looked up by the first 10 bits; the others by the 10
/// bits after the zeros.
pub struct DctVlc {
    short: Vec<Dct>,
    long: Vec<Dct>,
}

impl DctVlc {
    pub fn try_new(lists: &[&[(&str, u8, u8)]]) -> Result<Self, String> {
        let mut short = vec![Dct::default(); 1024];
        let mut long = vec![Dct::default(); 1024];
        for &(c, run, level) in lists.iter().flat_map(|l| l.iter()) {
            let (v, n) = parse(c);
            let (table, v, bits) = if n > 6 && v >> (n - 6) == 0 {
                (&mut long, v, n - 6)
            } else {
                (&mut short, v, n)
            };
            if bits > 10 {
                return Err(format!("`{c}` is too long"));
            }
            let shift = 10 - bits;
            for i in (v << shift)..((v + 1) << shift) {
                let e = &mut table[i as usize];
                if e.len != 0 {
                    return Err(format!("`{c}` overlaps another code"));
                }
                *e = Dct {
                    run,
                    level,
                    len: n as u8,
                };
            }
        }
        Ok(Self { short, long })
    }

    /// The next entry, consumed (without its sign bit); `len == 0` if invalid.
    #[inline]
    pub fn decode(&self, b: &mut Bits) -> Dct {
        let p = b.peek(16);
        let e = if p >> 10 != 0 {
            self.short[(p >> 6) as usize]
        } else {
            self.long[(p & 0x3FF) as usize]
        };
        b.skip(u32::from(e.len));
        e
    }
}

pub struct Tables {
    pub mb_addr: Vlc,
    pub mb_type: [Vlc; 3],
    pub cbp: Vlc,
    pub motion: Vlc,
    pub dmv: Vlc,
    pub dc_luma: Vlc,
    pub dc_chroma: Vlc,
    pub b14: DctVlc,
    pub b15: DctVlc,
}

impl Tables {
    pub fn new() -> Self {
        Self {
            mb_addr: Vlc::new(MB_ADDR),
            mb_type: [Vlc::new(I_TYPES), Vlc::new(P_TYPES), Vlc::new(B_TYPES)],
            cbp: Vlc::new(CBP),
            motion: Vlc::new(MOTION),
            dmv: Vlc::new(DMV),
            dc_luma: Vlc::new(DC_LUMA),
            dc_chroma: Vlc::new(DC_CHROMA),
            b14: DctVlc::try_new(&[B14, DCT_LONG]).expect("table B-14"),
            b15: DctVlc::try_new(&[B15, DCT_LONG]).expect("table B-15"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kraft sum of a code list, as a fraction of 2^17.
    fn kraft(codes: impl Iterator<Item = u32>) -> u32 {
        codes.map(|n| 1 << (17 - n)).sum()
    }

    #[test]
    fn tables_are_prefix_free() {
        for t in [
            MB_ADDR, I_TYPES, P_TYPES, B_TYPES, CBP, MOTION, DMV, DC_LUMA, DC_CHROMA,
        ] {
            Vlc::try_new(t).unwrap();
        }
        DctVlc::try_new(&[B14, DCT_LONG]).unwrap();
        DctVlc::try_new(&[B15, DCT_LONG]).unwrap();
    }

    #[test]
    fn coded_block_patterns_cover_every_value_once() {
        let mut seen = [0; 64];
        for &(_, v) in CBP {
            seen[v as usize] += 1;
        }
        assert!(seen.iter().all(|&n| n == 1));
    }

    #[test]
    fn dct_tables_use_the_whole_code_space() {
        // (The sign bit after a code doubles it without changing its share.)
        // Everything except a few all-zero prefixes is used.
        let sum = |l: &[&[(&str, u8, u8)]]| {
            kraft(l.iter().flat_map(|l| l.iter()).map(|(c, _, _)| parse(c).1))
        };
        let b14 = sum(&[B14, DCT_LONG]);
        let b15 = sum(&[B15, DCT_LONG]);
        assert!(b14 <= 1 << 17 && b14 > (1 << 17) - 128, "B-14 {b14}");
        assert!(b15 <= 1 << 17 && b15 > (1 << 17) - 1024, "B-15 {b15}");
        // Every run/level pair of table zero exists in table one too.
        let pairs = |l: &[(&str, u8, u8)]| {
            let mut v: Vec<_> = l.iter().map(|&(_, r, lv)| (r, lv)).collect();
            v.sort();
            v
        };
        let mut a = pairs(B14);
        a.extend(pairs(DCT_LONG));
        let mut b = pairs(B15);
        b.extend(pairs(DCT_LONG));
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }
}
