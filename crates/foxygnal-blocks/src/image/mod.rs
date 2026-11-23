//! Pictures: decoders that draw (SSTV, …) and the sink that saves them.
//!
//! ## Decoded pictures are row records on a `u8` stream
//!
//! A decoder that produces a picture writes it one row at a time as
//! [`foxygnal_core::raster`] records on a `u8` output. Such a wire can be
//! watched with the *Image* instrument (the `image` probe), saved as PNG files
//! with [`ImageSink`] (`image_sink`), or kept raw with a `file_sink`. As with
//! decoded text, nothing is decoder-specific.

mod sstv;

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use foxygnal_core::raster::{self, Next, Row};
use foxygnal_core::{
    Block, BlockDescriptor, Error, Instance, ParamKind, ParamReader, ParamSpec, PortSpec, Registry,
    StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

pub use sstv::{SSTV_MODES, SstvMode};

pub fn register(r: &mut Registry) {
    sstv::register(r);
    r.register(
        BlockDescriptor {
            kind: "image_sink",
            label: "Image Sink",
            category: "Sinks",
            help: "Saves the pictures on a picture wire (the `image` output of an SSTV \
                   decoder, …) as PNG files, one per picture, named \
                   `<prefix>-<UTC date and time>-<number>.png`. A picture is saved when its \
                   last row arrives, when the next picture starts, or when the run ends; \
                   rows that never arrived are black.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::U8),
            }],
            outputs: vec![],
            params: vec![
                ParamSpec {
                    name: "dir",
                    label: "Folder",
                    kind: ParamKind::Path,
                    default: json!(""),
                    help: "The folder to save pictures in (created if missing).",
                    live: false,
                },
                ParamSpec {
                    name: "prefix",
                    label: "Name prefix",
                    kind: ParamKind::String,
                    default: json!("picture"),
                    help: "The start of each file name.",
                    live: false,
                },
                ParamSpec {
                    name: "min_rows",
                    label: "Minimum rows",
                    kind: ParamKind::Int {
                        min: Some(1),
                        max: None,
                    },
                    default: json!(16),
                    help: "Pictures cut off with fewer rows than this are not saved.",
                    live: false,
                },
            ],
        },
        build_sink,
    );
}

/// Queue one row record (header and pixels) on a decoder's picture output.
pub(crate) fn push_row(q: &mut crate::digital::MsgQueue, row: Row, pixels: &[u8]) {
    debug_assert_eq!(pixels.len(), row.pixel_bytes());
    q.bytes(&row.header());
    q.bytes(pixels);
}

/// The picture being collected by an [`ImageSink`].
struct Picture {
    number: u32,
    width: u16,
    channels: u8,
    /// Rows promised by the header (0 = not known).
    height: u16,
    pixels: Vec<u8>,
    /// Rows received, and the highest row index seen + 1.
    rows: usize,
    extent: usize,
}

pub struct ImageSink {
    dir: PathBuf,
    prefix: String,
    min_rows: usize,
    /// Bytes not parsed yet (the start of a record that hasn't fully arrived).
    buf: Vec<u8>,
    picture: Option<Picture>,
    saved: u32,
}

impl ImageSink {
    fn take_row(&mut self, row: Row, pixels: &[u8]) -> Result<(), Error> {
        let same = self.picture.as_ref().is_some_and(|p| {
            p.number == row.picture && p.width == row.width && p.channels == row.channels
        });
        if !same {
            self.save()?;
            let height = usize::from(row.height.max(row.row + 1));
            self.picture = Some(Picture {
                number: row.picture,
                width: row.width,
                channels: row.channels,
                height: row.height,
                pixels: vec![0; height * row.pixel_bytes()],
                rows: 0,
                extent: 0,
            });
        }
        let p = self.picture.as_mut().expect("just set");
        let stride = row.pixel_bytes();
        let at = usize::from(row.row) * stride;
        if p.pixels.len() < at + stride {
            p.pixels.resize(at + stride, 0);
        }
        p.pixels[at..at + stride].copy_from_slice(pixels);
        p.rows += 1;
        p.extent = p.extent.max(usize::from(row.row) + 1);
        if row.is_last() {
            self.save()?;
        }
        Ok(())
    }

