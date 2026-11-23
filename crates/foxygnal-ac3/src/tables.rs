//! Tables of ATSC A/52 (AC-3), and the ones computed from them when a
//! decoder is made (no floating-point functions in `core`, so `cos`, `sqrt`
//! and the Bessel function are short series here).

/// Sample rates by fscod.
pub const RATES: [u32; 3] = [48000, 44100, 32000];

/// Bit rates in kbit/s by frmsizecod / 2.
pub const BITRATES: [u16; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

/// Full-bandwidth channels by acmod.
pub const FBW_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Words (16 bits) per frame at 44.1 kHz by frmsizecod / 2; odd codes add
/// one. At 48 and 32 kHz a frame is exactly 2 and 3 words per kbit/s.
pub const WORDS_44K: [u16; 19] = [
    69, 87, 104, 121, 139, 174, 208, 243, 278, 348, 417, 487, 557, 696, 835, 975, 1114, 1253, 1393,
];

/// Bands of the bit allocation: the first bin of each of the 50, and the end.
pub const BAND_START: [u8; 51] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 31, 34, 37, 40, 43, 46, 49, 55, 61, 67, 73, 79, 85, 97, 109, 121, 133, 157, 181,
    205, 229, 253,
];

/// The band of each bin.
pub const BIN_TO_BAND: [u8; 256] = {
    let mut t = [49u8; 256];
    let mut band = 0;
    while band < 50 {
        let mut bin = BAND_START[band] as usize;
        while bin < BAND_START[band + 1] as usize {
            t[bin] = band as u8;
            bin += 1;
        }
        band += 1;
    }
    t
};

