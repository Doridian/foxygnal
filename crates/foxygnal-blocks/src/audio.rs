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

/// One `work()` step of the sink: move as much of `inp` into the ring as
/// fits. Returns the number of samples consumed and what to tell the
/// scheduler.
pub fn sink_step(
    ring: &mut Producer,
    inp: &[f32],
    exhausted: bool,
    rate: f64,
) -> (usize, WorkStatus) {
    if inp.is_empty() {
        if exhausted && !ring.is_empty() {
            // Upstream ended: let the device play what's queued before the
            // block finishes and the stream is closed.
            return (0, WorkStatus::Wait(samples_to_duration(ring.len(), rate)));
        }
        return (0, WorkStatus::Blocked);
    }
    let n = ring.push(inp);
    if n == 0 {
        // Full: the device callback unparks us as it drains.
        return (
            0,
            WorkStatus::Wait(samples_to_duration(ring.capacity() / 4, rate)),
        );
    }
    (n, WorkStatus::Ok)
}

/// Device output callback body: fill interleaved `out` (with `channels`
/// channels) from the mono ring, the same sample on every channel. Frames the
/// ring can't supply are silent (an underrun). Returns the number of frames
/// that were missing.
pub fn render<T: Copy>(
    ring: &mut Consumer,
    out: &mut [T],
    channels: usize,
    silence: T,
    conv: impl Fn(f32) -> T,
) -> usize {
    let channels = channels.max(1);
    let frames = out.len() / channels;
    let got = ring.pop_with(frames, |i, v| {
        let s = conv(v);
        for o in &mut out[i * channels..(i + 1) * channels] {
            *o = s;
        }
    });
    for o in &mut out[got * channels..] {
        *o = silence;
    }
    frames - got
}

/// Device input callback body: push channel `channel` of interleaved `data`
/// into the ring. Samples that don't fit are dropped (an overrun). Returns
/// how many were dropped.
pub fn capture<T: Copy>(
    ring: &mut Producer,
    data: &[T],
    channels: usize,
    channel: usize,
    conv: impl Fn(T) -> f32,
) -> usize {
    let channels = channels.max(1);
    let frames = data.len() / channels;
    let pushed = ring.push_with(frames, |i| conv(data[i * channels + channel]));
    frames - pushed
}

