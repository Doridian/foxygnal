//! AIS (Automatic Identification System, ITU-R M.1371) decoder: the ship
//! reports on 161.975 MHz (channel A, 87B) and 162.025 MHz (channel B, 88B).
//!
//! AIS is GMSK (BT 0.4, ±2.4 kHz) at 9600 Bd in short TDMA bursts: a
//! 24-bit training sequence, an HDLC flag, the message (each byte sent least
//! significant bit first), the CRC-16 FCS of HDLC, a closing flag. Bits are
//! NRZI coded and 1s stuffed as in HDLC, so the bit-level receiver is the
//! one the HDLC Deframer uses; NRZI also makes the polarity irrelevant.
//!
//! The demodulator works on an FM discriminator's output: a matched filter
//! (a moving average one bit long), then eight receivers, each sampling the
//! bits at its own fixed phase: a burst is too short (26 ms) for a timing
//! loop to settle reliably on, and too short for the clocks to drift, so
//! one of them is always within 1/16 bit of the middle. Each slicer follows
//! the mean of its last 16 decisions while hunting; the training sequence
//! (line levels 1100…, which average to the centre) followed by the start
//! flag sets the centre for the burst, so a frequency offset (a DC shift of
//! the discriminator's output) doesn't matter. The same burst found at two
//! phases goes out once.
//!
//! The message itself is a string of bits, read most significant first
//! within each byte, the same bits the NMEA `!AIVDM` sentences armour six at
//! a time. [`describe`] turns one into `key=value` text; [`nmea`] into
//! `!AIVDM` sentences for chart plotters.

use std::fmt::Write;

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::hdlc::{Deframer, fcs_ok};
use super::text::Escaped;
use super::{MsgQueue, idle_status, text_meta};
use crate::design::{self, fmt_rate};
use crate::filters::not_configured;

/// Bits per second.
pub const BAUD: f64 = 9600.0;
/// Longest frame kept: five slots of 256 bits, FCS included.
const MAX_FRAME: usize = 160;
/// Shortest message (an acknowledgement, 72 bits) and the FCS.
const MIN_FRAME: usize = 9 + 2;
/// Soft decisions remembered for the slicer's centre.
const HISTORY: usize = 32;
/// Bits of the training sequence checked before the start flag (a multiple
/// of 4, at most 16), and how many of them may be wrong.
const TRAINING: usize = 12;
const TRAINING_ERRORS: u32 = 2;
/// Sampling phases per bit, each with its own receiver: bursts are too short
/// for a bit-timing loop to settle reliably, and too short for the clocks
/// to drift.
const PHASES: usize = 8;
/// Length of the matched filter (a moving average), in bits.
const MF_BITS: f64 = 1.0;
/// Bits the slicer centre averages over while hunting.
const HUNT_MEAN: usize = 16;
/// Soft decisions are clipped to this many times the signal level around the
/// centre, so the discriminator's wild output between bursts can't drag the
/// slicer far.
const CLIP: f32 = 3.0;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "ais_decoder",
            label: "AIS Decoder",
            category: "Digital",
            help: "Decodes AIS ship reports (GMSK 9600 Bd on 161.975 MHz, channel A, and \
                   162.025 MHz, channel B) from an FM demodulator's output (de-emphasis off; \
                   the level and a frequency offset don't matter) at 28.8 kS/s or more, e.g. \
                   48 kS/s. Eight sampling phases, a slicer set from the training \
                   sequence, NRZI, HDLC flags and bit unstuffing, CRC-16. Text: one line per \
                   message, e.g. `t=12.345 ch=A mmsi=366053209 type=1 \
                   status=restricted lat=37.802118 lon=-122.341618 sog=0.0 cog=219.3 \
                   heading=1`; static data (type 5, 19, 24) adds `name=\"…\" callsign=… \
                   shiptype=… length=… beam=…`, voyage data `dest=\"…\" eta=… draught=…`. \
                   Types 1–5, 9, 11, 18, 19, 21, 24 and 27 are decoded; others come out as \
                   type, MMSI and `raw=` hex. The Map instrument plots the ships. NMEA: \
                   `!AIVDM` sentences, for chart plotters (a File Sink or UDP Sink passes \
                   them on). Use one decoder per channel.",
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
                    name: "channel",
                    label: "Channel",
                    kind: ParamKind::String,
                    default: json!("A"),
                    help: "Name of the channel this decoder listens to, written into each \
                           message (`ch=`) and NMEA sentence: A for 161.975 MHz, B for \
                           162.025 MHz.",
                    live: false,
                },
                design::choice(
                    "format",
                    "Output",
                    vec!["text", "nmea"],
                    "text",
                    "Text: decoded `key=value` lines. NMEA: `!AIVDM` sentences as an AIS \
                     receiver would send them to a chart plotter.",
                ),
            ],
        },
        build,
    );
}

