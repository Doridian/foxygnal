//! The DVB-T frame structure (EN 300 744 §4.4–4.6): transmission modes,
//! constellations, pilot and TPS carrier positions, the pilot PRBS, the inner
//! (bit and symbol) interleavers, the mapping, and the TPS with its BCH code.
//!
//! Everything here is built once (at configure time) into plain tables; the
//! modulator and demodulator only index them.

use std::f32::consts::FRAC_1_SQRT_2;
use std::sync::OnceLock;

use crate::dvb::conv::CodeRate;

/// OFDM symbols per frame.
pub const FRAME_SYMBOLS: usize = 68;
/// Frames per superframe.
pub const SUPERFRAME: usize = 4;
/// Boosted pilot amplitude.
pub const PILOT_BOOST: f32 = 4.0 / 3.0;

/// The transmission mode (FFT size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    K2,
    K8,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::K2, Mode::K8];

    pub fn name(self) -> &'static str {
        match self {
            Mode::K2 => "2k",
            Mode::K8 => "8k",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == s)
    }

    pub fn fft_len(self) -> usize {
        match self {
            Mode::K2 => 2048,
            Mode::K8 => 8192,
        }
    }

    /// The highest carrier index (carriers are 0 ..= kmax).
    pub fn kmax(self) -> usize {
        match self {
            Mode::K2 => 1704,
            Mode::K8 => 6816,
        }
    }

    pub fn carriers(self) -> usize {
        self.kmax() + 1
    }

    pub fn data_carriers(self) -> usize {
        match self {
            Mode::K2 => 1512,
            Mode::K8 => 6048,
        }
    }

    fn index(self) -> usize {
        match self {
            Mode::K2 => 0,
            Mode::K8 => 1,
        }
    }
}

/// The constellation of the data carriers (non-hierarchical).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constellation {
    Qpsk,
    Qam16,
    Qam64,
}

impl Constellation {
    pub const ALL: [Constellation; 3] = [
        Constellation::Qpsk,
        Constellation::Qam16,
        Constellation::Qam64,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Constellation::Qpsk => "qpsk",
            Constellation::Qam16 => "16qam",
            Constellation::Qam64 => "64qam",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == s)
    }

    /// Bits per carrier.
    pub fn bits(self) -> usize {
        match self {
            Constellation::Qpsk => 2,
            Constellation::Qam16 => 4,
            Constellation::Qam64 => 6,
        }
    }

    /// Mean power of the odd-integer grid (±1, ±3, …): the points are this
    /// grid divided by its square root.
    pub fn grid_power(self) -> f32 {
        match self {
            Constellation::Qpsk => 2.0,
            Constellation::Qam16 => 10.0,
            Constellation::Qam64 => 42.0,
        }
    }

    fn index(self) -> usize {
        match self {
            Constellation::Qpsk => 0,
            Constellation::Qam16 => 1,
            Constellation::Qam64 => 2,
        }
    }
}

/// The guard interval, as a fraction of the useful symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    G1_4,
    G1_8,
    G1_16,
    G1_32,
}

impl Guard {
    pub const ALL: [Guard; 4] = [Guard::G1_4, Guard::G1_8, Guard::G1_16, Guard::G1_32];

    pub fn name(self) -> &'static str {
        match self {
            Guard::G1_4 => "1/4",
            Guard::G1_8 => "1/8",
            Guard::G1_16 => "1/16",
            Guard::G1_32 => "1/32",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.name() == s)
    }

    /// Guard samples for a useful symbol of `n` samples.
    pub fn len(self, n: usize) -> usize {
        n / match self {
            Guard::G1_4 => 4,
            Guard::G1_8 => 8,
            Guard::G1_16 => 16,
            Guard::G1_32 => 32,
        }
    }
}

/// The elementary sample rate for a channel bandwidth in MHz: 64/7 MHz for
/// 8 MHz channels, scaled for the others.
pub fn elementary_rate(bandwidth_mhz: f64) -> f64 {
    64e6 / 7.0 * bandwidth_mhz / 8.0
}

