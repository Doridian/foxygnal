//! RDS (RBDS in North America) decoder: MPX in, text lines out.
//!
//! Front end: shift the 57 kHz subcarrier to 0 Hz, low-pass and resample to
//! 19 kS/s (16 samples per 1187.5 bit/s bit), low-pass again at 3 kHz.
//! Demodulator: a Costas loop (BPSK) recovers the carrier; a matched filter
//! for the biphase (Manchester) symbol peaks at each bit's centre; the strobe
//! follows the sample phase (of 16) where its power is largest on average,
//! one sample per bit at most, which also tracks clock drift. A change of
//! sign between two symbols is a 1 (RDS is differentially coded, which also
//! removes the Costas loop's 180° ambiguity).
//!
//! Repaired blocks (one wrong bit or two adjacent) can be wrong on weak
//! signals: the PI code and PTY change only on error-free blocks, a text segment
//! built from a repaired block must arrive twice the same before it is used,
//! and clock time only comes from clean blocks.
//!
//! Bits form 26-bit blocks: 16 data bits and a 10-bit check word with one of
//! five offset words (A, B, C, C', D) added. The syndrome of a correct block
//! is its offset word, which is how block boundaries are found: two offsets
//! a whole number of blocks apart in the right order. Four blocks make a
//! group: PI code (A), group type, PTY and so on (B), then C and D.
//!
//! Decoded: PI, PTY / TP (`type=station`), the programme service name
//! (`type=ps`, groups 0A/0B), RadioText (`type=rt`, 2A/2B) and clock time
//! (`type=ct`, 4A). A line goes out when one of them is complete and differs
//! from what was last sent; `verbose` adds a line per group and sync changes.

use std::fmt;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Band, DesignWindow, FirSpec, Resampler, Rotator};
use serde_json::json;

use super::{PILOT, ratio};
use crate::design::{self, fmt_rate};
use crate::digital::{MsgQueue, idle_status, loop_gains, text_meta};
use crate::filters::not_configured;

/// The rate the demodulator runs at: 16 samples per bit.
pub const DEMOD_RATE: f64 = 19_000.0;
const SPS: f32 = 16.0;
const HALF: usize = 8;
const SUBCARRIER: f64 = 3.0 * PILOT;
/// Demodulator samples per inner pass.
const CHUNK: usize = 256;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "rds_decoder",
            label: "RDS Decoder",
            category: "Digital",
            help: "Decodes RDS / RBDS (the 57 kHz data subcarrier of broadcast FM) from the MPX \
                   signal: an FM Demodulator's output with de-emphasis off, at 120 kS/s or more \
                   (e.g. 240 kS/s). One text line per new piece of information: \
                   `t=12.345 pi=D3C3 type=ps ps=\"FOXY FM \"`, also `type=station` (PTY, TP), \
                   `type=rt` (RadioText) and `type=ct` (clock time). Connect it to a Message \
                   Log and watch the wire with the Messages instrument.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![
                ParamSpec {
                    name: "fix_errors",
                    label: "Fix errors",
                    kind: ParamKind::Bool,
                    default: json!(true),
                    help: "Repair blocks with one wrong bit or two adjacent wrong bits (found \
                           from the check word). Decodes more on weak stations, at a small \
                           risk of accepting a corrupted block.",
                    live: true,
                },
                design::choice(
                    "standard",
                    "PTY names",
                    vec!["rds", "rbds"],
                    "rds",
                    "Programme type names: RDS (Europe and most of the world) or RBDS (North \
                     America). Only the names differ.",
                ),
                ParamSpec {
                    name: "verbose",
                    label: "Verbose",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "Also write a line per group (`type=group group=0A blocks=…`) and \
                           when block sync is found or lost.",
                    live: true,
                },
            ],
        },
        build,
    );
}

// ---- Block code -----------------------------------------------------------------

/// g(x) = x¹⁰ + x⁸ + x⁷ + x⁵ + x⁴ + x³ + 1.
const POLY: u32 = 0x5B9;
/// Offset words A, B, C, C', D.
const OFFSETS: [u16; 5] = [0x0FC, 0x198, 0x168, 0x350, 0x1B4];

