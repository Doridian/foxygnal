//! UAT (Universal Access Transceiver, 978 MHz, US): ADS-B from aircraft
//! and FIS-B/TIS-B uplinks from ground stations.
//!
//! The signal is continuous-phase FSK at 1.041667 Mbit/s, ±312.5 kHz. The
//! input is an FM discriminator's output (`fm_demod`, deemphasis off), at
//! two or more samples per bit (2.083333 MS/s, as dump978 uses, or more):
//! positive for a 1.
//!
//! A frame starts with a 36-bit sync word: `0xEACDDA4E2` for ADS-B,
//! its complement `0x153225B1D` for uplinks. The decoder slices the
//! discriminator at zero at every sample offset and compares the 36 bits a
//! bit period apart with both words, allowing four wrong bits (like dump978).
//! At a match it refines the timing to a quarter sample by the soft
//! correlation with the word, takes the decision threshold halfway between
//! the mean "1" and "0" of the sync bits (which removes a frequency offset),
//! and reads the frame by interpolating between samples:
//!
//! - ADS-B: 384 bits, tried as a Long frame, RS(48, 34), then as a Basic
//!   one, RS(30, 18). The payload type must agree with the length.
//! - Uplink: 4416 bits, six RS(92, 72) blocks interleaved byte by byte.
//!
//! Each decoded frame is one line of text (an uplink adds a line per text
//! record of its FIS-B products).

mod frame;
mod rs;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamReader, PortSpec, Registry,
    StreamType, WorkIo, WorkStatus,
};

use super::{MsgQueue, idle_status, text_meta};
use frame::{AdsbMessage, InfoFrame, Uplink};

/// UAT bit rate.
pub const BIT_RATE: f64 = 1_041_666.666_666_7;
/// The lowest sample rate: two samples per bit.
pub const MIN_RATE: f64 = 2.0 * BIT_RATE;

pub const ADSB_SYNC: u64 = 0xE_ACDD_A4E2;
pub const UPLINK_SYNC: u64 = 0x1_5322_5B1D;
const SYNC_BITS: usize = 36;
/// Sync bits that may be wrong.
const SYNC_ERRORS: u32 = 4;

pub const LONG_BYTES: usize = 48;
pub const SHORT_BYTES: usize = 30;
pub const UPLINK_BYTES: usize = 552;
pub const UPLINK_BLOCK: usize = 92;
pub const UPLINK_DATA: usize = 72;

/// Samples copied in per pass.
const CHUNK: usize = 64 * 1024;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "uat_decoder",
            label: "UAT Decoder",
            category: "Digital",
            help: "Decodes UAT (978 MHz, US) from an FM demodulator's output at 2.083 MS/s or \
                   more (two or more samples per bit; deviation 312.5 kHz, de-emphasis off): \
                   sync words, Reed-Solomon correction, then ADS-B messages (address, position, \
                   altitude, velocity, call sign, category) and ground uplinks (station \
                   position, FIS-B products; METAR, TAF, NOTAM and other text products as \
                   their own lines, graphical ones such as NEXRAD counted). One line of text \
                   per message, e.g. `t=1.234567 type=adsb mdb=1 addr=A1B2C3 aq=0 lat=… \
                   alt_ft=4500 … callsign=N123AB raw=…`; connect it to a Message Log.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![],
        },
        build,
    );
}

/// What was found at a sync word.
enum Found {
    Adsb { len: usize, errors: usize },
    Uplink { errors: usize },
}

pub struct UatDecoder {
    rate: f64,
    /// Samples per bit.
    sps: f64,
    /// `round(k · sps)` for the 36 sync bits.
    sync_at: [usize; SYNC_BITS],
    /// Samples a whole uplink needs from its first sync bit, and a Long
    /// ADS-B frame.
    span: usize,
    adsb_span: usize,
    /// Samples being searched; `buf[0]` is stream sample `base`.
    buf: Vec<f32>,
    base: u64,
    scan: usize,
    /// Demodulated bytes of the frame being tried.
    raw: [u8; UPLINK_BYTES],
    /// The decoded payload (uplink: 6 × 72 bytes).
    payload: [u8; 6 * UPLINK_DATA],
    /// Scratch for FIS-B text.
    text: Vec<u8>,
    queue: MsgQueue,
}

