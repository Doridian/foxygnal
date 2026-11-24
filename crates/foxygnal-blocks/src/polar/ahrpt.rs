//! Metop AHRPT (1701.3 MHz): the link, receive and transmit side.
//!
//! **The link.** QPSK at 2.3333 MSym/s carrying 3.5 Mbit/s: 1024-byte CADUs
//! (the ASM, then a 1020-byte codeblock: a VCDU with a 2-byte insert zone,
//! RS(255,223) I = 4 in the dual basis, randomized), NRZ-L, through the
//! K = 7 convolutional code (G1 = 171₈, G2 = 133₈, G2 not inverted)
//! punctured to rate 3/4 (CCSDS 131.0-B: G1 kept on bits 1 and 3 of each
//! three, G2 on bits 1 and 2). Every three data bits make four code bits,
//! two QPSK symbols: the first (I = G1₁, Q = G2₁), the second (I = G1₃,
//! Q = G2₂).
//!
//! **Receiving.** The front end ([`super::frontend`]) gives one symbol per
//! symbol; which symbol starts a puncturing period, the carrier's phase (four
//! rotations and the I/Q swap; a half turn only inverts the data, which the
//! deframer resolves) and which of the second symbol's branches is G1 are
//! unknown. [`Search`] decodes a few thousand symbols under each of those 16
//! hypotheses and keeps the one whose re-encoded output agrees best with
//! what was received; the decoded bits then go to the CCSDS [`Deframer`]
//! (uncoded, NRZ-L), which finds the ASM. A hypothesis that doesn't give a
//! lock within a few frames is set aside and the next best tried.
//!
//! `ahrpt_demodulator` is that chain: baseband in, CADUs out, ready for
//! `ccsds_packets` (insert zone 2) and `avhrr_decoder`. `ahrpt_encoder`
//! (synthetic AVHRR/3 lines → CADUs) and `ahrpt_modulator` (CADUs →
//! baseband) are the transmit side, for the loopback template and the tests.

use std::time::Duration;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Resampler, taps};
use serde_json::json;

use super::avhrr::{APID_3A, APID_MHS, VCID_AVHRR, WORDS, avhrr_pattern};
use super::frontend::{CHUNK, FrontEnd, Psk};
use crate::ccsds::sync::{Modulation, Symmetry};
use crate::ccsds::vcdu::{FrameBuilder, Layout, PacketHeader, VCID_FILL, VcduHeader};
use crate::ccsds::wire::{ASM_BYTES, CaduReader};
use crate::ccsds::{Config, Deframer, Encoder, Frame};
use crate::design;
use crate::digital::{MsgQueue, text_meta};
use crate::dvb::ByteFifo;
use crate::dvb::conv::outputs;
use crate::dvb::viterbi::Viterbi;
use crate::filters::not_configured;

/// Data bits a second, and QPSK symbols a second (4 code bits per 3 data
/// bits, 2 a symbol).
pub const BIT_RATE: f64 = 3_500_000.0;
pub const SYMBOL_RATE: f64 = BIT_RATE * 2.0 / 3.0;
/// Bytes after the ASM, and of the transfer frame within them.
pub const FRAME_LEN: usize = 1020;
pub const VCDU_LEN: usize = 892;
/// Transfer frames a second.
pub const FRAMES_PER_SECOND: f64 = BIT_RATE / (8.0 * 1024.0);
/// The shaping filter's roll-off (not confirmed on air).
const ROLLOFF: f64 = 0.5;

/// The deframer's configuration for the decoded bits.
pub fn deframer_config() -> Config {
    Config {
        modulation: Modulation::Bpsk,
        coded: false,
        conv_invert: false,
        nrzm: false,
        randomized: true,
        rs: Some((4, true)),
        frame_len: FRAME_LEN,
    }
}

// ---- Puncturing ------------------------------------------------------------------

/// One way of reading the symbol stream: the carrier symmetry (modulo a half
/// turn), whether the first symbol is a period's second, and whether the
/// second symbol's I carries G1₃ (as sent) or G2₂.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hypothesis {
    pub symmetry: Symmetry,
    pub odd: bool,
    pub swapped: bool,
}

/// The hypotheses tried, in order.
pub const HYPOTHESES: usize = 16;

