//! Pictures from image files: segments put together, then sent out as row
//! records ([`foxygnal_core::raster`]) one picture at a time.
//!
//! A GOES full disk comes in 16 segments (one file each), and other
//! pictures' segments arrive in between. Each picture is collected on its
//! own; the output sends one picture at a time, in the order they started:
//! the oldest one's rows as its segments arrive, the others' once it is done
//! (so a picture's rows are never mixed with another's, which `image_sink`
//! and the Image instrument need). A picture is done when all its segments
//! are in, or given up on when a newer scan of the same product starts, when
//! nothing for it came among the last [`STALE_PARTS`] segments, or at the end
//! of the run; rows that never came stay black.
//!
//! When a scene's visible (channel 2) and infrared (channel 13) pictures are
//! both complete, a false-colour picture is made from them (see
//! [`false_colour`]).

use std::collections::VecDeque;

use foxygnal_core::raster::{LAST, Row};

use super::products::ImageInfo;
use crate::digital::MsgQueue;

/// A picture not added to among this many segments of others is given up on.
pub const STALE_PARTS: u64 = 200;
/// At most this many pictures are kept open; past that the oldest is given
/// up on.
pub const MAX_OPEN: usize = 6;

/// One segment (or a whole picture), 8 bits per channel.
pub struct Part<'a> {
    pub info: &'a ImageInfo,
    /// Tells segments of different pictures with the same name apart (the
    /// segment record's image id).
    pub image_id: u32,
    pub width: u16,
    pub lines: u16,
    /// The first line in the whole picture.
    pub start_line: u16,
    /// The whole picture's size.
    pub picture_width: u16,
    pub picture_height: u16,
    pub segment: u16,
    pub segments: u16,
    /// 1 (grey) or 3 (RGB).
    pub channels: u8,
    pub pixels: &'a [u8],
}

struct Mosaic {
    number: u32,
    info: ImageInfo,
    name: String,
    image_id: u32,
    width: u16,
    height: u16,
    channels: u8,
    pixels: Vec<u8>,
    have: Vec<bool>,
    /// Row ranges received and not sent yet.
    pending: VecDeque<(u16, u16)>,
    name_sent: bool,
    /// No more rows will come: finish once `pending` is sent.
    closing: bool,
    complete: bool,
    last_part: u64,
    last_sent: Option<u16>,
    last_flag_sent: bool,
}

impl Mosaic {
    fn stride(&self) -> usize {
        usize::from(self.width) * usize::from(self.channels)
    }
}

/// What happened, for the decoder's text output.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A picture is complete (`true`) or was given up on with rows missing.
    Done { name: String, complete: bool },
}

/// Satellite, region and scan time: what the two channels of a false-colour picture share.
type Scene = (String, String, Option<i64>);

#[derive(Default)]
struct Pair {
    vis: Option<(u16, u16, Vec<u8>, ImageInfo)>,
    ir: Option<(u16, u16, Vec<u8>)>,
}

pub struct Pictures {
    queue: VecDeque<Mosaic>,
    next_number: u32,
    parts: u64,
    pub false_colour: bool,
    pairs: VecDeque<(Scene, Pair)>,
    pub events: Vec<Event>,
}

impl Default for Pictures {
    fn default() -> Self {
        Self::new()
    }
}

