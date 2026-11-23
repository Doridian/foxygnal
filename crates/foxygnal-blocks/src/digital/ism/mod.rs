//! ISM-band sensor decoder (433 / 868 / 915 MHz and friends), in the spirit
//! of rtl_433: complex baseband in, one text line per decoded message out.
//!
//! The pipeline, all inside one block so a template needs nothing else:
//!
//! 1. [`pulse::Detector`]: an OOK envelope detector with an adaptive noise
//!    floor (packets of pulse and gap widths, ended by a long gap), and an FSK
//!    detector that runs while a carrier is on (bursts of mark and space
//!    widths from an FM discriminator).
//! 2. For each packet, every device of the matching kind (OOK or FSK) in the
//!    table ([`devices`]) slices the widths into rows of bits with its own
//!    line code and timing ([`slicer`]: PCM/NRZ, PPM, PWM, Manchester,
//!    differential Manchester), then looks for its preamble, checks its
//!    CRC or checksum, and writes the fields.
//! 3. With `analyze` on, packets no device claimed are summarised: pulse and
//!    gap width histograms and a guess at the line code.
//!
//! Lines look like rtl_433's key=value output:
//! `t=12.345 model=Acurite-Tower id=1234 channel=A battery_ok=1
//! temperature_C=21.3 humidity=45 mic=CHECKSUM rssi_dB=-31.0 snr_dB=18.2
//! offset_kHz=-35.4`.

pub mod bits;
pub mod devices;
pub mod out;
pub mod pulse;
pub mod slicer;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::{MsgQueue, idle_status, text_meta};
use crate::design::fmt_rate;
use crate::filters::not_configured;
use bits::BitBuffer;
use devices::Device;
use out::Out;
use pulse::{Detector, Event, Pulses};

/// The gap that ends an OOK packet: longer than the ~31-bit sync gap of
/// EV1527/PT2262 remotes (up to ~500 µs bits), so their repeats stay together.
const MAX_GAP_US: f64 = 17_000.0;
/// OOK devices with symbols at least this long (µs) are also tried on the
/// trains of a second detector with a slower envelope, which keeps weak
/// pulses whole.
const SLOW_SHORT_US: f32 = 150.0;
/// Fewest pulses worth slicing.
const MIN_PULSES: usize = 8;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "ism_decoder",
            label: "ISM Sensor Decoder",
            category: "Digital",
            help: "Decodes wireless sensors in the ISM bands (315/345/433/868/915 MHz) from \
                   complex baseband, like rtl_433: weather stations (Acurite, Ambient Weather, \
                   LaCrosse, Fine Offset, Oregon Scientific, Nexus/Prologue/Rubicson), tire \
                   pressure sensors (Schrader, Toyota, Ford, Citroen), door/window sensors \
                   (Honeywell/2Gig, Interlogix), EV1527/PT2262 remotes and doorbells, and ERT \
                   utility meters (SCM, SCM+, IDM). OOK and FSK, 250 kS/s to 3.2 MS/s (1 MS/s \
                   is a good choice; ERT meters want 1 MS/s or more). One text line per \
                   message: `t=12.345 model=Acurite-Tower id=1234 channel=A battery_ok=1 \
                   temperature_C=21.3 humidity=45 mic=CHECKSUM rssi_dB=… snr_dB=… \
                   offset_kHz=…`.",
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
                    name: "devices",
                    label: "Devices",
                    kind: ParamKind::String,
                    default: json!("all"),
                    help: device_help(),
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
                    default: json!(8.0),
                    help: "How far above the noise floor a pulse must rise to count. Lower \
                           finds weaker sensors and more noise.",
                    live: false,
                },
                ParamSpec {
                    name: "analyze",
                    label: "Analyze unknown signals",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "Also write a line for each packet no device decoded: pulse and gap \
                           width histograms and a guess at the line code (`model=unknown \
                           guess=PWM short_us=… long_us=…`).",
                    live: true,
                },
            ],
        },
        build,
    );
}

