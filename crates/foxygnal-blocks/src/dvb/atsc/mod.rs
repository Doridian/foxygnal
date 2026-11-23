//! ATSC 1.0 (A/53 Part 2), 8-VSB: `atsc_modulator` (TS → baseband) and
//! `atsc_demodulator` (baseband → TS, plus a status text output).
//!
//! ## Signal
//!
//! 10.762238 MSym/s (4.5 MHz × 684/286) of 8-level symbols in segments of
//! 832 (4 segment sync symbols, 828 data), 313 segments a field (one field
//! sync, 312 data segments carrying a TS packet each: 19.39 Mbit/s). A pilot
//! (a DC offset of 1.25 on the levels) sits 309.44 kHz above the lower band
//! edge. The vestigial sideband: the real symbol stream is shifted down by
//! Rs/4 and filtered by a root-raised cosine (α = 0.1152) for a "symbol" of
//! two symbols, centred on the channel, so the 6 MHz channel holds the upper
//! sideband of the symbols and a 310 kHz vestige of the lower one.
//!
//! ## Transmitter
//!
//! [`fec::Encoder`] (randomizer, RS(207,187), byte interleaver, trellis
//! coder, syncs), then the pilot, the shift by −Rs/4 and the RRC, as a
//! polyphase interpolator to any output rate.
//!
//! ## Receiver
//!
//! 1. **Pilot search**: an averaged FFT finds the pilot within ±250 kHz of
//!    its place (at the upper edge if the spectrum is inverted); an NCO
//!    removes the offset.
//! 2. **Matched filter and resampling in one**: the receive RRC, evaluated
//!    at any input position (256 phases), gives one value per symbol at the
//!    instants the timing loop asks for, at any input rate ≥ 6.25 MS/s.
//! 3. **Carrier**: rotating the k-th output by j^k puts the pilot at DC and
//!    the symbols in the real part; a PLL on the low-passed pilot.
//! 4. **Segment sync**: the correlation with +5 −5 −5 +5, accumulated per
//!    position mod 832, finds the segment; then **timing** from the same
//!    four symbols: Σ s·(late − early) at ± half a symbol (the neighbours'
//!    contributions cancel), a second-order loop that moves the filter's
//!    position and step (the clock offset).
//! 5. **Field sync**: the PN511 correlation in each segment; its sign is the
//!    polarity, the middle PN63 against the first tells the field.
//! 6. **Equalizer**: a DFE, 64 feed-forward taps (40 ahead) and 192
//!    feedback taps (17.8 µs of post-echo), LMS-trained on the field sync's
//!    known symbols (replayed a few times while new) and the segment syncs,
//!    decision-directed in between.
//! 7. **Decoding**: 12 four-state Viterbi decoders (the two parallel
//!    transitions of each branch, the precoder undone after), the byte
//!    deinterleaver, RS and the derandomizer ([`fec`]).

pub mod fec;
mod rx;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};
use foxygnal_dsp::{kernels, taps::rrc_at};

use super::{ByteFifo, SYNC, TS_LEN};
use crate::design::{self, fmt_rate};
use crate::digital::{MsgQueue, text_meta};
use crate::filters::not_configured;
use crate::params;

use fec::{Encoder, FIELD_SEGMENTS, SEGMENT, level};
use rx::{Phase, Receiver};

/// Symbols per second: 4.5 MHz × 684 / 286.
pub const SYMBOL_RATE: f64 = 4.5e6 * 684.0 / 286.0;
/// Excess bandwidth of the RRC.
pub const ROLLOFF: f64 = 0.1152;
/// The pilot: a DC offset on the symbol levels.
pub const PILOT: f32 = 1.25;
/// The lowest sample rate either block takes: the 6 MHz channel and a little.
pub const MIN_RATE: f64 = 6.25e6;

/// TS bytes per second an ATSC channel carries (19.39 Mbit/s).
pub fn ts_byte_rate() -> f64 {
    SYMBOL_RATE / SEGMENT as f64 * FIELD_SEGMENTS as f64 / (FIELD_SEGMENTS + 1) as f64
        * TS_LEN as f64
}

