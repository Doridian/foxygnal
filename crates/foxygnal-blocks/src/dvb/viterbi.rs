//! Soft-decision Viterbi decoder for the K = 7, (171₈, 133₈) code of
//! [`super::conv`], punctured or not (punctured bits come in as erasures,
//! soft value 0, from [`super::conv::Depuncturer`]).
//!
//! - **Soft input**: one `i8` per code bit, positive meaning 0 (the QPSK
//!   point's sign), magnitude the confidence; 0 is an erasure. Keep values
//!   within ±127.
//! - **Metrics**: `i16` correlations, maximized, renormalized every step so
//!   they never overflow. The 32 butterflies are plain array code that LLVM
//!   vectorizes (compiled a second time for AVX2, picked at run time);
//!   decisions are stored as one byte per state and step.
//! - **Traceback**: from the best state, every 256 steps, over the last
//!   384 steps; the oldest 256 decisions are released. So a bit comes out 128
//!   to 384 steps after it went in.
//! - **BER estimate**: released bits are re-encoded and compared with the hard
//!   decisions of the code bits that were actually received, which measures
//!   the channel's bit error rate before decoding (what the decoder fixed).
//!
//! Nothing allocates after [`Viterbi::new`].

const STATES: usize = 64;
const HALF: usize = STATES / 2;
/// Steps before a decision is final.
const TRACEBACK: usize = 128;
/// Bits released per traceback.
const CHUNK: usize = 256;
/// Decision history in steps (a power of two, at least TRACEBACK + CHUNK).
const HIST: usize = 512;

/// Expected-output signs per butterfly: the transition from state j with
/// input 0 sends (X, Y); +1 for a 0 bit, −1 for a 1.
struct Signs {
    x: [i16; HALF],
    y: [i16; HALF],
}

const SIGNS: Signs = {
    let mut x = [0i16; HALF];
    let mut y = [0i16; HALF];
    let mut j = 0;
    while j < HALF {
        let reg = (j as u32) << 1;
        let px = (reg & super::conv::POLY_X as u32).count_ones() & 1;
        let py = (reg & super::conv::POLY_Y as u32).count_ones() & 1;
        x[j] = 1 - 2 * px as i16;
        y[j] = 1 - 2 * py as i16;
        j += 1;
    }
    Signs { x, y }
};

/// The encoder's (X, Y) outputs (bits 0, 1) for each 7-bit register.
const ENCODED: [u8; 128] = {
    let mut t = [0u8; 128];
    let mut r = 0;
    while r < 128 {
        let x = (r as u8 & super::conv::POLY_X).count_ones() as u8 & 1;
        let y = (r as u8 & super::conv::POLY_Y).count_ones() as u8 & 1;
        t[r] = x | (y << 1);
        r += 1;
    }
    t
};

pub struct Viterbi {
    metrics: [i16; STATES],
    /// `dec[t % HIST][s]` = 1 if state s at step t came from the predecessor
    /// with the oldest bit set.
    dec: Box<[[u8; STATES]; HIST]>,
    /// Hard decisions of the received code bits per step: bit 0 X, bit 1 Y,
    /// bit 2 X received, bit 3 Y received.
    rx: Box<[u8; HIST]>,
    /// Steps taken.
    t: usize,
    /// Steps whose bits have not been released.
    pending: usize,
    bits: [u8; HIST],
    /// Encoder state after the last released bit (for the BER estimate).
    reenc: u8,
    /// Received (not erased) code bits compared with the re-encoded output.
    pub compared: u64,
    /// Of those, how many differed.
    pub errors: u64,
}

impl Default for Viterbi {
    fn default() -> Self {
        Self::new()
    }
}

impl Viterbi {
    pub fn new() -> Self {
        let mut v = Self {
            metrics: [0; STATES],
            dec: Box::new([[0; STATES]; HIST]),
            rx: Box::new([0; HIST]),
            t: 0,
            pending: 0,
            bits: [0; HIST],
            reenc: 0,
            compared: 0,
            errors: 0,
        };
        v.reset();
        v
    }

    /// Forget the trellis: the next pair starts a new stream, in an unknown
    /// state. The counters keep counting.
    pub fn reset(&mut self) {
        self.metrics = [0; STATES];
        self.t = 0;
        self.pending = 0;
        self.reenc = 0;
    }

    /// Zero [`Self::compared`] and [`Self::errors`].
    pub fn reset_counters(&mut self) {
        self.compared = 0;
        self.errors = 0;
    }