    /// Write the current picture, if it has enough rows.
    fn save(&mut self) -> Result<(), Error> {
        let Some(p) = self.picture.take() else {
            return Ok(());
        };
        if p.rows < self.min_rows {
            return Ok(());
        }
        let height = if p.height > 0 {
            usize::from(p.height)
        } else {
            p.extent
        };
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| Error::Other(format!("cannot create {}: {e}", self.dir.display())))?;
        self.saved += 1;
        let name = format!(
            "{}-{}-{}.png",
            self.prefix,
            utc_stamp(SystemTime::now()),
            self.saved
        );
        let path = self.dir.join(name);
        let stride = usize::from(p.width) * usize::from(p.channels);
        write_png(
            &path,
            u32::from(p.width),
            height as u32,
            p.channels,
            &p.pixels[..height * stride],
        )
    }
}

/// Write 8-bit grey (1 channel) or RGB (3) pixels as a PNG file.
pub fn write_png(
    path: &Path,
    width: u32,
    height: u32,
    channels: u8,
    pixels: &[u8],
) -> Result<(), Error> {
    let err = |e: &dyn std::fmt::Display| Error::Other(format!("{}: {e}", path.display()));
    let file = File::create(path).map_err(|e| err(&e))?;
    let mut enc = png::Encoder::new(BufWriter::new(file), width, height);
    enc.set_color(if channels == 3 {
        png::ColorType::Rgb
    } else {
        png::ColorType::Grayscale
    });
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| err(&e))?;
    w.write_image_data(pixels).map_err(|e| err(&e))?;
    w.finish().map_err(|e| err(&e))
}

/// `YYYYMMDD-HHMMSS` in UTC.
fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian
/// (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

impl Block for ImageSink {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<u8>();
        if inp.is_empty() {
            if io.inputs[0].exhausted() {
                self.save()?;
            }
            return Ok(WorkStatus::Blocked);
        }
        // Pictures are a few kB/s: copying into one buffer keeps records that
        // straddle two calls simple. The buffer only grows to one record plus
        // one input slice.
        let n = inp.len();
        let mut buf = std::mem::take(&mut self.buf);
        buf.extend_from_slice(inp);
        let mut at = 0;
        let result = loop {
            match raster::next(&buf[at..]) {
                Next::Row(row, len) => {
                    if let Err(e) = self.take_row(row, &buf[at + raster::HEADER..at + len]) {
                        break Err(e);
                    }
                    at += len;
                }
                Next::Skip(k) => at += k,
                Next::More => break Ok(()),
            }
        };
        buf.drain(..at);
        self.buf = buf;
        result?;
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.save()
    }

    fn dedicated_thread(&self) -> bool {
        // Writes files.
        true
    }
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let dir = PathBuf::from(p.str("dir")?);
    if dir.as_os_str().is_empty() {
        return Err(Error::Other("no folder chosen".into()));
    }
    let prefix = p.str("prefix")?.trim();
    if prefix.is_empty() || prefix.contains(['/', '\\']) {
        return Err(Error::Other(
            "the name prefix must be non-empty and not contain a path separator".into(),
        ));
    }
    Ok(Instance {
        block: Box::new(ImageSink {
            dir,
            prefix: prefix.to_owned(),
            min_rows: p.u64("min_rows")?.max(1) as usize,
            buf: Vec::with_capacity(1 << 16),
            picture: None,
            saved: 0,
        }),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamps() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_795_000_000);
        // 2026-11-18T11:06:40Z
        assert_eq!(utc_stamp(t), "20261118-110640");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