impl Default for UatDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl UatDecoder {
    pub fn new() -> Self {
        let mut d = Self {
            rate: MIN_RATE,
            sps: 2.0,
            sync_at: [0; SYNC_BITS],
            span: 0,
            adsb_span: 0,
            buf: Vec::new(),
            base: 0,
            scan: 0,
            raw: [0; UPLINK_BYTES],
            payload: [0; 6 * UPLINK_DATA],
            text: Vec::with_capacity(4096),
            queue: MsgQueue::new(),
        };
        d.set_rate(MIN_RATE);
        d
    }

    fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
        self.sps = rate / BIT_RATE;
        for (k, a) in self.sync_at.iter_mut().enumerate() {
            *a = (k as f64 * self.sps).round() as usize;
        }
        let span =
            |bytes: usize| ((SYNC_BITS + bytes * 8 + 2) as f64 * self.sps).ceil() as usize + 2;
        self.span = span(UPLINK_BYTES);
        self.adsb_span = span(LONG_BYTES);
        self.buf = Vec::with_capacity(CHUNK + self.span);
    }

    /// The input at fractional position `x`, interpolated.
    #[inline]
    fn at(&self, x: f64) -> f32 {
        let i = x.floor();
        let f = (x - i) as f32;
        let i = i as usize;
        self.buf[i] * (1.0 - f) + self.buf[i + 1] * f
    }

    /// Which sync word (if any) starts at sample `n`, sliced at zero.
    #[inline]
    fn sync_at(&self, n: usize) -> Option<u64> {
        let b = &self.buf[n..];
        let mut wrong = 0u32; // against the ADS-B word
        for (k, &off) in self.sync_at.iter().enumerate() {
            let bit = u64::from(b[off] > 0.0);
            let want = ADSB_SYNC >> (SYNC_BITS - 1 - k) & 1;
            wrong += (bit != want) as u32;
            let right = k as u32 + 1 - wrong;
            if wrong > SYNC_ERRORS && right > SYNC_ERRORS {
                return None;
            }
        }
        if wrong <= SYNC_ERRORS {
            Some(ADSB_SYNC)
        } else {
            Some(UPLINK_SYNC)
        }
    }

    /// Soft correlation with `word` for a frame starting at `t`, and the
    /// mean levels of its 1 and 0 bits.
    fn correlate(&self, t: f64, word: u64) -> (f32, f32, f32) {
        let (mut one, mut zero) = (0.0f32, 0.0f32);
        for k in 0..SYNC_BITS {
            let v = self.at(t + k as f64 * self.sps);
            if word >> (SYNC_BITS - 1 - k) & 1 == 1 {
                one += v;
            } else {
                zero += v;
            }
        }
        // Each word has 18 ones and 18 zeros.
        let (one, zero) = (one / 18.0, zero / 18.0);
        (one - zero, one, zero)
    }

    /// Demodulate `bytes` bytes starting after the sync word at `t`.
    fn demod(&mut self, t: f64, threshold: f32, bytes: usize) {
        let start = t + SYNC_BITS as f64 * self.sps;
        for k in 0..bytes {
            let mut v = 0u8;
            for i in 0..8 {
                let x = start + (8 * k + i) as f64 * self.sps;
                v = v << 1 | u8::from(self.at(x) > threshold);
            }
            self.raw[k] = v;
        }
    }

    fn try_adsb(&mut self, t: f64, threshold: f32) -> Option<Found> {
        self.demod(t, threshold, LONG_BYTES);
        let mut cw = [0u8; LONG_BYTES];
        cw.copy_from_slice(&self.raw[..LONG_BYTES]);
        if let Ok(errors) = rs::LONG.decode(&mut cw)
            && cw[0] >> 3 != 0
        {
            self.payload[..34].copy_from_slice(&cw[..34]);
            return Some(Found::Adsb { len: 34, errors });
        }
        let mut cw = [0u8; SHORT_BYTES];
        cw.copy_from_slice(&self.raw[..SHORT_BYTES]);
        if let Ok(errors) = rs::SHORT.decode(&mut cw)
            && cw[0] >> 3 == 0
        {
            self.payload[..18].copy_from_slice(&cw[..18]);
            return Some(Found::Adsb { len: 18, errors });
        }
        None
    }

    fn try_uplink(&mut self, t: f64, threshold: f32) -> Option<Found> {
        self.demod(t, threshold, UPLINK_BYTES);
        let mut errors = 0;
        for block in 0..6 {
            let mut cw = [0u8; UPLINK_BLOCK];
            for (i, c) in cw.iter_mut().enumerate() {
                *c = self.raw[i * 6 + block];
            }
            errors += rs::UPLINK.decode(&mut cw).ok()?;
            self.payload[block * UPLINK_DATA..(block + 1) * UPLINK_DATA]
                .copy_from_slice(&cw[..UPLINK_DATA]);
        }
        Some(Found::Uplink { errors })
    }

    /// Try to read a frame whose sync word starts near sample `n`.
    fn frame_at(&mut self, n: usize, word: u64) -> Option<(Found, f64)> {
        // Best timing within half a bit, to a quarter sample.
        let half = (self.sps / 2.0 / 0.25).floor() as i32;
        let mut best = (f32::NEG_INFINITY, n as f64, 0.0f32);
        for k in -half..=half {
            let t = n as f64 + f64::from(k) * 0.25;
            if t < 0.0 {
                continue;
            }
            let (c, one, zero) = self.correlate(t, word);
            if c > best.0 {
                best = (c, t, (one + zero) / 2.0);
            }
        }
        let (c, t0, threshold) = best;
        if c <= 0.0 {
            return None;
        }
        // Recheck the sync word against the threshold found.
        let wrong = (0..SYNC_BITS)
            .filter(|&k| {
                let bit = self.at(t0 + k as f64 * self.sps) > threshold;
                bit != (word >> (SYNC_BITS - 1 - k) & 1 == 1)
            })
            .count();
        if wrong > SYNC_ERRORS as usize {
            return None;
        }
        for t in [t0, t0 - 0.5, t0 + 0.5] {
            if t < 0.0 {
                continue;
            }
            let found = if word == ADSB_SYNC {
                self.try_adsb(t, threshold)
            } else {
                self.try_uplink(t, threshold)
            };
            if let Some(f) = found {
                return Some((f, t));
            }
        }
        None
    }

    fn report(&mut self, found: Found, t: f64) {
        let at = (self.base as f64 + t) / self.rate;
        let mut line = Line(&mut self.queue);
        let _ = write!(line, "t={at:.6}");
        match found {
            Found::Adsb { len, errors } => {
                let m = AdsbMessage(&self.payload[..len]);
                let _ = frame::format_adsb(&m, &mut line);
                let _ = write!(line, " rs_errors={errors} raw=");
                for b in &self.payload[..len] {
                    let _ = write!(line, "{b:02X}");
                }
                self.queue.line(format_args!(""));
            }
            Found::Uplink { errors } => {
                let up = Uplink(&self.payload);
                let _ = write!(line, " type=uplink");
                if let Some((lat, lon)) = up.position() {
                    let _ = write!(line, " lat={lat:.5} lon={lon:.5}");
                }
                let _ = write!(
                    line,
                    " slot={} tisb_site={} utc={}",
                    up.slot(),
                    up.tisb_site(),
                    u8::from(up.utc_coupled())
                );
                let (mut apdus, mut text, mut nexrad, mut other, mut other_frames) =
                    (0, 0, 0, 0, 0);
                if up.app_data_valid() {
                    for f in up.frames() {
                        match f {
                            InfoFrame::Fisb(a) => {
                                apdus += 1;
                                match frame::product_kind(a.product) {
                                    "nexrad" => nexrad += 1,
                                    "text" => text += 1,
                                    _ => other += 1,
                                }
                            }
                            InfoFrame::Other(..) => other_frames += 1,
                        }
                    }
                }
                let _ = write!(
                    line,
                    " apdus={apdus} text={text} nexrad={nexrad} other={other} \
                     other_frames={other_frames} rs_errors={errors}"
                );
                self.queue.line(format_args!(""));
                if up.app_data_valid() {
                    self.report_text(at);
                }
            }
        }
    }

    /// One line per text record of the uplink's FIS-B products.
    fn report_text(&mut self, at: f64) {
        let up = Uplink(&self.payload);
        let queue = &mut self.queue;
        let text = &mut self.text;
        for f in up.frames() {
            let InfoFrame::Fisb(a) = f else { continue };
            if a.segmented {
                continue;
            }
            frame::text_records(&a, text, |rec| {
                queue.text(format_args!(
                    "t={at:.6} type=fisb product={} time={:02}:{:02}",
                    a.product, a.hour, a.minute
                ));
                if let (Some(m), Some(d)) = (a.month, a.day) {
                    queue.text(format_args!(" date={m:02}-{d:02}"));
                }
                queue.line(format_args!(" text=\"{}\"", frame::escaped(rec)));
            });
        }
    }

    /// Search `buf` from `scan` until a frame is queued or too few samples
    /// remain. At the end of the input, frames shorter than an uplink are
    /// still looked for in the tail.
    fn search(&mut self, at_end: bool) {
        let need = if at_end { self.adsb_span } else { self.span };
        while self.scan + need <= self.buf.len() {
            let n = self.scan;
            if let Some(word) = self.sync_at(n)
                && (word == ADSB_SYNC || n + self.span <= self.buf.len())
                && let Some((found, t)) = self.frame_at(n, word)
            {
                let bits = match found {
                    Found::Adsb { len: 34, .. } => LONG_BYTES * 8,
                    Found::Adsb { .. } => SHORT_BYTES * 8,
                    Found::Uplink { .. } => UPLINK_BYTES * 8,
                };
                self.scan = (t + (SYNC_BITS + bits) as f64 * self.sps) as usize;
                self.report(found, t);
                return;
            }
            self.scan += 1;
        }
    }
}