fn device_help() -> &'static str {
    "`all`, or a comma-separated list of device keys to try: acurite_txr (592TXR, 5n1, Atlas, \
     3n1, 899), acurite_606, acurite_609, acurite_986, f007th, wh31e, tx141, tx29, tx35, lacrosse_tx (TX-3/4/6U/7U), wh24 (WH24/\
     WH65), wh1080, oregon (v2.1/v3), nexus, prologue, rubicson, schrader, toyota, ford, citroen, \
     honeywell (and 2Gig), interlogix, ev1527, pt2262, scm, scmplus, idm. A leading `-` \
     removes a device from all (`-ev1527,-pt2262`)."
}

/// Pick devices from a `devices` parameter.
pub fn select(spec: &str) -> Result<Vec<&'static Device>, Error> {
    let all = devices::all();
    let spec = spec.trim();
    if spec.is_empty() || spec == "all" {
        return Ok(all.iter().collect());
    }
    let names: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let exclude = names.iter().all(|n| n.starts_with('-'));
    for n in &names {
        let key = n.trim_start_matches('-');
        if !all.iter().any(|d| d.key == key) {
            return Err(Error::Other(format!(
                "unknown device `{key}` (known: {})",
                all.iter().map(|d| d.key).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(all
        .iter()
        .filter(|d| {
            let named = names.iter().any(|n| n.trim_start_matches('-') == d.key);
            if exclude { !named } else { named }
        })
        .collect())
}

pub struct IsmDecoder {
    devices: Vec<&'static Device>,
    level: f32,
    analyze: bool,
    det: Option<Detector>,
    /// The slow-envelope detector, for the `slow` devices.
    det_slow: Option<Detector>,
    slow: Vec<&'static Device>,
    bits: BitBuffer,
    scratch: BitBuffer,
    out: Out,
    queue: MsgQueue,
}

impl IsmDecoder {
    pub fn new(devices: Vec<&'static Device>, level: f32, analyze: bool) -> Self {
        let slow = devices
            .iter()
            .copied()
            .filter(|d| !d.modulation.is_fsk() && d.timing.short >= SLOW_SHORT_US)
            .collect();
        Self {
            devices,
            det_slow: None,
            slow,
            level,
            analyze,
            det: None,
            bits: BitBuffer::new(),
            scratch: BitBuffer::new(),
            out: Out::new(),
            queue: MsgQueue::new(),
        }
    }

    /// Decode one finished train into the queue.
    fn process(&mut self, slow: bool, fsk: bool) {
        let det = if slow { &self.det_slow } else { &self.det };
        let Some(det) = det else { return };
        let p = if fsk { &det.fsk } else { &det.ook };
        decode_train(
            p,
            if slow { &self.slow } else { &self.devices },
            self.analyze && !slow,
            &mut self.bits,
            &mut self.scratch,
            &mut self.out,
        );
        self.queue.bytes(self.out.text());
    }
}

/// Run every device of the right kind over `p`, writing lines to `out`
/// (reset first). Also the entry point for tools and tests.
pub fn decode_train(
    p: &Pulses,
    devices: &[&Device],
    analyze: bool,
    bits: &mut BitBuffer,
    scratch: &mut BitBuffer,
    out: &mut Out,
) {
    out.reset();
    if p.n < MIN_PULSES {
        return;
    }
    out.t = p.start as f64 / p.rate;
    out.rssi = 20.0 * p.high.max(1e-9).log10();
    out.snr = 20.0 * (p.high / p.noise.max(1e-9)).max(1e-9).log10();
    out.offset = p.offset;
    for d in devices {
        if d.modulation.is_fsk() != p.fsk {
            continue;
        }
        slicer::slice(p, d.modulation, &d.timing, bits);
        (d.decode)(bits, scratch, out);
    }
    if analyze && out.events == 0 {
        analysis(p, out);
    }
}

// ---- Analysis ----------------------------------------------------------------------

/// Width clusters: (mean µs, count).
struct Hist {
    bins: [(f64, u32); 8],
    n: usize,
}

impl Hist {
    fn new() -> Self {
        Self {
            bins: [(0.0, 0); 8],
            n: 0,
        }
    }

    fn add(&mut self, w: f64) {
        for b in &mut self.bins[..self.n] {
            if (w - b.0).abs() <= 0.2 * b.0 + 8.0 {
                b.1 += 1;
                b.0 += (w - b.0) / f64::from(b.1);
                return;
            }
        }
        if self.n < self.bins.len() {
            self.bins[self.n] = (w, 1);
            self.n += 1;
        }
    }

    fn finish(&mut self) {
        self.bins[..self.n].sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    /// Up to four widths of the clusters seen at least twice, shortest first.
    fn common(&self) -> ([f64; 4], usize) {
        let mut w = [0.0; 4];
        let mut n = 0;
        for b in &self.bins[..self.n] {
            if b.1 >= 2 && n < 4 {
                w[n] = b.0;
                n += 1;
            }
        }
        (w, n)
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 0.25 * a.max(b)
}

fn analysis(p: &Pulses, out: &mut Out) {
    let us = p.us();
    let (mut ph, mut gh) = (Hist::new(), Hist::new());
    for i in 0..p.n {
        if p.pulse[i] > 0 {
            ph.add(f64::from(p.pulse[i]) * us);
        }
        if i + 1 < p.n {
            gh.add(f64::from(p.gap[i]) * us);
        }
    }
    ph.finish();
    gh.finish();
    let (pw, pn) = ph.common();
    let (gw, gn) = gh.common();
    let (guess, short, long) = match (&pw[..pn], &gw[..gn]) {
        ([s, l, ..], [g0, g1, ..]) if near(*s, *g0) && near(*l, 2.0 * s) && near(*g1, 2.0 * g0) => {
            ("MANCHESTER", *s, 0.0)
        }
        ([_], [s, l, ..]) => ("PPM", *s, *l),
        ([s, l, ..], _) => ("PWM", *s, *l),
        ([s], _) => ("PCM", *s, *s),
        _ => ("unknown", 0.0, 0.0),
    };
    let mut line = out
        .begin("unknown")
        .kv("mod", if p.fsk { "FSK" } else { "OOK" })
        .kv("pulses", p.n)
        .kv("guess", guess)
        .f("short_us", short, 0)
        .f("long_us", long, 0)
        .hist("pulse_us", &ph.bins[..ph.n])
        .hist("gap_us", &gh.bins[..gh.n]);
    if p.fsk {
        line = line
            .f("f1_kHz", f64::from(p.f1) * 1e-3, 1)
            .f("f2_kHz", f64::from(p.f2) * 1e-3, 1);
    }
    drop(line);
}

// ---- Block ----------------------------------------------------------------------

impl Block for IsmDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.input_rate(0)?;
        if !(200e3..=3.3e6).contains(&rate) {
            return Err(Error::Other(format!(
                "works at 250 kS/s to 3.2 MS/s, not {}",
                fmt_rate(rate)
            )));
        }
        self.det = Some(Detector::new(rate, self.level, MAX_GAP_US));
        self.det_slow =
            (!self.slow.is_empty()).then(|| Detector::slow(rate, self.level, MAX_GAP_US));
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.det.is_none() {
            return Err(not_configured());
        }
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.queue.flush(&mut out[made..]);
            if !self.queue.is_empty() {
                break;
            }
            let inp = io.inputs[0].slice::<Complex32>();
            if used == inp.len() {
                break;
            }
            let det = self.det.as_mut().expect("configured");
            let (mut ev, mut ev_slow) = (Event::None, Event::None);
            while used < inp.len() {
                let x = inp[used];
                used += 1;
                ev = det.sample(x);
                if let Some(d) = self.det_slow.as_mut() {
                    ev_slow = d.sample(x);
                }
                if ev != Event::None || ev_slow != Event::None {
                    break;
                }
            }
            if ev != Event::None {
                self.process(false, ev == Event::Fsk);
            }
            if ev_slow != Event::None {
                self.process(true, ev_slow == Event::Fsk);
            }
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "analyze" => self.analyze = value.as_bool().unwrap_or(false),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(IsmDecoder::new(
            select(p.str("devices")?)?,
            p.f64("level")? as f32,
            p.bool("analyze")?,
        )),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_names_every_device_and_keys_are_unique() {
        let help = device_help();
        let all = devices::all();
        for (i, d) in all.iter().enumerate() {
            assert!(help.contains(d.key), "{} missing from the help", d.key);
            assert!(all[..i].iter().all(|e| e.key != d.key), "{} twice", d.key);
        }
    }
}
