//! `sat_tracker`: a source of control values that follows a satellite in
//! real time. Outputs, at `rate` values per second:
//!
//! - `offset`: where the satellite's carrier sits relative to the tuner (Hz),
//!   i.e. the `offset` setting plus the Doppler shift at `frequency`; wire it
//!   to `freq_xlating_fir`'s `@offset` and the channel stays centred;
//! - `az`, `el`: azimuth and elevation (degrees);
//! - `track` (text, once a second): position, look angles and Doppler as
//!   `key=value` lines, which the Map instrument draws as a ground track.
//!
//! The clock is the system's (or starts at the `time` setting and runs at
//! real speed). `work` never sleeps: it returns `Wait` until the next value
//! is due.

use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::observer::{Observer, geodetic, teme_to_ecef};
use super::sgp4::Sgp4;
use super::tle::{Tle, find, parse_all};
use super::{Utc, now_unix, parse_utc};
use crate::digital::text::Escaped;
use crate::digital::{MsgQueue, text_meta};

/// Most values written per `work` call.
const MAX_BATCH: usize = 1024;
/// Behind by more than this (the graph stalled): skip ahead to now.
const MAX_LAG_S: f64 = 2.0;

pub fn register(r: &mut Registry) {
    let f = |name, label, min, max, unit, default: f64, help, live| ParamSpec {
        name,
        label,
        kind: ParamKind::Float { min, max, unit },
        default: json!(default),
        help,
        live,
    };
    r.register(
        BlockDescriptor {
            kind: "sat_tracker",
            label: "Satellite Tracker",
            category: "Sources",
            help: "Follows a satellite from its two-line elements (SGP4/SDP4) as seen from your \
                   position, in real time. `offset` is the Doppler-corrected position of its \
                   carrier relative to the tuner: wire it to a Frequency Xlating FIR's offset \
                   input (⇢ input) to keep the channel centred during a pass. `az` and `el` are \
                   the look angles in degrees, `track` a text line per second (position, \
                   elevation, range, Doppler) for the Messages and Map instruments. Element \
                   sets come from a file (CelesTrak's `weather.txt` and friends) or pasted \
                   text; nothing is downloaded. Upcoming passes are listed in the palette's \
                   Passes section.",
            inputs: vec![],
            outputs: vec![
                PortSpec {
                    name: "offset",
                    ty: Some(StreamType::F32),
                },
                PortSpec {
                    name: "az",
                    ty: Some(StreamType::F32),
                },
                PortSpec {
                    name: "el",
                    ty: Some(StreamType::F32),
                },
                PortSpec {
                    name: "track",
                    ty: Some(StreamType::U8),
                },
            ],
            params: vec![
                ParamSpec {
                    name: "satellite",
                    label: "Satellite",
                    kind: ParamKind::String,
                    default: json!(""),
                    help: "Name (`METEOR-M2 3`, case-insensitive, a prefix will do) or NORAD \
                           catalog number. Empty: the first in the element sets.",
                    live: true,
                },
                ParamSpec {
                    name: "tle_file",
                    label: "TLE file",
                    kind: ParamKind::Path,
                    default: json!(""),
                    help: "A file of two- or three-line element sets, read when the graph \
                           starts. Relative to the project folder.",
                    live: false,
                },
                ParamSpec {
                    name: "tle",
                    label: "TLE text",
                    kind: ParamKind::String,
                    default: json!(""),
                    help: "Element sets pasted in (used with the file, if both are set). \
                           Checksums are checked.",
                    live: false,
                },
                f(
                    "lat",
                    "Latitude",
                    Some(-90.0),
                    Some(90.0),
                    Some("°"),
                    0.0,
                    "Your latitude, degrees north (WGS-84).",
                    true,
                ),
                f(
                    "lon",
                    "Longitude",
                    Some(-180.0),
                    Some(180.0),
                    Some("°"),
                    0.0,
                    "Your longitude, degrees east.",
                    true,
                ),
                f(
                    "alt",
                    "Altitude",
                    Some(-500.0),
                    Some(10_000.0),
                    Some("m"),
                    0.0,
                    "Your height above the WGS-84 ellipsoid.",
                    true,
                ),
                f(
                    "frequency",
                    "Downlink frequency",
                    Some(0.0),
                    None,
                    Some("Hz"),
                    137.9e6,
                    "The satellite's transmit frequency; the Doppler shift scales with it.",
                    true,
                ),
                f(
                    "offset",
                    "Offset",
                    None,
                    None,
                    Some("Hz"),
                    0.0,
                    "Where the carrier is relative to the tuner without Doppler (the downlink \
                     minus the SDR's centre frequency). Added to the `offset` output.",
                    true,
                ),
                f(
                    "min_elevation",
                    "Minimum elevation",
                    Some(-10.0),
                    Some(90.0),
                    Some("°"),
                    0.0,
                    "For the Passes list: a pass starts when the satellite rises above this.",
                    true,
                ),
                f(
                    "rate",
                    "Update rate",
                    Some(0.1),
                    Some(1000.0),
                    Some("Hz"),
                    20.0,
                    "Values per second on the f32 outputs (their sample rate). 20 Hz keeps the \
                     Doppler step at a LEO's fastest (≈ 100 Hz/s at 1.7 GHz) under 5 Hz.",
                    false,
                ),
                ParamSpec {
                    name: "time",
                    label: "Start time",
                    kind: ParamKind::String,
                    default: json!(""),
                    help: "Empty: the system clock. A UTC time (`2026-11-23T14:05:00Z`) starts \
                           the clock there instead, for a recording made then.",
                    live: false,
                },
            ],
        },
        build,
    );
}