impl Hypothesis {
    pub fn nth(i: usize) -> Self {
        Self {
            symmetry: Symmetry::new((i & 1) as u8, i & 2 != 0),
            odd: i & 4 != 0,
            swapped: i & 8 != 0,
        }
    }
}

/// Soft symbols → the mother code's (G1, G2) soft pairs, erasures (0)
/// where bits were punctured.
#[derive(Debug, Clone, Copy)]
pub struct Depuncturer {
    h: Hypothesis,
    /// The next symbol is a period's second.
    second: bool,
}

impl Depuncturer {
    pub fn new(h: Hypothesis) -> Self {
        Self { h, second: h.odd }
    }

    #[inline]
    pub fn push(&mut self, s: [i8; 2], out: &mut Vec<[i8; 2]>) {
        let [i, q] = self.h.symmetry.apply(s);
        if self.second {
            // (G1₃, G2₂) on (I, Q), or the other way round.
            let (g1, g2) = if self.h.swapped { (q, i) } else { (i, q) };
            out.push([0, g2]);
            out.push([g1, 0]);
        } else {
            out.push([i, q]);
        }
        self.second = !self.second;
    }
}

/// The transmit side: data bits → QPSK symbols (as code bits, 0/1 per
/// branch).
#[derive(Debug, Clone, Default)]
pub struct Puncturer {
    state: u8,
    /// Data bits of the period so far, and their (G1, G2).
    n: usize,
    code: [[u8; 2]; 3],
}

impl Puncturer {
    pub fn new() -> Self {
        Self::default()
    }

    /// One data bit; a completed period's two symbols go to `out` as
    /// (I, Q) bit pairs.
    #[inline]
    pub fn push(&mut self, bit: u8, mut out: impl FnMut([u8; 2])) {
        let reg = (self.state << 1) | (bit & 1);
        self.state = reg & 0x3F;
        let (x, y) = outputs(reg);
        self.code[self.n] = [x, y];
        self.n += 1;
        if self.n == 3 {
            self.n = 0;
            let c = self.code;
            out([c[0][0], c[0][1]]);
            out([c[2][0], c[1][1]]);
        }
    }
}

// ---- Search --------------------------------------------------------------------

/// Symbols decoded per hypothesis while searching.
pub const SEARCH_SYMBOLS: usize = 4096;
/// The best hypothesis's re-encoded disagreement must be below this.
const SEARCH_MAX_BER: f64 = 0.05;
/// Decoded bits without a deframer lock before a hypothesis is given up.
const GIVE_UP_BITS: u64 = 8 * 8192;
/// Symbols skipped after a search that found nothing (no signal yet), so
/// searching costs about 5 trellis steps a symbol rather than 50.
const SEARCH_PAUSE: usize = 4 * SEARCH_SYMBOLS;

/// Scores the hypotheses on a block of soft symbols.
pub struct Search {
    vit: Viterbi,
    pairs: Vec<[i8; 2]>,
}

impl Default for Search {
    fn default() -> Self {
        Self::new()
    }
}

impl Search {
    pub fn new() -> Self {
        Self {
            vit: Viterbi::new(),
            pairs: Vec::with_capacity(2 * SEARCH_SYMBOLS),
        }
    }

    /// The re-encoded disagreement under hypothesis `h`.
    pub fn score(&mut self, soft: &[[i8; 2]], h: Hypothesis) -> f64 {
        self.pairs.clear();
        let mut d = Depuncturer::new(h);
        for &s in soft {
            d.push(s, &mut self.pairs);
        }
        self.vit.reset();
        self.vit.reset_counters();
        self.vit.decode(&self.pairs, |_| {});
        self.vit.flush(|_| {});
        self.vit.errors as f64 / self.vit.compared.max(1) as f64
    }

    /// The best hypothesis not in `skip` (a bit per hypothesis), and its
    /// score, if good enough.
    pub fn best(&mut self, soft: &[[i8; 2]], skip: u16) -> Option<(usize, f64)> {
        let mut best: Option<(usize, f64)> = None;
        for i in 0..HYPOTHESES {
            if skip & (1 << i) != 0 {
                continue;
            }
            let s = self.score(soft, Hypothesis::nth(i));
            if best.is_none_or(|(_, b)| s < b) {
                best = Some((i, s));
            }
        }
        best.filter(|&(_, s)| s < SEARCH_MAX_BER)
    }
}

