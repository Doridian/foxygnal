//! POCSAG pager decoder (ITU-R M.584): FM-discriminator audio in, text out.
//!
//! A transmission is a preamble (at least 576 bits of 1010…), then batches
//! of a sync codeword (0x7CD215D8) and 16 codewords: 8 frames of two, a
//! pager listening only in the frame given by the low three bits of its
//! address (RIC). Each 32-bit codeword is a BCH(31,21) codeword plus an even
//! parity bit, sent most significant bit first. Bit 31 is 0 for an address
//! codeword (18 address bits, 2 function bits) and 1 for a message codeword
//! (20 data bits); 0x7A89C197 is the idle codeword. Messages are numeric (4-bit
//! BCD-like digits) or alphanumeric (7-bit ASCII), both least significant bit
//! first, packed across codewords.
//!
//! The decoder runs one lane per bit rate (512, 1200 and 2400 Bd when the
//! rate is `auto`): a matched filter one bit long, a slicer level halfway
//! between the tracked high and low levels, and a bit-timing PLL. Each lane
//! looks for the sync word or its inverse (so the FM polarity doesn't
//! matter), then reads batches until a sync word is missing.

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::text::{BitClock, Escaped};
use super::{MsgQueue, idle_status, text_meta};
use crate::design::{self, fmt_rate};
use crate::filters::not_configured;

pub const SYNC: u32 = 0x7CD2_15D8;
pub const IDLE: u32 = 0x7A89_C197;
/// g(x) = x¹⁰ + x⁹ + x⁸ + x⁶ + x⁵ + x³ + 1.
const POLY: u32 = 0x769;
/// Bit errors allowed in the sync word when looking for a transmission, and
/// when one is expected at the end of a batch.
const SYNC_HUNT_ERRORS: u32 = 2;
const SYNC_BATCH_ERRORS: u32 = 4;
/// Longest message kept, in message codewords.
const MAX_WORDS: usize = 256;
/// Bit rates tried in `auto`.
const RATES: [u32; 3] = [512, 1200, 2400];

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "pocsag_decoder",
            label: "POCSAG Decoder",
            category: "Digital",
            help: "Decodes POCSAG pager messages from an FM demodulator's output (de-emphasis \
                   off; 4.5 kHz deviation = ±1 is ideal, but the level doesn't matter) or any \
                   NRZ baseband, at 9.6 kS/s or more (e.g. 48 kS/s). 512, 1200 and 2400 Bd, \
                   either polarity, BCH(31,21) error correction. One text line per message: \
                   `t=12.345 baud=1200 ric=1234567 func=3 type=alpha msg=\"…\"` (`type=numeric`, \
                   or `type=tone` for an address without a message). Pager messages can be \
                   personal: decode only what you may lawfully receive.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![
                design::choice(
                    "baud",
                    "Bit rate",
                    vec!["auto", "512", "1200", "2400"],
                    "auto",
                    "Auto listens at all three rates at once.",
                ),
                design::choice(
                    "format",
                    "Message format",
                    vec!["auto", "numeric", "alpha", "both"],
                    "auto",
                    "Auto: numeric for function 0, alphanumeric for function 3, and for the \
                     others (or when that looks wrong) whichever decodes to more plausible \
                     text. Both: two fields, `num=` and `alpha=`.",
                ),
                ParamSpec {
                    name: "fix_errors",
                    label: "Fix bit errors",
                    kind: ParamKind::Int {
                        min: Some(0),
                        max: Some(2),
                    },
                    default: json!(2),
                    help: "Wrong bits repaired per codeword (0–2, the parity bit counts). More \
                           decodes weaker signals, at a higher risk of a wrong repair.",
                    live: true,
                },
            ],
        },
        build,
    );
}

// ---- Codewords ------------------------------------------------------------------

/// Remainder of a 31-bit BCH codeword divided by g(x): 0 if it is one.
pub fn syndrome(cw31: u32) -> u32 {
    let mut r = cw31 & 0x7FFF_FFFF;
    for i in (10..31).rev() {
        if r & (1 << i) != 0 {
            r ^= POLY << (i - 10);
        }
    }
    r
}

/// Error patterns (bits 1–31 of a codeword) by syndrome, for one and two
/// wrong bits; 0 where there is none.
fn error_table() -> Box<[u32; 1024]> {
    let mut t = Box::new([0u32; 1024]);
    for i in 0..31 {
        t[syndrome(1 << i) as usize] = 1 << (i + 1);
        for j in 0..i {
            let s = syndrome((1 << i) | (1 << j));
            t[s as usize] = (1 << (i + 1)) | (1 << (j + 1));
        }
    }
    t
}