/// The remainder of a 26-bit block divided by g(x): for a correct block,
/// its offset word.
pub fn syndrome(block: u32) -> u16 {
    let mut r = block & 0x3FF_FFFF;
    for i in (10..26).rev() {
        if r & (1 << i) != 0 {
            r ^= POLY << (i - 10);
        }
    }
    r as u16
}

/// The check word for 16 data bits (before adding the offset).
#[cfg(test)]
fn crc(data: u16) -> u16 {
    syndrome(u32::from(data) << 10)
}

/// Block slot (0 = A, 1 = B, 2 = C or C', 3 = D) of an offset word, and
/// whether it is C'.
fn slot_of(syn: u16) -> Option<(u8, bool)> {
    match OFFSETS.iter().position(|&o| o == syn)? {
        0 => Some((0, false)),
        1 => Some((1, false)),
        2 => Some((2, false)),
        3 => Some((2, true)),
        _ => Some((3, false)),
    }
}

/// Error patterns the decoder repairs: one bit, or two adjacent bits, keyed
/// by their syndrome. (All 51 syndromes are distinct and non-zero.)
fn error_table() -> [(u16, u32); 51] {
    std::array::from_fn(|i| {
        let pattern = if i < 26 { 1 << i } else { 0b11 << (i - 26) };
        (syndrome(pattern), pattern)
    })
}

/// A block's 16 data bits, and whether they were repaired.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Word {
    v: u16,
    fixed: bool,
}

struct Decoded {
    slot: u8,
    cprime: bool,
    data: Option<Word>,
}

/// Block synchronisation.
struct Sync {
    shift: u32,
    bits: u64,
    synced: bool,
    /// Recent offset words seen while searching: (bit count, slot).
    recent: [(u64, u8); 8],
    next_recent: usize,
    slot: u8,
    in_block: u8,
    /// One bit per recent block, 1 = bad (newest in bit 0).
    history: u64,
    fix: bool,
    errors: [(u16, u32); 51],
}

impl Sync {
    fn new(fix: bool) -> Self {
        Self {
            shift: 0,
            bits: 0,
            synced: false,
            recent: [(0, 0); 8],
            next_recent: 0,
            slot: 0,
            in_block: 0,
            history: 0,
            fix,
            errors: error_table(),
        }
    }

    /// Take one bit; returns a block when one ends. `lost` is set when sync
    /// was lost on this bit.
    fn bit(&mut self, bit: u8, lost: &mut bool) -> Option<Decoded> {
        self.shift = ((self.shift << 1) | u32::from(bit)) & 0x3FF_FFFF;
        self.bits += 1;
        if !self.synced {
            let (slot, cprime) = slot_of(syndrome(self.shift))?;
            let found = self.recent.iter().any(|&(at, s)| {
                let d = self.bits - at;
                at > 0
                    && d % 26 == 0
                    && d <= 26 * 8
                    && (u64::from(s) + d / 26) % 4 == u64::from(slot)
            });
            self.recent[self.next_recent] = (self.bits, slot);
            self.next_recent = (self.next_recent + 1) % self.recent.len();
            if !found {
                return None;
            }
            self.synced = true;
            self.history = 0;
            self.slot = (slot + 1) % 4;
            self.in_block = 0;
            return Some(Decoded {
                slot,
                cprime,
                data: Some(Word {
                    v: (self.shift >> 10) as u16,
                    fixed: false,
                }),
            });
        }
        self.in_block += 1;
        if self.in_block < 26 {
            return None;
        }
        self.in_block = 0;
        let slot = self.slot;
        self.slot = (slot + 1) % 4;
        let syn = syndrome(self.shift);
        let mut cprime = false;
        let mut data = None;
        let candidates: &[(u16, bool)] = match slot {
            0 => &[(OFFSETS[0], false)],
            1 => &[(OFFSETS[1], false)],
            2 => &[(OFFSETS[2], false), (OFFSETS[3], true)],
            _ => &[(OFFSETS[4], false)],
        };
        for &(off, cp) in candidates {
            if syn == off {
                data = Some(Word {
                    v: (self.shift >> 10) as u16,
                    fixed: false,
                });
                cprime = cp;
                break;
            }
        }
        if data.is_none() && self.fix {
            for &(off, cp) in candidates {
                if let Some(&(_, pattern)) = self.errors.iter().find(|e| e.0 == syn ^ off) {
                    data = Some(Word {
                        v: ((self.shift ^ pattern) >> 10) as u16,
                        fixed: true,
                    });
                    cprime = cp;
                    break;
                }
            }
        }
        self.history = (self.history << 1) | u64::from(data.is_none());
        let recent_bad = (self.history & ((1 << 50) - 1)).count_ones();
        // Lost: 20 bad blocks in a row (a bit slip), or almost nothing good.
        if (self.history & 0xF_FFFF) == 0xF_FFFF || recent_bad > 42 {
            self.synced = false;
            self.recent = [(0, 0); 8];
            *lost = true;
        }
        Some(Decoded { slot, cprime, data })
    }
}

