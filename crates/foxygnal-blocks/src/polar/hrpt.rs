//! Meteor-M HRPT (1700 MHz): the link, receive and transmit side.
//!
//! **The link.** BPSK carrying 665.4 kbit/s Manchester-coded (a 1 is the
//! chips 1 0, a 0 is 0 1), so 1.3308 Mchip/s. The bits are 1024-byte frames
//! starting with the ASM `1A CF FC 1D`, with neither randomizer nor forward
//! error correction. Of each frame, bytes 22–259, 278–515, 534–771 and
//! 790–1023 (counted from the ASM) carry the MSU-MR imager's stream, 948
//! bytes a frame; the rest is the frame header and other instruments.
//!
//! **Receiving.** The front end ([`super::frontend`]) runs at the chip rate
//! as BPSK; a bit is the difference of two chips. Which chip starts a bit is
//! unknown, so both pairings go to a CCSDS [`Deframer`] each (uncoded, no
//! RS, no randomizer) and the one that finds the ASM wins; its BPSK search
//! also resolves the carrier's half-turn. Frames whose ASM was missed while
//! locked (a flywheel frame: without FEC, most of its bits are still right)
//! go out with the inverted ASM, so the decoder keeps the stream aligned.
//!
//! `hrpt_demodulator` is that chain: baseband in, frames out (ASM + 1020
//! bytes, the `.cadu` layout), for `msumr_decoder`. `hrpt_encoder`
//! (synthetic MSU-MR lines → frames) and `hrpt_modulator` (frames →
//! baseband) are the transmit side, for the loopback template and the
//! tests.

use std::time::Duration;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Resampler, taps};
use serde_json::json;

use super::avhrr::avhrr_pattern;
use super::frontend::{CHUNK, FrontEnd, Psk};
use crate::ccsds::sync::Modulation;
use crate::ccsds::wire::{ASM_BAD, ASM_BYTES, CaduReader};
use crate::ccsds::{Config, Deframer, Encoder, Frame};
use crate::design;
use crate::digital::{MsgQueue, text_meta};
use crate::dvb::ByteFifo;
use crate::filters::not_configured;

/// Bits and chips a second.
pub const BIT_RATE: f64 = 665_400.0;
pub const CHIP_RATE: f64 = 2.0 * BIT_RATE;
/// Bytes after the ASM.
pub const FRAME_LEN: usize = 1020;
/// The MSU-MR stream's parts of a frame, as ranges of the bytes after the
/// ASM.
pub const MSUMR_PARTS: [(usize, usize); 4] = [(18, 256), (274, 512), (530, 768), (786, 1020)];
/// MSU-MR bytes a frame.
pub const MSUMR_PER_FRAME: usize = 948;
/// The shaping filter's roll-off (not confirmed on air).
const ROLLOFF: f64 = 0.7;

/// The deframer's configuration for the bits.
pub fn deframer_config() -> Config {
    Config {
        modulation: Modulation::Bpsk,
        coded: false,
        conv_invert: false,
        nrzm: false,
        randomized: false,
        rs: None,
        frame_len: FRAME_LEN,
    }
}

