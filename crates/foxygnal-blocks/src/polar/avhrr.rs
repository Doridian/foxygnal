//! `avhrr_decoder`: Metop's AVHRR/3 imagery (and the other instruments'
//! packets) from CCSDS packets.
//!
//! **AVHRR/3** sends a line (2048 Earth pixels in five channels, a sixth of
//! a second of scanning) as one space packet of 12 966 bytes on virtual
//! channel 9: APID 103 while channel 3 is 3A (1.6 µm, by day), 104 while it
//! is 3B (3.7 µm). The packet's data field starts with the time stamp (day
//! since 2000-01-01, milliseconds of the day, microseconds of the
//! millisecond: CCSDS day segmented) and six bytes of on-board time; then
//! 10 355 ten-bit words, most significant bit first: 50 words of space view,
//! 5 more, the Earth view with the five channels of each pixel together
//! (pixel `p`, channel `c` at word 55 + 5p + c), housekeeping and the
//! internal calibration target; two bytes of packet error control end it.
//!
//! **Rows.** A line's row is its time stamp's distance from the picture's
//! first line in sixths of a second, so lost packets become black rows (up
//! to `gap` of them; a longer dropout ends the picture and starts a new
//! one) and a packet that comes too late is dropped. Counts are shown as
//! sent, 10 bits cut to 8: no calibration, no correction for the scan
//! geometry or the pass direction.
//!
//! **Pictures**: `image` is the colour composite of the channels chosen as
//! red, green and blue (default 2-2-1), `ch1`…`ch5` each channel in grey,
//! one picture per pass, named `METOP-B/<date>/AVHRR_rgb221_<time>Z` (and
//! `AVHRR_ch1_…`) from the first line's time stamp.
//!
//! **Other instruments.** Every other packet (MHS, AMSU-A, HIRS, IASI, SEM,
//! A-DCS, …) goes out of `other` as a packet record, unchanged, for a file
//! or another decoder; `out` counts them per instrument in its line a
//! second.

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};

use super::lrpt::test_pattern;
use super::strip::Strip;
use crate::ccsds::wire::{FLAG_GAP, RECORD_MAGIC, Record, RecordReader};
use crate::design;
use crate::digital::{MsgQueue, text_meta};

/// AVHRR's APIDs (channel 3A or 3B) and virtual channel.
pub const APID_3A: u16 = 103;
pub const APID_3B: u16 = 104;
pub const VCID_AVHRR: u8 = 9;
/// MHS's APID (the encoder sends a placeholder).
pub const APID_MHS: u16 = 34;
/// Earth-view pixels a line, ten-bit words a line.
pub const WIDTH: usize = 2048;
pub const WORDS: usize = 10_355;
/// Where the words start in the packet's data field, and the first Earth
/// view word.
const WORDS_AT: usize = 14;
const EARTH_AT: usize = 55;
/// Bytes the words take.
const WORD_BYTES: usize = (WORDS * 10).div_ceil(8);
/// Lines a second.
const LINES_PER_SECOND: i64 = 6;

/// The synthetic picture `ahrpt_encoder` sends, as 10-bit counts: channel
/// `c` (0–4), pixel `x`, line `y`. Its top 8 bits are LRPT's test pattern.
pub fn avhrr_pattern(c: usize, x: usize, y: usize) -> u16 {
    u16::from(test_pattern(c, x, y)) << 2 | ((x ^ y) & 3) as u16
}

/// The instruments counted in the status line, by APID.
const INSTRUMENTS: [&str; 8] = [
    "mhs", "amsu_a1", "amsu_a2", "hirs", "iasi", "sem", "a_dcs", "other",
];

fn instrument(apid: u16) -> usize {
    match apid {
        34 => 0,
        39 => 1,
        40 => 2,
        38 => 3,
        130..=180 => 4,
        37 => 5,
        35 => 6,
        _ => 7,
    }
}

/// The satellite's name for picture names, by spacecraft id.
fn satellite(id: u8) -> &'static str {
    match id {
        11 => "METOP-B",
        12 => "METOP-A",
        13 => "METOP-C",
        _ => "METOP",
    }
}