// ---- Demodulator ----------------------------------------------------------------

/// Costas loop, biphase matched filter and bit timing at 19 kS/s.
struct Demod {
    phase: f32,
    freq: f32,
    pull: f32,
    alpha: f32,
    beta: f32,
    power: f32,
    hist: [f32; 2 * HALF],
    pos: usize,
    /// Average matched-filter power at each of the 16 sample phases of a
    /// bit, and the coefficient of that average.
    energy: [f32; 2 * HALF],
    ea: f32,
    /// Phase (0..16) of the current sample, and samples until the strobe.
    bin: usize,
    wait: usize,
    prev: f32,
    /// Matched filter taps: one period of a sine over the bit, close to the
    /// shaped biphase symbol RDS sends.
    weights: [f32; 2 * HALF],
}

const COSTAS_BW_HZ: f64 = 8.0;
const COSTAS_PULL_HZ: f64 = 30.0;
/// Time constant of the bit timing's power averages.
const TIMING_AVERAGE: f64 = 0.3;

impl Demod {
    fn new() -> Self {
        let (a, b) = loop_gains(std::f64::consts::TAU * COSTAS_BW_HZ / DEMOD_RATE);
        Self {
            weights: std::array::from_fn(|k| {
                (std::f32::consts::TAU * (k as f32 + 0.5) / (2 * HALF) as f32).sin()
            }),
            phase: 0.0,
            freq: 0.0,
            pull: (std::f64::consts::TAU * COSTAS_PULL_HZ / DEMOD_RATE) as f32,
            alpha: a as f32,
            beta: b as f32,
            power: 0.0,
            hist: [0.0; 2 * HALF],
            pos: 0,
            energy: [0.0; 2 * HALF],
            ea: (f64::from(SPS) / (TIMING_AVERAGE * DEMOD_RATE)) as f32,
            bin: 0,
            wait: 2 * HALF,
            prev: 0.0,
        }
    }

    /// One sample; returns a bit (after differential decoding) at each strobe.
    #[inline]
    fn sample(&mut self, z: Complex32) -> Option<u8> {
        use std::f32::consts::{PI, TAU};
        let (s, c) = self.phase.sin_cos();
        let y = z * Complex32::new(c, -s);
        let p = y.norm_sqr();
        self.power = if self.power == 0.0 {
            p
        } else {
            self.power + 0.001 * (p - self.power)
        };
        let err = if self.power > 1e-30 {
            (y.re * y.im / self.power).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        self.freq = (self.freq + self.beta * err).clamp(-self.pull, self.pull);
        self.phase += self.freq + self.alpha * err;
        if self.phase > PI {
            self.phase -= TAU;
        } else if self.phase < -PI {
            self.phase += TAU;
        }

        // Matched filter: + first half of the symbol, − second half.
        self.hist[self.pos] = y.re;
        self.pos = (self.pos + 1) % self.hist.len();
        let (mut m, n) = (0.0, self.hist.len());
        for (k, w) in self.weights.iter().enumerate() {
            m += w * self.hist[(self.pos + k) % n];
        }
        // Bit timing: strobe where the matched filter's power is largest on
        // average, moving at most one sample per bit towards it.
        let bin = self.bin;
        self.bin = (bin + 1) % self.energy.len();
        self.energy[bin] += self.ea * (m * m - self.energy[bin]);
        self.wait -= 1;
        if self.wait > 0 {
            return None;
        }
        let mut best = 0;
        for (i, &e) in self.energy.iter().enumerate() {
            if e > self.energy[best] {
                best = i;
            }
        }
        // How far the best phase is after this strobe's (0..16; above 8
        // means before it).
        let d = (best + 2 * HALF - bin) % (2 * HALF);
        self.wait = if d == 0 || d == HALF {
            2 * HALF
        } else if d < HALF {
            2 * HALF + 1
        } else {
            2 * HALF - 1
        };
        let sym = m;
        let bit = u8::from(sym * self.prev < 0.0);
        self.prev = sym;
        Some(bit)
    }
}

// ---- Group decoding ---------------------------------------------------------------

/// RDS text (EBU character set) as UTF-8 in quotes' content: `"` and `\`
/// escaped, control characters as spaces.
struct RdsText<'a>(&'a [u8]);

