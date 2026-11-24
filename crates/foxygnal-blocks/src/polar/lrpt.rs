//! Meteor-M LRPT (137.1 / 137.9 MHz): the link, receive and transmit side.
//!
//! **The link.** CCSDS-like: 1024-byte CADUs (the ASM, then a 1020-byte
//! codeblock: VCDU with a 2-byte insert zone, RS(255,223) I = 4 in the
//! conventional basis, randomized), NRZ-M on the data bits (Meteor-M N2-3
//! and later), then the r = 1/2, K = 7 code without CCSDS's inversion of
//! G2, G1 on I and G2 on Q. Three modes:
//!
//! | mode | modulation | symbols/s | NRZ-M | interleaved |
//! |---|---|---|---|---|
//! | `oqpsk72` (N2-3, N2-4) | offset QPSK | 72 000 | yes | no |
//! | `oqpsk80` (test periods) | offset QPSK | 80 000 | yes | yes |
//! | `qpsk72` (Meteor-M N2) | QPSK | 72 000 | no | no |
//!
//! **The 80k mode** interleaves the coded bits with a convolutional
//! interleaver of 36 branches, branch k delayed by k × 2048 commutator
//! cycles (the commutator moves on every bit), and inserts the marker 0x27
//! before every 72 coded bits (80 bits a period, which the higher symbol
//! rate pays for). The receiver finds the marker under every phase symmetry
//! of the carrier (and both OQPSK pairings), strips it, deinterleaves (16 s
//! of latency, 2.6 M soft bits of memory) and hands the bits on.
//!
//! `lrpt_demodulator` owns a PSK front end ([`super::frontend`]) and the
//! CCSDS [`Deframer`]: baseband in, CADUs out, ready for `ccsds_packets`
//! (insert zone 2) and `lrpt_decoder`. `lrpt_encoder` (synthetic MSU-MR
//! imagery → CADUs) and `lrpt_modulator` (CADUs → baseband) are the
//! transmit side, for the loopback template and the tests.

use std::time::Duration;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Resampler, taps};
use serde_json::json;

use super::frontend::{CHUNK, FrontEnd, OqpskPairs, Psk};
use super::jpeg;
use super::lrpt_image::{APID_FIRST, APID_TLM, PACKETS_PER_SCAN};
use crate::ccsds::sync::{Modulation, Symmetry};
use crate::ccsds::vcdu::{FrameBuilder, Layout, PacketHeader, VCID_FILL, VcduHeader};
use crate::ccsds::wire::{ASM_BYTES, CaduReader};
use crate::ccsds::{Config, Deframer, Encoder, Frame};
use crate::design;
use crate::digital::{MsgQueue, text_meta};
use crate::dvb::ByteFifo;
use crate::filters::not_configured;

/// Bytes after the ASM, and of the transfer frame within them.
pub const FRAME_LEN: usize = 1020;
pub const VCDU_LEN: usize = 892;
/// The imagery's virtual channel.
pub const VCID: u8 = 5;
/// RRC roll-off of the matched filter (and the test transmitter).
const ROLLOFF: f64 = 0.6;

/// An LRPT transmission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Oqpsk72,
    Oqpsk80,
    Qpsk72,
}

impl Mode {
    pub const NAMES: [&'static str; 3] = ["oqpsk72", "oqpsk80", "qpsk72"];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "oqpsk72" => Some(Self::Oqpsk72),
            "oqpsk80" => Some(Self::Oqpsk80),
            "qpsk72" => Some(Self::Qpsk72),
            _ => None,
        }
    }

    pub fn symbol_rate(self) -> f64 {
        if self == Self::Oqpsk80 {
            80_000.0
        } else {
            72_000.0
        }
    }

    pub fn psk(self) -> Psk {
        if self == Self::Qpsk72 {
            Psk::Qpsk
        } else {
            Psk::Oqpsk
        }
    }

    pub fn interleaved(self) -> bool {
        self == Self::Oqpsk80
    }

    /// The deframer's configuration.
    pub fn config(self) -> Config {
        Config {
            modulation: Modulation::Qpsk,
            coded: true,
            conv_invert: false,
            nrzm: self != Self::Qpsk72,
            randomized: true,
            rs: Some((4, false)),
            frame_len: FRAME_LEN,
        }
    }
}

// ---- The 80k interleaver ---------------------------------------------------------

/// The marker before every 72 coded bits in the 80k mode.
pub const MARKER: u8 = 0x27;
pub const PERIOD: usize = 80;
const BRANCHES: usize = 36;
const DELAY: usize = 2048;
const SPAN: usize = BRANCHES * DELAY;
const DEPTH: usize = (BRANCHES - 1) * SPAN + 1;

