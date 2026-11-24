//! What to do with each file: pictures to [`Pictures`], bulletins to
//! messages, everything worth keeping to the output folder.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::emwin::{self, Product};
use super::header::{Headers, noaa_compression};
use super::picture::{Event, Part, Pictures};
use super::products::{self, Class, clean, stem};
use super::transport::File;
use crate::digital::text::Escaped;

/// Settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where files are saved (`None`: nothing is saved).
    pub dir: Option<PathBuf>,
    /// Also keep every file as received (`lrit/…`).
    pub save_lrit: bool,
    pub false_colour: bool,
    /// Text in a message is cut after this many bytes.
    pub max_text: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dir: None,
            save_lrit: false,
            false_colour: true,
            max_text: 2000,
        }
    }
}

/// Counters for the status messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub files: u64,
    pub segments: u64,
    pub pictures: u64,
    pub emwin: u64,
    pub saved: u64,
    pub skipped: u64,
}

pub struct Decoder {
    pub cfg: Config,
    pub pictures: Pictures,
    /// Message lines waiting to go out (no newline).
    pub lines: VecDeque<String>,
    pub counts: Counts,
}

impl Decoder {
    pub fn new(cfg: Config) -> Self {
        let mut pictures = Pictures::new();
        pictures.false_colour = cfg.false_colour;
        Self {
            cfg,
            pictures,
            lines: VecDeque::new(),
            counts: Counts::default(),
        }
    }

    /// Handle one file.
    pub fn file(&mut self, f: File) {
        self.counts.files += 1;
        let name = stem(&f.headers).map_or_else(
            || format!("vc{}-apid{}-{}", f.vcid, f.apid, self.counts.files),
            clean,
        );
        if self.cfg.save_lrit {
            let sat = clean(&products::satellite(&f));
            self.save(&format!("lrit/{sat}/{name}.lrit"), &f.bytes());
        }
        if let Some(k) = f.headers.key.filter(|&k| k != 0) {
            self.counts.skipped += 1;
            self.lines.push_back(format!(
                "type=file event=encrypted name={name} vcid={} key={k}",
                f.vcid
            ));
            if !self.cfg.save_lrit {
                let sat = clean(&products::satellite(&f));
                self.save(&format!("encrypted/{sat}/{name}.lrit"), &f.bytes());
            }
            return;
        }
        match products::classify(&f) {
            Class::Image => self.image(&f, &name),
            Class::Emwin => self.emwin(&f, &name),
            Class::Text => self.text(&f, &name),
            Class::Dcs => {
                if self.complete(&f, &name) {
                    let path = format!("dcs/{}/{name}.dcs", date_of(&f.headers));
                    self.save(&path, &f.data);
                    self.lines.push_back(format!(
                        "type=dcs name={name} bytes={} file={path}",
                        f.data.len()
                    ));
                }
            }
            Class::Other => self.other(&f, &name),
        }
        self.take_events();
    }

    /// The end of the run.
    pub fn finish(&mut self) {
        self.pictures.close_all();
        self.take_events();
    }

    fn take_events(&mut self) {
        for e in self.pictures.events.drain(..) {
            match e {
                Event::Done { name, complete } => {
                    self.counts.pictures += 1;
                    self.lines.push_back(format!(
                        "type=picture name={name} complete={}",
                        u8::from(complete)
                    ));
                }
            }
        }
    }

    /// Say so (and return false) if a file didn't arrive whole.
    fn complete(&mut self, f: &File, name: &str) -> bool {
        if !f.complete {
            self.counts.skipped += 1;
            self.lines.push_back(format!(
                "type=file event=incomplete name={name} vcid={}",
                f.vcid
            ));
        }
        f.complete
    }

    fn save(&mut self, rel: &str, data: &[u8]) {
        let Some(dir) = &self.cfg.dir else {
            return;
        };
        let path = dir.join(rel);
        match write(&path, data) {
            Ok(()) => self.counts.saved += 1,
            Err(e) => self.lines.push_back(format!(
                "type=file event=error file={rel} error=\"{}\"",
                Escaped(e.as_bytes())
            )),
        }
    }