impl fmt::Display for RdsText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;
        for &b in self.0 {
            match b {
                b'"' => f.write_str("\\\"")?,
                b'\\' => f.write_str("\\\\")?,
                0x20..=0x7E => f.write_char(b as char)?,
                0x80..=0xFF => f.write_char(HIGH[usize::from(b - 0x80)])?,
                _ => f.write_char(' ')?,
            }
        }
        Ok(())
    }
}

/// Characters 0x80–0xFF of the RDS basic character set (EN 50067 annex E).
const HIGH: [char; 128] = [
    'á', 'à', 'é', 'è', 'í', 'ì', 'ó', 'ò', 'ú', 'ù', 'Ñ', 'Ç', 'Ş', 'ß', '¡', 'Ĳ', //
    'â', 'ä', 'ê', 'ë', 'î', 'ï', 'ô', 'ö', 'û', 'ü', 'ñ', 'ç', 'ş', 'ğ', 'ı', 'ĳ', //
    'ª', 'α', '©', '‰', 'Ğ', 'ě', 'ň', 'ő', 'π', '€', '£', '$', '←', '↑', '→', '↓', //
    'º', '¹', '²', '³', '±', 'İ', 'ń', 'ű', 'µ', '¿', '÷', '°', '¼', '½', '¾', '§', //
    'Á', 'À', 'É', 'È', 'Í', 'Ì', 'Ó', 'Ò', 'Ú', 'Ù', 'Ř', 'Č', 'Š', 'Ž', 'Ð', 'Ŀ', //
    'Â', 'Ä', 'Ê', 'Ë', 'Î', 'Ï', 'Ô', 'Ö', 'Û', 'Ü', 'ř', 'č', 'š', 'ž', 'đ', 'ŀ', //
    'Ã', 'Å', 'Æ', 'Œ', 'ŷ', 'Ý', 'Õ', 'Ø', 'Þ', 'Ŋ', 'Ŕ', 'Ć', 'Ś', 'Ź', 'Ŧ', 'ð', //
    'ã', 'å', 'æ', 'œ', 'ŵ', 'ý', 'õ', 'ø', 'þ', 'ŋ', 'ŕ', 'ć', 'ś', 'ź', 'ŧ', ' ', //
];

const PTY_RDS: [&str; 32] = [
    "None",
    "News",
    "Current affairs",
    "Information",
    "Sport",
    "Education",
    "Drama",
    "Culture",
    "Science",
    "Varied",
    "Pop music",
    "Rock music",
    "Easy listening",
    "Light classical",
    "Serious classical",
    "Other music",
    "Weather",
    "Finance",
    "Children's programmes",
    "Social affairs",
    "Religion",
    "Phone-in",
    "Travel",
    "Leisure",
    "Jazz music",
    "Country music",
    "National music",
    "Oldies music",
    "Folk music",
    "Documentary",
    "Alarm test",
    "Alarm",
];