pub struct SatTracker {
    tle_file: String,
    /// Sets from the `tle` text, then (from `start`) the file.
    sets: Vec<Tle>,
    query: String,
    sat: Option<(Sgp4, Tle)>,
    obs: Observer,
    freq: f64,
    offset: f64,
    rate: f64,
    time: Option<f64>,
    /// When the run started, and the Unix time that corresponds to.
    clock: Option<(Instant, f64)>,
    /// Values written so far (value `n` is for `t0 + n / rate`).
    ticks: u64,
    /// Next whole second (since `t0`) a `track` line is due.
    next_line: u64,
    queue: MsgQueue,
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let text = p.str("tle")?;
    let (sets, errors) = parse_all(text);
    if !text.trim().is_empty() && sets.is_empty() {
        return Err(Error::Other(format!(
            "no usable element set in the TLE text: {}",
            errors.join("; ")
        )));
    }
    let time = p.str("time")?.trim();
    let time = if time.is_empty() {
        None
    } else {
        Some(parse_utc(time).ok_or_else(|| {
            Error::Other(format!(
                "start time `{time}` is not a UTC time like 2026-11-23T14:05:00Z"
            ))
        })?)
    };
    let tle_file = p.str("tle_file")?.trim().to_owned();
    if tle_file.is_empty() && sets.is_empty() {
        return Err(Error::Other(
            "no element sets: choose a TLE file or paste TLEs".into(),
        ));
    }
    let mut t = SatTracker {
        tle_file,
        sets,
        query: p.str("satellite")?.to_owned(),
        sat: None,
        obs: Observer {
            lat: p.f64("lat")?,
            lon: p.f64("lon")?,
            alt_m: p.f64("alt")?,
        },
        freq: p.f64("frequency")?,
        offset: p.f64("offset")?,
        rate: p.f64("rate")?,
        time,
        clock: None,
        ticks: 0,
        next_line: 0,
        queue: MsgQueue::new(),
    };
    if t.tle_file.is_empty() {
        t.select()?;
    }
    Ok(Instance {
        block: Box::new(t),
        inputs: vec![],
        outputs: vec![
            ("offset".into(), StreamType::F32),
            ("az".into(), StreamType::F32),
            ("el".into(), StreamType::F32),
            ("track".into(), StreamType::U8),
        ],
    })
}

impl SatTracker {
    /// Pick the satellite named by `query` and set up its model.
    fn select(&mut self) -> Result<(), Error> {
        let tle = find(&self.sets, &self.query).ok_or_else(|| {
            let names: Vec<String> = self.sets.iter().take(12).map(Tle::label).collect();
            Error::Other(format!(
                "no satellite `{}` in the element sets (there are: {}{})",
                self.query,
                names.join(", "),
                if self.sets.len() > 12 { ", …" } else { "" }
            ))
        })?;
        let sgp4 = Sgp4::new(tle).map_err(|e| Error::Other(format!("{}: {e}", tle.label())))?;
        self.sat = Some((sgp4, tle.clone()));
        Ok(())
    }

    /// Unix time of value `n`.
    fn time_of(&self, t0: f64, n: u64) -> f64 {
        t0 + n as f64 / self.rate
    }
}

