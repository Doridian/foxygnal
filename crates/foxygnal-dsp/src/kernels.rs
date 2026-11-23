//! Vectorized inner loops, with the instruction set chosen once at run time.
//!
//! Every kernel is written once, in plain Rust shaped so LLVM vectorizes it
//! (fixed-width accumulator arrays, `chunks_exact`, no branches in the loop
//! body), in the private `portable` module. That code is then compiled
//! several times:
//!
//! - **baseline**: whatever the target guarantees. SSE2 on x86_64, NEON (with
//!   fused multiply-add) on aarch64.
//! - **avx2** (x86_64 only): the same source compiled with AVX2 and FMA
//!   enabled through `#[target_feature]`, used only after
//!   `is_x86_feature_detected!` says the CPU has both.
//!
//! [`Kernels`] is a table of function pointers to one of these sets. Pick it
//! once ([`kernels()`] detects and caches the best set for the process) and
//! store the pointers you need; calling through them costs one indirect call
//! and no feature checks. Setting `FOXYGNAL_SIMD=baseline` in the environment
//! makes [`kernels()`] return the baseline set, for comparisons.
//!
//! Results differ from a naive sequential loop by rounding only: sums are
//! reassociated across lanes, and FMA rounds once where `a * b + c` rounds
//! twice.

// The kernels are shaped around `chunks_exact` (with its remainder) so LLVM
// vectorizes them; they are benchmarked that way, so the newer `as_chunks`
// suggestion is not applied here.
#![allow(clippy::chunks_exact_to_as_chunks)]

use std::sync::OnceLock;

use num_complex::Complex32;

/// Which compiled variant a [`Kernels`] table points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The target's baseline: SSE2 on x86_64, NEON on aarch64.
    Baseline,
    /// x86_64 with AVX2 and FMA.
    Avx2,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Self::Baseline if cfg!(target_arch = "x86_64") => "sse2",
            Self::Baseline if cfg!(target_arch = "aarch64") => "neon",
            Self::Baseline => "baseline",
            Self::Avx2 => "avx2+fma",
        }
    }
}

/// Function pointers to one compiled set of kernels. See the module docs.
///
/// Slices passed together are processed up to the shorter length.
#[derive(Clone, Copy)]
pub struct Kernels {
    pub level: Level,
    /// `Σ h[i]·x[i]`.
    pub dot_f32: fn(h: &[f32], x: &[f32]) -> f32,
    /// Real taps times complex samples, both as interleaved floats: `h` holds
    /// every tap twice (`h0 h0 h1 h1 …`), `x` is `re0 im0 re1 im1 …`.
    /// Returns `Σ h[k]·x[k]` as a complex number.
    pub dot_c32: fn(h2: &[f32], x: &[f32]) -> Complex32,
    /// Multiplies `x` by a rotating phasor: `y[n] = x[n]·p·sᴺ`. `steps` holds
    /// `s⁰ … s⁷` and `step8` is `s⁸`. Processes whole groups of 8 samples
    /// only and returns the phasor for the first sample it did not process
    /// (renormalized to magnitude 1).
    pub rotate: RotateFn,
    /// `y = a·b` (complex multiply).
    pub mul_c32: fn(a: &[Complex32], b: &[Complex32], y: &mut [Complex32]),
    /// `y = x·w` with real `w` (window application).
    pub mul_c32_f32: fn(x: &[Complex32], w: &[f32], y: &mut [Complex32]),
    /// `y = |x|²·k`.
    pub power: fn(x: &[Complex32], k: f32, y: &mut [f32]),
    /// `y = 10·log10(max(x, 1e-20))`, accurate to about 1e-4 dB.
    pub power_to_db: fn(x: &[f32], y: &mut [f32]),
    /// `y = |x|`.
    pub magnitude: fn(x: &[Complex32], y: &mut [f32]),
    /// `y = arg(x)` in −π..π, by [`atan2`] (about 1e-7 rad from exact).
    pub phase: fn(x: &[Complex32], y: &mut [f32]),
    /// Quadrature FM discriminator: `y[n] = arg(x[n]·conj(x[n−1]))·gain`,
    /// where `x[−1]` is `prev`. Returns the new `prev` (the last sample).
    pub fm_discriminate:
        fn(prev: Complex32, x: &[Complex32], gain: f32, y: &mut [f32]) -> Complex32,
    /// `y = x·scale + offset` from unsigned bytes.
    pub u8_to_f32: fn(x: &[u8], scale: f32, offset: f32, y: &mut [f32]),
    /// `y = x·scale` from 16-bit integers.
    pub i16_to_f32: fn(x: &[i16], scale: f32, y: &mut [f32]),
}

/// `rotate(phasor, steps, step8, x, y) -> next phasor`, see [`Kernels::rotate`].
pub type RotateFn = fn(
    phasor: Complex32,
    steps: &[Complex32; 8],
    step8: Complex32,
    x: &[Complex32],
    y: &mut [Complex32],
) -> Complex32;

