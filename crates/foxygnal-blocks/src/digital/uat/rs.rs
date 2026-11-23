//! Reed-Solomon codes of UAT (DO-282B §2.2.3): over GF(2⁸) with field
//! polynomial x⁸ + x⁷ + x² + x + 1 (`0x187`), α = `0x02`, generator
//! g(x) = Π (x − α^(120 + i)), i = 0 … nroots − 1, shortened from (255, 255 −
//! nroots); the data bytes come first, then the parity, first byte = highest
//! power.
//!
//! - ADS-B Basic: RS(30, 18), 12 parity bytes, corrects 6.
//! - ADS-B Long: RS(48, 34), 14 parity bytes, corrects 7.
//! - Ground uplink: six interleaved RS(92, 72) blocks, 20 parity bytes each,
//!   corrects 10 per block.
//!
//! Decoding computes the syndromes, and only when one isn't zero runs
//! Berlekamp-Massey, a Chien search over the positions that exist and
//! Forney's formula. Nothing allocates.

const POLY: u16 = 0x187;
/// First consecutive root of the generator.
const FCR: usize = 120;
/// Most parity bytes of any UAT code.
pub const MAX_ROOTS: usize = 20;

struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

const fn build_gf() -> Gf {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    exp[510] = exp[0];
    exp[511] = exp[1];
    Gf { exp, log }
}

static GF: Gf = build_gf();

#[inline]
fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        GF.exp[usize::from(GF.log[usize::from(a)]) + usize::from(GF.log[usize::from(b)])]
    }
}

#[inline]
fn div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0);
    if a == 0 {
        0
    } else {
        GF.exp[usize::from(GF.log[usize::from(a)]) + 255 - usize::from(GF.log[usize::from(b)])]
    }
}

/// α^e for any e ≥ 0.
#[inline]
fn pow(e: usize) -> u8 {
    GF.exp[e % 255]
}

/// A UAT Reed-Solomon code with `roots` parity bytes.
pub struct Code {
    roots: usize,
    /// Generator coefficients, `generator[d]` of x^d (monic).
    #[cfg_attr(not(test), allow(dead_code))]
    generator: [u8; MAX_ROOTS + 1],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncorrectable;

impl Code {
    pub const fn new(roots: usize) -> Self {
        let mut g = [0u8; MAX_ROOTS + 1];
        g[0] = 1;
        let mut i = 0;
        while i < roots {
            // Multiply by (x + α^(FCR + i)).
            let r = GF.exp[(FCR + i) % 255];
            let mut d = i + 1;
            while d > 0 {
                g[d] = g[d - 1] ^ mul_const(r, g[d]);
                d -= 1;
            }
            g[0] = mul_const(r, g[0]);
            i += 1;
        }
        Self {
            roots,
            generator: g,
        }
    }

    #[cfg(test)]
    pub fn roots(&self) -> usize {
        self.roots
    }

    /// Fill the last `roots` bytes of `cw` with parity for the bytes before
    /// (the receiver never encodes; the tests do).
    #[cfg(test)]
    pub fn encode(&self, cw: &mut [u8]) {
        let k = cw.len() - self.roots;
        let mut rem = [0u8; MAX_ROOTS];
        let r = self.roots;
        for &byte in &cw[..k] {
            let fb = byte ^ rem[0];
            for j in 0..r - 1 {
                rem[j] = rem[j + 1] ^ mul(fb, self.generator[r - 1 - j]);
            }
            rem[r - 1] = mul(fb, self.generator[0]);
        }
        cw[k..].copy_from_slice(&rem[..r]);
    }

