//! Tables of ISO/IEC 11172-3 (MPEG-1 audio) and 13818-3 (its lower sample
//! rates), and the ones computed from them when a decoder is made (no
//! floating-point functions in `core`, so `cos` is a short series here).

/// Bit rates in kbit/s by bitrate_index: MPEG-1 Layer I, MPEG-1 Layer II,
/// MPEG-1 Layer III, MPEG-2/2.5 Layer I, MPEG-2/2.5 Layer II and III.
pub const BITRATES: [[u16; 15]; 5] = [
    [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ],
    [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// Sample rates (Hz) of MPEG-1 by sampling_frequency; MPEG-2 halves them,
/// MPEG-2.5 quarters them.
pub const RATES: [u32; 3] = [44100, 48000, 32000];

/// Layer II quantisation classes (Table B.4): steps, and bits per code
/// word. A negative count means three samples share one grouped code word
/// of that many bits.
pub const STEPS: [u16; 17] = [
    3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535,
];
pub const CODE_BITS: [i8; 17] = [-5, -7, 3, -10, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

// The classes each allocation code (1..2^nbal) selects, per subband group.
// nbal is log2(len + 1).
const A0: &[u8] = &[0, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
const A1: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16];
const A2: &[u8] = &[0, 1, 2, 3, 4, 5, 16];
const A3: &[u8] = &[0, 1, 16];
const C0: &[u8] = &[0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const C1: &[u8] = &[0, 1, 3, 4, 5, 6, 7];
const L0: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
const L2: &[u8] = &[0, 1, 3];

/// Bit allocation tables as (subbands, classes) runs: 11172-3 B.2a–d, and
/// 13818-3 B.1 for the lower sample rates.
pub const ALLOC: [&[(u8, &[u8])]; 5] = [
    &[(3, A0), (8, A1), (12, A2), (4, A3)],
    &[(3, A0), (8, A1), (12, A2), (7, A3)],
    &[(2, C0), (6, C1)],
    &[(2, C0), (10, C1)],
    &[(4, L0), (7, C1), (19, L2)],
];

/// Which allocation table a Layer II frame uses (11172-3 Annex B).
pub fn alloc_table(lsf: bool, rate: u32, kbps_per_channel: u32) -> usize {
    let b = kbps_per_channel;
    if lsf {
        4
    } else if (rate == 48000 && b >= 56) || (56..=80).contains(&b) {
        0
    } else if rate != 48000 && b >= 96 {
        1
    } else if rate != 32000 && b <= 48 {
        2
    } else {
        3
    }
}

/// The synthesis window D[0..=256] of Table B.3, times 65536 (the table's
/// values are exactly these over 65536). The rest follows by symmetry.
const WINDOW: [i32; 257] = [
    0, -1, -1, -1, -1, -1, -1, -2, -2, -2, -2, -3, -3, -4, -4, -5, -5, -6, -7, -7, -8, -9, -10,
    -11, -13, -14, -16, -17, -19, -21, -24, -26, -29, -31, -35, -38, -41, -45, -49, -53, -58, -63,
    -68, -73, -79, -85, -91, -97, -104, -111, -117, -125, -132, -139, -147, -154, -161, -169, -176,
    -183, -190, -196, -202, -208, 213, 218, 222, 225, 227, 228, 228, 227, 224, 221, 215, 208, 200,
    189, 177, 163, 146, 127, 106, 83, 57, 29, -2, -36, -72, -111, -153, -197, -244, -294, -347,
    -401, -459, -519, -581, -645, -711, -779, -848, -919, -991, -1064, -1137, -1210, -1283, -1356,
    -1428, -1498, -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962, -2001, -2032, -2057,
    -2075, -2085, -2087, -2080, -2063, 2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535, 1414, 1280,
    1131, 970, 794, 605, 402, 185, -45, -288, -545, -814, -1095, -1388, -1692, -2006, -2330, -2663,
    -3004, -3351, -3705, -4063, -4425, -4788, -5153, -5517, -5879, -6237, -6589, -6935, -7271,
    -7597, -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585, -9727, -9838, -9916, -9959,
    -9966, -9935, -9863, -9750, -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134, 6574, 5959,
    5288, 4561, 3776, 2935, 2037, 1082, 70, -998, -2122, -3300, -4533, -5818, -7154, -8540, -9975,
    -11455, -12980, -14548, -16155, -17799, -19478, -21189, -22929, -24694, -26482, -28289, -30112,
    -31947, -33791, -35640, -37489, -39336, -41176, -43006, -44821, -46617, -48390, -50137, -51853,
    -53534, -55178, -56778, -58333, -59838, -61289, -62684, -64019, -65290, -66494, -67629, -68692,
    -69679, -70590, -71420, -72169, -72835, -73415, -73908, -74313, -74630, -74856, -74992, 75038,
];

/// The 512-tap window D (Table B.3).
pub fn window() -> [f32; 512] {
    let mut d = [0f32; 512];
    for (i, &w) in WINDOW.iter().enumerate() {
        let v = w as f32 / 65536.0;
        d[i] = v;
        if i != 0 {
            // D[512 − i] = −D[i], except at multiples of 64.
            d[512 - i] = if i % 64 == 0 { v } else { -v };
        }
    }
    d
}

/// cos(m·π/64).
fn cos_pi64(m: usize) -> f64 {
    let m = m % 128;
    let m = if m > 64 { 128 - m } else { m };
    if m > 32 {
        return -cos_pi64(64 - m);
    }
    // 0 ≤ x ≤ π/2: the series is exact to f64 well before 14 terms.
    let x = m as f64 * core::f64::consts::PI / 64.0;
    let (mut term, mut sum) = (1.0f64, 1.0f64);
    for n in 1..14 {
        term *= -x * x / ((2 * n - 1) * (2 * n)) as f64;
        sum += term;
    }
    sum
}

/// The synthesis matrix N[i][k] = cos((16 + i)(2k + 1)π/64), i < 64, k < 32.
pub fn matrix() -> [[f32; 32]; 64] {
    let mut n = [[0f32; 32]; 64];
    for (i, row) in n.iter_mut().enumerate() {
        for (k, v) in row.iter_mut().enumerate() {
            *v = cos_pi64((16 + i) * (2 * k + 1)) as f32;
        }
    }
    n
}

/// Scale factors: 2^(1 − i/3) (Table B.1). Index 63 isn't allowed; it gets
/// the formula's value.
pub fn scalefactors() -> [f32; 64] {
    const CUBE_ROOTS: [f64; 3] = [1.0, 0.793_700_525_984_099_7, 0.629_960_524_947_436_6];
    let mut s = [0f32; 64];
    let mut p = 2.0f64;
    for (i, v) in s.iter_mut().enumerate() {
        if i > 0 && i % 3 == 0 {
            p /= 2.0;
        }
        *v = (p * CUBE_ROOTS[i % 3]) as f32;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computed_tables() {
        assert!((cos_pi64(0) - 1.0).abs() < 1e-15);
        assert!((cos_pi64(32)).abs() < 1e-15);
        assert!((cos_pi64(64) + 1.0).abs() < 1e-15);
        assert!((cos_pi64(16) - core::f64::consts::FRAC_1_SQRT_2).abs() < 1e-15);
        assert!((cos_pi64(127) - cos_pi64(1)).abs() < 1e-15);
        let s = scalefactors();
        assert_eq!(s[0], 2.0);
        assert_eq!(s[3], 1.0);
        for (i, &v) in s.iter().enumerate() {
            let want = 2f64.powf(1.0 - i as f64 / 3.0);
            assert!((v as f64 / want - 1.0).abs() < 1e-7, "{i}");
        }
        let d = window();
        assert_eq!(d[1], -1.0 / 65536.0);
        assert_eq!(d[511], 1.0 / 65536.0);
        assert_eq!(d[256], 75038.0 / 65536.0);
        assert_eq!(d[448], d[64]);
        for t in ALLOC {
            for &(_, c) in t {
                assert!((c.len() + 1).is_power_of_two());
            }
        }
        let sb = |t: usize| ALLOC[t].iter().map(|r| r.0 as usize).sum::<usize>();
        assert_eq!([sb(0), sb(1), sb(2), sb(3), sb(4)], [27, 30, 8, 12, 30]);
    }
}