impl std::fmt::Debug for Kernels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernels")
            .field("level", &self.level)
            .finish_non_exhaustive()
    }
}

/// The kernels for this process: the best set the CPU supports, chosen on the
/// first call (or the baseline set if `FOXYGNAL_SIMD=baseline`).
pub fn kernels() -> &'static Kernels {
    static K: OnceLock<Kernels> = OnceLock::new();
    K.get_or_init(|| match std::env::var("FOXYGNAL_SIMD").as_deref() {
        Ok("baseline" | "off" | "0") => Kernels::baseline(),
        _ => Kernels::best(),
    })
}

impl Kernels {
    /// The best set this CPU supports (detected on every call; prefer
    /// [`kernels()`]).
    pub fn best() -> Self {
        #[cfg(target_arch = "x86_64")]
        if let Some(k) = Self::avx2() {
            return k;
        }
        Self::baseline()
    }

    /// The set every CPU of this target architecture can run.
    pub fn baseline() -> Self {
        use baseline as b;
        Self {
            level: Level::Baseline,
            dot_f32: b::dot_f32,
            dot_c32: b::dot_c32,
            rotate: b::rotate,
            mul_c32: b::mul_c32,
            mul_c32_f32: b::mul_c32_f32,
            power: b::power,
            power_to_db: b::power_to_db,
            magnitude: b::magnitude,
            phase: b::phase,
            fm_discriminate: b::fm_discriminate,
            u8_to_f32: b::u8_to_f32,
            i16_to_f32: b::i16_to_f32,
        }
    }

    /// The AVX2 + FMA set, if this CPU has both.
    #[cfg(target_arch = "x86_64")]
    pub fn avx2() -> Option<Self> {
        if !(is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma")) {
            return None;
        }
        use avx2 as a;
        Some(Self {
            level: Level::Avx2,
            dot_f32: a::dot_f32,
            dot_c32: a::dot_c32,
            rotate: a::rotate,
            mul_c32: a::mul_c32,
            mul_c32_f32: a::mul_c32_f32,
            power: a::power,
            power_to_db: a::power_to_db,
            magnitude: a::magnitude,
            phase: a::phase,
            fm_discriminate: a::fm_discriminate,
            u8_to_f32: a::u8_to_f32,
            i16_to_f32: a::i16_to_f32,
        })
    }

    /// Every set this CPU can run (for tests and benchmarks).
    pub fn all() -> Vec<Self> {
        let v = vec![Self::baseline()];
        #[cfg(target_arch = "x86_64")]
        let v = {
            let mut v = v;
            v.extend(Self::avx2());
            v
        };
        v
    }
}

/// Fast `atan2(y, x)` in −π..π, branch-free so it vectorizes. Maximum error
/// about 1e-7 rad (the polynomial is good to 2e-8; the rest is f32
/// rounding). Differs from `f32::atan2` only in the signs of zero: `(±0, −0)`
/// gives ±0 instead of ±π.
#[inline]
pub fn atan2(y: f32, x: f32) -> f32 {
    portable::atan2::<false>(y, x)
}

