//! The GOES DCS transmission format of the DCPRS Certification Standard
//! version 2.0 (CS2, NOAA/NESDIS, June 2009), shared by the demodulator and
//! the test modulator:
//!
//! ```text
//! carrier │ clock │  FSS   │ GOES ID │ flag │ data … │ EOT │ flush
//! 0.5 s / │ 1 0 1 │15 bits │ 32 bits │8 bits│        │8 bit│ 32 bits
//! 0.25 s  │      BPSK      │◀──── scrambled, trellis coded 8PSK ─────▶│
//! ```
//!
//! The carrier is phase 0°. Clock and FSS are BPSK at the symbol rate (a 1
//! is 180°). From the GOES ID on, bytes are XORed with a 40-byte table and
//! split into bit pairs, least significant pair first; in each pair the
//! upper bit selects the half-plane (E-2, uncoded) and the lower one goes
//! through a rate 1/2, K = 7 convolutional code (the NASA 171/133
//! polynomials), giving E-1 E-0. That is a "pragmatic" trellis-coded 8PSK:
//! two bits per symbol, 150 symbols/s for 300 bit/s and 600 for 1200 bit/s.

/// The data scrambler: XORed byte by byte from the first GOES ID byte on,
/// cyclically (CS2 §3.2).
pub const SCRAMBLER: [u8; 40] = [
    0x53, 0x12, 0x72, 0xB2, 0x54, 0x62, 0xAA, 0xE4, 0xDB, 0xA7, 0x56, 0x08, 0xA8, 0x09, 0xB4, 0xBF,
    0x61, 0xDC, 0x50, 0xE3, 0xAB, 0x7F, 0x00, 0x87, 0x6D, 0xF5, 0x58, 0xCC, 0xCF, 0x3E, 0xE7, 0x2A,
    0x7E, 0x9B, 0x5C, 0x4D, 0xCE, 0xA5, 0x3C, 0x0A,
];

/// The frame synchronisation sequence, sent first bit (MSB) first, 1 = 180°.
pub const FSS: [u8; 15] = [0, 0, 1, 1, 1, 1, 1, 0, 0, 1, 1, 0, 1, 0, 1];
/// The three clock symbols between carrier and FSS (180°, 0°, 180°).
pub const CLOCK: [u8; 3] = [1, 0, 1];
/// ASCII EOT (odd parity as it is): ends ASCII and pseudo-binary messages.
pub const EOT: u8 = 0x04;
/// Zero bits (before scrambling) sent after the message to flush the
/// trellis encoder: 32 for ASCII / pseudo-binary.
pub const FLUSH_BYTES: usize = 4;

/// Uplink channel 1 (Hz) and the channel step: CS2 channels are 750 Hz apart,
/// numbered 1, 301, 2, 302, … 266, 566 upwards in frequency.
pub const CH1_HZ: f64 = 401_701_000.0;
pub const CHANNEL_STEP_HZ: f64 = 750.0;
/// GOES-R relays 401.7–402.1 MHz to 1679.7–1680.1 MHz (no inversion; the
/// pilots 401.85 / 401.70 MHz come down at 1679.85 / 1679.70 MHz).
pub const DOWNLINK_OFFSET_HZ: f64 = 1_278_000_000.0;
/// Highest index of the 750 Hz grid (channel 566).
const LAST_SLOT: i64 = 531;

/// User data rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baud {
    B300,
    B1200,
}

impl Baud {
    pub fn symbol_rate(self) -> f64 {
        match self {
            Baud::B300 => 150.0,
            Baud::B1200 => 600.0,
        }
    }
    pub fn bits(self) -> u32 {
        match self {
            Baud::B300 => 300,
            Baud::B1200 => 1200,
        }
    }
    /// Length of the carrier-only preamble in seconds.
    pub fn carrier_s(self) -> f64 {
        match self {
            Baud::B300 => 0.5,
            Baud::B1200 => 0.25,
        }
    }
    /// Longest message (bytes after the flag word) the fail-safe allows.
    pub fn max_bytes(self) -> usize {
        match self {
            Baud::B300 => 4_000,
            Baud::B1200 => 16_000,
        }
    }
}