fn port(name: &'static str) -> PortSpec {
    PortSpec {
        name,
        ty: Some(StreamType::U8),
    }
}

const OUTPUTS: [&str; 8] = ["image", "ch1", "ch2", "ch3", "ch4", "ch5", "out", "other"];
const TEXT: usize = 6;
const OTHER: usize = 7;

pub fn register(r: &mut Registry) {
    let channel = |name: &'static str, label: &'static str, default: &'static str| {
        design::choice(
            name,
            label,
            vec!["1", "2", "3", "4", "5"],
            default,
            "AVHRR/3 channel: 1 0.63 µm, 2 0.86 µm, 3 1.6 µm (3A, by day) or 3.7 µm (3B), \
             4 10.8 µm, 5 12.0 µm.",
        )
    };
    r.register(
        BlockDescriptor {
            kind: "avhrr_decoder",
            label: "AVHRR Decoder",
            category: "Satellite",
            help: "Metop AVHRR/3 imagery: CCSDS packet records in (CCSDS Packets with \
                   `format = records`, insert zone 2), a line per packet (APID 103/104, \
                   2048 pixels × 5 channels), placed by time stamp into one long picture \
                   per pass: `image` the colour composite of the channels chosen as red, \
                   green and blue (2-2-1 by default), `ch1`…`ch5` each channel in grey. \
                   Lost lines are black rows. The other instruments' packets (MHS, AMSU-A, \
                   HIRS, IASI, …) go out of `other` as packet records; `out` has a line a \
                   second.",
            inputs: vec![port("in")],
            outputs: OUTPUTS.iter().map(|&n| port(n)).collect(),
            params: vec![
                channel("red", "Red", "2"),
                channel("green", "Green", "2"),
                channel("blue", "Blue", "1"),
                design::int_range(
                    "vcid",
                    "Virtual channel",
                    i64::from(VCID_AVHRR),
                    -1,
                    63,
                    "Only AVHRR packets of this virtual channel (9 on Metop); −1 = any.",
                ),
                design::int_range(
                    "gap",
                    "Gap limit",
                    300,
                    1,
                    10_000,
                    "Lines (six a second) of a dropout filled with black rows; a longer \
                     one ends the picture.",
                ),
            ],
        },
        build,
    );
    r.set_keywords(
        "avhrr_decoder",
        &[
            "metop",
            "ahrpt",
            "avhrr",
            "mhs",
            "amsu",
            "iasi",
            "weather satellite",
            "picture",
            "image",
        ],
    );
}

#[derive(Default, Clone, Copy)]
struct Counters {
    lines: u64,
    bad: u64,
    late: u64,
    missing: u64,
    pictures: u64,
}

pub struct AvhrrDecoder {
    rgb: [usize; 3],
    vcid: Option<u8>,
    gap: i64,
    reader: Option<RecordReader>,
    open: bool,
    /// The picture's first line's time (µs since 2000-01-01) and the last
    /// row written.
    t0: i64,
    last: i64,
    /// Channel 3's mode (APID) of the last line.
    ch3: u16,
    /// Pixels of the line: channel × width, and the composite row.
    line: Vec<u8>,
    rgb_row: Vec<u8>,
    black: Vec<u8>,
    composite: Strip,
    channels: [Strip; 5],
    queues: [MsgQueue; 8],
    /// Other instruments' packets since the last text line.
    others: [u32; INSTRUMENTS.len()],
    missing_since: u64,
    counters: Counters,
    ending: bool,
    ended: bool,
}

/// `hh:mm:ss.mmm` of µs since 2000-01-01.
struct Hms(i64);

impl std::fmt::Display for Hms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = self.0.div_euclid(1000).rem_euclid(86_400_000);
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

/// Seconds from 1970 to 2000.
const EPOCH_2000: i64 = 946_684_800;

