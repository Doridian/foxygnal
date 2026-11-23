//! FLEX pager decoder (Motorola FLEX, ARIB STD-43): FM-discriminator audio
//! in, text out.
//!
//! FLEX sends a frame every 1.875 s, 128 frames per four-minute cycle. Each
//! frame starts at 1600 bit/s 2-FSK (±4.8 kHz) with **Sync 1**: 32 bits of
//! dotting, an A word naming the frame's speed, 16 bits of dotting and the A
//! word inverted. The **frame information word** (cycle and frame number,
//! 4-bit checksum) follows, then **Sync 2** (25 ms at the frame's speed),
//! then 11 blocks of 160 ms. A block carries 8 words per *phase*, the 32 bits
//! of its words interleaved (bit 0 of words 0–7, then bit 1, …):
//!
//! | speed     | symbols                        | phases     |
//! |-----------|--------------------------------|------------|
//! | 1600 bps  | 1600 Bd, 2 levels              | A          |
//! | 3200 bps  | 1600 Bd, 4 levels              | A, B       |
//! | 3200 bps  | 3200 Bd, 2 levels              | A, C       |
//! | 6400 bps  | 3200 Bd, 4 levels              | A, B, C, D |
//!
//! The 4 levels are ±4.8 and ±1.6 kHz: a symbol's sign is the bit of phase
//! A (C) and "inner level" the bit of phase B (D); at 3200 Bd symbols
//! alternate between A/B and C/D. Each phase is 88 words: a block information word (where the address
//! and vector fields start), addresses (one word, or two for a long address),
//! one vector word per address word (message type, start and length), then
//! the messages. Words are BCH(31,21) plus even parity, sent least
//! significant data bit first; on the air they look exactly like POCSAG
//! codewords, so the same repair table serves both.
//!
//! The demodulator runs one symbol clock at 3200 Bd (a matched filter one
//! 3200 Bd symbol long and a PLL on the zero crossings); a 1600 Bd symbol is
//! the mean of two of its decisions, with Sync 1 searched for at both
//! pairings and both polarities. From Sync 1 on, the slicer's centre and
//! outer level are refined by a decision-directed LMS loop, which also sets
//! the 4-level thresholds.

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::pocsag::{correct, error_table};
use super::text::{BitClock, Escaped};
use super::{MsgQueue, idle_status, text_meta};
use crate::design::fmt_rate;
use crate::filters::not_configured;

/// The symbol clock's rate: the fastest FLEX symbol rate.
const CLOCK_BAUD: f64 = 3200.0;
/// Words per phase in a frame.
const WORDS: usize = 88;
/// Clock decisions (at 3200 Bd) from the end of the sync pattern to the end
/// of the frame information word: 16 bits of dotting and 32 FIW bits, at
/// 1600 Bd.
const FIW_END: u32 = 2 * 48;
/// … to the end of Sync 2 (25 ms).
const SYNC2_END: u32 = FIW_END + 80;
/// … to the end of the frame (11 blocks of 160 ms).
const FRAME_END: u32 = SYNC2_END + 5632;
/// Bits 16–47 of the sync pattern (`B`: dotting, and the end of the A
/// word), as received with 1 = the higher frequency.
const SYNC_MARKER: u32 = 0x5939_5555;
/// First 16 bits of the A word for each speed: (A, baud, levels).
const MODES: [(u16, u32, u32); 5] = [
    (0x78F3, 1600, 2),
    (0x4F97, 1600, 4),
    (0x84E7, 3200, 2),
    (0x215F, 3200, 4),
    // A second code multimon-ng accepts for 6400 bps.
    (0xB383, 3200, 4),
];
/// Bit errors allowed in the 32-bit marker and in the two 16-bit A halves.
const SYNC_MARKER_ERRORS: u32 = 3;
const SYNC_CODE_ERRORS: u32 = 4;
/// Address and BIW values that mean "nothing here".
const IDLE0: u32 = 0;
const IDLE1: u32 = 0x1F_FFFF;
/// `fixed` value of a word BCH couldn't repair.
const BAD: u8 = u8::MAX;
/// Envelope tracker gains per clock decision: rise within a symbol, decay
/// over ~200 decisions.
const ATTACK: f32 = 0.5;
const DECAY: f32 = 0.005;
/// LMS gain of the slicer's centre and level during a frame.
const LMS: f32 = 0.02;
/// Bit timing PLL gain per zero crossing: fast while hunting (Sync 1 has 32
/// bits of dotting to lock on), slow within a frame, where 4-level crossings
/// between unequal levels are a quarter symbol early or late.
const HUNT_GAIN: f64 = 0.12;
const FRAME_GAIN: f64 = 0.04;
/// Numeric digits: 0–9, spare, U (urgent), space, hyphen, `]`, `[`.
const NUMERIC: &[u8; 16] = b"0123456789.U -][";

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "flex_decoder",
            label: "FLEX Decoder",
            category: "Digital",
            help: "Decodes Motorola FLEX pager messages from an FM demodulator's output \
                   (de-emphasis off; the level doesn't matter) at 12.8 kS/s or more (e.g. 48 \
                   kS/s). 1600 bps 2-FSK, 3200 bps 2- or 4-FSK and 6400 bps 4-FSK, told apart by \
                   the sync pattern; either polarity; phases A–D; BCH(31,21) error correction; \
                   short and long addresses. One text line per message: `t=12.345 baud=1600 \
                   levels=2 phase=A cycle=3 frame=45 capcode=001234567 type=alpha msg=\"…\"` \
                   (`type=numeric`, `tone`, `secure`, `binary` with `hex=`, or `instruction`; \
                   `part=first|more|last` on a fragment of a longer message). Pager messages \
                   can be personal: decode only what you may lawfully receive.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![ParamSpec {
                name: "fix_errors",
                label: "Fix bit errors",
                kind: ParamKind::Int {
                    min: Some(0),
                    max: Some(2),
                },
                default: json!(2),
                help: "Wrong bits repaired per word (0–2, the parity bit counts). More decodes \
                       weaker signals, at a higher risk of a wrong repair.",
                live: true,
            }],
        },
        build,
    );
}

