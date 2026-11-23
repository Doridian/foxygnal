//! The inner code of DVB-S and DVB-T (EN 300 421 §4.4.4): a rate-1/2
//! convolutional code, constraint length 7, generators G1 = 171₈ (output X)
//! and G2 = 133₈ (output Y), punctured to 2/3, 3/4, 5/6 or 7/8.
//!
//! Puncturing patterns (1 = sent), and the order the kept bits go out in:
//!
//! | rate | X | Y | sent |
//! |---|---|---|---|
//! | 1/2 | 1 | 1 | X1 Y1 |
//! | 2/3 | 10 | 11 | X1 Y1 Y2 |
//! | 3/4 | 101 | 110 | X1 Y1 Y2 X3 |
//! | 5/6 | 10101 | 11010 | X1 Y1 Y2 X3 Y4 X5 |
//! | 7/8 | 1000101 | 1111010 | X1 Y1 Y2 Y3 Y4 X5 Y6 X7 |
//!
//! In DVB-S the sent bits go alternately to I and Q (I first).
//!
//! Register convention used here: the 7-bit register holds the newest input
//! bit in bit 0, so the generators appear bit-reversed: 171₈ → `0x4F`,
//! 133₈ → `0x6D` (as in most software Viterbi decoders).

/// G1 = 171₈, bit-reversed (newest bit in bit 0).
pub const POLY_X: u8 = 0x4F;
/// G2 = 133₈, bit-reversed.
pub const POLY_Y: u8 = 0x6D;

/// A code rate of the punctured convolutional code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeRate {
    R1_2,
    R2_3,
    R3_4,
    R5_6,
    R7_8,
}

impl CodeRate {
    pub const ALL: [CodeRate; 5] = [
        CodeRate::R1_2,
        CodeRate::R2_3,
        CodeRate::R3_4,
        CodeRate::R5_6,
        CodeRate::R7_8,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CodeRate::R1_2 => "1/2",
            CodeRate::R2_3 => "2/3",
            CodeRate::R3_4 => "3/4",
            CodeRate::R5_6 => "5/6",
            CodeRate::R7_8 => "7/8",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.name() == s)
    }

    /// (input bits, sent bits) per puncturing period.
    pub fn ratio(self) -> (usize, usize) {
        match self {
            CodeRate::R1_2 => (1, 2),
            CodeRate::R2_3 => (2, 3),
            CodeRate::R3_4 => (3, 4),
            CodeRate::R5_6 => (5, 6),
            CodeRate::R7_8 => (7, 8),
        }
    }

    pub fn value(self) -> f64 {
        let (k, n) = self.ratio();
        k as f64 / n as f64
    }

    /// Which of X and Y are sent for each input bit of a period.
    fn pattern(self) -> (&'static [u8], &'static [u8]) {
        match self {
            CodeRate::R1_2 => (&[1], &[1]),
            CodeRate::R2_3 => (&[1, 0], &[1, 1]),
            CodeRate::R3_4 => (&[1, 0, 1], &[1, 1, 0]),
            CodeRate::R5_6 => (&[1, 0, 1, 0, 1], &[1, 1, 0, 1, 0]),
            CodeRate::R7_8 => (&[1, 0, 0, 0, 1, 0, 1], &[1, 1, 1, 1, 0, 1, 0]),
        }
    }
}

/// One sent bit of a puncturing period.
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    /// 0 = X, 1 = Y.
    which: u8,
    /// The last sent bit of its input bit.
    ends_step: bool,
    /// Which input bit of the period it belongs to.
    step: u8,
}

/// The sent bits of one period, in order.
#[derive(Debug, Clone, Copy)]
pub struct Puncture {
    slots: [Slot; 8],
    n: usize,
    k: usize,
    keep: [[bool; 2]; 8],
}

impl Puncture {
    pub fn new(rate: CodeRate) -> Self {
        let (px, py) = rate.pattern();
        let mut slots = [Slot::default(); 8];
        let mut keep = [[false; 2]; 8];
        let mut n = 0;
        for i in 0..px.len() {
            keep[i] = [px[i] == 1, py[i] == 1];
            if px[i] == 1 {
                slots[n] = Slot {
                    which: 0,
                    ends_step: py[i] == 0,
                    step: i as u8,
                };
                n += 1;
            }
            if py[i] == 1 {
                slots[n] = Slot {
                    which: 1,
                    ends_step: true,
                    step: i as u8,
                };
                n += 1;
            }
        }
        Self {
            slots,
            n,
            k: px.len(),
            keep,
        }
    }