// ---- Descriptors ---------------------------------------------------------------

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "ahrpt_demodulator",
            label: "Metop AHRPT Demodulator",
            category: "Satellite",
            help: "Metop AHRPT (1701.3 MHz): baseband in (at least 2 samples a symbol at \
                   2.3333 MSym/s: 4.7 MS/s or more), CADUs out for CCSDS Packets (insert \
                   zone 2) and the AVHRR Decoder. Matched filter, timing and carrier \
                   recovery for QPSK, a frequency search over ± 580 kHz, then the rate 3/4 \
                   punctured Viterbi decoder (puncturing phase and carrier phase found by \
                   trying each) and the CCSDS deframer: sync marker, derandomizer, \
                   RS(255,223) I = 4 in the dual basis. `offset` is where the carrier is \
                   expected (Hz): wire a Doppler source to it. `status` has a line per \
                   second and per lock change.",
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
                       error, Doppler); the search covers ± 580 kHz around it. Live, and can \
                       be wired (e.g. to a Satellite Tracker's Doppler).",
                live: true,
            }],
        },
        build_demodulator,
    );
    r.set_keywords(
        "ahrpt_demodulator",
        &[
            "metop",
            "ahrpt",
            "hrpt",
            "qpsk",
            "weather satellite",
            "l-band",
            "1701 mhz",
        ],
    );

    r.register(
        BlockDescriptor {
            kind: "ahrpt_encoder",
            label: "Metop AHRPT Encoder",
            category: "Satellite",
            help: "A test transmitter's data: synthetic AVHRR/3 lines (a smooth pattern per \
                   channel, the same the tests compare against; APID 103 on virtual channel \
                   9, six lines a second by their time stamps) and a placeholder MHS packet \
                   (APID 34) every 16 lines, in transfer frames, out as CADUs with room for \
                   the Reed-Solomon parity (AHRPT Modulator fills it in).",
            inputs: vec![],
            outputs: vec![port("out", StreamType::U8)],
            params: vec![
                design::int(
                    "lines",
                    "Lines",
                    0,
                    0,
                    "AVHRR lines to send, then stop; 0 = without end.",
                ),
                design::choice(
                    "pace",
                    "Pace",
                    vec!["air", "packed"],
                    "air",
                    "air: fill frames (virtual channel 63) between the lines, so a line \
                     takes its 1/6 s of the link as on air. packed: no fill, every frame \
                     carries data (faster than real time; for tests).",
                ),
            ],
        },
        build_encoder,
    );
    r.set_keywords(
        "ahrpt_encoder",
        &["metop", "ahrpt", "avhrr", "loopback", "test"],
    );

    r.register(
        BlockDescriptor {
            kind: "ahrpt_modulator",
            label: "Metop AHRPT Modulator",
            category: "Satellite",
            help: "The transmit side of the AHRPT Demodulator, for loopback tests: CADUs in, \
                   RS parity (dual basis), randomizer, the K = 7 code punctured to rate 3/4, \
                   QPSK at 2.3333 MSym/s shaped with an RRC (roll-off 0.5) at `sps` samples \
                   a symbol.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: vec![design::int_range(
                "sps",
                "Samples/symbol",
                2,
                2,
                16,
                "Output samples per symbol.",
            )],
        },
        build_modulator,
    );
    r.set_keywords(
        "ahrpt_modulator",
        &["metop", "ahrpt", "qpsk", "loopback", "test"],
    );
}

// ---- Demodulator ---------------------------------------------------------------

/// Status lines are dropped rather than queued beyond this.
const MSG_BACKLOG: usize = 4096;
/// Soft bit for a unit-power QPSK symbol's ±1/√2.
const SOFT_SCALE: f32 = 40.0 / std::f32::consts::FRAC_1_SQRT_2;
/// Soft value of a decoded bit for the deframer.
const BIT_SOFT: i8 = 64;

fn soft(z: Complex32) -> [i8; 2] {
    let q = |v: f32| (v * SOFT_SCALE).round().clamp(-127.0, 127.0) as i8;
    [q(z.re), q(z.im)]
}

fn write_frame(fifo: &mut ByteFifo, f: &Frame<'_>) {
    if f.ok() {
        fifo.write(&ASM_BYTES);
        fifo.write(f.data);
    }
}

