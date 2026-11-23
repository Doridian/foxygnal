//! ACARS (ARINC 618) decoder: AM-demodulated audio in, text out.
//!
//! ACARS sends 2400 bit/s MSK in the audio of an AM transmitter: 2400 Hz
//! for a bit equal to the one before it, 1200 Hz for a change, phase
//! continuous. Around 1800 Hz that is ±600 Hz, a quarter turn of phase per
//! bit, so on bit boundaries the signal's phase relative to an 1800 Hz
//! reference, rotated back by a quarter turn per bit, is 0 or π: the bit
//! itself. The demodulator therefore detects coherently:
//!
//! - mix down by 1800 Hz and filter with the MSK matched filter (a half
//!   cosine two bits long);
//! - bit timing from the zero crossings of a one-bit differential phase
//!   detector (its sign is the tone), with the bit-timing PLL the FSK
//!   decoders share;
//! - at each bit boundary, rotate the filtered sample back by a quarter turn
//!   per bit; a decision-directed PLL tracks its phase (modulo π: the
//!   polarity is settled by the sync characters), and the sign is the bit.
//!
//! A block is characters of 7 bits plus odd parity, least significant bit
//! first: pre-key (all 1s), `+` `*` (bit sync), SYN SYN (character sync),
//! SOH, mode, 7-character address (registration), technical
//! acknowledgement, 2-character label, block identifier, STX, the text
//! (downlinks start it with a 4-character message number and a 6-character
//! flight number), ETX (or ETB when more blocks follow), and a CRC-16
//! (CCITT, reflected, initial value 0) over everything from the mode to
//! ETX. Up to two wrong bits are found and repaired from the parity bits
//! and the CRC syndrome.

use std::f32::consts::{FRAC_PI_2, TAU};
use std::fmt::Write;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::text::{BitClock, Escaped};
use super::{MsgQueue, idle_status, text_meta};
use crate::design::{float_min, fmt_rate};

/// Bits per second.
pub const BAUD: f64 = 2400.0;
/// Centre of the two tones.
const CENTRE: f64 = 1800.0;
const SYN: u8 = 0x16;
const SOH: u8 = 0x01;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const ETB: u8 = 0x17;
const NAK: u8 = 0x15;
const DEL: u8 = 0x7F;
/// Mode through block identifier.
const HEADER: usize = 12;
/// Longest block kept, from the mode to ETX (220 characters of text).
const MAX_BLOCK: usize = HEADER + 2 + 220;
/// Parity errors after which a block is given up.
const MAX_PARITY_ERRORS: u32 = 4;
/// SYN SYN SOH as received (first character in the low byte).
const SYNC: u32 = SYN as u32 | (SYN as u32) << 8 | (SOH as u32) << 16;
/// Bit-timing PLL gain per zero crossing.
const CLOCK_GAIN: f64 = 0.07;
/// Carrier phase loop: gain per bit while hunting (the pre-key is a steady
/// tone) and within a block.
const HUNT_PHASE_GAIN: f32 = 0.1;
const BLOCK_PHASE_GAIN: f32 = 0.04;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "acars_decoder",
            label: "ACARS Decoder",
            category: "Digital",
            help: "Decodes ACARS (aircraft datalink, 2400 bit/s MSK on AM, around 129–131 MHz \
                   in Europe and North America) from an AM demodulator's output at 9.6 kS/s or \
                   more (e.g. 12.5 or 48 kS/s). Coherent MSK demodulation, bit sync, SYN SYN \
                   character sync, 7-bit characters with odd parity, CRC-16 with repair of up \
                   to two wrong bits. One line per block: `t=12.345 freq=131.550 mode=2 \
                   reg=N12345 ack=NAK label=H1 block=1 msgno=M01A flight=XX0123 \
                   text=\"…\"` (`msgno` and `flight` on downlinks; `more=1` when further \
                   blocks of the message follow; `fixed=` the bits repaired). ACARS messages \
                   can be personal or confidential: decode only what you may lawfully \
                   receive.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![
                float_min(
                    "frequency",
                    "Frequency",
                    0.0,
                    0.0,
                    Some("Hz"),
                    "The channel's frequency, written into each message (`freq=` in MHz). 0 \
                     = leave it out.",
                ),
                ParamSpec {
                    name: "fix_errors",
                    label: "Fix bit errors",
                    kind: ParamKind::Int {
                        min: Some(0),
                        max: Some(2),
                    },
                    default: json!(2),
                    help: "Wrong bits repaired per block (0–2), found from the parity bits and \
                           the CRC. More decodes weaker signals, at a higher risk of a wrong \
                           repair.",
                    live: true,
                },
            ],
        },
        build,
    );
}

