//! Sound card I/O.
//!
//! The device blocks (`audio_sink`, `audio_source`) need the `audio` cargo
//! feature, which pulls in `cpal` (and so the ALSA headers on Linux). The
//! pacing logic below is always compiled so it can be tested without a device.
//!
//! Between `work()` and the device callback sits a lock-free SPSC
//! [`ring`](crate::ring). Neither side locks or allocates. The device callback
//! unparks the block's thread after every period, so the sound card clocks the
//! graph: a full ring makes the sink park, and the device draining it wakes the
//! sink again. No throttle is needed.
//!
//! With `type = "c32"` the blocks are stereo, like `browser_audio`: one
//! complex item is one frame, real part left, imaginary part right. The ring
//! then carries interleaved L/R pairs, always whole frames.

use std::time::Duration;

use foxygnal_core::WorkStatus;

use crate::ring::{Consumer, Producer};

/// How long `n` samples last at `rate`.
fn samples_to_duration(n: usize, rate: f64) -> Duration {
    Duration::from_secs_f64((n as f64 / rate).clamp(0.000_5, 0.05))
}

/// Ring size for a buffer of `latency` seconds at `rate`.
pub fn ring_capacity(rate: f64, latency: f64) -> usize {
    ((rate * latency) as usize).max(1024)
}

/// One `work()` step of the sink: move as many whole frames of `inp`
/// (interleaved, `ch` = 1 or 2 values per frame) into the ring as fit.
/// Returns the number of frames consumed and what to tell the scheduler.
pub fn sink_step(
    ring: &mut Producer,
    inp: &[f32],
    ch: usize,
    exhausted: bool,
    rate: f64,
) -> (usize, WorkStatus) {
    let frames = inp.len() / ch;
    if frames == 0 {
        if exhausted && !ring.is_empty() {
            // Upstream ended: let the device play what's queued before the
            // block finishes and the stream is closed.
            return (
                0,
                WorkStatus::Wait(samples_to_duration(ring.len() / ch, rate)),
            );
        }
        return (0, WorkStatus::Blocked);
    }
    let n = frames.min(ring.free() / ch);
    if n == 0 {
        // Full: the device callback unparks us as it drains.
        return (
            0,
            WorkStatus::Wait(samples_to_duration(ring.capacity() / ch / 4, rate)),
        );
    }
    ring.push(&inp[..n * ch]);
    (n, WorkStatus::Ok)
}

/// Device output callback body: fill interleaved `out` (with `channels`
/// device channels) from the ring. A mono ring plays the same sample on
/// every channel. A stereo ring (`stereo`, L/R pairs) plays left and right
/// on the first two channels and silence on any others, or their average
/// on a mono device. Frames the ring can't supply are silent (an underrun).
/// Returns the number of frames that were missing.
pub fn render<T: Copy>(
    ring: &mut Consumer,
    out: &mut [T],
    channels: usize,
    stereo: bool,
    silence: T,
    conv: impl Fn(f32) -> T,
) -> usize {
    let channels = channels.max(1);
    let frames = out.len() / channels;
    let got = if !stereo {
        ring.pop_with(frames, |i, v| {
            out[i * channels..(i + 1) * channels].fill(conv(v));
        })
    } else if channels == 1 {
        let mut left = 0.0;
        ring.pop_with(frames * 2, |i, v| {
            if i & 1 == 0 {
                left = v;
            } else {
                out[i / 2] = conv(0.5 * (left + v));
            }
        }) / 2
    } else {
        ring.pop_with(frames * 2, |i, v| {
            let base = i / 2 * channels;
            out[base + (i & 1)] = conv(v);
            if i & 1 == 1 {
                out[base + 2..base + channels].fill(silence);
            }
        }) / 2
    };
    out[got * channels..].fill(silence);
    frames - got
}

/// Which input channels a capture keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// One channel (mono).
    One(usize),
    /// The first two as L/R pairs (stereo). A mono device feeds both.
    Stereo,
}