/// Generates one compiled set of every kernel from the `portable` bodies.
/// With a feature string, each kernel is an `unsafe` `#[target_feature]`
/// function behind a safe wrapper; the wrappers are only handed out by a
/// constructor that has checked the CPU.
macro_rules! kernel_set {
    ($m:ident, fma = $fma:expr, dot_f32 = $df:expr, dot_c32 = $dc:expr
     $(, feature = $feat:literal)?) => {
        mod $m {
            use super::portable as p;
            use num_complex::Complex32;
            kernel_set!(@fn $fma $(, $feat)?;
                dot_f32(h: &[f32], x: &[f32]) -> f32 => ($df)(h, x));
            kernel_set!(@fn $fma $(, $feat)?;
                dot_c32(h: &[f32], x: &[f32]) -> Complex32 => ($dc)(h, x));
            kernel_set!(@fn $fma $(, $feat)?;
                rotate(phasor: Complex32, steps: &[Complex32; 8], step8: Complex32,
                       x: &[Complex32], y: &mut [Complex32]) -> Complex32
                => p::rotate::<FMA>(phasor, steps, step8, x, y));
            kernel_set!(@fn $fma $(, $feat)?;
                mul_c32(a: &[Complex32], b: &[Complex32], y: &mut [Complex32]) -> ()
                => p::mul_c32::<FMA>(a, b, y));
            kernel_set!(@fn $fma $(, $feat)?;
                mul_c32_f32(x: &[Complex32], w: &[f32], y: &mut [Complex32]) -> ()
                => p::mul_c32_f32(x, w, y));
            kernel_set!(@fn $fma $(, $feat)?;
                power(x: &[Complex32], k: f32, y: &mut [f32]) -> () => p::power::<FMA>(x, k, y));
            kernel_set!(@fn $fma $(, $feat)?;
                power_to_db(x: &[f32], y: &mut [f32]) -> () => p::power_to_db::<FMA>(x, y));
            kernel_set!(@fn $fma $(, $feat)?;
                magnitude(x: &[Complex32], y: &mut [f32]) -> () => p::magnitude::<FMA>(x, y));
            kernel_set!(@fn $fma $(, $feat)?;
                phase(x: &[Complex32], y: &mut [f32]) -> () => p::phase::<FMA>(x, y));
            kernel_set!(@fn $fma $(, $feat)?;
                fm_discriminate(prev: Complex32, x: &[Complex32], gain: f32, y: &mut [f32])
                    -> Complex32
                => p::fm_discriminate::<FMA>(prev, x, gain, y));
            kernel_set!(@fn $fma $(, $feat)?;
                u8_to_f32(x: &[u8], scale: f32, offset: f32, y: &mut [f32]) -> ()
                => p::u8_to_f32::<FMA>(x, scale, offset, y));
            kernel_set!(@fn $fma $(, $feat)?;
                i16_to_f32(x: &[i16], scale: f32, y: &mut [f32]) -> () => p::i16_to_f32(x, scale, y));
        }
    };
    (@fn $fma:expr; $name:ident($($a:ident: $t:ty),*) -> $r:ty => $body:expr) => {
        #[allow(clippy::unused_unit)]
        pub(super) fn $name($($a: $t),*) -> $r {
            #[allow(dead_code)]
            const FMA: bool = $fma;
            $body
        }
    };
    (@fn $fma:expr, $feat:literal; $name:ident($($a:ident: $t:ty),*) -> $r:ty => $body:expr) => {
        #[allow(clippy::unused_unit)]
        pub(super) fn $name($($a: $t),*) -> $r {
            #[target_feature(enable = $feat)]
            unsafe fn inner($($a: $t),*) -> $r {
                #[allow(dead_code)]
                const FMA: bool = $fma;
                $body
            }
            // SAFETY: this set is only constructed by `Kernels::avx2`, after
            // checking that the CPU supports the features.
            unsafe { inner($($a),*) }
        }
    };
}

/// Whether the baseline target always has fused multiply-add.
const BASE_FMA: bool = cfg!(any(target_arch = "aarch64", target_feature = "fma"));

