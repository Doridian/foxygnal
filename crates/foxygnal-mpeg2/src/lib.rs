//! A compact MPEG-2 video decoder (ISO/IEC 13818-2 main profile: 4:2:0,
//! I/P/B, frame and field pictures, every prediction mode including dual
//! prime) that also reads MPEG-1 video (ISO/IEC 11172-2, without D pictures).
//!
//! It exists for the browser's Video instrument: WebCodecs decodes H.264 and
//! HEVC but not MPEG-2, which most DVB broadcasts still use. Built to
//! WebAssembly (`scripts/build-mpeg2-wasm.sh`), it imports nothing from its
//! host; see the `wasm` module for its C ABI.
//!
//! Feed it elementary stream bytes as they come ([`Decoder::push`], with a
//! PES packet's PTS), then take pictures in display order with
//! [`Decoder::decode`]. A picture is decoded once the start code after it
//! arrives; anchors (I and P) are held until the next one starts, so B
//! pictures can be shown before them. Broken slices are concealed from the
//! previous anchor and counted ([`Decoder::errors`]); garbage never panics.

mod bits;
mod idct;
mod mb;
mod vlc;
#[cfg(target_arch = "wasm32")]
mod wasm;

use std::collections::VecDeque;

use bits::Bits;
use mb::{Ctx, Scratch, ZIGZAG};
use vlc::Tables;

pub(crate) const BOTTOM: u8 = 2;
pub(crate) const FRAME: u8 = 3;

const PTS_MASK: u64 = (1 << 33) - 1;

/// Largest picture accepted (luma samples per side).
const MAX_SIZE: usize = 4096;

/// Bytes buffered without a start code before they are thrown away.
const MAX_UNIT: usize = 8 << 20;

const DEFAULT_INTRA: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
];

/// A decoded picture, in display order.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    /// I420: Y (`width`×`height`), then U and V (each half size, rounded up).
    pub data: Vec<u8>,
    /// 90 kHz, 33 bits. Interpolated from the previous picture if the
    /// stream gave none.
    pub pts: Option<u64>,
    /// Frames per second as a fraction.
    pub frame_rate: (u32, u32),
    /// aspect_ratio_information (1 square samples, 2 4:3, 3 16:9, 4 2.21:1).
    pub aspect: u8,
    /// 1 I, 2 P, 3 B.
    pub picture_type: u8,
    pub progressive: bool,
    pub top_field_first: bool,
    pub repeat_first_field: bool,
}

/// Sequence header and extension.
#[derive(Clone)]
pub(crate) struct Seq {
    pub width: usize,
    pub height: usize,
    pub aspect: u8,
    pub frame_rate: (u32, u32),
    pub mpeg2: bool,
    pub progressive: bool,
    pub chroma_format: u8,
    pub mb_width: usize,
    /// Macroblock rows of a frame.
    pub mb_height: usize,
}

impl Seq {
    fn layout(&mut self) {
        self.mb_width = self.width.div_ceil(16);
        self.mb_height = if self.mpeg2 && !self.progressive {
            2 * self.height.div_ceil(32)
        } else {
            self.height.div_ceil(16)
        };
    }
}

/// Quantiser matrices, in raster order.
pub(crate) struct Quant {
    pub intra: [u8; 64],
    pub non_intra: [u8; 64],
    pub chroma_intra: [u8; 64],
    pub chroma_non_intra: [u8; 64],
}

