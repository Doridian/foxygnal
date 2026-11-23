//! Field pictures and dual prime, which ffmpeg's encoder never produces:
//! a tiny encoder here writes a stream that uses them (field pictures of
//! both parities and orders, field / 16×8 / dual-prime prediction, the
//! second field predicting from the first, skipped and intra macroblocks,
//! and frame pictures with field and dual-prime prediction), and ffmpeg's
//! decoding of it is the reference.
//!
//! The pictures are random DC levels plus a few AC coefficients; motion is
//! random. Skipped (with a note) without ffmpeg.

use std::process::{Command, Stdio};

use foxygnal_mpeg2::Decoder;

const W: usize = 176;
const H: usize = 128;
const MBW: usize = W / 16;

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.acc = self.acc << 1 | u64::from(v >> i & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    fn code(&mut self, s: &str) {
        for c in s.bytes().filter(|c| *c != b' ') {
            self.put(u32::from(c - b'0'), 1);
        }
    }

    fn start(&mut self, code: u8) {
        while self.n != 0 {
            self.put(0, 1);
        }
        self.out.extend_from_slice(&[0, 0, 1, code]);
    }
}

struct Rng(u32);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) % n
    }

    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + self.below((hi - lo + 1) as u32) as i32
    }
}

const DC_LUMA: [&str; 12] = [
    "100",
    "00",
    "01",
    "101",
    "110",
    "1110",
    "11110",
    "111110",
    "1111110",
    "11111110",
    "111111110",
    "111111111",
];
const DC_CHROMA: [&str; 12] = [
    "00",
    "01",
    "10",
    "110",
    "1110",
    "11110",
    "111110",
    "1111110",
    "11111110",
    "111111110",
    "1111111110",
    "1111111111",
];
const MOTION: [&str; 17] = [
    "1",
    "01",
    "001",
    "0001",
    "000011",
    "0000101",
    "0000100",
    "0000011",
    "000001011",
    "000001010",
    "000001001",
    "0000010001",
    "0000010000",
    "0000001111",
    "0000001110",
    "0000001101",
    "0000001100",
];

/// f_code for every vector: f = 2, range ±32 half pels.
const FCODE: u32 = 2;

#[derive(Clone, Copy, PartialEq)]
enum Structure {
    Top = 1,
    Bottom = 2,
    Frame = 3,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    I = 1,
    P = 2,
    B = 3,
}

/// What a picture may do.
#[derive(Clone, Copy)]
struct Pic {
    kind: Kind,
    frame: bool,
    /// The second field of the stream's first frame: only the first field
    /// can be predicted from.
    one_field: bool,
}

struct Enc {
    b: Bits,
    rng: Rng,
    dc: [i32; 3],
    /// [r][s][t], as the decoder keeps them.
    pmv: [[[i32; 2]; 2]; 2],
}

impl Enc {
    fn sequence(&mut self) {
        let b = &mut self.b;
        b.start(0xB3);
        b.put(W as u32, 12);
        b.put(H as u32, 12);
        b.put(2, 4); // 4:3
        b.put(3, 4); // 25 fps
        b.put(5000, 18);
        b.put(1, 1);
        b.put(112, 10);
        b.put(0, 3); // constrained, no matrices
        b.start(0xB5);
        b.put(1, 4);
        b.put(0x48, 8); // main profile, main level
        b.put(0, 1); // interlaced sequence
        b.put(1, 2); // 4:2:0
        b.put(0, 4);
        b.put(0, 12);
        b.put(1, 1);
        b.put(0, 8);
        b.put(0, 1); // B pictures follow
        b.put(0, 7);
        b.start(0xB8);
        b.put(0, 12);
        b.put(1, 1);
        b.put(0, 12);
        b.put(1, 1); // closed GOP
        b.put(0, 1);
    }