const PTY_RBDS: [&str; 32] = [
    "None",
    "News",
    "Information",
    "Sports",
    "Talk",
    "Rock",
    "Classic rock",
    "Adult hits",
    "Soft rock",
    "Top 40",
    "Country",
    "Oldies",
    "Soft",
    "Nostalgia",
    "Jazz",
    "Classical",
    "Rhythm and blues",
    "Soft rhythm and blues",
    "Language",
    "Religious music",
    "Religious talk",
    "Personality",
    "Public",
    "College",
    "Spanish talk",
    "Spanish music",
    "Hip hop",
    "Unassigned",
    "Unassigned",
    "Weather",
    "Emergency test",
    "Emergency",
];

/// A value that counts once it was seen twice in a row in error-free blocks
/// (repaired blocks are wrong too often on weak signals to change it).
#[derive(Default)]
struct Confirmed<T> {
    candidate: Option<T>,
    value: Option<T>,
}

impl<T: Copy + PartialEq> Confirmed<T> {
    /// Feed a sighting; true if the confirmed value changed.
    fn see(&mut self, v: T, clean: bool) -> bool {
        if !clean {
            return false;
        }
        if self.candidate == Some(v) && self.value != Some(v) {
            self.value = Some(v);
            return true;
        }
        self.candidate = Some(v);
        false
    }
}

/// Text segments (up to four characters each). A segment counts once it
/// arrives in error-free blocks, or twice the same from repaired ones (a
/// repair can be wrong). A counted segment that changes means new text:
/// the others are collected afresh.
struct Segments<const N: usize> {
    val: [u32; N],
    have: u32,
    candidate: [Option<u32>; N],
}

impl<const N: usize> Segments<N> {
    fn new() -> Self {
        Self {
            val: [0; N],
            have: 0,
            candidate: [None; N],
        }
    }

    fn clear(&mut self) {
        self.have = 0;
        self.candidate = [None; N];
    }

    fn has(&self, seg: usize) -> bool {
        self.have & (1 << seg) != 0
    }

    fn put(&mut self, seg: usize, v: u32, clean: bool) {
        if !clean && self.candidate[seg] != Some(v) {
            if !(self.has(seg) && self.val[seg] == v) {
                self.candidate[seg] = Some(v);
            }
            return;
        }
        self.candidate[seg] = None;
        if self.has(seg) && self.val[seg] != v {
            self.clear();
        }
        self.val[seg] = v;
        self.have |= 1 << seg;
    }
}

/// Everything decoded from groups, and the lines waiting to go out.
struct Groups {
    rbds: bool,
    verbose: bool,
    blocks: [Option<Word>; 4],
    cprime: bool,
    pi: Confirmed<u16>,
    station: Confirmed<(u8, bool)>,
    ps: Segments<4>,
    ps_sent: Option<[u8; 8]>,
    rt: Segments<16>,
    rt_ab: Option<bool>,
    rt_b: bool,
    rt_sent: [u8; 64],
    rt_sent_len: Option<usize>,
    queue: MsgQueue,
}

impl Groups {
    fn new(rbds: bool, verbose: bool) -> Self {
        Self {
            rbds,
            verbose,
            blocks: [None; 4],
            cprime: false,
            pi: Confirmed::default(),
            station: Confirmed::default(),
            ps: Segments::new(),
            ps_sent: None,
            rt: Segments::new(),
            rt_ab: None,
            rt_b: false,
            rt_sent: [0; 64],
            rt_sent_len: None,
            queue: MsgQueue::new(),
        }
    }

    fn block(&mut self, b: Decoded, t: f64) {
        if b.slot == 0 {
            self.blocks = [None; 4];
            self.cprime = false;
        }
        self.blocks[usize::from(b.slot)] = b.data;
        if b.slot == 2 {
            self.cprime = b.cprime;
        }
        if b.slot == 3 {
            self.group(t);
            self.blocks = [None; 4];
        }
    }

    fn head(&mut self, t: f64, pi: Option<u16>, kind: &str) {
        match pi {
            Some(pi) => self
                .queue
                .text(format_args!("t={t:.3} pi={pi:04X} type={kind}")),
            None => self
                .queue
                .text(format_args!("t={t:.3} pi=---- type={kind}")),
        }
    }

    fn sync_changed(&mut self, t: f64, locked: bool) {
        if locked {
            self.blocks = [None; 4];
        }
        if self.verbose {
            let pi = self.pi.value;
            self.head(t, pi, "sync");
            self.queue.line(format_args!(" locked={locked}"));
        }
    }