// ---- Blocks -----------------------------------------------------------------------

/// CRC-16/KERMIT one byte at a time (reflected 0x1021, initial value 0).
fn crc_byte(crc: u16, b: u8) -> u16 {
    (0..8).fold(crc ^ u16::from(b), |c, _| {
        if c & 1 != 0 {
            (c >> 1) ^ 0x8408
        } else {
            c >> 1
        }
    })
}

/// The CRC residual of `data` followed by its two CRC bytes: 0 if intact.
pub fn residual(data: &[u8], crc: [u8; 2]) -> u16 {
    let c = data.iter().fold(0, |c, &b| crc_byte(c, b));
    crc_byte(crc_byte(c, crc[0]), crc[1])
}

/// The CRC value to send after `data` (low byte first).
pub fn crc(data: &[u8]) -> [u8; 2] {
    let c = data.iter().fold(0, |c, &b| crc_byte(c, b));
    [c as u8, (c >> 8) as u8]
}

fn odd(b: u8) -> bool {
    b.count_ones() % 2 == 1
}

/// Repairs up to `max` wrong bits in a block (`data` from the mode to ETX,
/// then the two CRC bytes) from its parity and CRC residual.
struct Repair {
    /// The residual a single wrong bit causes, by its distance from the end
    /// of the CRC (in bits, 0 = the CRC's last bit).
    syndrome: Vec<u16>,
}

impl Repair {
    fn new() -> Self {
        let n = 8 * (MAX_BLOCK + 2);
        let mut syndrome = Vec::with_capacity(n);
        // A 1 followed by d zero bits.
        let mut c = 0x8408u16;
        for _ in 0..n {
            syndrome.push(c);
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x8408
            } else {
                c >> 1
            };
        }
        Self { syndrome }
    }

    /// Syndrome of bit `bit` of byte `i` in a block of `n` bytes (CRC
    /// included).
    fn syn(&self, n: usize, i: usize, bit: usize) -> u16 {
        self.syndrome[8 * (n - 1 - i) + (7 - bit)]
    }

    /// Fix `data` in place. Returns the number of bits repaired, or `None`
    /// if the block can't be made consistent with at most `max` changes.
    fn fix(&self, data: &mut [u8], crc: [u8; 2], max: u32) -> Option<u32> {
        let r = residual(data, crc);
        let n = data.len() + 2;
        let mut bad = [0usize; 2];
        let mut nbad = 0;
        for (i, &b) in data.iter().enumerate() {
            if !odd(b) {
                if nbad == 2 {
                    return None;
                }
                bad[nbad] = i;
                nbad += 1;
            }
        }
        if r == 0 && nbad == 0 {
            return Some(0);
        }
        // A wrong bit in the CRC itself (it has no parity).
        let in_crc = |s: u16| (0..16).any(|e| self.syndrome[e] == s);
        match (nbad, max) {
            (_, 0) => None,
            (0, _) => {
                if in_crc(r) {
                    return Some(1);
                }
                if max < 2 {
                    return None;
                }
                // Two bits of one character (its parity still odd).
                for (i, byte) in data.iter_mut().enumerate() {
                    for a in 0..8 {
                        for b in a + 1..8 {
                            if self.syn(n, i, a) ^ self.syn(n, i, b) == r {
                                *byte ^= (1 << a) | (1 << b);
                                return Some(2);
                            }
                        }
                    }
                }
                // Two bits of the CRC.
                (0..16)
                    .any(|e| (e + 1..16).any(|f| self.syndrome[e] ^ self.syndrome[f] == r))
                    .then_some(2)
            }
            (1, _) => {
                let i = bad[0];
                for a in 0..8 {
                    let s = r ^ self.syn(n, i, a);
                    if s == 0 || (max >= 2 && in_crc(s)) {
                        data[i] ^= 1 << a;
                        return Some(if s == 0 { 1 } else { 2 });
                    }
                }
                None
            }
            (_, 1) => None,
            _ => {
                let (i, j) = (bad[0], bad[1]);
                for a in 0..8 {
                    for b in 0..8 {
                        if self.syn(n, i, a) ^ self.syn(n, j, b) == r {
                            data[i] ^= 1 << a;
                            data[j] ^= 1 << b;
                            return Some(2);
                        }
                    }
                }
                None
            }
        }
    }
}

