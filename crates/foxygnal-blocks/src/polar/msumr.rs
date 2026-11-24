//! `msumr_decoder`: Meteor-M HRPT's MSU-MR imagery from HRPT frames.
//!
//! **The stream.** Each 1024-byte HRPT frame carries 948 bytes of the
//! imager's stream ([`super::hrpt::MSUMR_PARTS`]); the stream is cut into
//! lines of 11 850 bytes, about 6.5 a second, each starting with the sync
//! word `02 18 A7 A3 92 DD 9A BF`, then the time of day (Moscow time: hour,
//! minute, second, and a fraction taken as 1/256 s), the calibration words
//! and, from byte 50, the video: 393 groups of 4 pixels, each group the six
//! channels' 4 ten-bit values in 5 bytes (most significant bit first),
//! 1572 pixels a channel.
//!
//! **Lines.** Frames that came with the inverted marker (sync missed but the
//! frame where it should be) are used too, so the stream stays aligned. The
//! decoder looks for the sync word (up to 6 wrong bits), then takes lines
//! 11 850 bytes apart as long as each starts with the sync word and the
//! next one follows (up to 16 wrong bits); a line that fails that is
//! dropped and the decoder searches again. A line that follows the previous one in the stream is
//! the next row; after a search, the time stamps say how many rows were
//! lost (black rows, up to `gap`; a longer dropout or a step back in time
//! starts a new picture).
//!
//! **Pictures**: `image` is the colour composite of the channels chosen as
//! red, green and blue (default 2-2-1), `ch1`…`ch6` each channel in grey
//! (10-bit counts cut to 8), one picture per pass, named
//! `METEOR-M/<date>/MSU-MR-HRPT_rgb221_<time>Z` by the receiver's clock.
//! `out` has a text line a second and per picture.

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};

use super::hrpt::{
    FRAME_LEN, LINE_LEN, LINE_SYNC, LINES_PER_SECOND, MSUMR_PARTS, VIDEO_AT, WIDTH, unpack4,
};
use super::strip::Strip;
use crate::ccsds::wire::CaduReader;
use crate::design;
use crate::digital::{MsgQueue, text_meta};

/// Wrong sync bits allowed while searching, and when a line is expected.
const SEARCH_ERRORS: u32 = 6;
const LOCKED_ERRORS: u32 = 16;

fn port(name: &'static str) -> PortSpec {
    PortSpec {
        name,
        ty: Some(StreamType::U8),
    }
}

const OUTPUTS: [&str; 8] = ["image", "ch1", "ch2", "ch3", "ch4", "ch5", "ch6", "out"];
const TEXT: usize = 7;

pub fn register(r: &mut Registry) {
    let channel = |name: &'static str, label: &'static str, default: &'static str| {
        design::choice(
            name,
            label,
            vec!["1", "2", "3", "4", "5", "6"],
            default,
            "MSU-MR channel: 1 0.5–0.7 µm, 2 0.7–1.1 µm, 3 1.6–1.8 µm, 4 3.5–4.1 µm, \
             5 10.5–11.5 µm, 6 11.5–12.5 µm.",
        )
    };
    r.register(
        BlockDescriptor {
            kind: "msumr_decoder",
            label: "Meteor HRPT Decoder",
            category: "Satellite",
            help: "Meteor-M HRPT imagery: frames in (from the Meteor HRPT Demodulator), the \
                   MSU-MR imager's stream cut into lines (sync word, time, six 10-bit \
                   channels of 1572 pixels) and put together into one long picture per \
                   pass: `image` the colour composite of the channels chosen as red, green \
                   and blue (2-2-1 by default), `ch1`…`ch6` each channel in grey. Lost \
                   lines are black rows; `out` has a text line a second.",
            inputs: vec![port("in")],
            outputs: OUTPUTS.iter().map(|&n| port(n)).collect(),
            params: vec![
                channel("red", "Red", "2"),
                channel("green", "Green", "2"),
                channel("blue", "Blue", "1"),
                design::int_range(
                    "gap",
                    "Gap limit",
                    300,
                    1,
                    10_000,
                    "Lines (6.5 a second) of a dropout filled with black rows; a longer \
                     one ends the picture.",
                ),
            ],
        },
        build,
    );
    r.set_keywords(
        "msumr_decoder",
        &[
            "meteor",
            "hrpt",
            "msu-mr",
            "weather satellite",
            "picture",
            "image",
        ],
    );
}

/// Wrong bits of the sync word at the start of `b`.
fn sync_errors(b: &[u8]) -> u32 {
    b[..8]
        .iter()
        .zip(&LINE_SYNC)
        .map(|(a, s)| (a ^ s).count_ones())
        .sum()
}

#[derive(Default, Clone, Copy)]
struct Counters {
    lines: u64,
    missing: u64,
    late: u64,
    pictures: u64,
    bad_frames: u64,
}

