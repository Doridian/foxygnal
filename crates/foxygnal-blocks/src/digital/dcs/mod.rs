//! GOES DCS: the Data Collection System relayed by the GOES satellites.
//! Thousands of data collection platforms (river gauges, rain gauges,
//! weather stations, buoys) transmit short bursts at 401.7–402.1 MHz; the
//! satellite relays the whole band unchanged to 1679.7–1680.1 MHz (GOES-R).
//!
//! `goes_dcs_decoder` takes complex baseband covering any part of that band
//! (or of the 402 MHz uplink), finds transmissions as they start, decodes
//! many at once and writes one line per message. `goes_dcs_sim` simulates a
//! band full of platforms, for the template and the tests.
//!
//! The signal format is that of the DCPRS Certification Standard 2.0 (CS2,
//! 2009): see [`frame`]. The older CS1 formats (100 bit/s Manchester PSK,
//! the interleaved CS1 HDR) are not decoded.

pub mod demod;
pub mod frame;
pub mod modulator;
pub mod receiver;
pub mod viterbi;

use std::fmt::Write as _;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::text::Escaped;
use super::{MsgQueue, idle_status, text_meta};
use crate::design::fmt_rate;
use crate::filters::not_configured;
use crate::params;
use frame::{Baud, Format};
use modulator::{Mix, Sim, SimConfig};
use receiver::{Config, Message, Receiver};

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "goes_dcs_decoder",
            label: "GOES DCS Decoder",
            category: "Digital",
            help: "Decodes the GOES Data Collection System: platforms (river and rain gauges, \
                   weather stations, buoys) bursting at 300 or 1200 bit/s (8PSK, trellis \
                   coded, CS2 format) on the 1679.7–1680.1 MHz downlink, or on the 401.7–402.1 \
                   MHz uplink directly. Complex baseband in, any rate from 20 kS/s to 4 MS/s \
                   (250 kS/s covers 330 channels; 500 kS/s the whole band). Transmissions are \
                   found by their carrier and decoded many at once. One text line per message: \
                   `t=… addr=CE1200B8 ch=123 baud=300 freq_MHz=… offset_Hz=… snr_dB=… \
                   quality=N format=ascii text=\"…\"`.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::C32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![
                ParamSpec {
                    name: "center",
                    label: "Centre frequency",
                    kind: ParamKind::Float {
                        min: Some(0.0),
                        max: None,
                        unit: Some("Hz"),
                    },
                    default: json!(0.0),
                    help: "RF frequency of the input's 0 Hz, for channel numbers. 0 takes it \
                           from the stream (an SDR source or a SigMF file says it). Downlink \
                           (1680 MHz) and uplink (402 MHz) frequencies both work.",
                    live: false,
                },
                ParamSpec {
                    name: "level",
                    label: "Detection level",
                    kind: ParamKind::Float {
                        min: Some(3.0),
                        max: Some(40.0),
                        unit: Some("dB"),
                    },
                    default: json!(10.0),
                    help: "How far a carrier must rise above the noise floor (in a bin of \
                           50–100 Hz) to start a demodulator.",
                    live: false,
                },
                ParamSpec {
                    name: "max_platforms",
                    label: "Simultaneous platforms",
                    kind: ParamKind::Int {
                        min: Some(1),
                        max: Some(512),
                    },
                    default: json!(48),
                    help: "Most transmissions decoded at the same time.",
                    live: false,
                },
                ParamSpec {
                    name: "rates",
                    label: "Data rates",
                    kind: ParamKind::Enum {
                        options: vec!["both", "300", "1200"],
                    },
                    default: json!("both"),
                    help: "Which platform data rates to look for.",
                    live: false,
                },
            ],
        },
        build_decoder,
    );
    r.register(
        BlockDescriptor {
            kind: "goes_dcs_sim",
            label: "GOES DCS Simulator",
            category: "Sources",
            help: "A simulated GOES DCS band: platforms on their CS2 channels, each sending an \
                   ASCII or pseudo-binary message every `interval` seconds or so (300 bit/s, \
                   and every third one at 1200 bit/s), with oscillator errors and levels up to \
                   10 dB apart. No noise: add some. For trying `goes_dcs_decoder` without a \
                   dish.",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::C32),
            }],
            params: vec![
                ParamSpec {
                    default: json!(250_000.0),
                    ..params::sample_rate()
                },
                ParamSpec {
                    name: "center",
                    label: "Centre frequency",
                    kind: ParamKind::Float {
                        min: Some(0.0),
                        max: None,
                        unit: Some("Hz"),
                    },
                    default: json!(1_679_900_000.0),
                    help: "RF frequency of the output's 0 Hz (sets which channels fit, and the \
                           stream's centre frequency).",
                    live: false,
                },
                ParamSpec {
                    name: "platforms",
                    label: "Platforms",
                    kind: ParamKind::Int {
                        min: Some(1),
                        max: Some(200),
                    },
                    default: json!(12),
                    help: "How many platforms.",
                    live: false,
                },
                ParamSpec {
                    name: "interval",
                    label: "Interval",
                    kind: ParamKind::Float {
                        min: Some(0.0),
                        max: None,
                        unit: Some("s"),
                    },
                    default: json!(20.0),
                    help: "Mean time between the end of one transmission of a platform and \
                           the start of its next (real platforms: minutes to hours).",
                    live: false,
                },
                ParamSpec {
                    name: "rates",
                    label: "Data rates",
                    kind: ParamKind::Enum {
                        options: vec!["both", "300", "1200"],
                    },
                    default: json!("both"),
                    help: "Data rates the platforms use.",
                    live: false,
                },
                params::float(
                    "amplitude",
                    "Amplitude",
                    0.05,
                    None,
                    "Carrier amplitude of the strongest platform.",
                ),
                params::float(
                    "freq_error",
                    "Frequency error",
                    100.0,
                    Some("Hz"),
                    "Largest oscillator error of a platform (CS2 allows ±125 Hz).",
                ),
                ParamSpec {
                    name: "seed",
                    label: "Seed",
                    kind: ParamKind::Int {
                        min: Some(0),
                        max: None,
                    },
                    default: json!(1),
                    help: "Picks the platforms, their channels and timing.",
                    live: false,
                },
            ],
        },
        build_sim,
    );
}