/// log-addition table (latab).
pub const LOG_ADD: [u8; 260] = [
    0x40, 0x3f, 0x3e, 0x3d, 0x3c, 0x3b, 0x3a, 0x39, 0x38, 0x37, 0x36, 0x35, 0x34, 0x34, 0x33, 0x32,
    0x31, 0x30, 0x2f, 0x2f, 0x2e, 0x2d, 0x2c, 0x2c, 0x2b, 0x2a, 0x29, 0x29, 0x28, 0x27, 0x26, 0x26,
    0x25, 0x24, 0x24, 0x23, 0x23, 0x22, 0x21, 0x21, 0x20, 0x20, 0x1f, 0x1e, 0x1e, 0x1d, 0x1d, 0x1c,
    0x1c, 0x1b, 0x1b, 0x1a, 0x1a, 0x19, 0x19, 0x18, 0x18, 0x17, 0x17, 0x16, 0x16, 0x15, 0x15, 0x15,
    0x14, 0x14, 0x13, 0x13, 0x13, 0x12, 0x12, 0x12, 0x11, 0x11, 0x11, 0x10, 0x10, 0x10, 0x0f, 0x0f,
    0x0f, 0x0e, 0x0e, 0x0e, 0x0d, 0x0d, 0x0d, 0x0d, 0x0c, 0x0c, 0x0c, 0x0c, 0x0b, 0x0b, 0x0b, 0x0b,
    0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x09, 0x09, 0x09, 0x09, 0x09, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x04, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x02,
    0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
    0x02, 0x02, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// Hearing threshold (hth) by band and fscod.
pub const HEARING: [[u16; 3]; 50] = [
    [0x04d0, 0x04f0, 0x0580],
    [0x04d0, 0x04f0, 0x0580],
    [0x0440, 0x0460, 0x04b0],
    [0x0400, 0x0410, 0x0450],
    [0x03e0, 0x03e0, 0x0420],
    [0x03c0, 0x03d0, 0x03f0],
    [0x03b0, 0x03c0, 0x03e0],
    [0x03b0, 0x03b0, 0x03d0],
    [0x03a0, 0x03b0, 0x03c0],
    [0x03a0, 0x03a0, 0x03b0],
    [0x03a0, 0x03a0, 0x03b0],
    [0x03a0, 0x03a0, 0x03b0],
    [0x03a0, 0x03a0, 0x03a0],
    [0x0390, 0x03a0, 0x03a0],
    [0x0390, 0x0390, 0x03a0],
    [0x0390, 0x0390, 0x03a0],
    [0x0380, 0x0390, 0x03a0],
    [0x0380, 0x0380, 0x03a0],
    [0x0370, 0x0380, 0x03a0],
    [0x0370, 0x0380, 0x03a0],
    [0x0360, 0x0370, 0x0390],
    [0x0360, 0x0370, 0x0390],
    [0x0350, 0x0360, 0x0390],
    [0x0350, 0x0360, 0x0390],
    [0x0340, 0x0350, 0x0380],
    [0x0340, 0x0350, 0x0380],
    [0x0330, 0x0340, 0x0380],
    [0x0320, 0x0340, 0x0370],
    [0x0310, 0x0320, 0x0360],
    [0x0300, 0x0310, 0x0350],
    [0x02f0, 0x0300, 0x0340],
    [0x02f0, 0x02f0, 0x0330],
    [0x02f0, 0x02f0, 0x0320],
    [0x02f0, 0x02f0, 0x0310],
    [0x0300, 0x02f0, 0x0300],
    [0x0310, 0x0300, 0x02f0],
    [0x0340, 0x0320, 0x02f0],
    [0x0390, 0x0350, 0x02f0],
    [0x03e0, 0x0390, 0x0300],
    [0x0420, 0x03e0, 0x0310],
    [0x0460, 0x0420, 0x0330],
    [0x0490, 0x0450, 0x0350],
    [0x04a0, 0x04a0, 0x03c0],
    [0x0460, 0x0490, 0x0410],
    [0x0440, 0x0460, 0x0470],
    [0x0440, 0x0440, 0x04a0],
    [0x0520, 0x0480, 0x0460],
    [0x0800, 0x0630, 0x0440],
    [0x0840, 0x0840, 0x0450],
    [0x0840, 0x0840, 0x04e0],
];

/// Bit allocation pointer by address (baptab).
pub const BAP: [u8; 64] = [
    0, 1, 1, 1, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9,
    10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14, 14, 14, 14, 14, 14, 14,
    15, 15, 15, 15, 15, 15, 15, 15, 15,
];

/// Bits of an ungrouped mantissa by bap (6 to 15).
pub const MANT_BITS: [u8; 16] = [0, 0, 0, 3, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

pub const SLOW_DECAY: [u8; 4] = [0x0f, 0x11, 0x13, 0x15];
pub const FAST_DECAY: [u8; 4] = [0x3f, 0x53, 0x67, 0x7b];
pub const SLOW_GAIN: [u16; 4] = [0x540, 0x4d8, 0x478, 0x410];
pub const DB_PER_BIT: [u16; 4] = [0x000, 0x700, 0x900, 0xb00];
pub const FLOOR: [i16; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -0x800];
pub const FAST_GAIN: [u16; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

/// The first bin of each rematrixing band, and the end.
pub const REMAT_BANDS: [usize; 5] = [13, 25, 37, 61, 253];

/// Mix levels: +3, +1.5, 0, −1.5, −3, −4.5, −6, −∞ and −9 dB.
pub const GAIN_LEVELS: [f32; 9] = [
    core::f32::consts::SQRT_2,
    1.189_207_1,
    1.0,
    0.840_896_4,
    core::f32::consts::FRAC_1_SQRT_2,
    0.594_603_55,
    0.5,
    0.0,
    0.353_553_38,
];

/// cmixlev and surmixlev to `GAIN_LEVELS` (the reserved codes as the middle one).
pub const CENTER_LEVELS: [u8; 4] = [4, 5, 6, 5];
pub const SURROUND_LEVELS: [u8; 4] = [4, 6, 7, 6];

// ---- Computed --------------------------------------------------------------

/// cos and sin of x, |x| ≤ 2π (a Taylor series, exact to f64 there).
pub fn cos_sin(x: f64) -> (f64, f64) {
    use core::f64::consts::PI;
    let x = if x > PI {
        x - 2.0 * PI
    } else if x < -PI {
        x + 2.0 * PI
    } else {
        x
    };
    let (mut c, mut s) = (1.0f64, x);
    let (mut tc, mut ts) = (1.0f64, x);
    for n in 1..24 {
        tc *= -x * x / ((2 * n - 1) * (2 * n)) as f64;
        ts *= -x * x / ((2 * n) * (2 * n + 1)) as f64;
        c += tc;
        s += ts;
    }
    (c, s)
}

pub fn sqrt(v: f64) -> f64 {
    if v <= 0.0 {
        return 0.0;
    }
    let mut x = if v > 1.0 { v } else { 1.0 };
    for _ in 0..200 {
        let next = 0.5 * (x + v / x);
        if next == x {
            break;
        }
        x = next;
    }
    x
}

/// The Kaiser-Bessel-derived window, α = 5, the rising half of 512 (A/52
/// 7.9.4: the same values as its table).
pub fn window() -> [f32; 256] {
    const N: usize = 256;
    let alpha = 5.0 * core::f64::consts::PI / N as f64;
    let alpha2 = alpha * alpha;
    // I0(2α√(i(N − i))) as a series in (x/2)².
    let bessel = |i: usize| {
        let q = (i * (N - i)) as f64 * alpha2;
        let (mut term, mut sum) = (1.0f64, 1.0f64);
        for k in 1..80 {
            term *= q / (k * k) as f64;
            sum += term;
        }
        sum
    };
    let mut total = 1.0;
    for i in 0..N {
        total += bessel(i);
    }
    let mut w = [0f32; N];
    let mut sum = 0.0;
    for (i, v) in w.iter_mut().enumerate() {
        sum += bessel(i);
        *v = sqrt(sum / total) as f32;
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computed() {
        for i in 0..64 {
            let x = i as f64 * 0.1 - 3.2;
            let (c, s) = cos_sin(x);
            assert!(
                (c - x.cos()).abs() < 1e-14 && (s - x.sin()).abs() < 1e-14,
                "{x}"
            );
        }
        assert!((sqrt(2.0) - 2f64.sqrt()).abs() < 1e-15);
        assert!((sqrt(1e-6) - 1e-3).abs() < 1e-15);
        let w = window();
        // Princen-Bradley: w[n]² + w[255 − n]² = 1.
        for n in 0..128 {
            let p = w[n] * w[n] + w[255 - n] * w[255 - n];
            assert!((p - 1.0).abs() < 1e-6);
        }
        // A/52 table 7.33 starts 0.00014, 0.00024, 0.00037 and ends 0.99999.
        assert!((w[0] - 0.00014).abs() < 1e-5 && (w[2] - 0.00037).abs() < 1e-5);
        assert!((w[255] - 1.0).abs() < 1e-6);
        assert_eq!(BIN_TO_BAND[28], 28);
        assert_eq!(BIN_TO_BAND[30], 28);
        assert_eq!(BIN_TO_BAND[252], 49);
    }
}
