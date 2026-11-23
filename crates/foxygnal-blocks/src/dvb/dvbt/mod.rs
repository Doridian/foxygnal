//! The DVB-T modem (EN 300 744): `dvbt_modulator` (TS → OFDM baseband) and
//! `dvbt_demodulator` (baseband → TS, plus a status text output).
//! Non-hierarchical; 2k and 8k; QPSK, 16-QAM, 64-QAM; code rates 1/2–7/8;
//! guard intervals 1/4–1/32; 5–8 MHz channels.
//!
//! ## Transmitter
//!
//! TS packets → energy dispersal, RS(204,188), Forney interleaver, the
//! punctured convolutional code (all shared with DVB-S) → the inner
//! interleaver (bit demultiplexing, six 126-bit interleavers, the symbol
//! interleaver, one gather table per even/odd symbol) → Gray mapping onto the
//! data carriers, plus continual and scattered pilots (boosted 4/3, from the
//! PRBS) and the TPS carriers (DBPSK, BCH-protected) → IFFT → cyclic prefix.
//!
//! ## Receiver
//!
//! 1. An arbitrary-ratio resampler takes the input to the elementary rate
//!    and absorbs the sampling clock offset (steered by step 6).
//! 2. **Detect**: over ~70 k samples, the cyclic prefix correlation folded at
//!    every (mode, guard) period: the strongest gives the mode, the guard,
//!    the symbol timing and the fractional frequency offset.
//! 3. **Integer frequency offset**: the continual pilots of consecutive
//!    symbols correlated at every shift within the band edges.
//! 4. Per symbol: NCO, FFT of the useful part (the guard isn't even
//!    resampled), and from the continual pilots against the previous
//!    symbol: the common phase step (→ frequency loop, and the derotation
//!    that keeps the channel estimate coherent) and the phase slope across
//!    the band (→ the timing drift → clock loop → resampler).
//! 5. **TPS**: one differential bit per symbol, soft-combined over the TPS
//!    carriers; the sync word gives the frame position, then BCH(67,53)
//!    (up to 2 errors corrected) the constellation, code rate, guard, mode
//!    and cell id.
//! 6. **Channel**: the scattered pilots of each symbol into a grid of every
//!    third carrier; the symbol 3 back is equalized with the grid linearly
//!    interpolated in time (pilots 4 symbols apart), then in frequency with
//!    16-tap fractional-delay filters after centring the delays (cut off just
//!    above ± G/2). Its mean delay steers the timing slowly.
//! 7. Equalized carriers → max-log soft bits weighted by |H|² (CSI) → the
//!    inner deinterleaver (one gather) → depuncturing (the phase is found by
//!    re-encoded BER) → Viterbi → packet sync → outer deinterleaver → RS →
//!    energy dispersal removal.

pub(super) mod frame;
mod resample;
mod rx;

use std::sync::Arc;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};
use rustfft::{Fft, FftPlanner};

use super::conv::{CodeRate, ConvEncoder};
use super::outer::OuterEncoder;
use super::{ByteFifo, RS_LEN, SYNC, TS_LEN};
use crate::design::{self, fmt_rate};
use crate::digital::{MsgQueue, text_meta};
use crate::filters::not_configured;
use crate::params;

pub use frame::{Constellation, Guard, Mode, elementary_rate, ts_bit_rate};
use frame::{FRAME_SYMBOLS, Layout, PILOT_BOOST, SUPERFRAME, Tps, map, tables};
use rx::{Phase, Receiver};

/// Input samples per processing step of the demodulator.
pub(super) const CHUNK: usize = 8192;

const RATE_NAMES: [&str; 5] = ["1/2", "2/3", "3/4", "5/6", "7/8"];
const BANDWIDTHS: [&str; 4] = ["8", "7", "6", "5"];

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

