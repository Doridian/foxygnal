//! `lrpt_decoder`: Meteor-M LRPT's MSU-MR imagery from CCSDS packets.
//!
//! The imager's six channels travel as APIDs 64–69 on virtual channel 5
//! (usually three of them: 64/65/66 by day, 64/65/67 or 64/65/68 at other
//! times). A scan of 8 pixel rows × 1568 pixels per channel is cut into 14
//! packets of 14 JPEG-coded 8 × 8 blocks ([`super::jpeg`]); the user data
//! starts with a time stamp (day, milliseconds and microseconds of the day),
//! the number of the first block (0, 14, … 182), the table ids (0) and the
//! marker `FF F0` with the quality factor.
//!
//! **Rows.** The packets of all APIDs share one 14-bit sequence counter, and
//! a scan takes 43 of them: 14 for each of the three channels, then one
//! telemetry packet (APID 70). So a channel's scan starts at sequence count
//! `seq − block/14`, and scans are 43 counts apart; the telemetry packet
//! marks where a cycle starts, so the three channels of one scan land on
//! the same row. Until it has been seen (it comes once a scan) the decoder
//! holds the packets back; without it the cycle starts at the channel that
//! no other channel precedes by 14 counts. The counter wraps every 16 384
//! packets (about 7.6 minutes); a gap longer than a minute is unwrapped with
//! the packets' time stamps.
//!
//! **Pictures.** One picture per pass on each output: `image` is the colour
//! composite of the channels chosen as red, green and blue (default 2-2-1,
//! the classic "221"), `ch1`…`ch6` each channel in grey, starting at the
//! scan it first appeared in. Scans are written out two scans behind the
//! newest (so a channel's packets have arrived). Missing packets leave black
//! blocks, missing scans black rows (gap rows), up to `gap` scans; a longer
//! dropout ends the picture and starts a new one. The picture ends when the
//! input does. Pictures are named `METEOR-M/<date>/MSU-MR_rgb221_<time>Z`
//! (and `MSU-MR_ch1_…`) by the receiver's clock. `out` has a text line per scan (`row=… time=… channels=…
//! missing=…`) and per picture (`event=start`, `event=end rows=…`).

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};

use super::jpeg::{self, STRIP_PIXELS, STRIP_W};
use super::strip::Strip;
use crate::ccsds::wire::{Record, RecordReader};
use crate::design;
use crate::digital::{MsgQueue, text_meta};

/// Pixels per row, packets per scan and channel, packets per scan cycle.
pub const WIDTH: usize = 1568;
pub const PACKETS_PER_SCAN: usize = 14;
pub const CYCLE: i64 = 43;
/// The first image APID and the telemetry APID.
pub const APID_FIRST: u16 = 64;
pub const APID_TLM: u16 = 70;
/// Scans buffered before they're written.
const RING: usize = 3;
/// Packets held back until the cycle start is known.
const HOLD: usize = 2 * CYCLE as usize;
/// Seconds per scan (8 rows), nominal; only used to unwrap long gaps.
const SCAN_SECONDS: f64 = 1.23;

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
            "MSU-MR channel (APID 63 + n): 1 0.5–0.7 µm, 2 0.7–1.1 µm, 3 1.6–1.8 µm, \
             4 3.5–4.1 µm, 5 10.5–11.5 µm, 6 11.5–12.5 µm.",
        )
    };
    r.register(
        BlockDescriptor {
            kind: "lrpt_decoder",
            label: "Meteor LRPT Decoder",
            category: "Satellite",
            help: "Meteor-M LRPT imagery: CCSDS packet records in (CCSDS Packets with \
                   `format = records`, insert zone 2), the MSU-MR imager's JPEG-coded scans \
                   (APIDs 64–69) decoded and put together into one long picture per pass: \
                   `image` the colour composite of the channels chosen as red, green and \
                   blue (2-2-1 by default), `ch1`…`ch6` each channel in grey. Missing \
                   packets are black blocks, short dropouts black rows; `out` has a text \
                   line per scan.",
            inputs: vec![port("in")],
            outputs: OUTPUTS.iter().map(|&n| port(n)).collect(),
            params: vec![
                channel("red", "Red", "2"),
                channel("green", "Green", "2"),
                channel("blue", "Blue", "1"),
                design::int_range(
                    "vcid",
                    "Virtual channel",
                    5,
                    -1,
                    63,
                    "Only packets of this virtual channel (5 on Meteor-M); −1 = any.",
                ),
                design::int_range(
                    "gap",
                    "Gap limit",
                    100,
                    1,
                    1000,
                    "Scans (8 rows, 1.2 s each) of a dropout filled with black rows; a \
                     longer one ends the picture.",
                ),
            ],
        },
        build,
    );
    r.set_keywords(
        "lrpt_decoder",
        &[
            "meteor",
            "lrpt",
            "msu-mr",
            "weather satellite",
            "jpeg",
            "picture",
            "image",
        ],
    );
}