/// Device input callback body: push the picked channels of interleaved
/// `data` into the ring, whole frames only. Frames that don't fit are
/// dropped (an overrun). Returns how many were dropped.
pub fn capture<T: Copy>(
    ring: &mut Producer,
    data: &[T],
    channels: usize,
    pick: Pick,
    conv: impl Fn(T) -> f32,
) -> usize {
    let channels = channels.max(1);
    let frames = data.len() / channels;
    let pushed = match pick {
        Pick::One(channel) => ring.push_with(frames, |i| conv(data[i * channels + channel])),
        Pick::Stereo => {
            let n = frames.min(ring.free() / 2);
            let right = 1.min(channels - 1);
            ring.push_with(n * 2, |i| {
                let f = i / 2 * channels;
                conv(data[f + if i & 1 == 0 { 0 } else { right }])
            }) / 2
        }
    };
    frames - pushed
}

/// One `work()` step of the source: copy whole frames (`ch` values each)
/// the device captured into `out`. Returns the number of frames.
pub fn source_step(
    ring: &mut Consumer,
    out: &mut [f32],
    ch: usize,
    rate: f64,
) -> (usize, WorkStatus) {
    let room = out.len() / ch;
    if room == 0 {
        return (0, WorkStatus::Blocked);
    }
    let n = ring.pop(&mut out[..room * ch]) / ch;
    if n == 0 {
        // The device callback unparks us when it has captured more.
        return (
            0,
            WorkStatus::Wait(samples_to_duration(ring.capacity() / ch / 4, rate)),
        );
    }
    (n, WorkStatus::Ok)
}

/// The values of a mono (f32) or stereo (c32: L/R pairs) stream's bytes.
pub fn as_floats(bytes: &[u8]) -> &[f32] {
    match bytes {
        [] => &[],
        b => bytemuck::cast_slice(b),
    }
}

/// The writable values of a mono or stereo stream's bytes.
pub fn as_floats_mut(bytes: &mut [u8]) -> &mut [f32] {
    match bytes {
        [] => &mut [],
        b => bytemuck::cast_slice_mut(b),
    }
}

/// Round a stream rate to whole Hz, as sound cards need.
pub fn whole_hz(rate: f64) -> Result<u32, String> {
    let r = rate.round();
    if (rate - r).abs() > 1e-6 || r < 1.0 || r > u32::MAX as f64 {
        return Err(format!(
            "a sound card needs a whole number of samples per second, but the stream runs at \
             {rate} S/s; resample it first"
        ));
    }
    Ok(r as u32)
}

#[cfg(feature = "audio")]
pub use device::register;

/// Without the `audio` feature there are no audio blocks.
#[cfg(not(feature = "audio"))]
pub fn register(_r: &mut foxygnal_core::Registry) {}