// ---- Descriptors ---------------------------------------------------------------

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "hrpt_demodulator",
            label: "Meteor HRPT Demodulator",
            category: "Satellite",
            help: "Meteor-M HRPT (1700 MHz): baseband in (at least 2 samples a chip at \
                   1.3308 Mchip/s: 2.67 MS/s or more), frames out (ASM + 1020 bytes) for the \
                   Meteor HRPT Decoder. Matched filter, timing and carrier recovery for BPSK \
                   at the chip rate, a frequency search over ± 665 kHz, Manchester decoding \
                   (both chip pairings tried), then the frame sync: no randomizer, no FEC. \
                   Frames whose sync marker was missed while locked go out with the \
                   inverted marker. `offset` is where the carrier is expected (Hz): wire a \
                   Doppler source to it. `status` has a line per second and per lock \
                   change.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![
                port("frames", StreamType::U8),
                port("status", StreamType::U8),
            ],
            params: vec![ParamSpec {
                name: "offset",
                label: "Carrier offset",
                kind: ParamKind::Float {
                    min: None,
                    max: None,
                    unit: Some("Hz"),
                },
                default: json!(0.0),
                help: "Where the carrier is expected relative to the input's centre (tuning \
                       error, Doppler); the search covers ± 665 kHz around it. Live, and can \
                       be wired (e.g. to a Satellite Tracker's Doppler).",
                live: true,
            }],
        },
        build_demodulator,
    );
    r.set_keywords(
        "hrpt_demodulator",
        &[
            "meteor",
            "hrpt",
            "msu-mr",
            "bpsk",
            "manchester",
            "weather satellite",
            "l-band",
        ],
    );

    r.register(
        BlockDescriptor {
            kind: "hrpt_encoder",
            label: "Meteor HRPT Encoder",
            category: "Satellite",
            help: "A test transmitter's data: synthetic MSU-MR lines (six channels of a \
                   smooth pattern, the same the tests compare against) in the MSU-MR part of \
                   HRPT frames, out as frames (ASM + 1020 bytes) for the HRPT Modulator. \
                   The frames fill the link, as on air.",
            inputs: vec![],
            outputs: vec![port("out", StreamType::U8)],
            params: vec![design::int(
                "lines",
                "Lines",
                0,
                0,
                "MSU-MR lines to send, then stop; 0 = without end.",
            )],
        },
        build_encoder,
    );
    r.set_keywords(
        "hrpt_encoder",
        &["meteor", "hrpt", "msu-mr", "loopback", "test"],
    );

    r.register(
        BlockDescriptor {
            kind: "hrpt_modulator",
            label: "Meteor HRPT Modulator",
            category: "Satellite",
            help: "The transmit side of the HRPT Demodulator, for loopback tests: frames \
                   in, Manchester-coded BPSK at 1.3308 Mchip/s shaped with an RRC \
                   (roll-off 0.7) at `sps` samples a chip.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: vec![design::int_range(
                "sps",
                "Samples/chip",
                2,
                2,
                16,
                "Output samples per chip.",
            )],
        },
        build_modulator,
    );
    r.set_keywords(
        "hrpt_modulator",
        &["meteor", "hrpt", "manchester", "loopback", "test"],
    );
}

// ---- Demodulator ---------------------------------------------------------------

/// Status lines are dropped rather than queued beyond this.
const MSG_BACKLOG: usize = 4096;
/// Soft bit for a chip difference of 2 (unit chips).
const SOFT_SCALE: f32 = 40.0;

fn write_frame(fifo: &mut ByteFifo, f: &Frame<'_>) {
    fifo.write(if f.ok() { &ASM_BYTES } else { &ASM_BAD });
    fifo.write(f.data);
}

struct DemodState {
    fe: FrontEnd,
    /// One deframer per chip pairing; the one in use once locked.
    deframers: [Deframer; 2],
    active: Option<usize>,
    /// The last chip and chips seen (for the pairings).
    last: f32,
    chips: u64,
    syms: Vec<Complex32>,
    soft: [Vec<[i8; 2]>; 2],
    fifo: ByteFifo,
    samples: u64,
    next_report: u64,
    last_locked: bool,
    last_frames: u64,
}

impl DemodState {
    fn locked(&self) -> bool {
        self.active.is_some_and(|i| self.deframers[i].is_locked())
    }

    fn process(&mut self, input: &[Complex32]) -> usize {
        self.syms.clear();
        let jumps = self.fe.jumps;
        self.fe.set_locked(self.locked());
        let used = self.fe.process(input, &mut self.syms);
        self.samples += used as u64;
        if self.fe.jumps != jumps && !self.locked() {
            for d in &mut self.deframers {
                d.reset();
            }
        }
        if !self.fe.settled() && !self.locked() {
            return used;
        }
        // Bits from both chip pairings.
        for s in &mut self.soft {
            s.clear();
        }
        for z in &self.syms {
            let c = z.re;
            if self.chips > 0 {
                let v = ((self.last - c) * 0.5 * SOFT_SCALE)
                    .round()
                    .clamp(-127.0, 127.0) as i8;
                self.soft[(self.chips & 1) as usize ^ 1].push([v, 0]);
            }
            self.last = c;
            self.chips += 1;
        }
        let fifo = &mut self.fifo;
        let mut emit = |f: Frame<'_>| write_frame(fifo, &f);
        match self.active.filter(|&i| self.deframers[i].is_locked()) {
            Some(i) => self.deframers[i].push_soft(&self.soft[i], &mut emit),
            None => {
                self.active = None;
                for i in 0..2 {
                    self.deframers[i].push_soft(&self.soft[i], &mut emit);
                    if self.deframers[i].is_locked() {
                        self.active = Some(i);
                        self.deframers[1 - i].reset();
                        break;
                    }
                }
            }
        }
        used
    }
}

pub struct HrptDemodulator {
    offset: f64,
    st: Option<Box<DemodState>>,
    msgs: MsgQueue,
}

