//! Reed-Solomon codes over GF(2⁸), shortened from RS(255, 255 − 2t):
//!
//! - [`DVB`]: RS(204, 188, t = 8) (EN 300 421 §4.4.2; also DVB-T and DVB-C):
//!   51 zero bytes are put in front of the 188 data bytes, encoded, and left
//!   out again.
//! - [`ATSC`]: RS(207, 187, t = 10) (A/53 Part 2 §6.3.2), 48 zero bytes.
//! - [`CCSDS`]: RS(255, 223, t = 16) (CCSDS 131.0-B §4), in the
//!   conventional basis; `crate::ccsds::rs` adds the dual basis, interleaving
//!   and shortening (virtual fill).
//!
//! A code is defined by its field polynomial p(x), a primitive element β
//! (given as a power of α = `0x02`), and its first consecutive root: the
//! generator is g(x) = (x + β^f)(x + β^(f+1))…(x + β^(f+2t−1)).
//!
//! | code | p(x) | β | f |
//! |---|---|---|---|
//! | DVB, ATSC | x⁸ + x⁴ + x³ + x² + 1 (`0x11D`) | α | 0 |
//! | CCSDS | x⁸ + x⁷ + x² + x + 1 (`0x187`) | α¹¹ | 112 |
//!
//! Systematic: the data bytes, then the parity bytes. The first byte sent is
//! the coefficient of the highest power.
//!
//! Each [`Code`] keeps its field's tables with logarithms to the base β, so
//! the decoder is the textbook one whatever β is: it computes the 2t
//! syndromes (one table lookup per byte and syndrome), and only when one is
//! not zero runs Berlekamp-Massey, a Chien search over the positions that
//! exist and Forney's formula. Up to t wrong bytes anywhere in the codeword
//! are corrected. Nothing allocates.

/// DVB bytes per codeword.
pub const N: usize = 204;
/// DVB data bytes per codeword.
pub const K: usize = 188;
/// DVB parity bytes.
pub const PARITY: usize = N - K;
/// DVB correctable byte errors.
pub const T: usize = PARITY / 2;

/// The most parity bytes a [`Code`] may have.
pub const MAX_PARITY: usize = 32;
const MAX_T: usize = MAX_PARITY / 2;

/// The DVB/ATSC field polynomial.
pub const POLY_DVB: u16 = 0x11D;
/// The CCSDS field polynomial.
pub const POLY_CCSDS: u16 = 0x187;

/// Field lookup tables, built at compile time, with logarithms to a chosen
/// primitive element β.
pub struct Gf {
    /// β^i for i in 0..512 (doubled so a sum of two logs needs no modulo).
    pub exp: [u8; 512],
    /// log_β(x); `log[0]` is unused.
    pub log: [u8; 256],
}

/// GF(2⁸) with `0x11D` and β = α: DVB and ATSC.
pub static GF: Gf = Gf::new(POLY_DVB, 1);

impl Gf {
    /// The field with polynomial `poly` (the x⁸ term included) and
    /// β = α^`prim`, which must be primitive (`prim` coprime with 255).
    pub const fn new(poly: u16, prim: usize) -> Self {
        assert!(
            poly & 0x100 != 0
                && !prim.is_multiple_of(3)
                && !prim.is_multiple_of(5)
                && !prim.is_multiple_of(17)
        );
        // Powers of α first.
        let mut alpha = [0u8; 255];
        let mut x: u16 = 1;
        let mut i = 0;
        while i < 255 {
            alpha[i] = x as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= poly;
            }
            i += 1;
        }
        assert!(x == 1, "the field polynomial is not primitive");
        // Then β^i = α^(prim·i).
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut i = 0;
        while i < 255 {
            let v = alpha[(i * prim) % 255];
            exp[i] = v;
            exp[i + 255] = v;
            log[v as usize] = i as u8;
            i += 1;
        }
        exp[510] = exp[0];
        exp[511] = exp[1];
        Gf { exp, log }
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

    /// β^e for any e ≥ 0.
    #[inline]
    pub fn pow(&self, e: usize) -> u8 {
        self.exp[e % 255]
    }

    const fn mul_const(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }
}

/// A shortened RS code with `parity` parity bytes and `n` bytes per codeword.
pub struct Code {
    n: usize,
    parity: usize,
    /// First consecutive root, as a power of β.
    fcr: usize,
    gf: Gf,
    /// Generator coefficients: `generator[d]` is the coefficient of x^d.
    generator: [u8; MAX_PARITY + 1],
    /// log of `generator[d]` for d < parity (all are non-zero).
    gen_log: [u8; MAX_PARITY],
    /// `root_mul[j][x] = x·β^(fcr + j)`, for the syndromes.
    root_mul: [[u8; 256]; MAX_PARITY],
}

