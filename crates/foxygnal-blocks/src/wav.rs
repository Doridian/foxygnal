//! WAV files (via `hound`, pure Rust).

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use serde_json::json;

use crate::convert::f32_to_i16;
use crate::params;

/// Most channels a WAV source reads (frames are decoded on the stack).
const MAX_CHANNELS: u16 = 64;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "wav_source",
            label: "WAV Source",
            category: "Sources",
            help: "Reads a WAV file (8–32-bit integer or float PCM). The sample rate comes from \
                   the file. c32 reads a stereo file as I/Q (left = I, right = Q), as many SDR \
                   programs record it.",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::stream_type("f32", vec!["f32", "c32", "i16"]),
                path_param("The WAV file to read."),
                ParamSpec {
                    name: "channel",
                    label: "Channel",
                    kind: ParamKind::Int {
                        min: Some(0),
                        max: Some(MAX_CHANNELS as i64 - 1),
                    },
                    default: json!(0),
                    help: "For f32 / i16: which channel to read (0 = first / left). Ignored \
                           for c32.",
                    live: false,
                },
                ParamSpec {
                    name: "repeat",
                    label: "Loop",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "Start over at end of file instead of ending the stream.",
                    live: false,
                },
            ],
        },
        build_source,
    );
    r.register(
        BlockDescriptor {
            kind: "wav_sink",
            label: "WAV Sink",
            category: "Sinks",
            help: "Writes a WAV file at the stream's sample rate. c32 writes a stereo file \
                   (left = I, right = Q).",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![
                params::stream_type("f32", vec!["f32", "c32", "i16"]),
                path_param("The WAV file to write (replaced if it exists)."),
                ParamSpec {
                    name: "format",
                    label: "Format",
                    kind: ParamKind::Enum {
                        options: vec!["float32", "pcm16"],
                    },
                    default: json!("float32"),
                    help: "float32 keeps everything; pcm16 plays anywhere but clips beyond ±1.",
                    live: false,
                },
            ],
        },
        build_sink,
    );
}

fn path_param(help: &'static str) -> ParamSpec {
    ParamSpec {
        name: "path",
        label: "File",
        kind: ParamKind::Path,
        default: json!(""),
        help,
        live: false,
    }
}

fn path_from(p: &ParamReader<'_>) -> Result<PathBuf, Error> {
    let path = PathBuf::from(p.str("path")?);
    if path.as_os_str().is_empty() {
        return Err(Error::Other("no file chosen".into()));
    }
    Ok(path)
}

fn wav_err(path: &Path, e: hound::Error) -> Error {
    match e {
        hound::Error::IoError(e) => Error::Other(format!("cannot open {}: {e}", path.display())),
        e => Error::Other(format!("{}: {e}", path.display())),
    }
}

pub struct WavSource {
    path: PathBuf,
    ty: StreamType,
    channel: usize,
    repeat: bool,
    /// The header as read in `configure`.
    spec: Option<WavSpec>,
    reader: Option<WavReader<BufReader<File>>>,
}

impl WavSource {
    fn check_spec(&self, spec: &WavSpec) -> Result<(), Error> {
        let ch = spec.channels;
        if ch == 0 || ch > MAX_CHANNELS {
            return Err(Error::Other(format!(
                "{} has {ch} channels; at most {MAX_CHANNELS} are supported",
                self.path.display()
            )));
        }
        if self.ty == StreamType::C32 && ch < 2 {
            return Err(Error::Other(format!(
                "{} is mono, but c32 reads a stereo file as I/Q; choose f32",
                self.path.display()
            )));
        }
        if self.ty != StreamType::C32 && self.channel >= ch as usize {
            return Err(Error::Other(format!(
                "{} has {ch} channel(s), so there is no channel {}",
                self.path.display(),
                self.channel
            )));
        }
        if spec.sample_format == SampleFormat::Float && spec.bits_per_sample != 32 {
            return Err(Error::Other(format!(
                "{}: only 32-bit float WAVs are supported",
                self.path.display()
            )));
        }
        Ok(())
    }

    /// Read up to `max` whole frames, handing each (normalized to ±1) to
    /// `f(i, frame)`. Returns frames read and whether the file ended.
    fn read_frames(
        &mut self,
        max: usize,
        mut f: impl FnMut(usize, &[f32]),
    ) -> Result<(usize, bool), Error> {
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| Error::Other("the file isn't open (start() not called)".into()))?;
        let spec = reader.spec();
        let ch = spec.channels as usize;
        let mut frame = [0.0f32; MAX_CHANNELS as usize];
        let frame = &mut frame[..ch];
        let mut n = 0;
        let path = &self.path;
        let mut fill = |next: &mut dyn FnMut() -> Option<hound::Result<f32>>| {
            while n < max {
                for v in frame.iter_mut() {
                    match next() {
                        Some(Ok(s)) => *v = s,
                        Some(Err(e)) => return Err(wav_err(path, e)),
                        // A partial frame at the end is dropped.
                        None => return Ok(true),
                    }
                }
                f(n, frame);
                n += 1;
            }
            Ok(false)
        };
        let eof = match spec.sample_format {
            SampleFormat::Float => {
                let mut it = reader.samples::<f32>();
                fill(&mut || it.next())?
            }
            SampleFormat::Int => {
                let k = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
                let mut it = reader.samples::<i32>();
                fill(&mut || it.next().map(|r| r.map(|v| v as f32 * k)))?
            }
        };
        Ok((n, eof))
    }
}

