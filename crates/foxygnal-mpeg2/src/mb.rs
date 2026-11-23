//! Slices and macroblocks: motion vectors, coefficients, prediction and
//! reconstruction (ISO/IEC 13818-2 sections 6.2.4–6.2.6 and 7).

use crate::bits::Bits;
use crate::idct::idct;
use crate::vlc::{BWD, EOB, ESCAPE, FWD, INTRA, MB_ESCAPE, MB_STUFFING, PATTERN, QUANT, Tables};
use crate::{FRAME, FrameBuf, Pic, Quant, Seq};

/// Zigzag scan: scan position → raster index.
pub const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Alternate scan (for interlaced material).
pub const ALTERNATE: [u8; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19,
    27, 34, 42, 50, 58, 35, 43, 51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53,
    61, 22, 30, 7, 15, 23, 31, 38, 46, 54, 62, 39, 47, 55, 63,
];

const NONLINEAR_QUANT: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64,
    72, 80, 88, 96, 104, 112,
];

const P: u8 = 2;

/// How a macroblock is predicted.
#[derive(Clone, Copy, Default)]
pub struct Motion {
    /// `FWD` and/or `BWD`.
    pub flags: u8,
    /// frame_motion_type (frame pictures: 1 field, 2 frame, 3 dual prime) or
    /// field_motion_type (field pictures: 1 field, 2 16×8, 3 dual prime).
    pub mt: u8,
    /// [r][s][t]: vector r (first/second), direction s, horizontal/vertical,
    /// in half pels (field lines for field vectors).
    pub mv: [[[i32; 2]; 2]; 2],
    /// motion_vertical_field_select [r][s].
    pub sel: [[u8; 2]; 2],
    pub dmv: [i32; 2],
}

/// Per-slice working state, kept between calls to avoid allocations.
pub struct Scratch {
    blocks: [[i32; 64]; 6],
    pred_y: [u8; 256],
    pred_c: [[u8; 64]; 2],
    dc_pred: [i32; 3],
    pmv: [[[i32; 2]; 2]; 2],
    last: Motion,
    qcode: u32,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            blocks: [[0; 64]; 6],
            pred_y: [0; 256],
            pred_c: [[0; 64]; 2],
            dc_pred: [0; 3],
            pmv: Default::default(),
            last: Motion::default(),
            qcode: 1,
        }
    }
}

/// Everything a slice needs, borrowed from the decoder.
pub struct Ctx<'a> {
    pub t: &'a Tables,
    pub seq: &'a Seq,
    pub pic: &'a Pic,
    pub q: &'a Quant,
    pub frames: &'a mut [FrameBuf],
    pub cur: usize,
    pub fwd: Option<usize>,
    pub bwd: Option<usize>,
    pub s: &'a mut Scratch,
}