/// Input samples per processing step of the demodulator.
const CHUNK: usize = 8192;

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "atsc_modulator",
            label: "ATSC Modulator",
            category: "Digital",
            help: "ATSC 1.0 (A/53) 8-VSB transmitter: an MPEG transport stream (whole \
                   188-byte packets starting with 0x47) in, complex baseband out, centred \
                   on the 6 MHz channel (the pilot 2.690559 MHz below the centre). \
                   Randomizer, RS(207,187), the 52-segment byte interleaver, the 2/3 \
                   trellis code (12 interleaved encoders with precoders), segment and \
                   field syncs, the pilot and the VSB filter (root-raised cosine, 11.5% \
                   excess bandwidth). The TS must arrive at 19.39 Mbit/s (e.g. a throttled \
                   file). Bytes before a sync byte are skipped.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: vec![
                design::float_min(
                    "sample_rate",
                    "Sample rate",
                    SYMBOL_RATE,
                    MIN_RATE,
                    Some("Hz"),
                    "Output sample rate: the symbol rate (10.762238 MS/s) or any other rate \
                     of at least 6.25 MS/s.",
                ),
                params::float("gain", "Level", 0.25, None, "RMS amplitude of the output."),
            ],
        },
        build_modulator,
    );
    r.set_keywords(
        "atsc_modulator",
        &[
            "atsc",
            "8vsb",
            "vsb",
            "digital tv",
            "transport stream",
            "mpeg-ts",
        ],
    );
    r.register(
        BlockDescriptor {
            kind: "atsc_demodulator",
            label: "ATSC Demodulator",
            category: "Digital",
            help: "ATSC 1.0 (A/53) 8-VSB receiver: complex baseband centred on the 6 MHz \
                   channel in, at any rate of at least 6.25 MS/s (10.76 MS/s or more is \
                   best; it resamples internally), MPEG transport stream out on `ts`: \
                   whole 188-byte packets, those RS couldn't correct with the \
                   transport_error_indicator set. Finds the pilot (±250 kHz), timing and \
                   sample clock from the segment syncs, the field syncs, and equalizes \
                   echoes with a decision-feedback equalizer (−3.7 to +17.8 µs) trained on \
                   the field syncs. `status` has one text line per second and one per \
                   lock change: `t=… state=lock mer_db=… freq_hz=… clock_ppm=… \
                   rs_corrected=… rs_failed=… packets=… pps=…`.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![port("ts", StreamType::U8), port("status", StreamType::U8)],
            params: vec![],
        },
        build_demodulator,
    );
    r.set_keywords(
        "atsc_demodulator",
        &[
            "atsc",
            "8vsb",
            "vsb",
            "digital tv",
            "transport stream",
            "mpeg-ts",
            "equalizer",
            "viterbi",
            "reed-solomon",
        ],
    );
}

// ---- Modulator ------------------------------------------------------------------

/// The TX filter spans ±16 symbols.
const TX_HALF: usize = 16;
const TX_PHASES: usize = 512;

/// Symbols (with the pilot, shifted by −Rs/4) → the RRC at any rate.
struct Shaper {
    bank: Vec<f32>,
    /// Symbols; `hist[0]` is the oldest still needed.
    hist: Vec<Complex32>,
    /// Time of the next output, in symbols from `hist[0]`.
    t: f64,
    /// Symbols per output sample.
    ratio: f64,
    k: u64,
    scale: f32,
    dot: fn(&[f32], &[f32]) -> Complex32,
}

impl Shaper {
    fn new(out_rate: f64, gain: f64) -> Self {
        let taps = 2 * TX_HALF;
        let rows: Vec<Vec<f64>> = (0..=TX_PHASES)
            .map(|ph| {
                let frac = ph as f64 / TX_PHASES as f64;
                (0..taps)
                    .map(|m| rrc_at((frac + TX_HALF as f64 - 1.0 - m as f64) / 2.0, ROLLOFF))
                    .collect()
            })
            .collect();
        // Normalize so a symbol's peak through both filters is 1 at 1 sps.
        let peak: f64 = rows[0].iter().map(|v| v * v).sum::<f64>().sqrt();
        // Output power: the data (mean level² 21) plus the pilot's line.
        let n = rows.len() as f64;
        let data: f64 = rows
            .iter()
            .map(|r| r.iter().map(|v| v * v).sum::<f64>())
            .sum::<f64>()
            / n;
        let pilot: f64 = rows
            .iter()
            .map(|r| {
                let (mut re, mut im) = (0.0, 0.0);
                for (m, v) in r.iter().enumerate() {
                    match m & 3 {
                        0 => re += v,
                        1 => im -= v,
                        2 => re -= v,
                        _ => im += v,
                    }
                }
                re * re + im * im
            })
            .sum::<f64>()
            / n;
        let power = 21.0 * data + (PILOT as f64).powi(2) * pilot;
        let mut bank = Vec::with_capacity(rows.len() * taps * 2);
        for r in &rows {
            for &v in r {
                bank.push((v / peak) as f32);
                bank.push((v / peak) as f32);
            }
        }
        let scale = (gain * peak / power.sqrt()) as f32;
        let mut hist = Vec::with_capacity(4 * SEGMENT);
        hist.resize(TX_HALF, Complex32::default());
        Self {
            bank,
            hist,
            t: (TX_HALF - 1) as f64,
            ratio: SYMBOL_RATE / out_rate,
            k: 0,
            scale,
            dot: kernels().dot_c32,
        }
    }

