//! SigMF recordings: `name.sigmf-meta` (JSON) next to `name.sigmf-data` (raw
//! samples). See <https://github.com/sigmf/SigMF>.

use std::ffi::OsString;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::{Value, json};

use crate::params;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "sigmf_source",
            label: "SigMF Source",
            category: "Sources",
            help: "Reads a SigMF recording. Sample rate and centre frequency come from the \
                   metadata. c32 converts any complex format (cf32, ci16, ci8, cu8, …) to \
                   ±1 floats, f32 any real one. i16 and u8 pass ci16 / cu8 (or ri16 / ru8) \
                   samples through unconverted; complex ones arrive as interleaved I/Q pairs.",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::any_type(),
                ParamSpec {
                    name: "path",
                    label: "File",
                    kind: ParamKind::Path,
                    default: json!(""),
                    help: "The .sigmf-meta or .sigmf-data file (the other one must sit next \
                           to it).",
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
            kind: "sigmf_sink",
            label: "SigMF Sink",
            category: "Sinks",
            help: "Records to name.sigmf-data plus name.sigmf-meta, with the sample rate and \
                   centre frequency from stream metadata.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![
                params::any_type(),
                ParamSpec {
                    name: "path",
                    label: "File",
                    kind: ParamKind::Path,
                    default: json!(""),
                    help: "Recording name; .sigmf-data and .sigmf-meta are added (replaced if \
                           they exist).",
                    live: false,
                },
                ParamSpec {
                    name: "complex",
                    label: "Interleaved I/Q",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "For i16 / u8: the stream holds interleaved I/Q pairs (recorded as \
                           ci16 / cu8, at half the stream's item rate).",
                    live: false,
                },
            ],
        },
        build_sink,
    );
}

/// The two file names of a recording, given either of them or the base name.
pub fn file_pair(path: &Path) -> (PathBuf, PathBuf) {
    let s = path.as_os_str().to_string_lossy();
    let base = [".sigmf-meta", ".sigmf-data", ".sigmf"]
        .iter()
        .find_map(|ext| s.strip_suffix(ext))
        .map(OsString::from)
        .unwrap_or_else(|| path.as_os_str().to_owned());
    let with = |ext: &str| {
        let mut p = base.clone();
        p.push(ext);
        PathBuf::from(p)
    };
    (with(".sigmf-meta"), with(".sigmf-data"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comp {
    F64,
    F32,
    I32,
    I16,
    I8,
    U32,
    U16,
    U8,
}

impl Comp {
    fn size(self) -> usize {
        match self {
            Self::F64 => 8,
            Self::F32 | Self::I32 | Self::U32 => 4,
            Self::I16 | Self::U16 => 2,
            Self::I8 | Self::U8 => 1,
        }
    }
}

/// A SigMF `core:datatype`, e.g. `cf32_le`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataType {
    complex: bool,
    comp: Comp,
    big_endian: bool,
}

impl DataType {
    pub fn parse(s: &str) -> Option<Self> {
        let (complex, rest) = match s.as_bytes().first()? {
            b'c' => (true, &s[1..]),
            b'r' => (false, &s[1..]),
            _ => return None,
        };
        let (ty, big_endian) = match rest.split_once('_') {
            Some((t, "le")) => (t, false),
            Some((t, "be")) => (t, true),
            Some(_) => return None,
            None => (rest, false),
        };
        let comp = match ty {
            "f64" => Comp::F64,
            "f32" => Comp::F32,
            "i32" => Comp::I32,
            "i16" => Comp::I16,
            "i8" => Comp::I8,
            "u32" => Comp::U32,
            "u16" => Comp::U16,
            "u8" => Comp::U8,
            _ => return None,
        };
        // Multi-byte types need an endianness; single bytes must not have one.
        if (comp.size() > 1) != rest.contains('_') {
            return None;
        }
        Some(Self {
            complex,
            comp,
            big_endian,
        })
    }

    /// Bytes per sample (both components for complex).
    fn sample_size(self) -> usize {
        self.comp.size() * if self.complex { 2 } else { 1 }
    }

    /// The decoder for one component, normalized to ±1 for integers.
    fn decoder(self) -> fn(&[u8]) -> f32 {
        macro_rules! dec {
            ($t:ty, $be:expr, $f:expr) => {
                if $be {
                    |b: &[u8]| {
                        let v = <$t>::from_be_bytes(b[..size_of::<$t>()].try_into().unwrap());
                        ($f)(v)
                    }
                } else {
                    |b: &[u8]| {
                        let v = <$t>::from_le_bytes(b[..size_of::<$t>()].try_into().unwrap());
                        ($f)(v)
                    }
                }
            };
        }
        let be = self.big_endian;
        match self.comp {
            Comp::F64 => dec!(f64, be, |v: f64| v as f32),
            Comp::F32 => dec!(f32, be, |v: f32| v),
            Comp::I32 => dec!(i32, be, |v: i32| v as f32 / 2_147_483_648.0),
            Comp::I16 => dec!(i16, be, |v: i16| v as f32 / 32_768.0),
            Comp::I8 => |b: &[u8]| b[0] as i8 as f32 / 128.0,
            Comp::U32 => dec!(u32, be, |v: u32| (v as f64 / 2_147_483_647.5 - 1.0) as f32),
            Comp::U16 => dec!(u16, be, |v: u16| (v as f32 - 32_767.5) / 32_767.5),
            Comp::U8 => |b: &[u8]| (b[0] as f32 - 127.5) / 127.5,
        }
    }

    /// The SigMF name of the type a stream of `ty` is recorded as.
    fn name_for(ty: StreamType, complex: bool) -> String {
        let endian = if cfg!(target_endian = "big") {
            "_be"
        } else {
            "_le"
        };
        match (ty, complex) {
            (StreamType::C32, _) => format!("cf32{endian}"),
            (StreamType::F32, _) => format!("rf32{endian}"),
            (StreamType::I16, true) => format!("ci16{endian}"),
            (StreamType::I16, false) => format!("ri16{endian}"),
            (StreamType::U8, true) => "cu8".into(),
            (StreamType::U8, false) => "ru8".into(),
        }
    }
}

/// What a source needs from `name.sigmf-meta`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meta {
    pub datatype: DataType,
    pub sample_rate: Option<f64>,
    pub frequency: Option<f64>,
}

pub fn parse_meta(text: &str) -> Result<Meta, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let global = v.get("global").ok_or("no `global` object")?;
    let dt = global
        .get("core:datatype")
        .and_then(Value::as_str)
        .ok_or("no `core:datatype`")?;
    let datatype = DataType::parse(dt).ok_or_else(|| format!("unknown datatype `{dt}`"))?;
    match global.get("core:num_channels").and_then(Value::as_u64) {
        Some(n) if n != 1 => {
            return Err(format!(
                "has {n} channels; only single-channel recordings are supported"
            ));
        }
        _ => {}
    }
    Ok(Meta {
        datatype,
        sample_rate: global.get("core:sample_rate").and_then(Value::as_f64),
        frequency: v
            .get("captures")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("core:frequency"))
            .and_then(Value::as_f64),
    })
}