    fn picture(&mut self, tr: u32, kind: Kind, s: Structure, tff: bool) {
        let b = &mut self.b;
        b.start(0);
        b.put(tr, 10);
        b.put(kind as u32, 3);
        b.put(0xFFFF, 16);
        for _ in 0..(kind as u32 - 1) {
            b.put(0, 1);
            b.put(7, 3);
        }
        b.put(0, 1);
        b.start(0xB5);
        b.put(8, 4);
        let f = |used: bool| if used { FCODE } else { 15 };
        for used in [kind != Kind::I, kind == Kind::B] {
            b.put(f(used), 4);
            b.put(f(used), 4);
        }
        b.put(0, 2); // 8-bit DC
        b.put(s as u32, 2);
        b.put(u32::from(tff), 1);
        b.put(0, 1); // frame_pred_frame_dct = 0
        b.put(0, 6); // concealment, q_scale_type, intra_vlc, alt scan, rff, chroma_420_type
        b.put(0, 2); // progressive_frame, composite
    }

    fn dc(&mut self, c: usize, level: i32) {
        let diff = level - self.dc[c];
        self.dc[c] = level;
        let size = 32 - diff.unsigned_abs().leading_zeros();
        self.b
            .code(if c == 0 { DC_LUMA } else { DC_CHROMA }[size as usize]);
        if size > 0 {
            let v = if diff > 0 {
                diff
            } else {
                diff + (1 << size) - 1
            };
            self.b.put(v as u32, size);
        }
    }

    /// A few coefficients by escape codes, then end of block.
    fn ac(&mut self, first: usize) {
        let mut at = first;
        for _ in 0..self.rng.below(3) + u32::from(first == 0) {
            let run = self.rng.below(6) as usize;
            if at + run > 63 {
                break;
            }
            at += run + 1;
            let mut level = self.rng.range(-12, 12);
            if level == 0 {
                level = 3;
            }
            self.b.code("000001");
            self.b.put(run as u32, 6);
            self.b.put((level & 0xFFF) as u32, 12);
        }
        self.b.code("10");
    }

    fn intra_blocks(&mut self) {
        let y = self.rng.range(30, 220);
        for _ in 0..4 {
            let v = (y + self.rng.range(-20, 20)).clamp(1, 254);
            self.dc(0, v);
            self.ac(1);
        }
        for c in 1..3 {
            let v = self.rng.range(60, 200);
            self.dc(c, v);
            self.ac(1);
        }
    }

    fn reset_dc(&mut self) {
        self.dc = [128; 3];
    }

    /// One component of motion_vector(r, s).
    #[allow(clippy::too_many_arguments)]
    fn component(&mut self, r: usize, s: usize, t: usize, v: i32, half: bool, dmv: Option<i32>) {
        let f = 1 << (FCODE - 1);
        let pmv = self.pmv[r][s][t];
        let pred = if half { pmv >> 1 } else { pmv };
        let mut delta = v - pred;
        if delta < -16 * f {
            delta += 32 * f;
        } else if delta > 16 * f - 1 {
            delta -= 32 * f;
        }
        if delta == 0 {
            self.b.code("1");
        } else {
            let a = delta.abs();
            let code = (a - 1) / f + 1;
            self.b.code(MOTION[code as usize]);
            self.b.put(u32::from(delta < 0), 1);
            self.b.put(((a - 1) % f) as u32, FCODE - 1);
        }
        if let Some(d) = dmv {
            self.b.code(match d {
                0 => "0",
                1 => "10",
                _ => "11",
            });
        }
        self.pmv[r][s][t] = if half { v * 2 } else { v };
    }

    fn vector(&mut self, r: usize, s: usize, v: [i32; 2], half: bool, dmv: Option<[i32; 2]>) {
        self.component(r, s, 0, v[0], false, dmv.map(|d| d[0]));
        self.component(r, s, 1, v[1], half, dmv.map(|d| d[1]));
    }