    fn push(&mut self, index: u8) {
        let l = (level(index) + PILOT) * self.scale;
        // × (−j)^k: the symbols' spectrum moved down by Rs/4.
        let s = match self.k & 3 {
            0 => Complex32::new(l, 0.0),
            1 => Complex32::new(0.0, -l),
            2 => Complex32::new(-l, 0.0),
            _ => Complex32::new(0.0, l),
        };
        self.k += 1;
        self.hist.push(s);
    }

    /// Symbols still to be pushed before another output can be made.
    fn wants(&self) -> bool {
        self.t as usize + TX_HALF >= self.hist.len()
    }

    fn produce(&mut self, out: &mut [Complex32]) -> usize {
        let taps = 2 * TX_HALF;
        let x: &[f32] = bytemuck::cast_slice(&self.hist);
        let mut made = 0;
        for o in out.iter_mut() {
            let k0 = self.t as usize;
            if k0 + TX_HALF >= self.hist.len() {
                break;
            }
            let ph = ((self.t - k0 as f64) * TX_PHASES as f64 + 0.5) as usize;
            let base = k0 + 1 - TX_HALF;
            *o = (self.dot)(
                &self.bank[ph * 2 * taps..(ph + 1) * 2 * taps],
                &x[2 * base..2 * (base + taps)],
            );
            self.t += self.ratio;
            made += 1;
        }
        let drop = (self.t as usize).saturating_sub(TX_HALF + 2);
        if drop > 0 {
            self.hist.drain(..drop);
            self.t -= drop as f64;
        }
        made
    }
}

pub struct AtscModulator {
    rate: f64,
    enc: Encoder,
    syms: Vec<u8>,
    sym_pos: usize,
    shaper: Shaper,
    packet: [u8; TS_LEN],
}

impl AtscModulator {
    fn new(rate: f64, gain: f64) -> Self {
        Self {
            rate,
            enc: Encoder::new(),
            syms: Vec::with_capacity(14 * SEGMENT),
            sym_pos: 0,
            shaper: Shaper::new(rate, gain),
            packet: [0; TS_LEN],
        }
    }
}

impl Block for AtscModulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(self.rate);
        }
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        loop {
            let out = &mut io.outputs[0].slice_mut::<Complex32>()[made..];
            if out.is_empty() {
                break;
            }
            made += self.shaper.produce(out);
            if !self.shaper.wants() {
                break;
            }
            if self.sym_pos < self.syms.len() {
                // A segment's worth at a time.
                let end = (self.sym_pos + SEGMENT).min(self.syms.len());
                for &s in &self.syms[self.sym_pos..end] {
                    self.shaper.push(s);
                }
                self.sym_pos = end;
                continue;
            }
            self.syms.clear();
            self.sym_pos = 0;
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.len() < TS_LEN {
                break;
            }
            if inp[0] != SYNC {
                used += inp.iter().position(|&b| b == SYNC).unwrap_or(inp.len());
                continue;
            }
            self.packet.copy_from_slice(&inp[..TS_LEN]);
            used += TS_LEN;
            self.enc.push(&self.packet, &mut self.syms);
        }
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn read_rate(p: &ParamReader<'_>) -> Result<f64, Error> {
    let rate = p.f64("sample_rate")?;
    if !(rate >= MIN_RATE && rate.is_finite()) {
        return Err(Error::Other(format!(
            "the ATSC modulator's sample rate must be at least {} (got {})",
            fmt_rate(MIN_RATE),
            fmt_rate(rate)
        )));
    }
    Ok(rate)
}