// ---- Decoder -------------------------------------------------------------------

pub struct DcsDecoder {
    center: f64,
    level: f32,
    max_platforms: usize,
    b300: bool,
    b1200: bool,
    rx: Option<Receiver>,
    queue: MsgQueue,
    line: String,
    ended: bool,
}

fn rates(s: &str) -> Result<(bool, bool), Error> {
    match s {
        "both" => Ok((true, true)),
        "300" => Ok((true, false)),
        "1200" => Ok((false, true)),
        _ => Err(Error::Other(format!("unknown rates `{s}`"))),
    }
}

/// One message as a text line (no newline).
pub fn format_line(m: &Message<'_>, out: &mut String) {
    out.clear();
    let _ = write!(out, "t={:.3}", m.t);
    match m.addr {
        Some((a, 0)) => {
            let _ = write!(out, " addr={a:08X}");
        }
        Some((a, fixed)) => {
            let _ = write!(
                out,
                " addr={a:08X} addr_rx={:08X} addr_fixed={fixed}",
                m.addr_rx
            );
        }
        None => {
            let _ = write!(out, " addr={:08X} addr_bad=1", m.addr_rx);
        }
    }
    if let Some((ch, off)) = m.channel {
        let _ = write!(out, " ch={ch}");
        let _ = write!(out, " baud={}", m.baud.bits());
        let _ = write!(out, " freq_MHz={:.6}", m.rf.unwrap_or_default() * 1e-6);
        let _ = write!(out, " offset_Hz={off:+.1}");
    } else {
        let _ = write!(out, " baud={}", m.baud.bits());
        match m.rf {
            Some(f) => {
                let _ = write!(out, " freq_MHz={:.6}", f * 1e-6);
            }
            None => {
                let _ = write!(out, " freq_Hz={:+.1}", m.freq);
            }
        }
    }
    let s = &m.stats;
    let quality = if s.good_phase_pct >= 85.0 {
        'N'
    } else if s.good_phase_pct >= 70.0 {
        'F'
    } else {
        'P'
    };
    let _ = write!(
        out,
        " level_dB={:.1} snr_dB={:.1} cn0_dBHz={:.1} phase_noise_deg={:.1} good_phase_pct={:.1} quality={quality}",
        20.0 * s.level.max(1e-9).log10(),
        s.snr_db,
        s.snr_db + 10.0 * (m.baud.symbol_rate() as f32).log10(),
        s.phase_noise_deg,
        s.good_phase_pct,
    );
    let _ = write!(
        out,
        " flag=0x{:02X} format={} len={} end={}",
        m.flag,
        m.format.name(),
        m.data.len(),
        m.end.name()
    );
    match m.format {
        Format::Binary => {
            out.push_str(" hex=");
            for b in m.data {
                let _ = write!(out, "{b:02X}");
            }
        }
        _ => {
            let pe = m.parity_errors();
            if pe > 0 {
                let _ = write!(out, " parity_errors={pe}");
            }
            // Text without the parity bits.
            out.push_str(" text=\"");
            let mut chunk = [0u8; 64];
            for part in m.data.chunks(chunk.len()) {
                for (c, &b) in chunk.iter_mut().zip(part) {
                    *c = b & 0x7F;
                }
                let _ = write!(out, "{}", Escaped(&chunk[..part.len()]));
            }
            out.push('"');
        }
    }
}