/// TS bits per second for a parameter set.
pub fn ts_bit_rate(
    mode: Mode,
    c: Constellation,
    rate: CodeRate,
    guard: Guard,
    sample_rate: f64,
) -> f64 {
    let n = mode.fft_len();
    let bits = (mode.data_carriers() * c.bits()) as f64 * rate.value() * 188.0 / 204.0;
    bits * sample_rate / (n + guard.len(n)) as f64
}

// ---- Carrier positions ----------------------------------------------------------

/// Continual pilots of the 2k mode; the 8k mode repeats them every 1704
/// carriers.
const CONTINUAL_2K: [u16; 45] = [
    0, 48, 54, 87, 141, 156, 192, 201, 255, 279, 282, 333, 432, 450, 483, 525, 531, 618, 636, 714,
    759, 765, 780, 804, 873, 888, 918, 939, 942, 969, 984, 1050, 1101, 1107, 1110, 1137, 1140,
    1146, 1206, 1269, 1323, 1377, 1491, 1683, 1704,
];

/// TPS carriers of the 2k mode; the 8k mode repeats them every 1704 carriers.
const TPS_2K: [u16; 17] = [
    34, 50, 209, 346, 413, 569, 595, 688, 790, 901, 1073, 1219, 1262, 1286, 1469, 1594, 1687,
];

/// The pilot reference sequence: `1 − 2·w_k` for carrier k, from the PRBS
/// x¹¹ + x² + 1 started at all ones (one bit per carrier, pilot or not).
fn prbs_signs(n: usize) -> Vec<f32> {
    let mut reg: u32 = 0x7FF;
    (0..n)
        .map(|_| {
            let w = reg & 1;
            let fb = (reg ^ (reg >> 2)) & 1;
            reg = (reg >> 1) | (fb << 10);
            if w == 0 { 1.0 } else { -1.0 }
        })
        .collect()
}

/// The symbol interleaver's permutation H(q) (EN 300 744 §4.3.4.2).
fn symbol_permutation(mode: Mode) -> Vec<u32> {
    let nr = mode.fft_len().trailing_zeros() as usize;
    let mmax = mode.fft_len();
    let nmax = mode.data_carriers();
    // R' bit j → R bit perm[j].
    let perm: &[usize] = match mode {
        // R' bits 9 8 7 6 5 4 3 2 1 0 → R bits 0 7 5 1 8 2 6 9 3 4.
        Mode::K2 => &[4, 3, 9, 6, 2, 8, 1, 5, 7, 0],
        // R' bits 11 … 0 → R bits 5 11 3 0 10 8 6 9 2 4 1 7.
        Mode::K8 => &[7, 1, 4, 2, 9, 6, 8, 10, 0, 3, 11, 5],
    };
    let mut out = Vec::with_capacity(nmax);
    let mut r: u32 = 0;
    for i in 0..mmax {
        let rp = match i {
            0 | 1 => 0,
            2 => 1,
            _ => {
                let fb = match mode {
                    Mode::K2 => (r ^ (r >> 3)) & 1,
                    Mode::K8 => (r ^ (r >> 1) ^ (r >> 4) ^ (r >> 6)) & 1,
                };
                (r >> 1) | (fb << (nr - 2))
            }
        };
        r = rp;
        let mut ri = 0u32;
        for (j, &to) in perm.iter().enumerate() {
            ri |= ((rp >> j) & 1) << to;
        }
        let h = ((i % 2) << (nr - 1)) as u32 + ri;
        if (h as usize) < nmax {
            out.push(h);
        }
    }
    debug_assert_eq!(out.len(), nmax);
    out
}

/// Where each carrier of a mode sits and what it carries.
pub struct Layout {
    pub mode: Mode,
    /// FFT size.
    pub n: usize,
    /// Carriers (kmax + 1).
    pub k: usize,
    /// FFT bin of carrier 0 (carrier kmax/2 is the centre, bin 0).
    pub bin0: usize,
    pub continual: Vec<u32>,
    pub tps: Vec<u32>,
    /// `1 − 2·w_k` per carrier.
    pub prbs: Vec<f32>,
    /// Data carriers in increasing order, per symbol index mod 4.
    pub data: [Vec<u32>; 4],
    /// The symbol interleaver H(q), and its inverse.
    pub h: Vec<u32>,
    pub h_inv: Vec<u32>,
}