    /// Sent bits per period.
    pub fn period(&self) -> usize {
        self.n
    }

    /// Start offsets (in sent bits) a receiver has to try when every
    /// modulation symbol carries `bits_per_symbol` sent bits and the period
    /// starts on a symbol boundary at the transmitter.
    pub fn phases(&self, bits_per_symbol: usize) -> impl Iterator<Item = usize> + use<> {
        let n = self.n;
        let mut seen = [false; 8];
        (0..n * bits_per_symbol)
            .step_by(bits_per_symbol)
            .map(move |p| p % n)
            .filter(move |&p| !std::mem::replace(&mut seen[p], true))
    }
}

#[inline]
fn parity(x: u8) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The encoder outputs (X, Y) for 7-bit register `reg` (newest bit in bit 0).
#[inline]
pub fn outputs(reg: u8) -> (u8, u8) {
    (parity(reg & POLY_X), parity(reg & POLY_Y))
}

/// Convolutional encoder plus puncturing: input bits in, sent bits out.
pub struct ConvEncoder {
    state: u8,
    punct: Puncture,
    step: usize,
}

impl ConvEncoder {
    pub fn new(rate: CodeRate) -> Self {
        Self {
            state: 0,
            punct: Puncture::new(rate),
            step: 0,
        }
    }

    /// Encode one input bit; `out` gets the 1 or 2 sent bits it produces.
    #[inline]
    pub fn push(&mut self, bit: u8, mut out: impl FnMut(u8)) {
        let reg = (self.state << 1) | (bit & 1);
        self.state = reg & 0x3F;
        let (x, y) = outputs(reg);
        let [kx, ky] = self.punct.keep[self.step];
        if kx {
            out(x);
        }
        if ky {
            out(y);
        }
        self.step += 1;
        if self.step == self.punct.k {
            self.step = 0;
        }
    }

    /// Encode a byte, most significant bit first.
    #[inline]
    pub fn push_byte(&mut self, byte: u8, mut out: impl FnMut(u8)) {
        for i in (0..8).rev() {
            self.push(byte >> i, &mut out);
        }
    }
}

/// Puts erasures (soft value 0) back where bits were punctured: sent soft
/// bits in, one (X, Y) pair per input bit of the code out.
#[derive(Debug, Clone, Copy)]
pub struct Depuncturer {
    punct: Puncture,
    pos: usize,
    cur: [i8; 2],
}

impl Depuncturer {
    /// `phase`: which sent bit of the period the first soft bit is. Starting
    /// in the middle of an input bit's pair makes that first pair a partial
    /// erasure, which the decoder handles like any other.
    pub fn new(rate: CodeRate, phase: usize) -> Self {
        let punct = Puncture::new(rate);
        Self {
            pos: phase % punct.n,
            punct,
            cur: [0; 2],
        }
    }

    pub fn rate_period(&self) -> usize {
        self.punct.n
    }

    /// Take one soft bit; returns a pair when it completes one.
    #[inline]
    pub fn push(&mut self, soft: i8) -> Option<[i8; 2]> {
        let slot = self.punct.slots[self.pos];
        self.pos += 1;
        if self.pos == self.punct.n {
            self.pos = 0;
        }
        self.cur[slot.which as usize] = soft;
        if slot.ends_step {
            let c = self.cur;
            self.cur = [0; 2];
            Some(c)
        } else {
            None
        }
    }