/// One `work()` step of the source: copy what the device captured into `out`.
pub fn source_step(ring: &mut Consumer, out: &mut [f32], rate: f64) -> (usize, WorkStatus) {
    if out.is_empty() {
        return (0, WorkStatus::Blocked);
    }
    let n = ring.pop(out);
    if n == 0 {
        // The device callback unparks us when it has captured more.
        return (
            0,
            WorkStatus::Wait(samples_to_duration(ring.capacity() / 4, rate)),
        );
    }
    (n, WorkStatus::Ok)
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

    use super::{capture, render, ring_capacity, sink_step, source_step, whole_hz};
    use crate::params;
    use crate::ring::{Consumer, Producer, ring};

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
                help: "Plays a real stream on the sound card at the stream's sample rate. The \
                       sound card sets the pace, so no throttle is needed.",
                inputs: vec![PortSpec {
                    name: "in",
                    ty: Some(StreamType::F32),
                }],
                outputs: vec![],
                params: vec![device_param(), latency_param()],
            },
            build_sink,
        );
        r.register(
            BlockDescriptor {
                kind: "audio_source",
                label: "Audio Source",
                category: "Sources",
                help: "Records from the sound card (one channel).",
                inputs: vec![],
                outputs: vec![PortSpec {
                    name: "out",
                    ty: Some(StreamType::F32),
                }],
                params: vec![
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
                        help: "Which input channel to record (0 = first / left).",
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

    // ----- sink -----

    pub struct AudioSink {
        device: String,
        latency: f64,
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
                render(&mut ring, out, channels, T::EQUILIBRIUM, T::from_sample);
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
            let supported = pick_config(&device, configs, rate, 1)?;
            let (prod, cons) = ring(ring_capacity(rate as f64, self.latency));
            let config = supported.config();
            let f = self.failure.clone();
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build_output::<f32>(&device, config, cons, f),
                SampleFormat::I16 => build_output::<i16>(&device, config, cons, f),
                SampleFormat::I32 => build_output::<i32>(&device, config, cons, f),
                SampleFormat::F64 => build_output::<f64>(&device, config, cons, f),
                SampleFormat::U16 => build_output::<u16>(&device, config, cons, f),
                SampleFormat::I8 => build_output::<i8>(&device, config, cons, f),
                _ => build_output::<u8>(&device, config, cons, f),
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
            let inp = io.inputs[0].slice::<f32>();
            let (n, status) = sink_step(ring, inp, exhausted, rate as f64);
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
        Ok(Instance {
            block: Box::new(AudioSink {
                device: p.str("device")?.to_owned(),
                latency: p.f64("latency")?,
                rate: None,
                ring: None,
                stream: None,
                failure: Arc::default(),
            }),
            inputs: vec![("in".into(), StreamType::F32)],
            outputs: vec![],
        })
    }

    // ----- source -----

    pub struct AudioSource {
        device: String,
        latency: f64,
        rate: f64,
        channel: usize,
        ring: Option<Consumer>,
        stream: Option<cpal::Stream>,
        failure: Arc<Failure>,
    }

    fn build_input<T>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        channel: usize,
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
                capture(&mut ring, data, channels, channel, |s| s.to_sample::<f32>());
                waker.unpark();
            },
            move |e| failure.set(e.to_string()),
            None,
        )
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
            let supported = pick_config(&device, configs, rate, self.channel as u16 + 1)?;
            let (prod, cons) = ring(ring_capacity(self.rate, self.latency));
            let config = supported.config();
            let (f, ch) = (self.failure.clone(), self.channel);
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build_input::<f32>(&device, config, ch, prod, f),
                SampleFormat::I16 => build_input::<i16>(&device, config, ch, prod, f),
                SampleFormat::I32 => build_input::<i32>(&device, config, ch, prod, f),
                SampleFormat::F64 => build_input::<f64>(&device, config, ch, prod, f),
                SampleFormat::U16 => build_input::<u16>(&device, config, ch, prod, f),
                SampleFormat::I8 => build_input::<i8>(&device, config, ch, prod, f),
                _ => build_input::<u8>(&device, config, ch, prod, f),
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
            let ring = self
                .ring
                .as_mut()
                .ok_or_else(|| Error::Other("the audio device isn't open".into()))?;
            let out = io.outputs[0].slice_mut::<f32>();
            let (n, status) = source_step(ring, out, self.rate);
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
        Ok(Instance {
            block: Box::new(AudioSource {
                device: p.str("device")?.to_owned(),
                latency: p.f64("latency")?,
                rate: p.f64("sample_rate")?,
                channel: p.u64("channel")? as usize,
                ring: None,
                stream: None,
                failure: Arc::default(),
            }),
            inputs: vec![],
            outputs: vec![("out".into(), StreamType::F32)],
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
        let (n, st) = sink_step(&mut p, &inp, false, 48_000.0);
        assert_eq!((n, st), (1024, WorkStatus::Ok));
        // Full: park instead of spinning, for about a quarter of the ring.
        let (n, st) = sink_step(&mut p, &inp[n..], false, 48_000.0);
        assert_eq!(n, 0);
        let WorkStatus::Wait(d) = st else {
            panic!("expected Wait, got {st:?}")
        };
        assert!(d >= Duration::from_millis(4) && d <= Duration::from_millis(6));
        // The device plays 256 frames of stereo: room again.
        let mut out = vec![0.0f32; 512];
        assert_eq!(render(&mut c, &mut out, 2, 0.0, |v| v), 0);
        assert!(out.iter().all(|&v| v == 0.5));
        assert_eq!(sink_step(&mut p, &inp, false, 48_000.0).0, 256);
        // No input yet, upstream still running: blocked.
        assert_eq!(
            sink_step(&mut p, &[], false, 48_000.0).1,
            WorkStatus::Blocked
        );
        // Upstream ended with audio still queued: wait for it to play.
        assert!(matches!(
            sink_step(&mut p, &[], true, 48_000.0).1,
            WorkStatus::Wait(_)
        ));
        let mut drain = vec![0.0f32; 2048];
        render(&mut c, &mut drain, 1, 0.0, |v| v);
        assert_eq!(
            sink_step(&mut p, &[], true, 48_000.0).1,
            WorkStatus::Blocked
        );
    }

    #[test]
    fn render_underrun_is_silence() {
        let (mut p, mut c) = ring(16);
        p.push(&[1.0, 2.0, 3.0]);
        let mut out = [7i16; 10];
        let missing = render(&mut c, &mut out, 2, 0, |v| (v * 100.0) as i16);
        assert_eq!(missing, 2);
        assert_eq!(out, [100, 100, 200, 200, 300, 300, 0, 0, 0, 0]);
    }

    #[test]
    fn capture_picks_a_channel_and_drops_on_overrun() {
        let (mut p, mut c) = ring(2);
        let data = [1i16, 10, 2, 20, 3, 30];
        let dropped = capture(&mut p, &data, 2, 1, |s| s as f32);
        assert_eq!(dropped, 1);
        let mut out = [0.0f32; 4];
        let (n, st) = source_step(&mut c, &mut out, 48_000.0);
        assert_eq!((n, st), (2, WorkStatus::Ok));
        assert_eq!(&out[..2], &[10.0, 20.0]);
        assert!(matches!(
            source_step(&mut c, &mut out, 48_000.0).1,
            WorkStatus::Wait(_)
        ));
    }

    #[test]
    fn whole_hz_rejects_fractions() {
        assert_eq!(whole_hz(48_000.0), Ok(48_000));
        assert!(whole_hz(48_000.5).unwrap_err().contains("resample"));
    }
}