impl Pictures {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            next_number: 1,
            parts: 0,
            false_colour: true,
            pairs: VecDeque::new(),
            events: Vec::new(),
        }
    }

    /// Pictures open or waiting to be sent.
    pub fn open(&self) -> usize {
        self.queue.len()
    }

    /// Add a segment.
    pub fn add(&mut self, p: Part<'_>) {
        self.parts += 1;
        let name = p.info.name();
        let channels = if p.channels == 3 { 3 } else { 1 };
        let at = self.queue.iter().position(|m| {
            !m.closing && m.name == name && m.image_id == p.image_id && m.channels == channels
        });
        let idx = match at {
            Some(i) => i,
            None => {
                let width = p.picture_width.max(p.width).max(1);
                let height = p
                    .picture_height
                    .max(p.start_line.saturating_add(p.lines))
                    .max(1);
                let segments = usize::from(p.segments.max(1));
                self.queue.push_back(Mosaic {
                    number: self.next_number,
                    info: p.info.clone(),
                    name: name.clone(),
                    image_id: p.image_id,
                    width,
                    height,
                    channels,
                    pixels: vec![
                        0;
                        usize::from(width) * usize::from(height) * usize::from(channels)
                    ],
                    have: vec![false; segments],
                    pending: VecDeque::new(),
                    name_sent: false,
                    closing: false,
                    complete: false,
                    last_part: self.parts,
                    last_sent: None,
                    last_flag_sent: false,
                });
                self.next_number = self.next_number.wrapping_add(1).max(1);
                self.queue.len() - 1
            }
        };
        let m = &mut self.queue[idx];
        m.last_part = self.parts;
        let stride = m.stride();
        let pw = usize::from(p.width) * usize::from(channels);
        let first = usize::from(p.start_line);
        let rows = usize::from(p.lines)
            .min(p.pixels.len() / pw.max(1))
            .min(usize::from(m.height).saturating_sub(first));
        for r in 0..rows {
            let dst = (first + r) * stride;
            let n = pw.min(stride);
            m.pixels[dst..dst + n].copy_from_slice(&p.pixels[r * pw..r * pw + n]);
        }
        if rows > 0 {
            m.pending.push_back((first as u16, (first + rows) as u16));
        }
        if let Some(h) = m.have.get_mut(usize::from(p.segment)) {
            *h = true;
        }
        if m.have.iter().all(|&h| h) {
            m.complete = true;
            m.closing = true;
            self.done(idx);
        }
        // A newer scan of the same product, or pictures that went quiet.
        let me = self.queue[idx].info.clone();
        let (base, time) = (product(&me), me.time);
        for i in 0..self.queue.len() {
            let o = &self.queue[i];
            if o.closing || i == idx {
                continue;
            }
            if (product(&o.info) == base && o.info.time != time)
                || self.parts - o.last_part > STALE_PARTS
            {
                self.queue[i].closing = true;
                self.done(i);
            }
        }
        let open = self.queue.iter().filter(|m| !m.closing).count();
        if open > MAX_OPEN
            && let Some(i) = self.queue.iter().position(|m| !m.closing)
        {
            self.queue[i].closing = true;
            self.done(i);
        }
    }

    /// Picture `i` has just been closed.
    fn done(&mut self, i: usize) {
        let m = &self.queue[i];
        self.events.push(Event::Done {
            name: m.name.clone(),
            complete: m.complete,
        });
        if !(self.false_colour && m.complete && m.channels == 1) {
            return;
        }
        let ch = m.info.channel.as_str();
        if ch != "CH02" && ch != "CH13" {
            return;
        }
        let scene = m.info.scene();
        let k = match self.pairs.iter().position(|(s, _)| *s == scene) {
            Some(k) => k,
            None => {
                self.pairs.push_back((scene, Pair::default()));
                if self.pairs.len() > 3 {
                    self.pairs.pop_front();
                }
                self.pairs.len() - 1
            }
        };
        let pair = &mut self.pairs[k].1;
        if ch == "CH02" {
            pair.vis = Some((m.width, m.height, m.pixels.clone(), m.info.clone()));
        } else {
            pair.ir = Some((m.width, m.height, m.pixels.clone()));
        }
        if pair.vis.is_none() || pair.ir.is_none() {
            return;
        }
        let (_, pair) = self.pairs.remove(k).expect("found above");
        let (vw, vh, vis, info) = pair.vis.expect("checked");
        let (iw, ih, ir) = pair.ir.expect("checked");
        let (w, h) = (vw.max(iw), vh.max(ih));
        let vis = resize(&vis, vw, vh, w, h);
        let ir = resize(&ir, iw, ih, w, h);
        let rgb = false_colour(&vis, &ir);
        let info = ImageInfo {
            channel: "FC".into(),
            ..info
        };
        let name = info.name();
        self.events.push(Event::Done {
            name: name.clone(),
            complete: true,
        });
        self.queue.push_back(Mosaic {
            number: self.next_number,
            info,
            name,
            image_id: u32::MAX,
            width: w,
            height: h,
            channels: 3,
            pixels: rgb,
            have: vec![true],
            pending: VecDeque::from([(0, h)]),
            name_sent: false,
            closing: true,
            complete: true,
            last_part: self.parts,
            last_sent: None,
            last_flag_sent: false,
        });
        self.next_number = self.next_number.wrapping_add(1).max(1);
    }

    /// The end of the run: give up on everything still open.
    pub fn close_all(&mut self) {
        for i in 0..self.queue.len() {
            if !self.queue[i].closing {
                self.queue[i].closing = true;
                self.done(i);
            }
        }
    }

    /// True if a record is ready to go out.
    pub fn ready(&self) -> bool {
        self.queue
            .front()
            .is_some_and(|m| !m.name_sent || !m.pending.is_empty() || m.closing)
    }

    /// Queue the next record (a name or a row) on `q`. Returns the bytes
    /// queued (0 if nothing is ready).
    pub(crate) fn next_record(&mut self, q: &mut MsgQueue) -> usize {
        loop {
            let Some(m) = self.queue.front_mut() else {
                return 0;
            };
            if !m.name_sent {
                m.name_sent = true;
                let before = q.pending();
                crate::image::push_name(q, m.number, &m.name);
                if q.pending() > before {
                    return q.pending() - before;
                }
                continue;
            }
            if let Some(&(a, b)) = m.pending.front() {
                if a + 1 >= b {
                    m.pending.pop_front();
                } else {
                    m.pending[0].0 = a + 1;
                }
                let last = m.closing && m.pending.is_empty();
                m.last_flag_sent |= last;
                m.last_sent = Some(a);
                return send_row(q, m, a, last);
            }
            if m.closing {
                let m = self.queue.pop_front().expect("front exists");
                if !m.last_flag_sent
                    && let Some(r) = m.last_sent
                {
                    return send_row(q, &m, r, true);
                }
                continue;
            }
            return 0;
        }
    }
}