/// An unquoted field: printable ASCII, anything else `_`.
struct Token<'a>(&'a [u8]);

impl std::fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for &b in self.0 {
            f.write_char(if b.is_ascii_graphic() {
                char::from(b)
            } else {
                '_'
            })?;
        }
        Ok(())
    }
}

/// Describe a checked block (`data` from the mode to ETX/ETB, parity bits
/// removed) as text fields, without `t=` and `freq=`.
pub fn describe(data: &[u8], out: &mut String) {
    let mode = data[0];
    let addr = &data[1..8];
    let start = addr.iter().position(|&c| c != b'.').unwrap_or(addr.len());
    let ack = data[8];
    let mut label = [data[9], data[10]];
    if label[1] == DEL {
        // "_\x7f", the general response, is written "_d".
        label[1] = b'd';
    }
    let block = data[11];
    let _ = write!(out, "mode={} reg={}", Token(&[mode]), Token(&addr[start..]));
    if ack == NAK {
        out.push_str(" ack=NAK");
    } else {
        let _ = write!(out, " ack={}", Token(&[ack]));
    }
    let _ = write!(out, " label={} block={}", Token(&label), Token(&[block]));
    let end = data.len() - 1;
    let mut text: &[u8] = if data[HEADER] == STX {
        &data[HEADER + 1..end]
    } else {
        &[]
    };
    // Downlinks (block identifiers 0–9) start with a message number and the
    // flight number.
    if block.is_ascii_digit() && text.len() >= 10 {
        let _ = write!(
            out,
            " msgno={} flight={}",
            Token(&text[..4]),
            Token(&text[4..10])
        );
        text = &text[10..];
    }
    if !text.is_empty() {
        let _ = write!(out, " text=\"{}\"", Escaped(text));
    }
    if data[end] == ETB {
        out.push_str(" more=1");
    }
}

// ---- Demodulator ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Hunt,
    Block,
    Crc,
}

pub struct AcarsDecoder {
    freq: f64,
    fix: u32,
    rate: f64,
    repair: Repair,
    // Front end.
    /// NCO at 1800 Hz: phase and step per sample.
    lo: Complex32,
    lo_step: Complex32,
    /// Mixed samples (ring, newest at `pos - 1`) and the matched filter.
    mixed: Vec<Complex32>,
    taps: Vec<f32>,
    pos: usize,
    /// Filtered samples one bit back (ring).
    delayed: Vec<Complex32>,
    dpos: usize,
    prev_z: Complex32,
    clock: Option<BitClock>,
    count: u32,
    // Bits.
    /// Bit index modulo 4 (for the quarter-turn rotation).
    quarter: usize,
    /// Carrier phase estimate (radians, modulo π).
    phase: f32,
    phase_gain: f32,
    /// Last 32 bits, newest in the top bit (characters arrive LSB first).
    reg: u32,
    state: State,
    invert: bool,
    nbits: u32,
    buf: [u8; MAX_BLOCK],
    len: usize,
    crc: [u8; 2],
    crc_bytes: usize,
    parity_errors: u32,
    samples: u64,
    text: String,
    queue: MsgQueue,
}

impl AcarsDecoder {
    pub fn new(freq: f64, fix: u32) -> Self {
        Self {
            freq,
            fix,
            rate: 0.0,
            repair: Repair::new(),
            lo: Complex32::new(1.0, 0.0),
            lo_step: Complex32::new(1.0, 0.0),
            mixed: Vec::new(),
            taps: Vec::new(),
            pos: 0,
            delayed: Vec::new(),
            dpos: 0,
            prev_z: Complex32::default(),
            clock: None,
            count: 0,
            quarter: 0,
            phase: 0.0,
            phase_gain: HUNT_PHASE_GAIN,
            reg: 0,
            state: State::Hunt,
            invert: false,
            nbits: 0,
            buf: [0; MAX_BLOCK],
            len: 0,
            crc: [0; 2],
            crc_bytes: 0,
            parity_errors: 0,
            samples: 0,
            text: String::with_capacity(1024),
            queue: MsgQueue::new(),
        }
    }

