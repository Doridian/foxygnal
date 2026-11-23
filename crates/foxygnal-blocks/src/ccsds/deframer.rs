//! Soft symbols in, CCSDS frames out: the receive chain of a CCSDS link
//! below the transfer frame layer, as a plain struct ([`Deframer`]) that the
//! `ccsds_deframer` block wraps and that a demodulator can embed.
//!
//! 1. **Soft bits**: an AGC scales the symbols so a mean channel bit is ±40
//!    (as `i8`).
//! 2. **Search**: two frames' worth of symbols are collected and searched for
//!    the sync pattern under every phase symmetry (see [`super::sync`]). Not
//!    found: the oldest frame's worth is dropped and the search goes on.
//! 3. **Lock**: the symmetry is fixed and the buffer replayed, from 64 data
//!    bits before the first ASM, through the decoder chain: the symmetry,
//!    pairing of channel bits (coded links), the Viterbi decoder with G2
//!    un-inverted, NRZ-M decoding, then the frame grid.
//! 4. **Frames**: each grid slot of 32 + 8·`frame_len` bits is checked for
//!    the ASM (≤ 6 wrong bits; an inverted ASM flips the data polarity, which
//!    is how a 180° slip of the carrier loop shows on an uncoded link),
//!    derandomized, and corrected by Reed-Solomon. A missing ASM in a frame
//!    RS can't correct either is a miss; the grid is kept (a flywheel) until
//!    2 misses in a row (a 90° slip of the carrier loop, a fade), then the
//!    search starts over.

use foxygnal_core::Complex32;

use super::ASM;
use super::conv::ConvDecoder;
use super::randomizer;
use super::rs::{Codec, Corrections};
use super::sync::{BitBuf, Modulation, Pattern, Symmetry, search};

/// Mean |soft bit| the AGC aims for.
const SOFT_MEAN: f32 = 40.0;
/// Data bits fed to the decoder chain before the first ASM after a lock.
const MARGIN_BITS: usize = 64;
/// Wrong ASM bits a frame may have.
pub const ASM_MAX_ERRORS: u32 = 6;
/// Frames in a row without an ASM (and not corrected by RS) before the lock
/// is given up.
pub const MAX_MISSES: u32 = 2;

/// How the link is coded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub modulation: Modulation,
    /// Rate 1/2, K = 7 convolutional code.
    pub coded: bool,
    /// The code's G2 output is inverted (CCSDS).
    pub conv_invert: bool,
    /// NRZ-M (differential) coding of the data bits.
    pub nrzm: bool,
    /// The CCSDS pseudo-randomizer is applied after the ASM.
    pub randomized: bool,
    /// Reed-Solomon: interleaving depth and dual basis, or none.
    pub rs: Option<(usize, bool)>,
    /// Bytes after the ASM: the codeblock (transfer frame plus RS parity).
    pub frame_len: usize,
}

impl Default for Config {
    /// The usual CCSDS layout: QPSK, coded, randomized, RS I = 4 in the dual
    /// basis, 1020-byte codeblocks (1024-byte CADUs).
    fn default() -> Self {
        Self {
            modulation: Modulation::Qpsk,
            coded: true,
            conv_invert: true,
            nrzm: false,
            randomized: true,
            rs: Some((4, true)),
            frame_len: 1020,
        }
    }
}

impl Config {
    /// The Reed-Solomon codec this configuration needs (checking the
    /// lengths).
    pub fn codec(&self) -> Result<Option<Codec>, String> {
        if !(8..=16384).contains(&self.frame_len) {
            return Err(format!(
                "the frame length must be 8 to 16384 bytes (got {})",
                self.frame_len
            ));
        }
        self.rs
            .map(|(i, dual)| Codec::new(self.frame_len, i, dual))
            .transpose()
    }

    /// Channel bits per frame (ASM included).
    pub fn frame_channel_bits(&self) -> usize {
        (32 + 8 * self.frame_len) * if self.coded { 2 } else { 1 }
    }
}