// ---- Messages -------------------------------------------------------------------

/// A message's bits, most significant first within each byte.
#[derive(Clone, Copy)]
pub struct Bits<'a> {
    data: &'a [u8],
    len: usize,
}

impl<'a> Bits<'a> {
    /// The first `len` bits of `data`.
    pub fn new(data: &'a [u8], len: usize) -> Self {
        Self {
            data,
            len: len.min(8 * data.len()),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `n` (≤ 32) bits from bit `start`, unsigned; bits past the end read 0.
    pub fn u(&self, start: usize, n: usize) -> u32 {
        (start..start + n).fold(0u32, |v, i| {
            let b = i < self.len && self.data[i / 8] & (0x80 >> (i % 8)) != 0;
            v << 1 | u32::from(b)
        })
    }

    /// `n` bits from bit `start`, two's complement.
    pub fn i(&self, start: usize, n: usize) -> i32 {
        let v = self.u(start, n);
        let shift = 32 - n as u32;
        ((v << shift) as i32) >> shift
    }

    /// `chars` six-bit characters from bit `start`.
    pub fn text(&self, start: usize, chars: usize) -> SixBit<'a> {
        SixBit {
            bits: *self,
            start,
            chars: chars.min(self.len.saturating_sub(start) / 6),
        }
    }
}

/// Six-bit ASCII text (`@` = 0 … `_` = 31, then space … `?`), up to the
/// first `@` and without trailing spaces.
#[derive(Clone, Copy)]
pub struct SixBit<'a> {
    bits: Bits<'a>,
    start: usize,
    chars: usize,
}

impl SixBit<'_> {
    fn char(&self, k: usize) -> u8 {
        let v = self.bits.u(self.start + 6 * k, 6) as u8;
        if v < 32 { v + 64 } else { v }
    }

    /// Characters before the terminator, trailing spaces dropped.
    fn end(&self) -> usize {
        let n = (0..self.chars)
            .position(|k| self.char(k) == b'@')
            .unwrap_or(self.chars);
        (0..n)
            .rev()
            .find(|&k| self.char(k) != b' ')
            .map_or(0, |k| k + 1)
    }

    pub fn is_empty(&self) -> bool {
        self.end() == 0
    }

    /// Write the characters into `buf`; returns how many.
    fn bytes(&self, buf: &mut [u8; 32]) -> usize {
        let n = self.end().min(32);
        for (k, b) in buf[..n].iter_mut().enumerate() {
            *b = self.char(k);
        }
        n
    }
}

impl std::fmt::Display for SixBit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut buf = [0u8; 32];
        let n = self.bytes(&mut buf);
        write!(f, "{}", Escaped(&buf[..n]))
    }
}

/// Short names of the navigational status values.
const STATUS: [&str; 16] = [
    "under_way",
    "at_anchor",
    "not_under_command",
    "restricted",
    "constrained",
    "moored",
    "aground",
    "fishing",
    "sailing",
    "9",
    "10",
    "11",
    "12",
    "13",
    "ais_sart",
    "",
];