pub struct MsumrDecoder {
    rgb: [usize; 3],
    gap: i64,
    reader: CaduReader,
    frame_buf: Vec<u8>,
    /// The MSU-MR stream not yet cut into lines.
    stream: Vec<u8>,
    locked: bool,
    /// The line just taken follows the previous one in the stream.
    contiguous: bool,
    open: bool,
    /// Row of the last line, and its time (s of the day).
    row: i64,
    time: f64,
    line: Vec<u8>,
    pixels: Vec<u8>,
    rgb_row: Vec<u8>,
    black: Vec<u8>,
    composite: Strip,
    channels: [Strip; 6],
    queues: [MsgQueue; 8],
    missing_since: u64,
    counters: Counters,
    ended: bool,
}

/// `hh:mm:ss.sss` of seconds of the day.
struct Hms(f64);

impl std::fmt::Display for Hms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = (self.0 * 1000.0).round() as i64;
        let ms = ms.rem_euclid(86_400_000);
        write!(
            f,
            "{:02}:{:02}:{:02}.{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    }
}

impl MsumrDecoder {
    pub fn new(rgb: [usize; 3], gap: i64) -> Self {
        Self {
            rgb,
            gap,
            reader: CaduReader::new(FRAME_LEN),
            frame_buf: vec![0; FRAME_LEN],
            stream: Vec::with_capacity(2 * LINE_LEN + 1024),
            locked: false,
            contiguous: false,
            open: false,
            row: -1,
            time: 0.0,
            line: vec![0; LINE_LEN],
            pixels: vec![0; 6 * WIDTH],
            rgb_row: vec![0; 3 * WIDTH],
            black: vec![0; 3 * WIDTH],
            composite: Strip::new(WIDTH as u16, 3),
            channels: std::array::from_fn(|_| Strip::new(WIDTH as u16, 1)),
            queues: std::array::from_fn(|_| MsgQueue::new()),
            missing_since: 0,
            counters: Counters::default(),
            ended: false,
        }
    }

    /// One frame's MSU-MR bytes into the stream, and the lines they complete.
    fn frame(&mut self, data: &[u8], ok: bool) {
        if !ok {
            self.counters.bad_frames += 1;
        }
        for &(a, b) in &MSUMR_PARTS {
            self.stream.extend_from_slice(&data[a..b]);
        }
        self.cut();
    }

    /// Take the lines the stream holds.
    fn cut(&mut self) {
        loop {
            if !self.locked {
                let found = (0..self.stream.len().saturating_sub(7))
                    .find(|&i| sync_errors(&self.stream[i..]) <= SEARCH_ERRORS);
                match found {
                    Some(i) => {
                        self.stream.drain(..i);
                        self.locked = true;
                        self.contiguous = false;
                    }
                    None => {
                        let keep = self.stream.len().min(7);
                        self.stream.drain(..self.stream.len() - keep);
                        return;
                    }
                }
            }
            // A line is taken when the next one's sync word follows it: a
            // stream broken inside it (frames lost while the demodulator
            // was unlocked) fails that, and the line is dropped.
            if self.stream.len() < LINE_LEN + 8 {
                return;
            }
            if sync_errors(&self.stream) > LOCKED_ERRORS
                || sync_errors(&self.stream[LINE_LEN..]) > LOCKED_ERRORS
            {
                self.locked = false;
                self.stream.drain(..1);
                continue;
            }
            self.line.copy_from_slice(&self.stream[..LINE_LEN]);
            self.stream.drain(..LINE_LEN);
            self.take_line();
            self.contiguous = true;
        }
    }

    fn take_line(&mut self) {
        let l = &self.line;
        let t = f64::from(l[8]) * 3600.0
            + f64::from(l[9]) * 60.0
            + f64::from(l[10])
            + f64::from(l[11]) / 256.0;
        let valid = l[8] < 24 && l[9] < 60 && l[10] < 60;
        let row = if !self.open {
            0
        } else if self.contiguous || !valid {
            self.row + 1
        } else {
            let mut dt = t - self.time;
            if dt < -43_200.0 {
                dt += 86_400.0;
            }
            if dt <= 0.0 {
                self.counters.late += 1;
                // A step back in time: a new pass.
                if dt < -1.0 {
                    self.end_picture();
                }
                if self.open {
                    return;
                }
                0
            } else {
                self.row + ((dt * LINES_PER_SECOND).round() as i64).max(1)
            }
        };
        if self.open && row - self.row - 1 > self.gap {
            self.end_picture();
        }
        if !self.open {
            self.start_picture(t);
        }
        let row = if self.row < 0 { 0 } else { row };
        for _ in self.row + 1..row {
            self.composite.row(&mut self.queues[0], &self.black);
            for c in 0..6 {
                self.channels[c].row(&mut self.queues[1 + c], &self.black[..WIDTH]);
            }
            self.missing_since += 1;
            self.counters.missing += 1;
        }
        for g in 0..WIDTH / 4 {
            for c in 0..6 {
                let at = VIDEO_AT + 30 * g + 5 * c;
                let v = unpack4(&self.line[at..at + 5]);
                for (k, &x) in v.iter().enumerate() {
                    self.pixels[c * WIDTH + 4 * g + k] = (x >> 2) as u8;
                }
            }
        }
        for x in 0..WIDTH {
            for (k, &c) in self.rgb.iter().enumerate() {
                self.rgb_row[3 * x + k] = self.pixels[c * WIDTH + x];
            }
        }
        self.composite.row(&mut self.queues[0], &self.rgb_row);
        for c in 0..6 {
            self.channels[c].row(
                &mut self.queues[1 + c],
                &self.pixels[c * WIDTH..(c + 1) * WIDTH],
            );
        }
        self.counters.lines += 1;
        // A text line about once a second.
        if self.row < 0 || row / 6 != self.row / 6 {
            self.queues[TEXT].line(format_args!(
                "row={row} time={} missing={}",
                Hms(t),
                self.missing_since
            ));
            self.missing_since = 0;
        }
        self.row = row;
        self.time = t;
    }