/// Predict one block of one plane, with half-pel interpolation, into `dst`
/// (rows `first`, `first + step`, … of a `dw`-wide buffer). Positions outside
/// the reference repeat its edge.
#[allow(clippy::too_many_arguments)]
fn pred_plane(
    src: &[u8],
    (base, stride, pw, ph): (usize, usize, usize, usize),
    (x, y, hx, hy): (i32, i32, bool, bool),
    (w, h): (usize, usize),
    dst: &mut [u8],
    (first, step, dw): (usize, usize, usize),
    avg: bool,
) {
    let inside = x >= 0
        && y >= 0
        && x as usize + w + usize::from(hx) <= pw
        && y as usize + h + usize::from(hy) <= ph;
    let mut tmp = [0u8; 16];
    for i in 0..h {
        let row = &mut tmp[..w];
        if inside {
            let o = base + (y as usize + i) * stride + x as usize;
            let a = &src[o..o + w + 1.min(pw - x as usize - w)];
            match (hx, hy) {
                (false, false) => row.copy_from_slice(&a[..w]),
                (true, false) => {
                    for (j, v) in row.iter_mut().enumerate() {
                        *v = ((u16::from(a[j]) + u16::from(a[j + 1]) + 1) >> 1) as u8;
                    }
                }
                (false, true) => {
                    let b = &src[o + stride..o + stride + w];
                    for (j, v) in row.iter_mut().enumerate() {
                        *v = ((u16::from(a[j]) + u16::from(b[j]) + 1) >> 1) as u8;
                    }
                }
                (true, true) => {
                    let b = &src[o + stride..o + stride + w + 1];
                    for (j, v) in row.iter_mut().enumerate() {
                        *v = ((u16::from(a[j])
                            + u16::from(a[j + 1])
                            + u16::from(b[j])
                            + u16::from(b[j + 1])
                            + 2)
                            >> 2) as u8;
                    }
                }
            }
        } else {
            let at = |xx: i32, yy: i32| {
                let xx = xx.clamp(0, pw as i32 - 1) as usize;
                let yy = yy.clamp(0, ph as i32 - 1) as usize;
                u16::from(src[base + yy * stride + xx])
            };
            let yy = y + i as i32;
            for (j, v) in row.iter_mut().enumerate() {
                let xx = x + j as i32;
                let s = match (hx, hy) {
                    (false, false) => at(xx, yy),
                    (true, false) => (at(xx, yy) + at(xx + 1, yy) + 1) >> 1,
                    (false, true) => (at(xx, yy) + at(xx, yy + 1) + 1) >> 1,
                    (true, true) => {
                        (at(xx, yy) + at(xx + 1, yy) + at(xx, yy + 1) + at(xx + 1, yy + 1) + 2) >> 2
                    }
                };
                *v = s as u8;
            }
        }
        let d = (first + i * step) * dw;
        let d = &mut dst[d..d + w];
        if avg {
            for (o, v) in d.iter_mut().zip(row.iter()) {
                *o = ((u16::from(*o) + u16::from(*v) + 1) >> 1) as u8;
            }
        } else {
            d.copy_from_slice(row);
        }
    }
}