    fn image(&mut self, f: &File, name: &str) {
        let h = &f.headers;
        let Some(img) = h.image else { return };
        let info = products::image_info(f);
        let noaa = h.noaa.map(|n| n.compression);
        let decoded = if f.rice_decoded || img.compression == 0 {
            raw_pixels(&f.data, img.bits_per_pixel, img.columns, img.lines)
                .map(|px| (img.columns, img.lines, 1, px))
        } else if img.compression == 2 || noaa == Some(noaa_compression::JPEG) {
            jpeg(&f.data)
        } else {
            None
        };
        let Some((w, l, channels, mut px)) = decoded else {
            self.counts.skipped += 1;
            let why = if img.compression == 1 {
                "lossless (wavelet / JPEG 2000)"
            } else {
                "unknown"
            };
            self.lines.push_back(format!(
                "type=file event=unsupported name={name} compression={} what=\"{why} compression\"",
                img.compression
            ));
            if !self.cfg.save_lrit {
                let sat = clean(&info.satellite);
                self.save(&format!("unsupported/{sat}/{name}.lrit"), &f.bytes());
            }
            return;
        };
        // GOES fills space around the disk with white: make it black.
        if info.satellite.starts_with('G') && info.region == "FD" && channels == 1 {
            fill_sides(&mut px, usize::from(w));
        }
        let seg = h.segment;
        let (segment, segments, image_id) = seg.map_or((0, 1, 0), |s| {
            (s.number, s.segments.max(1), u32::from(s.image_id))
        });
        let start_line = match seg {
            Some(s) if s.start_line != u16::MAX => s.start_line,
            Some(s) => s.number.saturating_mul(l),
            None => 0,
        };
        let (pw, ph) = match seg {
            Some(s) if s.columns > 0 => (s.columns, s.lines),
            _ => (w, segments.saturating_mul(l)),
        };
        self.counts.segments += 1;
        let mut m = format!(
            "type=image name={} segment={}/{segments} width={w} lines={l}",
            info.name(),
            segment + 1
        );
        if f.lines_lost > 0 {
            let _ = write!(m, " lost_lines={}", f.lines_lost);
        }
        self.lines.push_back(m);
        self.pictures.add(Part {
            info: &info,
            image_id,
            width: w,
            lines: l,
            start_line,
            picture_width: pw,
            picture_height: ph,
            segment,
            segments,
            channels,
            pixels: &px,
        });
    }

    fn emwin(&mut self, f: &File, name: &str) {
        if !self.complete(f, name) {
            return;
        }
        self.counts.emwin += 1;
        let annotation = f
            .headers
            .annotation
            .clone()
            .unwrap_or_else(|| name.to_owned());
        let zipped = f.headers.noaa.map(|n| n.compression) == Some(noaa_compression::ZIP)
            || f.data.starts_with(b"PK\x03\x04");
        let files = if zipped {
            emwin::unzip(&f.data)
        } else {
            vec![(annotation.clone(), f.data.clone())]
        };
        if files.is_empty() {
            self.lines
                .push_back(format!("type=emwin event=bad_zip name={name}"));
            return;
        }
        let date = date_of(&f.headers);
        for (inner, data) in files {
            // Keep the EMWIN name (it says what the product is) with the
            // extension of what is inside.
            let ext = inner.rsplit_once('.').map_or("bin", |(_, e)| e);
            let base = annotation
                .rsplit_once('.')
                .map_or(annotation.as_str(), |(b, _)| b);
            let file = format!("{}.{}", clean(base), clean(&ext.to_ascii_uppercase()));
            let path = format!("emwin/{date}/{file}");
            self.save(&path, &data);
            let mut p = Product::from_name(&annotation);
            let text = ext.eq_ignore_ascii_case("txt");
            if text {
                p.from_text(&String::from_utf8_lossy(&data));
                self.lines
                    .push_back(p.message(&path, &data, self.cfg.max_text));
            } else {
                let mut m = String::from("type=emwin_file");
                if let Some(a) = &p.awips {
                    let _ = write!(m, " product={a}");
                }
                let _ = write!(m, " bytes={} file={path}", data.len());
                self.lines.push_back(m);
            }
        }
    }

    fn text(&mut self, f: &File, name: &str) {
        if !self.complete(f, name) {
            return;
        }
        let ext = if name.contains('.') { "" } else { ".txt" };
        let path = format!("text/{}/{name}{ext}", date_of(&f.headers));
        self.save(&path, &f.data);
        let cut = f.data.len().min(self.cfg.max_text);
        self.lines.push_back(format!(
            "type=text name={name} file={path} text=\"{}\"",
            Escaped(trim_ascii(&f.data[..cut]))
        ));
    }