/// A decoded strip waiting for its place.
#[derive(Clone)]
struct Piece {
    channel: usize,
    /// Unwrapped sequence count of the channel's first packet of the scan.
    scan_seq: i64,
    col: usize,
    ms: u32,
    pixels: [u8; STRIP_PIXELS],
}

#[derive(Default, Clone, Copy)]
struct Counters {
    packets: u64,
    bad: u64,
    late: u64,
    pictures: u64,
}

pub struct LrptDecoder {
    rgb: [usize; 3],
    vcid: Option<u8>,
    gap: i64,
    reader: Option<RecordReader>,
    /// Last raw and unwrapped sequence counts and the time stamp (ms of the
    /// day) with it.
    last_seq: Option<(u16, i64, Option<u32>)>,
    /// Sequence count phase (mod 43) where a scan cycle starts.
    anchor: Option<i64>,
    /// Phases of the channels' scans seen while the anchor isn't known.
    phases: [Option<i64>; 6],
    held: Vec<Piece>,
    /// A piece waiting for rows to be written before it fits.
    waiting: Option<Box<Piece>>,
    // The picture in progress.
    open: bool,
    /// Scan of the picture's first row, the oldest scan buffered, the
    /// newest seen, and rows to write up to (exclusive).
    first: i64,
    base: i64,
    newest: i64,
    write_to: i64,
    /// Restart at this scan once written out (a dropout too long).
    restart: bool,
    /// Pixels: channel × ring slot × 8 rows × width; packets present per
    /// channel and slot (bit per column); the scan's time.
    ring: Vec<u8>,
    present: [[u16; RING]; 6],
    slot_ms: [Option<u32>; RING],
    /// The scan each channel first appeared at in this picture.
    since: [Option<i64>; 6],
    composite: Strip,
    channels: [Strip; 6],
    queues: [MsgQueue; 8],
    row: Vec<u8>,
    counters: Counters,
    ending: bool,
    ended: bool,
}

impl LrptDecoder {
    pub fn new(rgb: [usize; 3], vcid: Option<u8>, gap: i64) -> Self {
        Self {
            rgb,
            vcid,
            gap,
            reader: Some(RecordReader::new()),
            last_seq: None,
            anchor: None,
            phases: [None; 6],
            held: Vec::with_capacity(HOLD + 1),
            waiting: None,
            open: false,
            first: 0,
            base: 0,
            newest: 0,
            write_to: 0,
            restart: false,
            ring: vec![0; 6 * RING * 8 * WIDTH],
            present: [[0; RING]; 6],
            slot_ms: [None; RING],
            since: [None; 6],
            composite: Strip::new(WIDTH as u16, 3),
            channels: std::array::from_fn(|_| Strip::new(WIDTH as u16, 1)),
            queues: std::array::from_fn(|_| MsgQueue::new()),
            row: vec![0; 3 * WIDTH],
            counters: Counters::default(),
            ending: false,
            ended: false,
        }
    }

    /// Unwrap a 14-bit sequence count.
    fn unwrap(&mut self, seq: u16, ms: Option<u32>) -> i64 {
        let u = match self.last_seq {
            None => i64::from(seq),
            Some((raw, u, last_ms)) => {
                let mut d = (i64::from(seq) - i64::from(raw)).rem_euclid(16384);
                if d >= 8192 {
                    d -= 16384;
                }
                if let (Some(a), Some(b)) = (last_ms, ms) {
                    let dt =
                        f64::from((b as i64 - a as i64).rem_euclid(86_400_000) as u32) / 1000.0;
                    if dt > 60.0 && dt < 43_200.0 {
                        let expect = dt / SCAN_SECONDS * CYCLE as f64;
                        let k = ((expect - d as f64) / 16384.0).round();
                        d += k as i64 * 16384;
                    }
                }
                u + d
            }
        };
        self.last_seq = Some((seq, u, ms));
        u
    }

