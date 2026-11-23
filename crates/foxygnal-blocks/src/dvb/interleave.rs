//! Forney convolutional byte interleaver (EN 300 421 §4.4.3; also DVB-T and
//! DVB-C): I = 12 branches visited in turn, branch j a FIFO of j·M bytes
//! (M = 17), so a burst of errors after the inner decoder is spread over 12
//! RS codewords. The deinterleaver has branch j delay (11 − j)·M, so every
//! byte is delayed by the same 11·17·12 = 2244 bytes (11 packets of 204).
//!
//! Sync bytes always go through branch 0 (204 = 12·17), which has no delay
//! in the interleaver: they appear in the interleaved stream exactly where
//! they were, which is how a receiver finds the packet grid before
//! deinterleaving. Both sides must start branch 0 on a sync byte.
//!
//! The same structure with other sizes is ATSC's byte interleaver (A/53:
//! 52 branches, M = 4): [`ConvInterleaver::with`].

/// Branches.
pub const I: usize = 12;
/// FIFO length unit.
pub const M: usize = 17;
/// End-to-end delay in bytes.
pub const DELAY: usize = (I - 1) * M * I;

/// The most branches [`ConvInterleaver::with`] takes.
pub const MAX_BRANCHES: usize = 52;

/// One side of the interleaver pair: [`Self::interleaver`] or
/// [`Self::deinterleaver`].
pub struct ConvInterleaver {
    /// Every branch's FIFO, back to back.
    buf: Box<[u8]>,
    start: [usize; MAX_BRANCHES],
    len: [usize; MAX_BRANCHES],
    pos: [usize; MAX_BRANCHES],
    branches: usize,
    branch: usize,
}

impl ConvInterleaver {
    /// The DVB interleaver (I = 12, M = 17).
    pub fn interleaver() -> Self {
        Self::with(I, M, false)
    }

    /// The DVB deinterleaver.
    pub fn deinterleaver() -> Self {
        Self::with(I, M, true)
    }

    /// `branches` branches (at most [`MAX_BRANCHES`]), branch j a FIFO of
    /// j·m bytes, or (branches − 1 − j)·m for the `inverse`.
    pub fn with(branches: usize, m: usize, inverse: bool) -> Self {
        assert!((1..=MAX_BRANCHES).contains(&branches));
        let mut start = [0; MAX_BRANCHES];
        let mut len = [0; MAX_BRANCHES];
        let mut total = 0;
        for j in 0..branches {
            start[j] = total;
            len[j] = if inverse { branches - 1 - j } else { j } * m;
            total += len[j];
        }
        Self {
            buf: vec![0; total].into_boxed_slice(),
            start,
            len,
            pos: [0; MAX_BRANCHES],
            branches,
            branch: 0,
        }
    }

    /// Empty every FIFO and start again at branch 0.
    pub fn reset(&mut self) {
        self.buf.fill(0);
        self.pos = [0; MAX_BRANCHES];
        self.branch = 0;
    }

    /// Pass one byte through the branch whose turn it is.
    #[inline]
    pub fn push(&mut self, x: u8) -> u8 {
        let j = self.branch;
        self.branch = if j + 1 == self.branches { 0 } else { j + 1 };
        let len = self.len[j];
        if len == 0 {
            return x;
        }
        let p = self.pos[j];
        let at = self.start[j] + p;
        let out = self.buf[at];
        self.buf[at] = x;
        self.pos[j] = if p + 1 == len { 0 } else { p + 1 };
        out
    }

    /// [`Self::push`] every byte of `data`, in place.
    pub fn process(&mut self, data: &mut [u8]) {
        for b in data {
            *b = self.push(*b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_delays_every_byte_by_2244() {
        let mut il = ConvInterleaver::interleaver();
        let mut dil = ConvInterleaver::deinterleaver();
        let n = DELAY + 5000;
        let data: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
        let mut x = data.clone();
        il.process(&mut x);
        assert_ne!(x, data);
        dil.process(&mut x);
        assert_eq!(&x[DELAY..], &data[..n - DELAY]);
    }

    #[test]
    fn branch_zero_bytes_stay_in_place() {
        let mut il = ConvInterleaver::interleaver();
        let data: Vec<u8> = (0..204 * 20).map(|i| (i % 251) as u8).collect();
        let mut x = data.clone();
        il.process(&mut x);
        for p in 0..20 {
            assert_eq!(x[p * 204], data[p * 204]);
        }
        // Branch 1 is delayed by 17 visits of 12 bytes.
        assert_eq!(x[1 + 17 * 12], data[1]);
    }

    #[test]
    fn atsc_sizes_delay_every_byte_by_10608() {
        let mut il = ConvInterleaver::with(52, 4, false);
        let mut dil = ConvInterleaver::with(52, 4, true);
        let delay = 52 * 51 * 4;
        let n = delay + 3000;
        let data: Vec<u8> = (0..n).map(|i| (i * 13 + 5) as u8).collect();
        let mut x = data.clone();
        il.process(&mut x);
        dil.process(&mut x);
        assert_eq!(&x[delay..], &data[..n - delay]);
    }

    #[test]
    fn burst_is_spread_out() {
        // 12 consecutive wrong bytes after the channel land about a packet
        // apart after deinterleaving: one per RS codeword.
        let mut il = ConvInterleaver::interleaver();
        let mut dil = ConvInterleaver::deinterleaver();
        let n = DELAY + 204 * 30;
        let mut x = vec![0u8; n];
        il.process(&mut x);
        for b in &mut x[DELAY + 1000..DELAY + 1012] {
            *b ^= 0xFF;
        }
        dil.process(&mut x);
        let bad: Vec<usize> = (0..n).filter(|&i| x[i] != 0).collect();
        assert_eq!(bad.len(), 12);
        for w in bad.windows(2) {
            assert!(w[1] - w[0] >= 190, "{bad:?}");
        }
    }
}