    fn other(&mut self, f: &File, name: &str) {
        if !self.complete(f, name) {
            return;
        }
        let comp = f.headers.noaa.map(|n| n.compression);
        let date = date_of(&f.headers);
        let sat = clean(&products::satellite(f));
        let mut files = Vec::new();
        if comp == Some(noaa_compression::ZIP) {
            files = emwin::unzip(&f.data)
                .into_iter()
                .map(|(n, d)| (clean(n.rsplit('/').next().unwrap_or(&n)), d))
                .collect();
        }
        if files.is_empty() {
            let ext = match comp {
                Some(noaa_compression::GIF) => "gif",
                Some(noaa_compression::JPEG) => "jpg",
                _ if f.data.starts_with(b"GIF8") => "gif",
                _ if f.data.starts_with(&[0xFF, 0xD8]) => "jpg",
                _ if f.data.starts_with(b"\x89PNG") => "png",
                _ => "bin",
            };
            let file = if name.contains('.') {
                name.to_owned()
            } else {
                format!("{name}.{ext}")
            };
            files.push((file, f.data.clone()));
        }
        for (file, data) in files {
            let path = format!("files/{sat}/{date}/{file}");
            self.save(&path, &data);
            self.lines.push_back(format!(
                "type=file name={name} bytes={} file={path}",
                data.len()
            ));
        }
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let s = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let e = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(s, |i| i + 1);
    &b[s..e.max(s)]
}

/// `YYYY-MM-DD` of a file's time stamp, or `undated`.
fn date_of(h: &Headers) -> String {
    h.time.map_or_else(
        || "undated".into(),
        |t| {
            let (y, m, d, ..) = products::civil(t.unix());
            format!("{y:04}-{m:02}-{d:02}")
        },
    )
}

fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

/// 8-bit grey pixels from an uncompressed image: 8 bits as they are, 1 bit
/// as black and white, 9–16 bits (big-endian) cut to their top 8.
pub fn raw_pixels(data: &[u8], bpp: u8, cols: u16, lines: u16) -> Option<Vec<u8>> {
    let (w, l) = (usize::from(cols), usize::from(lines));
    match bpp {
        8 => {
            let mut v = data.get(..w * l).unwrap_or(data).to_vec();
            v.resize(w * l, 0);
            Some(v)
        }
        1 => {
            let stride = w.div_ceil(8);
            Some(
                (0..w * l)
                    .map(|i| {
                        let (y, x) = (i / w, i % w);
                        let byte = data.get(y * stride + x / 8).copied().unwrap_or(0);
                        if byte >> (7 - x % 8) & 1 != 0 { 255 } else { 0 }
                    })
                    .collect(),
            )
        }
        9..=16 => Some(
            (0..w * l)
                .map(|i| {
                    let v = data
                        .get(2 * i..2 * i + 2)
                        .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
                    (v >> (bpp - 8)).min(255) as u8
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Decode a JPEG to (width, lines, channels, pixels).
pub fn jpeg(data: &[u8]) -> Option<(u16, u16, u8, Vec<u8>)> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let mut d = JpegDecoder::new(ZCursor::new(data));
    d.decode_headers().ok()?;
    let grey = d.info()?.components == 1;
    let space = if grey {
        ColorSpace::Luma
    } else {
        ColorSpace::RGB
    };
    let mut d = JpegDecoder::new_with_options(
        ZCursor::new(data),
        DecoderOptions::default().jpeg_set_out_colorspace(space),
    );
    let px = d.decode().ok()?;
    let info = d.info()?;
    let ch = if grey { 1 } else { 3 };
    (px.len() == usize::from(info.width) * usize::from(info.height) * usize::from(ch)).then_some((
        info.width,
        info.height,
        ch,
        px,
    ))
}

/// GOES full disks have white (255) outside the disk: make it black, from
/// each row's ends inwards.
fn fill_sides(px: &mut [u8], w: usize) {
    if w == 0 {
        return;
    }
    for row in px.chunks_mut(w) {
        for p in row.iter_mut() {
            if *p != 255 {
                break;
            }
            *p = 0;
        }
        for p in row.iter_mut().rev() {
            if *p != 255 {
                break;
            }
            *p = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels() {
        assert_eq!(raw_pixels(&[0b1010_0000], 1, 3, 1).unwrap(), [255, 0, 255]);
        assert_eq!(
            raw_pixels(&[0x03, 0xFF, 0x00, 0x04], 10, 2, 1).unwrap(),
            [255, 1]
        );
        let mut row = vec![255, 255, 7, 255, 9, 255];
        fill_sides(&mut row, 6);
        assert_eq!(row, [0, 0, 7, 255, 9, 0]);
    }
}