    fn group(&mut self, t: f64) {
        let [a, b, c, d] = self.blocks;
        let version_b = b.map_or(self.cprime, |b| b.v & 0x800 != 0);
        // PI from block A, or from C' in version B groups.
        let pi_word = a.or(if self.cprime { c } else { None });
        if self.verbose {
            self.head(t, pi_word.map(|w| w.v), "group");
            match b {
                Some(b) => self.queue.text(format_args!(
                    " group={}{}",
                    b.v >> 12,
                    if version_b { 'B' } else { 'A' }
                )),
                None => self.queue.text(format_args!(" group=?")),
            }
            self.queue.text(format_args!(" blocks="));
            for (i, blk) in self.blocks.iter().enumerate() {
                let sep = if i == 0 { "" } else { "," };
                match blk {
                    Some(w) if w.fixed => self.queue.text(format_args!("{sep}{:04X}*", w.v)),
                    Some(w) => self.queue.text(format_args!("{sep}{:04X}", w.v)),
                    None => self.queue.text(format_args!("{sep}----")),
                }
            }
            self.queue.line(format_args!(""));
        }
        if let Some(w) = pi_word {
            if self.pi.see(w.v, !w.fixed) {
                // A different station: forget its texts.
                self.ps.clear();
                self.ps_sent = None;
                self.rt.clear();
                self.rt_ab = None;
                self.rt_sent_len = None;
                self.station = Confirmed::default();
            }
        }
        let Some(pi) = self.pi.value else { return };
        if pi_word.is_some_and(|w| w.v != pi) {
            return; // a group from another station
        }
        let Some(b) = b else { return };
        let pty = ((b.v >> 5) & 0x1F) as u8;
        let tp = b.v & 0x400 != 0;
        if self.station.see((pty, tp), !b.fixed) {
            let names = if self.rbds { &PTY_RBDS } else { &PTY_RDS };
            self.head(t, Some(pi), "station");
            self.queue.line(format_args!(
                " pty={pty} pty_name=\"{}\" tp={}",
                names[usize::from(pty)],
                u8::from(tp)
            ));
        }
        match (b.v >> 12, version_b) {
            (0, _) => self.ps_segment(t, pi, b, d),
            (2, _) => self.rt_segment(t, pi, b, c, d, version_b),
            (4, false) => self.clock(t, pi, b, c, d),
            _ => {}
        }
    }

    fn ps_segment(&mut self, t: f64, pi: u16, b: Word, d: Option<Word>) {
        let Some(d) = d else { return };
        self.ps
            .put(usize::from(b.v & 3), u32::from(d.v), !b.fixed && !d.fixed);
        if self.ps.have != 0xF {
            return;
        }
        let mut ps = [0u8; 8];
        for (seg, v) in self.ps.val.iter().enumerate() {
            ps[2 * seg..2 * seg + 2].copy_from_slice(&(*v as u16).to_be_bytes());
        }
        if self.ps_sent != Some(ps) {
            self.ps_sent = Some(ps);
            self.head(t, Some(pi), "ps");
            self.queue.line(format_args!(" ps=\"{}\"", RdsText(&ps)));
        }
    }