struct DemodState {
    fe: FrontEnd,
    search: Search,
    /// The hypothesis in use, its depuncturer and Viterbi decoder.
    hyp: Option<usize>,
    depunct: Depuncturer,
    vit: Viterbi,
    deframer: Deframer,
    /// Hypotheses that didn't lock since the last success or jump.
    tried: u16,
    /// Decoded bits since the deframer last was locked.
    unlocked_bits: u64,
    /// Symbols still to skip before the next search.
    pause: usize,
    acq: Vec<[i8; 2]>,
    syms: Vec<Complex32>,
    soft: Vec<[i8; 2]>,
    pairs: Vec<[i8; 2]>,
    bits: Vec<[i8; 2]>,
    fifo: ByteFifo,
    samples: u64,
    next_report: u64,
    last_locked: bool,
    last_frames: u64,
    last_vit: (u64, u64),
}

impl DemodState {
    fn locked(&self) -> bool {
        self.deframer.is_locked()
    }

    fn start(&mut self, h: usize) {
        self.hyp = Some(h);
        self.depunct = Depuncturer::new(Hypothesis::nth(h));
        self.vit.reset();
        self.deframer.reset();
        self.unlocked_bits = 0;
    }

    fn stop(&mut self) {
        self.hyp = None;
        self.acq.clear();
        self.deframer.reset();
    }