    /// One input sample. Returns true when a message was queued.
    #[inline]
    fn sample(&mut self, x: f32) -> bool {
        self.samples += 1;
        let m = self.lo * x;
        self.lo *= self.lo_step;
        self.count += 1;
        if self.count == 4096 {
            self.count = 0;
            self.lo /= self.lo.norm();
        }
        let len = self.mixed.len();
        self.mixed[self.pos] = m;
        self.pos += 1;
        if self.pos == len {
            self.pos = 0;
        }
        // Matched filter (symmetric, so the order of the taps doesn't
        // matter).
        let (old, new) = self.mixed.split_at(self.pos);
        let z = new
            .iter()
            .chain(old)
            .zip(&self.taps)
            .fold(Complex32::default(), |a, (&s, &t)| a + s * t);
        // One-bit differential phase: positive for 2400 Hz.
        let back = std::mem::replace(&mut self.delayed[self.dpos], z);
        self.dpos += 1;
        if self.dpos == self.delayed.len() {
            self.dpos = 0;
        }
        let d = z * back.conj();
        let q = d.im / (d.norm() + 1e-20);
        let prev_z = std::mem::replace(&mut self.prev_z, z);
        let Some(g) = self.clock.as_mut().and_then(|c| c.instant(q)) else {
            return false;
        };
        self.bit(prev_z + (z - prev_z) * g)
    }

    /// The matched filter's output at a bit boundary.
    fn bit(&mut self, z: Complex32) -> bool {
        // Back by a quarter turn per bit, and by the carrier phase.
        let rot = Complex32::from_polar(1.0, -(self.quarter as f32) * FRAC_PI_2 - self.phase);
        self.quarter = (self.quarter + 1) % 4;
        let w = z * rot;
        let one = w.re >= 0.0;
        let err = (if one { w.im } else { -w.im }) / (w.norm() + 1e-20);
        self.phase = (self.phase + self.phase_gain * err).rem_euclid(TAU);
        let bit = one ^ self.invert;
        self.reg = (self.reg >> 1) | u32::from(bit) << 31;
        match self.state {
            State::Hunt => {
                let r = self.reg >> 8;
                let inverted = if (r ^ SYNC).count_ones() <= 1 {
                    false
                } else if (r ^ !SYNC & 0xFF_FFFF).count_ones() <= 1 {
                    true
                } else {
                    return false;
                };
                self.invert ^= inverted;
                self.state = State::Block;
                self.phase_gain = BLOCK_PHASE_GAIN;
                (self.nbits, self.len, self.parity_errors) = (0, 0, 0);
                false
            }
            State::Block | State::Crc => {
                self.nbits += 1;
                if self.nbits < 8 {
                    return false;
                }
                self.nbits = 0;
                let c = (self.reg >> 24) as u8;
                self.char(c)
            }
        }
    }

    fn hunt(&mut self) {
        self.state = State::Hunt;
        self.phase_gain = HUNT_PHASE_GAIN;
    }

    /// One character of a block.
    fn char(&mut self, c: u8) -> bool {
        if self.state == State::Crc {
            self.crc[self.crc_bytes] = c;
            self.crc_bytes += 1;
            if self.crc_bytes < 2 {
                return false;
            }
            self.hunt();
            return self.finish();
        }
        if self.len == MAX_BLOCK {
            self.hunt();
            return false;
        }
        if !odd(c) {
            self.parity_errors += 1;
            if self.parity_errors > MAX_PARITY_ERRORS {
                self.hunt();
                return false;
            }
        }
        self.buf[self.len] = c;
        self.len += 1;
        let end = matches!(c & 0x7F, ETX | ETB) && self.len > HEADER;
        if end {
            // The CRC's two bytes follow.
            self.state = State::Crc;
            self.crc_bytes = 0;
        }
        false
    }

    /// A whole block arrived: check, repair, queue.
    fn finish(&mut self) -> bool {
        let len = self.len;
        let data = &mut self.buf[..len];
        let Some(fixed) = self.repair.fix(data, self.crc, self.fix) else {
            return false;
        };
        if data.iter().any(|&b| !odd(b)) {
            return false;
        }
        for b in data.iter_mut() {
            *b &= 0x7F;
        }
        if data[HEADER] != STX && data[HEADER] != ETX && data[HEADER] != ETB {
            return false;
        }
        let out = &mut self.text;
        out.clear();
        let _ = write!(out, "t={:.3}", self.samples as f64 / self.rate);
        if self.freq > 0.0 {
            let _ = write!(out, " freq={:.3}", self.freq / 1e6);
        }
        out.push(' ');
        describe(&self.buf[..len], out);
        if fixed > 0 {
            let _ = write!(out, " fixed={fixed}");
        }
        out.push('\n');
        self.queue.bytes(self.text.as_bytes());
        true
    }
}

