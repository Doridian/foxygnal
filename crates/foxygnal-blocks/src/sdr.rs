//! SDR hardware: receive (`sdr_source`) and transmit (`sdr_sink`) through the
//! drivers in `foxygnal-sdr`.
//!
//! Devices open in `start()`, never while building or configuring. Drivers
//! move samples on their own thread into a ring and wake the block, so
//! `work()` only copies what is there and returns `Blocked` otherwise.
//!
//! Tags on `sdr_source`'s output, on the first item after the change:
//! - `rx_freq` (float, Hz) after a live retune
//! - `rx_gain` (float, dB; NaN for automatic gain) after a live gain change
//! - `overflow` (int) the number of samples the device dropped just before
//!   this item because the graph fell behind

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use foxygnal_sdr::{
    Capabilities, Device, Gain, Range, RxConfig, RxStream, SdrError, TxConfig, TxStream,
};
use serde_json::json;

use crate::params;

const CATEGORY: &str = "Hardware";

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "sdr_source",
            label: "SDR Source",
            category: CATEGORY,
            help: "Receive I/Q from an SDR (RTL-SDR, rtl_tcp, SoapySDR devices, or the \
                   built-in simulator). Retunes and gain changes apply live and tag the \
                   stream (`rx_freq`, `rx_gain`); dropped samples are tagged `overflow`.",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::C32),
            }],
            params: vec![
                device_param(
                    "Device string, e.g. `rtlsdr:serial=00000001`, \
                     `rtltcp:host=10.0.0.5,port=1234` or `sim:`. Empty: the first device \
                     found (real hardware before the simulator).",
                ),
                frequency_param(100e6, "Centre frequency to tune to."),
                rate_param(2.4e6),
                params::float(
                    "gain",
                    "Gain",
                    30.0,
                    Some("dB"),
                    "Overall gain; the device picks the nearest step it supports.",
                )
                .live(),
                ParamSpec {
                    name: "agc",
                    label: "Automatic gain",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "Let the device set its gain (hardware AGC). Overrides Gain.",
                    live: true,
                },
                params::float(
                    "ppm",
                    "Frequency correction",
                    0.0,
                    Some("ppm"),
                    "Crystal error to correct for, in parts per million.",
                )
                .live(),
                bandwidth_param(),
                antenna_param(),
            ],
        },
        build_source,
    );
    r.register_tx(
        BlockDescriptor {
            kind: "sdr_sink",
            label: "SDR Sink (TX)",
            category: CATEGORY,
            help: "Transmit I/Q with an SDR. A graph with this block only runs when the \
                   engine allows TX (`--allow-tx`) and you arm TX for the run. \
                   Transmitting needs a licence and must be legal where you are.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::C32),
            }],
            outputs: vec![],
            params: vec![
                device_param(
                    "Device to transmit with, e.g. `soapy:driver=hackrf` or `sim:` (the \
                     simulator, which discards the samples). Must be set: TX never picks \
                     a device on its own.",
                ),
                frequency_param(0.0, "Centre frequency to transmit on. Must be set."),
                params::optional_rate("Samples per second; 0 = the stream's rate."),
                params::float(
                    "gain",
                    "Gain",
                    0.0,
                    Some("dB"),
                    "Transmit gain; its meaning depends on the device.",
                )
                .live(),
                bandwidth_param(),
                antenna_param(),
            ],
        },
        build_sink,
    );
}

fn device_param(help: &'static str) -> ParamSpec {
    ParamSpec {
        name: "device",
        label: "Device",
        kind: ParamKind::Device,
        default: json!(""),
        help,
        live: false,
    }
}

fn frequency_param(default: f64, help: &'static str) -> ParamSpec {
    ParamSpec {
        name: "frequency",
        label: "Frequency",
        kind: ParamKind::Float {
            min: Some(0.0),
            max: None,
            unit: Some("Hz"),
        },
        default: json!(default),
        help,
        live: true,
    }
}

fn rate_param(default: f64) -> ParamSpec {
    ParamSpec {
        default: json!(default),
        help: "Samples per second the device delivers.",
        ..params::sample_rate()
    }
}

fn bandwidth_param() -> ParamSpec {
    ParamSpec {
        name: "bandwidth",
        label: "Bandwidth",
        kind: ParamKind::Float {
            min: Some(0.0),
            max: None,
            unit: Some("Hz"),
        },
        default: json!(0.0),
        help: "Analog filter bandwidth; 0 = automatic.",
        live: false,
    }
}

