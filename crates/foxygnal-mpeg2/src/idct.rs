//! The inverse DCT: the integer Chen–Wang algorithm of the MPEG Software
//! Simulation Group's reference decoder, which meets IEEE 1180 accuracy.
//! Output is clipped to −256..255.

const W1: i32 = 2841; // 2048·√2·cos(1π/16)
const W2: i32 = 2676; // 2048·√2·cos(2π/16)
const W3: i32 = 2408; // 2048·√2·cos(3π/16)
const W5: i32 = 1609; // 2048·√2·cos(5π/16)
const W6: i32 = 1108; // 2048·√2·cos(6π/16)
const W7: i32 = 565; // 2048·√2·cos(7π/16)

#[inline]
fn row(b: &mut [i32; 8]) {
    let (x1, x2, x3, x4, x5, x6, x7) = (b[4] << 11, b[6], b[2], b[1], b[7], b[5], b[3]);
    if x1 | x2 | x3 | x4 | x5 | x6 | x7 == 0 {
        let v = (b[0] << 3).clamp(-32768, 32767);
        b.fill(v);
        return;
    }
    let mut x0 = (b[0] << 11) + 128;
    let (mut x1, mut x2, mut x3, mut x4, mut x5, mut x6, mut x7) = (x1, x2, x3, x4, x5, x6, x7);
    // First stage.
    let mut x8 = W7 * (x4 + x5);
    x4 = x8 + (W1 - W7) * x4;
    x5 = x8 - (W1 + W7) * x5;
    x8 = W3 * (x6 + x7);
    x6 = x8 - (W3 - W5) * x6;
    x7 = x8 - (W3 + W5) * x7;
    // Second stage.
    x8 = x0 + x1;
    x0 -= x1;
    x1 = W6 * (x3 + x2);
    x2 = x1 - (W2 + W6) * x2;
    x3 = x1 + (W2 - W6) * x3;
    x1 = x4 + x6;
    x4 -= x6;
    x6 = x5 + x7;
    x5 -= x7;
    // Third stage.
    x7 = x8 + x3;
    x8 -= x3;
    x3 = x0 + x2;
    x0 -= x2;
    x2 = ((181 * i64::from(x4 + x5) + 128) >> 8) as i32;
    x4 = ((181 * i64::from(x4 - x5) + 128) >> 8) as i32;
    // Fourth stage.
    // The reference keeps rows in 16 bits; only invalid input gets near.
    let s = |v: i32| (v >> 8).clamp(-32768, 32767);
    b[0] = s(x7 + x1);
    b[1] = s(x3 + x2);
    b[2] = s(x0 + x4);
    b[3] = s(x8 + x6);
    b[4] = s(x8 - x6);
    b[5] = s(x0 - x4);
    b[6] = s(x3 - x2);
    b[7] = s(x7 - x1);
}

#[inline]
fn clip(v: i32) -> i32 {
    v.clamp(-256, 255)
}

#[inline]
fn col(b: &mut [i32; 64], c: usize) {
    let (x1, x2, x3, x4, x5, x6, x7) = (
        b[c + 32] << 8,
        b[c + 48],
        b[c + 16],
        b[c + 8],
        b[c + 56],
        b[c + 40],
        b[c + 24],
    );
    if x1 | x2 | x3 | x4 | x5 | x6 | x7 == 0 {
        let v = clip((b[c] + 32) >> 6);
        for r in 0..8 {
            b[c + 8 * r] = v;
        }
        return;
    }
    let mut x0 = (b[c] << 8) + 8192;
    let (mut x1, mut x2, mut x3, mut x4, mut x5, mut x6, mut x7) = (x1, x2, x3, x4, x5, x6, x7);
    // First stage.
    let mut x8 = W7 * (x4 + x5) + 4;
    x4 = (x8 + (W1 - W7) * x4) >> 3;
    x5 = (x8 - (W1 + W7) * x5) >> 3;
    x8 = W3 * (x6 + x7) + 4;
    x6 = (x8 - (W3 - W5) * x6) >> 3;
    x7 = (x8 - (W3 + W5) * x7) >> 3;
    // Second stage.
    x8 = x0 + x1;
    x0 -= x1;
    x1 = W6 * (x3 + x2) + 4;
    x2 = (x1 - (W2 + W6) * x2) >> 3;
    x3 = (x1 + (W2 - W6) * x3) >> 3;
    x1 = x4 + x6;
    x4 -= x6;
    x6 = x5 + x7;
    x5 -= x7;
    // Third stage.
    x7 = x8 + x3;
    x8 -= x3;
    x3 = x0 + x2;
    x0 -= x2;
    // In 64 bits: with rows near the 16-bit limit (only from invalid
    // input) this product would overflow.
    x2 = ((181 * i64::from(x4 + x5) + 128) >> 8) as i32;
    x4 = ((181 * i64::from(x4 - x5) + 128) >> 8) as i32;
    // Fourth stage.
    b[c] = clip((x7 + x1) >> 14);
    b[c + 8] = clip((x3 + x2) >> 14);
    b[c + 16] = clip((x0 + x4) >> 14);
    b[c + 24] = clip((x8 + x6) >> 14);
    b[c + 32] = clip((x8 - x6) >> 14);
    b[c + 40] = clip((x0 - x4) >> 14);
    b[c + 48] = clip((x3 - x2) >> 14);
    b[c + 56] = clip((x7 - x1) >> 14);
}

/// In place, row-major coefficients in, samples out.
pub fn idct(b: &mut [i32; 64]) {
    for r in b.as_chunks_mut::<8>().0 {
        row(r);
    }
    for c in 0..8 {
        col(b, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defining formula, in floating point.
    fn reference(f: &[i32; 64]) -> [f64; 64] {
        let mut out = [0.0; 64];
        let c = |u: usize| if u == 0 { 0.5f64.sqrt() } else { 1.0 };
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0;
                for v in 0..8 {
                    for u in 0..8 {
                        s += c(u)
                            * c(v)
                            * f64::from(f[v * 8 + u])
                            * (((2 * x + 1) * u) as f64 * std::f64::consts::PI / 16.0).cos()
                            * (((2 * y + 1) * v) as f64 * std::f64::consts::PI / 16.0).cos();
                    }
                }
                out[y * 8 + x] = s / 4.0;
            }
        }
        out
    }

    #[test]
    fn matches_the_formula() {
        // A small LCG: reproducible pseudo-random blocks.
        let mut seed = 12345u32;
        let mut rnd = |m: i32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 16) as i32 % (2 * m + 1)) - m
        };
        let mut worst = 0.0f64;
        for _ in 0..500 {
            let mut b = [0i32; 64];
            for (i, v) in b.iter_mut().enumerate() {
                *v = if i < 10 { rnd(300) } else { rnd(5) };
            }
            let want = reference(&b);
            idct(&mut b);
            for (g, w) in b.iter().zip(want) {
                worst = worst.max((f64::from(*g) - w.clamp(-256.0, 255.0)).abs());
            }
        }
        assert!(worst <= 1.0, "peak error {worst}");
    }
}