fn bandwidth_param() -> foxygnal_core::ParamSpec {
    design::choice(
        "bandwidth",
        "Bandwidth",
        BANDWIDTHS.to_vec(),
        "8",
        "Channel bandwidth in MHz. The sample rate is 64/7 MHz × bandwidth / 8 \
         (9.142857 MS/s for 8 MHz, 8 MS/s for 7 MHz, 6.857143 MS/s for 6 MHz).",
    )
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "dvbt_modulator",
            label: "DVB-T Modulator",
            category: "Digital",
            help: "DVB-T (EN 300 744) transmitter, non-hierarchical: an MPEG transport \
                   stream (whole 188-byte packets starting with 0x47) in, OFDM baseband out \
                   at the elementary rate (64/7 MHz × bandwidth / 8). Energy dispersal, \
                   RS(204,188), outer interleaving, the punctured convolutional code, inner \
                   (bit and symbol) interleaving, QPSK/16-QAM/64-QAM, continual and \
                   scattered pilots and the TPS (with the cell id), then the IFFT and the \
                   guard interval. The TS must arrive at the mode's useful bit rate (e.g. \
                   8k, 64-QAM, 2/3, 1/32 in 8 MHz: 24.13 Mbit/s), e.g. from a throttled \
                   file. Bytes before a sync byte are skipped.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: vec![
                design::choice("mode", "Mode", vec!["2k", "8k"], "8k", "FFT size."),
                design::choice(
                    "constellation",
                    "Constellation",
                    vec!["qpsk", "16qam", "64qam"],
                    "64qam",
                    "Modulation of the data carriers.",
                ),
                design::choice(
                    "rate",
                    "Code rate",
                    RATE_NAMES.to_vec(),
                    "2/3",
                    "Convolutional code rate.",
                ),
                design::choice(
                    "guard",
                    "Guard interval",
                    vec!["1/4", "1/8", "1/16", "1/32"],
                    "1/32",
                    "Cyclic prefix, as a fraction of the useful symbol. Longer guards \
                     tolerate longer echoes (and single-frequency networks).",
                ),
                bandwidth_param(),
                design::int(
                    "cell_id",
                    "Cell id",
                    0,
                    0,
                    "Cell identifier sent in the TPS (0–65535).",
                ),
                params::float(
                    "gain",
                    "Level",
                    0.25,
                    None,
                    "RMS amplitude of the output (OFDM peaks reach about 4× this).",
                ),
            ],
        },
        build_modulator,
    );
    r.set_keywords(
        "dvbt_modulator",
        &["dvb-t", "digital tv", "ofdm", "transport stream", "mpeg-ts"],
    );
    r.register(
        BlockDescriptor {
            kind: "dvbt_demodulator",
            label: "DVB-T Demodulator",
            category: "Digital",
            help: "DVB-T (EN 300 744) receiver, non-hierarchical: complex baseband in at \
                   the elementary rate (64/7 MHz × bandwidth / 8) or any higher rate (it \
                   resamples internally), MPEG transport stream out on `ts`: whole \
                   188-byte packets, those RS couldn't correct with the \
                   transport_error_indicator set. Finds the mode, guard, timing, carrier \
                   offset (up to the band edges) and sample clock offset by itself, and \
                   the constellation, code rate and cell id from the TPS. `status` has one \
                   text line per second and one per lock change: `t=… state=lock mode=8k \
                   guard=1/32 constellation=64qam rate=2/3 cell_id=… mer_db=… freq_hz=… \
                   clock_ppm=… ber=… rs_corrected=… rs_failed=… pps=…`.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![port("ts", StreamType::U8), port("status", StreamType::U8)],
            params: vec![
                design::choice(
                    "mode",
                    "Mode",
                    vec!["auto", "2k", "8k"],
                    "auto",
                    "FFT size; auto detects it.",
                ),
                design::choice(
                    "guard",
                    "Guard interval",
                    vec!["auto", "1/4", "1/8", "1/16", "1/32"],
                    "auto",
                    "auto detects it.",
                ),
                bandwidth_param(),
            ],
        },
        build_demodulator,
    );
    r.set_keywords(
        "dvbt_demodulator",
        &[
            "dvb-t",
            "digital tv",
            "ofdm",
            "transport stream",
            "mpeg-ts",
            "viterbi",
            "reed-solomon",
            "tps",
        ],
    );
}

fn read_bandwidth(p: &ParamReader<'_>) -> Result<f64, Error> {
    let s = p.str("bandwidth")?;
    s.parse::<f64>()
        .ok()
        .filter(|b| BANDWIDTHS.contains(&s) && *b > 0.0)
        .ok_or_else(|| Error::Other(format!("unknown bandwidth `{s}` (5, 6, 7 or 8 MHz)")))
}

