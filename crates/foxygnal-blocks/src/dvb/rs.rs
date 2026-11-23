//! Reed-Solomon RS(204, 188, t = 8) (EN 300 421 §4.4.2; also DVB-T and
//! DVB-C), shortened from RS(255, 239): 51 zero bytes are put in front of the
//! 188 data bytes, encoded, and left out again.
//!
//! - Field GF(2⁸) with p(x) = x⁸ + x⁴ + x³ + x² + 1 (`0x11D`), α = `0x02`.
//! - Code generator g(x) = (x + α⁰)(x + α¹)…(x + α¹⁵).
//! - Systematic: the 188 data bytes, then the 16 parity bytes. The first byte
//!   sent is the coefficient of the highest power.
//!
//! The decoder computes the 16 syndromes (one table lookup per byte and
//! syndrome), and only when one is not zero runs Berlekamp-Massey, a Chien
//! search over the 204 positions that exist and Forney's formula. Up to 8 wrong
//! bytes anywhere in the packet are corrected. Nothing allocates.

/// Bytes per codeword.
pub const N: usize = 204;
/// Data bytes per codeword.
pub const K: usize = 188;
/// Parity bytes.
pub const PARITY: usize = N - K;
/// Correctable byte errors.
pub const T: usize = PARITY / 2;

/// Field polynomial without the x⁸ term.
const POLY: u16 = 0x11D;

/// Lookup tables, all built at compile time.
pub struct Gf {
    /// α^i for i in 0..512 (doubled so a sum of two logs needs no modulo).
    pub exp: [u8; 512],
    /// log_α(x); `log[0]` is unused.
    pub log: [u8; 256],
    /// Generator coefficients: `generator[d]` is the coefficient of x^d.
    pub generator: [u8; PARITY + 1],
    /// log of `generator[d]` for d < 16 (all are non-zero).
    gen_log: [u8; PARITY],
    /// `mul_alpha[j][x] = x·α^j`, for the syndromes.
    mul_alpha: [[u8; 256]; PARITY],
}

pub static GF: Gf = Gf::build();

impl Gf {
    const fn build() -> Self {
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

        // g(x) = Π (x + α^i), built up one factor at a time.
        let mut g = [0u8; PARITY + 1];
        g[0] = 1;
        let mut i = 0;
        while i < PARITY {
            let r = exp[i];
            // Multiply by (x + r): new[d] = g[d − 1] + r·g[d].
            let mut d = i + 1;
            while d > 0 {
                g[d] = g[d - 1] ^ mul_const(&exp, &log, r, g[d]);
                d -= 1;
            }
            g[0] = mul_const(&exp, &log, r, g[0]);
            i += 1;
        }
        let mut gen_log = [0u8; PARITY];
        let mut d = 0;
        while d < PARITY {
            gen_log[d] = log[g[d] as usize];
            d += 1;
        }

        let mut mul_alpha = [[0u8; 256]; PARITY];
        let mut j = 0;
        while j < PARITY {
            let mut v = 1;
            while v < 256 {
                mul_alpha[j][v] = exp[log[v] as usize + j];
                v += 1;
            }
            j += 1;
        }
        Gf {
            exp,
            log,
            generator: g,
            gen_log,
            mul_alpha,
        }
    }

    #[inline]
    pub fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    /// `a / b`; `b` must not be 0.
    #[inline]
    pub fn div(&self, a: u8, b: u8) -> u8 {
        if a == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + 255 - self.log[b as usize] as usize]
        }
    }

    /// α^e for any e ≥ 0.
    #[inline]
    pub fn pow(&self, e: usize) -> u8 {
        self.exp[e % 255]
    }
}

const fn mul_const(exp: &[u8; 512], log: &[u8; 256], a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        exp[log[a as usize] as usize + log[b as usize] as usize]
    }
}

/// Compute the 16 parity bytes of `cw[..188]` into `cw[188..]`.
pub fn encode(cw: &mut [u8; N]) {
    let gf = &GF;
    let mut par = [0u8; PARITY];
    for &b in &cw[..K] {
        let fb = b ^ par[0];
        if fb == 0 {
            par.copy_within(1.., 0);
            par[PARITY - 1] = 0;
        } else {
            let lf = gf.log[fb as usize] as usize;
            for i in 0..PARITY - 1 {
                par[i] = par[i + 1] ^ gf.exp[lf + gf.gen_log[PARITY - 1 - i] as usize];
            }
            par[PARITY - 1] = gf.exp[lf + gf.gen_log[0] as usize];
        }
    }
    cw[K..].copy_from_slice(&par);
}

/// Why a codeword could not be corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncorrectable;