/// Hamming distance from `w` to the sync word.
fn sync_errors(w: u32) -> u32 {
    (w ^ SYNC).count_ones()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Auto,
    Numeric,
    Alpha,
    Both,
}

/// What every lane shares: the repair table, the output and scratch space.
struct Shared {
    table: Box<[u32; 1024]>,
    fix: u32,
    format: Format,
    rate: f64,
    queue: MsgQueue,
    num: Vec<u8>,
    alpha: Vec<u8>,
}

impl Shared {
    /// Repair `w` if it has at most `fix` wrong bits. Returns the codeword
    /// and how many bits were repaired.
    fn correct(&self, w: u32) -> Option<(u32, u32)> {
        let mut w = w;
        let mut fixed = 0;
        let s = syndrome(w >> 1);
        if s != 0 {
            let e = self.table[s as usize];
            if e == 0 || e.count_ones() > self.fix {
                return None;
            }
            w ^= e;
            fixed = e.count_ones();
        }
        if !w.count_ones().is_multiple_of(2) {
            if fixed >= self.fix {
                return None;
            }
            w ^= 1;
            fixed += 1;
        }
        Some((w, fixed))
    }

    fn emit(&mut self, baud: u32, m: &Message) {
        let t = m.t / self.rate;
        self.queue.text(format_args!(
            "t={t:.3} baud={baud} ric={} func={}",
            m.ric, m.func
        ));
        if m.len == 0 {
            self.queue.text(format_args!(" type=tone"));
        } else {
            decode_numeric(&m.words[..m.len], &mut self.num);
            decode_alpha(&m.words[..m.len], &mut self.alpha);
            let (num, alpha) = (&self.num[..], &self.alpha[..]);
            let format = match self.format {
                Format::Auto => choose(m.func, num, alpha),
                f => f,
            };
            match format {
                Format::Numeric => self
                    .queue
                    .text(format_args!(" type=numeric msg=\"{}\"", Escaped(num))),
                Format::Alpha => self
                    .queue
                    .text(format_args!(" type=alpha msg=\"{}\"", Escaped(alpha))),
                _ => self.queue.text(format_args!(
                    " type=both num=\"{}\" alpha=\"{}\"",
                    Escaped(num),
                    Escaped(alpha)
                )),
            }
        }
        if m.errors > 0 {
            self.queue.text(format_args!(" fixed={}", m.errors));
        }
        if m.bad > 0 {
            self.queue.text(format_args!(" bad={}", m.bad));
        }
        self.queue.line(format_args!(""));
    }
}

/// Numeric digits: 0–9, then (A–F) spare, U (urgent), space, hyphen, `]`, `[`.
const NUMERIC: &[u8; 16] = b"0123456789.U -][";

fn decode_numeric(words: &[u32], out: &mut Vec<u8>) {
    out.clear();
    for &w in words {
        for k in 0..5 {
            let nibble = (w >> (16 - 4 * k)) & 0xF;
            // First bit sent is the least significant.
            let v = nibble.reverse_bits() >> 28;
            out.push(NUMERIC[v as usize]);
        }
    }
    while out.last() == Some(&b' ') {
        out.pop();
    }
}

fn decode_alpha(words: &[u32], out: &mut Vec<u8>) {
    out.clear();
    let (mut ch, mut n) = (0u8, 0);
    for &w in words {
        for i in (0..20).rev() {
            ch |= (((w >> i) & 1) as u8) << n;
            n += 1;
            if n == 7 {
                out.push(ch);
                (ch, n) = (0, 0);
            }
        }
    }
    // Padding: NUL, EOT, ETX and other control characters at the end.
    while out.last().is_some_and(|&c| c < 0x20 || c == 0x7F) {
        out.pop();
    }
}

/// Share of `s` for which `good` holds (0 when empty).
fn share(s: &[u8], good: impl Fn(u8) -> bool) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    s.iter().filter(|&&c| good(c)).count() as f64 / s.len() as f64
}

/// Auto format: the function bits' usual meaning, unless the text says
/// otherwise.
fn choose(func: u8, num: &[u8], alpha: &[u8]) -> Format {
    // Numeric text: mostly digits, and never the spare code (`.`).
    let digits = share(num, |c| c.is_ascii_digit());
    let num_ok = digits >= 0.5 && !num.contains(&b'.');
    let alpha_ok = share(alpha, |c| {
        (0x20..0x7F).contains(&c) || matches!(c, b'\r' | b'\n' | b'\t')
    }) >= 0.9;
    let alpha = match func {
        0 => alpha_ok && !num_ok,
        3 => alpha_ok || !num_ok,
        _ => match (num_ok, alpha_ok) {
            (true, true) => digits < 0.8,
            (num_ok, _) => !num_ok,
        },
    };
    if alpha {
        Format::Alpha
    } else {
        Format::Numeric
    }
}

