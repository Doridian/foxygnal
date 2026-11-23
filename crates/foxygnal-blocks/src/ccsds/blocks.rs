//! The CCSDS blocks: `ccsds_deframer` (symbols → frames), `ccsds_packets`
//! (frames → packets), and for the transmit side and loopback tests
//! `ccsds_framer` (bytes → frames) and `ccsds_modulator` (frames → symbols).

use std::time::Duration;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use serde_json::json;

use super::deframer::{Config, Deframer, Frame};
use super::encoder::Encoder;
use super::sync::Modulation;
use super::vcdu::{FrameBuilder, Layout, PacketAssembler, PacketHeader};
use super::wire::{ASM_BAD, ASM_BYTES, CaduReader, record_header};
use crate::design;
use crate::digital::{MsgQueue, idle_status, text_meta};
use crate::dvb::ByteFifo;
use crate::filters::not_configured;

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

fn boolean(
    name: &'static str,
    label: &'static str,
    default: bool,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Bool,
        default: json!(default),
        help,
        live: false,
    }
}

fn int_range(
    name: &'static str,
    label: &'static str,
    default: i64,
    min: i64,
    max: i64,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Int {
            min: Some(min),
            max: Some(max),
        },
        default: json!(default),
        help,
        live: false,
    }
}

fn frame_len_param() -> ParamSpec {
    int_range(
        "frame_len",
        "Frame length",
        1020,
        8,
        16384,
        "Bytes after the sync marker: the transfer frame plus its Reed-Solomon parity \
         (1020 for the usual 1024-byte CADU: 892 + 4 × 32).",
    )
}

fn rs_param() -> ParamSpec {
    int_range(
        "rs_interleave",
        "RS interleave",
        4,
        0,
        5,
        "Reed-Solomon RS(255,223) interleaving depth I (1–5); 0 = no Reed-Solomon. A frame \
         shorter than 255 × I bytes is shortened (virtual fill).",
    )
}

/// The link settings shared by the deframer and the modulator.
fn link_params() -> Vec<ParamSpec> {
    vec![
        design::choice(
            "modulation",
            "Modulation",
            vec!["qpsk", "bpsk"],
            "qpsk",
            "qpsk: two channel bits per symbol (I then Q). bpsk: one, on the real axis.",
        ),
        design::choice(
            "code",
            "Convolutional code",
            vec!["conv", "conv_noninverted", "none"],
            "conv",
            "conv: rate 1/2, K = 7 (171₈, 133₈) with the second output inverted, as CCSDS \
             specifies. conv_noninverted: the same code without the inversion (some \
             missions, GNU Radio's encoder). none: uncoded.",
        ),
        boolean(
            "nrzm",
            "NRZ-M",
            false,
            "Differential (NRZ-M) coding of the data bits, before the convolutional code.",
        ),
        boolean(
            "randomizer",
            "Randomizer",
            true,
            "The CCSDS pseudo-randomizer (x⁸ + x⁷ + x⁵ + x³ + 1) over everything after the \
             sync marker.",
        ),
        rs_param(),
        boolean(
            "dual_basis",
            "Dual basis",
            true,
            "Reed-Solomon symbols in Berlekamp's dual basis (CCSDS). Off: conventional.",
        ),
        frame_len_param(),
    ]
}

fn read_link(p: &ParamReader<'_>) -> Result<Config, Error> {
    let modulation = match p.str("modulation")? {
        "bpsk" => Modulation::Bpsk,
        "qpsk" => Modulation::Qpsk,
        m => return Err(Error::Other(format!("unknown modulation `{m}`"))),
    };
    let (coded, conv_invert) = match p.str("code")? {
        "conv" => (true, true),
        "conv_noninverted" => (true, false),
        "none" => (false, false),
        c => return Err(Error::Other(format!("unknown code `{c}`"))),
    };
    let depth = p.u64("rs_interleave")? as usize;
    let cfg = Config {
        modulation,
        coded,
        conv_invert,
        nrzm: p.bool("nrzm")?,
        randomized: p.bool("randomizer")?,
        rs: (depth > 0).then_some((depth, p.bool("dual_basis")?)),
        frame_len: p.u64("frame_len")? as usize,
    };
    cfg.codec().map_err(Error::Other)?;
    Ok(cfg)
}