    /// [`Self::push`] for a slice, appending the pairs to `out`; whole
    /// periods go through without a branch per bit.
    pub fn push_slice(&mut self, soft: &[i8], out: &mut Vec<[i8; 2]>) {
        let mut rest = soft;
        while self.pos != 0 && !rest.is_empty() {
            if let Some(p) = self.push(rest[0]) {
                out.push(p);
            }
            rest = &rest[1..];
        }
        let (n, k) = (self.punct.n, self.punct.k);
        let slots = &self.punct.slots[..n];
        let mut chunks = rest.chunks_exact(n);
        for c in &mut chunks {
            let mut pairs = [[0i8; 2]; 8];
            for (&v, sl) in c.iter().zip(slots) {
                pairs[usize::from(sl.step)][usize::from(sl.which)] = v;
            }
            out.extend_from_slice(&pairs[..k]);
        }
        for &v in chunks.remainder() {
            if let Some(p) = self.push(v) {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_slice_matches_push() {
        for rate in CodeRate::ALL {
            for phase in 0..Puncture::new(rate).period() {
                let soft: Vec<i8> = (0..101).map(|i| (i * 7 % 255 - 127) as i8).collect();
                let mut a = Depuncturer::new(rate, phase);
                let want: Vec<[i8; 2]> = soft.iter().filter_map(|&v| a.push(v)).collect();
                let mut b = Depuncturer::new(rate, phase);
                let mut got = Vec::new();
                for part in soft.chunks(13) {
                    b.push_slice(part, &mut got);
                }
                assert_eq!(got, want, "{rate:?} phase {phase}");
            }
        }
    }

    /// Octal generator → bit sequence in time order (current input first).
    fn taps(octal: u32) -> Vec<u8> {
        (0..7).rev().map(|i| ((octal >> i) & 1) as u8).collect()
    }

    #[test]
    fn impulse_response_is_the_generators() {
        // A single 1 then zeros: X follows G1 = 171₈, Y follows G2 = 133₈
        // (EN 300 421 figure 3).
        let mut e = ConvEncoder::new(CodeRate::R1_2);
        let mut bits = Vec::new();
        e.push(1, |b| bits.push(b));
        for _ in 0..6 {
            e.push(0, |b| bits.push(b));
        }
        let x: Vec<u8> = bits.iter().step_by(2).copied().collect();
        let y: Vec<u8> = bits.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(x, taps(0o171));
        assert_eq!(y, taps(0o133));
    }

    #[test]
    fn puncturing_keeps_the_standard_order() {
        // Encode with every rate and compare with rate 1/2 punctured by hand.
        let input: Vec<u8> = (0..70).map(|i| ((i * 7 + i / 3) % 2) as u8).collect();
        let mut full = ConvEncoder::new(CodeRate::R1_2);
        let mut xy = Vec::new();
        for &b in &input {
            full.push(b, |v| xy.push(v));
        }
        let names = |rate| match rate {
            CodeRate::R1_2 => "X1 Y1",
            CodeRate::R2_3 => "X1 Y1 Y2",
            CodeRate::R3_4 => "X1 Y1 Y2 X3",
            CodeRate::R5_6 => "X1 Y1 Y2 X3 Y4 X5",
            CodeRate::R7_8 => "X1 Y1 Y2 Y3 Y4 X5 Y6 X7",
        };
        for rate in CodeRate::ALL {
            let (k, n) = rate.ratio();
            let mut e = ConvEncoder::new(rate);
            let mut got = Vec::new();
            for &b in &input {
                e.push(b, |v| got.push(v));
            }
            let order: Vec<(usize, usize)> = names(rate)
                .split(' ')
                .map(|s| {
                    let which = usize::from(s.starts_with('Y'));
                    (s[1..].parse::<usize>().unwrap() - 1, which)
                })
                .collect();
            assert_eq!(order.len(), n);
            let mut want = Vec::new();
            for p in 0..input.len() / k {
                for &(i, which) in &order {
                    want.push(xy[2 * (p * k + i) + which]);
                }
            }
            assert_eq!(got[..want.len()], want, "rate {}", rate.name());
        }
    }

    #[test]
    fn depuncturer_inverts_puncturing() {
        for rate in CodeRate::ALL {
            let p = Puncture::new(rate);
            let mut d = Depuncturer::new(rate, 0);
            let (k, n) = rate.ratio();
            assert_eq!(p.period(), n);
            let mut pairs = Vec::new();
            for i in 0..n * 3 {
                if let Some(pair) = d.push(i as i8 + 1) {
                    pairs.push(pair);
                }
            }
            assert_eq!(pairs.len(), 3 * k);
            let erased = pairs.iter().flatten().filter(|&&v| v == 0).count();
            assert_eq!(erased, 3 * (2 * k - n), "rate {}", rate.name());
        }
    }

    #[test]
    fn phases_on_symbol_boundaries() {
        let ph = |r| Puncture::new(r).phases(2).collect::<Vec<_>>();
        assert_eq!(ph(CodeRate::R1_2), [0]);
        assert_eq!(ph(CodeRate::R2_3), [0, 2, 1]);
        assert_eq!(ph(CodeRate::R3_4), [0, 2]);
        assert_eq!(ph(CodeRate::R5_6), [0, 2, 4]);
        assert_eq!(ph(CodeRate::R7_8), [0, 2, 4, 6]);
    }
}