impl HrptDemodulator {
    fn report(st: &mut DemodState, msgs: &mut MsgQueue) {
        let t = st.samples as f64 / st.fe.rate();
        let locked = st.locked();
        if locked != st.last_locked && msgs.pending() < MSG_BACKLOG {
            if locked {
                let i = st.active.unwrap_or(0);
                msgs.line(format_args!(
                    "t={t:.3} event=lock pairing={i} inverted={} freq_hz={:+.0}",
                    u8::from(st.deframers[i].stats().symmetry.quarter != 0),
                    st.fe.offset_hz()
                ));
            } else {
                msgs.line(format_args!("t={t:.3} event=unlock"));
            }
        }
        st.last_locked = locked;
        if st.samples < st.next_report {
            return;
        }
        st.next_report += st.fe.rate().round().max(1.0) as u64;
        let mer = st.fe.take_mer();
        let (a, b) = (st.deframers[0].stats(), st.deframers[1].stats());
        let (frames, bad) = (a.frames + b.frames, a.bad_frames + b.bad_frames);
        let fps = frames - st.last_frames;
        st.last_frames = frames;
        if msgs.pending() >= MSG_BACKLOG {
            return;
        }
        let state = if locked { "lock" } else { "search" };
        msgs.text(format_args!(
            "t={t:.3} state={state} freq_hz={:+.0}",
            st.fe.offset_hz()
        ));
        if let Some(m) = mer {
            msgs.text(format_args!(" mer_db={m:.1}"));
        }
        msgs.line(format_args!(" frames={frames} missed_sync={bad} fps={fps}"));
    }
}

/// Frame FIFO size and the room kept free before taking more input.
const FIFO_LEN: usize = 1 << 16;
const FIFO_MARGIN: usize = 1 << 14;

impl Block for HrptDemodulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if rate < 2.0 * CHIP_RATE {
            return Err(Error::Other(format!(
                "the input rate {} is less than 2 samples per chip at {}; the \
                 demodulator needs at least 2 (2.67 MS/s)",
                design::fmt_rate(rate),
                design::fmt_rate(CHIP_RATE)
            )));
        }
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = Some(BIT_RATE / 8.0);
            o.center_freq = None;
        }
        text_meta(cx, 1);
        let mut fe = FrontEnd::new(rate, CHIP_RATE, Psk::Bpsk, ROLLOFF);
        fe.set_offset(self.offset);
        let d = || Deframer::new(deframer_config()).map_err(Error::Other);
        self.st = Some(Box::new(DemodState {
            fe,
            deframers: [d()?, d()?],
            active: None,
            last: 0.0,
            chips: 0,
            syms: Vec::with_capacity(CHUNK + 8),
            soft: [
                Vec::with_capacity(CHUNK / 2 + 8),
                Vec::with_capacity(CHUNK / 2 + 8),
            ],
            fifo: ByteFifo::new(FIFO_LEN),
            samples: 0,
            next_report: rate.round().max(1.0) as u64,
            last_locked: false,
            last_frames: 0,
        }));
        Ok(())
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "offset" => {
                let v = value.as_f64().unwrap_or_default();
                self.offset = v;
                if let Some(st) = &mut self.st {
                    st.fe.set_offset(v);
                }
                Ok(())
            }
            _ => Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let st = self.st.as_mut().ok_or_else(not_configured)?;
        let (mut used, mut made, mut msg_made) = (0, 0, 0);
        loop {
            made += st
                .fifo
                .read_into(&mut io.outputs[0].slice_mut::<u8>()[made..]);
            msg_made += self
                .msgs
                .flush(&mut io.outputs[1].slice_mut::<u8>()[msg_made..]);
            let inp = io.inputs[0].slice::<Complex32>();
            if used == inp.len() || st.fifo.free() < FIFO_MARGIN {
                break;
            }
            used += st.process(&inp[used..]);
            Self::report(st, &mut self.msgs);
        }
        if used == 0 && made == 0 && msg_made == 0 {
            let waiting = !st.fifo.is_empty() || !self.msgs.is_empty();
            if waiting && io.inputs[0].exhausted() && !io.outputs[0].abandoned() {
                return Ok(WorkStatus::Wait(Duration::from_millis(20)));
            }
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        io.produce(1, msg_made);
        Ok(WorkStatus::Ok)
    }
}

fn build_demodulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(HrptDemodulator {
            offset: p.f64("offset")?,
            st: None,
            msgs: MsgQueue::new(),
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![
            ("frames".into(), StreamType::U8),
            ("status".into(), StreamType::U8),
        ],
    })
}

// ---- Encoder: synthetic MSU-MR lines → frames ------------------------------------