    /// One packet record.
    fn record(&mut self, r: &Record<'_>) {
        if self.vcid.is_some_and(|v| v != r.vcid) {
            return;
        }
        let apid = r.header.apid;
        let data = &r.packet[6..];
        let ms =
            (data.len() >= 6).then(|| u32::from_be_bytes([data[2], data[3], data[4], data[5]]));
        if apid == APID_TLM {
            let u = self.unwrap(r.header.seq, ms);
            if self.anchor.is_none() {
                self.anchor = Some((u + 1).rem_euclid(CYCLE));
                self.release_held();
            }
            return;
        }
        if !(APID_FIRST..APID_FIRST + 6).contains(&apid) {
            return;
        }
        self.counters.packets += 1;
        let channel = usize::from(apid - APID_FIRST);
        // Time (8), block number, two table ids, FF F0, quality.
        if data.len() < 15
            || data[9] != 0
            || data[10] != 0
            || data[11..13] != [0xFF, 0xF0]
            || data[13] == 0
            || data[13] > 100
            || !data[8].is_multiple_of(14)
            || data[8] > 182
        {
            self.counters.bad += 1;
            return;
        }
        let u = self.unwrap(r.header.seq, ms);
        let col = usize::from(data[8]) / PACKETS_PER_SCAN;
        let mut piece = Piece {
            channel,
            scan_seq: u - col as i64,
            col,
            ms: ms.unwrap_or(0),
            pixels: [0; STRIP_PIXELS],
        };
        let n = jpeg::decode_strip(&data[14..], data[13], &mut piece.pixels);
        if n < jpeg::BLOCKS {
            // The blocks after a coding error stay black.
            self.counters.bad += 1;
            piece.pixels[..]
                .chunks_mut(STRIP_W)
                .for_each(|row| row[n * 8..].fill(0));
        }
        if self.anchor.is_some() {
            self.place(piece);
            return;
        }
        self.phases[channel] = Some(piece.scan_seq.rem_euclid(CYCLE));
        self.held.push(piece);
        if self.held.len() >= HOLD {
            self.anchor = Some(self.guess_anchor());
            self.release_held();
        }
    }

    /// The cycle start without a telemetry packet: the phase of the channel
    /// no other channel precedes by 14 counts.
    fn guess_anchor(&self) -> i64 {
        let seen: Vec<i64> = self.phases.iter().flatten().copied().collect();
        seen.iter()
            .copied()
            .find(|&p| {
                !seen
                    .iter()
                    .any(|&q| (p - q).rem_euclid(CYCLE) == PACKETS_PER_SCAN as i64)
            })
            .or_else(|| seen.first().copied())
            .unwrap_or(0)
    }

    fn release_held(&mut self) {
        // Pieces are placed in arrival order; one that has to wait for rows
        // to be written stops the release until they are.
        while self.waiting.is_none() && !self.held.is_empty() {
            let p = self.held.remove(0);
            self.place(p);
        }
    }

    /// Put a piece into the ring, or start the writing it needs first.
    fn place(&mut self, p: Piece) {
        let anchor = self.anchor.unwrap_or(0);
        let scan = (p.scan_seq - anchor).div_euclid(CYCLE);
        if !self.open {
            self.open = true;
            self.first = scan;
            self.base = scan;
            self.newest = scan;
            self.write_to = scan;
            self.since = [None; 6];
            self.counters.pictures += 1;
            self.name_pictures();
            self.queues[TEXT].line(format_args!(
                "event=start picture={} time={}",
                self.counters.pictures,
                Hms(p.ms)
            ));
        }
        if scan < self.base {
            self.counters.late += 1;
            return;
        }
        if scan > self.newest + self.gap || scan - self.first > 8000 {
            // A dropout too long: write out what's there, then a new picture.
            self.write_to = self.newest + 1;
            self.restart = true;
            self.waiting = Some(Box::new(p));
            return;
        }
        if scan >= self.base + RING as i64 {
            self.write_to = scan - RING as i64 + 1;
            self.waiting = Some(Box::new(p));
            return;
        }
        self.newest = self.newest.max(scan);
        let slot = scan.rem_euclid(RING as i64) as usize;
        let at = (p.channel * RING + slot) * 8 * WIDTH;
        for y in 0..8 {
            let dst = at + y * WIDTH + p.col * STRIP_W;
            self.ring[dst..dst + STRIP_W]
                .copy_from_slice(&p.pixels[y * STRIP_W..(y + 1) * STRIP_W]);
        }
        self.present[p.channel][slot] |= 1 << p.col;
        self.slot_ms[slot].get_or_insert(p.ms);
        self.since[p.channel].get_or_insert(scan);
    }