kernel_set!(
    baseline,
    fma = super::BASE_FMA,
    dot_f32 = |h, x| p::sum8(p::dot_lanes::<FMA>(h, x)),
    dot_c32 = |h, x| p::sum8_c32(p::dot_lanes::<FMA>(h, x))
);
// LLVM's SLP vectorizer builds the portable dot product from 128-bit
// vectors even with AVX2 enabled, so this set uses hand-written intrinsics
// for it (about 2× faster at 512 taps).
#[cfg(target_arch = "x86_64")]
kernel_set!(
    avx2,
    fma = true,
    dot_f32 = super::x86::dot_f32,
    dot_c32 = super::x86::dot_c32,
    feature = "avx2,fma"
);

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use num_complex::Complex32;

    // The two safe wrappers are only for the `avx2` kernel set, whose
    // functions run only on CPUs with AVX2 and FMA (and inline these into
    // their `#[target_feature]` bodies).

    #[inline(always)]
    pub fn dot_f32(h: &[f32], x: &[f32]) -> f32 {
        // SAFETY: only reachable through `Kernels::avx2`, which checked the CPU.
        unsafe { dot_f32_avx2(h, x) }
    }

    #[inline(always)]
    pub fn dot_c32(h: &[f32], x: &[f32]) -> Complex32 {
        // SAFETY: as above.
        unsafe { dot_c32_avx2(h, x) }
    }

    /// Multiply-accumulate whole 8-float blocks; returns the 8 lane sums
    /// (lane j: indices ≡ j mod 8) and how many floats were done.
    #[target_feature(enable = "avx2,fma")]
    #[inline]
    unsafe fn lanes(h: &[f32], x: &[f32]) -> (__m256, usize) {
        let n = h.len().min(x.len());
        let (hp, xp) = (h.as_ptr(), x.as_ptr());
        let mut i = 0;
        // SAFETY: every load reads [at, at + 8) with at + 8 <= n, inside
        // both slices.
        unsafe {
            let mac = |acc, at: usize| {
                _mm256_fmadd_ps(
                    _mm256_loadu_ps(hp.add(at)),
                    _mm256_loadu_ps(xp.add(at)),
                    acc,
                )
            };
            // Four accumulators in flight hide the FMA latency.
            let (mut a0, mut a1, mut a2, mut a3) = (
                _mm256_setzero_ps(),
                _mm256_setzero_ps(),
                _mm256_setzero_ps(),
                _mm256_setzero_ps(),
            );
            while i + 32 <= n {
                a0 = mac(a0, i);
                a1 = mac(a1, i + 8);
                a2 = mac(a2, i + 16);
                a3 = mac(a3, i + 24);
                i += 32;
            }
            while i + 8 <= n {
                a0 = mac(a0, i);
                i += 8;
            }
            (
                _mm256_add_ps(_mm256_add_ps(a0, a1), _mm256_add_ps(a2, a3)),
                i,
            )
        }
    }

    #[target_feature(enable = "avx2,fma")]
    #[inline]
    unsafe fn dot_f32_avx2(h: &[f32], x: &[f32]) -> f32 {
        // SAFETY: the caller has the target features. (One block: older
        // compilers want `unsafe` around every intrinsic.)
        let (mut sum, i) = unsafe {
            let (v, i) = lanes(h, x);
            // Fold 8 lanes to 4: [l0+l4, l1+l5, l2+l6, l3+l7].
            let q = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
            let q = _mm_add_ps(q, _mm_movehl_ps(q, q));
            (_mm_cvtss_f32(_mm_add_ss(q, _mm_movehdup_ps(q))), i)
        };
        let n = h.len().min(x.len());
        for (a, b) in h[i..n].iter().zip(&x[i..n]) {
            sum = a.mul_add(*b, sum);
        }
        sum
    }

    #[target_feature(enable = "avx2,fma")]
    #[inline]
    unsafe fn dot_c32_avx2(h: &[f32], x: &[f32]) -> Complex32 {
        // SAFETY: the caller has the target features.
        let (mut re, mut im, i) = unsafe {
            let (v, i) = lanes(h, x);
            // Fold 8 lanes to 4: [l0+l4, l1+l5, l2+l6, l3+l7].
            let q = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
            // Even lanes are real parts, odd lanes imaginary: [re, im, ..].
            let q = _mm_add_ps(q, _mm_movehl_ps(q, q));
            (_mm_cvtss_f32(q), _mm_cvtss_f32(_mm_movehdup_ps(q)), i)
        };
        let n = h.len().min(x.len());
        // `i` is a multiple of 8, so the tail starts on a real part.
        for (j, (a, b)) in h[i..n].iter().zip(&x[i..n]).enumerate() {
            if j % 2 == 0 {
                re = a.mul_add(*b, re);
            } else {
                im = a.mul_add(*b, im);
            }
        }
        Complex32::new(re, im)
    }
}

/// The kernel bodies. Everything is `#[inline(always)]` so it is compiled
/// into each kernel set with that set's target features.
mod portable {
    use std::f32::consts::{FRAC_PI_2, PI};

    use num_complex::Complex32;

    /// `a·b + c`, fused when the set has FMA (without it, `mul_add` would be
    /// a slow library call).
    #[inline(always)]
    fn fma<const FMA: bool>(a: f32, b: f32, c: f32) -> f32 {
        if FMA { a.mul_add(b, c) } else { a * b + c }
    }

    /// Dot product folded into 8 partial sums, lane `j` holding the products
    /// at indices ≡ j (mod 8), so even lanes are real parts of interleaved
    /// complex data and odd lanes imaginary parts.
    #[inline(always)]
    pub fn dot_lanes<const FMA: bool>(h: &[f32], x: &[f32]) -> [f32; 8] {
        let n = h.len().min(x.len());
        let (h, x) = (&h[..n], &x[..n]);
        // 32 lanes: four 8-wide (or eight 4-wide) accumulators in flight
        // hide the add/FMA latency.
        let mut acc = [0.0f32; 32];
        let (mut hc, mut xc) = (h.chunks_exact(32), x.chunks_exact(32));
        for (a, b) in (&mut hc).zip(&mut xc) {
            let a: &[f32; 32] = a.try_into().unwrap();
            let b: &[f32; 32] = b.try_into().unwrap();
            for j in 0..32 {
                acc[j] = fma::<FMA>(a[j], b[j], acc[j]);
            }
        }
        let mut l = [0.0f32; 8];
        for j in 0..8 {
            l[j] = (acc[j] + acc[j + 8]) + (acc[j + 16] + acc[j + 24]);
        }
        let (h, x) = (hc.remainder(), xc.remainder());
        let (mut hc, mut xc) = (h.chunks_exact(8), x.chunks_exact(8));
        for (a, b) in (&mut hc).zip(&mut xc) {
            let a: &[f32; 8] = a.try_into().unwrap();
            let b: &[f32; 8] = b.try_into().unwrap();
            for j in 0..8 {
                l[j] = fma::<FMA>(a[j], b[j], l[j]);
            }
        }
        for (j, (a, b)) in hc.remainder().iter().zip(xc.remainder()).enumerate() {
            l[j] = fma::<FMA>(*a, *b, l[j]);
        }
        l
    }