    fn start_picture(&mut self, t: f64) {
        self.open = true;
        self.row = -1;
        self.counters.pictures += 1;
        let (y, mo, d, h, mi, s) = crate::xrit::products::civil(crate::sat::now_unix());
        let stamp = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
        let dir = format!("METEOR-M/{y:04}-{mo:02}-{d:02}");
        let [r, g, b] = self.rgb.map(|c| c + 1);
        self.composite
            .set_name(format!("{dir}/MSU-MR-HRPT_rgb{r}{g}{b}_{stamp}"));
        for (c, strip) in self.channels.iter_mut().enumerate() {
            strip.set_name(format!("{dir}/MSU-MR-HRPT_ch{}_{stamp}", c + 1));
        }
        self.queues[TEXT].line(format_args!(
            "event=start picture={} time={} (Moscow)",
            self.counters.pictures,
            Hms(t)
        ));
    }

    fn end_picture(&mut self) {
        if !self.open {
            return;
        }
        let rows = self.composite.rows();
        self.composite.end(&mut self.queues[0]);
        for c in 0..6 {
            self.channels[c].end(&mut self.queues[1 + c]);
        }
        let k = self.counters;
        self.queues[TEXT].line(format_args!(
            "event=end picture={} rows={rows} lines={} missing={} late={} missed_sync_frames={}",
            k.pictures, k.lines, k.missing, k.late, k.bad_frames
        ));
        self.open = false;
    }
}

impl Block for MsumrDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for i in 0..OUTPUTS.len() {
            text_meta(cx, i);
        }
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let mut made = [0usize; 8];
        let mut used = 0;
        loop {
            let mut busy = false;
            for (i, q) in self.queues.iter_mut().enumerate() {
                made[i] += q.flush(&mut io.outputs[i].slice_mut::<u8>()[made[i]..]);
                busy |= !q.is_empty() && !io.outputs[i].abandoned();
            }
            if busy {
                break;
            }
            for (i, q) in self.queues.iter_mut().enumerate() {
                if !q.is_empty() && io.outputs[i].abandoned() {
                    q.clear();
                }
            }
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.is_empty() {
                if io.inputs[0].exhausted() && !self.ended {
                    self.end_picture();
                    self.ended = true;
                    continue;
                }
                break;
            }
            let (reader, buf) = (&mut self.reader, &mut self.frame_buf);
            let mut ok = None;
            used += reader.push_one(inp, |c| {
                buf.copy_from_slice(c.data);
                ok = Some(c.ok);
            });
            if let Some(ok) = ok {
                let buf = std::mem::take(&mut self.frame_buf);
                self.frame(&buf, ok);
                self.frame_buf = buf;
            }
        }
        let progress = used > 0 || made.iter().any(|&m| m > 0);
        if !progress {
            let pending = self.queues.iter().any(|q| !q.is_empty());
            if self.ended && !pending {
                return Ok(WorkStatus::Done);
            }
            if pending && io.inputs[0].exhausted() {
                return Ok(WorkStatus::Wait(std::time::Duration::from_millis(20)));
            }
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        for (i, &m) in made.iter().enumerate() {
            io.produce(i, m);
        }
        Ok(WorkStatus::Ok)
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ch = |name: &str| -> Result<usize, Error> {
        let v: usize = p
            .str(name)?
            .parse()
            .map_err(|_| Error::Other(format!("`{name}` must be a channel 1–6")))?;
        if !(1..=6).contains(&v) {
            return Err(Error::Other(format!("`{name}` must be a channel 1–6")));
        }
        Ok(v - 1)
    };
    Ok(Instance {
        block: Box::new(MsumrDecoder::new(
            [ch("red")?, ch("green")?, ch("blue")?],
            p.i64("gap")?.max(1),
        )),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: OUTPUTS
            .iter()
            .map(|&n| (n.into(), StreamType::U8))
            .collect(),
    })
}