    /// One trellis step for the soft (X, Y) pair of one input bit.
    #[inline(always)]
    fn step(&mut self, pair: [i8; 2]) {
        let (sx, sy) = (i16::from(pair[0]), i16::from(pair[1]));
        let mut bm = [0i16; HALF];
        for ((b, &gx), &gy) in bm.iter_mut().zip(&SIGNS.x).zip(&SIGNS.y) {
            *b = gx * sx + gy * sy;
        }
        let slot = self.t % HIST;
        let old = &self.metrics;
        let (lo, hi) = old.split_at(HALF);
        let mut even = [0i16; HALF];
        let mut odd = [0i16; HALF];
        let mut de = [0u8; HALF];
        let mut dodd = [0u8; HALF];
        for j in 0..HALF {
            let (a, b, m) = (lo[j], hi[j], bm[j]);
            let (m0, m1) = (a + m, b - m);
            let (m2, m3) = (a - m, b + m);
            even[j] = m0.max(m1);
            de[j] = u8::from(m1 > m0);
            odd[j] = m2.max(m3);
            dodd[j] = u8::from(m3 > m2);
        }
        let norm = even[0];
        let d = &mut self.dec[slot];
        for j in 0..HALF {
            self.metrics[2 * j] = even[j] - norm;
            self.metrics[2 * j + 1] = odd[j] - norm;
            d[2 * j] = de[j];
            d[2 * j + 1] = dodd[j];
        }
        self.rx[slot] = u8::from(pair[0] < 0)
            | (u8::from(pair[1] < 0) << 1)
            | (u8::from(pair[0] != 0) << 2)
            | (u8::from(pair[1] != 0) << 3);
        self.t += 1;
        self.pending += 1;
    }