/// The convolutional (de)interleaver: bit j goes through branch j mod 36,
/// delayed by k × 2048 × 36 bits (interleaver) or (35 − k) × 2048 × 36 bits
/// (deinterleaver), counting j from the first data bit after a marker.
pub struct ConvInterleaver {
    buf: Vec<i8>,
    w: usize,
    branch: usize,
    deinterleave: bool,
}

impl ConvInterleaver {
    pub fn new(deinterleave: bool) -> Self {
        Self {
            buf: vec![0; DEPTH],
            w: 0,
            branch: 0,
            deinterleave,
        }
    }

    /// Back to branch 0 with an empty memory.
    pub fn reset(&mut self) {
        self.buf.fill(0);
        self.w = 0;
        self.branch = 0;
    }

    /// End-to-end delay of interleaver and deinterleaver, in bits.
    pub const LATENCY: usize = (BRANCHES - 1) * SPAN;

    #[inline]
    pub fn push(&mut self, x: i8) -> i8 {
        let k = if self.deinterleave {
            BRANCHES - 1 - self.branch
        } else {
            self.branch
        };
        self.buf[self.w] = x;
        let mut r = self.w + DEPTH - k * SPAN;
        if r >= DEPTH {
            r -= DEPTH;
        }
        let y = self.buf[r];
        self.w += 1;
        if self.w == DEPTH {
            self.w = 0;
        }
        self.branch += 1;
        if self.branch == BRANCHES {
            self.branch = 0;
        }
        y
    }
}

/// Symbols collected for a marker search (40 a period).
const MARKER_SEARCH: usize = 48 * PERIOD / 2;
/// Wrong marker bits a period may have, and bad periods in a row before the
/// marker is lost.
const MARKER_ERRORS: u32 = 3;
const MARKER_MISSES: u32 = 8;

/// The receive side of the 80k mode: marker search and removal, then the
/// deinterleaver; soft QPSK symbols in (both OQPSK pairings), soft QPSK
/// symbols out in transmit order.
struct Deinterleaver {
    acq: [Vec<[i8; 2]>; 2],
    locked: bool,
    pairing: usize,
    symmetry: Symmetry,
    /// Position in the 80-bit period.
    pos: usize,
    period_errors: u32,
    misses: u32,
    conv: ConvInterleaver,
    half: Option<i8>,
    /// The marker was lost, but the period and the deinterleaver are still
    /// counted (erasures for what isn't decoded), so a relock at the same
    /// place keeps the deinterleaver's 16 s.
    coast: bool,
    /// Marker locks so far.
    locks: u64,
}

impl Deinterleaver {
    fn new() -> Self {
        Self {
            acq: [
                Vec::with_capacity(MARKER_SEARCH),
                Vec::with_capacity(MARKER_SEARCH),
            ],
            locked: false,
            pairing: 0,
            symmetry: Symmetry::default(),
            pos: 0,
            period_errors: 0,
            misses: 0,
            conv: ConvInterleaver::new(true),
            half: None,
            coast: false,
            locks: 0,
        }
    }

    /// Symbols of both pairings in; deinterleaved symbols appended to `out`.
    fn push(&mut self, a: &[[i8; 2]], b: &[[i8; 2]], out: &mut Vec<[i8; 2]>) {
        if self.locked {
            let s = if self.pairing == 0 { a } else { b };
            self.track(s, out, false);
            return;
        }
        let mut taken = [0; 2];
        for ((acq, s), t) in self.acq.iter_mut().zip([a, b]).zip(&mut taken) {
            *t = s.len().min(MARKER_SEARCH - acq.len());
            acq.extend_from_slice(&s[..*t]);
        }
        if self.acq[0].len() < MARKER_SEARCH || self.acq[1].len() < MARKER_SEARCH - 1 {
            return;
        }
        let rest = |p: usize| {
            if p == 0 {
                &a[taken[0]..]
            } else {
                &b[taken[1]..]
            }
        };
        if let Some((pairing, symmetry, offset)) = self.search() {
            self.locked = true;
            self.locks += 1;
            // Start the period at the marker found, then go on live.
            let pos = (PERIOD - offset % PERIOD) % PERIOD;
            if !(self.coast && pairing == self.pairing && pos == self.pos) {
                self.conv.reset();
                self.half = None;
            }
            self.coast = false;
            self.pairing = pairing;
            self.symmetry = symmetry;
            self.misses = 0;
            self.period_errors = 0;
            self.pos = pos;
            let acq = std::mem::take(&mut self.acq[pairing]);
            self.track(&acq, out, false);
            self.acq[pairing] = acq;
            self.track(rest(pairing), out, false);
        } else if self.coast {
            // Count through what was searched, as erasures.
            let p = self.pairing;
            let acq = std::mem::take(&mut self.acq[p]);
            self.track(&acq, out, true);
            self.acq[p] = acq;
            self.track(rest(p), out, true);
        }
        self.acq[0].clear();
        self.acq[1].clear();
    }