fn parse<T>(p: &ParamReader<'_>, name: &str, f: impl Fn(&str) -> Option<T>) -> Result<T, Error> {
    let s = p.str(name)?;
    f(s).ok_or_else(|| Error::Other(format!("unknown {name} `{s}`")))
}

fn parse_auto<T>(
    p: &ParamReader<'_>,
    name: &str,
    f: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, Error> {
    if p.str(name)? == "auto" {
        return Ok(None);
    }
    parse(p, name, f).map(Some)
}

// ---- Modulator ------------------------------------------------------------------

pub struct DvbtModulator {
    cons: Constellation,
    guard: Guard,
    rate: f64,
    layout: &'static Layout,
    perm: &'static [Vec<u32>; 2],
    outer: OuterEncoder,
    conv: ConvEncoder,
    /// Coded bits not yet sent (one per byte).
    bits: Vec<u8>,
    sym_bits: usize,
    /// Bits per data carrier, scattered by the inner interleaver.
    carrier_bits: Vec<u8>,
    tps_bits: [[u8; FRAME_SYMBOLS]; SUPERFRAME],
    tps_state: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    fbuf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    scale: f32,
    /// The symbol being sent, guard included.
    out: Vec<Complex32>,
    out_pos: usize,
    l: usize,
    frame: usize,
    packet: [u8; TS_LEN],
    coded: [u8; RS_LEN],
}

impl DvbtModulator {
    #[allow(clippy::too_many_arguments)]
    fn new(
        mode: Mode,
        cons: Constellation,
        code: CodeRate,
        guard: Guard,
        bandwidth: f64,
        cell_id: u16,
        gain: f64,
    ) -> Self {
        let t = tables(mode);
        let layout = &t.layout;
        let n = layout.n;
        let g = guard.len(n);
        let nd = mode.data_carriers();
        let sym_bits = nd * cons.bits();
        let tps_bits = std::array::from_fn(|f| {
            Tps {
                frame: f as u8,
                constellation: cons,
                hierarchy: 0,
                rate_hp: code,
                rate_lp: CodeRate::R1_2,
                guard,
                mode,
                cell_id_byte: if f % 2 == 0 {
                    (cell_id >> 8) as u8
                } else {
                    cell_id as u8
                },
            }
            .bits()
        });
        let npilots = layout.k - nd - layout.tps.len();
        let power =
            nd as f32 + npilots as f32 * PILOT_BOOST * PILOT_BOOST + layout.tps.len() as f32;
        let fft = FftPlanner::new().plan_fft_inverse(n);
        Self {
            cons,
            guard,
            rate: elementary_rate(bandwidth),
            layout,
            perm: &t.inner(cons).perm,
            outer: OuterEncoder::new(),
            conv: ConvEncoder::new(code),
            bits: Vec::with_capacity(sym_bits + 2 * RS_LEN * 8 + 16),
            sym_bits,
            carrier_bits: vec![0; sym_bits],
            tps_bits,
            tps_state: vec![0.0; layout.tps.len()],
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            fft,
            fbuf: vec![Complex32::default(); n],
            scale: gain as f32 / power.sqrt(),
            out: Vec::with_capacity(n + g),
            out_pos: 0,
            l: 0,
            frame: 0,
            packet: [0; TS_LEN],
            coded: [0; RS_LEN],
        }
    }

    /// Code the packet in `self.packet` into bits.
    fn encode_packet(&mut self) {
        self.outer.encode(&self.packet, &mut self.coded);
        let bits = &mut self.bits;
        for &byte in &self.coded {
            self.conv.push_byte(byte, |b| bits.push(b));
        }
    }

    /// Build the next OFDM symbol from `sym_bits` coded bits.
    fn build_symbol(&mut self) {
        let layout = self.layout;
        let (n, l) = (layout.n, self.l);
        let v = self.cons.bits();
        for (&dst, &b) in self.perm[l % 2].iter().zip(&self.bits[..self.sym_bits]) {
            self.carrier_bits[dst as usize] = b;
        }
        self.bits.copy_within(self.sym_bits.., 0);
        self.bits.truncate(self.bits.len() - self.sym_bits);

        self.fbuf.fill(Complex32::default());
        let s = self.scale;
        let put = |fbuf: &mut [Complex32], c: usize, v: Complex32| fbuf[layout.bin(c)] = v * s;
        for &c in &layout.continual {
            let c = c as usize;
            put(
                &mut self.fbuf,
                c,
                Complex32::new(PILOT_BOOST * layout.prbs[c], 0.0),
            );
        }
        let mut c = 3 * (l % 4);
        while c < layout.k {
            put(
                &mut self.fbuf,
                c,
                Complex32::new(PILOT_BOOST * layout.prbs[c], 0.0),
            );
            c += 12;
        }
        let tps = &self.tps_bits[self.frame];
        for (i, &c) in layout.tps.iter().enumerate() {
            let c = c as usize;
            self.tps_state[i] = if l == 0 {
                layout.prbs[c]
            } else if tps[l] == 1 {
                -self.tps_state[i]
            } else {
                self.tps_state[i]
            };
            put(&mut self.fbuf, c, Complex32::new(self.tps_state[i], 0.0));
        }
        for (ci, &c) in layout.data[l % 4].iter().enumerate() {
            let (re, im) = map(self.cons, &self.carrier_bits[ci * v..(ci + 1) * v]);
            put(&mut self.fbuf, c as usize, Complex32::new(re, im));
        }
        self.fft
            .process_with_scratch(&mut self.fbuf, &mut self.scratch);
        let g = self.guard.len(n);
        self.out.clear();
        self.out.extend_from_slice(&self.fbuf[n - g..]);
        self.out.extend_from_slice(&self.fbuf);
        self.out_pos = 0;
        self.l += 1;
        if self.l == FRAME_SYMBOLS {
            self.l = 0;
            self.frame = (self.frame + 1) % SUPERFRAME;
        }
    }
}

impl Block for DvbtModulator {
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
            if self.out_pos < self.out.len() {
                let out = &mut io.outputs[0].slice_mut::<Complex32>()[made..];
                let n = out.len().min(self.out.len() - self.out_pos);
                if n == 0 {
                    break;
                }
                out[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                made += n;
                continue;
            }
            if self.bits.len() >= self.sym_bits {
                self.build_symbol();
                continue;
            }
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
            self.encode_packet();
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
    let cell_id = p.u64("cell_id")?;
    if cell_id > 0xFFFF {
        return Err(Error::Other(format!(
            "the cell id must be 0 to 65535 (got {cell_id})"
        )));
    }
    let m = DvbtModulator::new(
        parse(p, "mode", Mode::parse)?,
        parse(p, "constellation", Constellation::parse)?,
        parse(p, "rate", CodeRate::parse)?,
        parse(p, "guard", Guard::parse)?,
        read_bandwidth(p)?,
        cell_id as u16,
        p.f64("gain")?,
    );
    Ok(Instance {
        block: Box::new(m),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}

// ---- Demodulator ----------------------------------------------------------------

/// TS FIFO size, and the room kept free before taking more input (a chunk
/// yields at most ~2 symbols, 8 KB of TS).
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
    last_locked: bool,
    /// The TPS and cell id last announced.
    last_tps: Option<(Tps, Option<u16>)>,
    last_packets: u64,
    last_samples: u64,
    last_vit: (u64, u64),
}

pub struct DvbtDemodulator {
    mode: Option<Mode>,
    guard: Option<Guard>,
    rate: f64,
    state: Option<State>,
    msgs: MsgQueue,
}

impl DvbtDemodulator {
    fn process(st: &mut State, msgs: &mut MsgQueue, input: &[Complex32]) {
        st.rx.process(input, &mut st.fifo);
        st.samples += input.len() as u64;
        let t = st.samples as f64 / st.in_rate;
        let tps = st.rx.tps.map(|t| {
            (
                Tps {
                    frame: 0,
                    cell_id_byte: 0,
                    ..t
                },
                st.rx.cell_id(),
            )
        });
        if tps.is_some() && tps != st.last_tps && msgs.pending() < MSG_BACKLOG {
            msgs.text(format_args!("t={t:.3} event=tps"));
            Self::params(&st.rx, msgs);
            msgs.line(format_args!(" freq_hz={:+.0}", st.rx.offset_hz()));
        }
        st.last_tps = tps;
        let locked = st.rx.fec.locked();
        if locked != st.last_locked {
            st.last_locked = locked;
            if msgs.pending() < MSG_BACKLOG {
                if locked {
                    msgs.text(format_args!("t={t:.3} event=lock"));
                    Self::params(&st.rx, msgs);
                    msgs.line(format_args!(
                        " freq_hz={:+.0} clock_ppm={:+.1}",
                        st.rx.offset_hz(),
                        st.rx.clock * 1e6
                    ));
                } else {
                    msgs.line(format_args!("t={t:.3} event=unlock"));
                }
            }
        }
        if st.samples >= st.next_report {
            st.next_report += st.in_rate.round().max(1.0) as u64;
            Self::report(st, msgs, t);
        }
    }

    fn params(rx: &Receiver, msgs: &mut MsgQueue) {
        if rx.phase == Phase::Detect {
            return;
        }
        msgs.text(format_args!(
            " mode={} guard={}",
            rx.mode.name(),
            rx.guard.name()
        ));
        if let Some(tps) = rx.tps {
            msgs.text(format_args!(
                " constellation={} rate={} hierarchy={}",
                tps.constellation.name(),
                tps.rate_hp.name(),
                tps.hierarchy
            ));
        }
        if let Some(id) = rx.cell_id() {
            msgs.text(format_args!(" cell_id={id}"));
        }
    }

    fn report(st: &mut State, msgs: &mut MsgQueue, t: f64) {
        let mer = st.rx.take_mer();
        let fec = &st.rx.fec;
        let vit = (fec.vit.errors, fec.vit.compared);
        let (de, dc) = (vit.0 - st.last_vit.0, vit.1 - st.last_vit.1);
        st.last_vit = vit;
        let outer = &fec.outer;
        let secs = (st.samples - st.last_samples) as f64 / st.in_rate;
        let pps = (outer.packets - st.last_packets) as f64 / secs.max(1e-9);
        st.last_packets = outer.packets;
        st.last_samples = st.samples;
        if msgs.pending() >= MSG_BACKLOG {
            return;
        }
        let state = if fec.locked() {
            "lock"
        } else if matches!(st.rx.phase, Phase::Locked | Phase::Tps) {
            "sync"
        } else {
            "search"
        };
        msgs.text(format_args!("t={t:.3} state={state}"));
        Self::params(&st.rx, msgs);
        if let Some(m) = mer {
            msgs.text(format_args!(" mer_db={m:.1}"));
        }
        if dc > 0 {
            msgs.text(format_args!(" ber={:.2e}", de as f64 / dc as f64));
        }
        msgs.line(format_args!(
            " rs_corrected={} rs_failed={} packets={} pps={pps:.0} freq_hz={:+.0} \
             clock_ppm={:+.1} tps_corrected={}",
            outer.corrected,
            outer.failed,
            outer.packets,
            st.rx.offset_hz(),
            st.rx.clock * 1e6,
            st.rx.tps_corrected,
        ));
    }
}

impl Block for DvbtDemodulator {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let in_rate = cx.input_rate(0)?;
        // A little below is fine: a clock offset of a few hundred ppm.
        if in_rate < self.rate * (1.0 - 1e-3) {
            return Err(Error::Other(format!(
                "the input rate {} is below the DVB-T elementary rate {} for this \
                 bandwidth; the demodulator needs at least that (any higher rate is \
                 resampled)",
                fmt_rate(in_rate),
                fmt_rate(self.rate)
            )));
        }
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = None;
            o.center_freq = None;
        }
        text_meta(cx, 1);
        self.state = Some(State {
            rx: Box::new(Receiver::new(in_rate, self.rate, self.mode, self.guard)),
            fifo: ByteFifo::new(FIFO_LEN),
            in_rate,
            samples: 0,
            next_report: in_rate.round().max(1.0) as u64,
            last_locked: false,
            last_tps: None,
            last_packets: 0,
            last_samples: 0,
            last_vit: (0, 0),
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

fn build_demodulator(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(DvbtDemodulator {
            mode: parse_auto(p, "mode", Mode::parse)?,
            guard: parse_auto(p, "guard", Guard::parse)?,
            rate: elementary_rate(read_bandwidth(p)?),
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