    fn slices(&mut self, pic: Pic) {
        let rows = if pic.frame { H / 16 } else { H / 32 };
        for row in 0..rows {
            self.b.start(row as u8 + 1);
            self.b.put(8, 5); // quantiser_scale_code
            self.b.put(0, 1);
            self.reset_dc();
            self.pmv = Default::default();
            let mut col = 0;
            let mut inc = 1;
            while col < MBW {
                self.b.code(if inc == 1 { "1" } else { "011" });
                let interior = (1..MBW - 1).contains(&col) && (1..rows - 1).contains(&row);
                let intra = self.macroblock(pic, interior);
                // Skip the next macroblock now and then (never the last;
                // in B pictures not after an intra one).
                let skip = pic.kind != Kind::I
                    && !pic.one_field
                    && col + 2 < MBW
                    && !(intra && pic.kind == Kind::B);
                inc = if skip && self.rng.below(5) == 0 { 2 } else { 1 };
                if inc == 2 {
                    self.reset_dc();
                    if pic.kind == Kind::P {
                        // In P pictures a skipped macroblock resets the
                        // predictors; in B pictures it uses them as vectors.
                        self.pmv = Default::default();
                    }
                }
                col += inc;
            }
        }
    }

    /// One macroblock; true if intra.
    fn macroblock(&mut self, pic: Pic, interior: bool) -> bool {
        let frame = pic.frame;
        if pic.kind == Kind::I || self.rng.below(12) == 0 {
            self.b.code(match pic.kind {
                Kind::I => "1",
                _ => "00011",
            });
            if frame {
                self.b.put(self.rng.below(2), 1); // dct_type
            }
            // Intra macroblocks keep the DC predictors going.
            self.pmv = Default::default();
            self.intra_blocks();
            return true;
        }
        self.reset_dc();
        let coded = self.rng.below(3) != 0;
        let dirs: &[usize] = if pic.kind == Kind::P {
            // The first frame's second field has no field of its own parity
            // to predict from: no zero-vector macroblocks there.
            if !pic.one_field && coded && self.rng.below(4) == 0 {
                // No motion compensation, coded.
                self.b.code("01");
                if frame {
                    self.b.put(0, 1);
                }
                self.pmv = Default::default();
                self.coded_blocks();
                return false;
            }
            self.b.code(if coded { "1" } else { "001" });
            &[0]
        } else {
            match (self.rng.below(3), coded) {
                (0, false) => {
                    self.b.code("0010");
                    &[0]
                }
                (0, true) => {
                    self.b.code("0011");
                    &[0]
                }
                (1, false) => {
                    self.b.code("010");
                    &[1]
                }
                (1, true) => {
                    self.b.code("011");
                    &[1]
                }
                (_, false) => {
                    self.b.code("10");
                    &[0, 1]
                }
                (_, true) => {
                    self.b.code("11");
                    &[0, 1]
                }
            }
        };
        // Motion type: 1 field, 2 frame (or 16×8 in field pictures), 3 dual
        // prime (P pictures, needs both fields of the reference).
        let mt = if pic.kind == Kind::P && interior && !pic.one_field {
            self.rng.range(1, 3)
        } else if pic.one_field && frame {
            2
        } else {
            self.rng.range(1, 2)
        };
        self.b.put(mt as u32, 2);
        if frame && coded {
            self.b.put(self.rng.below(2), 1); // dct_type
        }
        let small = |rng: &mut Rng| {
            if interior {
                [rng.range(-9, 9), rng.range(-9, 9)]
            } else {
                [0, 0]
            }
        };
        let one = pic.one_field;
        let sel = |rng: &mut Rng| if one { 0 } else { rng.below(2) };
        for &s in dirs {
            match (frame, mt) {
                (false, 1) => {
                    let f = sel(&mut self.rng);
                    self.b.put(f, 1);
                    let v = small(&mut self.rng);
                    self.vector(0, s, v, false, None);
                    self.pmv[1][s] = self.pmv[0][s];
                }
                (false, 2) | (true, 1) => {
                    for r in 0..2 {
                        let f = sel(&mut self.rng);
                        self.b.put(f, 1);
                        let v = small(&mut self.rng);
                        self.vector(r, s, v, frame, None);
                    }
                }
                (true, 2) => {
                    let v = small(&mut self.rng);
                    self.vector(0, s, v, false, None);
                    self.pmv[1][s] = self.pmv[0][s];
                }
                _ => {
                    let v = [self.rng.range(-6, 6), self.rng.range(-4, 4)];
                    let d = [self.rng.range(-1, 1), self.rng.range(-1, 1)];
                    self.vector(0, s, v, frame, Some(d));
                    self.pmv[1][s] = self.pmv[0][s];
                }
            }
        }
        if coded {
            self.coded_blocks();
        }
        false
    }