impl Layout {
    pub fn new(mode: Mode) -> Self {
        let k = mode.carriers();
        let n = mode.fft_len();
        let repeat = |base: &[u16]| -> Vec<u32> {
            let mut v: Vec<u32> = (0..k / 1704)
                .flat_map(|j| base.iter().map(move |&p| u32::from(p) + 1704 * j as u32))
                .filter(|&p| (p as usize) < k)
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let continual = repeat(&CONTINUAL_2K);
        let tps = repeat(&TPS_2K);
        let mut fixed = vec![false; k];
        for &p in continual.iter().chain(&tps) {
            fixed[p as usize] = true;
        }
        let data = std::array::from_fn(|lm| {
            (0..k as u32)
                .filter(|&c| !fixed[c as usize] && (c as usize) % 12 != 3 * lm)
                .collect()
        });
        let h = symbol_permutation(mode);
        let mut h_inv = vec![0u32; h.len()];
        for (q, &v) in h.iter().enumerate() {
            h_inv[v as usize] = q as u32;
        }
        Self {
            mode,
            n,
            k,
            bin0: n - mode.kmax() / 2,
            continual,
            tps,
            prbs: prbs_signs(k),
            data,
            h,
            h_inv,
        }
    }

    /// FFT bin of carrier `c`.
    #[inline]
    pub fn bin(&self, c: usize) -> usize {
        (c + self.bin0) % self.n
    }

    /// Carrier index relative to the centre (k').
    #[inline]
    pub fn rel(&self, c: usize) -> f32 {
        c as f32 - (self.k / 2) as f32
    }
}

// ---- Inner interleaving and mapping ---------------------------------------------

/// Bit demultiplexer order per constellation (input bit j of a group goes to
/// stream SIGMA[j]).
const SIGMA: [&[usize]; 3] = [&[0, 1], &[0, 2, 1, 3], &[0, 2, 4, 1, 3, 5]];
/// Bit interleaver offsets: H_e(w) = (w + OFFSET[e]) mod 126.
const OFFSET: [usize; 6] = [0, 63, 105, 42, 21, 84];
const BIT_BLOCK: usize = 126;

/// The inner interleaver of one mode and constellation as two gathers (even
/// and odd symbols): coded bit i of a symbol is bit `e` of data carrier `c`
/// (in carrier order), stored as `c·v + e`.
pub struct InnerInterleaver {
    pub bits: usize,
    pub perm: [Vec<u32>; 2],
}

impl InnerInterleaver {
    pub fn new(layout: &Layout, c: Constellation) -> Self {
        let v = c.bits();
        let sigma = SIGMA[c.index()];
        let total = v * layout.mode.data_carriers();
        let perm = std::array::from_fn(|odd| {
            (0..total)
                .map(|i| {
                    let blk = i / (BIT_BLOCK * v);
                    let within = i % (BIT_BLOCK * v);
                    let (w, j) = (within / v, within % v);
                    let e = sigma[j];
                    let wp = (w + BIT_BLOCK - OFFSET[e]) % BIT_BLOCK;
                    let q = blk * BIT_BLOCK + wp;
                    let carrier = if odd == 0 {
                        layout.h[q]
                    } else {
                        layout.h_inv[q]
                    };
                    carrier * v as u32 + e as u32
                })
                .collect()
        });
        Self { bits: total, perm }
    }
}

/// All inner interleavers of a mode, indexed by constellation.
pub struct ModeTables {
    pub layout: Layout,
    pub inner: [InnerInterleaver; 3],
}

impl ModeTables {
    pub fn new(mode: Mode) -> Self {
        let layout = Layout::new(mode);
        let inner = Constellation::ALL.map(|c| InnerInterleaver::new(&layout, c));
        Self { layout, inner }
    }

    pub fn inner(&self, c: Constellation) -> &InnerInterleaver {
        &self.inner[c.index()]
    }
}

/// Tables for both modes, built on first use.
pub fn tables(mode: Mode) -> &'static ModeTables {
    static T: OnceLock<[ModeTables; 2]> = OnceLock::new();
    &T.get_or_init(|| Mode::ALL.map(ModeTables::new))[mode.index()]
}