impl Default for Quant {
    fn default() -> Self {
        Self {
            intra: DEFAULT_INTRA,
            non_intra: [16; 64],
            chroma_intra: DEFAULT_INTRA,
            chroma_non_intra: [16; 64],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Header read, no slice yet.
    Pending,
    Decoding,
    Skip,
}

/// Picture header and coding extension.
pub(crate) struct Pic {
    pub coding_type: u8,
    pub f_code: [[u8; 2]; 2],
    pub full_pel: [bool; 2],
    pub dc_precision: u8,
    pub structure: u8,
    pub tff: bool,
    pub fpfd: bool,
    pub concealment: bool,
    pub q_scale_type: bool,
    pub intra_vlc: bool,
    pub alt_scan: bool,
    pub rff: bool,
    pub progressive_frame: bool,
    pub second_field: bool,
    pts: Option<u64>,
    state: State,
}

#[derive(Clone, Copy, Default)]
struct Meta {
    pts: Option<u64>,
    coding_type: u8,
    progressive: bool,
    tff: bool,
    rff: bool,
}

/// A frame store: coded size, 4:2:0.
#[derive(Default)]
pub(crate) struct FrameBuf {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub w: usize,
    pub h: usize,
    meta: Meta,
}

pub struct Decoder {
    t: Tables,
    buf: Vec<u8>,
    /// Stream offset of `buf[0]`.
    base: u64,
    /// Where to look for the next start code.
    scan: usize,
    /// The start code of the unit being collected.
    unit: Option<usize>,
    /// PES timestamps: (first byte, end, PTS) of each pushed chunk.
    pts: VecDeque<(u64, u64, u64)>,
    seq: Option<Seq>,
    q: Quant,
    pic: Option<Pic>,
    closed_gop: bool,
    frames: Vec<FrameBuf>,
    /// The older and newer anchor (I or P frame).
    prev: Option<usize>,
    last: Option<usize>,
    /// `last` was output already.
    last_out: bool,
    /// The frame being decoded.
    cur: usize,
    in_progress: bool,
    /// The first field of `cur` is done (its parity); waiting for the second.
    pending_field: Option<u8>,
    /// A first field was skipped: skip its second field too.
    skip_field: Option<u8>,
    out: VecDeque<Picture>,
    last_pts: Option<u64>,
    errors: u32,
    s: Scratch,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// The next start code (00 00 01 xx) at or after `from` whose code byte is
/// in `buf`.
fn find_start(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 < buf.len() {
        if buf[i + 2] > 1 {
            i += 3;
        } else if buf[i + 2] == 1 && buf[i] == 0 && buf[i + 1] == 0 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

fn read_matrix(b: &mut Bits, m: &mut [u8; 64]) {
    for &z in &ZIGZAG {
        m[usize::from(z)] = b.read(8) as u8;
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            t: Tables::new(),
            buf: Vec::new(),
            base: 0,
            scan: 0,
            unit: None,
            pts: VecDeque::new(),
            seq: None,
            q: Quant::default(),
            pic: None,
            closed_gop: false,
            frames: (0..3).map(|_| FrameBuf::default()).collect(),
            prev: None,
            last: None,
            last_out: true,
            cur: 0,
            in_progress: false,
            pending_field: None,
            skip_field: None,
            out: VecDeque::new(),
            last_pts: None,
            errors: 0,
            s: Scratch::default(),
        }
    }

    /// Slices concealed or pictures dropped because of errors, so far.
    pub fn errors(&self) -> u32 {
        self.errors
    }

    /// Take elementary stream bytes. `pts` (90 kHz) belongs to the first
    /// picture whose start code begins in them, as with a PES packet.
    pub fn push(&mut self, es: &[u8], pts: Option<u64>) {
        let start = self.base + self.buf.len() as u64;
        if let Some(p) = pts {
            if self.pts.len() >= 64 {
                self.pts.pop_front();
            }
            self.pts
                .push_back((start, start + es.len() as u64, p & PTS_MASK));
        }
        self.buf.extend_from_slice(es);
    }

    /// The next picture in display order, if one is ready.
    pub fn decode(&mut self) -> Option<Picture> {
        loop {
            if let Some(p) = self.out.pop_front() {
                return Some(p);
            }
            if !self.step() {
                return None;
            }
        }
    }

    /// The stream ended: decode what is buffered and release the held
    /// anchor. Pictures come out of [`Decoder::decode`].
    pub fn flush(&mut self) {
        while self.step() {}
        if let Some(u) = self.unit.take() {
            let buf = std::mem::take(&mut self.buf);
            self.unit_done(buf[u + 3], &buf[u + 4..], self.base + u as u64);
            self.buf = buf;
        }
        self.base += self.buf.len() as u64;
        self.buf.clear();
        self.scan = 0;
        self.end_of_sequence();
    }

    /// Start over after a discontinuity: drop buffered data and references,
    /// and wait for a sequence header and an I picture.
    pub fn reset(&mut self) {
        self.base += self.buf.len() as u64;
        self.buf.clear();
        self.scan = 0;
        self.unit = None;
        self.pts.clear();
        self.seq = None;
        self.pic = None;
        self.prev = None;
        self.last = None;
        self.last_out = true;
        self.in_progress = false;
        self.pending_field = None;
        self.skip_field = None;
        self.out.clear();
        self.last_pts = None;
    }

    /// Find the next start code; process the unit it ends. False if more
    /// data is needed.
    fn step(&mut self) -> bool {
        match find_start(&self.buf, self.scan) {
            Some(p) => {
                if let Some(u) = self.unit {
                    let buf = std::mem::take(&mut self.buf);
                    self.unit_done(buf[u + 3], &buf[u + 4..p], self.base + u as u64);
                    self.buf = buf;
                }
                self.unit = Some(p);
                self.scan = p + 4;
                true
            }
            None => {
                self.scan = self.scan.max(self.buf.len().saturating_sub(3));
                let keep = match self.unit {
                    Some(u) if self.buf.len() - u > MAX_UNIT => {
                        self.errors += 1;
                        self.unit = None;
                        self.buf.len() - 3
                    }
                    Some(u) => u,
                    None => self.buf.len().saturating_sub(3),
                };
                if keep > 0 {
                    self.buf.drain(..keep);
                    self.base += keep as u64;
                    self.scan -= keep;
                    if let Some(u) = &mut self.unit {
                        *u -= keep;
                    }
                }
                let now = self.base;
                while self.pts.front().is_some_and(|&(_, end, _)| end <= now) {
                    self.pts.pop_front();
                }
                false
            }
        }
    }

    fn unit_done(&mut self, code: u8, data: &[u8], at: u64) {
        match code {
            0x00 => {
                self.finish_picture();
                self.picture_header(data, at);
            }
            0x01..=0xAF => self.slice(data, code),
            0xB3 => {
                self.finish_picture();
                self.sequence_header(data);
            }
            0xB5 => self.extension(data),
            0xB7 => {
                self.finish_picture();
                self.end_of_sequence();
            }
            0xB8 => {
                self.finish_picture();
                // time_code (25 bits), closed_gop, broken_link
                let mut b = Bits::new(data);
                b.skip(25);
                self.closed_gop = b.bit();
            }
            _ => {}
        }
    }

    fn sequence_header(&mut self, data: &[u8]) {
        let mut b = Bits::new(data);
        let width = b.read(12) as usize;
        let height = b.read(12) as usize;
        let aspect = b.read(4) as u8;
        let rate = match b.read(4) {
            1 => (24000, 1001),
            2 => (24, 1),
            3 => (25, 1),
            4 => (30000, 1001),
            5 => (30, 1),
            6 => (50, 1),
            7 => (60000, 1001),
            8 => (60, 1),
            _ => (0, 1),
        };
        b.skip(18 + 1 + 10 + 1); // bit rate, marker, VBV buffer size, constrained
        let mut q = Quant::default();
        if b.bit() {
            read_matrix(&mut b, &mut q.intra);
            q.chroma_intra = q.intra;
        }
        if b.bit() {
            read_matrix(&mut b, &mut q.non_intra);
            q.chroma_non_intra = q.non_intra;
        }
        if width == 0 || height == 0 || b.overrun() {
            self.seq = None;
            return;
        }
        self.q = q;
        let mut seq = Seq {
            width,
            height,
            aspect,
            frame_rate: rate,
            // Until a sequence extension says otherwise.
            mpeg2: false,
            progressive: true,
            chroma_format: 1,
            mb_width: 0,
            mb_height: 0,
        };
        seq.layout();
        self.seq = Some(seq);
    }

    fn extension(&mut self, data: &[u8]) {
        let mut b = Bits::new(data);
        match b.read(4) {
            1 => {
                let Some(seq) = &mut self.seq else { return };
                b.skip(8); // profile and level
                seq.progressive = b.bit();
                seq.chroma_format = b.read(2) as u8;
                seq.width |= (b.read(2) as usize) << 12;
                seq.height |= (b.read(2) as usize) << 12;
                b.skip(12 + 1 + 8 + 1); // bit rate, marker, VBV, low_delay
                let n = b.read(2) + 1;
                let d = b.read(5) + 1;
                seq.frame_rate = (seq.frame_rate.0 * n, seq.frame_rate.1 * d);
                seq.mpeg2 = true;
                seq.layout();
            }
            3 => {
                let q = &mut self.q;
                if b.bit() {
                    read_matrix(&mut b, &mut q.intra);
                    q.chroma_intra = q.intra;
                }
                if b.bit() {
                    read_matrix(&mut b, &mut q.non_intra);
                    q.chroma_non_intra = q.non_intra;
                }
                if b.bit() {
                    read_matrix(&mut b, &mut q.chroma_intra);
                }
                if b.bit() {
                    read_matrix(&mut b, &mut q.chroma_non_intra);
                }
            }
            8 => {
                let Some(p) = &mut self.pic else { return };
                if p.state != State::Pending {
                    return;
                }
                for f in p.f_code.iter_mut().flatten() {
                    *f = b.read(4) as u8;
                }
                p.full_pel = [false; 2];
                p.dc_precision = b.read(2) as u8;
                p.structure = b.read(2) as u8;
                p.tff = b.bit();
                p.fpfd = b.bit();
                p.concealment = b.bit();
                p.q_scale_type = b.bit();
                p.intra_vlc = b.bit();
                p.alt_scan = b.bit();
                p.rff = b.bit();
                b.skip(1); // chroma_420_type
                p.progressive_frame = b.bit();
                if p.structure == 0 {
                    p.state = State::Skip;
                }
            }
            _ => {}
        }
    }

    fn picture_header(&mut self, data: &[u8], at: u64) {
        let mut b = Bits::new(data);
        b.skip(10); // temporal_reference
        let coding_type = b.read(3) as u8;
        b.skip(16); // vbv_delay
        let mut f_code = [[7u8; 2]; 2];
        let mut full_pel = [false; 2];
        for s in 0..2 {
            if coding_type == 2 + s as u8 || (s == 0 && coding_type == 3) {
                full_pel[s] = b.bit();
                let f = b.read(3) as u8;
                f_code[s] = [f, f];
            }
        }
        let mut pts = None;
        while let Some(&(start, end, p)) = self.pts.front() {
            if end <= at {
                self.pts.pop_front();
                continue;
            }
            if start <= at {
                self.pts.pop_front();
                pts = Some(p);
            }
            break;
        }
        self.pic = Some(Pic {
            coding_type,
            f_code,
            full_pel,
            dc_precision: 0,
            structure: FRAME,
            tff: false,
            fpfd: true,
            concealment: false,
            q_scale_type: false,
            intra_vlc: false,
            alt_scan: false,
            rff: false,
            progressive_frame: true,
            second_field: false,
            pts,
            state: State::Pending,
        });
    }

    /// Allocate the frame stores for this sequence; false if unsupported.
    fn layout(&mut self) -> bool {
        let Some(seq) = &self.seq else { return false };
        if seq.chroma_format != 1 || seq.width > MAX_SIZE || seq.height > MAX_SIZE {
            return false;
        }
        let (w, h) = (seq.mb_width * 16, seq.mb_height * 16);
        if self.frames[0].w != w || self.frames[0].h != h {
            for f in &mut self.frames {
                f.w = w;
                f.h = h;
                f.y = vec![0; w * h];
                f.cb = vec![128; w * h / 4];
                f.cr = vec![128; w * h / 4];
            }
            self.prev = None;
            self.last = None;
            self.last_out = true;
            self.in_progress = false;
            self.pending_field = None;
        }
        true
    }

    /// The first slice of a picture: decide whether it can be decoded, and
    /// where it goes.
    fn begin_picture(&mut self) {
        let Some(pic) = &self.pic else { return };
        let (ct, structure) = (pic.coding_type, pic.structure);
        let field = structure != FRAME;
        let parity = structure - 1;
        let mut ok = self.layout() && (1..=3).contains(&ct);
        if !ok {
            self.errors += 1;
        }
        if ok && field {
            if let Some(first) = self.pending_field {
                let first_b = self.frames[self.cur].meta.coding_type == 3;
                if first != parity && first_b == (ct == 3) {
                    // The second field of the frame in progress.
                    self.pending_field = None;
                    if let Some(p) = &mut self.pic {
                        p.second_field = true;
                        p.state = State::Decoding;
                    }
                    return;
                }
            }
            if self.skip_field.take().is_some_and(|f| f != parity) {
                ok = false;
            }
        }
        if self.in_progress {
            self.complete_frame();
        }
        ok = ok
            && match ct {
                2 => self.last.is_some(),
                3 => self.last.is_some() && (self.prev.is_some() || self.closed_gop),
                _ => true,
            };
        let Some(pic) = &mut self.pic else { return };
        if !ok {
            pic.state = State::Skip;
            if field {
                self.skip_field = Some(parity);
            }
            return;
        }
        pic.state = State::Decoding;
        let meta = Meta {
            pts: pic.pts,
            coding_type: ct,
            progressive: pic.progressive_frame,
            tff: pic.tff,
            rff: pic.rff,
        };
        if ct == 3 {
            self.cur = (0..3)
                .find(|&i| Some(i) != self.prev && Some(i) != self.last)
                .unwrap_or(0);
        } else {
            if let Some(l) = self.last
                && !self.last_out
            {
                self.emit(l);
            }
            let old = self.prev;
            self.prev = self.last;
            self.cur = (0..3)
                .find(|&i| Some(i) != self.prev && Some(i) != old)
                .or_else(|| (0..3).find(|&i| Some(i) != self.prev))
                .unwrap_or(0);
            self.last = Some(self.cur);
            self.last_out = false;
        }
        self.frames[self.cur].meta = meta;
        self.in_progress = true;
    }

    fn slice(&mut self, data: &[u8], code: u8) {
        let Some(pic) = &self.pic else { return };
        if pic.state == State::Pending {
            self.begin_picture();
        }
        let Self {
            t,
            seq,
            q,
            pic,
            frames,
            prev,
            last,
            cur,
            s,
            ..
        } = self;
        let (Some(pic), Some(seq)) = (pic.as_ref(), seq.as_ref()) else {
            return;
        };
        if pic.state != State::Decoding {
            return;
        }
        let mut ctx = Ctx {
            t,
            seq,
            pic,
            q,
            frames,
            cur: *cur,
            fwd: *prev,
            bwd: if pic.coding_type == 3 { *last } else { None },
            s,
        };
        if pic.coding_type == 3 && ctx.fwd.is_none() {
            // A closed GOP's first B pictures predict backwards only.
            ctx.fwd = *last;
        }
        if !ctx.slice(data, code) {
            self.errors += 1;
        }
    }

    /// The end of a picture's slices.
    fn finish_picture(&mut self) {
        let Some(pic) = self.pic.take() else { return };
        if pic.state != State::Decoding {
            return;
        }
        if pic.structure == FRAME || pic.second_field {
            self.complete_frame();
        } else {
            self.pending_field = Some(pic.structure - 1);
        }
    }

    fn complete_frame(&mut self) {
        self.in_progress = false;
        self.pending_field = None;
        if self.frames[self.cur].meta.coding_type == 3 {
            self.emit(self.cur);
        }
    }

    fn end_of_sequence(&mut self) {
        self.finish_picture();
        if self.in_progress {
            self.complete_frame();
        }
        if let Some(l) = self.last
            && !self.last_out
        {
            self.emit(l);
            self.last_out = true;
        }
    }

    /// Queue frame `i` for output, cropped to the display size.
    fn emit(&mut self, i: usize) {
        let Some(seq) = &self.seq else { return };
        let f = &self.frames[i];
        let (w, h) = (seq.width.min(f.w), seq.height.min(f.h));
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut data = Vec::with_capacity(w * h + 2 * cw * ch);
        for r in 0..h {
            data.extend_from_slice(&f.y[r * f.w..r * f.w + w]);
        }
        for plane in [&f.cb, &f.cr] {
            for r in 0..ch {
                data.extend_from_slice(&plane[r * f.w / 2..r * f.w / 2 + cw]);
            }
        }
        let (num, den) = seq.frame_rate;
        let pts = f.meta.pts.or_else(|| {
            let last = self.last_pts?;
            (num > 0).then(|| (last + 90_000 * u64::from(den) / u64::from(num)) & PTS_MASK)
        });
        self.last_pts = pts;
        if i == self.last.unwrap_or(usize::MAX) {
            self.last_out = true;
        }
        self.out.push_back(Picture {
            width: w,
            height: h,
            data,
            pts,
            frame_rate: seq.frame_rate,
            aspect: seq.aspect,
            picture_type: f.meta.coding_type,
            progressive: f.meta.progressive,
            top_field_first: f.meta.tff,
            repeat_first_field: f.meta.rff,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_start_codes() {
        let b = [9, 0, 0, 1, 0xB3, 7, 0, 0, 0, 1, 0x00, 0, 0, 1];
        assert_eq!(find_start(&b, 0), Some(1));
        assert_eq!(find_start(&b, 2), Some(7));
        assert_eq!(find_start(&b, 8), None, "no code byte yet");
    }

    #[test]
    fn garbage_never_panics() {
        let mut d = Decoder::new();
        let mut seed = 1u32;
        let mut junk = vec![0u8; 200_000];
        for v in &mut junk {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (seed >> 24) as u8;
        }
        // Sprinkle start codes to reach every parser.
        for (i, code) in [0xB3u8, 0xB5, 0x00, 0x01, 0x05, 0xB8, 0x00, 0x10, 0xB7]
            .iter()
            .cycle()
            .take(400)
            .enumerate()
        {
            let at = i * 499 + 7;
            junk[at..at + 4].copy_from_slice(&[0, 0, 1, *code]);
        }
        for c in junk.chunks(1000) {
            d.push(c, None);
            while d.decode().is_some() {}
        }
        d.flush();
        while d.decode().is_some() {}
    }
}
