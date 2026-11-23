//! Soft-decision Viterbi decoder for the CS2 pragmatic trellis-coded 8PSK.
//!
//! 64 states (the K = 7 code on the lower bit of each pair). For every
//! symbol and each of the four coded labels E-1 E-0 the two candidate points
//! are 180° apart, so the branch metric is the larger of their correlations,
//! `|Re(r·e^{-jθ})|`, and the uncoded bit is the sign; it is kept per label
//! and read back once the survivor path says which label was sent. Decisions
//! come out a fixed [`DEPTH`] symbols late (sliding traceback), and
//! [`Tcm::finish`] flushes the rest.

use foxygnal_core::Complex32;

use super::frame::{LABEL_PHASE, label};

/// Traceback depth in symbols (≈ 9 constraint lengths).
pub const DEPTH: usize = 64;
const RING: usize = 128;

/// Unit vectors e^{-jθ} of the four labels.
fn label_rot() -> [Complex32; 4] {
    LABEL_PHASE.map(|p| {
        let a = -f32::from(p) * std::f32::consts::FRAC_PI_4;
        Complex32::new(a.cos(), a.sin())
    })
}

pub struct Tcm {
    metric: [f32; 64],
    next: [f32; 64],
    /// Per symbol, the predecessor choice of every state (bit `s`).
    dec: [u64; RING],
    /// Per symbol, the uncoded bit of each label (bit `c`).
    ubits: [u8; RING],
    /// Labels of the 128 register values.
    labels: [u8; 128],
    rot: [Complex32; 4],
    /// Symbols pushed, and symbols whose bits went out.
    t: usize,
    emitted: usize,
}

impl Default for Tcm {
    fn default() -> Self {
        Self::new()
    }
}

impl Tcm {
    pub fn new() -> Self {
        let mut labels = [0u8; 128];
        for (r, l) in labels.iter_mut().enumerate() {
            *l = label(r as u8);
        }
        let mut t = Self {
            metric: [0.0; 64],
            next: [0.0; 64],
            dec: [0; RING],
            ubits: [0; RING],
            labels,
            rot: label_rot(),
            t: 0,
            emitted: 0,
        };
        t.reset();
        t
    }

    /// Start a new message: the encoder starts in state 0.
    pub fn reset(&mut self) {
        self.metric = [-1e9; 64];
        self.metric[0] = 0.0;
        self.t = 0;
        self.emitted = 0;
    }

    /// Add one received symbol (unit amplitude, phase-corrected). Bit pairs
    /// (upper bit uncoded) that are now final go to `out`, in order.
    pub fn push(&mut self, r: Complex32, out: &mut impl FnMut(u8)) {
        let mut bm = [0.0f32; 4];
        let mut ub = 0u8;
        for c in 0..4 {
            let p = (r * self.rot[c]).re;
            bm[c] = p.abs();
            ub |= u8::from(p < 0.0) << c;
        }
        let slot = self.t % RING;
        let mut dec = 0u64;
        let mut best = f32::MIN;
        for ns in 0..64usize {
            let base = ns >> 1;
            let reg0 = ns;
            let reg1 = 64 | ns;
            let m0 = self.metric[base] + bm[self.labels[reg0] as usize];
            let m1 = self.metric[base | 32] + bm[self.labels[reg1] as usize];
            let m = if m1 > m0 {
                dec |= 1 << ns;
                m1
            } else {
                m0
            };
            self.next[ns] = m;
            best = best.max(m);
        }
        for (m, n) in self.metric.iter_mut().zip(&self.next) {
            *m = n - best;
        }
        self.dec[slot] = dec;
        self.ubits[slot] = ub;
        self.t += 1;
        while self.t - self.emitted > DEPTH {
            let pair = self.trace(self.best_state(), self.t - 1, self.emitted);
            out(pair);
            self.emitted += 1;
        }
    }

    fn best_state(&self) -> usize {
        let mut s = 0;
        for i in 1..64 {
            if self.metric[i] > self.metric[s] {
                s = i;
            }
        }
        s
    }

    /// Trace back from `state` at symbol `from` to symbol `to`; the pair
    /// decoded at `to`.
    fn trace(&self, mut state: usize, from: usize, to: usize) -> u8 {
        let mut t = from;
        loop {
            let slot = t % RING;
            let x = (self.dec[slot] >> state & 1) as usize;
            if t == to {
                let c = self.labels[(x << 6) | state];
                let u = self.ubits[slot] >> c & 1;
                return (u << 1) | (state & 1) as u8;
            }
            state = (state >> 1) | (x << 5);
            t -= 1;
        }
    }

    /// Emit every symbol still held back (the end of a message).
    pub fn finish(&mut self, out: &mut impl FnMut(u8)) {
        if self.t == self.emitted {
            return;
        }
        let mut pairs = [0u8; DEPTH];
        let n = self.t - self.emitted;
        let mut state = self.best_state();
        let mut t = self.t - 1;
        for i in (0..n).rev() {
            let slot = t % RING;
            let x = (self.dec[slot] >> state & 1) as usize;
            let c = self.labels[(x << 6) | state];
            pairs[i] = ((self.ubits[slot] >> c & 1) << 1) | (state & 1) as u8;
            state = (state >> 1) | (x << 5);
            t = t.wrapping_sub(1);
        }
        for &p in &pairs[..n] {
            out(p);
        }
        self.emitted = self.t;
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::TrellisEncoder;
    use super::*;
    use crate::sources::Rng;

    fn point(ph: u8) -> Complex32 {
        let a = f32::from(ph) * std::f32::consts::FRAC_PI_4;
        Complex32::new(a.cos(), a.sin())
    }

    fn run(snr_db: f32, n: usize, seed: u64) -> usize {
        let mut rng = Rng::new(seed);
        let pairs: Vec<u8> = (0..n).map(|_| (rng.next_u64() & 3) as u8).collect();
        let mut enc = TrellisEncoder::default();
        let sigma = (10f32.powf(-snr_db / 10.0) / 2.0).sqrt();
        let mut dec = Tcm::new();
        let mut got = Vec::new();
        for &p in &pairs {
            let (a, b) = rng.gaussian_pair();
            let r = point(enc.push(p)) + Complex32::new(a * sigma, b * sigma);
            dec.push(r, &mut |q| got.push(q));
        }
        dec.finish(&mut |q| got.push(q));
        assert_eq!(got.len(), n);
        got.iter().zip(&pairs).filter(|(a, b)| a != b).count()
    }

    #[test]
    fn decodes_clean_symbols_exactly() {
        assert_eq!(run(60.0, 1000, 1), 0);
    }

    #[test]
    fn corrects_noisy_symbols() {
        // Es/N0 12 dB: hard 8PSK decisions would be wrong ~1 % of the time.
        let errs = run(12.0, 20_000, 2);
        assert!(errs < 20, "{errs} pair errors");
    }
}