// ---- Words ------------------------------------------------------------------------

/// The 4-bit checksum of an information word (frame, block, vector): the
/// data bits' nibbles (and bit 20) add up to 0xF.
fn checksum_ok(w: u32) -> bool {
    let sum = (0..5).map(|i| (w >> (4 * i)) & 0xF).sum::<u32>() + ((w >> 20) & 1);
    sum & 0xF == 0xF
}

/// Which speed a 64-bit sync register holds (1 = higher frequency, newest
/// bit last): the A word's first half, the marker, the inverted A word's
/// first half.
fn sync_mode(r: u64) -> Option<usize> {
    if ((r >> 16) as u32 ^ SYNC_MARKER).count_ones() > SYNC_MARKER_ERRORS {
        return None;
    }
    let (a, abar) = ((r >> 48) as u16, r as u16);
    MODES.iter().position(|&(code, _, _)| {
        (a ^ code).count_ones() + (abar ^ !code).count_ones() <= SYNC_CODE_ERRORS
    })
}

/// Long address from its two words (ARIB STD-43's conversion).
fn long_capcode(a1: u32, a2: u32) -> u64 {
    u64::from(a1) + (u64::from(!a2 & 0x1F_FFFF) << 15) + 0x1F_9000
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Hunt,
    Frame,
}

/// One phase's words after repair: data (bits 0–20) and bits fixed, or
/// [`BAD`] (then the data are the bits as received).
struct Phase {
    raw: [u32; WORDS],
    data: [u32; WORDS],
    fixed: [u8; WORDS],
}

/// What a page needs to be printed.
#[derive(Clone, Copy)]
struct Page {
    phase: usize,
    capcode: u64,
    /// Vector word index.
    vec: usize,
    long: bool,
    fixed: u32,
    bad: u32,
}

pub struct FlexDecoder {
    rate: f64,
    fix: u32,
    table: Box<[u32; 1024]>,
    // Front end.
    ring: Vec<f32>,
    pos: usize,
    sum: f64,
    scale: f32,
    clock: Option<BitClock>,
    hi: f32,
    lo: f32,
    /// The previous clock decision, and which pairing it closed.
    prev: f32,
    parity: usize,
    /// Sync registers for the two pairings of 3200 Bd decisions.
    hunt: [u64; 2],
    // The frame being received.
    state: State,
    /// Clock decisions since the end of Sync 1, and the pairing of its bits.
    n: u32,
    pairing: usize,
    /// Polarity (±1), slicer centre and outer level.
    pol: f32,
    centre: f32,
    level: f32,
    mode: usize,
    fiw: u32,
    cycle: u32,
    frame: u32,
    /// Input sample at the end of Sync 1.
    t: u64,
    phases: Box<[Phase; 4]>,
    // Output.
    /// Pages of the last frame still to print: next phase and address word.
    pending: bool,
    cur_phase: usize,
    cur_addr: usize,
    queue: MsgQueue,
    text: Vec<u8>,
    samples: u64,
}

