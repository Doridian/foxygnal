//! Raw sample files (interleaved, native little-endian, same layout as GNU Radio).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use foxygnal_core::{
    Block, BlockDescriptor, Error, Instance, ParamKind, ParamReader, ParamSpec, PortSpec, Registry,
    WorkIo, WorkStatus,
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
                },
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

pub struct FileSource {
    file: BufReader<File>,
    item: usize,
    repeat: bool,
    /// Bytes of a partially read item carried over to the next call.
    carry: Vec<u8>,
}

impl Block for FileSource {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let out = io.outputs[0].bytes_mut();
        if out.len() < self.item {
            return Ok(WorkStatus::Blocked);
        }
        let c = self.carry.len();
        out[..c].copy_from_slice(&self.carry);
        self.carry.clear();
        let got = c + self.file.read(&mut out[c..])?;
        if got == c {
            // EOF
            if self.repeat {
                self.file.seek(SeekFrom::Start(0))?;
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
    let file = File::open(&path)
        .map_err(|e| Error::Other(format!("cannot open {}: {e}", path.display())))?;
    Ok(Instance {
        block: Box::new(FileSource {
            file: BufReader::with_capacity(1 << 16, file),
            item: ty.item_size(),
            repeat: p.bool("repeat")?,
            carry: Vec::new(),
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct FileSink {
    file: BufWriter<File>,
    item: usize,
}

impl Block for FileSink {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].bytes();
        let n = inp.len() / self.item;
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        self.file.write_all(&inp[..n * self.item])?;
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.file.flush()?;
        Ok(())
    }
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let path = PathBuf::from(p.str("path")?);
    let file = File::create(&path)
        .map_err(|e| Error::Other(format!("cannot create {}: {e}", path.display())))?;
    Ok(Instance {
        block: Box::new(FileSink {
            file: BufWriter::with_capacity(1 << 16, file),
            item: ty.item_size(),
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}