    /// Decode pairs; `out` gets the released bits (one per byte, 0 or 1), in
    /// order, in chunks.
    pub fn decode(&mut self, pairs: &[[i8; 2]], out: impl FnMut(&[u8])) {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has AVX2 (checked just above).
            unsafe { self.decode_avx2(pairs, out) };
            return;
        }
        self.decode_portable(pairs, out);
    }

    /// [`Self::decode`] compiled for AVX2: the butterflies take half the
    /// instructions with 256-bit vectors.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn decode_avx2(&mut self, pairs: &[[i8; 2]], out: impl FnMut(&[u8])) {
        self.decode_portable(pairs, out);
    }

    #[inline(always)]
    fn decode_portable(&mut self, pairs: &[[i8; 2]], mut out: impl FnMut(&[u8])) {
        for &p in pairs {
            self.step(p);
            if self.pending == TRACEBACK + CHUNK {
                self.release(CHUNK, &mut out);
            }
        }
    }

    /// Release every pending bit (the end of a stream or a test buffer); the
    /// last ones are less reliable.
    pub fn flush(&mut self, mut out: impl FnMut(&[u8])) {
        let n = self.pending;
        if n > 0 {
            self.release(n, &mut out);
        }
    }

    /// Trace back from the best state through all pending steps and release
    /// the oldest `n`.
    fn release(&mut self, n: usize, out: &mut impl FnMut(&[u8])) {
        let mut s = 0usize;
        let mut best = i16::MIN;
        for (i, &m) in self.metrics.iter().enumerate() {
            if m > best {
                best = m;
                s = i;
            }
        }
        let depth = self.pending;
        let keep = depth - n;
        for back in 0..depth {
            let t = self.t - 1 - back;
            let slot = t % HIST;
            if back >= keep {
                self.bits[depth - 1 - back] = (s & 1) as u8;
            }
            s = (s >> 1) | (usize::from(self.dec[slot][s]) << 5);
        }
        // Re-encode and compare with what was received.
        let first = self.t - depth;
        let (mut cmp, mut err) = (0u32, 0u32);
        let mut reenc = usize::from(self.reenc);
        for i in 0..n {
            reenc = ((reenc << 1) | usize::from(self.bits[i])) & 0x7F;
            let r = self.rx[(first + i) % HIST];
            // Bits 2–3: X and Y received; bits 0–1 their hard decisions.
            let present = (r >> 2) & 3;
            cmp += u32::from((present & 1) + (present >> 1));
            let wrong = (ENCODED[reenc] ^ r) & present;
            err += u32::from((wrong & 1) + (wrong >> 1));
        }
        self.reenc = (reenc & 0x3F) as u8;
        self.compared += u64::from(cmp);
        self.errors += u64::from(err);
        self.pending -= n;
        out(&self.bits[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::super::conv::{CodeRate, ConvEncoder, Depuncturer};
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn uniform(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let (u, v) = (self.uniform().max(1e-300), self.uniform());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    /// Encode `n` random bits at `rate`, add Gaussian noise for `ebn0_db`
    /// (per information bit), decode; returns (bit errors, bits, the decoder's
    /// channel BER estimate, the true channel BER).
    fn run(rate: CodeRate, n: usize, ebn0_db: f64, seed: u64) -> (usize, usize, f64, f64) {
        let mut rng = Rng(seed);
        let bits: Vec<u8> = (0..n).map(|_| (rng.next() >> 40) as u8 & 1).collect();
        let mut enc = ConvEncoder::new(rate);
        let mut sent = Vec::new();
        for &b in &bits {
            enc.push(b, |c| sent.push(c));
        }
        // ±1 per code bit; Es per code bit = R·Eb.
        let ebn0 = 10f64.powf(ebn0_db / 10.0);
        let sigma = (1.0 / (2.0 * rate.value() * ebn0)).sqrt();
        let scale = 24.0;
        let mut chan_err = 0;
        let soft: Vec<i8> = sent
            .iter()
            .map(|&c| {
                let x = if c == 0 { 1.0 } else { -1.0 } + sigma * rng.gauss();
                chan_err += usize::from((x < 0.0) != (c == 1));
                (x * scale).round().clamp(-127.0, 127.0) as i8
            })
            .collect();
        let mut dp = Depuncturer::new(rate, 0);
        let pairs: Vec<[i8; 2]> = soft.iter().filter_map(|&s| dp.push(s)).collect();
        let mut v = Viterbi::new();
        let mut got = Vec::new();
        v.decode(&pairs, |b| got.extend_from_slice(b));
        v.flush(|b| got.extend_from_slice(b));
        assert_eq!(got.len(), n);
        // The last few bits have no tail; leave them out.
        let errs = got[..n - 64]
            .iter()
            .zip(&bits)
            .filter(|(a, b)| a != b)
            .count();
        (
            errs,
            n - 64,
            v.errors as f64 / v.compared as f64,
            chan_err as f64 / sent.len() as f64,
        )
    }

    #[test]
    fn decodes_noiseless_streams_at_every_rate() {
        for rate in CodeRate::ALL {
            let (errs, n, est, _) = run(rate, 5000, 100.0, 7);
            assert_eq!(errs, 0, "rate {} ({n} bits)", rate.name());
            assert_eq!(est, 0.0);
        }
    }

    #[test]
    fn corrects_channel_errors_at_the_expected_ber() {
        // EN 300 421 table A.1: QEF needs a BER of 2e-4 after Viterbi, at
        // Eb/N0 = 4.5 / 5.0 / 5.5 / 6.0 / 6.4 dB per useful bit for rates
        // 1/2…7/8. That includes a 0.8 dB implementation margin and the RS
        // overhead (0.36 dB), so an ideal decoder gets there about 1.15 dB
        // sooner, per bit into the convolutional code. Check that point,
        // allowing 2.5× for 8-bit soft values and a short run.
        for (rate, ebn0) in [
            (CodeRate::R1_2, 3.35),
            (CodeRate::R2_3, 3.85),
            (CodeRate::R3_4, 4.35),
            (CodeRate::R5_6, 4.85),
            (CodeRate::R7_8, 5.25),
        ] {
            let max_ber = 5e-4;
            let (errs, n, est, chan) = run(rate, 100_000, ebn0, 99);
            let ber = errs as f64 / n as f64;
            eprintln!(
                "rate {} Eb/N0 {ebn0}: BER {ber:.2e} (channel {chan:.4}, estimate {est:.4})",
                rate.name()
            );
            assert!(
                ber <= max_ber,
                "rate {}: BER {ber:.2e} at Eb/N0 {ebn0} dB (channel {chan:.3})",
                rate.name()
            );
            // The channel had plenty of errors that were fixed.
            assert!(chan > 0.005, "{chan}");
            // The estimate tracks the true channel BER.
            assert!((est / chan - 1.0).abs() < 0.2, "estimate {est} vs {chan}");
        }
    }

    /// Decoding speed (release builds: `cargo test --release -p
    /// foxygnal-blocks viterbi_speed -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn viterbi_speed() {
        let mut rng = Rng(3);
        let pairs: Vec<[i8; 2]> = (0..1 << 20)
            .map(|_| [(rng.next() >> 56) as i8, (rng.next() >> 56) as i8])
            .collect();
        let mut v = Viterbi::new();
        let t = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..4 {
            v.decode(&pairs, |b| n += b.len());
        }
        let s = t.elapsed().as_secs_f64();
        println!("viterbi: {:.1} Mbit/s", n as f64 / s / 1e6);
    }

    #[test]
    fn wrong_puncturing_phase_gives_a_high_ber_estimate() {
        let mut rng = Rng(5);
        let rate = CodeRate::R3_4;
        let mut enc = ConvEncoder::new(rate);
        let mut sent = Vec::new();
        for _ in 0..6000 {
            enc.push((rng.next() >> 33) as u8 & 1, |c| sent.push(c));
        }
        let soft: Vec<i8> = sent
            .iter()
            .map(|&c| if c == 0 { 40 } else { -40 })
            .collect();
        for (phase, ok) in [(0, true), (2, false)] {
            let mut dp = Depuncturer::new(rate, phase);
            let pairs: Vec<[i8; 2]> = soft.iter().filter_map(|&s| dp.push(s)).collect();
            let mut v = Viterbi::new();
            v.decode(&pairs, |_| {});
            let est = v.errors as f64 / v.compared as f64;
            assert_eq!(est < 0.01, ok, "phase {phase}: {est}");
        }
    }
}