    /// Names for the picture starting: `METEOR-M/<date>/MSU-MR_rgb221_<time>Z`
    /// and `…/MSU-MR_ch1_<time>Z`, by the receiver's clock (the packets
    /// carry only the time of day).
    fn name_pictures(&mut self) {
        let (y, mo, d, h, mi, s) = crate::xrit::products::civil(crate::sat::now_unix());
        let stamp = format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z");
        let dir = format!("METEOR-M/{y:04}-{mo:02}-{d:02}");
        let [r, g, b] = self.rgb.map(|c| c + 1);
        self.composite
            .set_name(format!("{dir}/MSU-MR_rgb{r}{g}{b}_{stamp}"));
        for (c, strip) in self.channels.iter_mut().enumerate() {
            strip.set_name(format!("{dir}/MSU-MR_ch{}_{stamp}", c + 1));
        }
    }

    /// Write the oldest buffered scan (8 rows on every output).
    fn write_scan(&mut self) {
        let scan = self.base;
        let slot = scan.rem_euclid(RING as i64) as usize;
        let chan = |c: usize, y: usize| (c * RING + slot) * 8 * WIDTH + y * WIDTH;
        for y in 0..8 {
            for x in 0..WIDTH {
                for (k, &c) in self.rgb.iter().enumerate() {
                    self.row[3 * x + k] = self.ring[chan(c, y) + x];
                }
            }
            self.composite.row(&mut self.queues[0], &self.row);
            for c in 0..6 {
                if self.since[c].is_some_and(|s| s <= scan) {
                    let at = chan(c, y);
                    self.channels[c].row(&mut self.queues[1 + c], &self.ring[at..at + WIDTH]);
                }
            }
        }
        let q = &mut self.queues[TEXT];
        q.text(format_args!("row={}", (scan - self.first) * 8));
        if let Some(ms) = self.slot_ms[slot] {
            q.text(format_args!(" time={}", Hms(ms)));
        }
        q.text(format_args!(" channels="));
        let mut missing = 0;
        let mut sep = "";
        for c in 0..6 {
            if self.present[c][slot] != 0 {
                q.text(format_args!("{sep}{}", c + 1));
                sep = ",";
            }
            if self.since[c].is_some_and(|s| s <= scan) {
                missing += PACKETS_PER_SCAN as u32 - self.present[c][slot].count_ones();
            }
        }
        q.line(format_args!(" missing={missing}"));
        for c in 0..6 {
            let at = chan(c, 0);
            self.ring[at..at + 8 * WIDTH].fill(0);
            self.present[c][slot] = 0;
        }
        self.slot_ms[slot] = None;
        self.base += 1;
    }

    /// End the picture: its last rows on every output.
    fn end_picture(&mut self) {
        if !self.open {
            return;
        }
        let rows = self.composite.rows();
        self.composite.end(&mut self.queues[0]);
        for c in 0..6 {
            self.channels[c].end(&mut self.queues[1 + c]);
        }
        self.queues[TEXT].line(format_args!(
            "event=end picture={} rows={rows} packets={} bad={} late={}",
            self.counters.pictures, self.counters.packets, self.counters.bad, self.counters.late
        ));
        self.open = false;
    }

    /// One step of pending work (writing a scan, placing a waiting piece,
    /// ending); false if there is none.
    fn step(&mut self) -> bool {
        if self.open && self.base < self.write_to {
            self.write_scan();
            return true;
        }
        if self.restart {
            self.restart = false;
            self.end_picture();
            return true;
        }
        if let Some(p) = self.waiting.take() {
            self.place(*p);
            if self.waiting.is_none() {
                self.release_held();
            }
            return true;
        }
        if self.ending && !self.ended {
            if self.anchor.is_none() && !self.held.is_empty() {
                self.anchor = Some(self.guess_anchor());
                self.release_held();
                return true;
            }
            if self.open && self.base <= self.newest {
                self.write_to = self.newest + 1;
                return true;
            }
            self.end_picture();
            self.ended = true;
            return true;
        }
        false
    }
}

/// Milliseconds of the day as `hh:mm:ss.mmm`.
struct Hms(u32);

impl std::fmt::Display for Hms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = self.0 % 86_400_000;
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

impl Block for LrptDecoder {
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
            if self.step() {
                continue;
            }
            let inp = &io.inputs[0].slice::<u8>()[used..];
            if inp.is_empty() {
                if io.inputs[0].exhausted() && !self.ending {
                    self.ending = true;
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
            if self.ended {
                return Ok(WorkStatus::Done);
            }
            let pending = self.queues.iter().any(|q| !q.is_empty());
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
    let vcid = p.i64("vcid")?;
    Ok(Instance {
        block: Box::new(LrptDecoder::new(
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
