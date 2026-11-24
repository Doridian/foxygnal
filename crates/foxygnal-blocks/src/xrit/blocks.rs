//! The xRIT blocks: `xrit_decoder` (CADUs → pictures, messages, files) and
//! `xrit_source` (a synthetic GOES HRIT broadcast, as CADUs, for loopback).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, TagPropagation, WorkIo, WorkStatus,
};
use serde_json::json;

use super::decoder::{Config, Decoder};
use super::picture::MAX_OPEN;
use super::synth::Synth;
use super::transport::{FileAssembler, FileEncoder};
use crate::ccsds::vcdu::{FrameBuilder, Layout, PacketAssembler};
use crate::ccsds::wire::{ASM_BYTES, CaduReader};
use crate::digital::{MsgQueue, text_meta};
use crate::dvb::ByteFifo;

fn port(name: &'static str) -> PortSpec {
    PortSpec {
        name,
        ty: Some(StreamType::U8),
    }
}

fn int(
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

fn frame_params() -> Vec<ParamSpec> {
    vec![
        int(
            "frame_len",
            "Frame length",
            1020,
            64,
            16384,
            "Bytes after the sync marker: transfer frame plus Reed-Solomon parity (1020 for \
             the 1024-byte CADUs of GOES, GK-2A and Elektro-L).",
        ),
        int(
            "rs_interleave",
            "RS interleave",
            4,
            0,
            8,
            "Reed-Solomon interleaving depth: the last 32 × I bytes of each frame are parity \
             (4 for every xRIT link).",
        ),
    ]
}

/// The transfer frame length (without parity) from the frame parameters.
fn layout(p: &ParamReader<'_>) -> Result<(usize, Layout), Error> {
    let frame_len = p.u64("frame_len")? as usize;
    let parity = 32 * p.u64("rs_interleave")? as usize;
    let layout = Layout {
        frame_len: frame_len.saturating_sub(parity),
        insert_zone: 0,
        trailer: 0,
    };
    layout.check().map_err(Error::Other)?;
    Ok((frame_len, layout))
}

pub fn register(r: &mut Registry) {
    let mut params = frame_params();
    params.extend([
        ParamSpec {
            name: "dir",
            label: "Folder",
            kind: ParamKind::Path,
            default: json!("xrit"),
            help: "Where files are saved (created if missing; a relative folder is in the \
                   project folder): EMWIN bulletins and charts in `emwin/<date>/`, \
                   administrative messages in `text/`, NWS charts and other files in \
                   `files/<satellite>/<date>/`, DCS in `dcs/`, files that can't be decoded \
                   (encrypted, unsupported compression) in `encrypted/` and `unsupported/`. \
                   Empty: save nothing. Pictures go out on `image` (save them with \
                   `image_sink`).",
            live: false,
        },
        boolean(
            "save_lrit",
            "Keep raw files",
            false,
            "Also save every file as received (Rice-coded pictures decompressed) in \
             `lrit/<satellite>/`.",
        ),
        boolean(
            "false_colour",
            "False colour",
            true,
            "Make a false-colour picture (`…/FC/…`) when a scene's channel 2 (visible) and \
             13 (infrared) are both complete.",
        ),
        int(
            "image_rate",
            "Picture rate (kB/s)",
            1000,
            0,
            1_000_000,
            "Send picture rows at most this fast (0: as fast as they're taken). A full disk \
             is 29 MB at once; pacing it lets the Image instrument, which reads a lossy tap, \
             keep up. `image_sink` gets every row either way.",
        ),
        int(
            "max_text",
            "Text in messages",
            2000,
            0,
            1_000_000,
            "Bulletin text in a message is cut after this many bytes (the saved file has all \
             of it).",
        ),
    ]);
    r.register(
        BlockDescriptor {
            kind: "xrit_decoder",
            label: "xRIT Decoder",
            category: "Satellite",
            help: "LRIT / HRIT files from CADUs (the `frames` output of `ccsds_deframer`): \
                   GOES-R HRIT and LRIT (and the GK-2A / Elektro-L file layer where the \
                   format allows). Puts files back together from packets (CRC-checked, \
                   Rice-coded pictures decompressed line by line), segmented pictures from \
                   their segments, and sends pictures on `image` (named \
                   `G19/FD/CH13/<date>/G19_FD_CH13_<time>`; full disk, mesoscale, the \
                   channels sent and a channel 2 + 13 false colour), EMWIN bulletins as \
                   messages on `out` (`type=emwin product=TORBMX event=\"Tornado Warning\" \
                   office=KBMX wmo=… issued=… text=…`), and saves files in `dir`.",
            inputs: vec![port("in")],
            outputs: vec![port("out"), port("image")],
            params,
        },
        build_decoder,
    );
    let mut params = frame_params();
    params.extend([
        int(
            "width",
            "Full-disk size",
            512,
            32,
            5424,
            "Pixels (square) of the made-up full disk; the mesoscale picture is half as \
             big.",
        ),
        int("segments", "Segments", 4, 1, 64, "Segments per full disk."),
        int(
            "rounds",
            "Rounds",
            0,
            0,
            1_000_000,
            "Rounds to send (each: two full-disk channels, a mesoscale picture, two EMWIN \
             bulletins and an admin message), then stop; 0 = forever.",
        ),
        int(
            "spacecraft",
            "Spacecraft",
            0,
            0,
            255,
            "Spacecraft id in the frame headers.",
        ),
    ]);
    r.register(
        BlockDescriptor {
            kind: "xrit_source",
            label: "HRIT Test Broadcast",
            category: "Satellite",
            help: "A made-up GOES HRIT broadcast for loopback tests, as CADUs with zeroed \
                   Reed-Solomon parity for `ccsds_modulator`: per round, channels 13 and 2 \
                   of a synthetic full disk (segmented, Rice-coded), a mesoscale picture, \
                   an EMWIN tornado warning (zipped) and forecast discussion, and an admin \
                   message, with the headers GOES-19 sends.",
            inputs: vec![],
            outputs: vec![port("out")],
            params,
        },
        build_source,
    );
}

// ---- Decoder ----------------------------------------------------------------------

/// Message lines or pictures waiting past which no more input is taken.
const MAX_LINES: usize = 256;
/// Bytes of rows the picture pacing allows at once.
const BURST: f64 = 65_536.0;

pub struct DecoderBlock {
    reader: CaduReader,
    packets: PacketAssembler,
    files: FileAssembler,
    dec: Decoder,
    text: MsgQueue,
    image: MsgQueue,
    /// Bytes per second for picture rows (None: unpaced).
    rate: Option<f64>,
    started: Option<Instant>,
    image_sent: f64,
    finished: bool,
}

impl DecoderBlock {
    /// Seconds to wait before more rows may go out (0: now).
    fn pacing(&mut self) -> f64 {
        let Some(rate) = self.rate else { return 0.0 };
        let start = *self.started.get_or_insert_with(Instant::now);
        let allowed = start.elapsed().as_secs_f64() * rate + BURST;
        if self.image_sent <= allowed {
            0.0
        } else {
            (self.image_sent - allowed) / rate
        }
    }
}

impl Block for DecoderBlock {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        text_meta(cx, 0);
        text_meta(cx, 1);
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn dedicated_thread(&self) -> bool {
        // Writes files.
        true
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut tmade, mut imade) = (0, 0, 0);
        let mut wait = 0.0;
        loop {
            tmade += self
                .text
                .flush(&mut io.outputs[0].slice_mut::<u8>()[tmade..]);
            imade += self
                .image
                .flush(&mut io.outputs[1].slice_mut::<u8>()[imade..]);
            let mut progress = false;
            if self.text.is_empty()
                && let Some(l) = self.dec.lines.pop_front()
            {
                self.text.line(format_args!("{l}"));
                progress = true;
            }
            if self.image.is_empty() && self.dec.pictures.ready() {
                wait = self.pacing();
                if wait == 0.0 {
                    let n = self.dec.pictures.next_record(&mut self.image);
                    self.image_sent += n as f64;
                    progress |= n > 0;
                }
            }
            if progress {
                continue;
            }
            let backlog = self.dec.lines.len() > MAX_LINES
                || self.dec.pictures.open() > MAX_OPEN + 4
                || !self.text.is_empty();
            let inp = io.inputs[0].slice::<u8>();
            if backlog || used == inp.len() {
                break;
            }
            let (packets, files, dec) = (&mut self.packets, &mut self.files, &mut self.dec);
            used += self.reader.push_one(&inp[used..], |cadu| {
                if !cadu.ok {
                    return;
                }
                packets.push_frame(cadu.data, |p| {
                    files.push(p.spacecraft, p.vcid, p.bytes, |f| dec.file(f));
                });
            });
        }
        if used > 0 || tmade > 0 || imade > 0 {
            io.consume(0, used);
            io.produce(0, tmade);
            io.produce(1, imade);
            return Ok(WorkStatus::Ok);
        }
        let ended = io.inputs[0].exhausted() && io.inputs[0].slice::<u8>().is_empty();
        if ended && !self.finished {
            self.finished = true;
            let dec = &mut self.dec;
            self.files.flush(|f| dec.file(f));
            dec.finish();
            return Ok(WorkStatus::Ok);
        }
        let pending = !self.text.is_empty()
            || !self.image.is_empty()
            || !self.dec.lines.is_empty()
            || self.dec.pictures.ready();
        if wait > 0.0 {
            return Ok(WorkStatus::Wait(Duration::from_secs_f64(wait.min(0.05))));
        }
        if pending && (ended || !self.text.is_empty() || !self.image.is_empty()) {
            let gone = io.outputs[0].abandoned() && io.outputs[1].abandoned();
            if !gone {
                return Ok(WorkStatus::Wait(Duration::from_millis(20)));
            }
        }
        Ok(WorkStatus::Blocked)
    }

    fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

fn build_decoder(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (frame_len, layout) = layout(p)?;
    let dir = p.str("dir")?.trim();
    let rate = p.u64("image_rate")?;
    let cfg = Config {
        dir: (!dir.is_empty()).then(|| PathBuf::from(dir)),
        save_lrit: p.bool("save_lrit")?,
        false_colour: p.bool("false_colour")?,
        max_text: p.u64("max_text")? as usize,
    };
    Ok(Instance {
        block: Box::new(DecoderBlock {
            reader: CaduReader::new(frame_len),
            packets: PacketAssembler::new(layout).map_err(Error::Other)?,
            files: FileAssembler::new(),
            dec: Decoder::new(cfg),
            text: MsgQueue::new(),
            image: MsgQueue::new(),
            rate: (rate > 0).then_some(rate as f64 * 1000.0),
            started: None,
            image_sent: 0.0,
            finished: false,
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![
            ("out".into(), StreamType::U8),
            ("image".into(), StreamType::U8),
        ],
    })
}

// ---- Source -----------------------------------------------------------------------

pub struct SourceBlock {
    synth: Synth,
    layout: Layout,
    frame_len: usize,
    spacecraft: u8,
    rounds: u32,
    round: u32,
    /// Per virtual channel: its frame builder and packet encoders (by APID).
    channels: Vec<(u8, FrameBuilder, Vec<FileEncoder>)>,
    fifo: ByteFifo,
    /// Files of the round being sent.
    pending: std::collections::VecDeque<super::synth::SynthFile>,
}

impl SourceBlock {
    fn send(&mut self, f: &super::synth::SynthFile) -> Result<(), Error> {
        let i = match self.channels.iter().position(|c| c.0 == f.vcid) {
            Some(i) => i,
            None => {
                let b = FrameBuilder::new(self.layout, self.spacecraft, f.vcid)
                    .map_err(Error::Other)?;
                self.channels.push((f.vcid, b, Vec::new()));
                self.channels.len() - 1
            }
        };
        let (fifo, frame_len) = (&mut self.fifo, self.frame_len);
        let mut out = |frame: &[u8]| {
            const ZEROS: [u8; 512] = [0; 512];
            fifo.write(&ASM_BYTES);
            fifo.write(frame);
            let mut pad = frame_len - frame.len();
            while pad > 0 {
                let n = pad.min(ZEROS.len());
                fifo.write(&ZEROS[..n]);
                pad -= n;
            }
        };
        let (_, builder, encoders) = &mut self.channels[i];
        let k = match encoders.iter().position(|e| e.apid == f.apid) {
            Some(k) => k,
            None => {
                encoders.push(FileEncoder::new(f.apid));
                encoders.len() - 1
            }
        };
        encoders[k].encode(&f.headers, &f.data, &mut |p: &[u8]| {
            builder.push_packet(p, &mut out)
        });
        builder.flush(&mut out);
        Ok(())
    }
}

impl Block for SourceBlock {
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
            if !self.fifo.is_empty() {
                break;
            }
            if self.pending.is_empty() {
                if self.rounds > 0 && self.round >= self.rounds {
                    break;
                }
                self.pending = self.synth.round(self.round).into();
                self.round += 1;
            }
            let f = self.pending.pop_front().expect("just filled");
            self.send(&f)?;
        }
        if made > 0 {
            io.produce(0, made);
            return Ok(WorkStatus::Ok);
        }
        if self.fifo.is_empty()
            && self.pending.is_empty()
            && self.rounds > 0
            && self.round >= self.rounds
        {
            return Ok(WorkStatus::Done);
        }
        Ok(WorkStatus::Blocked)
    }
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (frame_len, layout) = layout(p)?;
    let synth = Synth {
        width: p.u64("width")? as u16,
        segments: p.u64("segments")? as u16,
        ..Synth::default()
    };
    // One full-disk segment, Rice-coded, can't be longer than its raw size
    // plus a little; the FIFO holds one file at a time.
    let biggest = usize::from(synth.width) * usize::from(synth.width) * 2 + (1 << 16);
    let frames = biggest / layout.frame_len.saturating_sub(8).max(1) + 4;
    Ok(Instance {
        block: Box::new(SourceBlock {
            synth,
            layout,
            frame_len,
            spacecraft: p.u64("spacecraft")? as u8,
            rounds: p.u64("rounds")? as u32,
            round: 0,
            channels: Vec::new(),
            fifo: ByteFifo::new(frames * (4 + frame_len)),
            pending: Default::default(),
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}