impl AvhrrDecoder {
    pub fn new(rgb: [usize; 3], vcid: Option<u8>, gap: i64) -> Self {
        Self {
            rgb,
            vcid,
            gap,
            reader: Some(RecordReader::new()),
            open: false,
            t0: 0,
            last: -1,
            ch3: APID_3A,
            line: vec![0; 5 * WIDTH],
            rgb_row: vec![0; 3 * WIDTH],
            black: vec![0; 3 * WIDTH],
            composite: Strip::new(WIDTH as u16, 3),
            channels: std::array::from_fn(|_| Strip::new(WIDTH as u16, 1)),
            queues: std::array::from_fn(|_| MsgQueue::new()),
            others: [0; INSTRUMENTS.len()],
            missing_since: 0,
            counters: Counters::default(),
            ending: false,
            ended: false,
        }
    }

    fn record(&mut self, r: &Record<'_>) {
        let apid = r.header.apid;
        if apid != APID_3A && apid != APID_3B {
            self.other(r);
            return;
        }
        if self.vcid.is_some_and(|v| v != r.vcid) {
            return;
        }
        let data = &r.packet[6..];
        if data.len() < WORDS_AT + WORD_BYTES {
            self.counters.bad += 1;
            return;
        }
        let day = i64::from(u16::from_be_bytes([data[0], data[1]]));
        let ms = i64::from(u32::from_be_bytes([data[2], data[3], data[4], data[5]]));
        let us = i64::from(u16::from_be_bytes([data[6], data[7]]));
        if ms >= 86_400_000 || us >= 1000 {
            self.counters.bad += 1;
            return;
        }
        let t = (day * 86_400_000 + ms) * 1000 + us;
        if self.open {
            let row = self.row_of(t);
            if row <= self.last {
                self.counters.late += 1;
                return;
            }
            if row - self.last - 1 > self.gap {
                self.end_picture();
            }
        }
        if !self.open {
            self.start_picture(t, r.spacecraft);
        }
        let row = self.row_of(t);
        // Lost lines: black rows.
        for _ in self.last + 1..row {
            self.composite.row(&mut self.queues[0], &self.black);
            for c in 0..5 {
                self.channels[c].row(&mut self.queues[1 + c], &self.black[..WIDTH]);
            }
            self.missing_since += 1;
            self.counters.missing += 1;
        }
        self.unpack(&data[WORDS_AT..WORDS_AT + WORD_BYTES]);
        for x in 0..WIDTH {
            for (k, &c) in self.rgb.iter().enumerate() {
                self.rgb_row[3 * x + k] = self.line[c * WIDTH + x];
            }
        }
        self.composite.row(&mut self.queues[0], &self.rgb_row);
        for c in 0..5 {
            self.channels[c].row(
                &mut self.queues[1 + c],
                &self.line[c * WIDTH..(c + 1) * WIDTH],
            );
        }
        self.counters.lines += 1;
        if apid != self.ch3 {
            self.ch3 = apid;
            self.queues[TEXT].line(format_args!(
                "event=ch3 row={row} time={} ch3={}",
                Hms(t),
                Self::ch3_name(apid)
            ));
        }
        self.last = row;
        if row % LINES_PER_SECOND == 0 {
            self.text_line(row, t);
        }
    }