// ---- Lanes ----------------------------------------------------------------------

/// A message being collected.
struct Message {
    active: bool,
    /// Input sample of its address codeword.
    t: f64,
    ric: u32,
    func: u8,
    words: Vec<u32>,
    len: usize,
    errors: u32,
    bad: u32,
}

struct Lane {
    baud: u32,
    ring: Vec<f32>,
    pos: usize,
    sum: f64,
    scale: f32,
    hi: f32,
    lo: f32,
    attack: f32,
    decay: f32,
    clock: BitClock,
    /// The last 32 bits, as received.
    reg: u32,
    synced: bool,
    invert: bool,
    /// Bits of the current codeword so far, and the codeword.
    nbits: u32,
    cw: u32,
    /// Codeword index in the batch; 16 = the sync word is next.
    index: u32,
    msg: Message,
}

impl Lane {
    fn new(baud: u32, rate: f64) -> Self {
        let sps = rate / f64::from(baud);
        let len = (sps.round() as usize).max(1);
        Self {
            baud,
            ring: vec![0.0; len],
            pos: 0,
            sum: 0.0,
            scale: 1.0 / len as f32,
            hi: 0.0,
            lo: 0.0,
            // Level tracker: rises within about a bit, decays over ~100 bits.
            attack: (1.0 - (-1.0 / sps).exp()) as f32,
            decay: (1.0 - (-1.0 / (100.0 * sps)).exp()) as f32,
            clock: BitClock::new(sps, 0.12),
            reg: 0,
            synced: false,
            invert: false,
            nbits: 0,
            cw: 0,
            index: 0,
            msg: Message {
                active: false,
                t: 0.0,
                ric: 0,
                func: 0,
                words: vec![0; MAX_WORDS],
                len: 0,
                errors: 0,
                bad: 0,
            },
        }
    }

    #[inline]
    fn sample(&mut self, x: f32, now: u64, sh: &mut Shared) {
        let old = std::mem::replace(&mut self.ring[self.pos], x);
        self.pos += 1;
        if self.pos == self.ring.len() {
            self.pos = 0;
        }
        self.sum += f64::from(x) - f64::from(old);
        let y = self.sum as f32 * self.scale;
        let a = if y > self.hi { self.attack } else { self.decay };
        self.hi += a * (y - self.hi);
        let a = if y < self.lo { self.attack } else { self.decay };
        self.lo += a * (y - self.lo);
        if let Some(v) = self.clock.sample(y - 0.5 * (self.hi + self.lo)) {
            self.bit(u8::from(v >= 0.0), now, sh);
        }
    }

    fn bit(&mut self, b: u8, now: u64, sh: &mut Shared) {
        self.reg = (self.reg << 1) | u32::from(b);
        if !self.synced {
            if sync_errors(self.reg) <= SYNC_HUNT_ERRORS {
                self.invert = false;
            } else if sync_errors(!self.reg) <= SYNC_HUNT_ERRORS {
                self.invert = true;
            } else {
                return;
            }
            self.synced = true;
            (self.nbits, self.index) = (0, 0);
            return;
        }
        self.cw = (self.cw << 1) | u32::from(b ^ u8::from(self.invert));
        self.nbits += 1;
        if self.nbits < 32 {
            return;
        }
        self.nbits = 0;
        let cw = self.cw;
        if self.index == 16 {
            if sync_errors(cw) <= SYNC_BATCH_ERRORS {
                self.index = 0;
            } else {
                // End of the transmission.
                self.finish(sh);
                self.synced = false;
            }
            return;
        }
        let frame = self.index / 2;
        self.index += 1;
        match sh.correct(cw) {
            Some((w, _)) if w == IDLE => self.finish(sh),
            Some((w, fixed)) if w & 0x8000_0000 == 0 => {
                self.finish(sh);
                let m = &mut self.msg;
                m.active = true;
                m.t = now as f64;
                m.ric = (((w >> 13) & 0x3_FFFF) << 3) | frame;
                m.func = ((w >> 11) & 3) as u8;
                m.len = 0;
                m.errors = fixed;
                m.bad = 0;
            }
            Some((w, fixed)) => {
                self.msg.errors += fixed;
                self.push((w >> 11) & 0xF_FFFF);
            }
            None => {
                // Keep its bits so the rest of the text stays aligned.
                self.msg.bad += 1;
                self.push((cw >> 11) & 0xF_FFFF);
            }
        }
    }

    fn push(&mut self, data: u32) {
        let m = &mut self.msg;
        if m.active && m.len < MAX_WORDS {
            m.words[m.len] = data;
            m.len += 1;
        }
    }