/// Position fixing devices.
const EPFD: [&str; 16] = [
    "",
    "gps",
    "glonass",
    "gps_glonass",
    "loran_c",
    "chayka",
    "ins",
    "surveyed",
    "galileo",
    "9",
    "10",
    "11",
    "12",
    "13",
    "14",
    "gnss",
];

fn status(out: &mut String, v: u32) {
    let s = STATUS[v as usize & 15];
    if !s.is_empty() {
        let _ = write!(out, " status={s}");
    }
}

fn epfd(out: &mut String, v: u32) {
    let s = EPFD[v as usize & 15];
    if !s.is_empty() {
        let _ = write!(out, " epfd={s}");
    }
}

/// Latitude and longitude in 1/10000 minute (28 and 27 bits), or 1/10
/// minute for type 27, skipped when "not available" (91°, 181°).
fn position(out: &mut String, lat: i32, lon: i32, per_degree: f64) {
    let (lat, lon) = (f64::from(lat) / per_degree, f64::from(lon) / per_degree);
    if lat.abs() <= 90.0 && lon.abs() <= 180.0 {
        let _ = write!(out, " lat={lat:.6} lon={lon:.6}");
    }
}

/// Speed in 1/10 knot (1023 = not available, 1022 = 102.2 knots or more).
fn sog(out: &mut String, v: u32) {
    if v != 1023 {
        let _ = write!(out, " sog={:.1}", f64::from(v) / 10.0);
    }
}

/// Course in 1/10 degree (3600 = not available).
fn cog(out: &mut String, v: u32) {
    if v < 3600 {
        let _ = write!(out, " cog={:.1}", f64::from(v) / 10.0);
    }
}

/// True heading in degrees (511 = not available).
fn heading(out: &mut String, v: u32) {
    if v < 360 {
        let _ = write!(out, " heading={v}");
    }
}

/// Rate of turn: ±(4.733·√(°/min)), ±127 turning faster than 5°/30 s
/// without a rate-of-turn indicator, −128 not available.
fn rot(out: &mut String, v: i32) {
    match v {
        -128 => {}
        127 => out.push_str(" rot=right"),
        -127 => out.push_str(" rot=left"),
        v => {
            let r = (f64::from(v) / 4.733).powi(2).round() as i32;
            let _ = write!(out, " rot={}", if v < 0 { -r } else { r });
        }
    }
}

fn text_field(out: &mut String, key: &str, t: SixBit<'_>) {
    if !t.is_empty() {
        let _ = write!(out, " {key}=\"{t}\"");
    }
}

/// Dimensions: distances from the reference point to bow, stern, port and
/// starboard, as length and beam.
fn dimensions(out: &mut String, b: &Bits<'_>, at: usize) {
    let (bow, stern) = (b.u(at, 9), b.u(at + 9, 9));
    let (port, star) = (b.u(at + 18, 6), b.u(at + 24, 6));
    if bow + stern > 0 {
        let _ = write!(out, " length={}", bow + stern);
    }
    if port + star > 0 {
        let _ = write!(out, " beam={}", port + star);
    }
}

fn shiptype(out: &mut String, v: u32) {
    if v != 0 {
        let _ = write!(out, " shiptype={v}");
    }
}

fn raw(out: &mut String, b: &Bits<'_>) {
    out.push_str(" raw=");
    for byte in &b.data[..b.len.div_ceil(8)] {
        let _ = write!(out, "{byte:02X}");
    }
}

/// Shortest length, in bits, of each decoded message type.
fn min_bits(kind: u32) -> usize {
    match kind {
        1..=4 | 9 | 11 | 18 => 168,
        5 => 424,
        19 => 312,
        21 => 272,
        24 => 160,
        27 => 96,
        _ => 38,
    }
}