    fn rt_segment(
        &mut self,
        t: f64,
        pi: u16,
        b: Word,
        c: Option<Word>,
        d: Option<Word>,
        version_b: bool,
    ) {
        let ab = b.v & 0x10 != 0;
        if self.rt_ab != Some(ab) || self.rt_b != version_b {
            if b.fixed {
                return; // don't start over on a flag that may be wrong
            }
            self.rt_ab = Some(ab);
            self.rt_b = version_b;
            self.rt.clear();
        }
        let (v, clean, width) = if version_b {
            let Some(d) = d else { return };
            (u32::from(d.v), !d.fixed, 2)
        } else {
            let (Some(c), Some(d)) = (c, d) else { return };
            (
                (u32::from(c.v) << 16) | u32::from(d.v),
                !c.fixed && !d.fixed,
                4,
            )
        };
        self.rt.put(usize::from(b.v & 0xF), v, clean && !b.fixed);

        // Complete once every segment up to the end (a CR, or the last
        // segment) is in.
        let mut text = [b' '; 64];
        let mut len = 16 * width;
        for seg in 0..16 {
            if !self.rt.has(seg) {
                return;
            }
            let bytes = self.rt.val[seg].to_be_bytes();
            let chars = &bytes[4 - width..];
            text[seg * width..(seg + 1) * width].copy_from_slice(chars);
            if let Some(i) = chars.iter().position(|&ch| ch == 0x0D) {
                len = seg * width + i;
                break;
            }
        }
        while len > 0 && text[len - 1] == b' ' {
            len -= 1;
        }
        if self.rt_sent_len == Some(len) && self.rt_sent[..len] == text[..len] {
            return;
        }
        self.rt_sent[..len].copy_from_slice(&text[..len]);
        self.rt_sent_len = Some(len);
        self.head(t, Some(pi), "rt");
        self.queue.line(format_args!(
            " ab={} rt=\"{}\"",
            if ab { 'B' } else { 'A' },
            RdsText(&text[..len])
        ));
    }

    fn clock(&mut self, t: f64, pi: u16, b: Word, c: Option<Word>, d: Option<Word>) {
        let (Some(c), Some(d)) = (c, d) else { return };
        if b.fixed || c.fixed || d.fixed {
            return; // a wrong repair would make a plausible wrong time
        }
        let (b, c, d) = (b.v, c.v, d.v);
        let mjd = (u32::from(b & 3) << 15) | u32::from(c >> 1);
        let hour = (u32::from(c & 1) << 4) | u32::from(d >> 12);
        let minute = u32::from((d >> 6) & 0x3F);
        let half_hours = u32::from(d & 0x1F);
        let sign = if d & 0x20 != 0 { '-' } else { '+' };
        if hour > 23 || minute > 59 || mjd < 15079 {
            return;
        }
        let (y, m, day) = mjd_to_date(mjd);
        self.head(t, Some(pi), "ct");
        self.queue.line(format_args!(
            " utc={y:04}-{m:02}-{day:02}T{hour:02}:{minute:02}Z offset={sign}{:02}:{:02}",
            half_hours / 2,
            (half_hours % 2) * 30
        ));
    }
}

/// Modified Julian Day → (year, month, day), per EN 50067 annex G.
fn mjd_to_date(mjd: u32) -> (u32, u32, u32) {
    let mjd = f64::from(mjd);
    let yp = ((mjd - 15078.2) / 365.25).floor();
    let mp = ((mjd - 14956.1 - (yp * 365.25).floor()) / 30.6001).floor();
    let day = mjd - 14956.0 - (yp * 365.25).floor() - (mp * 30.6001).floor();
    let k = if mp == 14.0 || mp == 15.0 { 1.0 } else { 0.0 };
    (
        (yp + k + 1900.0) as u32,
        (mp - 1.0 - k * 12.0) as u32,
        day as u32,
    )
}

// ---- Block ------------------------------------------------------------------------

struct Front {
    mix: Rotator,
    stage1: Resampler<Complex32>,
    stage2: Resampler<Complex32>,
    s1: Vec<Complex32>,
    s2: Vec<Complex32>,
}

pub struct RdsDecoder {
    front: Option<Front>,
    demod: Demod,
    sync: Sync,
    groups: Groups,
    /// Demodulator samples so far (the time base of `t=`).
    samples: u64,
}

impl RdsDecoder {
    pub fn new(fix_errors: bool, rbds: bool, verbose: bool) -> Self {
        Self {
            front: None,
            demod: Demod::new(),
            sync: Sync::new(fix_errors),
            groups: Groups::new(rbds, verbose),
            samples: 0,
        }
    }