    fn coded_blocks(&mut self) {
        self.b.code("001100"); // coded_block_pattern 63
        for _ in 0..6 {
            self.ac(0);
        }
    }
}

#[test]
fn field_pictures_and_dual_prime_match_ffmpeg() {
    let ok = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .status()
        .is_ok();
    if !ok {
        eprintln!("ffmpeg not found: skipping");
        return;
    }
    let mut e = Enc {
        b: Bits {
            out: Vec::new(),
            acc: 0,
            n: 0,
        },
        rng: Rng(99),
        dc: [128; 3],
        pmv: Default::default(),
    };
    e.sequence();
    use Kind::*;
    use Structure::*;
    // (temporal_reference, type, structure, top field first) per picture,
    // in decoding order; fields in pairs.
    let plan: &[(u32, Kind, Structure, bool)] = &[
        (0, I, Top, true),
        (0, P, Bottom, true),
        (3, P, Top, true),
        (3, P, Bottom, true),
        (1, B, Top, true),
        (1, B, Bottom, true),
        (2, B, Frame, true),
        (5, P, Frame, true),
        (4, B, Bottom, false),
        (4, B, Top, false),
        (7, P, Bottom, false),
        (7, P, Top, false),
        (6, B, Frame, false),
        (8, P, Frame, false),
        (9, P, Top, true),
        (9, P, Bottom, true),
    ];
    for (i, &(tr, kind, s, tff)) in plan.iter().enumerate() {
        e.picture(tr, kind, s, tff);
        e.slices(Pic {
            kind,
            frame: s == Frame,
            one_field: i == 1,
        });
    }
    e.b.start(0xB7);
    let stream = e.b.out;
    let frames = 10;

    let dir = std::env::temp_dir().join(format!("foxygnal-mpeg2-{}-synthetic", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let es = dir.join("fields.m2v");
    std::fs::write(&es, &stream).unwrap();
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&es)
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-",
        ])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = out.stdout;

    let mut d = Decoder::new();
    d.push(&stream, None);
    let mut got = Vec::new();
    while let Some(p) = d.decode() {
        got.push(p);
    }
    d.flush();
    while let Some(p) = d.decode() {
        got.push(p);
    }
    assert_eq!(d.errors(), 0);
    let fsize = W * H * 3 / 2;
    assert_eq!(want.len() / fsize, frames, "ffmpeg's frames");
    assert_eq!(got.len(), frames, "our frames");
    for (i, p) in got.iter().enumerate() {
        let r = &want[i * fsize..(i + 1) * fsize];
        let se: f64 = p
            .data
            .iter()
            .zip(r)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum();
        let psnr = if se == 0.0 {
            99.0
        } else {
            10.0 * (255.0f64.powi(2) * fsize as f64 / se).log10()
        };
        eprintln!("frame {i} ({}): {psnr:.1} dB", p.picture_type);
        // Bit-exact but for IDCT rounding: about 66 dB. A wrong prediction
        // in a few macroblocks drops to under 40.
        assert!(psnr > 55.0, "frame {i}: {psnr:.1} dB");
    }
}