impl Ctx<'_> {
    fn frame(&self) -> bool {
        self.pic.structure == FRAME
    }

    /// 0 for a top field, 1 for a bottom field.
    fn parity(&self) -> usize {
        usize::from(self.pic.structure == crate::BOTTOM)
    }

    fn mb_rows(&self) -> usize {
        if self.frame() {
            self.seq.mb_height
        } else {
            self.seq.mb_height / 2
        }
    }

    fn reset_dc(&mut self) {
        let v = 1 << (7 + self.pic.dc_precision);
        self.s.dc_pred = [v; 3];
    }

    /// Decode one slice (the bytes after its start code). False on an error;
    /// the rest of the row is then concealed.
    pub fn slice(&mut self, data: &[u8], code: u8) -> bool {
        let mbw = self.seq.mb_width;
        let count = mbw * self.mb_rows();
        let mut b = Bits::new(data);
        let mut row = usize::from(code) - 1;
        if self.seq.height > 2800 {
            row += (b.read(3) as usize) << 7;
        }
        self.s.qcode = b.read(5);
        if self.s.qcode == 0 {
            self.conceal(row * mbw);
            return false;
        }
        // intra_slice_flag etc. (MPEG-2) or extra_information_slice (MPEG-1).
        while b.bit() {
            b.skip(8);
            if b.overrun() {
                return false;
            }
        }
        self.reset_dc();
        self.s.pmv = Default::default();
        let mut prev = (row * mbw) as isize - 1;
        let mut first = true;
        loop {
            let mut inc = 0usize;
            loop {
                match self.t.mb_addr.decode(&mut b) {
                    Some(MB_ESCAPE) => inc += 33,
                    Some(MB_STUFFING) => {}
                    Some(v) => {
                        inc += v as usize;
                        break;
                    }
                    None => return self.fail(prev),
                }
                if b.overrun() {
                    return self.fail(prev);
                }
            }
            let addr = prev + inc as isize;
            if addr < 0 || addr as usize >= count {
                return self.fail(prev);
            }
            if !first {
                for a in prev + 1..addr {
                    if !self.skipped(a as usize) {
                        return self.fail(a - 1);
                    }
                }
            }
            if !self.macroblock(&mut b, addr as usize) || b.overrun() {
                return self.fail(addr - 1);
            }
            prev = addr;
            first = false;
            if b.peek(23) == 0 {
                return true;
            }
        }
    }

    fn fail(&mut self, last_good: isize) -> bool {
        let count = self.seq.mb_width * self.mb_rows();
        let from = (last_good + 1).max(0) as usize;
        if from < count {
            self.conceal(from);
        }
        false
    }

    /// Fill the rest of a macroblock row from the nearest reference.
    fn conceal(&mut self, from: usize) {
        let mbw = self.seq.mb_width;
        let end = ((from / mbw + 1) * mbw).min(mbw * self.mb_rows());
        let flags = if self.fwd.is_some() {
            FWD
        } else if self.bwd.is_some() {
            BWD
        } else {
            return;
        };
        let mut m = self.zero_motion();
        m.flags = flags;
        m.sel[0][1] = m.sel[0][0];
        for a in from..end {
            self.predict(&m, a);
            self.store(a, 0, false);
        }
    }

    fn zero_motion(&self) -> Motion {
        let mut m = Motion {
            flags: FWD,
            mt: if self.frame() { 2 } else { 1 },
            ..Motion::default()
        };
        m.sel[0][0] = self.parity() as u8;
        m
    }

    fn skipped(&mut self, addr: usize) -> bool {
        self.reset_dc();
        let m = match self.pic.coding_type {
            P => {
                self.s.pmv = Default::default();
                self.zero_motion()
            }
            3 => {
                // The previous macroblock's directions, but frame (or
                // same-parity field) prediction with the predictors as
                // vectors, whatever its own prediction type was.
                let mut m = self.zero_motion();
                m.flags = self.s.last.flags;
                for s in 0..2 {
                    m.mv[0][s] = self.s.pmv[0][s];
                    m.sel[0][s] = m.sel[0][0];
                }
                m
            }
            _ => return false,
        };
        self.predict(&m, addr);
        self.store(addr, 0, false);
        true
    }

    fn macroblock(&mut self, b: &mut Bits, addr: usize) -> bool {
        let pic = self.pic;
        let Some(flags) = self.t.mb_type[usize::from(pic.coding_type - 1)].decode(b) else {
            return false;
        };
        let flags = flags as u8;
        let intra = flags & INTRA != 0;
        let frame = self.frame();
        let mut mt = if frame { 2 } else { 1 };
        if flags & (FWD | BWD) != 0 && (!frame || !pic.fpfd) {
            mt = b.read(2) as u8;
            if mt == 0 {
                return false;
            }
        }
        let field_dct = frame && !pic.fpfd && (intra || flags & PATTERN != 0) && b.bit();
        if flags & QUANT != 0 {
            self.s.qcode = b.read(5);
            if self.s.qcode == 0 {
                return false;
            }
        }
        let mut m = Motion {
            flags: flags & (FWD | BWD),
            mt,
            ..Motion::default()
        };
        if intra {
            if pic.concealment {
                m.mt = if frame { 2 } else { 1 };
                if !self.motion_vectors(b, &mut m, 0) {
                    return false;
                }
                b.skip(1); // marker_bit
            } else {
                self.s.pmv = Default::default();
            }
        } else {
            self.reset_dc();
            if flags & FWD != 0 && !self.motion_vectors(b, &mut m, 0) {
                return false;
            }
            if flags & BWD != 0 && !self.motion_vectors(b, &mut m, 1) {
                return false;
            }
            if pic.coding_type == P && flags & FWD == 0 {
                // No motion compensation: a zero vector.
                self.s.pmv = Default::default();
                m = self.zero_motion();
            }
        }
        let cbp = if flags & PATTERN != 0 {
            match self.t.cbp.decode(b) {
                Some(v) => v as u8,
                None => return false,
            }
        } else if intra {
            63
        } else {
            0
        };
        for i in 0..6 {
            if cbp & (32 >> i) != 0 && !self.block(b, i, intra) {
                return false;
            }
        }
        if b.overrun() {
            return false;
        }
        if intra {
            self.s.pred_y.fill(0);
            self.s.pred_c = [[0; 64]; 2];
        } else {
            self.predict(&m, addr);
            self.s.last = m;
        }
        self.store(addr, cbp, field_dct);
        true
    }

    fn motion_vectors(&mut self, b: &mut Bits, m: &mut Motion, s: usize) -> bool {
        let (count, field, dmv) = match (self.frame(), m.mt) {
            (true, 1) => (2, true, false),
            (true, 2) => (1, false, false),
            (true, 3) => (1, true, true),
            (false, 1) => (1, true, false),
            (false, 2) => (2, true, false),
            (false, 3) => (1, true, true),
            _ => return false,
        };
        if count == 1 {
            if field && !dmv {
                m.sel[0][s] = b.read(1) as u8;
            }
            if !self.motion_vector(b, m, 0, s, field, dmv) {
                return false;
            }
            self.s.pmv[1][s] = self.s.pmv[0][s];
        } else {
            for r in 0..2 {
                m.sel[r][s] = b.read(1) as u8;
                if !self.motion_vector(b, m, r, s, field, false) {
                    return false;
                }
            }
        }
        true
    }

    fn motion_vector(
        &mut self,
        b: &mut Bits,
        m: &mut Motion,
        r: usize,
        s: usize,
        field: bool,
        dmv: bool,
    ) -> bool {
        for t in 0..2 {
            let Some(code) = self.t.motion.decode(b) else {
                return false;
            };
            let code = i32::from(code);
            let fcode = i32::from(self.pic.f_code[s][t]);
            if !(1..=9).contains(&fcode) {
                return false;
            }
            let rsize = fcode - 1;
            let residual = if rsize != 0 && code != 0 {
                b.read(rsize as u32) as i32
            } else {
                0
            };
            if dmv {
                let Some(d) = self.t.dmv.decode(b) else {
                    return false;
                };
                m.dmv[t] = i32::from(d);
            }
            let f = 1 << rsize;
            let delta = if f == 1 || code == 0 {
                code
            } else {
                let d = (code.abs() - 1) * f + residual + 1;
                if code < 0 { -d } else { d }
            };
            // Field vectors in frame pictures predict from (and update) the
            // frame-unit predictor.
            let half = field && t == 1 && self.frame();
            let pmv = self.s.pmv[r][s][t];
            let mut v = if half { pmv >> 1 } else { pmv } + delta;
            if v < -16 * f {
                v += 32 * f;
            } else if v > 16 * f - 1 {
                v -= 32 * f;
            }
            self.s.pmv[r][s][t] = if half { v * 2 } else { v };
            m.mv[r][s][t] = v;
        }
        true
    }

    fn block(&mut self, b: &mut Bits, i: usize, intra: bool) -> bool {
        let pic = self.pic;
        let mpeg2 = self.seq.mpeg2;
        let scan = if pic.alt_scan { &ALTERNATE } else { &ZIGZAG };
        let luma = i < 4;
        let w = match (intra, luma) {
            (true, true) => &self.q.intra,
            (true, false) => &self.q.chroma_intra,
            (false, true) => &self.q.non_intra,
            (false, false) => &self.q.chroma_non_intra,
        };
        let qs = if pic.q_scale_type {
            i32::from(NONLINEAR_QUANT[self.s.qcode as usize & 31])
        } else {
            2 * self.s.qcode as i32
        };
        let blk = &mut self.s.blocks[i];
        blk.fill(0);
        let mut sum = 0i32;
        let mut n;
        let table;
        if intra {
            let size = if luma {
                self.t.dc_luma.decode(b)
            } else {
                self.t.dc_chroma.decode(b)
            };
            let Some(size) = size else { return false };
            let size = size as u32;
            let diff = if size == 0 {
                0
            } else {
                let v = b.read(size) as i32;
                if v < 1 << (size - 1) {
                    v - (1 << size) + 1
                } else {
                    v
                }
            };
            let c = if luma { 0 } else { i - 3 };
            self.s.dc_pred[c] += diff;
            // Only a broken stream drifts out of range.
            self.s.dc_pred[c] = self.s.dc_pred[c].clamp(0, 2047 >> (3 - pic.dc_precision));
            let dc = self.s.dc_pred[c] << (3 - pic.dc_precision);
            blk[0] = dc;
            sum = dc;
            n = 1;
            table = if pic.intra_vlc {
                &self.t.b15
            } else {
                &self.t.b14
            };
        } else {
            n = 0;
            table = &self.t.b14;
            if b.peek(1) == 1 {
                // The first coefficient's `1s`: run 0, level 1.
                b.skip(1);
                let level = if b.bit() { -1 } else { 1 };
                let v = dequant(level, false, w[usize::from(scan[0])], qs, mpeg2);
                blk[usize::from(scan[0])] = v;
                sum += v;
                n = 1;
            }
        }
        loop {
            let e = table.decode(b);
            if e.len == 0 {
                return false;
            }
            let (run, level) = match e.run {
                EOB => break,
                ESCAPE => {
                    let run = b.read(6) as usize;
                    let level = if mpeg2 {
                        let v = b.read(12) as i32;
                        if v == 0 || v == 2048 {
                            return false;
                        }
                        if v >= 2048 { v - 4096 } else { v }
                    } else {
                        let v = b.read(8) as i32;
                        match v {
                            0 => b.read(8) as i32,
                            128 => b.read(8) as i32 - 256,
                            v if v > 128 => v - 256,
                            v => v,
                        }
                    };
                    (run, level)
                }
                run => {
                    let level = i32::from(e.level);
                    (usize::from(run), if b.bit() { -level } else { level })
                }
            };
            n += run;
            if n > 63 {
                return false;
            }
            let pos = usize::from(scan[n]);
            let v = dequant(level, intra, w[pos], qs, mpeg2);
            blk[pos] = v;
            sum += v;
            n += 1;
        }
        if mpeg2 && sum & 1 == 0 {
            // Mismatch control.
            blk[63] ^= 1;
        }
        idct(blk);
        true
    }

    /// The reference frame for direction `s`, and field `sel` in field
    /// pictures: the second field of a P frame may predict from the first.
    fn reference(&self, s: usize, sel: Option<u8>) -> Option<usize> {
        let r = if s == 1 {
            self.bwd
        } else {
            match sel {
                Some(f)
                    if !self.frame()
                        && self.pic.coding_type == P
                        && self.pic.second_field
                        && usize::from(f) != self.parity() =>
                {
                    Some(self.cur)
                }
                _ => self.fwd,
            }
        };
        r.or(self.fwd).or(self.bwd)
    }

    /// Predict a block of the macroblock (luma `w`×`h` at `x`, `y` in frame
    /// or field coordinates, and the chroma that goes with it) into the
    /// prediction buffers, at rows `first`, `first + step`, … of the
    /// macroblock. Blocks span the macroblock's width (`x` is its left edge).
    /// Without a reference (a broken stream), the block is grey.
    #[allow(clippy::too_many_arguments)]
    fn mc(
        &mut self,
        reference: Option<usize>,
        field: Option<u8>,
        (x, y, w, h): (i32, i32, usize, usize),
        mv: [i32; 2],
        (first, step): (usize, usize),
        avg: bool,
    ) {
        let Some(r) = reference else {
            let cf = if step == 2 { first } else { first / 2 };
            for i in 0..h {
                let o = (first + i * step) * 16;
                self.s.pred_y[o..o + w].fill(128);
            }
            for c in &mut self.s.pred_c {
                for i in 0..h / 2 {
                    let o = (cf + i * step) * 8;
                    c[o..o + w / 2].fill(128);
                }
            }
            return;
        };
        let f = &self.frames[r];
        let (fw, fh) = (f.w, f.h);
        let (base, stride, lines) = match field {
            None => (0, fw, fh),
            Some(p) => (usize::from(p) * fw, 2 * fw, fh / 2),
        };
        let [mx, my] = mv;
        pred_plane(
            &f.y,
            (base, stride, fw, lines),
            (x + (mx >> 1), y + (my >> 1), mx & 1 != 0, my & 1 != 0),
            (w, h),
            &mut self.s.pred_y,
            (first, step, 16),
            avg,
        );
        let cw = fw / 2;
        let (cbase, cstride, clines) = match field {
            None => (0, cw, fh / 2),
            Some(p) => (usize::from(p) * cw, 2 * cw, fh / 4),
        };
        let (cx, cy) = (mx / 2, my / 2);
        let cfirst = if step == 2 { first } else { first / 2 };
        for (plane, dst) in [&f.cb, &f.cr].into_iter().zip(self.s.pred_c.iter_mut()) {
            pred_plane(
                plane,
                (cbase, cstride, cw, clines),
                (
                    x / 2 + (cx >> 1),
                    y / 2 + (cy >> 1),
                    cx & 1 != 0,
                    cy & 1 != 0,
                ),
                (w / 2, h / 2),
                dst,
                (cfirst, step, 8),
                avg,
            );
        }
    }

    fn predict(&mut self, m: &Motion, addr: usize) {
        let mbw = self.seq.mb_width;
        let (mbx, mby) = ((addr % mbw) as i32, (addr / mbw) as i32);
        let x = mbx * 16;
        let frame = self.frame();
        if m.flags == 0 {
            self.s.pred_y.fill(128);
            self.s.pred_c = [[128; 64]; 2];
            return;
        }
        let mut avg = false;
        for s in 0..2 {
            if m.flags & (FWD << s) == 0 {
                continue;
            }
            let full = self.pic.full_pel[s];
            let v = |v: [i32; 2]| if full { [v[0] * 2, v[1] * 2] } else { v };
            match (frame, m.mt) {
                (true, 1) => {
                    for r in 0..2 {
                        let sel = m.sel[r][s];
                        let rf = self.reference(s, None);
                        self.mc(rf, Some(sel), (x, mby * 8, 16, 8), m.mv[r][s], (r, 2), avg);
                    }
                }
                (true, 3) => {
                    let rf = self.reference(0, None);
                    let [mx, my] = m.mv[0][0];
                    let [dx, dy] = m.dmv;
                    let k = if self.pic.tff { 1 } else { 3 };
                    let tb = [
                        ((mx * k + i32::from(mx > 0)) >> 1) + dx,
                        ((my * k + i32::from(my > 0)) >> 1) + dy - 1,
                    ];
                    let k = 4 - k;
                    let bt = [
                        ((mx * k + i32::from(mx > 0)) >> 1) + dx,
                        ((my * k + i32::from(my > 0)) >> 1) + dy + 1,
                    ];
                    let at = (x, mby * 8, 16, 8);
                    self.mc(rf, Some(0), at, [mx, my], (0, 2), false);
                    self.mc(rf, Some(1), at, [mx, my], (1, 2), false);
                    self.mc(rf, Some(1), at, tb, (0, 2), true);
                    self.mc(rf, Some(0), at, bt, (1, 2), true);
                }
                (true, _) => {
                    let rf = self.reference(s, None);
                    self.mc(rf, None, (x, mby * 16, 16, 16), v(m.mv[0][s]), (0, 1), avg);
                }
                (false, 2) => {
                    for r in 0..2 {
                        let sel = m.sel[r][s];
                        let rf = self.reference(s, Some(sel));
                        let at = (x, mby * 16 + 8 * r as i32, 16, 8);
                        self.mc(rf, Some(sel), at, m.mv[r][s], (8 * r, 1), avg);
                    }
                }
                (false, 3) => {
                    let par = self.parity() as u8;
                    let [mx, my] = m.mv[0][0];
                    let [dx, dy] = m.dmv;
                    let opp = [
                        ((mx + i32::from(mx > 0)) >> 1) + dx,
                        ((my + i32::from(my > 0)) >> 1) + dy + if par == 0 { -1 } else { 1 },
                    ];
                    let at = (x, mby * 16, 16, 16);
                    let same = self.reference(0, Some(par));
                    self.mc(same, Some(par), at, [mx, my], (0, 1), false);
                    let other = self.reference(0, Some(1 - par));
                    self.mc(other, Some(1 - par), at, opp, (0, 1), true);
                }
                (false, _) => {
                    let sel = m.sel[0][s];
                    let rf = self.reference(s, Some(sel));
                    self.mc(
                        rf,
                        Some(sel),
                        (x, mby * 16, 16, 16),
                        m.mv[0][s],
                        (0, 1),
                        avg,
                    );
                }
            }
            avg = true;
        }
    }

    /// Add the coded blocks to the prediction and write the macroblock.
    fn store(&mut self, addr: usize, cbp: u8, field_dct: bool) {
        let s = &mut *self.s;
        for i in 0..4 {
            if cbp & (32 >> i) == 0 {
                continue;
            }
            let blk = &s.blocks[i];
            let bx = (i & 1) * 8;
            let (first, step) = if field_dct {
                (i >> 1, 2)
            } else {
                ((i >> 1) * 8, 1)
            };
            for r in 0..8 {
                let o = (first + r * step) * 16 + bx;
                for (p, d) in s.pred_y[o..o + 8].iter_mut().zip(&blk[r * 8..r * 8 + 8]) {
                    *p = (i32::from(*p) + d).clamp(0, 255) as u8;
                }
            }
        }
        for c in 0..2 {
            if cbp & (2 >> c) == 0 {
                continue;
            }
            for (p, d) in s.pred_c[c].iter_mut().zip(&s.blocks[4 + c]) {
                *p = (i32::from(*p) + d).clamp(0, 255) as u8;
            }
        }
        let mbw = self.seq.mb_width;
        let (mbx, mby) = (addr % mbw, addr / mbw);
        let frame = self.pic.structure == FRAME;
        let par = usize::from(self.pic.structure == crate::BOTTOM);
        let f = &mut self.frames[self.cur];
        let w = f.w;
        let (base, stride) = if frame {
            (mby * 16 * w, w)
        } else {
            ((mby * 32 + par) * w, 2 * w)
        };
        for r in 0..16 {
            let o = base + r * stride + mbx * 16;
            f.y[o..o + 16].copy_from_slice(&s.pred_y[r * 16..r * 16 + 16]);
        }
        let cw = w / 2;
        let (base, stride) = if frame {
            (mby * 8 * cw, cw)
        } else {
            ((mby * 16 + par) * cw, 2 * cw)
        };
        for (plane, pred) in [&mut f.cb, &mut f.cr].into_iter().zip(&s.pred_c) {
            for r in 0..8 {
                let o = base + r * stride + mbx * 8;
                plane[o..o + 8].copy_from_slice(&pred[r * 8..r * 8 + 8]);
            }
        }
    }
}

/// Inverse quantisation of one coefficient (not the intra DC), saturated.
/// MPEG-1 makes each coefficient odd instead of the mismatch control.
#[inline]
fn dequant(level: i32, intra: bool, w: u8, qs: i32, mpeg2: bool) -> i32 {
    let k = if intra { 0 } else { level.signum() };
    let mut v = (2 * level + k) * i32::from(w) * qs / 32;
    if !mpeg2 && v & 1 == 0 && v != 0 {
        v -= v.signum();
    }
    v.clamp(-2048, 2047)
}
