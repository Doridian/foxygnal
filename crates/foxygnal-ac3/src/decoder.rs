//! One AC-3 frame (A/52 section 5 and 7): the BSI, six audio blocks with
//! their exponents, bit allocation and mantissas, coupling, rematrixing and
//! dynamic range, then the inverse transform and the downmix to stereo.

// The loops below follow the standard's over channels, bins and bands.
#![allow(clippy::needless_range_loop)]

use crate::header::Header;
use crate::imdct::Imdct;
use crate::tables::{
    BAND_START, BAP, BIN_TO_BAND, CENTER_LEVELS, DB_PER_BIT, FAST_DECAY, FAST_GAIN, FLOOR,
    GAIN_LEVELS, HEARING, LOG_ADD, MANT_BITS, REMAT_BANDS, SLOW_DECAY, SLOW_GAIN, SURROUND_LEVELS,
};

/// Per-channel arrays: the full-bandwidth channels 0 to 4, then these.
const CPL: usize = 5;
const LFE: usize = 6;
const NCH: usize = 7;

/// Why a frame couldn't be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a valid header.
    Header,
    /// Fewer bytes than the header says.
    Truncated,
    /// E-AC-3, which this decoder doesn't do.
    Unsupported,
}

/// What [`Decoder::decode`] found.
#[derive(Debug, Clone, Copy)]
pub struct Decoded {
    pub header: Header,
    /// A CRC didn't match or the frame was malformed: the output is silence.
    pub error: bool,
}