    #[inline(always)]
    pub fn sum8(l: [f32; 8]) -> f32 {
        ((l[0] + l[1]) + (l[2] + l[3])) + ((l[4] + l[5]) + (l[6] + l[7]))
    }

    #[inline(always)]
    pub fn sum8_c32(l: [f32; 8]) -> Complex32 {
        Complex32::new((l[0] + l[2]) + (l[4] + l[6]), (l[1] + l[3]) + (l[5] + l[7]))
    }

    #[inline(always)]
    fn cmul<const FMA: bool>(ar: f32, ai: f32, br: f32, bi: f32) -> (f32, f32) {
        (fma::<FMA>(ar, br, -(ai * bi)), fma::<FMA>(ar, bi, ai * br))
    }

    /// Phasors renormalize every this many groups of 8 samples.
    const RENORM_GROUPS: usize = 64;

    #[inline(always)]
    pub fn rotate<const FMA: bool>(
        phasor: Complex32,
        steps: &[Complex32; 8],
        step8: Complex32,
        x: &[Complex32],
        y: &mut [Complex32],
    ) -> Complex32 {
        // Eight phasors one step apart, kept as separate re/im arrays so
        // each lane is plain vertical arithmetic.
        let (mut pr, mut pi) = ([0.0f32; 8], [0.0f32; 8]);
        for j in 0..8 {
            let p = phasor * steps[j];
            pr[j] = p.re;
            pi[j] = p.im;
        }
        let n = x.len().min(y.len());
        let (xc, yc) = (x[..n].chunks_exact(8), y[..n].chunks_exact_mut(8));
        for (g, (xs, ys)) in xc.zip(yc).enumerate() {
            let xs: &[Complex32; 8] = xs.try_into().unwrap();
            let ys: &mut [Complex32; 8] = ys.try_into().unwrap();
            for j in 0..8 {
                let (r, i) = cmul::<FMA>(xs[j].re, xs[j].im, pr[j], pi[j]);
                ys[j] = Complex32::new(r, i);
            }
            for j in 0..8 {
                (pr[j], pi[j]) = cmul::<FMA>(pr[j], pi[j], step8.re, step8.im);
            }
            if g % RENORM_GROUPS == RENORM_GROUPS - 1 {
                renorm(&mut pr, &mut pi);
            }
        }
        // Lane 0 now holds the phasor for the next unprocessed sample.
        let m = (pr[0] * pr[0] + pi[0] * pi[0]).sqrt();
        Complex32::new(pr[0] / m, pi[0] / m)
    }

    #[inline(always)]
    fn renorm(pr: &mut [f32; 8], pi: &mut [f32; 8]) {
        for j in 0..8 {
            let k = 1.0 / (pr[j] * pr[j] + pi[j] * pi[j]).sqrt();
            pr[j] *= k;
            pi[j] *= k;
        }
    }

    #[inline(always)]
    pub fn mul_c32<const FMA: bool>(a: &[Complex32], b: &[Complex32], y: &mut [Complex32]) {
        for ((y, a), b) in y.iter_mut().zip(a).zip(b) {
            let (r, i) = cmul::<FMA>(a.re, a.im, b.re, b.im);
            *y = Complex32::new(r, i);
        }
    }

    #[inline(always)]
    pub fn mul_c32_f32(x: &[Complex32], w: &[f32], y: &mut [Complex32]) {
        for ((y, x), &w) in y.iter_mut().zip(x).zip(w) {
            *y = Complex32::new(x.re * w, x.im * w);
        }
    }

    #[inline(always)]
    pub fn power<const FMA: bool>(x: &[Complex32], k: f32, y: &mut [f32]) {
        for (y, x) in y.iter_mut().zip(x) {
            *y = fma::<FMA>(x.re, x.re, x.im * x.im) * k;
        }
    }

    /// log2 of a positive normal float: exponent from the bits, mantissa
    /// reduced to [√½, √2) and fed to the atanh series
    /// `log2(m) = 2/ln2 · (s + s³/3 + s⁵/5 + s⁷/7)`, `s = (m−1)/(m+1)`.
    /// |s| ≤ 0.172, so the truncation error is below 2e-8.
    #[inline(always)]
    fn log2<const FMA: bool>(x: f32) -> f32 {
        let bits = x.to_bits();
        let mut e = ((bits >> 23) & 0xff) as i32 - 127;
        let mut m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
        let big = m > std::f32::consts::SQRT_2;
        m = if big { m * 0.5 } else { m };
        e += big as i32;
        let s = (m - 1.0) / (m + 1.0);
        let s2 = s * s;
        let mut p = 1.0 / 7.0;
        p = fma::<FMA>(p, s2, 1.0 / 5.0);
        p = fma::<FMA>(p, s2, 1.0 / 3.0);
        p = fma::<FMA>(p, s2, 1.0);
        fma::<FMA>(s * p, 2.0 * std::f32::consts::LOG2_E, e as f32)
    }