/// Describe an AIS message (its first `len` bits) as `key=value` text:
/// `mmsi=… type=…` and the fields of the type. Returns the message type.
pub fn describe(data: &[u8], len: usize, out: &mut String) -> u32 {
    let b = Bits::new(data, len);
    let kind = b.u(0, 6);
    let _ = write!(out, "mmsi={:09} type={kind}", b.u(8, 30));
    if b.len() < min_bits(kind) {
        out.push_str(" short=1");
        raw(out, &b);
        return kind;
    }
    match kind {
        1..=3 => {
            status(out, b.u(38, 4));
            position(out, b.i(89, 27), b.i(61, 28), 600_000.0);
            sog(out, b.u(50, 10));
            cog(out, b.u(116, 12));
            heading(out, b.u(128, 9));
            rot(out, b.i(42, 8));
            let _ = write!(out, " second={}", b.u(137, 6));
        }
        4 | 11 => {
            let (y, mo, d) = (b.u(38, 14), b.u(52, 4), b.u(56, 5));
            let (h, mi, s) = (b.u(61, 5), b.u(66, 6), b.u(72, 6));
            if y != 0 && mo != 0 && d != 0 && h < 24 && mi < 60 && s < 60 {
                let _ = write!(out, " time={y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
            }
            position(out, b.i(107, 27), b.i(79, 28), 600_000.0);
            epfd(out, b.u(134, 4));
        }
        5 => {
            let imo = b.u(40, 30);
            if imo != 0 {
                let _ = write!(out, " imo={imo}");
            }
            text_field(out, "callsign", b.text(70, 7));
            text_field(out, "name", b.text(112, 20));
            shiptype(out, b.u(232, 8));
            dimensions(out, &b, 240);
            let (mo, d, h, mi) = (b.u(274, 4), b.u(278, 5), b.u(283, 5), b.u(288, 6));
            if mo != 0 && d != 0 && h < 24 && mi < 60 {
                let _ = write!(out, " eta={mo:02}-{d:02}T{h:02}:{mi:02}");
            }
            let draught = b.u(294, 8);
            if draught != 0 {
                let _ = write!(out, " draught={:.1}", f64::from(draught) / 10.0);
            }
            text_field(out, "dest", b.text(302, 20));
        }
        9 => {
            let alt = b.u(38, 12);
            if alt != 4095 {
                let _ = write!(out, " alt={alt}");
            }
            position(out, b.i(89, 27), b.i(61, 28), 600_000.0);
            if b.u(50, 10) != 1023 {
                let _ = write!(out, " sog={}", b.u(50, 10));
            }
            cog(out, b.u(116, 12));
        }
        18 | 19 => {
            position(out, b.i(85, 27), b.i(57, 28), 600_000.0);
            sog(out, b.u(46, 10));
            cog(out, b.u(112, 12));
            heading(out, b.u(124, 9));
            let _ = write!(out, " second={}", b.u(133, 6));
            if kind == 19 {
                text_field(out, "name", b.text(143, 20));
                shiptype(out, b.u(263, 8));
                dimensions(out, &b, 271);
            }
        }
        21 => {
            let _ = write!(out, " aid_type={}", b.u(38, 5));
            // The name, continued in the extension field.
            let (name, ext) = (b.text(43, 20), b.text(272, (b.len() - 272) / 6));
            if !name.is_empty() {
                let _ = write!(out, " name=\"{name}{ext}\"");
            }
            position(out, b.i(192, 27), b.i(164, 28), 600_000.0);
            dimensions(out, &b, 219);
            if b.u(259, 1) == 1 && b.u(253, 6) < 60 {
                out.push_str(" off_position=1");
            }
            if b.u(269, 1) == 1 {
                out.push_str(" virtual=1");
            }
        }
        24 => match b.u(38, 2) {
            0 => {
                out.push_str(" part=A");
                text_field(out, "name", b.text(40, 20));
            }
            1 => {
                out.push_str(" part=B");
                shiptype(out, b.u(40, 8));
                text_field(out, "vendor", b.text(48, 3));
                text_field(out, "callsign", b.text(90, 7));
                let mmsi = b.u(8, 30);
                if mmsi / 10_000_000 == 98 {
                    // An auxiliary craft names its mother ship instead.
                    let _ = write!(out, " mothership={:09}", b.u(132, 30));
                } else {
                    dimensions(out, &b, 132);
                }
            }
            p => {
                let _ = write!(out, " part={p}");
            }
        },
        27 => {
            status(out, b.u(40, 4));
            position(out, b.i(62, 17), b.i(44, 18), 600.0);
            if b.u(79, 6) != 63 {
                let _ = write!(out, " sog={}", b.u(79, 6));
            }
            if b.u(85, 9) < 360 {
                let _ = write!(out, " cog={}", b.u(85, 9));
            }
        }
        _ => raw(out, &b),
    }
    kind
}

/// The six-bit value of an armoured payload character.
fn unarmor(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'W' => Some(c - 48),
        b'`'..=b'w' => Some(c - 56),
        _ => None,
    }
}