/// The level (±1, ±3, ±5, ±7) of one axis: sign bit, then the amplitude
/// bits (0 = outer).
#[inline]
fn level(c: Constellation, s: u8, a: u8, b: u8) -> f32 {
    let sign = 1.0 - 2.0 * f32::from(s & 1);
    sign * match c {
        Constellation::Qpsk => 1.0,
        Constellation::Qam16 => 3.0 - 2.0 * f32::from(a & 1),
        Constellation::Qam64 => {
            let (a, b) = (1.0 - 2.0 * f32::from(a & 1), 1.0 - 2.0 * f32::from(b & 1));
            4.0 + a * (2.0 + b)
        }
    }
}

/// The constellation point for bits y0 … y(v−1) (one per byte), unit mean
/// power.
#[inline]
pub fn map(c: Constellation, y: &[u8]) -> (f32, f32) {
    let k = match c {
        Constellation::Qpsk => FRAC_1_SQRT_2,
        Constellation::Qam16 => 1.0 / 10f32.sqrt(),
        Constellation::Qam64 => 1.0 / 42f32.sqrt(),
    };
    let (re, im) = match c {
        Constellation::Qpsk => (level(c, y[0], 0, 0), level(c, y[1], 0, 0)),
        Constellation::Qam16 => (level(c, y[0], y[2], 0), level(c, y[1], y[3], 0)),
        Constellation::Qam64 => (level(c, y[0], y[2], y[4]), level(c, y[1], y[3], y[5])),
    };
    (re * k, im * k)
}

/// Max-log soft values (positive = 0) of the bits of one axis value `r` on
/// the odd-integer grid, written to `out[0]`, `out[2]`, `out[4]` (the axis'
/// bits in y order), scaled by `w`.
#[inline]
pub fn demap_axis(c: Constellation, r: f32, w: f32, out: &mut [f32]) {
    out[0] = r * w;
    match c {
        Constellation::Qpsk => {}
        Constellation::Qam16 => out[2] = (r.abs() - 2.0) * w,
        Constellation::Qam64 => {
            let a = r.abs() - 4.0;
            out[2] = a * w;
            out[4] = (a.abs() - 2.0) * w;
        }
    }
}

/// The nearest grid level to `r` for a constellation.
#[inline]
pub fn slice(c: Constellation, r: f32) -> f32 {
    let max = match c {
        Constellation::Qpsk => 1.0,
        Constellation::Qam16 => 3.0,
        Constellation::Qam64 => 7.0,
    };
    // Truncating an offset value is floor without a libm call; anything
    // below the offset ends up clamped anyway.
    let q = (r * 0.5 + 64.0) as i32 - 64;
    ((2 * q + 1) as f32).clamp(-max, max)
}

// ---- TPS --------------------------------------------------------------------------

/// The TPS synchronisation word (s1 … s16) of frames 1 and 3; frames 2 and 4
/// send it inverted.
pub const TPS_SYNC: u16 = 0b0011_0101_1110_1110;
/// BCH(67,53) generator x¹⁴ + x⁹ + x⁸ + x⁶ + x⁵ + x⁴ + x² + x + 1.
const BCH_POLY: u128 = 0x4377;
/// The TPS length indicator s17 … s22 (EN 300 744 §4.6.2.3): 31 bits used,
/// because the cell id (s40 … s47) is always sent. (010111, 23 bits, is for
/// transmitters without cell identification.) The receiver ignores it.
const TPS_LEN_INDICATOR: u64 = 0b011111;

/// What the TPS of one frame says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tps {
    /// Frame number in the superframe, 0 … 3.
    pub frame: u8,
    pub constellation: Constellation,
    /// Hierarchy α (0 = non-hierarchical).
    pub hierarchy: u8,
    pub rate_hp: CodeRate,
    pub rate_lp: CodeRate,
    pub guard: Guard,
    pub mode: Mode,
    /// Cell id byte: the high byte in frames 1 and 3 (0, 2), the low byte in
    /// frames 2 and 4.
    pub cell_id_byte: u8,
}

fn rate_bits(r: CodeRate) -> u64 {
    CodeRate::ALL.iter().position(|&x| x == r).unwrap_or(0) as u64
}