/// Phase (in 45° steps) of the trellis output E-2 E-1 E-0 (CS2 Table 1).
pub const PHASE: [u8; 8] = [0, 1, 3, 2, 4, 5, 7, 6];

/// Taps on the shift register (bit 0 = Q1, the newest bit) for E-1 and E-0.
const G1: u8 = 0x6D; // Q1 Q3 Q4 Q6 Q7 (133 octal)
const G0: u8 = 0x4F; // Q1 Q2 Q3 Q4 Q7 (171 octal)

/// The coded label E-1 E-0 (0–3) for a 7-bit register value.
pub const fn label(reg: u8) -> u8 {
    ((((reg & G1).count_ones() & 1) << 1) | ((reg & G0).count_ones() & 1)) as u8
}

/// Phase (in 45° steps) of coded label `c` with the uncoded bit 0; the
/// uncoded bit adds 180°.
pub const LABEL_PHASE: [u8; 4] = [0, 1, 3, 2];

/// The CS2 trellis encoder: one bit pair in, one 8PSK phase out.
#[derive(Default)]
pub struct TrellisEncoder {
    reg: u8,
}

impl TrellisEncoder {
    /// `pair` is two bits, the upper one uncoded; returns a phase 0–7 (×45°).
    pub fn push(&mut self, pair: u8) -> u8 {
        self.reg = ((self.reg << 1) | (pair & 1)) & 0x7F;
        let c = label(self.reg);
        (LABEL_PHASE[c as usize] + 4 * ((pair >> 1) & 1)) & 7
    }

    /// Encode bytes (already scrambled), least significant pair first.
    pub fn bytes(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            for p in 0..4 {
                out.push(self.push(b >> (2 * p)));
            }
        }
    }
}

// ---- GOES ID: BCH(31,21) ------------------------------------------------------------

/// Generator of the (31,21) BCH code, x¹⁰+x⁹+x⁸+x⁶+x⁵+x³+1. The examples in
/// the certification standards (`CE1200B8` in CS2 with its appended 0 bit,
/// `3485763E` in the 100 bit/s standard) are codewords of it.
const BCH_GEN: u32 = 0x769;

fn bch_remainder(mut v: u32) -> u32 {
    for i in (10..31).rev() {
        if v >> i & 1 != 0 {
            v ^= BCH_GEN << (i - 10);
        }
    }
    v
}

/// The 32-bit GOES ID (31-bit codeword and a 0) for 21 information bits.
pub fn bch_encode(info: u32) -> u32 {
    let v = (info & 0x1F_FFFF) << 10;
    (v | bch_remainder(v)) << 1
}

/// Syndrome → error pattern for every pattern of up to two bits (the code
/// corrects two errors; 1 + 31 + 465 patterns, each syndrome unique).
fn syndrome_table() -> &'static [u32; 1024] {
    use std::sync::OnceLock;
    static T: OnceLock<Box<[u32; 1024]>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = Box::new([u32::MAX; 1024]);
        t[0] = 0;
        for i in 0..31 {
            t[bch_remainder(1 << i) as usize] = 1 << i;
            for j in i + 1..31 {
                let e = (1u32 << i) | (1 << j);
                t[bch_remainder(e) as usize] = e;
            }
        }
        t
    })
}

/// Check and correct a received 32-bit GOES ID. Returns the corrected ID and
/// the number of bits fixed (the appended 0 counts), or `None` if more than
/// two of the 31 code bits are wrong.
pub fn bch_correct(addr: u32) -> Option<(u32, u32)> {
    let word = addr >> 1;
    let e = syndrome_table()[bch_remainder(word) as usize];
    if e == u32::MAX {
        return None;
    }
    Some(((word ^ e) << 1, e.count_ones() + (addr & 1)))
}

// ---- Flag word ---------------------------------------------------------------------