/// The bits of an `!AIVDM` payload (several sentences' payloads
/// concatenated), with `fill` bits to drop at the end: bytes and bit count.
pub fn dearmor(payload: &str, fill: usize) -> Option<(Vec<u8>, usize)> {
    let mut data = vec![0u8; (6 * payload.len()).div_ceil(8)];
    for (k, c) in payload.bytes().enumerate() {
        let v = unarmor(c)?;
        for i in 0..6 {
            if v & (0x20 >> i) != 0 {
                let bit = 6 * k + i;
                data[bit / 8] |= 0x80 >> (bit % 8);
            }
        }
    }
    let len = (6 * payload.len()).checked_sub(fill)?;
    Some((data, len))
}

/// The payload character of a six-bit value.
fn armor(v: u8) -> char {
    char::from(if v < 40 { v + 48 } else { v + 56 })
}

/// Characters of payload per sentence.
const NMEA_CHARS: usize = 60;

/// `!AIVDM` sentences (each ending in `\n`) carrying the first `len` bits of
/// `data`, received on `channel`; `seq` numbers multi-sentence messages.
pub fn nmea(data: &[u8], len: usize, channel: &str, seq: u8, out: &mut String) {
    let b = Bits::new(data, len);
    let chars = b.len().div_ceil(6);
    let fill = 6 * chars - b.len();
    let count = chars.div_ceil(NMEA_CHARS).max(1);
    for part in 0..count {
        let start = out.len();
        let _ = write!(out, "!AIVDM,{count},{},", part + 1);
        if count > 1 {
            let _ = write!(out, "{seq}");
        }
        let _ = write!(out, ",{channel},");
        let chunk = part * NMEA_CHARS..((part + 1) * NMEA_CHARS).min(chars);
        for k in chunk {
            out.push(armor(b.u(6 * k, 6) as u8));
        }
        let _ = write!(out, ",{}", if part + 1 == count { fill } else { 0 });
        let sum = out.as_bytes()[start + 1..].iter().fold(0u8, |a, &c| a ^ c);
        let _ = writeln!(out, "*{sum:02X}");
    }
}

// ---- Demodulator -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Nmea,
}

/// The receiver behind one sampling phase: slicer, NRZI, HDLC.
struct Slicer {
    /// The slicer's centre (the discriminator's DC: the frequency offset).
    mid: f32,
    /// Signal level (mean distance of the training's decisions from the
    /// centre).
    level: f32,
    /// The last [`HISTORY`] soft decisions, newest at `hist_pos - 1`.
    hist: [f32; HISTORY],
    hist_pos: usize,
    /// Previous line bit (NRZI) and the last 24 data bits, newest lowest.
    prev: bool,
    reg: u32,
    /// Bits left with the centre held; 0 = hunting.
    locked: u32,
    deframer: Deframer,
}

impl Slicer {
    fn new() -> Self {
        Self {
            mid: 0.0,
            level: 1.0,
            hist: [0.0; HISTORY],
            hist_pos: 0,
            prev: false,
            reg: 0,
            locked: 0,
            deframer: Deframer::new(true, MAX_FRAME),
        }
    }

    /// The soft decision `k` bits ago.
    fn back(&self, k: usize) -> f32 {
        self.hist[(self.hist_pos + HISTORY - 1 - k) % HISTORY]
    }