impl Block for DcsDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let fs = cx.input_rate(0)?;
        if !(20e3..=4.1e6).contains(&fs) {
            return Err(Error::Other(format!(
                "works at 20 kS/s to 4 MS/s, not {}",
                fmt_rate(fs)
            )));
        }
        let center = if self.center > 0.0 {
            Some(self.center)
        } else {
            cx.inputs.first().and_then(|m| m.center_freq)
        };
        self.rx = Some(Receiver::new(Config {
            fs,
            center,
            level_db: self.level,
            max_platforms: self.max_platforms,
            b300: self.b300,
            b1200: self.b1200,
        }));
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let Some(rx) = self.rx.as_mut() else {
            return Err(not_configured());
        };
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.queue.flush(&mut out[made..]);
            if !self.queue.is_empty() {
                break;
            }
            let line = &mut self.line;
            if rx.take(|m| format_line(m, line)) {
                self.queue.line(format_args!("{}", self.line));
                continue;
            }
            let inp = io.inputs[0].slice::<Complex32>();
            if used == inp.len() {
                if io.inputs[0].exhausted() && !self.ended {
                    self.ended = true;
                    rx.finish();
                    continue;
                }
                break;
            }
            used += rx.push(&inp[used..]);
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build_decoder(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let (b300, b1200) = rates(p.str("rates")?)?;
    Ok(Instance {
        block: Box::new(DcsDecoder {
            center: p.f64("center")?,
            level: p.f64("level")? as f32,
            max_platforms: p.u64("max_platforms")? as usize,
            b300,
            b1200,
            rx: None,
            queue: MsgQueue::new(),
            line: String::with_capacity(4096),
            ended: false,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

// ---- Simulator -----------------------------------------------------------------

pub struct DcsSim {
    sim: Sim,
    fs: f64,
    center: f64,
}

impl Block for DcsSim {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        cx.set_rate(self.fs);
        for o in cx.outputs.iter_mut() {
            o.center_freq = Some(self.center);
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let out = io.outputs[0].slice_mut::<Complex32>();
        let n = out.len().min(16_384);
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        self.sim.fill(&mut out[..n]);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_sim(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let fs = p.f64("sample_rate")?;
    let center = p.f64("center")?;
    let mix = match rates(p.str("rates")?)? {
        (true, true) => Mix::Both,
        (true, false) => Mix::Only(Baud::B300),
        _ => Mix::Only(Baud::B1200),
    };
    Ok(Instance {
        block: Box::new(DcsSim {
            sim: Sim::new(SimConfig {
                fs,
                center,
                platforms: p.u64("platforms")? as usize,
                interval: p.f64("interval")?,
                mix,
                amplitude: p.f64("amplitude")? as f32,
                freq_error: p.f64("freq_error")?,
                seed: p.u64("seed")?,
            }),
            fs,
            center,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}