impl Pictures {
    /// Every record ready now, as bytes (for tests and tools; the decoder
    /// block sends them one at a time).
    pub fn drain_records(&mut self) -> Vec<u8> {
        let mut q = MsgQueue::new();
        let mut out = Vec::new();
        while self.next_record(&mut q) > 0 {
            let at = out.len();
            out.resize(at + q.pending(), 0);
            q.flush(&mut out[at..]);
        }
        out
    }
}

fn send_row(q: &mut MsgQueue, m: &Mosaic, row: u16, last: bool) -> usize {
    let stride = m.stride();
    let at = usize::from(row) * stride;
    let header = Row {
        picture: m.number,
        row,
        height: m.height,
        width: m.width,
        channels: m.channels,
        flags: if last { LAST } else { 0 },
    };
    crate::image::push_row(q, header, &m.pixels[at..at + stride]);
    header.len()
}

/// The product a picture belongs to, without its time.
fn product(i: &ImageInfo) -> (&str, &str, &str) {
    (&i.satellite, &i.region, &i.channel)
}

/// Nearest-neighbour resize of a grey picture.
fn resize(px: &[u8], w: u16, h: u16, nw: u16, nh: u16) -> Vec<u8> {
    if (w, h) == (nw, nh) {
        return px.to_vec();
    }
    let (w, h, nw, nh) = (
        usize::from(w),
        usize::from(h),
        usize::from(nw),
        usize::from(nh),
    );
    let mut out = vec![0; nw * nh];
    for y in 0..nh {
        let sy = y * h / nh;
        for x in 0..nw {
            out[y * nw + x] = px[sy * w + x * w / nw];
        }
    }
    out
}