/// Correct `cw` in place. Returns the number of bytes corrected (0 if it was
/// a codeword), or [`Uncorrectable`] (more than 8 errors, as far as can be
/// told), in which case `cw` is left as it was.
pub fn decode(cw: &mut [u8; N]) -> Result<usize, Uncorrectable> {
    let gf = &GF;
    // Syndromes S_j = c(α^j), by Horner's rule.
    let mut s = [0u8; PARITY];
    for &b in cw.iter() {
        for (j, sj) in s.iter_mut().enumerate() {
            *sj = gf.mul_alpha[j][*sj as usize] ^ b;
        }
    }
    if s.iter().all(|&v| v == 0) {
        return Ok(0);
    }

    // Berlekamp-Massey: the error locator Λ(x) = Π (1 − X_l·x).
    let mut lambda = [0u8; PARITY + 1];
    let mut prev = [0u8; PARITY + 1];
    lambda[0] = 1;
    prev[0] = 1;
    let (mut l, mut m, mut b) = (0usize, 1usize, 1u8);
    for n in 0..PARITY {
        let mut d = s[n];
        for i in 1..=l {
            d ^= gf.mul(lambda[i], s[n - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let coef = gf.div(d, b);
        let old = lambda;
        for i in m..=PARITY {
            lambda[i] ^= gf.mul(coef, prev[i - m]);
        }
        if 2 * l <= n {
            l = n + 1 - l;
            prev = old;
            b = d;
            m = 1;
        } else {
            m += 1;
        }
    }
    if l > T {
        return Err(Uncorrectable);
    }

    // Ω(x) = S(x)·Λ(x) mod x¹⁶.
    let mut omega = [0u8; PARITY];
    for (i, o) in omega.iter_mut().enumerate() {
        let mut v = 0;
        for j in 0..=i.min(l) {
            v ^= gf.mul(lambda[j], s[i - j]);
        }
        *o = v;
    }

    // Chien search over the positions that exist (the shortened code's first
    // 51 positions are always zero), then Forney.
    let mut pos = [0usize; T];
    let mut val = [0u8; T];
    let mut found = 0;
    for (k, _) in cw.iter().enumerate() {
        // Byte k is the coefficient of x^p.
        let p = N - 1 - k;
        let xinv = (255 - p) % 255; // log of X⁻¹
        let mut sum = 0u8;
        for (i, &c) in lambda.iter().enumerate().take(l + 1) {
            if c != 0 {
                sum ^= gf.exp[gf.log[c as usize] as usize + (xinv * i) % 255];
            }
        }
        if sum != 0 {
            continue;
        }
        if found == T {
            return Err(Uncorrectable);
        }
        // e = X·Ω(X⁻¹) / Λ'(X⁻¹), first root α⁰.
        let mut om = 0u8;
        for (i, &c) in omega.iter().enumerate() {
            if c != 0 {
                om ^= gf.exp[gf.log[c as usize] as usize + (xinv * i) % 255];
            }
        }
        let mut der = 0u8;
        for i in (1..=l).step_by(2) {
            let c = lambda[i];
            if c != 0 {
                der ^= gf.exp[gf.log[c as usize] as usize + (xinv * (i - 1)) % 255];
            }
        }
        if der == 0 {
            return Err(Uncorrectable);
        }
        pos[found] = k;
        val[found] = gf.mul(gf.pow(p), gf.div(om, der));
        found += 1;
    }
    if found != l {
        return Err(Uncorrectable);
    }
    for i in 0..found {
        cw[pos[i]] ^= val[i];
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn codeword(seed: &mut u64) -> [u8; N] {
        let mut cw = [0u8; N];
        for b in &mut cw[..K] {
            *b = rng(seed) as u8;
        }
        encode(&mut cw);
        cw
    }

    #[test]
    fn field_and_generator_match_the_published_values() {
        // α⁸ = x⁴ + x³ + x² + 1.
        assert_eq!(GF.exp[8], 0x1D);
        assert_eq!(GF.exp[255], 1);
        // The DVB RS(255,239) generator, highest power first (as tabulated in
        // e.g. the DVB-C/DVB-T literature): x¹⁶ + 59x¹⁵ + 13x¹⁴ + 104x¹³ + …
        let want = [
            1u8, 59, 13, 104, 189, 68, 209, 30, 8, 163, 65, 41, 229, 98, 50, 36, 59,
        ];
        let mut got = GF.generator;
        got.reverse();
        assert_eq!(got, want);
    }

    #[test]
    fn codewords_have_zero_syndromes() {
        let mut seed = 0x1234_5678_9abc_def1;
        for _ in 0..20 {
            let mut cw = codeword(&mut seed);
            let orig = cw;
            assert_eq!(decode(&mut cw), Ok(0));
            assert_eq!(cw, orig);
            // Every root of g(x) is a root of the codeword.
            for j in 0..PARITY {
                let mut v = 0u8;
                for &b in &cw {
                    v = GF.mul(v, GF.pow(j)) ^ b;
                }
                assert_eq!(v, 0, "root α^{j}");
            }
        }
    }

    #[test]
    fn corrects_up_to_8_byte_errors() {
        let mut seed = 0xfeed_beef_0bad_cafe;
        for errors in 1..=T {
            for _ in 0..30 {
                let orig = codeword(&mut seed);
                let mut cw = orig;
                let mut at = Vec::new();
                while at.len() < errors {
                    let p = (rng(&mut seed) % N as u64) as usize;
                    if !at.contains(&p) {
                        at.push(p);
                    }
                }
                for &p in &at {
                    cw[p] ^= (rng(&mut seed) % 255 + 1) as u8;
                }
                assert_eq!(decode(&mut cw), Ok(errors), "errors at {at:?}");
                assert_eq!(cw, orig);
            }
        }
        // Errors in the first and last byte.
        let orig = codeword(&mut seed);
        let mut cw = orig;
        cw[0] ^= 0xFF;
        cw[N - 1] ^= 0x01;
        assert_eq!(decode(&mut cw), Ok(2));
        assert_eq!(cw, orig);
    }

    #[test]
    fn too_many_errors_are_detected_and_left_alone() {
        let mut seed = 42;
        let mut failed = 0;
        for _ in 0..200 {
            let orig = codeword(&mut seed);
            let mut cw = orig;
            for p in 0..12 {
                cw[p * 17] ^= (rng(&mut seed) % 255 + 1) as u8;
            }
            let bad = cw;
            match decode(&mut cw) {
                Err(Uncorrectable) => {
                    failed += 1;
                    assert_eq!(cw, bad);
                }
                // A miscorrection to another codeword is possible but rare.
                Ok(_) => assert_ne!(cw, orig),
            }
        }
        assert!(failed > 190, "{failed}");
    }
}