impl FlexDecoder {
    pub fn new(fix: u32) -> Self {
        let phase = || Phase {
            raw: [0; WORDS],
            data: [0; WORDS],
            fixed: [0; WORDS],
        };
        Self {
            rate: 0.0,
            fix,
            table: error_table(),
            ring: Vec::new(),
            pos: 0,
            sum: 0.0,
            scale: 1.0,
            clock: None,
            hi: 0.0,
            lo: 0.0,
            prev: 0.0,
            parity: 0,
            hunt: [0; 2],
            state: State::Hunt,
            n: 0,
            pairing: 0,
            pol: 1.0,
            centre: 0.0,
            level: 1.0,
            mode: 0,
            fiw: 0,
            cycle: 0,
            frame: 0,
            t: 0,
            phases: Box::new([phase(), phase(), phase(), phase()]),
            pending: false,
            cur_phase: 0,
            cur_addr: 0,
            queue: MsgQueue::new(),
            text: Vec::with_capacity(4 * WORDS * 3),
            samples: 0,
        }
    }

    fn baud(&self) -> u32 {
        MODES[self.mode].1
    }

    fn levels(&self) -> u32 {
        MODES[self.mode].2
    }

    /// Phases this frame's speed carries.
    fn has_phase(&self, p: usize) -> bool {
        match (self.baud(), self.levels()) {
            (1600, 2) => p == 0,
            (1600, _) => p <= 1,
            (_, 2) => p == 0 || p == 2,
            _ => true,
        }
    }

    // ---- Demodulator ----

    #[inline]
    fn sample(&mut self, x: f32) {
        let old = std::mem::replace(&mut self.ring[self.pos], x);
        self.pos += 1;
        if self.pos == self.ring.len() {
            self.pos = 0;
        }
        self.sum += f64::from(x) - f64::from(old);
        let y = self.sum as f32 * self.scale;
        let th = 0.5 * (self.hi + self.lo);
        let Some(clock) = self.clock.as_mut() else {
            return;
        };
        if let Some(v) = clock.sample(y - th) {
            self.decision(v + th);
        }
    }

    /// One 3200 Bd decision (the matched filter's output at the symbol end).
    fn decision(&mut self, d: f32) {
        let a = if d > self.hi { ATTACK } else { DECAY };
        self.hi += a * (d - self.hi);
        let a = if d < self.lo { ATTACK } else { DECAY };
        self.lo += a * (d - self.lo);
        let z = 0.5 * (self.prev + d);
        self.prev = d;
        self.parity ^= 1;
        match self.state {
            State::Hunt => self.hunt(z),
            State::Frame => self.frame_decision(d, z),
        }
    }

    fn set_state(&mut self, state: State) {
        self.state = state;
        if let Some(c) = self.clock.as_mut() {
            c.set_gain(if state == State::Hunt {
                HUNT_GAIN
            } else {
                FRAME_GAIN
            });
        }
    }

    fn hunt(&mut self, z: f32) {
        let p = self.parity;
        let r = (self.hunt[p] << 1) | u64::from(z > 0.5 * (self.hi + self.lo));
        self.hunt[p] = r;
        let (mode, pol) = if let Some(m) = sync_mode(r) {
            (m, 1.0)
        } else if let Some(m) = sync_mode(!r) {
            (m, -1.0)
        } else {
            return;
        };
        self.set_state(State::Frame);
        self.mode = mode;
        self.pol = pol;
        self.pairing = p;
        self.n = 0;
        self.fiw = 0;
        self.t = self.samples;
        self.centre = 0.5 * (self.hi + self.lo);
        self.level = (0.5 * (self.hi - self.lo)).max(1e-9);
        for ph in self.phases.iter_mut() {
            ph.raw = [0; WORDS];
        }
    }

    /// Slice `y` (1 or 3 levels per side) and adapt the slicer to it.
    /// Returns the sign bit and the inner-level bit.
    fn slice(&mut self, y: f32, levels: u32) -> (bool, bool) {
        let u = self.pol * (y - self.centre) / self.level;
        let inner = levels == 4 && u.abs() < 2.0 / 3.0;
        let s = match (u > 0.0, inner) {
            (true, false) => 1.0,
            (true, true) => 1.0 / 3.0,
            (false, true) => -1.0 / 3.0,
            (false, false) => -1.0,
        };
        let e = y - self.centre - self.pol * s * self.level;
        self.centre += LMS * e;
        self.level = (self.level + LMS * e * self.pol * s).max(1e-9);
        (u > 0.0, inner)
    }