/// RS(204, 188): DVB.
pub static DVB: Code = Code::new(N, PARITY);
/// RS(207, 187): ATSC.
pub static ATSC: Code = Code::new(207, 20);
/// RS(255, 223) of CCSDS, conventional basis, not shortened.
pub static CCSDS: Code = Code::with_field(255, 32, POLY_CCSDS, 11, 112);

impl Code {
    /// A code over DVB's field (`0x11D`, β = α, first root α⁰).
    pub const fn new(n: usize, parity: usize) -> Self {
        Self::with_field(n, parity, POLY_DVB, 1, 0)
    }

    /// A code over the field `poly` with β = α^`prim` and the generator's
    /// first root β^`fcr`.
    pub const fn with_field(n: usize, parity: usize, poly: u16, prim: usize, fcr: usize) -> Self {
        assert!(parity.is_multiple_of(2) && parity <= MAX_PARITY && n <= 255 && n > parity);
        let gf = Gf::new(poly, prim);
        // g(x) = Π (x + β^(fcr + i)), built up one factor at a time.
        let mut g = [0u8; MAX_PARITY + 1];
        g[0] = 1;
        let mut i = 0;
        while i < parity {
            let r = gf.exp[(fcr + i) % 255];
            // Multiply by (x + r): new[d] = g[d − 1] + r·g[d].
            let mut d = i + 1;
            while d > 0 {
                g[d] = g[d - 1] ^ gf.mul_const(r, g[d]);
                d -= 1;
            }
            g[0] = gf.mul_const(r, g[0]);
            i += 1;
        }
        let mut gen_log = [0u8; MAX_PARITY];
        let mut d = 0;
        while d < parity {
            gen_log[d] = gf.log[g[d] as usize];
            d += 1;
        }
        let mut root_mul = [[0u8; 256]; MAX_PARITY];
        let mut j = 0;
        while j < parity {
            let mut v = 1;
            while v < 256 {
                root_mul[j][v] = gf.exp[gf.log[v] as usize + (fcr + j) % 255];
                v += 1;
            }
            j += 1;
        }
        Code {
            n,
            parity,
            fcr,
            gf,
            generator: g,
            gen_log,
            root_mul,
        }
    }

    /// Bytes per codeword.
    pub fn n(&self) -> usize {
        self.n
    }

    /// Data bytes per codeword.
    pub fn k(&self) -> usize {
        self.n - self.parity
    }

    /// Parity bytes per codeword.
    pub fn parity(&self) -> usize {
        self.parity
    }

    /// Correctable byte errors.
    pub fn t(&self) -> usize {
        self.parity / 2
    }

    /// The code's field (logarithms to the base β).
    pub fn field(&self) -> &Gf {
        &self.gf
    }

    /// The generator's coefficients, constant term first.
    pub fn generator(&self) -> &[u8] {
        &self.generator[..=self.parity]
    }

    /// Compute the parity bytes of `cw[..k]` into `cw[k..]`. `cw` must be
    /// `n` bytes long.
    pub fn encode(&self, cw: &mut [u8]) {
        assert_eq!(cw.len(), self.n);
        let k = self.k();
        let (data, parity) = cw.split_at_mut(k);
        self.encode_parts(data.iter().copied(), parity);
    }

    /// Compute the parity of the data bytes `data` (any count up to k: a
    /// shorter code, the missing leading bytes being zero) into `parity`
    /// (`2t` bytes).
    pub fn encode_parts(&self, data: impl IntoIterator<Item = u8>, parity: &mut [u8]) {
        let gf = &self.gf;
        let p = self.parity;
        assert_eq!(parity.len(), p);
        let mut par = [0u8; MAX_PARITY];
        for b in data {
            let fb = b ^ par[0];
            if fb == 0 {
                par.copy_within(1..p, 0);
                par[p - 1] = 0;
            } else {
                let lf = gf.log[fb as usize] as usize;
                for i in 0..p - 1 {
                    par[i] = par[i + 1] ^ gf.exp[lf + self.gen_log[p - 1 - i] as usize];
                }
                par[p - 1] = gf.exp[lf + self.gen_log[0] as usize];
            }
        }
        parity.copy_from_slice(&par[..p]);
    }

    /// Correct `cw` (`n` bytes) in place. Returns the number of bytes
    /// corrected (0 if it was a codeword), or [`Uncorrectable`] (more than t
    /// errors, as far as can be told), in which case `cw` is left as it was.
    pub fn decode(&self, cw: &mut [u8]) -> Result<usize, Uncorrectable> {
        assert_eq!(cw.len(), self.n);
        self.decode_shortened(cw)
    }