impl Block for AcarsDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if rate < 4.0 * BAUD {
            return Err(Error::Other(format!(
                "needs at least 9.6 kS/s (four samples per 2400 bit/s bit) but gets {}; \
                 e.g. 12.5 or 48 kS/s",
                fmt_rate(rate)
            )));
        }
        let sps = rate / BAUD;
        self.rate = rate;
        let w = std::f64::consts::TAU * CENTRE / rate;
        self.lo = Complex32::new(1.0, 0.0);
        self.lo_step = Complex32::new(w.cos() as f32, -w.sin() as f32);
        // Half a cosine, two bits long, centred.
        let half = sps.round() as usize;
        let n = 2 * half + 1;
        let taps: Vec<f32> = (0..n)
            .map(|i| {
                let t = (i as f64 - half as f64) / sps;
                (std::f64::consts::FRAC_PI_2 * t).cos().max(0.0) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        self.taps = taps.iter().map(|t| t / sum).collect();
        self.mixed = vec![Complex32::default(); n];
        self.pos = 0;
        self.delayed = vec![Complex32::default(); half.max(1)];
        self.dpos = 0;
        self.clock = Some(BitClock::new(sps, CLOCK_GAIN));
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.clock.is_none() {
            return Err(crate::filters::not_configured());
        }
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.queue.flush(&mut out[made..]);
            if !self.queue.is_empty() {
                break;
            }
            let inp = io.inputs[0].slice::<f32>();
            if used == inp.len() {
                break;
            }
            while used < inp.len() {
                let done = self.sample(inp[used]);
                used += 1;
                if done {
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
            "fix_errors" => {
                self.fix = value.as_u64().unwrap_or(2).min(2) as u32;
                Ok(())
            }
            _ => Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(AcarsDecoder::new(
            p.f64("frequency")?,
            p.u64("fix_errors")?.min(2) as u32,
        )),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_parity(s: &[u8]) -> Vec<u8> {
        s.iter()
            .map(|&c| if odd(c) { c } else { c | 0x80 })
            .collect()
    }

    #[test]
    fn crc_is_kermit() {
        // CRC-16/KERMIT of "123456789" is 0x2189 (sent 0x89 0x21).
        assert_eq!(crc(b"123456789"), [0x89, 0x21]);
        assert_eq!(residual(b"123456789", [0x89, 0x21]), 0);
    }

    #[test]
    fn repairs_one_and_two_bits() {
        let r = Repair::new();
        let good = with_parity(b"2.N12345\x15H11\x02M01AXX0123HELLO\x03");
        let c = crc(&good);
        let cases: &[&[(usize, u8)]] = &[&[], &[(5, 0x04)], &[(5, 0x14)], &[(3, 0x01), (20, 0x40)]];
        for flips in cases {
            let mut d = good.clone();
            for &(i, m) in *flips {
                d[i] ^= m;
            }
            let bits: u32 = flips.iter().map(|(_, m)| m.count_ones()).sum();
            assert_eq!(r.fix(&mut d, c, 2), Some(bits), "{flips:?}");
            assert_eq!(d, good);
        }
        // An error in the CRC bytes leaves the data alone.
        let mut d = good.clone();
        assert_eq!(r.fix(&mut d, [c[0] ^ 0x10, c[1]], 2), Some(1));
        assert_eq!(d, good);
        // Three wrong bits are too many; repairs can be switched off.
        let mut d = good.clone();
        d[2] ^= 0x01;
        d[9] ^= 0x01;
        d[15] ^= 0x01;
        assert_eq!(r.fix(&mut d, c, 2), None);
        let mut d = good.clone();
        d[2] ^= 0x01;
        assert_eq!(r.fix(&mut d, c, 0), None);
    }

    #[test]
    fn describes_uplinks_and_downlinks() {
        let mut s = String::new();
        describe(b"2.N12345\x15H11\x02M01AXX0123HELLO\x03", &mut s);
        assert_eq!(
            s,
            "mode=2 reg=N12345 ack=NAK label=H1 block=1 msgno=M01A flight=XX0123 \
             text=\"HELLO\""
        );
        s.clear();
        describe(b"2.G-ABCDA_\x7fA\x03", &mut s);
        assert_eq!(s, "mode=2 reg=G-ABCD ack=A label=_d block=A");
        s.clear();
        describe(b"E.N12345\x15Q0S\x02line \"1\"\r\nline 2\x17", &mut s);
        assert_eq!(
            s,
            "mode=E reg=N12345 ack=NAK label=Q0 block=S text=\"line \\\"1\\\"\\r\\nline 2\" more=1"
        );
    }
}