/// The transfer frame layout from `frame_len`, `rs_interleave`,
/// `insert_zone` and `trailer`.
fn read_layout(p: &ParamReader<'_>) -> Result<(usize, Layout), Error> {
    let frame_len = p.u64("frame_len")? as usize;
    let depth = p.u64("rs_interleave")? as usize;
    let parity = 32 * depth;
    if frame_len <= parity + 8 {
        return Err(Error::Other(format!(
            "a {frame_len}-byte frame has no room for a transfer frame next to {parity} bytes \
             of Reed-Solomon parity"
        )));
    }
    let layout = Layout {
        frame_len: frame_len - parity,
        insert_zone: p.u64("insert_zone")? as usize,
        trailer: p.u64("trailer")? as usize,
    };
    layout.check().map_err(Error::Other)?;
    Ok((frame_len, layout))
}

fn layout_params() -> Vec<ParamSpec> {
    vec![
        frame_len_param(),
        rs_param(),
        int_range(
            "insert_zone",
            "Insert zone",
            0,
            0,
            64,
            "Bytes between the 6-byte VCDU header and the M_PDU header (frame header error \
             control, insert zone): 0 for GOES and Metop, 2 for Meteor-M LRPT.",
        ),
        int_range(
            "trailer",
            "Trailer",
            0,
            0,
            64,
            "Bytes at the end of the transfer frame that aren't packet data (operational \
             control field, frame error control).",
        ),
    ]
}