    fn ch3_name(apid: u16) -> &'static str {
        if apid == APID_3B { "3b" } else { "3a" }
    }

    fn row_of(&self, t: i64) -> i64 {
        ((t - self.t0) * LINES_PER_SECOND + 500_000).div_euclid(1_000_000)
    }

    /// The Earth view's ten-bit words, cut to 8 bits, into `line`.
    fn unpack(&mut self, words: &[u8]) {
        for p in 0..WIDTH {
            for c in 0..5 {
                let bit = (EARTH_AT + 5 * p + c) * 10;
                let (byte, shift) = (bit / 8, bit % 8);
                // The word within 24 bits (`shift` is at most 6).
                let v = u32::from(words[byte]) << 16
                    | u32::from(words[byte + 1]) << 8
                    | u32::from(words[byte + 2]);
                let w = (v >> (14 - shift)) & 0x3FF;
                self.line[c * WIDTH + p] = (w >> 2) as u8;
            }
        }
    }

    fn other(&mut self, r: &Record<'_>) {
        self.others[instrument(r.header.apid)] += 1;
        let q = &mut self.queues[OTHER];
        q.bytes(&RECORD_MAGIC);
        q.bytes(&[
            r.spacecraft,
            r.vcid,
            if r.after_gap { FLAG_GAP } else { 0 },
            0,
        ]);
        q.bytes(r.packet);
    }

    fn start_picture(&mut self, t: i64, spacecraft: u8) {
        self.open = true;
        self.t0 = t;
        self.last = -1;
        self.counters.pictures += 1;
        let (y, mo, d, h, mi, s) =
            crate::xrit::products::civil((t / 1_000_000 + EPOCH_2000) as f64);
        let stamp = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
        let dir = format!("{}/{y:04}-{mo:02}-{d:02}", satellite(spacecraft));
        let [r, g, b] = self.rgb.map(|c| c + 1);
        self.composite
            .set_name(format!("{dir}/AVHRR_rgb{r}{g}{b}_{stamp}"));
        for (c, strip) in self.channels.iter_mut().enumerate() {
            strip.set_name(format!("{dir}/AVHRR_ch{}_{stamp}", c + 1));
        }
        self.queues[TEXT].line(format_args!(
            "event=start picture={} satellite={} date={y:04}-{mo:02}-{d:02} time={}",
            self.counters.pictures,
            satellite(spacecraft),
            Hms(t)
        ));
    }

    fn text_line(&mut self, row: i64, t: i64) {
        let q = &mut self.queues[TEXT];
        q.text(format_args!(
            "row={row} time={} ch3={} missing={}",
            Hms(t),
            Self::ch3_name(self.ch3),
            self.missing_since
        ));
        for (name, n) in INSTRUMENTS.iter().zip(&mut self.others) {
            if *n > 0 {
                q.text(format_args!(" {name}={n}"));
                *n = 0;
            }
        }
        q.line(format_args!(""));
        self.missing_since = 0;
    }

    fn end_picture(&mut self) {
        if !self.open {
            return;
        }
        let rows = self.composite.rows();
        self.composite.end(&mut self.queues[0]);
        for c in 0..5 {
            self.channels[c].end(&mut self.queues[1 + c]);
        }
        let k = self.counters;
        self.queues[TEXT].line(format_args!(
            "event=end picture={} rows={rows} lines={} missing={} late={} bad={}",
            k.pictures, k.lines, k.missing, k.late, k.bad
        ));
        self.open = false;
    }
}

impl Block for AvhrrDecoder {
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
            // Outputs nobody reads have no room: drop what's queued there.
            for (i, q) in self.queues.iter_mut().enumerate() {
                if !q.is_empty() && io.outputs[i].abandoned() {
                    q.clear();
                }
            }
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.is_empty() {
                if io.inputs[0].exhausted() && !self.ending {
                    self.ending = true;
                    self.end_picture();
                    self.ended = true;
                    continue;
                }
                break;
            }
            let mut reader = self.reader.take().expect("reader");
            let n = reader.push_one(inp, |r| self.record(&r));
            self.reader = Some(reader);
            used += n;
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
            .map_err(|_| Error::Other(format!("`{name}` must be a channel 1–5")))?;
        if !(1..=5).contains(&v) {
            return Err(Error::Other(format!("`{name}` must be a channel 1–5")));
        }
        Ok(v - 1)
    };
    let vcid = p.i64("vcid")?;
    Ok(Instance {
        block: Box::new(AvhrrDecoder::new(
            [ch("red")?, ch("green")?, ch("blue")?],
            u8::try_from(vcid).ok(),
            p.i64("gap")?.max(1),
        )),
        inputs: vec![("in".into(), StreamType::U8)],
        outputs: OUTPUTS
            .iter()
            .map(|&n| (n.into(), StreamType::U8))
            .collect(),
    })
}