/// Remainder of `word` (degree < 67) divided by the BCH generator.
fn bch_rem(mut word: u128) -> u128 {
    for i in (14..67).rev() {
        if (word >> i) & 1 == 1 {
            word ^= BCH_POLY << (i - 14);
        }
    }
    word
}

impl Tps {
    /// The 68 TPS bits of this frame (s0 … s67, one per byte); s0, the
    /// differential reference, is 0.
    pub fn bits(&self) -> [u8; FRAME_SYMBOLS] {
        let sync = if self.frame.is_multiple_of(2) {
            u64::from(TPS_SYNC)
        } else {
            u64::from(!TPS_SYNC)
        };
        let guard = match self.guard {
            Guard::G1_32 => 0,
            Guard::G1_16 => 1,
            Guard::G1_8 => 2,
            Guard::G1_4 => 3,
        };
        let mode = match self.mode {
            Mode::K2 => 0,
            Mode::K8 => 1,
        };
        // s1 … s53, s1 the most significant.
        let fields: [(u64, u32); 12] = [
            (sync, 16),
            (TPS_LEN_INDICATOR, 6),
            (u64::from(self.frame), 2),
            (self.constellation.index() as u64, 2),
            (u64::from(self.hierarchy), 3),
            (rate_bits(self.rate_hp), 3),
            (rate_bits(self.rate_lp), 3),
            (guard, 2),
            (mode, 2),
            (u64::from(self.cell_id_byte), 8),
            (0, 6),
            (0, 0),
        ];
        let mut info = 0u128;
        for (v, n) in fields {
            info = (info << n) | u128::from(v & ((1u64 << n) - 1));
        }
        let word = (info << 14) | bch_rem(info << 14);
        let mut out = [0u8; FRAME_SYMBOLS];
        for (i, b) in out.iter_mut().enumerate().skip(1) {
            *b = ((word >> (67 - i)) & 1) as u8;
        }
        out
    }

    /// Decode s1 … s67 (bits 1.. of `bits`), correcting up to 2 errors.
    /// Returns the TPS and the number of bits corrected.
    pub fn decode(bits: &[u8; FRAME_SYMBOLS]) -> Option<(Self, u32)> {
        let mut word = 0u128;
        for &b in &bits[1..] {
            word = (word << 1) | u128::from(b & 1);
        }
        let syn = bch_rem(word) as usize;
        let mut fixed = 0;
        if syn != 0 {
            let e = bch_table()[syn];
            if e == 0 {
                return None;
            }
            let (p, q) = ((e & 0x7F) as u32, (e >> 7) as u32);
            word ^= 1u128 << (p - 1);
            fixed = 1;
            if q != 0 {
                word ^= 1u128 << (q - 1);
                fixed = 2;
            }
        }
        let field = |from: u32, n: u32| -> u64 {
            // `from`: index s_from (1-based) of the field's first bit.
            ((word >> (67 - from - n + 1)) & ((1u128 << n) - 1)) as u64
        };
        let sync = field(1, 16) as u16;
        let frame = field(23, 2) as u8;
        let expect = if frame.is_multiple_of(2) {
            TPS_SYNC
        } else {
            !TPS_SYNC
        };
        if sync != expect {
            return None;
        }
        let constellation = *Constellation::ALL.get(field(25, 2) as usize)?;
        let hierarchy = field(27, 3) as u8;
        let rate_hp = *CodeRate::ALL.get(field(30, 3) as usize)?;
        let rate_lp = *CodeRate::ALL.get(field(33, 3) as usize)?;
        let guard = [Guard::G1_32, Guard::G1_16, Guard::G1_8, Guard::G1_4][field(36, 2) as usize];
        let mode = match field(38, 2) {
            0 => Mode::K2,
            1 => Mode::K8,
            _ => return None,
        };
        Some((
            Self {
                frame,
                constellation,
                hierarchy,
                rate_hp,
                rate_lp,
                guard,
                mode,
                cell_id_byte: field(40, 8) as u8,
            },
            fixed,
        ))
    }
}