    /// The best (pairing, symmetry, bit offset of a marker) if the marker is
    /// clearly there.
    fn search(&self) -> Option<(usize, Symmetry, usize)> {
        let mut best: Option<(u32, usize, Symmetry, usize)> = None;
        let mut bits = [0u8; 2 * MARKER_SEARCH];
        for (p, acq) in self.acq.iter().enumerate() {
            for &sym in Modulation::Qpsk.symmetries() {
                for (k, &s) in acq.iter().enumerate() {
                    let t = sym.apply(s);
                    bits[2 * k] = u8::from(t[0] < 0);
                    bits[2 * k + 1] = u8::from(t[1] < 0);
                }
                let n = 2 * acq.len();
                for off in 0..PERIOD {
                    let mut errors = 0;
                    let mut at = off;
                    while at + 8 <= n {
                        for i in 0..8 {
                            errors += u32::from(bits[at + i] != (MARKER >> (7 - i)) & 1);
                        }
                        at += PERIOD;
                    }
                    if best.is_none_or(|b| errors < b.0) {
                        best = Some((errors, p, sym, off));
                    }
                }
            }
        }
        let (errors, p, sym, off) = best?;
        let periods = (2 * MARKER_SEARCH / PERIOD - 1) as u32;
        (errors <= periods).then_some((p, sym, off))
    }

    /// Symbols through the marker check and the deinterleaver (`erase`:
    /// count through them, deinterleaving erasures).
    fn track(&mut self, syms: &[[i8; 2]], out: &mut Vec<[i8; 2]>, erase: bool) {
        for &s in syms {
            let t = if erase {
                [0, 0]
            } else {
                self.symmetry.apply(s)
            };
            for v in t {
                if self.pos < 8 {
                    let want = (MARKER >> (7 - self.pos)) & 1;
                    self.period_errors += u32::from(u8::from(v < 0) != want);
                } else {
                    let y = self.conv.push(v);
                    match self.half.take() {
                        None => self.half = Some(y),
                        Some(h) => out.push([h, y]),
                    }
                }
                self.pos += 1;
                if self.pos == PERIOD {
                    self.pos = 0;
                    if self.period_errors > MARKER_ERRORS {
                        self.misses += 1;
                    } else {
                        self.misses = 0;
                    }
                    self.period_errors = 0;
                }
            }
        }
        if !erase && self.misses >= MARKER_MISSES {
            self.locked = false;
            self.coast = true;
            self.acq[0].clear();
            self.acq[1].clear();
        }
    }
}

/// The transmit side of the 80k mode: coded bits in, marker and
/// interleaver, bits out.
pub struct Interleaver {
    conv: ConvInterleaver,
    pos: usize,
}

impl Interleaver {
    pub fn new() -> Self {
        Self {
            conv: ConvInterleaver::new(false),
            pos: 0,
        }
    }

    /// One coded bit in; the channel bits (marker first when one is due)
    /// out.
    pub fn push(&mut self, bit: u8, mut out: impl FnMut(u8)) {
        if self.pos == 0 {
            for i in 0..8 {
                out((MARKER >> (7 - i)) & 1);
            }
            self.pos = 8;
        }
        out(self.conv.push(bit as i8) as u8);
        self.pos += 1;
        if self.pos == PERIOD {
            self.pos = 0;
        }
    }
}