/// What the flag word says the message is (bits 6 and 7, plus the binary
/// protocol draft's extended type in bits 3–5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Ascii,
    PseudoBinary,
    Binary,
    Unknown,
}

impl Format {
    pub fn of(flag: u8) -> Self {
        match (flag >> 5) & 3 {
            1 => Format::Ascii,
            3 => Format::PseudoBinary,
            2 => Format::Binary,
            _ => Format::Unknown,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Format::Ascii => "ascii",
            Format::PseudoBinary => "pseudo-binary",
            Format::Binary => "binary",
            Format::Unknown => "unknown",
        }
    }
    /// Ends with an EOT character rather than a length.
    pub fn has_eot(self) -> bool {
        !matches!(self, Format::Binary)
    }
}

/// `v` with bit 7 set so that the byte has odd parity.
pub const fn odd_parity(v: u8) -> u8 {
    let v = v & 0x7F;
    if v.count_ones().is_multiple_of(2) {
        v | 0x80
    } else {
        v
    }
}

/// The flag word for a format (UTC-synced bit off, spare bits 0).
pub fn flag_for(format: Format) -> u8 {
    odd_parity(match format {
        Format::Ascii => 0x20,
        Format::PseudoBinary => 0x60,
        Format::Binary => 0x40,
        Format::Unknown => 0,
    })
}

/// Length of an open-binary packet from the three bytes after the flag word:
/// 14-bit length and 10-bit BCH(31,21) parity over the flag's lower seven
/// bits and the length (GOES HDR Binary Protocol draft 0.9a). The bit layout
/// (length, then parity, most significant first) is our reading of the
/// draft; the BCH check confirms it before the length is trusted.
pub fn binary_length(flag: u8, b: [u8; 3]) -> Option<usize> {
    let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
    let len = v >> 10;
    let info = (u32::from(flag & 0x7F) << 14) | len;
    let word = (info << 10) | (v & 0x3FF);
    (bch_remainder(word) == 0).then_some(len as usize)
}