#[cfg(feature = "audio")]
mod device {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, SupportedStreamConfigRange};
    use foxygnal_core::{
        Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
        PortSpec, Registry, StreamType, WorkIo, WorkStatus,
    };
    use serde_json::json;

    use super::{
        Pick, as_floats, as_floats_mut, capture, render, ring_capacity, sink_step, source_step,
        whole_hz,
    };
    use crate::params;
    use crate::ring::{Consumer, Producer, ring};

    fn channels_param(help: &'static str) -> ParamSpec {
        ParamSpec {
            name: "type",
            label: "Channels",
            kind: ParamKind::Enum {
                options: vec!["f32", "c32"],
            },
            default: json!("f32"),
            help,
            live: false,
        }
    }

    /// `type`: c32 is stereo; anything else (older graphs have none) mono.
    fn stereo(p: &ParamReader<'_>) -> Result<bool, Error> {
        Ok(p.stream_type("type")? == StreamType::C32)
    }

    fn port_type(stereo: bool) -> StreamType {
        if stereo {
            StreamType::C32
        } else {
            StreamType::F32
        }
    }

    fn device_param() -> ParamSpec {
        ParamSpec {
            name: "device",
            label: "Device",
            kind: ParamKind::String,
            default: json!(""),
            help: "Empty = the system default. Otherwise part of the device's name; the error \
                   message lists the available devices.",
            live: false,
        }
    }

    fn latency_param() -> ParamSpec {
        ParamSpec {
            name: "latency",
            label: "Buffer",
            kind: ParamKind::Float {
                min: Some(0.005),
                max: Some(5.0),
                unit: Some("s"),
            },
            default: json!(0.1),
            help: "How much audio is buffered between the graph and the sound card. Larger \
                   survives hiccups, smaller reacts faster.",
            live: false,
        }
    }

    pub fn register(r: &mut Registry) {
        r.register(
            BlockDescriptor {
                kind: "audio_sink",
                label: "Audio Sink",
                category: "Sinks",
                help: "Plays its input on the sound card at the stream's sample rate. The \
                       sound card sets the pace, so no throttle is needed. For stereo set the \
                       channels to c32 and feed left as the real part, right as the imaginary \
                       part (e.g. through Re + Im → Complex).",
                inputs: vec![PortSpec {
                    name: "in",
                    ty: None,
                }],
                outputs: vec![],
                params: vec![
                    channels_param(
                        "f32: mono, the same on every speaker. c32: stereo, real part = left, \
                         imaginary part = right (mixed down on a mono device).",
                    ),
                    device_param(),
                    latency_param(),
                ],
            },
            build_sink,
        );
        r.register(
            BlockDescriptor {
                kind: "audio_source",
                label: "Audio Source",
                category: "Sources",
                help: "Records from the sound card: one channel (f32), or the first two as \
                       stereo (c32, left = real part, right = imaginary part).",
                inputs: vec![],
                outputs: vec![PortSpec {
                    name: "out",
                    ty: None,
                }],
                params: vec![
                    channels_param(
                        "f32: mono, the channel below. c32: stereo from the first two \
                         channels, real part = left, imaginary part = right.",
                    ),
                    device_param(),
                    params::sample_rate(),
                    ParamSpec {
                        name: "channel",
                        label: "Channel",
                        kind: ParamKind::Int {
                            min: Some(0),
                            max: Some(63),
                        },
                        default: json!(0),
                        help: "Which input channel to record in mono (0 = first / left).",
                        live: false,
                    },
                    latency_param(),
                ],
            },
            build_source,
        );
    }

    /// A device error reported by the callback thread, picked up by `work()`
    /// through an atomic flag (the mutex is only touched once it is set).
    #[derive(Default)]
    struct Failure {
        failed: AtomicBool,
        msg: Mutex<Option<String>>,
    }

    impl Failure {
        fn set(&self, msg: String) {
            if let Ok(mut m) = self.msg.lock() {
                m.get_or_insert(msg);
            }
            self.failed.store(true, Ordering::Release);
        }

        fn check(&self) -> Result<(), Error> {
            if !self.failed.load(Ordering::Acquire) {
                return Ok(());
            }
            let msg = self.msg.lock().ok().and_then(|m| m.clone());
            Err(Error::Other(format!(
                "audio device error: {}",
                msg.unwrap_or_else(|| "unknown".into())
            )))
        }
    }

    fn cpal_err(what: &str, e: impl std::fmt::Display) -> Error {
        Error::Other(format!("{what}: {e}"))
    }

    /// Find a device by (part of) its name, or the default one.
    fn find_device(want: &str, output: bool) -> Result<cpal::Device, Error> {
        let host = cpal::default_host();
        let dir = if output { "output" } else { "input" };
        if want.is_empty() {
            let d = if output {
                host.default_output_device()
            } else {
                host.default_input_device()
            };
            return d.ok_or_else(|| Error::Other(format!("no default audio {dir} device")));
        }
        let devices: Vec<cpal::Device> = if output {
            host.output_devices()
                .map_err(|e| cpal_err("cannot list audio devices", e))?
                .collect()
        } else {
            host.input_devices()
                .map_err(|e| cpal_err("cannot list audio devices", e))?
                .collect()
        };
        // (id, name): ids are unique (`hw:CARD=…`), names often aren't.
        let named: Vec<(String, String)> = devices
            .iter()
            .map(|d| {
                let id = d.id().map(|i| i.id().to_owned()).unwrap_or_default();
                (id, d.to_string())
            })
            .collect();
        let lower = want.to_lowercase();
        let found = named.iter().position(|(id, _)| id == want).or_else(|| {
            named.iter().position(|(id, name)| {
                id.to_lowercase().contains(&lower) || name.to_lowercase().contains(&lower)
            })
        });
        if let Some(i) = found {
            return Ok(devices[i].clone());
        }
        let mut names: Vec<String> = named
            .iter()
            .map(|(id, name)| format!("`{id}` ({name})"))
            .collect();
        names.dedup();
        Err(Error::Other(format!(
            "no audio {dir} device matches `{want}`; available: {}",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        )))
    }

    fn format_rank(f: SampleFormat) -> Option<u8> {
        Some(match f {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            SampleFormat::I32 => 2,
            SampleFormat::F64 => 3,
            SampleFormat::U16 => 4,
            SampleFormat::I8 => 5,
            SampleFormat::U8 => 6,
            _ => return None,
        })
    }

    /// Pick the config to open: one that runs at `rate` with at least
    /// `min_channels`, preferring f32 and few channels. On failure, the error
    /// lists the rates the device does support.
    fn pick_config(
        device: &cpal::Device,
        configs: Vec<SupportedStreamConfigRange>,
        rate: u32,
        min_channels: u16,
    ) -> Result<cpal::SupportedStreamConfig, Error> {
        let best = configs
            .iter()
            .filter(|c| c.contains_rate(rate) && c.channels() >= min_channels)
            .filter_map(|c| format_rank(c.sample_format()).map(|r| (r, c.channels(), *c)))
            .min_by_key(|&(r, ch, _)| (r, ch));
        if let Some((_, _, c)) = best {
            return Ok(c.with_sample_rate(rate));
        }
        let mut ranges: Vec<(u32, u32)> = configs
            .iter()
            .filter(|c| c.channels() >= min_channels)
            .map(|c| (c.min_sample_rate(), c.max_sample_rate()))
            .collect();
        ranges.sort_unstable();
        ranges.dedup();
        let list: Vec<String> = ranges
            .iter()
            .map(|&(lo, hi)| {
                if lo == hi {
                    format!("{lo} Hz")
                } else {
                    format!("{lo}–{hi} Hz")
                }
            })
            .collect();
        Err(Error::Other(format!(
            "audio device `{device}` can't run at {rate} Hz{}. Supported: {}. Resample the \
             stream to a supported rate (e.g. 48000 Hz) before this block.",
            if min_channels > 1 {
                format!(" with {min_channels} channels")
            } else {
                String::new()
            },
            if list.is_empty() {
                "nothing".into()
            } else {
                list.join(", ")
            }
        )))
    }

    /// Pick a config with at least `want` channels if the device has one,
    /// otherwise fewer (stereo then mixes down, or a mono input feeds both
    /// sides).
    fn pick_config_prefer(
        device: &cpal::Device,
        configs: Vec<SupportedStreamConfigRange>,
        rate: u32,
        want: u16,
    ) -> Result<cpal::SupportedStreamConfig, Error> {
        if want > 1
            && let Ok(c) = pick_config(device, configs.clone(), rate, want)
        {
            return Ok(c);
        }
        pick_config(device, configs, rate, 1)
    }

    // ----- sink -----

    pub struct AudioSink {
        device: String,
        latency: f64,
        /// c32 input: L/R frames.
        stereo: bool,
        /// From stream metadata, in `configure`.
        rate: Option<u32>,
        ring: Option<Producer>,
        /// Kept alive while running; dropped in `stop`.
        stream: Option<cpal::Stream>,
        failure: Arc<Failure>,
    }

    fn build_output<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        stereo: bool,
        mut ring: Consumer,
        failure: Arc<Failure>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
    {
        let waker = std::thread::current();
        let channels = config.channels as usize;
        device.build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| {
                render(
                    &mut ring,
                    out,
                    channels,
                    stereo,
                    T::EQUILIBRIUM,
                    T::from_sample,
                );
                waker.unpark();
            },
            move |e| failure.set(e.to_string()),
            None,
        )
    }

    impl Block for AudioSink {
        // The device callback unparks the thread that ran `start`, and cpal
        // streams are best created and dropped on one thread.
        fn dedicated_thread(&self) -> bool {
            true
        }

        fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
            let rate = cx.inputs[0].sample_rate.ok_or_else(|| {
                Error::Other(
                    "the audio sink plays at the stream's sample rate, but nothing upstream \
                     sets one (set it on the source)"
                        .into(),
                )
            })?;
            self.rate = Some(whole_hz(rate).map_err(Error::Other)?);
            Ok(())
        }

        fn start(&mut self) -> Result<(), Error> {
            let rate = self
                .rate
                .ok_or_else(|| Error::Other("not configured".into()))?;
            let device = find_device(&self.device, true)?;
            let configs: Vec<_> = device
                .supported_output_configs()
                .map_err(|e| cpal_err(&format!("audio device `{device}`"), e))?
                .collect();
            let ch = if self.stereo { 2 } else { 1 };
            let supported = pick_config_prefer(&device, configs, rate, ch)?;
            let (prod, cons) = ring(ring_capacity(rate as f64, self.latency) * ch as usize);
            let config = supported.config();
            let (f, st) = (self.failure.clone(), self.stereo);
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build_output::<f32>(&device, config, st, cons, f),
                SampleFormat::I16 => build_output::<i16>(&device, config, st, cons, f),
                SampleFormat::I32 => build_output::<i32>(&device, config, st, cons, f),
                SampleFormat::F64 => build_output::<f64>(&device, config, st, cons, f),
                SampleFormat::U16 => build_output::<u16>(&device, config, st, cons, f),
                SampleFormat::I8 => build_output::<i8>(&device, config, st, cons, f),
                _ => build_output::<u8>(&device, config, st, cons, f),
            }
            .map_err(|e| cpal_err(&format!("cannot open audio device `{device}`"), e))?;
            stream
                .play()
                .map_err(|e| cpal_err(&format!("cannot start audio device `{device}`"), e))?;
            self.ring = Some(prod);
            self.stream = Some(stream);
            Ok(())
        }

        fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
            self.failure.check()?;
            let (Some(ring), Some(rate)) = (self.ring.as_mut(), self.rate) else {
                return Err(Error::Other("the audio device isn't open".into()));
            };
            let exhausted = io.inputs[0].exhausted();
            let inp = as_floats(io.inputs[0].bytes());
            let ch = if self.stereo { 2 } else { 1 };
            let (n, status) = sink_step(ring, inp, ch, exhausted, rate as f64);
            io.consume(0, n);
            Ok(status)
        }

        fn stop(&mut self) -> Result<(), Error> {
            self.stream = None;
            self.ring = None;
            Ok(())
        }
    }

    fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
        let stereo = stereo(p)?;
        Ok(Instance {
            block: Box::new(AudioSink {
                device: p.str("device")?.to_owned(),
                latency: p.f64("latency")?,
                stereo,
                rate: None,
                ring: None,
                stream: None,
                failure: Arc::default(),
            }),
            inputs: vec![("in".into(), port_type(stereo))],
            outputs: vec![],
        })
    }

    // ----- source -----

    pub struct AudioSource {
        device: String,
        latency: f64,
        rate: f64,
        pick: Pick,
        ring: Option<Consumer>,
        stream: Option<cpal::Stream>,
        failure: Arc<Failure>,
    }

    fn build_input<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        pick: Pick,
        mut ring: Producer,
        failure: Arc<Failure>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let waker = std::thread::current();
        let channels = config.channels as usize;
        device.build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                capture(&mut ring, data, channels, pick, |s| s.to_sample::<f32>());
                waker.unpark();
            },
            move |e| failure.set(e.to_string()),
            None,
        )
    }

    impl AudioSource {
        fn ch(&self) -> usize {
            if self.pick == Pick::Stereo { 2 } else { 1 }
        }
    }

    impl Block for AudioSource {
        // The device callback unparks the thread that ran `start`, and cpal
        // streams are best created and dropped on one thread.
        fn dedicated_thread(&self) -> bool {
            true
        }

        fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
            whole_hz(self.rate).map_err(Error::Other)?;
            cx.set_rate(self.rate);
            Ok(())
        }

        fn start(&mut self) -> Result<(), Error> {
            let rate = whole_hz(self.rate).map_err(Error::Other)?;
            let device = find_device(&self.device, false)?;
            let configs: Vec<_> = device
                .supported_input_configs()
                .map_err(|e| cpal_err(&format!("audio device `{device}`"), e))?
                .collect();
            let supported = match self.pick {
                Pick::One(c) => pick_config(&device, configs, rate, c as u16 + 1)?,
                Pick::Stereo => pick_config_prefer(&device, configs, rate, 2)?,
            };
            let (prod, cons) = ring(ring_capacity(self.rate, self.latency) * self.ch());
            let config = supported.config();
            let (f, p) = (self.failure.clone(), self.pick);
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build_input::<f32>(&device, config, p, prod, f),
                SampleFormat::I16 => build_input::<i16>(&device, config, p, prod, f),
                SampleFormat::I32 => build_input::<i32>(&device, config, p, prod, f),
                SampleFormat::F64 => build_input::<f64>(&device, config, p, prod, f),
                SampleFormat::U16 => build_input::<u16>(&device, config, p, prod, f),
                SampleFormat::I8 => build_input::<i8>(&device, config, p, prod, f),
                _ => build_input::<u8>(&device, config, p, prod, f),
            }
            .map_err(|e| cpal_err(&format!("cannot open audio device `{device}`"), e))?;
            stream
                .play()
                .map_err(|e| cpal_err(&format!("cannot start audio device `{device}`"), e))?;
            self.ring = Some(cons);
            self.stream = Some(stream);
            Ok(())
        }

        fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
            self.failure.check()?;
            let ch = self.ch();
            let ring = self
                .ring
                .as_mut()
                .ok_or_else(|| Error::Other("the audio device isn't open".into()))?;
            let out = as_floats_mut(io.outputs[0].bytes_mut());
            let (n, status) = source_step(ring, out, ch, self.rate);
            io.produce(0, n);
            Ok(status)
        }

        fn stop(&mut self) -> Result<(), Error> {
            self.stream = None;
            self.ring = None;
            Ok(())
        }
    }

    fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
        let stereo = stereo(p)?;
        Ok(Instance {
            block: Box::new(AudioSource {
                device: p.str("device")?.to_owned(),
                latency: p.f64("latency")?,
                rate: p.f64("sample_rate")?,
                pick: if stereo {
                    Pick::Stereo
                } else {
                    Pick::One(p.u64("channel")? as usize)
                },
                ring: None,
                stream: None,
                failure: Arc::default(),
            }),
            inputs: vec![],
            outputs: vec![("out".into(), port_type(stereo))],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::ring;

    #[test]
    fn sink_paces_on_a_full_ring_and_drains_at_end() {
        let (mut p, mut c) = ring(1024);
        let inp = vec![0.5f32; 3000];
        let (n, st) = sink_step(&mut p, &inp, 1, false, 48_000.0);
        assert_eq!((n, st), (1024, WorkStatus::Ok));
        // Full: park instead of spinning, for about a quarter of the ring.
        let (n, st) = sink_step(&mut p, &inp[n..], 1, false, 48_000.0);
        assert_eq!(n, 0);
        let WorkStatus::Wait(d) = st else {
            panic!("expected Wait, got {st:?}")
        };
        assert!(d >= Duration::from_millis(4) && d <= Duration::from_millis(6));
        // The device plays 256 frames of stereo: room again.
        let mut out = vec![0.0f32; 512];
        assert_eq!(render(&mut c, &mut out, 2, false, 0.0, |v| v), 0);
        assert!(out.iter().all(|&v| v == 0.5));
        assert_eq!(sink_step(&mut p, &inp, 1, false, 48_000.0).0, 256);
        // No input yet, upstream still running: blocked.
        assert_eq!(
            sink_step(&mut p, &[], 1, false, 48_000.0).1,
            WorkStatus::Blocked
        );
        // Upstream ended with audio still queued: wait for it to play.
        assert!(matches!(
            sink_step(&mut p, &[], 1, true, 48_000.0).1,
            WorkStatus::Wait(_)
        ));
        let mut drain = vec![0.0f32; 2048];
        render(&mut c, &mut drain, 1, false, 0.0, |v| v);
        assert_eq!(
            sink_step(&mut p, &[], 1, true, 48_000.0).1,
            WorkStatus::Blocked
        );
    }

    #[test]
    fn render_underrun_is_silence() {
        let (mut p, mut c) = ring(16);
        p.push(&[1.0, 2.0, 3.0]);
        let mut out = [7i16; 10];
        let missing = render(&mut c, &mut out, 2, false, 0, |v| (v * 100.0) as i16);
        assert_eq!(missing, 2);
        assert_eq!(out, [100, 100, 200, 200, 300, 300, 0, 0, 0, 0]);
    }

    #[test]
    fn stereo_sink_moves_whole_frames() {
        let (mut p, mut c) = ring(8);
        // Five L/R frames, but only four fit; never half a frame.
        let inp = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0, 5.0, -5.0];
        assert_eq!(
            sink_step(&mut p, &inp, 2, false, 48_000.0),
            (4, WorkStatus::Ok)
        );
        assert!(matches!(
            sink_step(&mut p, &inp[8..], 2, false, 48_000.0).1,
            WorkStatus::Wait(_)
        ));
        // A trailing half frame is not consumed.
        let (mut p2, _c2) = ring(8);
        assert_eq!(sink_step(&mut p2, &inp[..3], 2, false, 48_000.0).0, 1);
        // Stereo device: L/R as they are.
        let mut out = [0.0f32; 4];
        assert_eq!(render(&mut c, &mut out, 2, true, 0.0, |v| v), 0);
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0]);
        // Surround device: L/R on the first two channels, silence elsewhere,
        // and silence after an underrun.
        let mut out = [9.0f32; 12];
        assert_eq!(render(&mut c, &mut out, 4, true, 0.0, |v| v), 1);
        assert_eq!(
            out,
            [3.0, -3.0, 0.0, 0.0, 4.0, -4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn stereo_on_a_mono_device_mixes_down() {
        let (mut p, mut c) = ring(16);
        p.push(&[1.0, 0.0, 0.5, 0.5, -1.0, 0.0]);
        let mut out = [7i16; 4];
        let missing = render(&mut c, &mut out, 1, true, 0, |v| (v * 100.0) as i16);
        assert_eq!(missing, 1);
        assert_eq!(out, [50, 50, -50, 0]);
    }

    #[test]
    fn capture_picks_a_channel_and_drops_on_overrun() {
        let (mut p, mut c) = ring(2);
        let data = [1i16, 10, 2, 20, 3, 30];
        let dropped = capture(&mut p, &data, 2, Pick::One(1), |s| s as f32);
        assert_eq!(dropped, 1);
        let mut out = [0.0f32; 4];
        let (n, st) = source_step(&mut c, &mut out, 1, 48_000.0);
        assert_eq!((n, st), (2, WorkStatus::Ok));
        assert_eq!(&out[..2], &[10.0, 20.0]);
        assert!(matches!(
            source_step(&mut c, &mut out, 1, 48_000.0).1,
            WorkStatus::Wait(_)
        ));
    }

    #[test]
    fn stereo_capture_keeps_whole_frames() {
        let (mut p, mut c) = ring(4);
        // Three channels; the first two are L/R. Room for two frames.
        let data = [1i16, 10, 100, 2, 20, 200, 3, 30, 300];
        assert_eq!(capture(&mut p, &data, 3, Pick::Stereo, |s| s as f32), 1);
        // An output with room for one and a half frames takes one.
        let mut out = [0.0f32; 3];
        assert_eq!(
            source_step(&mut c, &mut out, 2, 48_000.0),
            (1, WorkStatus::Ok)
        );
        assert_eq!(&out[..2], &[1.0, 10.0]);
        assert_eq!(
            source_step(&mut c, &mut out, 2, 48_000.0),
            (1, WorkStatus::Ok)
        );
        assert_eq!(&out[..2], &[2.0, 20.0]);
        // A mono device feeds both sides.
        capture(&mut p, &[5i16, 6], 1, Pick::Stereo, |s| s as f32);
        let mut out = [0.0f32; 4];
        assert_eq!(source_step(&mut c, &mut out, 2, 48_000.0).0, 2);
        assert_eq!(out, [5.0, 5.0, 6.0, 6.0]);
    }

    #[test]
    fn stream_bytes_as_floats() {
        let v = [1.0f32, 2.0];
        assert_eq!(as_floats(bytemuck::cast_slice(&v)), &v);
        assert!(as_floats(&[]).is_empty());
    }

    #[test]
    fn whole_hz_rejects_fractions() {
        assert_eq!(whole_hz(48_000.0), Ok(48_000));
        assert!(whole_hz(48_000.5).unwrap_err().contains("resample"));
    }
}