pub fn register(r: &mut Registry) {
    let mut deframer_params = link_params();
    deframer_params.push(design::choice(
        "bad_frames",
        "Bad frames",
        vec!["drop", "mark"],
        "drop",
        "Frames Reed-Solomon couldn't correct (or, without RS, whose sync marker was \
         missing): drop them, or send them with the inverted marker E5 30 03 E2.",
    ));
    r.register(
        BlockDescriptor {
            kind: "ccsds_deframer",
            label: "CCSDS Deframer",
            category: "Digital",
            help: "CCSDS frame synchronization and decoding: symbols in (one sample per \
                   symbol, after timing and carrier recovery: Clock Recovery and Costas \
                   Loop), frames out on `frames` as CADUs (the sync marker 1A CF FC 1D, then \
                   the frame derandomized and corrected, Reed-Solomon parity included; the \
                   `.cadu` format). Finds the sync marker through every phase ambiguity of \
                   the carrier loop (BPSK ±, QPSK 4 rotations × I/Q swap), on the encoded \
                   marker for coded links; soft Viterbi decoding (r = 1/2, K = 7), NRZ-M, \
                   derandomizer, RS(255,223) with interleaving and the dual basis. `status` \
                   has a text line per second and per lock change: `t=… state=lock \
                   frames=… bad=… rs_corrected=… rs_failed=… ber=… fps=…`.",
            inputs: vec![port("in", StreamType::C32)],
            outputs: vec![
                port("frames", StreamType::U8),
                port("status", StreamType::U8),
            ],
            params: deframer_params,
        },
        build_deframer,
    );
    r.set_keywords(
        "ccsds_deframer",
        &[
            "ccsds",
            "cadu",
            "vcdu",
            "asm",
            "sync marker",
            "viterbi",
            "reed-solomon",
            "weather satellite",
            "lrpt",
            "hrit",
            "ahrpt",
        ],
    );

    let mut packet_params = layout_params();
    packet_params.push(design::choice(
        "format",
        "Output",
        vec!["text", "records"],
        "text",
        "text: one line per packet (`scid=… vcid=… apid=… seq=… len=… data=…`), for \
         the Messages instrument. records: binary packet records (`FXSP`, spacecraft, \
         VCID, flags, 0, then the space packet), for decoders downstream.",
    ));
    packet_params.push(boolean(
        "idle",
        "Idle packets",
        false,
        "Pass idle packets (APID 2047) on too.",
    ));
    r.register(
        BlockDescriptor {
            kind: "ccsds_packets",
            label: "CCSDS Packets",
            category: "Digital",
            help: "CCSDS space packets out of frames: CADUs in (a CCSDS Deframer's output, \
                   or a `.cadu` file), the AOS transfer frame (VCDU) header and M_PDU read, \
                   packets reassembled across frames per virtual channel, with the VC frame \
                   counter checked for gaps (the first packet after one is flagged). Fill \
                   frames (VCID 63) and idle packets are skipped.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::U8)],
            params: packet_params,
        },
        build_packets,
    );
    r.set_keywords(
        "ccsds_packets",
        &["ccsds", "space packet", "vcdu", "m_pdu", "apid", "cadu"],
    );

    let mut framer_params = vec![
        int_range(
            "spacecraft",
            "Spacecraft id",
            0,
            0,
            255,
            "VCDU spacecraft id.",
        ),
        int_range("vcid", "VCID", 1, 0, 62, "Virtual channel id."),
        int_range(
            "apid",
            "APID",
            100,
            0,
            2046,
            "Application process id of the packets.",
        ),
        int_range(
            "packet_len",
            "Packet data",
            1000,
            1,
            65536,
            "Input bytes per packet (the packet's data field).",
        ),
    ];
    framer_params.extend(layout_params());
    r.register(
        BlockDescriptor {
            kind: "ccsds_framer",
            label: "CCSDS Framer",
            category: "Digital",
            help: "The transmit side of CCSDS Packets: bytes in, cut into space packets \
                   (one APID, counting sequence numbers), multiplexed into AOS transfer \
                   frames (M_PDU with first header pointers) of one virtual channel, out as \
                   CADUs with room for the Reed-Solomon parity, which the CCSDS Modulator \
                   fills in. When the input ends, the last frame is filled with an idle \
                   packet.",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::U8)],
            params: framer_params,
        },
        build_framer,
    );
    r.set_keywords("ccsds_framer", &["ccsds", "space packet", "vcdu", "cadu"]);

    let mut mod_params = link_params();
    mod_params.push(design::float_min(
        "symbol_rate",
        "Symbol rate",
        0.0,
        0.0,
        Some("Bd"),
        "Symbols per second, the output's sample rate; 0 = not set.",
    ));
    r.register(
        BlockDescriptor {
            kind: "ccsds_modulator",
            label: "CCSDS Modulator",
            category: "Digital",
            help: "The transmit side of the CCSDS Deframer, for loopback tests: CADUs in \
                   (sync marker, then the frame; bytes before a marker are skipped), \
                   Reed-Solomon parity computed, randomized, NRZ-M and convolutionally \
                   coded as set, mapped to BPSK or QPSK symbols at one sample per symbol \
                   (shape them with an RRC Filter in interpolate mode).",
            inputs: vec![port("in", StreamType::U8)],
            outputs: vec![port("out", StreamType::C32)],
            params: mod_params,
        },
        build_modulator,
    );
    r.set_keywords(
        "ccsds_modulator",
        &["ccsds", "cadu", "bpsk", "qpsk", "loopback"],
    );
}

// ---- Deframer -------------------------------------------------------------------

/// Input symbols per call into the deframer.
const CHUNK: usize = 2048;
/// Status lines queued at most (they're dropped beyond, so a stuck log never
/// stalls the frames).
const MSG_BACKLOG: usize = 4096;

pub struct DeframerBlock {
    cfg: Config,
    mark_bad: bool,
    st: Option<Box<DeframerState>>,
    msgs: MsgQueue,
}

struct DeframerState {
    deframer: Deframer,
    fifo: ByteFifo,
    margin: usize,
    rate: Option<f64>,
    symbols: u64,
    next_report: u64,
    report_every: u64,
    last_locked: bool,
    last: Snapshot,
    flushed: bool,
}

#[derive(Default, Clone, Copy)]
struct Snapshot {
    symbols: u64,
    frames: u64,
    vit_bits: u64,
    vit_errors: u64,
    asm_bits: u64,
    asm_errors: u64,
}

/// A frame onto the frames wire.
fn write_frame(fifo: &mut ByteFifo, mark_bad: bool, f: &Frame<'_>) {
    let ok = f.ok();
    if ok || mark_bad {
        fifo.write(if ok { &ASM_BYTES } else { &ASM_BAD });
        fifo.write(f.data);
    }
}

