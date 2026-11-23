//! Raw sample files (interleaved, native little-endian, same layout as GNU Radio).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, WorkIo, WorkStatus,
};
use serde_json::json;

use crate::params;

fn path_param() -> ParamSpec {
    ParamSpec {
        name: "path",
        label: "File",
        kind: ParamKind::Path,
        default: json!(""),
        help: "Raw interleaved samples, little-endian (c32 = float I, float Q, …).",
        live: false,
    }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "file_source",
            label: "File Source",
            category: "Sources",
            help: "Reads raw samples from a file (e.g. a GNU Radio / SDR++ IQ recording).",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::any_type(),
                path_param(),
                ParamSpec {
                    name: "repeat",
                    label: "Loop",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "Start over at end of file instead of ending the stream.",
                    live: false,
                },
                params::optional_rate(
                    "The recording's sample rate, for everything downstream. 0 = unknown.",
                ),
                params::float(
                    "center_freq",
                    "Centre frequency",
                    0.0,
                    Some("Hz"),
                    "The RF frequency the recording is centred on. 0 = unknown / baseband.",
                ),
            ],
        },
        build_source,
    );
    r.register(
        BlockDescriptor {
            kind: "file_sink",
            label: "File Sink",
            category: "Sinks",
            help: "Writes raw samples to a file.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![params::any_type(), path_param()],
        },
        build_sink,
    );
}

fn open_err(path: &std::path::Path, e: std::io::Error) -> Error {
    Error::Other(format!("cannot open {}: {e}", path.display()))
}

fn no_file() -> Error {
    Error::Other("the file isn't open (start() not called)".into())
}

pub struct FileSource {
    path: PathBuf,
    /// Opened in `start`, so building the block has no side effects.
    file: Option<BufReader<File>>,
    sample_rate: f64,
    center_freq: f64,
    item: usize,
    repeat: bool,
    /// Bytes of a partially read item carried over to the next call.
    carry: Vec<u8>,
}

impl Block for FileSource {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut() {
            o.sample_rate = (self.sample_rate > 0.0).then_some(self.sample_rate);
            o.center_freq = (self.center_freq != 0.0).then_some(self.center_freq);
        }
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let file = File::open(&self.path).map_err(|e| open_err(&self.path, e))?;
        self.file = Some(BufReader::with_capacity(1 << 16, file));
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let file = self.file.as_mut().ok_or_else(no_file)?;
        let out = io.outputs[0].bytes_mut();
        if out.len() < self.item {
            return Ok(WorkStatus::Blocked);
        }
        let c = self.carry.len();
        out[..c].copy_from_slice(&self.carry);
        self.carry.clear();
        let got = c + file.read(&mut out[c..])?;
        if got == c {
            // EOF
            if self.repeat {
                file.seek(SeekFrom::Start(0))?;
                return Ok(WorkStatus::Ok);
            }
            return Ok(WorkStatus::Done);
        }
        let whole = got / self.item;
        self.carry.extend_from_slice(&out[whole * self.item..got]);
        io.produce(0, whole);
        Ok(WorkStatus::Ok)
    }
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let path = PathBuf::from(p.str("path")?);
    if path.as_os_str().is_empty() {
        return Err(no_path());
    }
    Ok(Instance {
        block: Box::new(FileSource {
            path,
            file: None,
            sample_rate: p.f64("sample_rate")?,
            center_freq: p.f64("center_freq")?,
            item: ty.item_size(),
            repeat: p.bool("repeat")?,
            carry: Vec::new(),
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

fn no_path() -> Error {
    Error::Other("no file chosen".into())
}

pub struct FileSink {
    path: PathBuf,
    /// Created in `start`, so building the block has no side effects.
    file: Option<BufWriter<File>>,
    item: usize,
}

impl Block for FileSink {
    fn start(&mut self) -> Result<(), Error> {
        let file = File::create(&self.path)
            .map_err(|e| Error::Other(format!("cannot create {}: {e}", self.path.display())))?;
        self.file = Some(BufWriter::with_capacity(1 << 16, file));
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let file = self.file.as_mut().ok_or_else(no_file)?;
        let inp = io.inputs[0].bytes();
        let n = inp.len() / self.item;
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        file.write_all(&inp[..n * self.item])?;
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        if let Some(f) = &mut self.file {
            f.flush()?;
        }
        Ok(())
    }
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let path = PathBuf::from(p.str("path")?);
    if path.as_os_str().is_empty() {
        return Err(no_path());
    }
    Ok(Instance {
        block: Box::new(FileSink {
            path,
            file: None,
            item: ty.item_size(),
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}