fn read_meta(path: &Path) -> Result<Meta, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Other(format!("cannot open {}: {e}", path.display())))?;
    parse_meta(&text).map_err(|e| Error::Other(format!("{}: {e}", path.display())))
}

/// How samples are turned into output items.
#[derive(Clone, Copy)]
enum Conv {
    /// Complex samples to c32.
    Complex(fn(&[u8]) -> f32),
    /// Real samples to f32.
    Real(fn(&[u8]) -> f32),
    /// i16 components passed through, one item each (complex samples become
    /// interleaved pairs).
    I16 { big_endian: bool },
    /// u8 components passed through.
    U8,
}

pub struct SigmfSource {
    meta_path: PathBuf,
    data_path: PathBuf,
    ty: StreamType,
    repeat: bool,
    /// From `configure`: the recording's datatype, how to convert it, and
    /// output items per sample.
    plan: Option<(DataType, Conv, usize)>,
    file: Option<File>,
    /// Raw bytes read but not yet converted. Allocated in `start`.
    scratch: Vec<u8>,
    fill: usize,
}

impl SigmfSource {
    /// Check that the recording can be read as `self.ty`. Returns the
    /// conversion and how many output items one sample makes.
    fn plan(&self, dt: DataType) -> Result<(Conv, usize), Error> {
        let kind = if dt.complex { "complex" } else { "real" };
        let per = if dt.complex { 2 } else { 1 };
        let bad = |hint: &str| {
            Err(Error::Other(format!(
                "{} holds {kind} {:?} samples; {hint}",
                self.data_path.display(),
                dt.comp
            )))
        };
        match self.ty {
            StreamType::C32 if dt.complex => Ok((Conv::Complex(dt.decoder()), 1)),
            StreamType::C32 => bad("choose f32 for a real recording"),
            StreamType::F32 if !dt.complex => Ok((Conv::Real(dt.decoder()), 1)),
            StreamType::F32 => bad("choose c32 for a complex recording"),
            StreamType::I16 if dt.comp == Comp::I16 => Ok((
                Conv::I16 {
                    big_endian: dt.big_endian,
                },
                per,
            )),
            StreamType::U8 if dt.comp == Comp::U8 => Ok((Conv::U8, per)),
            _ => bad(if dt.complex {
                "choose c32 (or i16 / u8 for ci16 / cu8 as they are)"
            } else {
                "choose f32 (or i16 / u8 for ri16 / ru8 as they are)"
            }),
        }
    }
}