fn antenna_param() -> ParamSpec {
    ParamSpec {
        name: "antenna",
        label: "Antenna",
        kind: ParamKind::String,
        default: json!(""),
        help: "Antenna port; empty = the device's default.",
        live: false,
    }
}

// ---- Formatting and checks ------------------------------------------------

/// `2e9, "Hz"` → `2 GHz`; up to four significant digits.
pub fn si(v: f64, unit: &str) -> String {
    let (scale, prefix) = match v.abs() {
        a if a >= 1e9 => (1e9, "G"),
        a if a >= 1e6 => (1e6, "M"),
        a if a >= 1e3 => (1e3, "k"),
        _ => (1.0, ""),
    };
    let x = v / scale;
    let digits = if x.abs() >= 100.0 {
        1
    } else if x.abs() >= 10.0 {
        2
    } else {
        3
    };
    let s = format!("{x:.digits$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    format!("{s} {prefix}{unit}")
}

fn fmt_ranges(rs: &[Range], unit: &str) -> String {
    rs.iter()
        .map(|r| {
            if r.min == r.max {
                si(r.min, unit)
            } else {
                format!("{}–{}", si(r.min, unit), si(r.max, unit))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Range checks against what the device reports, worded for people:
/// "frequency 2 GHz is out of range; tunable: 24 MHz–1.766 GHz".
fn check_frequency(caps: &Capabilities, f: f64) -> Result<(), String> {
    if caps.frequency.is_empty() || caps.frequency.iter().any(|r| r.contains(f)) {
        return Ok(());
    }
    Err(format!(
        "frequency {} is out of range; tunable: {}",
        si(f, "Hz"),
        fmt_ranges(&caps.frequency, "Hz")
    ))
}

fn check_rate(caps: &Capabilities, rate: f64) -> Result<(), String> {
    if caps.sample_rate.is_empty() || caps.sample_rate.iter().any(|r| r.contains(rate)) {
        return Ok(());
    }
    Err(format!(
        "sample rate {} is not supported; supported: {}",
        si(rate, "S/s"),
        fmt_ranges(&caps.sample_rate, "S/s")
    ))
}

fn check_gain(caps: &Capabilities, db: f64) -> Result<(), String> {
    match caps.gain {
        Some(r) if !r.contains(db) => Err(format!(
            "gain {db} dB is out of range; supported: {}–{} dB",
            r.min, r.max
        )),
        _ => Ok(()),
    }
}

fn opt_str(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

fn opt_pos(v: f64) -> Option<f64> {
    (v > 0.0).then_some(v)
}

/// An open device and how to name it in messages.
struct Opened {
    device: Box<dyn Device>,
    caps: Capabilities,
    /// "Simulated FM band (sim:)".
    name: String,
}

fn open(device: &str) -> Result<Opened, Error> {
    let d = foxygnal_sdr::drivers().open(device).map_err(|e| {
        let what = if device.trim().is_empty() {
            "no device given and none was found: ".to_owned()
        } else {
            format!("can't open `{device}`: ")
        };
        Error::Other(match e {
            SdrError::NotFound(_) if device.trim().is_empty() => {
                "no SDR device found (connect one, or set Device to `sim:` for the \
                 simulator)"
                    .to_owned()
            }
            e => format!("{what}{e}"),
        })
    })?;
    let info = d.info();
    let name = format!("{} ({})", info.label, info.args);
    Ok(Opened {
        caps: d.capabilities(),
        device: d,
        name,
    })
}

// ---- Source ----------------------------------------------------------------

pub struct SdrSource {
    device: String,
    frequency: f64,
    sample_rate: f64,
    gain: f64,
    agc: bool,
    ppm: f64,
    bandwidth: f64,
    antenna: String,
    // Dropped in this order: the stream before the device.
    stream: Option<Box<dyn RxStream>>,
    opened: Option<Opened>,
    overflows: u64,
    tag_freq: bool,
    tag_gain: bool,
}

impl SdrSource {
    fn name(&self) -> &str {
        self.opened.as_ref().map_or("SDR", |o| o.name.as_str())
    }

    fn err(&self, msg: impl std::fmt::Display) -> Error {
        Error::Other(format!("{}: {msg}", self.name()))
    }

    fn gain_setting(&self) -> Gain {
        if self.agc {
            Gain::Auto
        } else {
            Gain::Db(self.gain)
        }
    }
}

impl Block for SdrSource {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(self.sample_rate);
            o.center_freq = Some(self.frequency);
        }
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let opened = open(&self.device)?;
        self.opened = Some(opened);
        let caps = &self.opened.as_ref().expect("just set").caps;
        let checks = check_rate(caps, self.sample_rate)
            .and_then(|()| check_frequency(caps, self.frequency))
            .and_then(|()| {
                if self.agc {
                    Ok(())
                } else {
                    check_gain(caps, self.gain)
                }
            });
        checks.map_err(|m| self.err(m))?;
        let cfg = RxConfig {
            sample_rate: self.sample_rate,
            frequency: self.frequency,
            gain: self.gain_setting(),
            ppm: self.ppm,
            bandwidth: opt_pos(self.bandwidth),
            antenna: opt_str(&self.antenna),
            channel: 0,
        };
        let opened = self.opened.as_mut().expect("just set");
        let mut rx = opened
            .device
            .start_rx(&cfg)
            .map_err(|e| Error::Other(format!("{}: {e}", opened.name)))?;
        rx.set_waker(std::thread::current());
        self.overflows = rx.overflows();
        self.stream = Some(rx);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let Some(rx) = self.stream.as_mut() else {
            return Err(Error::Other("SDR source used before start".into()));
        };
        let out = io.outputs[0].slice_mut::<Complex32>();
        if out.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        let n = match rx.read(out) {
            Ok(n) => n,
            Err(e) => {
                let name = self.name().to_owned();
                return Err(Error::Other(format!("{name}: stream ended: {e}")));
            }
        };
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let overflows = rx.overflows();
        let port = &mut io.outputs[0];
        if self.tag_freq {
            port.add_tag(0, "rx_freq", self.frequency);
            self.tag_freq = false;
        }
        if self.tag_gain {
            let g = if self.agc { f64::NAN } else { self.gain };
            port.add_tag(0, "rx_gain", g);
            self.tag_gain = false;
        }
        if overflows != self.overflows {
            port.add_tag(0, "overflow", (overflows - self.overflows) as i64);
            self.overflows = overflows;
        }
        port.produce(n);
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.stream = None;
        self.opened = None;
        Ok(())
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "frequency" => {
                let f = value.as_f64().unwrap_or_default();
                if let (Some(o), Some(rx)) = (&self.opened, self.stream.as_mut()) {
                    check_frequency(&o.caps, f)
                        .map_err(|m| Error::Other(format!("{}: {m}", o.name)))?;
                    rx.set_frequency(f)
                        .map_err(|e| Error::Other(format!("{}: retuning: {e}", o.name)))?;
                    self.tag_freq = true;
                }
                self.frequency = f;
            }
            "gain" | "agc" => {
                if name == "gain" {
                    self.gain = value.as_f64().unwrap_or_default();
                } else {
                    self.agc = value.as_bool().unwrap_or_default();
                }
                let g = self.gain_setting();
                if let (Some(o), Some(rx)) = (&self.opened, self.stream.as_mut()) {
                    if let Gain::Db(db) = g {
                        check_gain(&o.caps, db)
                            .map_err(|m| Error::Other(format!("{}: {m}", o.name)))?;
                    }
                    rx.set_gain(g)
                        .map_err(|e| Error::Other(format!("{}: setting gain: {e}", o.name)))?;
                    self.tag_gain = true;
                }
            }
            "ppm" => {
                let ppm = value.as_f64().unwrap_or_default();
                if let (Some(o), Some(rx)) = (&self.opened, self.stream.as_mut()) {
                    rx.set_ppm(ppm).map_err(|e| {
                        Error::Other(format!("{}: frequency correction: {e}", o.name))
                    })?;
                }
                self.ppm = ppm;
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let sample_rate = p.f64("sample_rate")?;
    Ok(Instance {
        block: Box::new(SdrSource {
            device: p.str("device")?.to_owned(),
            frequency: p.f64("frequency")?,
            sample_rate,
            gain: p.f64("gain")?,
            agc: p.bool("agc")?,
            ppm: p.f64("ppm")?,
            bandwidth: p.f64("bandwidth")?,
            antenna: p.str("antenna")?.to_owned(),
            stream: None,
            opened: None,
            overflows: 0,
            tag_freq: false,
            tag_gain: false,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}

// ---- Sink (TX) -------------------------------------------------------------

pub struct SdrSink {
    device: String,
    frequency: f64,
    /// 0 until configured from upstream.
    sample_rate: f64,
    gain: f64,
    bandwidth: f64,
    antenna: String,
    stream: Option<Box<dyn TxStream>>,
    opened: Option<Opened>,
}

/// Why a TX block won't start without the arm switch.
pub const NOT_ARMED: &str = "transmitting is not armed. Only the engine arms TX, for one run \
                             you confirmed: start the server with `--allow-tx` and choose \
                             \"Arm TX and run\", or pass `--arm-tx` to `foxygnal run`";

impl Block for SdrSink {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if self.device.trim().is_empty() {
            return Err(Error::Other(
                "set the device to transmit with (`sim:` for the simulator)".into(),
            ));
        }
        if self.frequency <= 0.0 {
            return Err(Error::Other("set the frequency to transmit on".into()));
        }
        if self.sample_rate <= 0.0 {
            self.sample_rate = cx.input_rate(0)?;
        }
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        // Defence in depth: the server and CLI refuse earlier, but a block
        // that transmits checks for itself too.
        if !foxygnal_sdr::tx_armed() {
            return Err(Error::Other(NOT_ARMED.into()));
        }
        let mut opened = open(&self.device)?;
        let caps = &opened.caps;
        check_rate(caps, self.sample_rate)
            .and_then(|()| check_frequency(caps, self.frequency))
            .map_err(|m| Error::Other(format!("{}: {m}", opened.name)))?;
        let cfg = TxConfig {
            sample_rate: self.sample_rate,
            frequency: self.frequency,
            gain: self.gain,
            bandwidth: opt_pos(self.bandwidth),
            antenna: opt_str(&self.antenna),
            channel: 0,
        };
        let mut tx = opened
            .device
            .start_tx(&cfg)
            .map_err(|e| Error::Other(format!("{}: {e}", opened.name)))?;
        tx.set_waker(std::thread::current());
        self.stream = Some(tx);
        self.opened = Some(opened);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let Some(tx) = self.stream.as_mut() else {
            return Err(Error::Other("SDR sink used before start".into()));
        };
        let input = io.inputs[0].slice::<Complex32>();
        if input.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        let n = match tx.write(input) {
            Ok(n) => n,
            Err(e) => {
                let name = self.opened.as_ref().map_or("SDR", |o| o.name.as_str());
                return Err(Error::Other(format!("{name}: {e}")));
            }
        };
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.stream = None;
        self.opened = None;
        Ok(())
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        let (Some(o), Some(tx)) = (&self.opened, self.stream.as_mut()) else {
            match name {
                "frequency" => self.frequency = v,
                "gain" => self.gain = v,
                _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
            }
            return Ok(());
        };
        match name {
            "frequency" => {
                check_frequency(&o.caps, v)
                    .map_err(|m| Error::Other(format!("{}: {m}", o.name)))?;
                tx.set_frequency(v)
                    .map_err(|e| Error::Other(format!("{}: retuning: {e}", o.name)))?;
                self.frequency = v;
            }
            "gain" => {
                tx.set_gain(v)
                    .map_err(|e| Error::Other(format!("{}: setting gain: {e}", o.name)))?;
                self.gain = v;
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(SdrSink {
            device: p.str("device")?.to_owned(),
            frequency: p.f64("frequency")?,
            sample_rate: p.f64("sample_rate")?,
            gain: p.f64("gain")?,
            bandwidth: p.f64("bandwidth")?,
            antenna: p.str("antenna")?.to_owned(),
            stream: None,
            opened: None,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![],
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::testing::VectorSink;
    use foxygnal_core::{Flowgraph, GraphSpec, TagValue};

    #[test]
    fn si_formatting() {
        assert_eq!(si(2e9, "Hz"), "2 GHz");
        assert_eq!(si(24e6, "Hz"), "24 MHz");
        assert_eq!(si(1.766e9, "Hz"), "1.766 GHz");
        assert_eq!(si(250e3, "S/s"), "250 kS/s");
        assert_eq!(si(3.2e6, "S/s"), "3.2 MS/s");
        assert_eq!(si(100.3e6, "Hz"), "100.3 MHz");
    }

    #[test]
    fn configure_has_no_side_effects_and_sets_meta() {
        let r = crate::registry();
        let spec = GraphSpec::from_toml(
            r#"
[[block]]
id = "rx"
kind = "sdr_source"
params = { device = "nonexistent:", frequency = 94.1e6, sample_rate = 1e6 }
[[block]]
id = "sink"
kind = "null_sink"
[[connect]]
from = "rx.out"
to = "sink.in"
"#,
        )
        .unwrap();
        let mut fg = Flowgraph::from_spec(&spec, &r).unwrap();
        let meta = fg.configure().unwrap();
        assert_eq!(meta[0].meta.sample_rate, Some(1e6));
        assert_eq!(meta[0].meta.center_freq, Some(94.1e6));
        // Starting is what opens the device.
        let err = fg.start().unwrap().wait().unwrap_err().to_string();
        assert!(err.contains("unknown SDR driver `nonexistent`"), "{err}");
    }

    fn source_graph(freq: f64) -> Flowgraph {
        let r = crate::registry();
        let mut fg = Flowgraph::new();
        let mut params = foxygnal_core::Params::new();
        params.insert("device".into(), json!("sim:"));
        params.insert("frequency".into(), json!(freq));
        params.insert("sample_rate".into(), json!(1e6));
        let src = fg.add("rx", r.build("sdr_source", &params).unwrap());
        let (inst, _) = VectorSink::<Complex32>::instance();
        let sink = fg.add("sink", inst);
        fg.connect(src, 0, sink, 0).unwrap();
        fg
    }

    #[test]
    fn range_errors_are_readable() {
        let fg = source_graph(2e9);
        let err = fg.start().unwrap().wait().unwrap_err().to_string();
        assert!(
            err.contains("frequency 2 GHz is out of range; tunable: 24 MHz–1.766 GHz"),
            "{err}"
        );
        assert!(err.contains("Simulated FM band (sim:)"), "{err}");
    }

    #[test]
    fn receives_from_the_simulator_and_tags_retunes() {
        let r = crate::registry();
        let mut params = foxygnal_core::Params::new();
        params.insert("device".into(), json!("sim:"));
        params.insert("sample_rate".into(), json!(1e6));
        let mut fg = Flowgraph::new();
        let src = fg.add("rx", r.build("sdr_source", &params).unwrap());
        let (inst, _data, tags) = VectorSink::<Complex32>::with_tags();
        let sink = fg.add("sink", inst);
        fg.connect(src, 0, sink, 0).unwrap();
        let running = fg.start().unwrap();
        let params = running.param_sender();
        std::thread::sleep(Duration::from_millis(100));
        params.send("rx", "frequency", json!(94.1e6)).unwrap();
        params.send("rx", "gain", json!(20.0)).unwrap();
        let t0 = Instant::now();
        loop {
            let got = tags.lock().unwrap().clone();
            if got.iter().any(|t| &*t.key == "rx_gain") || t0.elapsed() > Duration::from_secs(5) {
                assert!(
                    got.iter()
                        .any(|t| &*t.key == "rx_freq" && t.value == TagValue::Float(94.1e6)),
                    "{got:?}"
                );
                assert!(
                    got.iter()
                        .any(|t| &*t.key == "rx_gain" && t.value == TagValue::Float(20.0)),
                    "{got:?}"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        running.stop();
        running.wait().unwrap();
    }

    #[test]
    fn tx_refuses_to_start_unless_armed() {
        let r = crate::registry();
        assert!(r.is_tx("sdr_sink"));
        assert!(!r.is_tx("sdr_source"));
        let graph = || {
            GraphSpec::from_toml(
                r#"
[[block]]
id = "tone"
kind = "signal_source"
params = { sample_rate = 1e6 }
[[block]]
id = "tx"
kind = "sdr_sink"
params = { device = "sim:", frequency = 433.92e6 }
[[connect]]
from = "tone.out"
to = "tx.in"
"#,
            )
            .unwrap()
        };
        let fg = Flowgraph::from_spec(&graph(), &r).unwrap();
        let err = fg.start().unwrap().wait().unwrap_err().to_string();
        assert!(err.contains("transmitting is not armed"), "{err}");

        // Armed: it runs (the simulator discards the samples).
        let arm = foxygnal_sdr::arm_tx();
        let running = Flowgraph::from_spec(&graph(), &r).unwrap().start().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!running.is_finished());
        running.stop();
        running.wait().unwrap();
        drop(arm);
    }

    #[test]
    fn tx_needs_a_device_and_frequency() {
        let r = crate::registry();
        let spec = GraphSpec::from_toml(
            r#"
[[block]]
id = "tone"
kind = "signal_source"
params = { sample_rate = 1e6 }
[[block]]
id = "tx"
kind = "sdr_sink"
[[connect]]
from = "tone.out"
to = "tx.in"
"#,
        )
        .unwrap();
        let mut fg = Flowgraph::from_spec(&spec, &r).unwrap();
        let err = fg.configure().unwrap_err().to_string();
        assert!(err.contains("set the device"), "{err}");
    }
}