impl Block for SatTracker {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut().take(3) {
            o.sample_rate = Some(self.rate);
            o.center_freq = None;
        }
        text_meta(cx, 3);
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        if !self.tle_file.is_empty() {
            let text = std::fs::read_to_string(&self.tle_file)
                .map_err(|e| Error::Other(format!("can't read {}: {e}", self.tle_file)))?;
            let (sets, errors) = parse_all(&text);
            if sets.is_empty() {
                return Err(Error::Other(format!(
                    "no usable element set in {}: {}",
                    self.tle_file,
                    errors.join("; ")
                )));
            }
            self.sets.extend(sets);
            self.select()?;
        }
        self.clock = Some((Instant::now(), self.time.unwrap_or_else(now_unix)));
        self.ticks = 0;
        self.next_line = 0;
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let Some((start, t0)) = self.clock else {
            return Err(Error::Other("not started".into()));
        };
        // Text first: a line waiting for room never holds up the values.
        let mut made_text = 0;
        if !self.queue.is_empty() {
            made_text = self.queue.flush(io.outputs[3].slice_mut::<u8>());
        }
        let elapsed = start.elapsed().as_secs_f64();
        let due = (elapsed * self.rate).floor() as u64 + 1;
        if due > self.ticks + (MAX_LAG_S * self.rate) as u64 + 1 {
            self.ticks = due - 1;
        }
        let room = (0..3)
            .map(|i| io.outputs[i].slice_mut::<f32>().len())
            .min()
            .unwrap_or(0);
        let n = ((due.saturating_sub(self.ticks)) as usize)
            .min(room)
            .min(MAX_BATCH);
        for i in 0..n {
            let t = self.time_of(t0, self.ticks + i as u64);
            let (off, az, el) = match &self.sat {
                Some((sat, tle)) => match sat.propagate_unix(t) {
                    Ok(s) => {
                        let look = self.obs.look(&s, t);
                        let sec = (t - t0).max(0.0) as u64;
                        if sec >= self.next_line && self.queue.is_empty() {
                            self.next_line = sec + 1;
                            let (r, _) = teme_to_ecef(&s, t);
                            let (lat, lon, h) = geodetic(&r);
                            self.queue.line(format_args!(
                                "time={} name=\"{}\" id={} lat={:.4} lon={:.4} alt_km={:.1} \
                                 az={:.2} el={:.2} range_km={:.1} range_rate_kms={:.4} \
                                 doppler_Hz={:.1}",
                                Utc(t),
                                Escaped(tle.name.as_bytes()),
                                tle.norad,
                                lat,
                                lon,
                                h,
                                look.az,
                                look.el,
                                look.range,
                                look.range_rate,
                                look.doppler(self.freq)
                            ));
                        }
                        (
                            (self.offset + look.doppler(self.freq)) as f32,
                            look.az as f32,
                            look.el as f32,
                        )
                    }
                    // NaN is ignored by control inputs: the last good value stays.
                    Err(_) => (f32::NAN, f32::NAN, f32::NAN),
                },
                None => (f32::NAN, f32::NAN, f32::NAN),
            };
            io.outputs[0].slice_mut::<f32>()[i] = off;
            io.outputs[1].slice_mut::<f32>()[i] = az;
            io.outputs[2].slice_mut::<f32>()[i] = el;
        }
        if !self.queue.is_empty() {
            let out = io.outputs[3].slice_mut::<u8>();
            made_text += self.queue.flush(&mut out[made_text..]);
        }
        self.ticks += n as u64;
        for i in 0..3 {
            io.produce(i, n);
        }
        io.produce(3, made_text);
        if n > 0 || made_text > 0 {
            return Ok(WorkStatus::Ok);
        }
        if due > self.ticks {
            // Due but no room: a consumer frees space and wakes us.
            return Ok(WorkStatus::Blocked);
        }
        let next_at = self.ticks as f64 / self.rate;
        Ok(WorkStatus::Wait(Duration::from_secs_f64(
            (next_at - elapsed).max(0.000_1),
        )))
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let num = || {
            value
                .as_f64()
                .ok_or_else(|| Error::Other(format!("`{name}` must be a number")))
        };
        match name {
            "satellite" => {
                let old =
                    std::mem::replace(&mut self.query, value.as_str().unwrap_or("").to_owned());
                if let Err(e) = self.select() {
                    self.query = old;
                    return Err(e);
                }
            }
            "lat" => self.obs.lat = num()?,
            "lon" => self.obs.lon = num()?,
            "alt" => self.obs.alt_m = num()?,
            "frequency" => self.freq = num()?,
            "offset" => self.offset = num()?,
            "min_elevation" => {}
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}