/// The three bytes [`binary_length`] reads, for the test modulator.
pub fn binary_length_bytes(flag: u8, len: usize) -> [u8; 3] {
    let info = (u32::from(flag & 0x7F) << 14) | (len as u32 & 0x3FFF);
    let v = ((len as u32 & 0x3FFF) << 10) | bch_remainder(info << 10);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

// ---- Channels ----------------------------------------------------------------------

/// Uplink centre frequency of a CS2 channel (1–266, 301–566).
pub fn channel_freq(ch: u16) -> Option<f64> {
    let k = match ch {
        1..=266 => 2 * (i64::from(ch) - 1),
        301..=566 => 2 * (i64::from(ch) - 301) + 1,
        _ => return None,
    };
    Some(CH1_HZ + k as f64 * CHANNEL_STEP_HZ)
}

/// The CS2 channel nearest an uplink frequency and the offset from its
/// centre, if within half a channel of the plan.
pub fn channel_at(f_uplink: f64) -> Option<(u16, f64)> {
    let k = ((f_uplink - CH1_HZ) / CHANNEL_STEP_HZ).round() as i64;
    if !(0..=LAST_SLOT).contains(&k) {
        return None;
    }
    let off = f_uplink - (CH1_HZ + k as f64 * CHANNEL_STEP_HZ);
    let ch = if k % 2 == 0 { k / 2 + 1 } else { 301 + k / 2 };
    Some((ch as u16, off))
}

/// Channels 1200 bit/s platforms may use (every third 750 Hz slot from
/// channel 301, i.e. 2250 Hz apart).
pub fn is_1200_channel(ch: u16) -> bool {
    channel_freq(ch).is_some_and(|f| {
        let k = ((f - CH1_HZ) / CHANNEL_STEP_HZ).round() as i64;
        k % 3 == 1
    })
}

/// An RF frequency as seen on the uplink: L-band downlink frequencies are
/// moved back down by [`DOWNLINK_OFFSET_HZ`], so the decoder can listen to
/// either the 1680 MHz downlink or platforms at 402 MHz directly.
pub fn to_uplink(f: f64) -> f64 {
    if f > 1.0e9 { f - DOWNLINK_OFFSET_HZ } else { f }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_examples_are_bch_codewords() {
        assert_eq!(bch_correct(0xCE12_00B8), Some((0xCE12_00B8, 0)));
        assert_eq!(bch_correct(0x3485_763E << 1), Some((0x3485_763E << 1, 0)));
        assert_eq!(bch_encode(0xCE12_00B8 >> 11), 0xCE12_00B8);
    }

    #[test]
    fn bch_fixes_two_errors_and_refuses_three() {
        let a = bch_encode(0x12345);
        for i in 1..32 {
            for j in (i + 1)..32 {
                let r = bch_correct(a ^ (1 << i) ^ (1 << j)).unwrap();
                assert_eq!(r, (a, 2), "{i} {j}");
            }
        }
        // Three errors decode to some other codeword or fail, never to `a`.
        let r = bch_correct(a ^ 0b1110);
        assert!(r.is_none_or(|(c, _)| c != a));
    }

    #[test]
    fn scrambler_example_from_the_standard() {
        assert_eq!(0xCE ^ SCRAMBLER[0], 0x9D);
        assert_eq!(0x12 ^ SCRAMBLER[1], 0x00);
    }

    #[test]
    fn channel_plan() {
        assert_eq!(channel_freq(1), Some(401_701_000.0));
        assert_eq!(channel_freq(301), Some(401_701_750.0));
        assert_eq!(channel_freq(2), Some(401_702_500.0));
        assert_eq!(channel_freq(566), Some(402_099_250.0));
        assert_eq!(channel_freq(565), Some(402_097_750.0));
        assert_eq!(channel_freq(267), None);
        for ch in (1..=266).chain(301..=566) {
            let f = channel_freq(ch).unwrap();
            assert_eq!(channel_at(f + 100.0), Some((ch, 100.0)));
        }
        assert!(is_1200_channel(301) && is_1200_channel(3) && is_1200_channel(304));
        assert!(is_1200_channel(565) && !is_1200_channel(1) && !is_1200_channel(302));
        assert_eq!(to_uplink(1_679_850_000.0), 401_850_000.0);
    }

    #[test]
    fn encoder_is_the_k7_code_with_an_uncoded_bit() {
        // A single 1 into the coded input: the impulse response on E-1 E-0
        // spells the generators 133 / 171 (octal), Q1 first.
        let mut e = TrellisEncoder::default();
        let mut e1 = 0u8;
        let mut e0 = 0u8;
        for i in 0..7 {
            let ph = e.push(u8::from(i == 0));
            let c = LABEL_PHASE.iter().position(|&p| p == ph).unwrap() as u8;
            e1 |= (c >> 1) << i;
            e0 |= (c & 1) << i;
        }
        assert_eq!(e1, 0b110_1101);
        assert_eq!(e0, 0b100_1111);
        // The uncoded bit only adds 180°.
        let mut a = TrellisEncoder::default();
        let mut b = TrellisEncoder::default();
        assert_eq!(a.push(0b10), (b.push(0b00) + 4) & 7);
    }

    #[test]
    fn formats_and_binary_length() {
        assert_eq!(Format::of(flag_for(Format::Ascii)), Format::Ascii);
        assert_eq!(
            Format::of(flag_for(Format::PseudoBinary)),
            Format::PseudoBinary
        );
        assert_eq!(Format::of(flag_for(Format::Binary)), Format::Binary);
        for f in [Format::Ascii, Format::PseudoBinary, Format::Binary] {
            assert_eq!(flag_for(f).count_ones() % 2, 1);
        }
        let flag = flag_for(Format::Binary);
        let b = binary_length_bytes(flag, 1234);
        assert_eq!(binary_length(flag, b), Some(1234));
        assert_eq!(binary_length(flag, [b[0], b[1] ^ 4, b[2]]), None);
    }
}