    fn frame_decision(&mut self, d: f32, z: f32) {
        self.n += 1;
        let n = self.n;
        if n <= FIW_END {
            if n.is_multiple_of(2) {
                let (bit, _) = self.slice(z, 2);
                let i = n / 2 - 1;
                if i >= 16 {
                    self.fiw |= u32::from(bit) << (i - 16);
                }
            }
            if n == FIW_END && !self.frame_info() {
                self.set_state(State::Hunt);
            }
        } else if n > SYNC2_END {
            let m = n - SYNC2_END - 1;
            let levels = self.levels();
            if self.baud() == 1600 {
                if m % 2 == 1 {
                    let bits = self.slice(z, levels);
                    self.store(m / 2, 0, bits);
                }
            } else {
                let bits = self.slice(d, levels);
                self.store(m / 2, 2 * (m % 2) as usize, bits);
            }
            if n == FRAME_END {
                self.finish_frame();
            }
        }
    }

    /// Check and read the frame information word.
    fn frame_info(&mut self) -> bool {
        match correct(&self.table, self.fix, self.fiw.reverse_bits()) {
            Some((w, _)) => {
                let w = w.reverse_bits() & 0x1F_FFFF;
                if !checksum_ok(w) {
                    return false;
                }
                self.cycle = (w >> 4) & 0xF;
                self.frame = (w >> 8) & 0x7F;
                true
            }
            None => false,
        }
    }

    /// Bit `k` of the phase (or phases, with 4 levels) starting at `first`.
    fn store(&mut self, k: u32, first: usize, (a, b): (bool, bool)) {
        let k = k as usize;
        let word = (k / 256) * 8 + k % 8;
        let bit = (k % 256) / 8;
        self.phases[first].raw[word] |= u32::from(a) << bit;
        if self.levels() == 4 {
            self.phases[first + 1].raw[word] |= u32::from(b) << bit;
        }
    }

    fn finish_frame(&mut self) {
        self.set_state(State::Hunt);
        self.hunt = [0; 2];
        for p in 0..4 {
            if !self.has_phase(p) {
                continue;
            }
            let ph = &mut self.phases[p];
            for i in 0..WORDS {
                // On the air the first bit is the codeword's top bit.
                match correct(&self.table, self.fix, ph.raw[i].reverse_bits()) {
                    Some((w, f)) => {
                        ph.data[i] = w.reverse_bits() & 0x1F_FFFF;
                        ph.fixed[i] = f as u8;
                    }
                    None => {
                        ph.data[i] = ph.raw[i] & 0x1F_FFFF;
                        ph.fixed[i] = BAD;
                    }
                }
            }
        }
        self.pending = true;
        self.cur_phase = 0;
        self.cur_addr = 0;
    }

    // ---- Pages ----

    /// Print the next page of the last frame, if any is left.
    fn emit_next(&mut self) {
        while self.cur_phase < 4 {
            let p = self.cur_phase;
            if let Some((aoff, voff)) = self.fields(p) {
                if self.cur_addr < aoff {
                    self.cur_addr = aoff;
                }
                while self.cur_addr < voff {
                    let i = self.cur_addr;
                    let page = self.page(p, i, aoff, voff);
                    self.cur_addr += if page.as_ref().is_some_and(|pg| pg.long) {
                        2
                    } else {
                        1
                    };
                    if let Some(page) = page {
                        self.print(page);
                        return;
                    }
                }
            }
            self.cur_phase += 1;
            self.cur_addr = 0;
        }
        self.pending = false;
    }

    /// An information word (BIW, vector) that BCH repaired must also pass its
    /// 4-bit checksum: two wrong bits plus noise can look like a different
    /// codeword. Words received intact aren't held to it, so a transmitter
    /// that fills it differently still decodes.
    fn plausible(&self, p: usize, i: usize) -> bool {
        let ph = &self.phases[p];
        ph.fixed[i] == 0 || checksum_ok(ph.data[i])
    }

    /// The address and vector fields' starts, from the block information word.
    fn fields(&self, p: usize) -> Option<(usize, usize)> {
        if !self.has_phase(p) {
            return None;
        }
        let ph = &self.phases[p];
        let biw = ph.data[0];
        if ph.fixed[0] == BAD || biw == IDLE0 || biw == IDLE1 || !self.plausible(p, 0) {
            return None;
        }
        let aoff = ((biw >> 8) & 3) as usize + 1;
        let voff = ((biw >> 10) & 0x3F) as usize;
        (voff > aoff && voff + (voff - aoff) <= WORDS).then_some((aoff, voff))
    }