    #[inline(always)]
    pub fn power_to_db<const FMA: bool>(x: &[f32], y: &mut [f32]) {
        // 10·log10(x) = 10·log10(2)·log2(x)
        const K: f32 = 10.0 * std::f32::consts::LOG10_2;
        for (y, &x) in y.iter_mut().zip(x) {
            // max() also maps NaN to the floor.
            *y = K * log2::<FMA>(x.max(1e-20));
        }
    }

    #[inline(always)]
    pub fn magnitude<const FMA: bool>(x: &[Complex32], y: &mut [f32]) {
        for (y, x) in y.iter_mut().zip(x) {
            *y = fma::<FMA>(x.re, x.re, x.im * x.im).sqrt();
        }
    }

    /// atan on [0, 1]: Abramowitz & Stegun 4.4.49, |error| ≤ 2e-8.
    /// (Coefficients as published, more digits than an f32 holds.)
    #[allow(clippy::excessive_precision)]
    #[inline(always)]
    fn atan01<const FMA: bool>(t: f32) -> f32 {
        let t2 = t * t;
        let mut p = 0.002_866_225_7;
        p = fma::<FMA>(p, t2, -0.016_165_736_7);
        p = fma::<FMA>(p, t2, 0.042_909_613_8);
        p = fma::<FMA>(p, t2, -0.075_289_640_0);
        p = fma::<FMA>(p, t2, 0.106_562_639_3);
        p = fma::<FMA>(p, t2, -0.142_088_994_4);
        p = fma::<FMA>(p, t2, 0.199_935_508_5);
        p = fma::<FMA>(p, t2, -0.333_331_452_8);
        p = fma::<FMA>(p, t2, 1.0);
        t * p
    }

    #[inline(always)]
    pub fn atan2<const FMA: bool>(y: f32, x: f32) -> f32 {
        let (ax, ay) = (x.abs(), y.abs());
        let (mn, mx) = (ax.min(ay), ax.max(ay));
        // max() keeps 0/0 out: (0, 0) gives t = 0.
        let t = mn / mx.max(f32::MIN_POSITIVE);
        let mut r = atan01::<FMA>(t);
        r = if ay > ax { FRAC_PI_2 - r } else { r };
        r = if x < 0.0 { PI - r } else { r };
        r.copysign(y)
    }

    #[inline(always)]
    pub fn phase<const FMA: bool>(x: &[Complex32], y: &mut [f32]) {
        for (y, x) in y.iter_mut().zip(x) {
            *y = atan2::<FMA>(x.im, x.re);
        }
    }

    /// arg(a·conj(b))·gain.
    #[inline(always)]
    fn discriminate<const FMA: bool>(a: Complex32, b: Complex32, gain: f32) -> f32 {
        let re = fma::<FMA>(a.re, b.re, a.im * b.im);
        let im = fma::<FMA>(a.im, b.re, -(a.re * b.im));
        atan2::<FMA>(im, re) * gain
    }

    #[inline(always)]
    pub fn fm_discriminate<const FMA: bool>(
        prev: Complex32,
        x: &[Complex32],
        gain: f32,
        y: &mut [f32],
    ) -> Complex32 {
        let n = x.len().min(y.len());
        if n == 0 {
            return prev;
        }
        let (x, y) = (&x[..n], &mut y[..n]);
        y[0] = discriminate::<FMA>(x[0], prev, gain);
        for ((y, &a), &b) in y[1..].iter_mut().zip(&x[1..]).zip(&x[..n - 1]) {
            *y = discriminate::<FMA>(a, b, gain);
        }
        x[n - 1]
    }

    #[inline(always)]
    pub fn u8_to_f32<const FMA: bool>(x: &[u8], scale: f32, offset: f32, y: &mut [f32]) {
        for (y, &x) in y.iter_mut().zip(x) {
            *y = fma::<FMA>(x as f32, scale, offset);
        }
    }

    #[inline(always)]
    pub fn i16_to_f32(x: &[i16], scale: f32, y: &mut [f32]) {
        for (y, &x) in y.iter_mut().zip(x) {
            *y = x as f32 * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise in [-1, 1).
    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed.wrapping_mul(2_654_435_761).max(1);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn cnoise(n: usize, seed: u32) -> Vec<Complex32> {
        noise(2 * n, seed)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| Complex32::new(p[0], p[1]))
            .collect()
    }

    /// Lengths around every chunk boundary, plus long ones.
    fn lengths() -> impl Iterator<Item = usize> {
        (0..=70).chain([127, 128, 129, 511, 1000, 1001, 4099])
    }

    /// Slices starting 0..3 elements into the buffer, so loads are unaligned.
    const OFFSETS: [usize; 4] = [0, 1, 2, 3];

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol * (1.0 + b.abs())
    }