impl Default for Interleaver {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Demodulator ----------------------------------------------------------------

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

fn mode_param() -> ParamSpec {
    design::choice(
        "mode",
        "Mode",
        Mode::NAMES.to_vec(),
        "oqpsk72",
        "oqpsk72: offset QPSK at 72 kBd with NRZ-M, Meteor-M N2-3 and N2-4. oqpsk80: \
         80 kBd, interleaved (their test mode). qpsk72: plain QPSK at 72 kBd without NRZ-M \
         (Meteor-M N2).",
    )
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "lrpt_demodulator",
            label: "Meteor LRPT Demodulator",
            category: "Satellite",
            help: "Meteor-M LRPT (137.1 / 137.9 MHz): baseband in (at least 2 samples a \
                   symbol; 150–300 kS/s is plenty), CADUs out for CCSDS Packets (insert zone \
                   2) and the Meteor LRPT Decoder. Matched filter, timing and carrier \
                   recovery for QPSK or offset QPSK, a frequency search over ± a quarter of \
                   the symbol rate (± 18 kHz), the 80k mode's deinterleaver, then the CCSDS \
                   deframer: sync marker through every phase ambiguity, Viterbi (r = 1/2, \
                   K = 7), NRZ-M, derandomizer, RS(255,223) I = 4. `offset` is where the \
                   carrier is expected (Hz): wire a Doppler source to it. `status` has a \
                   line per second and per lock change.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![
                port("frames", StreamType::U8),
                port("status", StreamType::U8),
            ],
            params: vec![
                mode_param(),
                ParamSpec {
                    name: "offset",
                    label: "Carrier offset",
                    kind: ParamKind::Float {
                        min: None,
                        max: None,
                        unit: Some("Hz"),
                    },
                    default: json!(0.0),
                    help: "Where the carrier is expected relative to the input's centre \
                           (tuning error, Doppler); the search covers ± 18 kHz around it. \
                           Live, and can be wired (e.g. to a Satellite Tracker's Doppler).",
                    live: true,
                },
            ],
        },
        build_demodulator,
    );
    r.set_keywords(
        "lrpt_demodulator",
        &[
            "meteor",
            "lrpt",
            "oqpsk",
            "qpsk",
            "weather satellite",
            "137 mhz",
        ],
    );

    r.register(
        BlockDescriptor {
            kind: "lrpt_encoder",
            label: "Meteor LRPT Encoder",
            category: "Satellite",
            help: "A test transmitter's data: a synthetic MSU-MR picture (a smooth pattern \
                   per channel, the same the tests compare against), JPEG-coded into image \
                   packets (APIDs 64–69, 14 per scan and channel) and a telemetry packet \
                   (APID 70) per scan, in transfer frames of virtual channel 5, out as \
                   CADUs with room for the Reed-Solomon parity (LRPT Modulator fills it \
                   in).",
            inputs: vec![],
            outputs: vec![port("out", StreamType::U8)],
            params: vec![
                ParamSpec {
                    name: "channels",
                    label: "Channels",
                    kind: ParamKind::String,
                    default: json!("1,2,3"),
                    help: "MSU-MR channels sent (1–6), in transmit order.",
                    live: false,
                },
                design::int_range("quality", "Quality", 80, 1, 100, "JPEG quality factor."),
                design::int(
                    "scans",
                    "Scans",
                    0,
                    0,
                    "Scans (8 rows each) to send, then stop; 0 = without end.",
                ),
            ],
        },
        build_encoder,
    );
    r.set_keywords(
        "lrpt_encoder",
        &["meteor", "lrpt", "msu-mr", "loopback", "test"],
    );

    r.register(
        BlockDescriptor {
            kind: "lrpt_modulator",
            label: "Meteor LRPT Modulator",
            category: "Satellite",
            help: "The transmit side of the LRPT Demodulator, for loopback tests: CADUs in, \
                   RS parity (conventional basis), randomizer, NRZ-M, r = 1/2 K = 7 code, \
                   the 80k mode's interleaver, QPSK or offset QPSK shaped with an RRC \
                   (roll-off 0.6) at `sps` samples a symbol.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: vec![
                mode_param(),
                design::int_range(
                    "sps",
                    "Samples/symbol",
                    4,
                    2,
                    16,
                    "Output samples per symbol (even).",
                ),
            ],
        },
        build_modulator,
    );
    r.set_keywords(
        "lrpt_modulator",
        &["meteor", "lrpt", "oqpsk", "loopback", "test"],
    );
}

/// Status lines are dropped rather than queued beyond this.
const MSG_BACKLOG: usize = 4096;
/// Soft bit for a unit-power QPSK symbol's ±1/√2.
const SOFT_SCALE: f32 = 40.0 / std::f32::consts::FRAC_1_SQRT_2;

struct DemodState {
    fe: FrontEnd,
    /// One deframer per OQPSK pairing (only the first otherwise); the one
    /// in use once locked.
    deframers: Vec<Deframer>,
    active: Option<usize>,
    deint: Option<Box<Deinterleaver>>,
    pairs: OqpskPairs,
    syms: Vec<Complex32>,
    alt: Vec<Complex32>,
    soft_a: Vec<[i8; 2]>,
    soft_b: Vec<[i8; 2]>,
    soft_out: Vec<[i8; 2]>,
    fifo: ByteFifo,
    samples: u64,
    next_report: u64,
    last_locked: bool,
    last_frames: u64,
    last_vit: (u64, u64),
}

pub struct LrptDemodulator {
    mode: Mode,
    offset: f64,
    st: Option<Box<DemodState>>,
    msgs: MsgQueue,
}

fn write_frame(fifo: &mut ByteFifo, f: &Frame<'_>) {
    if f.ok() {
        fifo.write(&ASM_BYTES);
        fifo.write(f.data);
    }
}