impl Block for SigmfSource {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        // Reads the (small) metadata file only; nothing is kept open.
        let meta = read_meta(&self.meta_path)?;
        let (conv, per) = self.plan(meta.datatype)?;
        for o in cx.outputs.iter_mut() {
            // Interleaved output counts items, two per complex sample.
            o.sample_rate = meta.sample_rate.map(|r| r * per as f64);
            o.center_freq = meta.frequency;
        }
        self.plan = Some((meta.datatype, conv, per));
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let file = File::open(&self.data_path)
            .map_err(|e| Error::Other(format!("cannot open {}: {e}", self.data_path.display())))?;
        self.file = Some(file);
        self.scratch = vec![0; 1 << 16];
        self.fill = 0;
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (Some((dt, conv, _)), Some(file)) = (self.plan, self.file.as_mut()) else {
            return Err(Error::Other(
                "the file isn't open (start() not called)".into(),
            ));
        };
        let cs = dt.comp.size();
        // Raw outputs go one component per item, so an interleaved pair may
        // straddle a buffer wrap.
        let in_size = match conv {
            Conv::I16 { .. } | Conv::U8 => cs,
            _ => dt.sample_size(),
        };
        let out = &mut io.outputs[0];
        let room = out.bytes_mut().len() / out.stream_type().item_size();
        if room == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let mut eof = false;
        if self.fill < self.scratch.len() {
            let got = file.read(&mut self.scratch[self.fill..])?;
            eof = got == 0;
            self.fill += got;
        }
        let n = (self.fill / in_size).min(room);
        let raw = &self.scratch[..n * in_size];
        match conv {
            Conv::Complex(dec) => {
                let o = out.slice_mut::<Complex32>();
                for (o, s) in o[..n].iter_mut().zip(raw.chunks_exact(in_size)) {
                    *o = Complex32::new(dec(s), dec(&s[cs..]));
                }
            }
            Conv::Real(dec) => {
                let o = out.slice_mut::<f32>();
                for (o, s) in o[..n].iter_mut().zip(raw.chunks_exact(in_size)) {
                    *o = dec(s);
                }
            }
            Conv::I16 { big_endian } => {
                let o = out.slice_mut::<i16>();
                for (o, b) in o[..n].iter_mut().zip(raw.chunks_exact(2)) {
                    let b = [b[0], b[1]];
                    *o = if big_endian {
                        i16::from_be_bytes(b)
                    } else {
                        i16::from_le_bytes(b)
                    };
                }
            }
            Conv::U8 => out.slice_mut::<u8>()[..n].copy_from_slice(raw),
        }
        self.scratch.copy_within(n * in_size..self.fill, 0);
        self.fill -= n * in_size;
        out.produce(n);
        if n == 0 && eof {
            if self.repeat && file.metadata()?.len() >= dt.sample_size() as u64 {
                file.seek(SeekFrom::Start(0))?;
                self.fill = 0;
                return Ok(WorkStatus::Ok);
            }
            return Ok(WorkStatus::Done);
        }
        Ok(WorkStatus::Ok)
    }
}