fn build_modulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let m = AtscModulator::new(read_rate(p)?, p.f64("gain")?);
    Ok(Instance {
        block: Box::new(m),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}

// ---- Demodulator ----------------------------------------------------------------

/// TS FIFO size, and the room kept free before taking more input (a chunk
/// is at most ~1300 symbols, under 2 packets at 12 MS/s; far fewer than
/// this even at the lowest rate).
const FIFO_LEN: usize = 1 << 16;
const FIFO_MARGIN: usize = 1 << 14;
/// Status lines are dropped rather than queued beyond this.
const MSG_BACKLOG: usize = 2048;

struct State {
    rx: Box<Receiver>,
    fifo: ByteFifo,
    in_rate: f64,
    samples: u64,
    next_report: u64,
    last_packets: u64,
    last_samples: u64,
}

pub struct AtscDemodulator {
    state: Option<State>,
    msgs: MsgQueue,
}

impl AtscDemodulator {
    fn process(st: &mut State, msgs: &mut MsgQueue, input: &[Complex32]) {
        st.rx.process(input, &mut st.fifo);
        st.samples += input.len() as u64;
        let t = st.samples as f64 / st.in_rate;
        if std::mem::take(&mut st.rx.lock_changed) && msgs.pending() < MSG_BACKLOG {
            if st.rx.locked() {
                msgs.line(format_args!(
                    "t={t:.3} event=lock freq_hz={:+.0} clock_ppm={:+.1} spectrum={} field={}",
                    st.rx.offset_hz(),
                    st.rx.clock() * 1e6,
                    if st.rx.inverted { "inverted" } else { "normal" },
                    if st.rx.second_field() { 2 } else { 1 },
                ));
            } else {
                msgs.line(format_args!("t={t:.3} event=unlock"));
            }
        }
        if st.samples >= st.next_report {
            st.next_report += st.in_rate.round().max(1.0) as u64;
            Self::report(st, msgs, t);
        }
    }

    fn report(st: &mut State, msgs: &mut MsgQueue, t: f64) {
        let mer = st.rx.take_mer();
        let outer = &st.rx.outer;
        let secs = (st.samples - st.last_samples) as f64 / st.in_rate;
        let pps = (outer.packets - st.last_packets) as f64 / secs.max(1e-9);
        st.last_packets = outer.packets;
        st.last_samples = st.samples;
        if msgs.pending() >= MSG_BACKLOG {
            return;
        }
        let state = match st.rx.phase {
            Phase::Coarse | Phase::Search => "search",
            Phase::Segment => "sync",
            Phase::Field => "lock",
        };
        msgs.text(format_args!("t={t:.3} state={state}"));
        if let Some(m) = mer {
            msgs.text(format_args!(" mer_db={m:.1}"));
        }
        msgs.line(format_args!(
            " freq_hz={:+.0} clock_ppm={:+.1} rs_corrected={} rs_failed={} packets={} \
             pps={pps:.0} fields={} pn63_errors={}",
            st.rx.offset_hz(),
            st.rx.clock() * 1e6,
            outer.corrected,
            outer.failed,
            outer.packets,
            st.rx.fields,
            st.rx.pn63_errors,
        ));
    }
}

impl Block for AtscDemodulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let in_rate = cx.input_rate(0)?;
        if in_rate < MIN_RATE {
            return Err(Error::Other(format!(
                "the input rate {} is below {}: the ATSC demodulator needs the whole \
                 6 MHz channel (the symbol rate, 10.762238 MS/s, or more is best)",
                fmt_rate(in_rate),
                fmt_rate(MIN_RATE)
            )));
        }
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = None;
            o.center_freq = None;
        }
        text_meta(cx, 1);
        self.state = Some(State {
            rx: Box::new(Receiver::new(in_rate, CHUNK)),
            fifo: ByteFifo::new(FIFO_LEN),
            in_rate,
            samples: 0,
            next_report: in_rate.round().max(1.0) as u64,
            last_packets: 0,
            last_samples: 0,
        });
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let st = self.state.as_mut().ok_or_else(not_configured)?;
        let (mut used, mut ts_made, mut msg_made) = (0, 0, 0);
        loop {
            ts_made += st
                .fifo
                .read_into(&mut io.outputs[0].slice_mut::<u8>()[ts_made..]);
            msg_made += self
                .msgs
                .flush(&mut io.outputs[1].slice_mut::<u8>()[msg_made..]);
            let inp = io.inputs[0].slice::<Complex32>();
            if used == inp.len() || st.fifo.free() < FIFO_MARGIN {
                break;
            }
            let n = (inp.len() - used).min(CHUNK);
            Self::process(st, &mut self.msgs, &inp[used..used + n]);
            used += n;
        }
        if used == 0 && ts_made == 0 && msg_made == 0 {
            let waiting = !st.fifo.is_empty() || !self.msgs.is_empty();
            if waiting && io.inputs[0].exhausted() && !io.outputs[0].abandoned() {
                return Ok(WorkStatus::Wait(std::time::Duration::from_millis(20)));
            }
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, ts_made);
        io.produce(1, msg_made);
        Ok(WorkStatus::Ok)
    }
}

fn build_demodulator(_p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(AtscDemodulator {
            state: None,
            msgs: MsgQueue::new(),
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![
            ("ts".into(), StreamType::U8),
            ("status".into(), StreamType::U8),
        ],
    })
}