fn soft(z: Complex32) -> [i8; 2] {
    let q = |v: f32| (v * SOFT_SCALE).round().clamp(-127.0, 127.0) as i8;
    [q(z.re), q(z.im)]
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
            // The frequency moved: what was collected is stale.
            for d in &mut self.deframers {
                d.reset();
            }
        }
        if !self.fe.settled() && !self.locked() {
            return used;
        }
        let fifo = &mut self.fifo;
        let mut emit = |f: Frame<'_>| write_frame(fifo, &f);
        if let Some(deint) = &mut self.deint {
            self.alt.clear();
            self.pairs.pair(&self.syms, &mut self.alt);
            self.soft_a.clear();
            self.soft_a.extend(self.syms.iter().map(|&z| soft(z)));
            self.soft_b.clear();
            self.soft_b.extend(self.alt.iter().map(|&z| soft(z)));
            self.soft_out.clear();
            deint.push(&self.soft_a, &self.soft_b, &mut self.soft_out);
            self.deframers[0].push_soft(&self.soft_out, &mut emit);
            self.active = Some(0);
            return used;
        }
        if self.deframers.len() == 1 {
            self.deframers[0].push(&self.syms, &mut emit);
            self.active = Some(0);
            return used;
        }
        self.alt.clear();
        self.pairs.pair(&self.syms, &mut self.alt);
        match self.active.filter(|&i| self.deframers[i].is_locked()) {
            Some(i) => {
                let s = if i == 0 { &self.syms } else { &self.alt };
                self.deframers[i].push(s, &mut emit);
            }
            None => {
                self.active = None;
                for (i, s) in [&self.syms, &self.alt].into_iter().enumerate() {
                    self.deframers[i].push(s, &mut emit);
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

impl LrptDemodulator {
    fn report(st: &mut DemodState, msgs: &mut MsgQueue, mode: Mode) {
        let t = st.samples as f64 / st.fe.rate();
        let locked = st.locked();
        if locked != st.last_locked && msgs.pending() < MSG_BACKLOG {
            if locked {
                let d = &st.deframers[st.active.unwrap_or(0)];
                msgs.line(format_args!(
                    "t={t:.3} event=lock mode={} rotation={} conj={} freq_hz={:+.0}",
                    Mode::NAMES[mode as usize],
                    d.stats().symmetry.degrees(),
                    u8::from(d.stats().symmetry.conj),
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
        let mut s = *st.deframers[0].stats();
        for d in &st.deframers[1..] {
            let o = d.stats();
            s.frames += o.frames;
            s.bad_frames += o.bad_frames;
            s.rs_corrected += o.rs_corrected;
            s.rs_failed += o.rs_failed;
            s.viterbi_bits += o.viterbi_bits;
            s.viterbi_errors += o.viterbi_errors;
        }
        let fps = s.frames - st.last_frames;
        st.last_frames = s.frames;
        let (vb, ve) = (
            s.viterbi_bits - st.last_vit.0,
            s.viterbi_errors - st.last_vit.1,
        );
        st.last_vit = (s.viterbi_bits, s.viterbi_errors);
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
        if let Some(d) = &st.deint {
            msgs.text(format_args!(
                " marker={}",
                if d.locked { "lock" } else { "search" }
            ));
        }
        msgs.text(format_args!(
            " frames={} bad={} rs_corrected={} rs_failed={} fps={fps}",
            s.frames, s.bad_frames, s.rs_corrected, s.rs_failed
        ));
        if vb > 0 {
            msgs.text(format_args!(" ber={:.2e}", ve as f64 / vb as f64));
        }
        msgs.line(format_args!(""));
    }
}

/// CADU FIFO size and the room kept free before taking more input.
const FIFO_LEN: usize = 1 << 16;
const FIFO_MARGIN: usize = 1 << 14;

impl Block for LrptDemodulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        let symbol_rate = self.mode.symbol_rate();
        if rate < 2.0 * symbol_rate {
            return Err(Error::Other(format!(
                "the input rate {} is less than 2 samples per symbol at {} Bd; the \
                 demodulator needs at least 2",
                design::fmt_rate(rate),
                symbol_rate
            )));
        }
        if let Some(o) = cx.outputs.get_mut(0) {
            // One data bit a symbol (rate 1/2 QPSK), less the 80k markers.
            let bits = if self.mode.interleaved() {
                symbol_rate * 72.0 / 80.0
            } else {
                symbol_rate
            };
            o.sample_rate = Some(bits / 8.0);
            o.center_freq = None;
        }
        text_meta(cx, 1);
        let mut fe = FrontEnd::new(rate, symbol_rate, self.mode.psk(), ROLLOFF);
        fe.set_offset(self.offset);
        let cfg = self.mode.config();
        let n = if self.mode == Mode::Oqpsk72 { 2 } else { 1 };
        let deframers = (0..n)
            .map(|_| Deframer::new(cfg.clone()).map_err(Error::Other))
            .collect::<Result<Vec<_>, _>>()?;
        self.st = Some(Box::new(DemodState {
            fe,
            deframers,
            active: None,
            deint: self
                .mode
                .interleaved()
                .then(|| Box::new(Deinterleaver::new())),
            pairs: OqpskPairs::new(),
            syms: Vec::with_capacity(CHUNK + 8),
            alt: Vec::with_capacity(CHUNK + 8),
            soft_a: Vec::with_capacity(CHUNK + 8),
            soft_b: Vec::with_capacity(CHUNK + 8),
            soft_out: Vec::with_capacity(MARKER_SEARCH + CHUNK + 8),
            fifo: ByteFifo::new(FIFO_LEN),
            samples: 0,
            next_report: rate.round().max(1.0) as u64,
            last_locked: false,
            last_frames: 0,
            last_vit: (0, 0),
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
            Self::report(st, &mut self.msgs, self.mode);
        }
        if used == 0 && made == 0 && msg_made == 0 {
            let waiting = !st.fifo.is_empty() || !self.msgs.is_empty();
            if waiting && io.inputs[0].exhausted() && !io.outputs[0].abandoned() {
                return Ok(WorkStatus::Wait(Duration::from_millis(20)));
            }
            if io.inputs[0].exhausted() && st.locked() {
                // The end: what the Viterbi decoder still holds.
                let fifo = &mut st.fifo;
                let i = st.active.unwrap_or(0);
                st.deframers[i].flush(&mut |f: Frame<'_>| write_frame(fifo, &f));
                st.deframers[i].reset();
                return Ok(WorkStatus::Ok);
            }
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        io.produce(1, msg_made);
        Ok(WorkStatus::Ok)
    }
}

fn read_mode(p: &ParamReader<'_>) -> Result<Mode, Error> {
    let m = p.str("mode")?;
    Mode::parse(m).ok_or_else(|| Error::Other(format!("unknown mode `{m}`")))
}

fn build_demodulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(LrptDemodulator {
            mode: read_mode(p)?,
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

// ---- Encoder: synthetic imagery → CADUs -------------------------------------------

/// The synthetic picture `lrpt_encoder` sends: channel `c` (0–5), pixel
/// `x`, row `y`. Smooth enough for JPEG to keep it within a few levels.
pub fn test_pattern(c: usize, x: usize, y: usize) -> u8 {
    let (xf, yf) = (x as f32, y as f32);
    let c = c as f32;
    let v = 128.0
        + 70.0 * (xf * (0.004 + 0.001 * c) + yf * 0.003 + c).sin()
        + 40.0 * (yf * (0.02 + 0.004 * c) - xf * 0.001).cos()
        + if ((x / 160) + (y / 96)).is_multiple_of(2) {
            8.0
        } else {
            -8.0
        };
    v.clamp(0.0, 255.0) as u8
}

/// Milliseconds of the day the encoder's first scan is stamped with.
pub const TEST_START_MS: u32 = 36_000_000;
/// Milliseconds a scan takes.
pub const SCAN_MS: u32 = 1230;

/// Transfer frames a second (one data bit a symbol at 72 kBd).
pub const FRAMES_PER_SECOND: f64 = 72_000.0 / (8.0 * 1024.0);

/// Builds the packets and frames of synthetic LRPT imagery, with fill
/// frames (VCID 63) so that a scan takes its 1.23 s of the link, as on air.
pub struct LrptSource {
    channels: Vec<usize>,
    quality: u8,
    builder: FrameBuilder,
    frames: u64,
    fill: Vec<u8>,
    fill_count: u32,
    seq: u16,
    scan: u64,
    strip: [u8; jpeg::STRIP_PIXELS],
    packet: Vec<u8>,
}

impl LrptSource {
    pub fn new(channels: Vec<usize>, quality: u8) -> Self {
        let layout = Layout {
            frame_len: VCDU_LEN,
            insert_zone: 2,
            trailer: 0,
        };
        Self {
            channels,
            quality,
            builder: FrameBuilder::new(layout, 0, VCID).expect("valid layout"),
            frames: 0,
            fill: vec![0; VCDU_LEN],
            fill_count: 0,
            seq: 0,
            scan: 0,
            strip: [0; jpeg::STRIP_PIXELS],
            packet: Vec::with_capacity(4096),
        }
    }

    pub fn scans(&self) -> u64 {
        self.scan
    }

    fn header(&mut self, apid: u16, len: usize) {
        let h = PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: true,
            apid,
            seq_flags: 3,
            seq: self.seq,
            data_len: len,
        };
        self.seq = (self.seq + 1) & 0x3FFF;
        h.write(&mut self.packet[..6]);
    }

    /// The next scan's packets, then fill frames up to the scan's share of
    /// the link; transfer frames (892 bytes) go to `out`.
    pub fn scan(&mut self, out: &mut impl FnMut(&[u8])) {
        let mut frames = 0u64;
        let mut count = |f: &[u8]| {
            frames += 1;
            out(f);
        };
        self.scan_packets(&mut count);
        self.frames += frames;
        let due = (self.scan as f64 * f64::from(SCAN_MS) / 1000.0 * FRAMES_PER_SECOND) as u64;
        while self.frames < due {
            VcduHeader {
                version: 1,
                spacecraft: 0,
                vcid: VCID_FILL,
                counter: self.fill_count,
                replay: false,
            }
            .write(&mut self.fill);
            self.fill_count = (self.fill_count + 1) & 0xFF_FFFF;
            out(&self.fill);
            self.frames += 1;
        }
    }

    fn scan_packets(&mut self, out: &mut impl FnMut(&[u8])) {
        let ms = TEST_START_MS.wrapping_add((self.scan as u32).wrapping_mul(SCAN_MS));
        let row0 = self.scan as usize * 8;
        let mut time = [0u8; 8];
        time[0..2].copy_from_slice(&9000u16.to_be_bytes());
        time[2..6].copy_from_slice(&ms.to_be_bytes());
        for ci in 0..self.channels.len() {
            let c = self.channels[ci];
            for col in 0..PACKETS_PER_SCAN {
                for y in 0..8 {
                    for x in 0..jpeg::STRIP_W {
                        self.strip[y * jpeg::STRIP_W + x] =
                            test_pattern(c, col * jpeg::STRIP_W + x, row0 + y);
                    }
                }
                self.packet.clear();
                self.packet.resize(6, 0);
                self.packet.extend_from_slice(&time);
                self.packet.extend_from_slice(&[
                    (col * PACKETS_PER_SCAN) as u8,
                    0,
                    0,
                    0xFF,
                    0xF0,
                    self.quality,
                ]);
                jpeg::encode_strip(&self.strip, self.quality, &mut self.packet);
                let len = self.packet.len() - 6;
                self.header(APID_FIRST + c as u16, len);
                let packet = std::mem::take(&mut self.packet);
                self.builder.push_packet(&packet, out);
                self.packet = packet;
            }
        }
        // Telemetry: the MSU-MR serial number (4: N2-4) in byte 20.
        self.packet.clear();
        self.packet.resize(6, 0);
        self.packet.extend_from_slice(&time);
        self.packet.resize(6 + 56, 0);
        self.packet[6 + 20] = 0x40;
        self.header(APID_TLM, 56);
        let packet = std::mem::take(&mut self.packet);
        self.builder.push_packet(&packet, out);
        self.packet = packet;
        self.scan += 1;
    }

    pub fn flush(&mut self, out: &mut impl FnMut(&[u8])) {
        self.builder.flush(out);
    }
}

/// A transfer frame as a CADU, with zeros where the RS parity goes.
fn write_cadu(fifo: &mut ByteFifo, frame: &[u8]) {
    const ZEROS: [u8; FRAME_LEN - VCDU_LEN] = [0; FRAME_LEN - VCDU_LEN];
    fifo.write(&ASM_BYTES);
    fifo.write(frame);
    fifo.write(&ZEROS);
}

pub struct EncoderBlock {
    src: LrptSource,
    limit: u64,
    fifo: ByteFifo,
    flushed: bool,
}

/// A scan of three channels at quality 100 is under 40 frames.
const ENC_FIFO: usize = 1 << 18;
const ENC_NEED: usize = 1 << 17;

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
            if self.fifo.free() < ENC_NEED {
                break;
            }
            let fifo = &mut self.fifo;
            let mut out = |f: &[u8]| write_cadu(fifo, f);
            if self.limit == 0 || self.src.scans() < self.limit {
                self.src.scan(&mut out);
            } else if !self.flushed {
                self.flushed = true;
                self.src.flush(&mut out);
            } else if self.fifo.is_empty() {
                return Ok(WorkStatus::Done);
            } else {
                break;
            }
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
    let mut channels = Vec::new();
    for t in p
        .str("channels")?
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let c: usize = t
            .parse()
            .ok()
            .filter(|c| (1..=6).contains(c))
            .ok_or_else(|| Error::Other(format!("`{t}` is not an MSU-MR channel (1–6)")))?;
        if channels.contains(&(c - 1)) {
            return Err(Error::Other(format!("channel {c} is listed twice")));
        }
        channels.push(c - 1);
    }
    if channels.is_empty() {
        return Err(Error::Other("no channels to send".into()));
    }
    let quality = p.u64("quality")?.clamp(1, 100) as u8;
    Ok(Instance {
        block: Box::new(EncoderBlock {
            src: LrptSource::new(channels, quality),
            limit: p.u64("scans")?,
            fifo: ByteFifo::new(ENC_FIFO),
            flushed: false,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Modulator ------------------------------------------------------------------

/// Codeblocks → shaped baseband for a mode.
pub struct LrptTransmitter {
    mode: Mode,
    enc: Encoder,
    il: Option<Interleaver>,
    bits: Vec<u8>,
    syms: Vec<Complex32>,
    shaper: Resampler<Complex32>,
    sps: usize,
    /// Q delayed by half a symbol (OQPSK).
    qdelay: Vec<f32>,
    qpos: usize,
}

impl LrptTransmitter {
    pub fn new(mode: Mode, sps: usize) -> Self {
        let h = taps::rrc(sps as f64, ROLLOFF, 12, sps as f64);
        let half = if mode.psk() == Psk::Oqpsk { sps / 2 } else { 0 };
        Self {
            mode,
            enc: Encoder::new(mode.config()).expect("valid config"),
            il: mode.interleaved().then(Interleaver::new),
            bits: Vec::with_capacity(2 * 8 * (4 + FRAME_LEN) * 10 / 9 + 64),
            syms: Vec::with_capacity(8 * (4 + FRAME_LEN) + 64),
            shaper: Resampler::new(sps, 1, &h),
            sps,
            qdelay: vec![0.0; half.max(1)],
            qpos: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// One codeblock (1020 bytes, parity zeros) → samples appended to `out`.
    pub fn frame(&mut self, codeblock: &[u8], out: &mut Vec<Complex32>) {
        self.bits.clear();
        let (bits, il) = (&mut self.bits, &mut self.il);
        self.enc.encode(codeblock, |s| {
            for v in [s.re, s.im] {
                let b = u8::from(v < 0.0);
                match il {
                    Some(il) => il.push(b, |c| bits.push(c)),
                    None => bits.push(b),
                }
            }
        });
        let a = std::f32::consts::FRAC_1_SQRT_2;
        let level = |b: u8| if b == 0 { a } else { -a };
        self.syms.clear();
        self.syms.extend(
            self.bits
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| Complex32::new(level(p[0]), level(p[1]))),
        );
        // (A frame is 16 384 coded bits plus whole markers: always even.)
        let start = out.len();
        out.resize(start + self.syms.len() * self.sps, Complex32::default());
        let (_, made) = self.shaper.process(&self.syms, &mut out[start..]);
        out.truncate(start + made);
        if self.mode.psk() == Psk::Oqpsk {
            for z in &mut out[start..] {
                std::mem::swap(&mut z.im, &mut self.qdelay[self.qpos]);
                self.qpos = (self.qpos + 1) % self.qdelay.len();
            }
        }
    }
}

pub struct ModulatorBlock {
    tx: LrptTransmitter,
    reader: CaduReader,
    out: Vec<Complex32>,
    pos: usize,
}

impl Block for ModulatorBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = Some(self.tx.mode().symbol_rate() * self.tx.sps as f64);
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
    let mode = read_mode(p)?;
    let sps = p.u64("sps")? as usize;
    if !(2..=16).contains(&sps) || !sps.is_multiple_of(2) {
        return Err(Error::Other(format!(
            "samples per symbol must be even, 2 to 16 (got {sps})"
        )));
    }
    let tx = LrptTransmitter::new(mode, sps);
    Ok(Instance {
        block: Box::new(ModulatorBlock {
            out: Vec::with_capacity(sps * 10 * (4 + FRAME_LEN) + 64),
            tx,
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
    fn interleaver_and_deinterleaver_delay_every_bit_alike() {
        let mut il = ConvInterleaver::new(false);
        let mut de = ConvInterleaver::new(true);
        let n = ConvInterleaver::LATENCY + 5000;
        let x = |j: usize| ((j * 7919 + j / 3) % 251) as i8;
        let mut checked = 0;
        for j in 0..n {
            let y = de.push(il.push(x(j)));
            if j >= ConvInterleaver::LATENCY {
                assert_eq!(y, x(j - ConvInterleaver::LATENCY), "bit {j}");
                checked += 1;
            }
        }
        assert_eq!(checked, 5000);
        // Interleaved neighbours end up 2048 × 36 bits apart.
        let mut il = ConvInterleaver::new(false);
        let mut first_seen = vec![usize::MAX; 72];
        for j in 0..40 * SPAN {
            let v = il.push(if j < 72 { j as i8 + 1 } else { 0 });
            if v > 0 {
                first_seen[(v - 1) as usize] = j;
            }
        }
        assert_eq!(first_seen[0], 0);
        assert_eq!(first_seen[1], SPAN + 1);
        assert_eq!(first_seen[36], 36);
    }

    #[test]
    fn marker_every_80_bits() {
        let mut il = Interleaver::new();
        let mut bits = Vec::new();
        for _ in 0..720 {
            il.push(1, |b| bits.push(b));
        }
        assert_eq!(bits.len(), 800);
        for p in 0..10 {
            let m = bits[p * 80..p * 80 + 8]
                .iter()
                .fold(0u8, |a, &b| (a << 1) | b);
            assert_eq!(m, MARKER);
        }
    }
}