/// A false-colour picture from visible (`vis`, ABI channel 2: brighter is
/// more reflective) and infrared (`ir`, channel 13 as GOES sends it: brighter
/// is colder) of the same size: `r = g = vis`, `b = max(vis, ir)`. Sunlit
/// cloud and land show in their natural brightness, cold cloud tops that the
/// visible doesn't show (at night, past the terminator) in blue. Our own
/// simple scheme: goesproc's look comes from a colour table (the WeatherStar
/// one) we don't ship.
pub fn false_colour(vis: &[u8], ir: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vis.len() * 3);
    for (&v, &t) in vis.iter().zip(ir) {
        out.extend_from_slice(&[v, v, v.max(t)]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use foxygnal_core::raster::{self, Next};

    fn info(ch: &str, time: f64) -> ImageInfo {
        ImageInfo {
            satellite: "G18".into(),
            region: "FD".into(),
            channel: ch.into(),
            time: Some(time),
        }
    }

    /// Everything the pictures send: (picture, name or row, last).
    fn drain(p: &mut Pictures) -> Vec<(u32, String, bool)> {
        let mut q = MsgQueue::new();
        let mut out = Vec::new();
        let mut bytes = vec![0u8; 1 << 16];
        while p.next_record(&mut q) > 0 {
            let n = q.flush(&mut bytes);
            match raster::next(&bytes[..n]) {
                Next::Row(r, _) => out.push((r.picture, format!("row{}", r.row), r.is_last())),
                Next::Name(pic, len) => out.push((
                    pic,
                    raster::parse_name(&bytes[..len]).unwrap().1.to_owned(),
                    false,
                )),
                other => panic!("{other:?}"),
            }
        }
        out
    }

    fn part<'a>(info: &'a ImageInfo, id: u32, seg: u16, px: &'a [u8]) -> Part<'a> {
        Part {
            info,
            image_id: id,
            width: 4,
            lines: 2,
            start_line: seg * 2,
            picture_width: 4,
            picture_height: 4,
            segment: seg,
            segments: 2,
            channels: 1,
            pixels: px,
        }
    }

    #[test]
    fn interleaved_segments_come_out_one_picture_at_a_time() {
        let mut p = Pictures::new();
        p.false_colour = false;
        let (a, b) = (info("CH13", 100.0), info("CH07", 100.0));
        let px = [7u8; 8];
        p.add(part(&a, 1, 0, &px));
        p.add(part(&b, 2, 0, &px));
        let out = drain(&mut p);
        // The first picture's rows so far; nothing of the second yet.
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|r| r.0 == 1));
        p.add(part(&b, 2, 1, &px));
        assert!(drain(&mut p).is_empty(), "waits for the first picture");
        p.add(part(&a, 1, 1, &px));
        let out = drain(&mut p);
        let names: Vec<_> = out.iter().map(|r| (r.0, r.1.as_str(), r.2)).collect();
        assert_eq!(
            names,
            [
                (1, "row2", false),
                (1, "row3", true),
                (
                    2,
                    "G18/FD/CH07/1970-01-01/G18_FD_CH07_19700101T000140Z",
                    false
                ),
                (2, "row0", false),
                (2, "row1", false),
                (2, "row2", false),
                (2, "row3", true),
            ]
        );
    }

    #[test]
    fn a_newer_scan_ends_an_incomplete_picture() {
        let mut p = Pictures::new();
        p.false_colour = false;
        let (old, new) = (info("CH13", 100.0), info("CH13", 700.0));
        let px = [1u8; 8];
        p.add(part(&old, 1, 0, &px));
        p.add(part(&new, 2, 0, &px));
        let out = drain(&mut p);
        // Picture 1 ends with its last row repeated as the last.
        let ones: Vec<_> = out
            .iter()
            .filter(|r| r.0 == 1)
            .map(|r| (r.1.as_str(), r.2))
            .collect();
        assert_eq!(ones[1..], [("row0", false), ("row1", true)]);
        assert_eq!(
            p.events[0],
            Event::Done {
                name: old.name(),
                complete: false
            }
        );
        p.close_all();
        assert!(drain(&mut p).last().unwrap().2);
    }

    #[test]
    fn false_colour_from_channels_2_and_13() {
        let mut p = Pictures::new();
        let (vis, ir) = (info("CH02", 5.0), info("CH13", 5.0));
        let v = [200u8; 8];
        let t = [50u8; 8];
        for seg in 0..2 {
            p.add(part(&vis, 1, seg, &v));
            p.add(part(&ir, 2, seg, &t));
        }
        let out = drain(&mut p);
        assert!(out.iter().any(|r| r.1.contains("/FC/")));
        assert_eq!(out.iter().filter(|r| r.2).count(), 3);
        assert_eq!(
            false_colour(&[10, 200], &[100, 0]),
            [10, 10, 100, 200, 200, 200]
        );
    }
}