/// A frame, as handed to the [`Deframer`]'s output.
#[derive(Debug)]
pub struct Frame<'a> {
    /// The codeblock after the ASM, derandomized and corrected (parity
    /// included).
    pub data: &'a [u8],
    /// What Reed-Solomon did (`None` without RS).
    pub rs: Option<Corrections>,
    /// Wrong bits in the ASM, or `None` if it wasn't found (a flywheel
    /// frame).
    pub asm_errors: Option<u32>,
}

impl Frame<'_> {
    /// Usable: RS corrected every codeword, or (without RS) the ASM was
    /// there.
    pub fn ok(&self) -> bool {
        match self.rs {
            Some(c) => c.ok(),
            None => self.asm_errors.is_some(),
        }
    }
}

/// Counters for the UI. They only grow; take differences for rates.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub locked: bool,
    /// The symmetry the lock was found at.
    pub symmetry: Symmetry,
    /// Frames out (good or not).
    pub frames: u64,
    /// Frames that weren't [`Frame::ok`].
    pub bad_frames: u64,
    /// Bytes RS corrected.
    pub rs_corrected: u64,
    /// RS codewords that couldn't be corrected.
    pub rs_failed: u64,
    /// Frames whose ASM wasn't found.
    pub asm_missed: u64,
    /// Locks found, and lost.
    pub locks: u64,
    pub unlocks: u64,
    /// Coded links: channel bits compared with the re-encoded Viterbi output,
    /// and how many differed (the channel BER the decoder corrected).
    pub viterbi_bits: u64,
    pub viterbi_errors: u64,
    /// ASM bits checked and how many were wrong (the decoded BER, or the
    /// channel BER of an uncoded link).
    pub asm_bits: u64,
    pub asm_errors: u64,
}

/// The frame grid in the decoded bit stream.
struct Framer {
    nrzm: bool,
    /// Decoded bits still to drop (the margin before the first ASM).
    skip: usize,
    prev: u8,
    /// XORed onto every data bit (a 180° slip on a link without NRZ-M).
    invert: u8,
    cadu: Vec<u8>,
    nbits: usize,
    misses: u32,
    /// The lock was given up; ignore bits until the next search.
    lost: bool,
    randomized: bool,
    codec: Option<Codec>,
}

impl Framer {
    fn start(&mut self) {
        self.skip = MARGIN_BITS;
        self.prev = 0;
        self.invert = 0;
        self.nbits = 0;
        self.misses = 0;
        self.lost = false;
        self.cadu.fill(0);
    }