    fn finish(&mut self, sh: &mut Shared) {
        if self.msg.active {
            self.msg.active = false;
            sh.emit(self.baud, &self.msg);
        }
    }
}

// ---- Block ----------------------------------------------------------------------

pub struct PocsagDecoder {
    rates: Vec<u32>,
    lanes: Vec<Lane>,
    shared: Shared,
    /// Input samples so far (the time base of `t=`).
    samples: u64,
}

impl PocsagDecoder {
    pub fn new(rates: Vec<u32>, format: &str, fix: u32) -> Self {
        Self {
            rates,
            lanes: Vec::new(),
            shared: Shared {
                table: error_table(),
                fix,
                format: match format {
                    "numeric" => Format::Numeric,
                    "alpha" => Format::Alpha,
                    "both" => Format::Both,
                    _ => Format::Auto,
                },
                rate: 0.0,
                queue: MsgQueue::new(),
                num: Vec::with_capacity(MAX_WORDS * 5),
                alpha: Vec::with_capacity(MAX_WORDS * 20 / 7 + 1),
            },
            samples: 0,
        }
    }
}

impl Block for PocsagDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let top = self.rates.iter().copied().max().unwrap_or(2400);
        let min = 4.0 * f64::from(top);
        if rate < min {
            return Err(Error::Other(format!(
                "needs at least {} (4 samples per bit at {top} Bd), not {}",
                fmt_rate(min),
                fmt_rate(rate)
            )));
        }
        self.shared.rate = rate;
        self.lanes = self.rates.iter().map(|&b| Lane::new(b, rate)).collect();
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.lanes.is_empty() {
            return Err(not_configured());
        }
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.shared.queue.flush(&mut out[made..]);
            if !self.shared.queue.is_empty() {
                break;
            }
            let inp = io.inputs[0].slice::<f32>();
            if used == inp.len() {
                break;
            }
            while used < inp.len() {
                let x = inp[used];
                used += 1;
                self.samples += 1;
                for lane in &mut self.lanes {
                    lane.sample(x, self.samples, &mut self.shared);
                }
                if !self.shared.queue.is_empty() {
                    break;
                }
            }
        }
        if used == 0 && made == 0 {
            // The input ended: a message still being collected is complete.
            if io.inputs[0].exhausted() && self.shared.queue.is_empty() {
                for lane in &mut self.lanes {
                    lane.finish(&mut self.shared);
                }
                if !self.shared.queue.is_empty() {
                    return Ok(WorkStatus::Ok);
                }
            }
            return Ok(idle_status(io, &self.shared.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "fix_errors" => self.shared.fix = value.as_u64().unwrap_or(2).min(2) as u32,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let rates = match p.str("baud")? {
        "512" => vec![512],
        "1200" => vec![1200],
        "2400" => vec![2400],
        _ => RATES.to_vec(),
    };
    Ok(Instance {
        block: Box::new(PocsagDecoder::new(
            rates,
            p.str("format")?,
            p.u64("fix_errors")?.min(2) as u32,
        )),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_and_idle_are_codewords() {
        for w in [SYNC, IDLE] {
            assert_eq!(syndrome(w >> 1), 0, "{w:08x}");
            assert_eq!(w.count_ones() % 2, 0, "{w:08x}");
        }
    }

    #[test]
    fn one_and_two_bit_errors_have_distinct_syndromes() {
        let t = error_table();
        let n = t.iter().filter(|&&e| e != 0).count();
        assert_eq!(n, 31 + 31 * 30 / 2);
    }

    #[test]
    fn numeric_and_alpha_unpack() {
        // "12" + three spaces: digits sent least significant bit first.
        let rev = |d: u32| d.reverse_bits() >> 28;
        let w = rev(1) << 16 | rev(2) << 12 | rev(0xC) << 8 | rev(0xC) << 4 | rev(0xC);
        let mut out = Vec::new();
        decode_numeric(&[w], &mut out);
        assert_eq!(out, b"12");
        // 'A' = 0x41: bits sent 1,0,0,0,0,0,1, then padding zeros.
        let w = 0b1000001 << 13;
        decode_alpha(&[w], &mut out);
        assert_eq!(out, b"A");
    }

    #[test]
    fn auto_format_follows_function_bits_and_content() {
        assert!(choose(0, b"5551234", b"\x13\x01q") == Format::Numeric);
        assert!(choose(3, b"U[]5.-", b"Hello") == Format::Alpha);
        // Function 0 but the numeric reading is junk and the alpha reading is text.
        assert!(choose(0, b"U[]5.]U[", b"Hello world") == Format::Alpha);
        assert!(choose(1, b"U[]5.]U[", b"Hello world") == Format::Alpha);
        assert!(choose(2, b"0123", b"\x01\x02\x03") == Format::Numeric);
    }
}