    /// Run soft symbols through the hypothesis in use.
    fn decode(&mut self, soft: &[[i8; 2]]) {
        let Some(h) = self.hyp else { return };
        for part in soft.chunks(CHUNK) {
            self.pairs.clear();
            for &s in part {
                self.depunct.push(s, &mut self.pairs);
            }
            self.bits.clear();
            let bits = &mut self.bits;
            self.vit.decode(&self.pairs, |b| {
                bits.extend(
                    b.iter()
                        .map(|&v| [if v == 0 { BIT_SOFT } else { -BIT_SOFT }, 0]),
                );
            });
            let fifo = &mut self.fifo;
            self.deframer
                .push_soft(&self.bits, &mut |f: Frame<'_>| write_frame(fifo, &f));
            if self.deframer.is_locked() {
                self.unlocked_bits = 0;
                self.tried = 0;
            } else {
                self.unlocked_bits += self.bits.len() as u64;
                if self.unlocked_bits > GIVE_UP_BITS {
                    self.tried |= 1 << h;
                    if self.tried == u16::MAX {
                        self.tried = 0;
                    }
                    self.stop();
                    return;
                }
            }
        }
    }

    fn process(&mut self, input: &[Complex32]) -> usize {
        self.syms.clear();
        let jumps = self.fe.jumps;
        self.fe.set_locked(self.locked());
        let used = self.fe.process(input, &mut self.syms);
        self.samples += used as u64;
        if self.fe.jumps != jumps && !self.locked() {
            // The frequency moved: what was collected is stale.
            self.stop();
            self.tried = 0;
        }
        if !self.fe.settled() && !self.locked() {
            return used;
        }
        self.soft.clear();
        self.soft.extend(self.syms.iter().map(|&z| soft(z)));
        let soft = std::mem::take(&mut self.soft);
        let mut rest = &soft[..];
        while !rest.is_empty() {
            if self.hyp.is_some() {
                self.decode(rest);
                // A hypothesis given up mid-way: the rest is searched.
                if self.hyp.is_some() {
                    break;
                }
                rest = &rest[rest.len().min(CHUNK)..];
                continue;
            }
            if self.pause > 0 {
                let k = self.pause.min(rest.len());
                self.pause -= k;
                rest = &rest[k..];
                continue;
            }
            let room = (SEARCH_SYMBOLS - self.acq.len()).min(rest.len());
            self.acq.extend_from_slice(&rest[..room]);
            rest = &rest[room..];
            if self.acq.len() < SEARCH_SYMBOLS {
                break;
            }
            match self.search.best(&self.acq, self.tried) {
                Some((h, _)) => {
                    self.start(h);
                    let acq = std::mem::take(&mut self.acq);
                    self.decode(&acq);
                    self.acq = acq;
                    self.acq.clear();
                }
                None => {
                    // Nothing (good enough) yet: pause, then look again,
                    // every hypothesis allowed.
                    self.acq.clear();
                    self.pause = SEARCH_PAUSE;
                    self.tried = 0;
                }
            }
        }
        self.soft = soft;
        used
    }

    fn flush(&mut self) {
        if self.hyp.is_none() {
            return;
        }
        self.bits.clear();
        let bits = &mut self.bits;
        self.vit.flush(|b| {
            bits.extend(
                b.iter()
                    .map(|&v| [if v == 0 { BIT_SOFT } else { -BIT_SOFT }, 0]),
            );
        });
        let fifo = &mut self.fifo;
        self.deframer
            .push_soft(&self.bits, &mut |f: Frame<'_>| write_frame(fifo, &f));
        self.stop();
    }
}

pub struct AhrptDemodulator {
    offset: f64,
    st: Option<Box<DemodState>>,
    msgs: MsgQueue,
}

impl AhrptDemodulator {
    fn report(st: &mut DemodState, msgs: &mut MsgQueue) {
        let t = st.samples as f64 / st.fe.rate();
        let locked = st.locked();
        if locked != st.last_locked && msgs.pending() < MSG_BACKLOG {
            if locked {
                let h = Hypothesis::nth(st.hyp.unwrap_or(0));
                let sym = st.deframer.stats().symmetry;
                msgs.line(format_args!(
                    "t={t:.3} event=lock rotation={} conj={} puncture={} swapped={} \
                     inverted={} freq_hz={:+.0}",
                    h.symmetry.degrees(),
                    u8::from(h.symmetry.conj),
                    u8::from(h.odd),
                    u8::from(h.swapped),
                    u8::from(sym.quarter != 0),
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
        let s = *st.deframer.stats();
        let fps = s.frames - st.last_frames;
        st.last_frames = s.frames;
        let (vb, ve) = (
            st.vit.compared - st.last_vit.0,
            st.vit.errors - st.last_vit.1,
        );
        st.last_vit = (st.vit.compared, st.vit.errors);
        if msgs.pending() >= MSG_BACKLOG {
            return;
        }
        let state = if locked {
            "lock"
        } else if st.hyp.is_some() {
            "sync"
        } else {
            "search"
        };
        msgs.text(format_args!(
            "t={t:.3} state={state} freq_hz={:+.0}",
            st.fe.offset_hz()
        ));
        if let Some(m) = mer {
            msgs.text(format_args!(" mer_db={m:.1}"));
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
const FIFO_LEN: usize = 1 << 17;
const FIFO_MARGIN: usize = 1 << 15;

impl Block for AhrptDemodulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if rate < 2.0 * SYMBOL_RATE {
            return Err(Error::Other(format!(
                "the input rate {} is less than 2 samples per symbol at {}; the \
                 demodulator needs at least 2 (4.67 MS/s)",
                design::fmt_rate(rate),
                design::fmt_rate(SYMBOL_RATE)
            )));
        }
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = Some(BIT_RATE / 8.0);
            o.center_freq = None;
        }
        text_meta(cx, 1);
        let mut fe = FrontEnd::new(rate, SYMBOL_RATE, Psk::Qpsk, ROLLOFF);
        fe.set_offset(self.offset);
        let h0 = Hypothesis::nth(0);
        self.st = Some(Box::new(DemodState {
            fe,
            search: Search::new(),
            hyp: None,
            depunct: Depuncturer::new(h0),
            vit: Viterbi::new(),
            deframer: Deframer::new(deframer_config()).map_err(Error::Other)?,
            tried: 0,
            unlocked_bits: 0,
            pause: 0,
            acq: Vec::with_capacity(SEARCH_SYMBOLS),
            syms: Vec::with_capacity(CHUNK + 8),
            soft: Vec::with_capacity(CHUNK + 8),
            pairs: Vec::with_capacity(2 * CHUNK + 8),
            bits: Vec::with_capacity(2 * CHUNK + 512),
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
            Self::report(st, &mut self.msgs);
        }
        if used == 0 && made == 0 && msg_made == 0 {
            let waiting = !st.fifo.is_empty() || !self.msgs.is_empty();
            if waiting && io.inputs[0].exhausted() && !io.outputs[0].abandoned() {
                return Ok(WorkStatus::Wait(Duration::from_millis(20)));
            }
            if io.inputs[0].exhausted() && st.hyp.is_some() {
                // The end: what the Viterbi decoder still holds.
                st.flush();
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

fn build_demodulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(AhrptDemodulator {
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

// ---- Encoder: synthetic AVHRR lines → CADUs --------------------------------------

/// Metop-B's spacecraft id, which the encoder sends.
pub const SPACECRAFT: u8 = 11;
/// The virtual channel of the encoder's other instruments' packets.
const VCID_OTHER: u8 = 12;
/// Day (since 2000-01-01) and microseconds of the day the encoder's first
/// line is stamped with.
pub const TEST_DAY: u16 = 9000;
pub const TEST_START_US: u64 = 36_000_000_000;
/// Microseconds a line takes (six a second), times 6.
const LINE_US_X6: u64 = 1_000_000;
/// AVHRR lines between MHS packets.
const MHS_EVERY: u64 = 16;

/// Builds the packets and frames of synthetic AVHRR/3 lines, optionally
/// with fill frames so that a line takes its 1/6 s of the link.
pub struct AhrptSource {
    avhrr: FrameBuilder,
    other: FrameBuilder,
    paced: bool,
    frames: u64,
    fill: Vec<u8>,
    fill_count: u32,
    seq_avhrr: u16,
    seq_mhs: u16,
    line: u64,
    packet: Vec<u8>,
}

/// The time stamp (CCSDS day segmented: day, ms of day, µs of ms) of line
/// `n` of the encoder.
pub fn line_time(n: u64) -> [u8; 8] {
    let us = TEST_START_US + n * LINE_US_X6 / 6;
    let mut t = [0u8; 8];
    t[0..2].copy_from_slice(&TEST_DAY.to_be_bytes());
    t[2..6].copy_from_slice(&((us / 1000) as u32).to_be_bytes());
    t[6..8].copy_from_slice(&((us % 1000) as u16).to_be_bytes());
    t
}

/// Write a 10-bit word at `word` (MSB first) into `buf`.
fn put10(buf: &mut [u8], word: usize, v: u16) {
    let bit = word * 10;
    let (byte, shift) = (bit / 8, bit % 8);
    // 10 bits within the next two or three bytes.
    let w = u32::from(v & 0x3FF) << (22 - shift);
    for (k, b) in w.to_be_bytes()[..3].iter().enumerate() {
        if let Some(d) = buf.get_mut(byte + k) {
            *d |= b;
        }
    }
}

impl AhrptSource {
    pub fn new(paced: bool) -> Self {
        let layout = Layout {
            frame_len: VCDU_LEN,
            insert_zone: 2,
            trailer: 0,
        };
        Self {
            avhrr: FrameBuilder::new(layout, SPACECRAFT, VCID_AVHRR).expect("valid layout"),
            other: FrameBuilder::new(layout, SPACECRAFT, VCID_OTHER).expect("valid layout"),
            paced,
            frames: 0,
            fill: vec![0; VCDU_LEN],
            fill_count: 0,
            seq_avhrr: 0,
            seq_mhs: 0,
            line: 0,
            packet: Vec::with_capacity(6 + 2 * WORDS),
        }
    }

    pub fn lines(&self) -> u64 {
        self.line
    }

    fn header(&mut self, apid: u16, seq: u16) {
        let h = PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: true,
            apid,
            seq_flags: 3,
            seq,
            data_len: self.packet.len() - 6,
        };
        h.write(&mut self.packet[..6]);
    }

    /// The next line's packet (and now and then an MHS packet), then fill
    /// frames up to the line's share of the link if paced; transfer frames
    /// (892 bytes) go to `out`.
    pub fn line(&mut self, out: &mut impl FnMut(&[u8])) {
        let mut frames = 0u64;
        let mut count = |f: &[u8]| {
            frames += 1;
            out(f);
        };
        let n = self.line;
        let time = line_time(n);
        // AVHRR: time, 6 bytes of on-board time, 10 355 ten-bit words
        // (12 944 bytes), 2 bytes of packet error control.
        self.packet.clear();
        self.packet.resize(6, 0);
        self.packet.extend_from_slice(&time);
        self.packet.resize(6 + 14 + 12_944 + 2, 0);
        {
            let words = &mut self.packet[6 + 14..6 + 14 + 12_944];
            for w in 0..50 {
                put10(words, w, 900);
            }
            for p in 0..2048 {
                for c in 0..5 {
                    put10(words, 55 + 5 * p + c, avhrr_pattern(c, p, n as usize));
                }
            }
            for w in 10_305..WORDS {
                put10(words, w, 500);
            }
        }
        let seq = self.seq_avhrr;
        self.seq_avhrr = (seq + 1) & 0x3FFF;
        self.header(APID_3A, seq);
        let packet = std::mem::take(&mut self.packet);
        self.avhrr.push_packet(&packet, &mut count);
        self.packet = packet;
        if n.is_multiple_of(MHS_EVERY) {
            self.packet.clear();
            self.packet.resize(6, 0);
            self.packet.extend_from_slice(&time);
            self.packet.resize(6 + 14 + 1274, 0);
            for (k, b) in self.packet[20..].iter_mut().enumerate() {
                *b = (k * 7 + n as usize) as u8;
            }
            let seq = self.seq_mhs;
            self.seq_mhs = (seq + 1) & 0x3FFF;
            self.header(APID_MHS, seq);
            let packet = std::mem::take(&mut self.packet);
            self.other.push_packet(&packet, &mut count);
            self.other.flush(&mut count);
            self.packet = packet;
        }
        self.frames += frames;
        self.line += 1;
        if !self.paced {
            return;
        }
        let due = (self.line as f64 / 6.0 * FRAMES_PER_SECOND) as u64;
        while self.frames < due {
            VcduHeader {
                version: 1,
                spacecraft: SPACECRAFT,
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

    pub fn flush(&mut self, out: &mut impl FnMut(&[u8])) {
        self.avhrr.flush(out);
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
    src: AhrptSource,
    limit: u64,
    fifo: ByteFifo,
    flushed: bool,
}

/// A paced line is about 71 frames.
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
            if self.limit == 0 || self.src.lines() < self.limit {
                self.src.line(&mut out);
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
    let paced = match p.str("pace")? {
        "air" => true,
        "packed" => false,
        other => return Err(Error::Other(format!("unknown pace `{other}`"))),
    };
    Ok(Instance {
        block: Box::new(EncoderBlock {
            src: AhrptSource::new(paced),
            limit: p.u64("lines")?,
            fifo: ByteFifo::new(ENC_FIFO),
            flushed: false,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Modulator ------------------------------------------------------------------

/// Codeblocks → shaped baseband.
pub struct AhrptTransmitter {
    enc: Encoder,
    punct: Puncturer,
    syms: Vec<Complex32>,
    shaper: Resampler<Complex32>,
    sps: usize,
}

impl AhrptTransmitter {
    pub fn new(sps: usize) -> Self {
        let h = taps::rrc(sps as f64, ROLLOFF, 12, sps as f64);
        Self {
            enc: Encoder::new(deframer_config()).expect("valid config"),
            punct: Puncturer::new(),
            syms: Vec::with_capacity(8 * (4 + FRAME_LEN) * 2 / 3 + 8),
            shaper: Resampler::new(sps, 1, &h),
            sps,
        }
    }

    /// One codeblock (1020 bytes, parity zeros) → samples appended to `out`.
    pub fn frame(&mut self, codeblock: &[u8], out: &mut Vec<Complex32>) {
        let a = std::f32::consts::FRAC_1_SQRT_2;
        let level = |b: u8| if b == 0 { a } else { -a };
        self.syms.clear();
        let (syms, punct) = (&mut self.syms, &mut self.punct);
        self.enc.encode(codeblock, |s| {
            punct.push(u8::from(s.re < 0.0), |[i, q]| {
                syms.push(Complex32::new(level(i), level(q)));
            });
        });
        let start = out.len();
        out.resize(start + self.syms.len() * self.sps, Complex32::default());
        let (_, made) = self.shaper.process(&self.syms, &mut out[start..]);
        out.truncate(start + made);
    }
}

pub struct ModulatorBlock {
    tx: AhrptTransmitter,
    reader: CaduReader,
    out: Vec<Complex32>,
    pos: usize,
}

impl Block for ModulatorBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = Some(SYMBOL_RATE * self.tx.sps as f64);
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
            "samples per symbol must be 2 to 16 (got {sps})"
        )));
    }
    Ok(Instance {
        block: Box::new(ModulatorBlock {
            out: Vec::with_capacity(sps * 8 * (4 + FRAME_LEN) + 64),
            tx: AhrptTransmitter::new(sps),
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

    fn bits(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| ((i * 7919 + i / 5) % 3 == 0) as u8)
            .collect()
    }

    /// Clean symbols through every hypothesis: the one that undoes the
    /// channel decodes the data without disagreement. (So does at least one
    /// other, which decodes something else: a stream with G2 inverted is
    /// also a codeword of the punctured code. The deframer tells them apart
    /// by the ASM.)
    #[test]
    fn the_right_hypothesis_scores_best_and_decodes() {
        let data = bits(3 * 3000);
        let mut p = Puncturer::new();
        let mut syms = Vec::new();
        for &b in &data {
            p.push(b, |[i, q]| {
                let l = |v: u8| if v == 0 { 60i8 } else { -60 };
                syms.push([l(i), l(q)]);
            });
        }
        let mut search = Search::new();
        for skew in [0usize, 1] {
            for rot in 0..4u8 {
                for conj in [false, true] {
                    let s = Symmetry::new(rot, conj);
                    let rx: Vec<[i8; 2]> = syms[skew..].iter().map(|&v| s.apply(v)).collect();
                    let mut good = 0;
                    let mut decoded = false;
                    for h in 0..HYPOTHESES {
                        let score = search.score(&rx[..SEARCH_SYMBOLS], Hypothesis::nth(h));
                        if score > 0.01 {
                            continue;
                        }
                        good += 1;
                        let mut d = Depuncturer::new(Hypothesis::nth(h));
                        let mut pairs = Vec::new();
                        for &v in &rx {
                            d.push(v, &mut pairs);
                        }
                        let mut vit = Viterbi::new();
                        let mut out = Vec::new();
                        vit.decode(&pairs, |b| out.extend_from_slice(b));
                        vit.flush(|b| out.extend_from_slice(b));
                        // Decoding starts at data bit 0, or 1 without the
                        // first symbol; maybe inverted.
                        let want = &data[2..2002];
                        decoded |= (0..4).any(|lag| {
                            let got = &out[lag..lag + 2000];
                            got == want || got.iter().zip(want).all(|(a, b)| a != b)
                        });
                    }
                    assert!(decoded, "rot {rot} conj {conj} skew {skew}: data not found");
                    assert!(good <= 4, "rot {rot} conj {conj} skew {skew}: {good} fit");
                }
            }
        }
    }

    fn unpack(bytes: &[u8], n: usize) -> Vec<u8> {
        (0..n).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1).collect()
    }

    /// Against scikit-commpy's encoder (`scripts/ahrpt-conv-vectors.py`):
    /// the puncturer makes the same serial stream G1 G2 G2 G1 (sent as
    /// (G1₁, G2₁), (G1₃, G2₂)), and the depuncturer and Viterbi decoder take
    /// it back to the message.
    #[test]
    fn matches_commpy() {
        let f = include_bytes!("../../tests/fixtures/ahrpt/conv34.bin");
        let n = u32::from_le_bytes([f[0], f[1], f[2], f[3]]) as usize;
        let msg = unpack(&f[4..], n);
        let code = unpack(&f[4 + n.div_ceil(8)..], 4 * n / 3);
        let mut p = Puncturer::new();
        let mut serial = Vec::new();
        let mut second = false;
        for &b in &msg {
            p.push(b, |[i, q]| {
                if second {
                    serial.extend_from_slice(&[q, i]);
                } else {
                    serial.extend_from_slice(&[i, q]);
                }
                second = !second;
            });
        }
        assert_eq!(serial, code);
        let l = |v: u8| if v == 0 { 60i8 } else { -60 };
        let mut d = Depuncturer::new(Hypothesis::nth(0));
        let mut pairs = Vec::new();
        for q in code.chunks(4) {
            d.push([l(q[0]), l(q[1])], &mut pairs);
            d.push([l(q[3]), l(q[2])], &mut pairs);
        }
        let mut vit = Viterbi::new();
        let mut out = Vec::new();
        vit.decode(&pairs, |b| out.extend_from_slice(b));
        vit.flush(|b| out.extend_from_slice(b));
        assert_eq!(out, msg);
    }

    #[test]
    fn put10_packs_msb_first() {
        let mut b = [0u8; 5];
        for (w, v) in [0x3FF, 0x000, 0x2AA, 0x155].into_iter().enumerate() {
            put10(&mut b, w, v);
        }
        assert_eq!(b, [0xFF, 0xC0, 0x0A, 0xA9, 0x55]);
    }
}