/// A big-endian bit reader; past the end it reads zeros.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    /// The next `n` (0..=16) bits.
    #[inline]
    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let i = self.pos >> 3;
        let byte = |k: usize| u32::from(self.data.get(i + k).copied().unwrap_or(0));
        let w = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        let v = (w << (8 + (self.pos & 7))) >> (32 - n);
        self.pos += n as usize;
        v
    }

    #[inline]
    fn bit(&mut self) -> bool {
        self.read(1) != 0
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

/// CRC-16 (x¹⁶ + x¹⁵ + x² + 1, no reflection, starting from 0) of `data`.
pub(crate) fn crc16(data: &[u8]) -> u16 {
    const TABLE: [u16; 256] = {
        let mut t = [0u16; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = (i as u16) << 8;
            let mut k = 0;
            while k < 8 {
                c = if c & 0x8000 != 0 {
                    (c << 1) ^ 0x8005
                } else {
                    c << 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    data.iter().fold(0u16, |c, &b| {
        (c << 8) ^ TABLE[usize::from((c >> 8) as u8 ^ b)]
    })
}

/// Where CRC1's part of a frame ends: 5/8 of it, in whole words.
pub(crate) fn crc1_end(frame_len: usize) -> usize {
    ((frame_len >> 2) + (frame_len >> 4)) << 1
}

/// Delta bit allocation of one channel.
#[derive(Clone, Copy, Default)]
struct Dba {
    /// 0 reuse, 1 new, 2 none.
    mode: u8,
    nseg: usize,
    offset: [u8; 8],
    len: [u8; 8],
    ba: [u8; 8],
}

/// Grouped mantissas waiting to be used (bap 1, 2 and 4), shared by the
/// channels of a block.
#[derive(Default)]
struct Groups {
    b1: [f32; 2],
    n1: usize,
    b2: [f32; 2],
    n2: usize,
    b4: f32,
    n4: usize,
}

/// A symmetric quantiser's value: 2·(code − ⌊levels/2⌋)/levels.
#[inline]
fn level(code: u32, levels: u32) -> f32 {
    (2 * (code as i32 - (levels / 2) as i32)) as f32 / levels as f32
}

/// 2^e for small e.
fn pow2(e: i32) -> f32 {
    f32::from_bits(((127 + e) as u32) << 23)
}

/// Decodes whole frames to PCM. Keeps the transform's overlap between
/// frames, so feed it consecutive frames of one stream.
pub struct Decoder {
    imdct: Imdct,
    /// 2^−e, e = 0…24.
    scale: [f32; 25],
    /// The overlap of each full-bandwidth channel.
    delay: [[f32; 128]; 5],
    rng: u32,

    // State that blocks reuse from earlier blocks of the frame.
    blksw: [bool; 5],
    dithflag: [bool; 5],
    dynrng: [f32; 2],
    cplinu: bool,
    chincpl: [bool; 5],
    phsflginu: bool,
    /// The last bin + 1 of each coupling band.
    cpl_band_end: [usize; 18],
    ncplbnd: usize,
    cplco: [[f32; 18]; 5],
    phsflg: [bool; 18],
    nrematbnd: usize,
    rematflg: [bool; 4],
    start: [usize; NCH],
    end: [usize; NCH],
    exps: [[u8; 256]; NCH],
    sdecay: i32,
    fdecay: i32,
    sgain: i32,
    dbknee: i32,
    floor: i32,
    snroffst: [i32; NCH],
    fgain: [i32; NCH],
    cplfleak: i32,
    cplsleak: i32,
    dba: [Dba; NCH],
    bap: [[u8; 256]; NCH],
    coef: [[f32; 256]; NCH],
    /// One block of time samples per full-bandwidth channel.
    pcm: [[f32; 256]; 5],
    /// Where some fields of the last frame are (tests change them), and
    /// each block's bit allocation.
    #[cfg(test)]
    pub(crate) marks: Vec<(Mark, usize, usize)>,
    #[cfg(test)]
    pub(crate) baps: Vec<[[u8; 256]; NCH]>,
}

/// A field of the bit stream, for tests: (mark, block, bit position).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    Blksw(usize),
    Dithflag,
    Dynrnge,
    Phsflginu,
    /// Where phase flags for this many bands go (or are).
    Phsflg(usize),
    /// deltbaie, with the number of deltbae fields and the index of the
    /// first full-bandwidth channel among them.
    Deltbaie(usize, usize),
    /// The end of the audio blocks.
    End,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        let mut scale = [0f32; 25];
        for (e, s) in scale.iter_mut().enumerate() {
            *s = pow2(-(e as i32));
        }
        Decoder {
            imdct: Imdct::new(),
            scale,
            delay: [[0.0; 128]; 5],
            rng: 0x1234_5678,
            blksw: [false; 5],
            dithflag: [false; 5],
            dynrng: [1.0; 2],
            cplinu: false,
            chincpl: [false; 5],
            phsflginu: false,
            cpl_band_end: [0; 18],
            ncplbnd: 0,
            cplco: [[0.0; 18]; 5],
            phsflg: [false; 18],
            nrematbnd: 0,
            rematflg: [false; 4],
            start: [0; NCH],
            end: [0; NCH],
            exps: [[0; 256]; NCH],
            sdecay: 0,
            fdecay: 0,
            sgain: 0,
            dbknee: 0,
            floor: 0,
            snroffst: [0; NCH],
            fgain: [0; NCH],
            cplfleak: 0,
            cplsleak: 0,
            dba: [Dba::default(); NCH],
            bap: [[0; 256]; NCH],
            coef: [[0.0; 256]; NCH],
            pcm: [[0.0; 256]; 5],
            #[cfg(test)]
            marks: Vec::new(),
            #[cfg(test)]
            baps: Vec::new(),
        }
    }

    /// Forget the transform overlap (after a gap).
    pub fn reset(&mut self) {
        self.delay = [[0.0; 128]; 5];
    }

    /// Decode one whole frame (`frame` starts with its sync word) into
    /// `out`, interleaved, `samples() × channels()` values in ±1. `out` must
    /// hold at least 3072. A frame that fails its CRCs or doesn't parse
    /// comes out as silence.
    pub fn decode(&mut self, frame: &[u8], out: &mut [f32]) -> Result<Decoded, Error> {
        let h = Header::parse(frame).ok_or(Error::Header)?;
        if h.eac3() {
            return Err(Error::Unsupported);
        }
        if frame.len() < h.frame_len {
            return Err(Error::Truncated);
        }
        let frame = &frame[..h.frame_len];
        let n = h.samples() * h.channels();
        let f58 = crc1_end(h.frame_len);
        let ok = crc16(&frame[2..f58]) == 0
            && crc16(&frame[f58..]) == 0
            && self.frame(&h, frame, &mut out[..n]).is_ok();
        if !ok {
            out[..n].fill(0.0);
            self.reset();
        }
        Ok(Decoded {
            header: h,
            error: !ok,
        })
    }

    /// The BSI and the six blocks.
    fn frame(&mut self, h: &Header, frame: &[u8], out: &mut [f32]) -> Result<(), ()> {
        let acmod = h.acmod;
        let mut b = Bits {
            data: frame,
            pos: 40 + 5 + 3 + 3,
        };
        #[cfg(test)]
        {
            self.marks.clear();
            self.baps.clear();
        }
        // Mix levels, as indices into GAIN_LEVELS: −4.5 and −6 dB unless said.
        let mut cmix = 5usize;
        let mut smix = 6usize;
        if acmod & 1 != 0 && acmod != 1 {
            cmix = usize::from(CENTER_LEVELS[b.read(2) as usize]);
        }
        if acmod & 4 != 0 {
            smix = usize::from(SURROUND_LEVELS[b.read(2) as usize]);
        }
        if acmod == 2 {
            b.skip(2); // dsurmod
        }
        b.skip(1); // lfeon
        for _ in 0..if acmod == 0 { 2 } else { 1 } {
            b.skip(5); // dialnorm
            if b.bit() {
                b.skip(8); // compr: heavy compression isn't applied
            }
            if b.bit() {
                b.skip(8); // langcod
            }
            if b.bit() {
                b.skip(7); // mixlevel, roomtyp
            }
        }
        b.skip(2); // copyrightb, origbs
        if h.bsid == 6 {
            // The alternate syntax: xbsi1 has the Lo/Ro mix levels.
            if b.bit() {
                b.skip(2 + 3 + 3); // dmixmod, ltrtcmixlev, ltrtsurmixlev
                cmix = b.read(3) as usize;
                smix = (b.read(3) as usize).clamp(3, 7);
            }
            if b.bit() {
                b.skip(2 + 2 + 1 + 8 + 1);
            }
        } else {
            for _ in 0..2 {
                if b.bit() {
                    b.skip(14); // timecod
                }
            }
        }
        if b.bit() {
            let len = b.read(6) as usize + 1;
            b.skip(8 * len); // addbsi
        }

        let mix = downmix(acmod, GAIN_LEVELS[cmix], GAIN_LEVELS[smix]);
        let nfch = h.fbw_channels();
        let nout = h.channels();
        let shift = h.bsid.saturating_sub(8) as u32;
        for blk in 0..6 {
            self.block(h, &mut b, blk, shift)?;
            if b.pos > frame.len() * 8 {
                return Err(());
            }
            #[cfg(test)]
            if blk == 5 {
                self.marks.push((Mark::End, blk, b.pos));
            }
            for ch in 0..nfch {
                let gain = 2.0 * self.dynrng[if acmod == 0 { ch } else { 0 }];
                let mut x = [0f32; 256];
                for (x, &c) in x.iter_mut().zip(&self.coef[ch]) {
                    *x = c * gain;
                }
                self.imdct
                    .block(&x, self.blksw[ch], &mut self.delay[ch], &mut self.pcm[ch]);
            }
            let o = &mut out[blk * 256 * nout..(blk + 1) * 256 * nout];
            for (c, m) in mix.iter().enumerate().take(nout) {
                for i in 0..256 {
                    let mut v = 0.0;
                    for ch in 0..nfch {
                        v += m[ch] * self.pcm[ch][i];
                    }
                    o[i * nout + c] = v;
                }
            }
        }
        Ok(())
    }

    /// Parse one audio block and leave its coefficients in `coef`.
    fn block(&mut self, h: &Header, b: &mut Bits, blk: usize, shift: u32) -> Result<(), ()> {
        let acmod = h.acmod;
        let nfch = h.fbw_channels();
        let lfe = h.lfe;
        for ch in 0..nfch {
            #[cfg(test)]
            self.marks.push((Mark::Blksw(ch), blk, b.pos));
            self.blksw[ch] = b.bit();
        }
        for ch in 0..nfch {
            #[cfg(test)]
            self.marks.push((Mark::Dithflag, blk, b.pos));
            self.dithflag[ch] = b.bit();
        }
        for i in 0..if acmod == 0 { 2 } else { 1 } {
            #[cfg(test)]
            if i == 0 {
                self.marks.push((Mark::Dynrnge, blk, b.pos));
            }
            if b.bit() {
                self.dynrng[i] = dynrng(b.read(8));
            } else if blk == 0 {
                self.dynrng[i] = 1.0;
            }
        }

        // Coupling strategy.
        if b.bit() {
            self.cplinu = b.bit();
            if self.cplinu {
                if acmod < 2 {
                    return Err(());
                }
                for ch in 0..nfch {
                    self.chincpl[ch] = b.bit();
                }
                #[cfg(test)]
                if acmod == 2 {
                    self.marks.push((Mark::Phsflginu, blk, b.pos));
                }
                self.phsflginu = acmod == 2 && b.bit();
                let begf = b.read(4) as usize;
                let endf = b.read(4) as usize + 3;
                if begf >= endf {
                    return Err(());
                }
                self.start[CPL] = begf * 12 + 37;
                self.end[CPL] = endf * 12 + 37;
                let mut n = 0;
                self.cpl_band_end[0] = self.start[CPL] + 12;
                for _ in 1..endf - begf {
                    if b.bit() {
                        self.cpl_band_end[n] += 12;
                    } else {
                        n += 1;
                        self.cpl_band_end[n] = self.cpl_band_end[n - 1] + 12;
                    }
                }
                self.ncplbnd = n + 1;
            } else {
                self.chincpl = [false; 5];
                self.phsflginu = false;
            }
        } else if blk == 0 {
            return Err(());
        }
        let cplinu = self.cplinu;

        // Coupling coordinates.
        if cplinu {
            let mut any = false;
            for ch in 0..nfch {
                if !self.chincpl[ch] {
                    continue;
                }
                if b.bit() {
                    any = true;
                    let mstr = 3 * b.read(2);
                    for bnd in 0..self.ncplbnd {
                        let e = b.read(4);
                        let m = b.read(4) as i32;
                        // As 23-bit fixed point, like the reference decoder.
                        let v = if e == 15 { m << 22 } else { (m + 16) << 21 };
                        self.cplco[ch][bnd] = (v >> (e + mstr)) as f32 * pow2(-23);
                    }
                } else if blk == 0 {
                    return Err(());
                }
            }
            #[cfg(test)]
            if acmod == 2 && any {
                self.marks.push((Mark::Phsflg(self.ncplbnd), blk, b.pos));
            }
            if acmod == 2 && any {
                for bnd in 0..self.ncplbnd {
                    self.phsflg[bnd] = self.phsflginu && b.bit();
                }
            }
        }

        // Rematrixing.
        if acmod == 2 {
            if b.bit() {
                let mut n = 4;
                if cplinu && self.start[CPL] <= 61 {
                    n -= 1 + usize::from(self.start[CPL] == 37);
                }
                self.nrematbnd = n;
                for f in &mut self.rematflg[..n] {
                    *f = b.bit();
                }
            } else if blk == 0 {
                self.nrematbnd = 0;
            }
        }

        // Exponent strategies and bandwidths.
        let mut expstr = [0u32; NCH];
        if cplinu {
            expstr[CPL] = b.read(2);
        }
        for ch in 0..nfch {
            expstr[ch] = b.read(2);
        }
        if lfe {
            expstr[LFE] = b.read(1);
        }
        for ch in 0..nfch {
            self.start[ch] = 0;
            if expstr[ch] != 0 {
                if self.chincpl[ch] {
                    self.end[ch] = self.start[CPL];
                } else {
                    let c = b.read(6) as usize;
                    if c > 60 {
                        return Err(());
                    }
                    self.end[ch] = c * 3 + 73;
                }
            }
        }
        self.start[LFE] = 0;
        self.end[LFE] = 7;

        // Exponents.
        if cplinu && expstr[CPL] != 0 {
            let gs = 3 << (expstr[CPL] - 1);
            let ngrps = (self.end[CPL] - self.start[CPL]) / gs;
            let abs = b.read(4) << 1;
            exponents(
                b,
                expstr[CPL],
                ngrps,
                abs,
                &mut self.exps[CPL],
                self.start[CPL],
            )?;
        }
        for ch in 0..nfch {
            if expstr[ch] != 0 {
                let gs = 3 << (expstr[ch] - 1);
                let ngrps = (self.end[ch] + gs - 4) / gs;
                let abs = b.read(4);
                self.exps[ch][0] = abs as u8;
                exponents(b, expstr[ch], ngrps, abs, &mut self.exps[ch], 1)?;
                b.skip(2); // gainrng
            }
        }
        if lfe && expstr[LFE] != 0 {
            let abs = b.read(4);
            self.exps[LFE][0] = abs as u8;
            exponents(b, 1, 2, abs, &mut self.exps[LFE], 1)?;
        }

        // Bit allocation parameters.
        if b.bit() {
            self.sdecay = i32::from(SLOW_DECAY[b.read(2) as usize]) >> shift;
            self.fdecay = i32::from(FAST_DECAY[b.read(2) as usize]) >> shift;
            self.sgain = i32::from(SLOW_GAIN[b.read(2) as usize]);
            self.dbknee = i32::from(DB_PER_BIT[b.read(2) as usize]);
            self.floor = i32::from(FLOOR[b.read(3) as usize]);
        } else if blk == 0 {
            return Err(());
        }
        // The channels in use, in bit stream order.
        let mut used = [0usize; NCH];
        let mut nused = 0;
        if cplinu {
            used[nused] = CPL;
            nused += 1;
        }
        for ch in 0..nfch {
            used[nused] = ch;
            nused += 1;
        }
        if lfe {
            used[nused] = LFE;
            nused += 1;
        }
        let used = &used[..nused];
        if b.bit() {
            let csnr = (b.read(6) as i32 - 15) << 4;
            for &ch in used {
                self.snroffst[ch] = (csnr + b.read(4) as i32) << 2;
                self.fgain[ch] = i32::from(FAST_GAIN[b.read(3) as usize]);
            }
        } else if blk == 0 {
            return Err(());
        }
        if cplinu {
            if b.bit() {
                self.cplfleak = b.read(3) as i32;
                self.cplsleak = b.read(3) as i32;
            } else if blk == 0 {
                return Err(());
            }
        }
        // Delta bit allocation, for all but the LFE.
        let dba_chs = &used[..nused - usize::from(lfe)];
        #[cfg(test)]
        self.marks.push((
            Mark::Deltbaie(dba_chs.len(), usize::from(cplinu)),
            blk,
            b.pos,
        ));
        if b.bit() {
            for &ch in dba_chs {
                self.dba[ch].mode = b.read(2) as u8;
                if self.dba[ch].mode == 3 {
                    return Err(());
                }
            }
            for &ch in dba_chs {
                let d = &mut self.dba[ch];
                if d.mode == 1 {
                    d.nseg = b.read(3) as usize + 1;
                    for s in 0..d.nseg {
                        d.offset[s] = b.read(5) as u8;
                        d.len[s] = b.read(4) as u8;
                        d.ba[s] = b.read(3) as u8;
                    }
                }
            }
        } else if blk == 0 {
            for d in &mut self.dba {
                d.mode = 2;
            }
        }
        for &ch in used {
            self.bit_allocation(ch, h.fscod as usize, shift)?;
        }
        #[cfg(test)]
        self.baps.push(self.bap);

        // Skipped data.
        if b.bit() {
            let n = b.read(9) as usize;
            b.skip(8 * n);
        }

        // Mantissas; the coupling channel's follow the first coupled channel's.
        let mut g = Groups::default();
        let mut got_cpl = false;
        for ch in 0..nfch {
            self.mantissas(b, ch, self.dithflag[ch], &mut g);
            if self.chincpl[ch] && !got_cpl {
                self.mantissas(b, CPL, true, &mut g);
                got_cpl = true;
            }
        }
        if lfe {
            self.mantissas(b, LFE, false, &mut g);
        }

        // Decoupling: each coupled channel is the coupling channel times its
        // coordinates. Without dither, bins the coupling channel left at zero
        // bits stay zero.
        if cplinu {
            for ch in 0..nfch {
                if !self.chincpl[ch] {
                    continue;
                }
                let mut bin = self.start[CPL];
                for bnd in 0..self.ncplbnd {
                    let end = self.cpl_band_end[bnd];
                    let mut co = self.cplco[ch][bnd];
                    if acmod == 2 && ch == 1 && self.phsflg[bnd] {
                        co = -co;
                    }
                    for i in bin..end {
                        self.coef[ch][i] = if !self.dithflag[ch] && self.bap[CPL][i] == 0 {
                            0.0
                        } else {
                            self.coef[CPL][i] * co
                        };
                    }
                    bin = end;
                }
            }
        }
        for ch in 0..nfch {
            let end = if self.chincpl[ch] {
                self.end[CPL]
            } else {
                self.end[ch]
            };
            self.coef[ch][end..].fill(0.0);
        }

        // Rematrixing: the bands flagged carry L + R and L − R.
        if acmod == 2 {
            let end = self.end[0].min(self.end[1]);
            for bnd in 0..self.nrematbnd {
                if !self.rematflg[bnd] {
                    continue;
                }
                for i in REMAT_BANDS[bnd]..REMAT_BANDS[bnd + 1].min(end) {
                    let (l, r) = (self.coef[0][i], self.coef[1][i]);
                    self.coef[0][i] = l + r;
                    self.coef[1][i] = l - r;
                }
            }
        }
        Ok(())
    }

    /// Mantissas of one channel, dequantized with its exponents into `coef`.
    fn mantissas(&mut self, b: &mut Bits, ch: usize, dither: bool, g: &mut Groups) {
        for bin in self.start[ch]..self.end[ch] {
            let v = match self.bap[ch][bin] {
                0 => {
                    if dither {
                        // Uniform in ±0.707.
                        self.rng ^= self.rng << 13;
                        self.rng ^= self.rng >> 17;
                        self.rng ^= self.rng << 5;
                        ((self.rng >> 8) as f32 * pow2(-23) - 1.0) * 0.707
                    } else {
                        0.0
                    }
                }
                1 => {
                    if g.n1 > 0 {
                        g.n1 -= 1;
                        g.b1[g.n1]
                    } else {
                        let c = b.read(5);
                        g.b1 = [level(c % 3, 3), level((c % 9) / 3, 3)];
                        g.n1 = 2;
                        level(c / 9, 3)
                    }
                }
                2 => {
                    if g.n2 > 0 {
                        g.n2 -= 1;
                        g.b2[g.n2]
                    } else {
                        let c = b.read(7);
                        g.b2 = [level(c % 5, 5), level((c % 25) / 5, 5)];
                        g.n2 = 2;
                        level(c / 25, 5)
                    }
                }
                3 => match b.read(3) {
                    7 => 0.0,
                    c => level(c, 7),
                },
                4 => {
                    if g.n4 > 0 {
                        g.n4 = 0;
                        g.b4
                    } else {
                        let c = b.read(7);
                        g.b4 = level(c % 11, 11);
                        g.n4 = 1;
                        level(c / 11, 11)
                    }
                }
                5 => match b.read(4) {
                    15 => 0.0,
                    c => level(c, 15),
                },
                bap => {
                    let n = u32::from(MANT_BITS[usize::from(bap)]);
                    let v = ((b.read(n) << (32 - n)) as i32) >> (32 - n);
                    v as f32 * pow2(1 - n as i32)
                }
            };
            self.coef[ch][bin] = v * self.scale[usize::from(self.exps[ch][bin])];
        }
    }

    /// The parametric bit allocation (A/52 7.2): the exponents' power
    /// spectral density, banded; the masking curve; bit allocation pointers.
    fn bit_allocation(&mut self, ch: usize, fscod: usize, shift: u32) -> Result<(), ()> {
        let (start, end) = (self.start[ch], self.end[ch]);
        if start >= end {
            return Ok(());
        }
        let exps = &self.exps[ch];
        let mut psd = [0i32; 256];
        for bin in start..end {
            psd[bin] = 3072 - (i32::from(exps[bin]) << 7);
        }
        let mut bndpsd = [0i32; 51];
        let mut bin = start;
        let mut band = usize::from(BIN_TO_BAND[start]);
        loop {
            let band_end = usize::from(BAND_START[band + 1]).min(end);
            let mut v = psd[bin];
            bin += 1;
            while bin < band_end {
                let max = v.max(psd[bin]);
                let adr = (max - ((v + psd[bin] + 1) >> 1)).min(255);
                v = max + i32::from(LOG_ADD[adr as usize]);
                bin += 1;
            }
            bndpsd[band] = v;
            band += 1;
            if end <= usize::from(BAND_START[band]) {
                break;
            }
        }

        // Excitation.
        let bndstrt = usize::from(BIN_TO_BAND[start]);
        let bndend = usize::from(BIN_TO_BAND[end - 1]) + 1;
        let fgain = self.fgain[ch];
        let (sgain, sdecay, fdecay) = (self.sgain, self.sdecay, self.fdecay);
        let is_lfe = ch == LFE;
        let mut excite = [0i32; 50];
        let (mut fastleak, mut slowleak);
        let begin;
        if bndstrt == 0 {
            let mut lowcomp = lowcomp1(0, bndpsd[0], bndpsd[1], 384);
            excite[0] = bndpsd[0] - fgain - lowcomp;
            lowcomp = lowcomp1(lowcomp, bndpsd[1], bndpsd[2], 384);
            excite[1] = bndpsd[1] - fgain - lowcomp;
            let mut from = 7;
            fastleak = 0;
            slowleak = 0;
            for band in 2..7 {
                let skip = is_lfe && band == 6;
                if !skip {
                    lowcomp = lowcomp1(lowcomp, bndpsd[band], bndpsd[band + 1], 384);
                }
                fastleak = bndpsd[band] - fgain;
                slowleak = bndpsd[band] - sgain;
                excite[band] = fastleak - lowcomp;
                if !skip && bndpsd[band] <= bndpsd[band + 1] {
                    from = band + 1;
                    break;
                }
            }
            for band in from..bndend.min(22) {
                if !(is_lfe && band == 6) {
                    lowcomp = if band < 7 {
                        lowcomp1(lowcomp, bndpsd[band], bndpsd[band + 1], 384)
                    } else if band < 20 {
                        lowcomp1(lowcomp, bndpsd[band], bndpsd[band + 1], 320)
                    } else {
                        (lowcomp - 128).max(0)
                    };
                }
                fastleak = (fastleak - fdecay).max(bndpsd[band] - fgain);
                slowleak = (slowleak - sdecay).max(bndpsd[band] - sgain);
                excite[band] = (fastleak - lowcomp).max(slowleak);
            }
            begin = 22;
        } else {
            begin = bndstrt;
            fastleak = (self.cplfleak << 8) + 768;
            slowleak = (self.cplsleak << 8) + 768;
        }
        for band in begin..bndend {
            fastleak = (fastleak - fdecay).max(bndpsd[band] - fgain);
            slowleak = (slowleak - sdecay).max(bndpsd[band] - sgain);
            excite[band] = fastleak.max(slowleak);
        }

        // Masking curve.
        let mut mask = [0i32; 51];
        for band in bndstrt..bndend {
            let t = self.dbknee - bndpsd[band];
            if t > 0 {
                excite[band] += t >> 2;
            }
            mask[band] = i32::from(HEARING[band >> shift][fscod]).max(excite[band]);
        }
        let d = &self.dba[ch];
        if d.mode < 2 {
            let mut band = bndstrt;
            for s in 0..d.nseg {
                band += usize::from(d.offset[s]);
                let len = usize::from(d.len[s]);
                if band >= 50 || len > 50 - band {
                    return Err(());
                }
                let ba = i32::from(d.ba[s]);
                let delta = if ba >= 4 {
                    (ba - 3) * 128
                } else {
                    (ba - 4) * 128
                };
                for m in &mut mask[band..band + len] {
                    *m += delta;
                }
                band += len;
            }
        }

        // Bit allocation pointers.
        let bap = &mut self.bap[ch];
        let snr = self.snroffst[ch];
        if snr == -960 {
            bap.fill(0);
            return Ok(());
        }
        let floor = self.floor;
        let mut bin = start;
        let mut band = bndstrt;
        while bin < end {
            let m = ((mask[band] - snr - floor).max(0) & 0x1FE0) + floor;
            let band_end = usize::from(BAND_START[band + 1]).min(end);
            while bin < band_end {
                let adr = ((psd[bin] - m) >> 5).clamp(0, 63);
                bap[bin] = BAP[adr as usize];
                bin += 1;
            }
            band += 1;
        }
        Ok(())
    }
}

fn lowcomp1(a: i32, b0: i32, b1: i32, c: i32) -> i32 {
    if b0 + 256 == b1 {
        c
    } else if b0 > b1 {
        (a - 64).max(0)
    } else {
        a
    }
}

/// Exponents from their strategy (D15, D25, D45: one value per 1, 2 or 4
/// bins) and groups of three 5-level differences in 7 bits, from `abs`,
/// written from `exps[from]`.
fn exponents(
    b: &mut Bits,
    strategy: u32,
    ngrps: usize,
    abs: u32,
    exps: &mut [u8; 256],
    from: usize,
) -> Result<(), ()> {
    let size = [0, 1, 2, 4][strategy as usize];
    let mut prev = abs as i32;
    let mut j = from;
    for _ in 0..ngrps {
        let g = b.read(7);
        if g >= 125 {
            return Err(());
        }
        for d in [g / 25, (g % 25) / 5, g % 5] {
            prev += d as i32 - 2;
            if !(0..=24).contains(&prev) {
                return Err(());
            }
            let dst = exps.get_mut(j..j + size).ok_or(())?;
            dst.fill(prev as u8);
            j += size;
        }
    }
    Ok(())
}

/// A/52 7.7.1: dynrng's gain, 2^X · (1 + Y/32)/… for X the signed top three
/// bits and Y the low five, so 0 is unity: 2^(X−5) · (32 + Y).
fn dynrng(code: u32) -> f32 {
    let x = (code >> 5) as i32 - if code >= 128 { 8 } else { 0 };
    (32 + (code & 31)) as f32 * pow2(x - 5)
}

/// The Lo/Ro downmix (A/52 7.8.2) of each mode's channels to left and
/// right, the LFE left out: centre at `cmix`, surround at `smix` (a single
/// surround to both at −3 dB more), each side scaled so its gains add up to
/// one and it can't clip. Two channels or one pass as they are.
fn downmix(acmod: u8, cmix: f32, smix: f32) -> [[f32; 5]; 2] {
    let h = core::f32::consts::FRAC_1_SQRT_2;
    let mut m = [[0f32; 5]; 2];
    match acmod {
        0 | 2 => {
            m[0][0] = 1.0;
            m[1][1] = 1.0;
            return m;
        }
        1 => {
            m[0][0] = 1.0;
            return m;
        }
        3 => {
            m[0] = [1.0, cmix, 0.0, 0.0, 0.0];
            m[1] = [0.0, cmix, 1.0, 0.0, 0.0];
        }
        4 => {
            m[0] = [1.0, 0.0, smix * h, 0.0, 0.0];
            m[1] = [0.0, 1.0, smix * h, 0.0, 0.0];
        }
        5 => {
            m[0] = [1.0, cmix, 0.0, smix * h, 0.0];
            m[1] = [0.0, cmix, 1.0, smix * h, 0.0];
        }
        6 => {
            m[0] = [1.0, 0.0, smix, 0.0, 0.0];
            m[1] = [0.0, 1.0, 0.0, smix, 0.0];
        }
        _ => {
            m[0] = [1.0, cmix, 0.0, smix, 0.0];
            m[1] = [0.0, cmix, 1.0, 0.0, smix];
        }
    }
    for side in &mut m {
        let sum: f32 = side.iter().sum();
        for v in side.iter_mut() {
            *v /= sum;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_pieces() {
        assert_eq!(dynrng(0), 1.0);
        assert_eq!(dynrng(0x20), 2.0);
        assert_eq!(dynrng(0xE0), 0.5);
        assert!((dynrng(0x1F) - 63.0 / 32.0).abs() < 1e-6);
        assert_eq!(level(0, 3), -2.0 / 3.0);
        assert_eq!(level(6, 7), 6.0 / 7.0);
        // A known CRC: the reference CRC-16/BUYPASS ("123456789" → 0xFEE8).
        assert_eq!(crc16(b"123456789"), 0xFEE8);
        let m = downmix(7, GAIN_LEVELS[4], GAIN_LEVELS[4]);
        let s = 1.0 + 2.0 * core::f32::consts::FRAC_1_SQRT_2;
        assert!((m[0][0] - 1.0 / s).abs() < 1e-6 && (m[1][4] - m[0][1]).abs() < 1e-6);
    }
}