    #[inline]
    fn push_bits(&mut self, bits: &[u8], stats: &mut Stats, out: &mut impl FnMut(Frame<'_>)) {
        for &b in bits {
            if self.lost {
                return;
            }
            let b = b & 1;
            if self.skip > 0 {
                self.skip -= 1;
                self.prev = b;
                continue;
            }
            let d = if self.nrzm {
                b ^ self.prev
            } else {
                b ^ self.invert
            };
            self.prev = b;
            if d != 0 {
                self.cadu[self.nbits / 8] |= 0x80 >> (self.nbits % 8);
            }
            self.nbits += 1;
            if self.nbits == self.cadu.len() * 8 {
                self.frame(stats, out);
                self.nbits = 0;
                self.cadu.fill(0);
            }
        }
    }

    fn frame(&mut self, stats: &mut Stats, out: &mut impl FnMut(Frame<'_>)) {
        let asm = u32::from_be_bytes([self.cadu[0], self.cadu[1], self.cadu[2], self.cadu[3]]);
        let e = (asm ^ ASM).count_ones();
        let asm_errors = if e <= ASM_MAX_ERRORS {
            Some(e)
        } else if !self.nrzm && 32 - e <= ASM_MAX_ERRORS {
            // Inverted: the carrier loop slipped by 180°.
            self.invert ^= 1;
            for b in self.cadu.iter_mut() {
                *b = !*b;
            }
            Some(32 - e)
        } else {
            None
        };
        match asm_errors {
            Some(e) => {
                stats.asm_bits += 32;
                stats.asm_errors += u64::from(e);
            }
            None => stats.asm_missed += 1,
        }
        let data = &mut self.cadu[4..];
        if self.randomized {
            randomizer::apply(data);
        }
        let rs = self.codec.as_ref().map(|c| c.decode(data));
        if let Some(c) = rs {
            stats.rs_corrected += c.bytes as u64;
            stats.rs_failed += c.failed as u64;
        }
        let f = Frame {
            data,
            rs,
            asm_errors,
        };
        // A frame RS corrected was on the grid, whatever its ASM looked like.
        if asm_errors.is_some() || f.ok() {
            self.misses = 0;
        } else {
            self.misses += 1;
            if self.misses >= MAX_MISSES {
                self.lost = true;
                return;
            }
        }
        stats.frames += 1;
        if !f.ok() {
            stats.bad_frames += 1;
        }
        out(f);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Search,
    Locked,
}

/// The receive chain; see the module docs.
pub struct Deframer {
    cfg: Config,
    pattern: Pattern,
    /// Channel bits of a frame.
    frame_bits: usize,
    level: f32,
    state: State,
    acq: Vec<[i8; 2]>,
    acq_need: usize,
    bits: BitBuf,
    symmetry: Symmetry,
    /// A G1 soft bit waiting for its G2 (coded links).
    half: Option<i8>,
    conv: ConvDecoder,
    framer: Framer,
    soft: Vec<[i8; 2]>,
    stats: Stats,
}

/// Symbols converted to soft bits at a time.
const CHUNK: usize = 1024;

impl Deframer {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let codec = cfg.codec()?;
        let pattern = Pattern::new(cfg.coded, cfg.conv_invert, cfg.nrzm);
        let frame_bits = cfg.frame_channel_bits();
        let bps = cfg.modulation.bits_per_symbol();
        let first = Self::margin_channel_bits(&cfg) + pattern.offset;
        let acq_need = (first + 2 * frame_bits + pattern.len).div_ceil(bps) + 1;
        Ok(Self {
            pattern,
            frame_bits,
            level: 0.0,
            state: State::Search,
            acq: Vec::with_capacity(acq_need + CHUNK),
            acq_need,
            bits: BitBuf::with_capacity(acq_need * bps + 64),
            symmetry: Symmetry::default(),
            half: None,
            conv: ConvDecoder::new(cfg.conv_invert),
            framer: Framer {
                nrzm: cfg.nrzm,
                skip: 0,
                prev: 0,
                invert: 0,
                cadu: vec![0; 4 + cfg.frame_len],
                nbits: 0,
                misses: 0,
                lost: false,
                randomized: cfg.randomized,
                codec,
            },
            soft: Vec::with_capacity(CHUNK),
            stats: Stats::default(),
            cfg,
        })
    }

    fn margin_channel_bits(cfg: &Config) -> usize {
        MARGIN_BITS * if cfg.coded { 2 } else { 1 }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn is_locked(&self) -> bool {
        self.state == State::Locked
    }

    /// Symbols (one per symbol, after timing and carrier recovery) in;
    /// frames out of `out` as they complete.
    pub fn push(&mut self, symbols: &[Complex32], out: &mut impl FnMut(Frame<'_>)) {
        for chunk in symbols.chunks(CHUNK) {
            self.soft.clear();
            let qpsk = self.cfg.modulation == Modulation::Qpsk;
            for &z in chunk {
                let a = if qpsk {
                    0.5 * (z.re.abs() + z.im.abs())
                } else {
                    z.re.abs()
                };
                if !a.is_finite() {
                    self.soft.push([0, 0]);
                    continue;
                }
                if self.level <= 0.0 {
                    self.level = a.max(1e-20);
                } else {
                    self.level += (a - self.level) * (1.0 / 512.0);
                }
                let k = SOFT_MEAN / self.level.max(1e-20);
                let q = |v: f32| (v * k).round().clamp(-127.0, 127.0) as i8;
                self.soft.push([q(z.re), q(z.im)]);
            }
            let soft = std::mem::take(&mut self.soft);
            self.push_soft(&soft, out);
            self.soft = soft;
        }
    }

    /// Soft symbols in (I, Q as `i8`, positive = 0, within ±127; BPSK uses
    /// only I), already scaled.
    pub fn push_soft(&mut self, soft: &[[i8; 2]], out: &mut impl FnMut(Frame<'_>)) {
        let mut rest = soft;
        while !rest.is_empty() {
            match self.state {
                State::Search => {
                    let room = (self.acq_need - self.acq.len()).min(rest.len());
                    self.acq.extend_from_slice(&rest[..room]);
                    rest = &rest[room..];
                    if self.acq.len() == self.acq_need {
                        self.try_lock(out);
                    }
                }
                State::Locked => {
                    let n = rest.len().min(CHUNK);
                    let (now, later) = rest.split_at(n);
                    self.decode(now, 0, out);
                    rest = later;
                }
            }
        }
    }

    fn try_lock(&mut self, out: &mut impl FnMut(Frame<'_>)) {
        let first = Self::margin_channel_bits(&self.cfg) + self.pattern.offset;
        let max_errors = if self.cfg.coded { 12 } else { 5 };
        let found = search(
            &self.acq,
            self.cfg.modulation,
            &self.pattern,
            first,
            self.frame_bits,
            max_errors,
            &mut self.bits,
        );
        let bps = self.cfg.modulation.bits_per_symbol();
        let Some(f) = found else {
            // Drop the oldest frame's worth and wait for more.
            let drop = self.frame_bits / bps;
            self.acq.copy_within(drop.., 0);
            self.acq.truncate(self.acq.len() - drop);
            return;
        };
        self.state = State::Locked;
        self.symmetry = f.symmetry;
        self.stats.locked = true;
        self.stats.symmetry = f.symmetry;
        self.stats.locks += 1;
        self.half = None;
        self.conv.reset();
        self.framer.start();
        let start = f.asm_bit - Self::margin_channel_bits(&self.cfg);
        let acq = std::mem::take(&mut self.acq);
        self.decode(&acq[start / bps..], start % bps, out);
        self.acq = acq;
        self.acq.clear();
    }

    /// Run soft symbols through the locked chain, skipping the first `skip`
    /// channel bits.
    fn decode(&mut self, soft: &[[i8; 2]], mut skip: usize, out: &mut impl FnMut(Frame<'_>)) {
        let bps = self.cfg.modulation.bits_per_symbol();
        let sym = self.symmetry;
        let stats = &mut self.stats;
        let framer = &mut self.framer;
        let mut emit = |bits: &[u8]| framer.push_bits(bits, stats, out);
        for &s in soft {
            let t = sym.apply(s);
            for &v in &t[..bps] {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if self.cfg.coded {
                    match self.half.take() {
                        None => self.half = Some(v),
                        Some(h) => self.conv.push([h, v], &mut emit),
                    }
                } else {
                    emit(&[u8::from(v < 0)]);
                }
            }
        }
        let (c, e) = self.conv.take_counts();
        self.stats.viterbi_bits += c;
        self.stats.viterbi_errors += e;
        if self.framer.lost {
            self.unlock();
        }
    }

    /// The end of the stream: release the bits the Viterbi decoder still
    /// holds (up to 384), which may complete a last frame.
    pub fn flush(&mut self, out: &mut impl FnMut(Frame<'_>)) {
        if self.state != State::Locked || !self.cfg.coded {
            return;
        }
        let stats = &mut self.stats;
        let framer = &mut self.framer;
        let mut emit = |bits: &[u8]| framer.push_bits(bits, stats, out);
        self.conv.flush(&mut emit);
        if self.framer.lost {
            self.unlock();
        }
    }

    fn unlock(&mut self) {
        self.state = State::Search;
        self.stats.locked = false;
        self.stats.unlocks += 1;
        self.acq.clear();
        self.conv.reset();
        self.half = None;
    }
}