    fn page(&self, p: usize, i: usize, aoff: usize, voff: usize) -> Option<Page> {
        let ph = &self.phases[p];
        let a1 = ph.data[i];
        if ph.fixed[i] == BAD || a1 == IDLE0 || a1 == IDLE1 {
            return None;
        }
        let vec = voff + (i - aoff);
        if ph.fixed[vec] == BAD || !self.plausible(p, vec) {
            return None;
        }
        let mut fixed = u32::from(ph.fixed[i]) + u32::from(ph.fixed[vec]);
        let long = a1 <= 0x8000;
        let capcode = if long {
            // The second word follows, with a second vector word.
            if i + 1 >= voff || ph.fixed[i + 1] == BAD {
                return None;
            }
            fixed += u32::from(ph.fixed[i + 1]);
            long_capcode(a1, ph.data[i + 1])
        } else {
            u64::from(a1 - 0x8000)
        };
        Some(Page {
            phase: p,
            capcode,
            vec,
            long,
            fixed,
            bad: 0,
        })
    }

    /// Word `k` of a message: for a long address the vector field's second
    /// word stands in for the message's first word.
    fn msg_word(&self, page: &mut Page, start: usize, k: usize) -> Option<u32> {
        let ph = &self.phases[page.phase];
        let i = match (page.long, k) {
            (true, 0) => page.vec + 1,
            (true, k) => start + k - 1,
            (false, k) => start + k,
        };
        if i >= WORDS {
            return None;
        }
        match ph.fixed[i] {
            BAD => page.bad += 1,
            f => page.fixed += u32::from(f),
        }
        Some(ph.data[i])
    }

    fn print(&mut self, mut page: Page) {
        let v = self.phases[page.phase].data[page.vec];
        let kind = (v >> 4) & 7;
        let t = self.t as f64 / self.rate;
        self.queue.text(format_args!(
            "t={t:.3} baud={} levels={} phase={} cycle={} frame={} capcode={:09}",
            self.baud(),
            self.levels(),
            char::from(b'A' + page.phase as u8),
            self.cycle,
            self.frame,
            page.capcode
        ));
        let start = ((v >> 7) & 0x7F) as usize;
        match kind {
            // Numeric: standard, special, numbered (which has a message
            // number before the digits).
            3 | 4 | 7 => {
                let words = ((v >> 14) & 7) as usize + 1;
                let skip = if kind == 7 { 10 } else { 2 };
                self.text.clear();
                let (mut acc, mut n, mut seen) = (0u32, 0, 0);
                for k in 0..words {
                    let Some(w) = self.msg_word(&mut page, start, k) else {
                        break;
                    };
                    for b in 0..21 {
                        seen += 1;
                        if seen <= skip {
                            continue;
                        }
                        acc |= ((w >> b) & 1) << n;
                        n += 1;
                        if n == 4 {
                            self.text.push(NUMERIC[acc as usize]);
                            (acc, n) = (0, 0);
                        }
                    }
                }
                trim_spaces(&mut self.text);
                self.queue.text(format_args!(
                    " type=numeric msg=\"{}\"",
                    Escaped(&self.text)
                ));
            }
            // Tone only, possibly with a few digits in the vector itself.
            2 => {
                self.queue.text(format_args!(" type=tone"));
                if (v >> 7) & 3 == 0 {
                    self.text.clear();
                    for b in [9, 13, 17] {
                        self.text.push(NUMERIC[((v >> b) & 0xF) as usize]);
                    }
                    if page.long {
                        let w = self.msg_word(&mut page, 0, 0).unwrap_or(0);
                        for b in [0, 4, 8, 12, 16] {
                            self.text.push(NUMERIC[((w >> b) & 0xF) as usize]);
                        }
                    }
                    trim_spaces(&mut self.text);
                    if !self.text.is_empty() {
                        self.queue
                            .text(format_args!(" msg=\"{}\"", Escaped(&self.text)));
                    }
                }
            }
            // Alphanumeric, secure and binary: a header word (fragment
            // number, continuation flag), then the content.
            0 | 5 | 6 => {
                let len = ((v >> 14) & 0x7F) as usize;
                let header = if len > 0 {
                    self.msg_word(&mut page, start, 0)
                } else {
                    None
                };
                let frag = header.map_or(3, |h| (h >> 11) & 3);
                let more = header.is_some_and(|h| (h >> 10) & 1 == 1);
                self.text.clear();
                if kind == 6 {
                    self.queue.text(format_args!(" type=binary hex=\""));
                    for k in 1..len {
                        let Some(w) = self.msg_word(&mut page, start, k) else {
                            break;
                        };
                        let sep = if k > 1 { " " } else { "" };
                        self.queue.text(format_args!("{sep}{w:06x}"));
                    }
                    self.queue.text(format_args!("\""));
                } else {
                    for k in 1..len {
                        let Some(w) = self.msg_word(&mut page, start, k) else {
                            break;
                        };
                        for c in 0..3 {
                            // The first fragment starts with a signature.
                            if k == 1 && c == 0 && frag == 3 {
                                continue;
                            }
                            let ch = ((w >> (7 * c)) & 0x7F) as u8;
                            if ch != 0x03 {
                                self.text.push(ch);
                            }
                        }
                    }
                    while self.text.last() == Some(&0) {
                        self.text.pop();
                    }
                    let name = if kind == 0 { "secure" } else { "alpha" };
                    self.queue
                        .text(format_args!(" type={name} msg=\"{}\"", Escaped(&self.text)));
                }
                match (frag == 3, more) {
                    (true, false) => {}
                    (true, true) => self.queue.text(format_args!(" part=first")),
                    (false, true) => self.queue.text(format_args!(" part=more")),
                    (false, false) => self.queue.text(format_args!(" part=last")),
                }
            }
            _ => self
                .queue
                .text(format_args!(" type=instruction vector={v:06x}")),
        }
        if page.fixed > 0 {
            self.queue.text(format_args!(" fixed={}", page.fixed));
        }
        if page.bad > 0 {
            self.queue.text(format_args!(" bad={}", page.bad));
        }
        self.queue.line(format_args!(""));
    }
}