impl DeframerBlock {
    pub fn new(cfg: Config, mark_bad: bool) -> Self {
        Self {
            cfg,
            mark_bad,
            st: None,
            msgs: MsgQueue::new(),
        }
    }

    fn process(st: &mut DeframerState, msgs: &mut MsgQueue, mark_bad: bool, input: &[Complex32]) {
        let fifo = &mut st.fifo;
        st.deframer
            .push(input, &mut |f: Frame<'_>| write_frame(fifo, mark_bad, &f));
        st.symbols += input.len() as u64;
        let t = st.rate.map_or(0.0, |r| st.symbols as f64 / r);
        let stats = *st.deframer.stats();
        if stats.locked != st.last_locked && msgs.pending() < MSG_BACKLOG {
            if stats.locked {
                msgs.line(format_args!(
                    "t={t:.3} event=lock modulation={} rotation={} conj={}",
                    st.deframer.config().modulation.name(),
                    stats.symmetry.degrees(),
                    u8::from(stats.symmetry.conj)
                ));
            } else {
                msgs.line(format_args!("t={t:.3} event=unlock"));
            }
        }
        st.last_locked = stats.locked;
        if st.symbols >= st.next_report {
            st.next_report += st.report_every;
            Self::report(st, msgs, t);
        }
    }

    fn report(st: &mut DeframerState, msgs: &mut MsgQueue, t: f64) {
        let s = *st.deframer.stats();
        let last = st.last;
        st.last = Snapshot {
            symbols: st.symbols,
            frames: s.frames,
            vit_bits: s.viterbi_bits,
            vit_errors: s.viterbi_errors,
            asm_bits: s.asm_bits,
            asm_errors: s.asm_errors,
        };
        if msgs.pending() >= MSG_BACKLOG {
            return;
        }
        let state = if s.locked { "lock" } else { "search" };
        msgs.text(format_args!(
            "t={t:.3} state={state} frames={} bad={} rs_corrected={} rs_failed={}",
            s.frames, s.bad_frames, s.rs_corrected, s.rs_failed
        ));
        let (vb, ve) = (
            s.viterbi_bits - last.vit_bits,
            s.viterbi_errors - last.vit_errors,
        );
        if vb > 0 {
            msgs.text(format_args!(" ber={:.2e}", ve as f64 / vb as f64));
        }
        let (ab, ae) = (s.asm_bits - last.asm_bits, s.asm_errors - last.asm_errors);
        if ab > 0 {
            msgs.text(format_args!(" asm_ber={:.2e}", ae as f64 / ab as f64));
        }
        let dt = (st.symbols - last.symbols) as f64;
        if let Some(r) = st.rate.filter(|_| dt > 0.0) {
            msgs.text(format_args!(
                " fps={:.1}",
                (s.frames - last.frames) as f64 * r / dt
            ));
        }
        msgs.line(format_args!(""));
    }
}

impl Block for DeframerBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs.first().and_then(|m| m.sample_rate);
        let deframer = Deframer::new(self.cfg.clone()).map_err(Error::Other)?;
        let cadu = 4 + self.cfg.frame_len;
        if let Some(o) = cx.outputs.get_mut(0) {
            let bits_per_frame = self.cfg.frame_channel_bits() as f64;
            let bps = self.cfg.modulation.bits_per_symbol() as f64;
            o.sample_rate = rate.map(|r| r * bps / bits_per_frame * cadu as f64);
            o.center_freq = None;
        }
        text_meta(cx, 1);
        let report_every = rate.map_or(1 << 20, |r| r.round().max(1.0) as u64);
        self.st = Some(Box::new(DeframerState {
            deframer,
            fifo: ByteFifo::new((16 * cadu).max(1 << 16)),
            margin: 6 * cadu,
            rate,
            symbols: 0,
            next_report: report_every,
            report_every,
            last_locked: false,
            last: Snapshot::default(),
            flushed: false,
        }));
        Ok(())
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
            if used == inp.len() || st.fifo.free() < st.margin {
                break;
            }
            let n = (inp.len() - used).min(CHUNK);
            Self::process(st, &mut self.msgs, self.mark_bad, &inp[used..used + n]);
            used += n;
        }
        if used == 0 && made == 0 && msg_made == 0 {
            if io.inputs[0].exhausted() && !st.flushed {
                // The end: what the Viterbi decoder still holds.
                st.flushed = true;
                let (fifo, mark_bad) = (&mut st.fifo, self.mark_bad);
                st.deframer
                    .flush(&mut |f: Frame<'_>| write_frame(fifo, mark_bad, &f));
                return Ok(WorkStatus::Ok);
            }
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

fn build_deframer(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let cfg = read_link(p)?;
    let mark_bad = p.str("bad_frames")? == "mark";
    Ok(Instance {
        block: Box::new(DeframerBlock::new(cfg, mark_bad)),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![
            ("frames".into(), StreamType::U8),
            ("status".into(), StreamType::U8),
        ],
    })
}

// ---- Packets --------------------------------------------------------------------

pub struct PacketsBlock {
    reader: CaduReader,
    asm: PacketAssembler,
    text: bool,
    queue: MsgQueue,
}

/// Packet data bytes shown in a text line.
const TEXT_BYTES: usize = 16;

impl Block for PacketsBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        text_meta(cx, 0);
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut made = 0;
        let mut used = 0;
        loop {
            made += self
                .queue
                .flush(&mut io.outputs[0].slice_mut::<u8>()[made..]);
            if !self.queue.is_empty() {
                break;
            }
            let inp = io.inputs[0].slice::<u8>();
            if used == inp.len() {
                break;
            }
            // Up to the end of one frame, so the queue holds at most one
            // frame's packets.
            let (queue, asm, text) = (&mut self.queue, &mut self.asm, self.text);
            let take = self.reader.push_one(&inp[used..], |cadu| {
                if !cadu.ok {
                    return;
                }
                asm.push_frame(cadu.data, |p| {
                    if text {
                        queue.text(format_args!(
                            "scid={} vcid={} apid={} seq={} len={}",
                            p.spacecraft, p.vcid, p.header.apid, p.header.seq, p.header.data_len
                        ));
                        if p.after_gap {
                            queue.text(format_args!(" gap=1"));
                        }
                        queue.text(format_args!(" data="));
                        for b in p.bytes.iter().skip(6).take(TEXT_BYTES) {
                            queue.text(format_args!("{b:02x}"));
                        }
                        let more = if p.header.data_len > TEXT_BYTES {
                            "…"
                        } else {
                            ""
                        };
                        queue.line(format_args!("{more}"));
                    } else {
                        queue.bytes(&record_header(&p));
                        queue.bytes(p.bytes);
                    }
                });
            });
            used += take;
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_packets(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (frame_len, layout) = read_layout(p)?;
    let mut asm = PacketAssembler::new(layout).map_err(Error::Other)?;
    asm.keep_idle = p.bool("idle")?;
    Ok(Instance {
        block: Box::new(PacketsBlock {
            reader: CaduReader::new(frame_len),
            asm,
            text: p.str("format")? == "text",
            queue: MsgQueue::new(),
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Framer ---------------------------------------------------------------------

/// A transfer frame as a CADU, with zeros where the RS parity goes.
fn write_cadu(fifo: &mut ByteFifo, frame: &[u8], frame_len: usize) {
    const ZEROS: [u8; 256] = [0; 256];
    fifo.write(&ASM_BYTES);
    fifo.write(frame);
    let mut pad = frame_len - frame.len();
    while pad > 0 {
        let n = pad.min(ZEROS.len());
        fifo.write(&ZEROS[..n]);
        pad -= n;
    }
}

pub struct FramerBlock {
    builder: FrameBuilder,
    frame_len: usize,
    apid: u16,
    seq: u16,
    packet: Vec<u8>,
    packet_len: usize,
    fifo: ByteFifo,
    /// FIFO room one packet (and the flush) may need.
    need: usize,
    flushed: bool,
}

impl FramerBlock {
    fn send_packet(&mut self) {
        let data_len = self.packet.len() - 6;
        if data_len == 0 {
            return;
        }
        PacketHeader {
            version: 0,
            kind: 0,
            secondary_header: false,
            apid: self.apid,
            seq_flags: 3,
            seq: self.seq,
            data_len,
        }
        .write(&mut self.packet);
        self.seq = (self.seq + 1) & 0x3FFF;
        let (fifo, frame_len) = (&mut self.fifo, self.frame_len);
        let mut out = |f: &[u8]| write_cadu(fifo, f, frame_len);
        self.builder.push_packet(&self.packet, &mut out);
        self.packet.truncate(6);
    }

    fn flush(&mut self) {
        self.send_packet();
        let (fifo, frame_len) = (&mut self.fifo, self.frame_len);
        let mut out = |f: &[u8]| write_cadu(fifo, f, frame_len);
        self.builder.flush(&mut out);
    }
}

impl Block for FramerBlock {
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(o) = cx.outputs.get_mut(0) {
            o.center_freq = None;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        let need = self.need;
        loop {
            made += self
                .fifo
                .read_into(&mut io.outputs[0].slice_mut::<u8>()[made..]);
            let inp = io.inputs[0].slice::<u8>();
            if used == inp.len() || self.fifo.free() < need {
                break;
            }
            let room = 6 + self.packet_len - self.packet.len();
            let n = room.min(inp.len() - used);
            self.packet.extend_from_slice(&inp[used..used + n]);
            used += n;
            if self.packet.len() == 6 + self.packet_len {
                self.send_packet();
            }
        }
        if used == 0 && made == 0 {
            if io.inputs[0].exhausted() && io.inputs[0].slice::<u8>().is_empty() {
                if !self.flushed {
                    self.flushed = true;
                    self.flush();
                    return Ok(WorkStatus::Ok);
                }
                if self.fifo.is_empty() || io.outputs[0].abandoned() {
                    return Ok(WorkStatus::Done);
                }
                return Ok(WorkStatus::Wait(Duration::from_millis(20)));
            }
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_framer(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (frame_len, layout) = read_layout(p)?;
    let builder = FrameBuilder::new(layout, p.u64("spacecraft")? as u8, p.u64("vcid")? as u8)
        .map_err(Error::Other)?;
    let packet_len = (p.u64("packet_len")? as usize).clamp(1, 65536);
    let mut packet = Vec::with_capacity(6 + packet_len);
    packet.resize(6, 0);
    let (zs, ze) = layout.zone();
    let need = ((6 + packet_len) / (ze - zs) + 3) * (4 + frame_len);
    let fifo_len = (2 * need).max(1 << 16);
    Ok(Instance {
        block: Box::new(FramerBlock {
            builder,
            frame_len,
            apid: p.u64("apid")? as u16,
            seq: 0,
            packet,
            packet_len,
            fifo: ByteFifo::new(fifo_len),
            need,
            flushed: false,
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Modulator ------------------------------------------------------------------

pub struct ModulatorBlock {
    enc: Encoder,
    reader: CaduReader,
    symbol_rate: Option<f64>,
    syms: Vec<Complex32>,
    pos: usize,
}

impl Block for ModulatorBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(o) = cx.outputs.get_mut(0) {
            o.sample_rate = self.symbol_rate;
        }
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        loop {
            if self.pos < self.syms.len() {
                let out = &mut io.outputs[0].slice_mut::<Complex32>()[made..];
                let n = out.len().min(self.syms.len() - self.pos);
                out[..n].copy_from_slice(&self.syms[self.pos..self.pos + n]);
                self.pos += n;
                made += n;
                if self.pos < self.syms.len() {
                    break;
                }
                continue;
            }
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.is_empty() {
                break;
            }
            self.syms.clear();
            self.pos = 0;
            let (enc, syms) = (&mut self.enc, &mut self.syms);
            let take = self
                .reader
                .push_one(inp, |cadu| enc.encode(cadu.data, |s| syms.push(s)));
            used += take;
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
    let cfg = read_link(p)?;
    let frame_len = cfg.frame_len;
    let enc = Encoder::new(cfg).map_err(Error::Other)?;
    let syms = Vec::with_capacity(enc.symbols_per_frame() + 1);
    let rate = p.f64("symbol_rate")?;
    Ok(Instance {
        block: Box::new(ModulatorBlock {
            enc,
            reader: CaduReader::new(frame_len),
            symbol_rate: (rate > 0.0).then_some(rate),
            syms,
            pos: 0,
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}