/// Bytes of an MSU-MR line, and its sync word.
pub const LINE_LEN: usize = 11_850;
pub const LINE_SYNC: [u8; 8] = [0x02, 0x18, 0xA7, 0xA3, 0x92, 0xDD, 0x9A, 0xBF];
/// Pixels a line, and where the video starts (groups of 4 pixels × 6
/// channels × 5 bytes).
pub const WIDTH: usize = 1572;
pub const VIDEO_AT: usize = 50;
/// MSU-MR lines a second (the stream's rate over the line's length).
pub const LINES_PER_SECOND: f64 =
    BIT_RATE / 8.0 / (4 + FRAME_LEN) as f64 * MSUMR_PER_FRAME as f64 / LINE_LEN as f64;
/// Seconds of the (Moscow) day of the encoder's first line.
pub const TEST_START_S: f64 = 36_000.0;

/// Pack 4 ten-bit values into 5 bytes, MSB first.
pub fn pack4(v: [u16; 4], out: &mut [u8]) {
    let w = v
        .iter()
        .fold(0u64, |a, &x| (a << 10) | u64::from(x & 0x3FF));
    out[..5].copy_from_slice(&w.to_be_bytes()[3..]);
}

/// Unpack 5 bytes into 4 ten-bit values.
pub fn unpack4(b: &[u8]) -> [u16; 4] {
    let w = b[..5].iter().fold(0u64, |a, &x| (a << 8) | u64::from(x));
    std::array::from_fn(|k| ((w >> (30 - 10 * k)) & 0x3FF) as u16)
}

/// Write MSU-MR line `n` of the encoder's pattern into `line` (11 850
/// bytes).
pub fn test_line(n: u64, line: &mut [u8]) {
    line.fill(0);
    line[..8].copy_from_slice(&LINE_SYNC);
    let t = TEST_START_S + n as f64 / LINES_PER_SECOND;
    let s = t.floor() as u32;
    line[8] = (s / 3600 % 24) as u8;
    line[9] = (s / 60 % 60) as u8;
    line[10] = (s % 60) as u8;
    line[11] = ((t - t.floor()) * 256.0) as u8;
    for g in 0..WIDTH / 4 {
        for c in 0..6 {
            let at = VIDEO_AT + 30 * g + 5 * c;
            let v = std::array::from_fn(|k| avhrr_pattern(c, 4 * g + k, n as usize));
            pack4(v, &mut line[at..at + 5]);
        }
    }
}

/// Builds frames carrying synthetic MSU-MR lines.
pub struct HrptSource {
    line: u64,
    buf: Vec<u8>,
    /// Bytes of `buf` still to send.
    pos: usize,
    frames: u32,
}

impl Default for HrptSource {
    fn default() -> Self {
        Self::new()
    }
}

impl HrptSource {
    pub fn new() -> Self {
        Self {
            line: 0,
            buf: vec![0; LINE_LEN],
            pos: LINE_LEN,
            frames: 0,
        }
    }

    pub fn lines(&self) -> u64 {
        self.line
    }