    fn front(rate: f64) -> Result<Front, Error> {
        let min = 2.0 * (SUBCARRIER + 3_000.0);
        if rate < min {
            return Err(Error::Other(format!(
                "needs the FM discriminator output (MPX) at {} or more to carry the 57 kHz \
                 RDS subcarrier, not {}: decimate less before the FM demodulator",
                fmt_rate(min),
                fmt_rate(rate)
            )));
        }
        let (l, m) = ratio(rate, DEMOD_RATE)?;
        let fast = rate * l as f64;
        // Pass ±3 kHz, stop at ±9 kHz (nothing folds into the RDS band).
        let taps1 = FirSpec {
            band: Band::LowPass(6_000.0 / fast),
            transition: 6_000.0 / fast,
            window: DesignWindow::Kaiser { attenuation: 60.0 },
            gain: l as f64,
        }
        .design()
        .map_err(|e| design::design_error(e, fast))?;
        // Then keep ±2.4 kHz and drop L−R (from 4 kHz below the subcarrier).
        let taps2 = FirSpec {
            band: Band::LowPass(3_000.0 / DEMOD_RATE),
            transition: 1_200.0 / DEMOD_RATE,
            window: DesignWindow::Kaiser { attenuation: 60.0 },
            gain: 1.0,
        }
        .design()
        .map_err(|e| design::design_error(e, DEMOD_RATE))?;
        Ok(Front {
            mix: Rotator::new(-SUBCARRIER / rate),
            stage1: Resampler::new(l, m, &taps1),
            stage2: Resampler::fir(&taps2),
            s1: vec![Complex32::default(); CHUNK],
            s2: vec![Complex32::default(); CHUNK],
        })
    }
}

impl Block for RdsDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.front = Some(Self::front(cx.input_rate(0)?)?);
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let front = self.front.as_mut().ok_or_else(not_configured)?;
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.groups.queue.flush(&mut out[made..]);
            if !self.groups.queue.is_empty() {
                break;
            }
            let inp = io.inputs[0].slice::<f32>();
            let mix = &mut front.mix;
            let (u, k) = front.stage1.process_with(&inp[used..], &mut front.s1, |x| {
                mix.next(Complex32::new(x, 0.0))
            });
            used += u;
            let (_, k) = front.stage2.process(&front.s1[..k], &mut front.s2[..k]);
            for &z in &front.s2[..k] {
                self.samples += 1;
                let Some(bit) = self.demod.sample(z) else {
                    continue;
                };
                let t = self.samples as f64 / DEMOD_RATE;
                let was = self.sync.synced;
                let mut lost = false;
                let block = self.sync.bit(bit, &mut lost);
                if !was && self.sync.synced {
                    self.groups.sync_changed(t, true);
                }
                if let Some(b) = block {
                    self.groups.block(b, t);
                }
                if lost {
                    self.groups.sync_changed(t, false);
                }
            }
            if u == 0 && k == 0 {
                break;
            }
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.groups.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "fix_errors" => self.sync.fix = value.as_bool().unwrap_or(true),
            "verbose" => self.groups.verbose = value.as_bool().unwrap_or(false),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(RdsDecoder::new(
            p.bool("fix_errors")?,
            p.str("standard")? == "rbds",
            p.bool("verbose")?,
        )),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_syndromes_of_their_blocks() {
        for (data, off) in [(0x1234u16, OFFSETS[0]), (0xBEEF, OFFSETS[3])] {
            let block = (u32::from(data) << 10) | u32::from(crc(data) ^ off);
            assert_eq!(syndrome(block), off);
        }
    }

    #[test]
    fn error_patterns_are_distinct() {
        let t = error_table();
        for (i, a) in t.iter().enumerate() {
            assert_ne!(a.0, 0);
            for b in &t[i + 1..] {
                assert_ne!(a.0, b.0, "{:#x} vs {:#x}", a.1, b.1);
            }
            // Block C is checked against both C and C': a repaired C must
            // not look like a clean C' or the other way round.
            assert_ne!(a.0, OFFSETS[2] ^ OFFSETS[3], "{:#x}", a.1);
        }
    }

    #[test]
    fn mjd_converts() {
        // EN 50067 example: MJD 45218 = 1982-09-06.
        assert_eq!(mjd_to_date(45218), (1982, 9, 6));
        assert_eq!(mjd_to_date(61041), (2026, 1, 1));
    }

    #[test]
    fn rds_text_maps_and_escapes() {
        assert_eq!(
            RdsText(b"a\"b\\c\x0D\x91").to_string(),
            "a\\\"b\\\\c \u{e4}"
        );
    }
}
