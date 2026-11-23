//! The CCSDS convolutional code (CCSDS 131.0-B §3): rate 1/2, K = 7,
//! G1 = 171₈ then G2 = 133₈, **the G2 output inverted**. It is DVB's inner
//! code ([`crate::dvb::conv`]) with every second symbol inverted, so the
//! decoder is DVB's soft Viterbi decoder ([`crate::dvb::viterbi`]) fed the
//! second soft bit of each pair negated. Some missions send G2 as it is
//! (`invert = false`); GNU Radio's `encode_ccsds_27_bb` also doesn't invert.

use crate::dvb::conv::outputs;
use crate::dvb::viterbi::Viterbi;

/// The encoder: one data bit in, the (G1, G2) channel bits out.
#[derive(Debug, Clone)]
pub struct ConvEncoder {
    state: u8,
    invert: u8,
}

impl ConvEncoder {
    /// `invert`: invert the G2 output (CCSDS).
    pub fn new(invert: bool) -> Self {
        Self {
            state: 0,
            invert: u8::from(invert),
        }
    }

    /// Start over from the all-zero state.
    pub fn reset(&mut self) {
        self.state = 0;
    }

    /// Set the register to the last six data bits (newest in bit 0).
    pub fn set_state(&mut self, state: u8) {
        self.state = state & 0x3F;
    }

    /// Encode one bit; returns (G1, G2) as 0/1.
    #[inline]
    pub fn push(&mut self, bit: u8) -> [u8; 2] {
        let reg = (self.state << 1) | (bit & 1);
        self.state = reg & 0x3F;
        let (x, y) = outputs(reg);
        [x, y ^ self.invert]
    }

    /// Encode a byte, most significant bit first; `out` gets 16 channel bits.
    #[inline]
    pub fn push_byte(&mut self, byte: u8, mut out: impl FnMut(u8)) {
        for i in (0..8).rev() {
            let [a, b] = self.push(byte >> i);
            out(a);
            out(b);
        }
    }
}

/// Soft-decision decoder: DVB's Viterbi decoder with the G2 inversion undone
/// on the way in.
pub struct ConvDecoder {
    viterbi: Viterbi,
    invert: bool,
    /// Pairs waiting for the decoder, G2 already un-inverted.
    pairs: Vec<[i8; 2]>,
}

/// Pairs handed to the Viterbi decoder at a time.
const BATCH: usize = 1024;

impl ConvDecoder {
    pub fn new(invert: bool) -> Self {
        Self {
            viterbi: Viterbi::new(),
            invert,
            pairs: Vec::with_capacity(BATCH),
        }
    }

    /// Forget the trellis (a new stream); the BER counters keep counting.
    pub fn reset(&mut self) {
        self.pairs.clear();
        self.viterbi.reset();
    }

    /// Feed one soft pair (G1, G2; positive = 0, magnitude ≤ 127). Decoded
    /// bits come out of `out` in chunks, 128 to 384 pairs later.
    #[inline]
    pub fn push(&mut self, pair: [i8; 2], out: &mut impl FnMut(&[u8])) {
        let y = if self.invert { -pair[1] } else { pair[1] };
        self.pairs.push([pair[0], y]);
        if self.pairs.len() == BATCH {
            self.viterbi.decode(&self.pairs, &mut *out);
            self.pairs.clear();
        }
    }

    /// Decode whatever is waiting, then release every pending bit (the end of
    /// a stream).
    pub fn flush(&mut self, out: &mut impl FnMut(&[u8])) {
        self.viterbi.decode(&self.pairs, &mut *out);
        self.pairs.clear();
        self.viterbi.flush(&mut *out);
    }

    /// Received channel bits compared with the re-encoded output, and how many
    /// differed (the channel bit error rate before decoding), since the last
    /// [`Self::take_counts`].
    pub fn take_counts(&mut self) -> (u64, u64) {
        let c = (self.viterbi.compared, self.viterbi.errors);
        self.viterbi.reset_counters();
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impulse_response_is_g1_and_inverted_g2() {
        let mut e = ConvEncoder::new(true);
        let mut g1 = Vec::new();
        let mut g2 = Vec::new();
        for i in 0..7 {
            let [a, b] = e.push(u8::from(i == 0));
            g1.push(a);
            g2.push(b);
        }
        let taps = |o: u32| {
            (0..7)
                .rev()
                .map(|i| ((o >> i) & 1) as u8)
                .collect::<Vec<_>>()
        };
        assert_eq!(g1, taps(0o171));
        assert_eq!(g2, taps(0o133).iter().map(|b| b ^ 1).collect::<Vec<_>>());
    }

    #[test]
    fn decodes_its_own_output() {
        for invert in [false, true] {
            let mut e = ConvEncoder::new(invert);
            let data: Vec<u8> = (0..4000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 & 1)
                .collect();
            let mut d = ConvDecoder::new(invert);
            let mut got = Vec::new();
            let mut out = |b: &[u8]| got.extend_from_slice(b);
            for &b in &data {
                let [x, y] = e.push(b);
                let s = |v: u8| if v == 0 { 60 } else { -60 };
                d.push([s(x), s(y)], &mut out);
            }
            d.flush(&mut out);
            assert_eq!(got, data);
            assert_eq!(d.take_counts().1, 0);
        }
    }
}