/// `fmt::Write` into the message queue.
struct Line<'a>(&'a mut MsgQueue);

impl std::fmt::Write for Line<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.bytes(s.as_bytes());
        Ok(())
    }
}

impl Block for UatDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs[0].sample_rate.unwrap_or(MIN_RATE);
        if rate < MIN_RATE * 0.999 {
            return Err(Error::Other(format!(
                "needs at least 2.083 MS/s (two samples per UAT bit) but gets {}; set the \
                 receiver to 2.083333 MS/s or more",
                crate::design::fmt_rate(rate)
            )));
        }
        self.set_rate(rate);
        text_meta(cx, 0);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (mut used, mut made) = (0, 0);
        loop {
            let out = io.outputs[0].slice_mut::<u8>();
            made += self.queue.flush(&mut out[made..]);
            if !self.queue.is_empty() {
                break;
            }
            let inp_len = io.inputs[0].slice::<f32>().len();
            let at_end = io.inputs[0].exhausted() && used == inp_len;
            self.search(at_end);
            if !self.queue.is_empty() {
                continue;
            }
            // Not enough samples left for a frame: keep the tail, read more.
            let inp = io.inputs[0].slice::<f32>();
            let room = self.buf.capacity() - self.buf.len() + self.scan;
            let take = (inp.len() - used).min(room);
            if take == 0 {
                break;
            }
            self.buf.drain(..self.scan);
            self.base += self.scan as u64;
            self.scan = 0;
            self.buf.extend_from_slice(&inp[used..used + take]);
            used += take;
        }
        if used == 0 && made == 0 {
            return Ok(idle_status(io, &self.queue, 0));
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build(_: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(UatDecoder::new()),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}