    /// Correct `cw` (data then parity) in place. Returns the number of bytes
    /// corrected.
    pub fn decode(&self, cw: &mut [u8]) -> Result<usize, Uncorrectable> {
        let n = cw.len();
        let r = self.roots;
        // Syndromes S_j = c(α^(FCR + j)).
        let mut syn = [0u8; MAX_ROOTS];
        let mut any = false;
        for (j, s) in syn.iter_mut().enumerate().take(r) {
            let root = pow(FCR + j);
            let mut acc = 0u8;
            for &b in cw.iter() {
                acc = mul(acc, root) ^ b;
            }
            *s = acc;
            any |= acc != 0;
        }
        if !any {
            return Ok(0);
        }
        // Berlekamp-Massey: the error locator Λ(x).
        let mut lambda = [0u8; MAX_ROOTS + 1];
        let mut prev = [0u8; MAX_ROOTS + 1];
        lambda[0] = 1;
        prev[0] = 1;
        let (mut l, mut m, mut b) = (0usize, 1usize, 1u8);
        for k in 0..r {
            let mut d = syn[k];
            for i in 1..=l {
                d ^= mul(lambda[i], syn[k - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = div(d, b);
            let old = lambda;
            for i in m..=r {
                lambda[i] ^= mul(coef, prev[i - m]);
            }
            if 2 * l <= k {
                l = k + 1 - l;
                prev = old;
                b = d;
                m = 1;
            } else {
                m += 1;
            }
        }
        if l == 0 || 2 * l > r {
            return Err(Uncorrectable);
        }
        // Ω(x) = S(x)·Λ(x) mod x^r.
        let mut omega = [0u8; MAX_ROOTS];
        for i in 0..r {
            let mut v = 0u8;
            for j in 0..=i.min(l) {
                v ^= mul(lambda[j], syn[i - j]);
            }
            omega[i] = v;
        }
        // Chien search over the positions that exist: byte k is the
        // coefficient of x^(n − 1 − k), so its locator is X = α^(n − 1 − k).
        let mut found = 0;
        for (k, byte) in cw.iter_mut().enumerate() {
            let p = n - 1 - k;
            let xinv = pow(255 - p % 255);
            // Λ(X⁻¹)
            let mut v = 0u8;
            let mut xp = 1u8;
            for &c in lambda.iter().take(l + 1) {
                v ^= mul(c, xp);
                xp = mul(xp, xinv);
            }
            if v != 0 {
                continue;
            }
            // Forney: e = X^(1 − FCR) · Ω(X⁻¹) / Λ'(X⁻¹).
            let mut om = 0u8;
            let mut xp = 1u8;
            for &c in omega.iter().take(r) {
                om ^= mul(c, xp);
                xp = mul(xp, xinv);
            }
            let mut dl = 0u8;
            let mut xp = 1u8; // X⁻¹^(i−1) for odd i
            let x2 = mul(xinv, xinv);
            let mut i = 1;
            while i <= l {
                dl ^= mul(lambda[i], xp);
                xp = mul(xp, x2);
                i += 2;
            }
            if dl == 0 {
                return Err(Uncorrectable);
            }
            let xpow = pow((p * (256 - FCR)) % 255); // X^(1 − FCR)
            *byte ^= mul(xpow, div(om, dl));
            found += 1;
        }
        if found != l {
            return Err(Uncorrectable);
        }
        Ok(found)
    }
}

const fn mul_const(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        GF.exp[GF.log[a as usize] as usize + GF.log[b as usize] as usize]
    }
}

/// ADS-B Basic (short) frames.
pub static SHORT: Code = Code::new(12);
/// ADS-B Long frames.
pub static LONG: Code = Code::new(14);
/// Each of the six uplink blocks.
pub static UPLINK: Code = Code::new(20);

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u32) -> u8 {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (*seed >> 16) as u8
    }

    #[test]
    fn corrects_up_to_t_errors_and_detects_more() {
        let mut seed = 7;
        for (code, n) in [(&SHORT, 30), (&LONG, 48), (&UPLINK, 92)] {
            let t = code.roots() / 2;
            for trial in 0..200 {
                let mut cw = vec![0u8; n];
                for b in cw.iter_mut().take(n - code.roots()) {
                    *b = lcg(&mut seed);
                }
                code.encode(&mut cw);
                let clean = cw.clone();
                assert_eq!(code.decode(&mut cw.clone()), Ok(0));
                let errors = trial % (t + 1);
                let mut bad = cw.clone();
                let mut at = Vec::new();
                while at.len() < errors {
                    let p = usize::from(lcg(&mut seed)) % n;
                    if !at.contains(&p) {
                        at.push(p);
                        bad[p] ^= lcg(&mut seed) | 1;
                    }
                }
                assert_eq!(code.decode(&mut bad), Ok(errors), "n={n} errors={errors}");
                assert_eq!(bad, clean);
            }
            // t + 1 errors: never "corrected" back to the codeword.
            let mut cw = vec![0u8; n];
            code.encode(&mut cw);
            let mut bad = cw.clone();
            for p in 0..=t {
                bad[p * 2] ^= 0x5A;
            }
            if code.decode(&mut bad).is_ok() {
                assert_ne!(bad, cw);
            }
        }
    }

    #[test]
    fn generator_has_the_right_roots() {
        for code in [&SHORT, &LONG, &UPLINK] {
            for i in 0..code.roots() {
                let x = pow(FCR + i);
                let v = code.generator[..=code.roots()]
                    .iter()
                    .rev()
                    .fold(0u8, |acc, &c| mul(acc, x) ^ c);
                assert_eq!(v, 0);
            }
        }
    }
}