    /// Mean of the soft decisions from `from` to `to` bits ago.
    fn mean(&self, from: usize, to: usize) -> f32 {
        (to..from).map(|k| self.back(k)).sum::<f32>() / (from - to) as f32
    }

    /// One soft decision (the matched filter's output in the bit's middle).
    /// Returns the length of a frame that just ended, if any.
    #[inline]
    fn bit(&mut self, d: f32) -> Option<usize> {
        let lim = CLIP * self.level;
        self.hist[self.hist_pos] = d.clamp(self.mid - lim, self.mid + lim);
        self.hist_pos = (self.hist_pos + 1) % HISTORY;
        let line = d > self.mid;
        let bit = line == self.prev;
        self.prev = line;
        self.reg = (self.reg << 1 | u32::from(bit)) & 0xFF_FFFF;
        if let Some(n) = self.deframer.bit(u8::from(line), MIN_FRAME) {
            self.locked = 0;
            return Some(n);
        }
        let mask = (1 << TRAINING) - 1;
        let training = self.reg & 0xFF == 0x7E
            && [0x5555 & mask, 0xAAAA & mask]
                .iter()
                .any(|t| ((self.reg >> 8) & mask ^ t).count_ones() <= TRAINING_ERRORS);
        if training {
            // Training sequence and start flag (also while locked: that lock
            // may have been noise). The training's line levels (1100…)
            // average to the centre; hold it for the burst.
            self.mid = self.mean(8 + TRAINING, 8);
            let spread = (8..8 + TRAINING)
                .map(|k| (self.back(k) - self.mid).abs())
                .sum::<f32>()
                / TRAINING as f32;
            self.level = 0.5 * (self.level + spread.max(1e-6));
            self.locked = 8 * MAX_FRAME as u32 + 64;
        } else if self.locked > 0 {
            // Seven 1s: an abort, or noise.
            self.locked = if self.reg & 0x7F == 0x7F {
                0
            } else {
                self.locked - 1
            };
        } else {
            // Hunting: follow the mean of the last few bits.
            self.mid = self.mean(HUNT_MEAN, 0);
        }
        None
    }
}

pub struct AisDecoder {
    channel: String,
    format: Format,
    rate: f64,
    /// Matched filter: the last bit's samples and their sum.
    ring: Vec<f32>,
    pos: usize,
    sum: f64,
    scale: f32,
    /// Its previous output.
    prev_y: f32,
    /// Bits per sample, and the bit clock (bits since the start, fraction).
    step: f64,
    clock: f64,
    slicers: Vec<Slicer>,
    /// The last message sent (its bytes and when), so that the same burst
    /// decoded at two sampling phases goes out once.
    last: [u8; MAX_FRAME],
    last_len: usize,
    last_at: u64,
    samples: u64,
    /// NMEA multi-sentence message number (0–9).
    seq: u8,
    text: String,
    queue: MsgQueue,
}

impl AisDecoder {
    pub fn new(channel: &str, format: &str) -> Self {
        Self {
            channel: channel.to_owned(),
            format: if format == "nmea" {
                Format::Nmea
            } else {
                Format::Text
            },
            rate: 0.0,
            ring: Vec::new(),
            pos: 0,
            sum: 0.0,
            scale: 1.0,
            prev_y: 0.0,
            step: 0.0,
            clock: 0.0,
            slicers: (0..PHASES).map(|_| Slicer::new()).collect(),
            last: [0; MAX_FRAME],
            last_len: 0,
            last_at: 0,
            samples: 0,
            seq: 0,
            text: String::with_capacity(1024),
            queue: MsgQueue::new(),
        }
    }