    /// [`Self::decode`] for a codeword shortened further than `n`: `cw` holds
    /// the last `cw.len()` (> 2t) bytes of an `n`-byte codeword whose leading
    /// bytes are zero.
    pub fn decode_shortened(&self, cw: &mut [u8]) -> Result<usize, Uncorrectable> {
        let gf = &self.gf;
        let p = self.parity;
        let t = self.t();
        let n = cw.len();
        assert!(n > p && n <= self.n);
        // Syndromes S_j = c(β^(fcr + j)), by Horner's rule.
        let mut s = [0u8; MAX_PARITY];
        match p {
            16 => self.syndromes::<16>(cw, &mut s),
            20 => self.syndromes::<20>(cw, &mut s),
            32 => self.syndromes::<32>(cw, &mut s),
            _ => {
                for &b in cw.iter() {
                    for (j, sj) in s[..p].iter_mut().enumerate() {
                        *sj = self.root_mul[j][*sj as usize] ^ b;
                    }
                }
            }
        }
        if s[..p].iter().all(|&v| v == 0) {
            return Ok(0);
        }

        // Berlekamp-Massey: the error locator Λ(x) = Π (1 − X_l·x).
        let mut lambda = [0u8; MAX_PARITY + 1];
        let mut prev = [0u8; MAX_PARITY + 1];
        lambda[0] = 1;
        prev[0] = 1;
        let (mut l, mut m, mut b) = (0usize, 1usize, 1u8);
        for r in 0..p {
            let mut d = s[r];
            for i in 1..=l {
                d ^= gf.mul(lambda[i], s[r - i]);
            }
            if d == 0 {
                m += 1;
                continue;
            }
            let coef = gf.div(d, b);
            let old = lambda;
            for i in m..=p {
                lambda[i] ^= gf.mul(coef, prev[i - m]);
            }
            if 2 * l <= r {
                l = r + 1 - l;
                prev = old;
                b = d;
                m = 1;
            } else {
                m += 1;
            }
        }
        if l > t {
            return Err(Uncorrectable);
        }

        // Ω(x) = S(x)·Λ(x) mod x^2t.
        let mut omega = [0u8; MAX_PARITY];
        for (i, o) in omega[..p].iter_mut().enumerate() {
            let mut v = 0;
            for j in 0..=i.min(l) {
                v ^= gf.mul(lambda[j], s[i - j]);
            }
            *o = v;
        }

        // X^(1 − fcr) in Forney's formula, as a power of X.
        let xpow = (256 - self.fcr % 255) % 255;
        // Chien search over the positions that exist (the shortened code's
        // leading positions are always zero), then Forney.
        let mut pos = [0usize; MAX_T];
        let mut val = [0u8; MAX_T];
        let mut found = 0;
        for k in 0..n {
            // Byte k is the coefficient of x^e, X = β^e.
            let e = n - 1 - k;
            let xinv = (255 - e) % 255; // log of X⁻¹
            let mut sum = 0u8;
            for (i, &c) in lambda.iter().enumerate().take(l + 1) {
                if c != 0 {
                    sum ^= gf.exp[gf.log[c as usize] as usize + (xinv * i) % 255];
                }
            }
            if sum != 0 {
                continue;
            }
            if found == t {
                return Err(Uncorrectable);
            }
            // e = X^(1 − fcr)·Ω(X⁻¹) / Λ'(X⁻¹).
            let mut om = 0u8;
            for (i, &c) in omega[..p].iter().enumerate() {
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
            val[found] = gf.mul(gf.pow(e * xpow), gf.div(om, der));
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

    /// The first `P` syndromes (a fixed count, so the inner loop unrolls).
    #[inline]
    fn syndromes<const P: usize>(&self, cw: &[u8], s: &mut [u8; MAX_PARITY]) {
        let mut acc = [0u8; P];
        let tab: &[[u8; 256]; P] = self.root_mul[..P].try_into().unwrap();
        for &b in cw {
            for (sj, t) in acc.iter_mut().zip(tab) {
                *sj = t[*sj as usize] ^ b;
            }
        }
        s[..P].copy_from_slice(&acc);
    }
}

/// Compute the 16 parity bytes of `cw[..188]` into `cw[188..]` (DVB).
pub fn encode(cw: &mut [u8; N]) {
    DVB.encode(cw);
}

/// Why a codeword could not be corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uncorrectable;

/// Correct a DVB codeword in place: [`Code::decode`] of [`DVB`].
pub fn decode(cw: &mut [u8; N]) -> Result<usize, Uncorrectable> {
    DVB.decode(cw)
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

    fn codeword(code: &Code, seed: &mut u64) -> Vec<u8> {
        let mut cw = vec![0u8; code.n()];
        for b in &mut cw[..code.k()] {
            *b = rng(seed) as u8;
        }
        code.encode(&mut cw);
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
        let mut got = DVB.generator().to_vec();
        got.reverse();
        assert_eq!(got, want);
        // ATSC's 20 roots: monic, and every root is one.
        let g = ATSC.generator();
        assert_eq!(g.len(), 21);
        assert_eq!(g[20], 1);
        for i in 0..20 {
            let mut v = 0u8;
            for &c in g.iter().rev() {
                v = GF.mul(v, GF.pow(i)) ^ c;
            }
            assert_eq!(v, 0, "α^{i}");
        }
    }

    #[test]
    fn codewords_have_zero_syndromes() {
        let mut seed = 0x1234_5678_9abc_def1;
        for code in [&DVB, &ATSC, &CCSDS] {
            for _ in 0..20 {
                let mut cw = codeword(code, &mut seed);
                let orig = cw.clone();
                assert_eq!(code.decode(&mut cw), Ok(0));
                assert_eq!(cw, orig);
                // Every root of g(x) is a root of the codeword.
                let gf = code.field();
                for j in 0..code.n() - code.k() {
                    let mut v = 0u8;
                    for &b in &cw {
                        v = gf.mul(v, gf.pow(code.fcr + j)) ^ b;
                    }
                    assert_eq!(v, 0, "root α^{j}");
                }
            }
        }
    }

    #[test]
    fn corrects_up_to_t_byte_errors() {
        let mut seed = 0xfeed_beef_0bad_cafe;
        for code in [&DVB, &ATSC, &CCSDS] {
            let n = code.n();
            for errors in 1..=code.t() {
                for _ in 0..30 {
                    let orig = codeword(code, &mut seed);
                    let mut cw = orig.clone();
                    let mut at = Vec::new();
                    while at.len() < errors {
                        let p = (rng(&mut seed) % n as u64) as usize;
                        if !at.contains(&p) {
                            at.push(p);
                        }
                    }
                    for &p in &at {
                        cw[p] ^= (rng(&mut seed) % 255 + 1) as u8;
                    }
                    assert_eq!(code.decode(&mut cw), Ok(errors), "errors at {at:?}");
                    assert_eq!(cw, orig);
                }
            }
            // Errors in the first and last byte.
            let orig = codeword(code, &mut seed);
            let mut cw = orig.clone();
            cw[0] ^= 0xFF;
            cw[n - 1] ^= 0x01;
            assert_eq!(code.decode(&mut cw), Ok(2));
            assert_eq!(cw, orig);
        }
    }

    #[test]
    fn too_many_errors_are_detected_and_left_alone() {
        let mut seed = 42;
        for code in [&DVB, &ATSC, &CCSDS] {
            let mut failed = 0;
            for _ in 0..200 {
                let orig = codeword(code, &mut seed);
                let mut cw = orig.clone();
                let step = code.n() / (code.t() + 4);
                for p in 0..code.t() + 4 {
                    cw[p * step] ^= (rng(&mut seed) % 255 + 1) as u8;
                }
                let bad = cw.clone();
                match code.decode(&mut cw) {
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

    #[test]
    fn ccsds_field_and_generator() {
        // α = 0x02 in 0x187, β = α¹¹. CCSDS 131.0-B §4.2.2: g(x) is
        // symmetric (its roots come in reciprocal pairs, 112 + 143 = 255).
        let gf = CCSDS.field();
        let alpha11 = {
            let mut x: u16 = 1;
            for _ in 0..11 {
                x <<= 1;
                if x & 0x100 != 0 {
                    x ^= POLY_CCSDS;
                }
            }
            x as u8
        };
        assert_eq!(gf.exp[1], alpha11);
        let g = CCSDS.generator();
        assert_eq!(g.len(), 33);
        for d in 0..=32 {
            assert_eq!(g[d], g[32 - d], "x^{d}");
        }
        // The published coefficients (conventional basis), x³² first:
        // 1, 0x5B, 0x7F, 0x56, 0x10, 0x1E, … (Berlekamp's code).
        assert_eq!(g[27..], [0x1E, 0x10, 0x56, 0x7F, 0x5B, 1]);
    }

    #[test]
    fn shortened_decoding_matches_zero_padding() {
        let mut seed = 77;
        let code = &CCSDS;
        for short in [0usize, 1, 33, 100] {
            let mut full = vec![0u8; 255];
            for b in &mut full[short..223] {
                *b = rng(&mut seed) as u8;
            }
            code.encode(&mut full);
            assert!(full[..short].iter().all(|&b| b == 0));
            let mut parity = [0u8; 32];
            code.encode_parts(full[short..223].iter().copied(), &mut parity);
            assert_eq!(parity, full[223..]);
            let orig = full[short..].to_vec();
            let mut cw = orig.clone();
            for k in 0..16 {
                let at = (k * 13 + short) % cw.len();
                cw[at] ^= 0x5A;
            }
            assert_eq!(
                code.decode_shortened(&mut cw),
                Ok(16),
                "shortened by {short}"
            );
            assert_eq!(cw, orig);
        }
    }
}