/// Syndrome → error positions (1-based bit numbers of the 67-bit word,
/// p | q << 7), for every pattern of one or two errors.
fn bch_table() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = vec![0u16; 1 << 14];
        for p in 0..67u32 {
            t[bch_rem(1 << p) as usize] = (p + 1) as u16;
            for q in p + 1..67 {
                t[bch_rem((1u128 << p) | (1u128 << q)) as usize] =
                    ((p + 1) | ((q + 1) << 7)) as u16;
            }
        }
        t
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_counts() {
        for mode in Mode::ALL {
            let l = Layout::new(mode);
            let (cp, tps) = match mode {
                Mode::K2 => (45, 17),
                Mode::K8 => (177, 68),
            };
            assert_eq!(l.continual.len(), cp);
            assert_eq!(l.tps.len(), tps);
            for d in &l.data {
                assert_eq!(d.len(), mode.data_carriers(), "{mode:?}");
            }
            // TPS never coincides with a scattered pilot.
            assert!(l.tps.iter().all(|&p| p % 3 != 0));
        }
        assert_eq!(
            &Layout::new(Mode::K8).tps[17..20],
            &[1738, 1754, 1913],
            "8k TPS repeat the 2k ones every 1704"
        );
    }

    #[test]
    fn prbs_starts_with_ones() {
        let p = prbs_signs(16);
        assert!(p[..11].iter().all(|&v| v == -1.0));
        assert_eq!(p[11], 1.0);
    }

    #[test]
    fn symbol_permutation_is_a_permutation() {
        for mode in Mode::ALL {
            let mut h = symbol_permutation(mode);
            assert_eq!(h[0], 0);
            h.sort_unstable();
            assert!(h.iter().enumerate().all(|(i, &v)| v as usize == i));
        }
        // Known first values (2k): H(1) = 1024 + R_1 = 1024, H(2) = R_2 with R'_2 = 1 → bit 4.
        let h = symbol_permutation(Mode::K2);
        assert_eq!(&h[..3], &[0, 1024, 16]);
    }

    #[test]
    fn inner_interleaver_is_a_permutation() {
        let t = tables(Mode::K2);
        for c in Constellation::ALL {
            for p in &t.inner(c).perm {
                let mut v = p.clone();
                v.sort_unstable();
                assert!(v.iter().enumerate().all(|(i, &x)| x as usize == i));
            }
        }
    }

    #[test]
    fn mapping_and_demapping_agree() {
        for c in Constellation::ALL {
            let v = c.bits();
            let k = c.grid_power().sqrt();
            let mut power = 0.0;
            for word in 0..(1u32 << v) {
                let y: Vec<u8> = (0..v).map(|i| ((word >> i) & 1) as u8).collect();
                let (re, im) = map(c, &y);
                power += re * re + im * im;
                let mut soft = [0f32; 6];
                demap_axis(c, re * k, 1.0, &mut soft[0..]);
                demap_axis(c, im * k, 1.0, &mut soft[1..]);
                for i in 0..v {
                    assert_eq!(soft[i] < 0.0, y[i] == 1, "{c:?} {word:b} bit {i}");
                }
                assert_eq!(slice(c, re * k * 1.1), (re * k).round());
            }
            let mean = power / (1 << v) as f32;
            assert!((mean - 1.0).abs() < 1e-5, "{c:?} {mean}");
        }
    }

    #[test]
    fn bch_corrects_two_errors_and_syndromes_are_unique() {
        let t = bch_table();
        // 67 single + 2211 double patterns, all distinct syndromes.
        assert_eq!(t.iter().filter(|&&v| v != 0).count(), 67 + 67 * 66 / 2);
        let tps = Tps {
            frame: 2,
            constellation: Constellation::Qam64,
            hierarchy: 0,
            rate_hp: CodeRate::R2_3,
            rate_lp: CodeRate::R1_2,
            guard: Guard::G1_8,
            mode: Mode::K8,
            cell_id_byte: 0xA5,
        };
        let mut bits = tps.bits();
        assert_eq!(&bits[1..5], &[0, 0, 1, 1]);
        assert_eq!(Tps::decode(&bits), Some((tps, 0)));
        bits[5] ^= 1;
        bits[60] ^= 1;
        assert_eq!(Tps::decode(&bits), Some((tps, 2)));
        bits[30] ^= 1;
        assert_ne!(Tps::decode(&bits).map(|t| t.0), Some(tps));
    }
}