    #[test]
    fn baseline_and_best_are_available() {
        let all = Kernels::all();
        assert_eq!(all[0].level, Level::Baseline);
        assert_eq!(Kernels::best().level, all.last().unwrap().level);
        // The cached set is one of them.
        assert!(all.iter().any(|k| k.level == kernels().level));
        eprintln!(
            "kernel sets: {:?}",
            all.iter().map(|k| k.level.name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn dot_products_match_the_scalar_sum() {
        for k in Kernels::all() {
            for n in lengths() {
                for off in OFFSETS {
                    let h = &noise(n + off, 1)[off..];
                    let x = &noise(n + off + 1, 2)[off + 1..];
                    let want: f64 = h.iter().zip(x).map(|(a, b)| *a as f64 * *b as f64).sum();
                    let got = (k.dot_f32)(h, x);
                    // Error bound grows with n (rounding of n products).
                    let tol = 1e-6 * (n as f32 + 1.0);
                    assert!(
                        close(got, want as f32, tol),
                        "{:?} n={n}: {got} vs {want}",
                        k.level
                    );

                    let h2: Vec<f32> = h.iter().flat_map(|&v| [v, v]).collect();
                    let xc = &cnoise(n + off, 3)[off..];
                    let xf: &[f32] = bytemuck::cast_slice(xc);
                    let want = h
                        .iter()
                        .zip(xc)
                        .fold(Complex32::default(), |acc, (&h, &x)| acc + x * h);
                    let got = (k.dot_c32)(&h2, xf);
                    assert!(
                        close(got.re, want.re, tol) && close(got.im, want.im, tol),
                        "{:?} n={n}: {got} vs {want}",
                        k.level
                    );
                }
            }
        }
    }

    #[test]
    fn rotate_matches_a_running_phasor() {
        let freq = 0.0123f64;
        let steps: [Complex32; 8] = std::array::from_fn(|j| {
            let (s, c) = (std::f64::consts::TAU * freq * j as f64).sin_cos();
            Complex32::new(c as f32, s as f32)
        });
        let (s, c) = (std::f64::consts::TAU * freq * 8.0).sin_cos();
        let step8 = Complex32::new(c as f32, s as f32);
        let p0 = Complex32::from_polar(1.0, 0.3);
        for k in Kernels::all() {
            for n in lengths() {
                for off in OFFSETS {
                    let x = &cnoise(n + off, 4)[off..];
                    let mut y = vec![Complex32::default(); n];
                    let p = (k.rotate)(p0, &steps, step8, x, &mut y);
                    let done = n / 8 * 8;
                    for (i, (&x, &y)) in x.iter().zip(&y).enumerate().take(done) {
                        let ph =
                            (0.3 + std::f64::consts::TAU * freq * i as f64) % std::f64::consts::TAU;
                        let want = x * Complex32::from_polar(1.0, ph as f32);
                        assert!(
                            (y - want).norm() < 2e-5,
                            "{:?} n={n} i={i}: {y} vs {want}",
                            k.level
                        );
                    }
                    let ph =
                        (0.3 + std::f64::consts::TAU * freq * done as f64) % std::f64::consts::TAU;
                    let want = Complex32::from_polar(1.0, ph as f32);
                    assert!(
                        (p - want).norm() < 2e-5,
                        "{:?} n={n}: {p} vs {want}",
                        k.level
                    );
                    assert!(y[done..].iter().all(|v| *v == Complex32::default()));
                }
            }
        }
    }

    #[test]
    fn long_rotation_stays_on_the_unit_circle() {
        let freq = 0.1234567f64;
        let steps: [Complex32; 8] = std::array::from_fn(|j| {
            Complex32::from_polar(1.0, (std::f64::consts::TAU * freq * j as f64) as f32)
        });
        let step8 = Complex32::from_polar(1.0, (std::f64::consts::TAU * freq * 8.0) as f32);
        let x = vec![Complex32::new(1.0, 0.0); 1 << 20];
        let mut y = vec![Complex32::default(); x.len()];
        for k in Kernels::all() {
            (k.rotate)(Complex32::new(1.0, 0.0), &steps, step8, &x, &mut y);
            let worst = y.iter().map(|v| (v.norm() - 1.0).abs()).fold(0.0, f32::max);
            assert!(worst < 1e-5, "{:?}: |p| off by {worst}", k.level);
        }
    }

    #[test]
    fn elementwise_kernels_match_scalar_code() {
        for k in Kernels::all() {
            for n in lengths() {
                for off in OFFSETS {
                    let a = &cnoise(n + off, 5)[off..];
                    let b = &cnoise(n + off, 6)[off..];
                    let w = &noise(n + off, 7)[off..];
                    let mut yc = vec![Complex32::default(); n];
                    let mut yf = vec![0.0f32; n];

                    (k.mul_c32)(a, b, &mut yc);
                    for i in 0..n {
                        assert!((yc[i] - a[i] * b[i]).norm() < 1e-6, "{:?} mul_c32", k.level);
                    }
                    (k.mul_c32_f32)(a, w, &mut yc);
                    for i in 0..n {
                        assert_eq!(yc[i], a[i] * w[i], "{:?} mul_c32_f32", k.level);
                    }
                    (k.power)(a, 0.5, &mut yf);
                    for i in 0..n {
                        assert!(
                            close(yf[i], a[i].norm_sqr() * 0.5, 1e-6),
                            "{:?} power",
                            k.level
                        );
                    }
                    (k.magnitude)(a, &mut yf);
                    for i in 0..n {
                        assert!(close(yf[i], a[i].norm(), 1e-6), "{:?} magnitude", k.level);
                    }
                    (k.phase)(a, &mut yf);
                    for i in 0..n {
                        let want = (a[i].im as f64).atan2(a[i].re as f64) as f32;
                        assert!(
                            (yf[i] - want).abs() < 3e-7,
                            "{:?} phase {} vs {want}",
                            k.level,
                            yf[i]
                        );
                    }
                    let mut prev = Complex32::from_polar(1.0, 0.1);
                    let got_prev = (k.fm_discriminate)(prev, a, 2.0, &mut yf);
                    for i in 0..n {
                        let z = a[i] * prev.conj();
                        let want = (z.im as f64).atan2(z.re as f64) as f32 * 2.0;
                        assert!(
                            (yf[i] - want).abs() < 1e-6,
                            "{:?} fm {} vs {want}",
                            k.level,
                            yf[i]
                        );
                        prev = a[i];
                    }
                    assert_eq!(got_prev, prev);

                    let bytes: Vec<u8> = (0..n + off).map(|i| (i * 37 % 256) as u8).collect();
                    (k.u8_to_f32)(&bytes[off..], 1.0 / 127.5, -1.0, &mut yf);
                    for i in 0..n {
                        let want = (bytes[off + i] as f32 - 127.5) / 127.5;
                        assert!((yf[i] - want).abs() < 1e-6, "{:?} u8", k.level);
                    }
                    let shorts: Vec<i16> = (0..n + off)
                        .map(|i| (i as i32 * 7919 % 65536 - 32768) as i16)
                        .collect();
                    (k.i16_to_f32)(&shorts[off..], 1.0 / 32767.0, &mut yf);
                    for i in 0..n {
                        assert_eq!(
                            yf[i],
                            shorts[off + i] as f32 * (1.0 / 32767.0),
                            "{:?} i16",
                            k.level
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn power_to_db_is_accurate_over_the_whole_range() {
        let x: Vec<f32> = (0..20_000)
            .map(|i| 10f32.powf(-24.0 + 30.0 * i as f32 / 20_000.0))
            .chain([0.0, -1.0, f32::NAN, 1.0, 2.0, 0.5, f32::MIN_POSITIVE])
            .collect();
        for k in Kernels::all() {
            let mut y = vec![0.0; x.len()];
            (k.power_to_db)(&x, &mut y);
            let mut worst = 0.0f64;
            for (&x, &y) in x.iter().zip(&y) {
                let want = 10.0 * (x.max(1e-20) as f64).log10();
                let want = if x.is_nan() { -200.0 } else { want };
                worst = worst.max((y as f64 - want).abs());
            }
            assert!(worst < 1e-4, "{:?}: worst error {worst} dB", k.level);
        }
    }

    /// The approximation error of the fast atan2 over every direction and a
    /// wide range of magnitudes (printed with `--nocapture`).
    #[test]
    fn fast_atan2_error_is_tiny() {
        let mut worst = 0.0f64;
        for i in 0..200_000 {
            let th = -std::f64::consts::PI + std::f64::consts::TAU * i as f64 / 200_000.0;
            for r in [1e-6f64, 1e-3, 0.5, 1.0, 3.0e4] {
                let (y, x) = ((r * th.sin()) as f32, (r * th.cos()) as f32);
                let want = (y as f64).atan2(x as f64);
                worst = worst.max((atan2(y, x) as f64 - want).abs());
            }
        }
        eprintln!("fast atan2: worst error {worst:.3e} rad");
        assert!(worst < 3e-7, "worst error {worst}");
        // Axes and zero.
        assert_eq!(atan2(0.0, 0.0), 0.0);
        assert_eq!(atan2(0.0, 1.0), 0.0);
        assert!((atan2(1.0, 0.0) - std::f32::consts::FRAC_PI_2).abs() < 1e-7);
        assert!((atan2(0.0, -1.0) - std::f32::consts::PI).abs() < 1e-7);
        assert!((atan2(-1.0, 0.0) + std::f32::consts::FRAC_PI_2).abs() < 1e-7);
    }
}