fn path_from(p: &ParamReader<'_>) -> Result<PathBuf, Error> {
    let path = PathBuf::from(p.str("path")?);
    if path.as_os_str().is_empty() {
        return Err(Error::Other("no file chosen".into()));
    }
    Ok(path)
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let (meta_path, data_path) = file_pair(&path_from(p)?);
    Ok(Instance {
        block: Box::new(SigmfSource {
            meta_path,
            data_path,
            ty,
            repeat: p.bool("repeat")?,
            plan: None,
            file: None,
            scratch: Vec::new(),
            fill: 0,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

pub struct SigmfSink {
    meta_path: PathBuf,
    data_path: PathBuf,
    ty: StreamType,
    complex: bool,
    /// From stream metadata, in `configure`.
    sample_rate: Option<f64>,
    frequency: Option<f64>,
    file: Option<BufWriter<File>>,
}

impl SigmfSink {
    /// The metadata document for this recording.
    fn meta_json(&self, datetime: &str) -> Value {
        let complex = self.complex || self.ty == StreamType::C32;
        let mut global = json!({
            "core:datatype": DataType::name_for(self.ty, complex),
            "core:version": "1.2.0",
            "core:recorder": "foxygnal",
        });
        if let Some(r) = self.sample_rate {
            global["core:sample_rate"] = json!(r);
        }
        let mut capture = json!({
            "core:sample_start": 0,
            "core:datetime": datetime,
        });
        if let Some(f) = self.frequency {
            capture["core:frequency"] = json!(f);
        }
        json!({
            "global": global,
            "captures": [capture],
            "annotations": [],
        })
    }
}

impl Block for SigmfSink {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let interleaved = self.complex && self.ty != StreamType::C32 && self.ty != StreamType::F32;
        self.sample_rate = cx.inputs[0]
            .sample_rate
            .map(|r| if interleaved { r / 2.0 } else { r });
        self.frequency = cx.inputs[0].center_freq;
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let create = |p: &Path| {
            File::create(p).map_err(|e| Error::Other(format!("cannot create {}: {e}", p.display())))
        };
        let meta = serde_json::to_string_pretty(&self.meta_json(&iso8601_now()))
            .map_err(|e| Error::Other(e.to_string()))?;
        let mut mf = create(&self.meta_path)?;
        mf.write_all(meta.as_bytes())?;
        mf.write_all(b"\n")?;
        self.file = Some(BufWriter::with_capacity(1 << 16, create(&self.data_path)?));
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::Other("the file isn't open (start() not called)".into()))?;
        let item = io.inputs[0].stream_type().item_size();
        let inp = io.inputs[0].bytes();
        let n = inp.len() / item;
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        // Stream items are native-endian, which the datatype name records.
        file.write_all(&inp[..n * item])?;
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
    let (meta_path, data_path) = file_pair(&path_from(p)?);
    Ok(Instance {
        block: Box::new(SigmfSink {
            meta_path,
            data_path,
            ty,
            complex: p.bool("complex")?,
            sample_rate: None,
            frequency: None,
            file: None,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}

/// The current UTC time as ISO 8601 (`2026-11-22T13:45:00.123Z`).
fn iso8601_now() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso8601(d.as_secs() as i64, d.subsec_millis())
}

fn iso8601(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        tod / 60 % 60,
        tod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datatypes() {
        let cf = DataType::parse("cf32_le").unwrap();
        assert!(cf.complex && cf.comp == Comp::F32 && !cf.big_endian);
        assert_eq!(cf.sample_size(), 8);
        assert_eq!(DataType::parse("cu8").unwrap().sample_size(), 2);
        assert!(DataType::parse("ri16_be").unwrap().big_endian);
        for bad in ["cf32", "cu8_le", "xf32_le", "cf16_le", "", "c"] {
            assert!(DataType::parse(bad).is_none(), "{bad}");
        }
        let d = DataType::parse("ri16_be").unwrap().decoder();
        assert_eq!(d(&[0x40, 0x00]), 0.5);
        let d = DataType::parse("ci8").unwrap().decoder();
        assert_eq!(d(&[0x80]), -1.0);
    }

    #[test]
    fn file_names() {
        let want = (
            PathBuf::from("/x/rec.sigmf-meta"),
            PathBuf::from("/x/rec.sigmf-data"),
        );
        for p in [
            "/x/rec.sigmf-meta",
            "/x/rec.sigmf-data",
            "/x/rec.sigmf",
            "/x/rec",
        ] {
            assert_eq!(file_pair(Path::new(p)), want);
        }
    }

    #[test]
    fn meta() {
        let m = parse_meta(
            r#"{"global": {"core:datatype": "ci16_le", "core:sample_rate": 2048000,
                "core:version": "1.0.0"},
                "captures": [{"core:sample_start": 0, "core:frequency": 100.1e6}],
                "annotations": []}"#,
        )
        .unwrap();
        assert_eq!(m.sample_rate, Some(2_048_000.0));
        assert_eq!(m.frequency, Some(100.1e6));
        assert_eq!(m.datatype, DataType::parse("ci16_le").unwrap());
        assert!(parse_meta(r#"{"global": {}}"#).is_err());
    }

    #[test]
    fn dates() {
        assert_eq!(iso8601(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_795_000_000, 5), "2026-11-18T11:06:40.005Z");
        assert_eq!(iso8601(951_782_400, 0), "2000-02-29T00:00:00.000Z");
    }
}