    /// One input sample. Returns true when a message was queued.
    #[inline]
    fn sample(&mut self, x: f32) -> bool {
        self.samples += 1;
        let old = std::mem::replace(&mut self.ring[self.pos], x);
        self.pos += 1;
        if self.pos == self.ring.len() {
            self.pos = 0;
        }
        self.sum += f64::from(x) - f64::from(old);
        let y = self.sum as f32 * self.scale;
        let prev_y = std::mem::replace(&mut self.prev_y, y);
        let before = self.clock;
        self.clock += self.step;
        if self.clock >= 1.0 {
            self.clock -= 1.0;
        }
        // Each phase decides once per bit, at clock = k / PHASES; the value
        // is interpolated between this sample and the previous one.
        let mut queued = false;
        for k in 0..PHASES {
            let at = k as f64 / PHASES as f64;
            let past = if self.clock >= before {
                before < at && at <= self.clock
            } else {
                before < at || at <= self.clock
            };
            if !past {
                continue;
            }
            let late = (self.clock - at).rem_euclid(1.0) / self.step;
            let d = y - late as f32 * (y - prev_y);
            if let Some(n) = self.slicers[k].bit(d) {
                queued |= self.frame(k, n);
            }
        }
        queued
    }

    /// Phase `k`'s receiver found a frame of `n` bytes: check it and queue
    /// the message.
    fn frame(&mut self, k: usize, n: usize) -> bool {
        let frame = self.slicers[k].deframer.frame(n);
        if !fcs_ok(frame) {
            return false;
        }
        // Each byte came least significant bit first; the message is read
        // most significant first.
        let mut data = [0u8; MAX_FRAME];
        for (d, &f) in data.iter_mut().zip(&frame[..n - 2]) {
            *d = f.reverse_bits();
        }
        let len = n - 2;
        let data = &data[..len];
        // The same burst from a neighbouring phase ends within a bit.
        let window = 2 * self.ring.len() as u64;
        if self.samples - self.last_at <= window && &self.last[..self.last_len] == data {
            return false;
        }
        self.last[..len].copy_from_slice(data);
        (self.last_len, self.last_at) = (len, self.samples);
        let out = &mut self.text;
        out.clear();
        match self.format {
            Format::Text => {
                let _ = write!(
                    out,
                    "t={:.3} ch={} ",
                    self.samples as f64 / self.rate,
                    self.channel
                );
                describe(data, 8 * len, out);
                out.push('\n');
            }
            Format::Nmea => {
                nmea(data, 8 * len, &self.channel, self.seq, out);
                if 8 * len > 6 * NMEA_CHARS {
                    self.seq = (self.seq + 1) % 10;
                }
            }
        }
        // Several phases may finish on one sample: only one message goes in
        // the queue at a time, the others are already duplicates.
        if !self.queue.is_empty() {
            return false;
        }
        self.queue.bytes(self.text.as_bytes());
        true
    }
}

impl Block for AisDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if rate < 3.0 * BAUD {
            return Err(Error::Other(format!(
                "needs at least 28.8 kS/s (three samples per 9600 Bd bit) but gets {}; \
                 e.g. 48 kS/s",
                fmt_rate(rate)
            )));
        }
        let sps = rate / BAUD;
        let len = ((MF_BITS * sps).round() as usize).max(1);
        self.rate = rate;
        self.ring = vec![0.0; len];
        self.pos = 0;
        self.sum = 0.0;
        self.scale = 1.0 / len as f32;
        self.step = 1.0 / sps;
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.ring.is_empty() {
            return Err(not_configured());
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
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(AisDecoder::new(p.str("channel")?, p.str("format")?)),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armour_round_trips() {
        let p = "15M67FC000G?ufbE`FepT@3n00Sa";
        let (data, len) = dearmor(p, 0).unwrap();
        assert_eq!(len, 168);
        let mut s = String::new();
        nmea(&data, len, "B", 0, &mut s);
        assert_eq!(s, "!AIVDM,1,1,,B,15M67FC000G?ufbE`FepT@3n00Sa,0*5C\n");
        assert!(dearmor("1x", 0).is_none());
    }

    #[test]
    fn signed_fields() {
        let b = Bits::new(&[0xFF, 0x00], 16);
        assert_eq!(b.i(0, 8), -1);
        assert_eq!(b.u(4, 8), 0xF0);
        assert_eq!(b.i(4, 8), -16);
        assert_eq!(b.u(12, 8), 0, "past the end reads 0");
    }
}