fn trim_spaces(s: &mut Vec<u8>) {
    while s.last() == Some(&b' ') {
        s.pop();
    }
}

impl Block for FlexDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let min = 4.0 * CLOCK_BAUD;
        if rate < min {
            return Err(Error::Other(format!(
                "needs at least {} (4 samples per symbol at 3200 Bd), not {}",
                fmt_rate(min),
                fmt_rate(rate)
            )));
        }
        let sps = rate / CLOCK_BAUD;
        let len = (sps.round() as usize).max(1);
        self.rate = rate;
        self.ring = vec![0.0; len];
        self.pos = 0;
        self.sum = 0.0;
        self.scale = 1.0 / len as f32;
        self.clock = Some(BitClock::new(sps, HUNT_GAIN));
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.clock.is_none() {
            return Err(not_configured());
        }
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.queue.flush(&mut out[made..]);
            if !self.queue.is_empty() {
                break;
            }
            if self.pending {
                self.emit_next();
                continue;
            }
            let inp = io.inputs[0].slice::<f32>();
            if used == inp.len() {
                break;
            }
            while used < inp.len() {
                let x = inp[used];
                used += 1;
                self.samples += 1;
                self.sample(x);
                if self.pending {
                    break;
                }
            }
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "fix_errors" => self.fix = value.as_u64().unwrap_or(2).min(2) as u32,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(FlexDecoder::new(p.u64("fix_errors")?.min(2) as u32)),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_codes_are_far_apart() {
        for (i, a) in MODES.iter().enumerate() {
            for b in &MODES[i + 1..] {
                assert!((a.0 ^ b.0).count_ones() >= 6, "{:04x} {:04x}", a.0, b.0);
            }
        }
        let r = (0x78F3u64 << 48) | (u64::from(SYNC_MARKER) << 16) | u64::from(!0x78F3u16);
        assert_eq!(sync_mode(r), Some(0));
        assert_eq!(sync_mode(r ^ 0b1001), Some(0));
        assert_eq!(sync_mode(!r), None);
    }

    #[test]
    fn long_addresses_start_after_the_short_ones() {
        assert_eq!(long_capcode(1, 0x1F_FFFE), 2_101_249);
    }

    #[test]
    fn frame_information_checksum() {
        // Cycle 3, frame 45: nibbles 0..4 plus bit 20 add up to 0xF.
        let w = (45 << 8) | (3 << 4);
        let c = (0x2F - (3 + 13 + 2)) & 0xF;
        assert!(checksum_ok(w | c));
        assert!(!checksum_ok(w | ((c + 1) & 0xF)));
    }
}