fn read_header(path: &Path) -> Result<WavSpec, Error> {
    Ok(WavReader::open(path).map_err(|e| wav_err(path, e))?.spec())
}

impl Block for WavSource {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        // Reads the header only; nothing is kept open.
        let spec = read_header(&self.path)?;
        self.check_spec(&spec)?;
        cx.set_rate(spec.sample_rate as f64);
        self.spec = Some(spec);
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let file = File::open(&self.path)
            .map_err(|e| Error::Other(format!("cannot open {}: {e}", self.path.display())))?;
        let reader = WavReader::new(BufReader::with_capacity(1 << 16, file))
            .map_err(|e| wav_err(&self.path, e))?;
        let spec = reader.spec();
        self.check_spec(&spec)?;
        if self.spec.is_some_and(|s| s.sample_rate != spec.sample_rate) {
            return Err(Error::Other(format!(
                "{} changed since the graph was configured",
                self.path.display()
            )));
        }
        self.reader = Some(reader);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let c = self.channel;
        let (n, eof) = match self.ty {
            StreamType::C32 => {
                let out = io.outputs[0].slice_mut::<Complex32>();
                let max = out.len();
                self.read_frames(max, |i, fr| out[i] = Complex32::new(fr[0], fr[1]))?
            }
            StreamType::I16 => {
                let out = io.outputs[0].slice_mut::<i16>();
                let max = out.len();
                self.read_frames(max, |i, fr| out[i] = f32_to_i16(fr[c], 32768.0))?
            }
            _ => {
                let out = io.outputs[0].slice_mut::<f32>();
                let max = out.len();
                self.read_frames(max, |i, fr| out[i] = fr[c])?
            }
        };
        io.produce(0, n);
        if eof {
            let reader = self.reader.as_mut().expect("opened in start");
            if self.repeat && reader.duration() > 0 {
                reader.seek(0)?;
                return Ok(WorkStatus::Ok);
            }
            return Ok(WorkStatus::Done);
        }
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    if ty == StreamType::U8 {
        return Err(Error::Other("a WAV source outputs f32, c32 or i16".into()));
    }
    Ok(Instance {
        block: Box::new(WavSource {
            path: path_from(p)?,
            ty,
            channel: p.u64("channel")? as usize,
            repeat: p.bool("repeat")?,
            spec: None,
            reader: None,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct WavSink {
    path: PathBuf,
    ty: StreamType,
    pcm16: bool,
    /// From stream metadata, in `configure`.
    spec: Option<WavSpec>,
    writer: Option<WavWriter<BufWriter<File>>>,
}

impl Block for WavSink {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs[0].sample_rate.ok_or_else(|| {
            Error::Other(
                "a WAV file records its sample rate, but nothing upstream sets one \
                 (set it on the source)"
                    .into(),
            )
        })?;
        let r = rate.round();
        if (rate - r).abs() > 1e-6 || r < 1.0 || r > u32::MAX as f64 {
            return Err(Error::Other(format!(
                "a WAV file needs a whole number of samples per second, but the stream runs \
                 at {rate} S/s"
            )));
        }
        self.spec = Some(WavSpec {
            channels: if self.ty == StreamType::C32 { 2 } else { 1 },
            sample_rate: r as u32,
            bits_per_sample: if self.pcm16 { 16 } else { 32 },
            sample_format: if self.pcm16 {
                SampleFormat::Int
            } else {
                SampleFormat::Float
            },
        });
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let spec = self
            .spec
            .ok_or_else(|| Error::Other("not configured".into()))?;
        let file = File::create(&self.path)
            .map_err(|e| Error::Other(format!("cannot create {}: {e}", self.path.display())))?;
        self.writer = Some(
            WavWriter::new(BufWriter::with_capacity(1 << 16, file), spec)
                .map_err(|e| wav_err(&self.path, e))?,
        );
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::Other("the file isn't open (start() not called)".into()))?;
        let (pcm16, path) = (self.pcm16, &self.path);
        let put = |w: &mut WavWriter<_>, v: f32| {
            if pcm16 {
                w.write_sample(f32_to_i16(v, 32767.0))
            } else {
                w.write_sample(v)
            }
            .map_err(|e| wav_err(path, e))
        };
        let n = match self.ty {
            StreamType::C32 => {
                let inp = io.inputs[0].slice::<Complex32>();
                for x in inp {
                    put(w, x.re)?;
                    put(w, x.im)?;
                }
                inp.len()
            }
            StreamType::I16 => {
                let inp = io.inputs[0].slice::<i16>();
                for &x in inp {
                    if pcm16 {
                        w.write_sample(x).map_err(|e| wav_err(path, e))?;
                    } else {
                        put(w, x as f32 / 32768.0)?;
                    }
                }
                inp.len()
            }
            _ => {
                let inp = io.inputs[0].slice::<f32>();
                for &x in inp {
                    put(w, x)?;
                }
                inp.len()
            }
        };
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        if let Some(w) = self.writer.take() {
            w.finalize().map_err(|e| wav_err(&self.path, e))?;
        }
        Ok(())
    }
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    if ty == StreamType::U8 {
        return Err(Error::Other("a WAV sink takes f32, c32 or i16".into()));
    }
    Ok(Instance {
        block: Box::new(WavSink {
            path: path_from(p)?,
            ty,
            pcm16: p.str("format")? == "pcm16",
            spec: None,
            writer: None,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}