    /// The next frame (1020 bytes after the ASM) into `frame`; false once
    /// `limit` lines (0: no limit) have gone out.
    pub fn frame(&mut self, limit: u64, frame: &mut [u8]) -> bool {
        if limit > 0 && self.line >= limit && self.pos == LINE_LEN {
            return false;
        }
        frame.fill(0);
        frame[..4].copy_from_slice(&self.frames.to_be_bytes());
        self.frames = self.frames.wrapping_add(1);
        for &(a, b) in &MSUMR_PARTS {
            let mut at = a;
            while at < b {
                if self.pos == LINE_LEN {
                    if limit > 0 && self.line >= limit {
                        // The stream ends: zeros for the rest.
                        break;
                    }
                    test_line(self.line, &mut self.buf);
                    self.line += 1;
                    self.pos = 0;
                }
                let n = (b - at).min(LINE_LEN - self.pos);
                frame[at..at + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                at += n;
            }
        }
        true
    }
}

pub struct EncoderBlock {
    src: HrptSource,
    limit: u64,
    frame: Vec<u8>,
    fifo: ByteFifo,
    done: bool,
}

const ENC_FIFO: usize = 1 << 16;

impl Block for EncoderBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = None;
            o.center_freq = None;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut made = 0;
        loop {
            made += self
                .fifo
                .read_into(&mut io.outputs[0].slice_mut::<u8>()[made..]);
            if self.fifo.free() < 4 + FRAME_LEN {
                break;
            }
            if self.done || !self.src.frame(self.limit, &mut self.frame) {
                self.done = true;
                if self.fifo.is_empty() {
                    return Ok(WorkStatus::Done);
                }
                break;
            }
            self.fifo.write(&ASM_BYTES);
            self.fifo.write(&self.frame);
        }
        if made == 0 {
            if io.outputs[0].abandoned() {
                return Ok(WorkStatus::Done);
            }
            return Ok(WorkStatus::Blocked);
        }
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_encoder(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(EncoderBlock {
            src: HrptSource::new(),
            limit: p.u64("lines")?,
            frame: vec![0; FRAME_LEN],
            fifo: ByteFifo::new(ENC_FIFO),
            done: false,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Modulator ------------------------------------------------------------------

/// Frames → shaped baseband.
pub struct HrptTransmitter {
    enc: Encoder,
    chips: Vec<Complex32>,
    shaper: Resampler<Complex32>,
    sps: usize,
}

impl HrptTransmitter {
    pub fn new(sps: usize) -> Self {
        let h = taps::rrc(sps as f64, ROLLOFF, 12, sps as f64);
        Self {
            enc: Encoder::new(deframer_config()).expect("valid config"),
            chips: Vec::with_capacity(2 * 8 * (4 + FRAME_LEN) + 8),
            shaper: Resampler::new(sps, 1, &h),
            sps,
        }
    }

    /// One frame (1020 bytes after the ASM) → samples appended to `out`.
    pub fn frame(&mut self, frame: &[u8], out: &mut Vec<Complex32>) {
        self.chips.clear();
        let chips = &mut self.chips;
        // The encoder sends 0 as +1: a 0 bit is the chips 0 1 (+1, −1), a 1
        // bit 1 0 (−1, +1).
        self.enc.encode(frame, |s| {
            chips.push(Complex32::new(s.re, 0.0));
            chips.push(Complex32::new(-s.re, 0.0));
        });
        let start = out.len();
        out.resize(start + self.chips.len() * self.sps, Complex32::default());
        let (_, made) = self.shaper.process(&self.chips, &mut out[start..]);
        out.truncate(start + made);
    }
}

pub struct ModulatorBlock {
    tx: HrptTransmitter,
    reader: CaduReader,
    out: Vec<Complex32>,
    pos: usize,
}

impl Block for ModulatorBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = Some(CHIP_RATE * self.tx.sps as f64);
            o.center_freq = None;
        }
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        loop {
            if self.pos < self.out.len() {
                let o = &mut io.outputs[0].slice_mut::<Complex32>()[made..];
                let n = o.len().min(self.out.len() - self.pos);
                o[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
                self.pos += n;
                made += n;
                if self.pos < self.out.len() {
                    break;
                }
                continue;
            }
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.is_empty() {
                break;
            }
            self.out.clear();
            self.pos = 0;
            let (tx, out) = (&mut self.tx, &mut self.out);
            used += self.reader.push_one(inp, |c| tx.frame(c.data, out));
        }
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_modulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let sps = p.u64("sps")? as usize;
    if !(2..=16).contains(&sps) {
        return Err(Error::Other(format!(
            "samples per chip must be 2 to 16 (got {sps})"
        )));
    }
    Ok(Instance {
        block: Box::new(ModulatorBlock {
            out: Vec::with_capacity(sps * 16 * (4 + FRAME_LEN) + 64),
            tx: HrptTransmitter::new(sps),
            reader: CaduReader::new(FRAME_LEN),
            pos: 0,
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_bit_packing() {
        let mut b = [0u8; 5];
        pack4([0x3FF, 0x000, 0x2AA, 0x155], &mut b);
        assert_eq!(b, [0xFF, 0xC0, 0x0A, 0xA9, 0x55]);
        assert_eq!(unpack4(&b), [0x3FF, 0x000, 0x2AA, 0x155]);
    }

    #[test]
    fn parts_add_up() {
        let n: usize = MSUMR_PARTS.iter().map(|(a, b)| b - a).sum();
        assert_eq!(n, MSUMR_PER_FRAME);
        assert!((LINES_PER_SECOND - 6.5).abs() < 0.01, "{LINES_PER_SECOND}");
    }

    #[test]
    fn frames_carry_the_lines_back_to_back() {
        let mut src = HrptSource::new();
        let mut frame = vec![0; FRAME_LEN];
        let mut stream = Vec::new();
        while src.frame(3, &mut frame) {
            for &(a, b) in &MSUMR_PARTS {
                stream.extend_from_slice(&frame[a..b]);
            }
        }
        let mut line = vec![0; LINE_LEN];
        for n in 0..3 {
            test_line(n, &mut line);
            assert_eq!(&stream[n as usize * LINE_LEN..][..LINE_LEN], &line[..]);
        }
    }
}
