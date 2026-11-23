//! ADS-B / Mode S extended squitter (1090 MHz) decoder.
//!
//! Input: the magnitude of 1090 MHz baseband at 2 MS/s, i.e. two samples per
//! 1 µs bit. A Mode S reply is an 8 µs preamble (pulses at 0, 1, 3.5 and
//! 4.5 µs) followed by 56 or 112 bits of pulse-position modulation: a pulse in
//! the first half of the bit period is a 1, in the second half a 0. Extended
//! squitters (DF17 from transponders, DF18 from other emitters) carry a
//! 24-bit CRC over the whole message and are decoded here; other downlink
//! formats are ignored.

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use super::{MsgQueue, idle_status, text_meta};

/// The sample rate the decoder is built for.
pub const RATE: f64 = 2e6;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "adsb_decoder",
            label: "ADS-B Decoder",
            category: "Digital",
            help: "Decodes ADS-B (Mode S extended squitter, DF17/18) from the magnitude of a \
                   1090 MHz signal sampled at 2 MS/s: preamble detection, PPM bits, CRC-24 \
                   (optionally repairing one wrong bit), then aircraft address, call sign, \
                   altitude, velocity and position (from an even/odd CPR pair). One line of \
                   text per message, e.g. `t=1.234567 icao=4840D6 df=17 tc=4 type=ident \
                   callsign=KLM1023 raw=…`; connect it to a Message Log and watch the wire \
                   with the Messages or Map instrument.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![ParamSpec {
                name: "fix_errors",
                label: "Fix 1-bit errors",
                kind: ParamKind::Bool,
                default: json!(true),
                help: "Repair messages with one wrong bit (found from the CRC). Decodes more \
                       weak messages, at a small risk of accepting a corrupted one.",
                live: true,
            }],
        },
        build,
    );
}

// ---- Mode S frames -------------------------------------------------------------

const POLY: u32 = 0xFF_F409;

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 16;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80_0000 != 0 {
                (c << 1) ^ POLY
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c & 0xFF_FFFF;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

fn crc24(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &b| {
        ((crc << 8) ^ CRC_TABLE[usize::from((crc >> 16) as u8 ^ b)]) & 0xFF_FFFF
    })
}

/// The CRC of the first 11 bytes XOR the parity field: 0 for an intact
/// extended squitter.
pub fn crc_residual(bytes: &[u8; 14]) -> u32 {
    let parity = u32::from(bytes[11]) << 16 | u32::from(bytes[12]) << 8 | u32::from(bytes[13]);
    crc24(&bytes[..11]) ^ parity
}

/// A CPR-encoded position (17-bit latitude and longitude).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cpr {
    pub odd: bool,
    pub lat: u32,
    pub lon: u32,
}

/// Ground velocity or airspeed (TC 19).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Velocity {
    Ground {
        speed_kt: f64,
        track_deg: f64,
        vrate_fpm: Option<i32>,
    },
    Air {
        speed_kt: f64,
        true_airspeed: bool,
        heading_deg: Option<f64>,
        vrate_fpm: Option<i32>,
    },
}

/// A 112-bit Mode S message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeS {
    pub bytes: [u8; 14],
}

impl ModeS {
    /// From 28 hex digits.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 28 {
            return None;
        }
        let mut bytes = [0u8; 14];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Self { bytes })
    }

    pub fn df(&self) -> u8 {
        self.bytes[0] >> 3
    }

    pub fn icao(&self) -> u32 {
        u32::from(self.bytes[1]) << 16 | u32::from(self.bytes[2]) << 8 | u32::from(self.bytes[3])
    }

    /// The 56-bit ME field.
    fn me(&self) -> u64 {
        self.bytes[4..11]
            .iter()
            .fold(0u64, |v, &b| v << 8 | u64::from(b))
    }

    /// `len` bits of the ME field starting at bit `start` (0 = first).
    fn bits(&self, start: u32, len: u32) -> u32 {
        ((self.me() >> (56 - start - len)) & ((1 << len) - 1)) as u32
    }

    /// Type code.
    pub fn tc(&self) -> u8 {
        self.bytes[4] >> 3
    }

    /// The call sign of an identification message (TC 1–4), spaces trimmed.
    pub fn callsign(&self) -> Option<CallSign> {
        if !(1..=4).contains(&self.tc()) {
            return None;
        }
        const CHARS: &[u8; 64] =
            b"?ABCDEFGHIJKLMNOPQRSTUVWXYZ????? ???????????????0123456789??????";
        let mut cs = CallSign([b' '; 8]);
        for (i, c) in cs.0.iter_mut().enumerate() {
            *c = CHARS[self.bits(8 + 6 * i as u32, 6) as usize];
        }
        Some(cs)
    }

    /// Barometric altitude of an airborne position (TC 9–18), in feet.
    /// `None` when unavailable or in the 100 ft Gillham code.
    pub fn altitude_ft(&self) -> Option<i32> {
        if !(9..=18).contains(&self.tc()) {
            return None;
        }
        let a = self.bits(8, 12);
        if a == 0 || a & 0x10 == 0 {
            return None;
        }
        let n = ((a & 0xFE0) >> 1) | (a & 0xF);
        Some(n as i32 * 25 - 1000)
    }

    /// The CPR position of an airborne position message (TC 9–18, 20–22).
    pub fn cpr(&self) -> Option<Cpr> {
        let tc = self.tc();
        if !((9..=18).contains(&tc) || (20..=22).contains(&tc)) {
            return None;
        }
        Some(Cpr {
            odd: self.bits(21, 1) == 1,
            lat: self.bits(22, 17),
            lon: self.bits(39, 17),
        })
    }

    /// Velocity (TC 19, subtypes 1–4).
    pub fn velocity(&self) -> Option<Velocity> {
        if self.tc() != 19 {
            return None;
        }
        let st = self.bits(5, 3);
        let vr = self.bits(37, 9);
        let vrate_fpm = (vr != 0).then(|| {
            let v = (vr as i32 - 1) * 64;
            if self.bits(36, 1) == 1 { -v } else { v }
        });
        let scale = if st == 2 || st == 4 { 4.0 } else { 1.0 };
        match st {
            1 | 2 => {
                let (ew, ns) = (self.bits(14, 10), self.bits(25, 10));
                if ew == 0 || ns == 0 {
                    return None;
                }
                let sign = |neg: u32, v: u32| {
                    let v = f64::from(v - 1) * scale;
                    if neg == 1 { -v } else { v }
                };
                let vx = sign(self.bits(13, 1), ew);
                let vy = sign(self.bits(24, 1), ns);
                Some(Velocity::Ground {
                    speed_kt: vx.hypot(vy),
                    track_deg: vx.atan2(vy).to_degrees().rem_euclid(360.0),
                    vrate_fpm,
                })
            }
            3 | 4 => {
                let speed = self.bits(25, 10);
                if speed == 0 {
                    return None;
                }
                Some(Velocity::Air {
                    speed_kt: f64::from(speed - 1) * scale,
                    true_airspeed: self.bits(24, 1) == 1,
                    heading_deg: (self.bits(13, 1) == 1)
                        .then(|| f64::from(self.bits(14, 10)) * 360.0 / 1024.0),
                    vrate_fpm,
                })
            }
            _ => None,
        }
    }
}

/// An eight-character call sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallSign(pub [u8; 8]);

impl CallSign {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("").trim_matches(' ')
    }
}

/// Number of longitude zones at a latitude (the CPR `NL` function).
fn nl(lat: f64) -> u32 {
    let lat = lat.abs();
    if lat < 1e-9 {
        return 59;
    }
    if lat >= 87.0 {
        return if lat > 87.0 { 1 } else { 2 };
    }
    let nz = 15.0;
    let a = 1.0 - (std::f64::consts::PI / (2.0 * nz)).cos();
    let b = lat.to_radians().cos().powi(2);
    (std::f64::consts::TAU / (1.0 - a / b).acos()).floor() as u32
}

/// Globally unambiguous airborne position from an even and an odd CPR frame
/// (sent within ~10 s of each other). `odd_newest` picks which one the
/// position is for. `None` if the two straddle a longitude-zone boundary.
pub fn cpr_global(even: Cpr, odd: Cpr, odd_newest: bool) -> Option<(f64, f64)> {
    const SCALE: f64 = 131_072.0; // 2^17
    let (lat_e, lon_e) = (f64::from(even.lat) / SCALE, f64::from(even.lon) / SCALE);
    let (lat_o, lon_o) = (f64::from(odd.lat) / SCALE, f64::from(odd.lon) / SCALE);
    let j = (59.0 * lat_e - 60.0 * lat_o + 0.5).floor();
    let fix = |lat: f64| if lat >= 270.0 { lat - 360.0 } else { lat };
    let lat_even = fix(360.0 / 60.0 * (j.rem_euclid(60.0) + lat_e));
    let lat_odd = fix(360.0 / 59.0 * (j.rem_euclid(59.0) + lat_o));
    if nl(lat_even) != nl(lat_odd) {
        return None;
    }
    let (lat, l) = if odd_newest {
        (lat_odd, nl(lat_odd))
    } else {
        (lat_even, nl(lat_even))
    };
    let ni = if odd_newest { l.saturating_sub(1) } else { l }.max(1);
    let m = (lon_e * f64::from(l - 1) - lon_o * f64::from(l) + 0.5).floor();
    let own = if odd_newest { lon_o } else { lon_e };
    let mut lon = 360.0 / f64::from(ni) * (m.rem_euclid(f64::from(ni)) + own);
    if lon >= 180.0 {
        lon -= 360.0;
    }
    Some((lat, lon))
}

// ---- Demodulation --------------------------------------------------------------

/// Samples in a 112-bit frame including the preamble.
const FRAME: usize = 16 + 2 * 112;
/// Samples copied in per pass.
const CHUNK: usize = 16 * 1024;

/// If a Mode S preamble starts at `m[0]`, the 112 bits after it, provided
/// the downlink format is 17 or 18 (extended squitter). `m` holds at least
/// [`FRAME`] samples.
#[inline]
fn detect(m: &[f32]) -> Option<[u8; 14]> {
    let m = &m[..FRAME];
    // Pulses at samples 0, 2, 7 and 9, nothing in between (as dump1090).
    if !(m[0] > m[1]
        && m[1] < m[2]
        && m[2] > m[3]
        && m[3] < m[0]
        && m[4] < m[0]
        && m[5] < m[0]
        && m[6] < m[0]
        && m[7] > m[8]
        && m[8] < m[9]
        && m[9] > m[6])
    {
        return None;
    }
    let high = (m[0] + m[2] + m[7] + m[9]) / 6.0;
    if m[4] >= high || m[5] >= high || m[11..15].iter().any(|&v| v >= high) {
        return None;
    }
    let bits = &m[16..];
    let byte = |k: usize| {
        (0..8).fold(0u8, |acc, i| {
            let b = 2 * (8 * k + i);
            acc << 1 | u8::from(bits[b] > bits[b + 1])
        })
    };
    let first = byte(0);
    if !matches!(first >> 3, 17 | 18) {
        return None;
    }
    let mut out = [0u8; 14];
    out[0] = first;
    for (k, o) in out.iter_mut().enumerate().skip(1) {
        *o = byte(k);
    }
    Some(out)
}

/// The CRC residual a single wrong bit at each position causes.
fn syndromes() -> [u32; 112] {
    let mut s = [0u32; 112];
    for (i, v) in s.iter_mut().enumerate() {
        let mut b = [0u8; 14];
        b[i / 8] = 0x80 >> (i % 8);
        *v = crc_residual(&b);
    }
    s
}

/// One aircraft's last CPR frames, for global position decoding.
#[derive(Debug, Clone, Copy)]
struct Aircraft {
    icao: u32,
    seen: f64,
    even: Option<(Cpr, f64)>,
    odd: Option<(Cpr, f64)>,
}

/// Aircraft remembered at once; the one heard least recently is replaced.
const MAX_AIRCRAFT: usize = 512;
/// An even and an odd frame further apart than this aren't paired.
const CPR_PAIR_S: f64 = 10.0;

pub struct AdsbDecoder {
    fix: bool,
    rate: f64,
    syndromes: [u32; 112],
    /// Samples being searched; `buf[0]` is stream sample `base`.
    buf: Vec<f32>,
    base: u64,
    /// Next start position in `buf` to test for a preamble.
    scan: usize,
    aircraft: Vec<Aircraft>,
    queue: MsgQueue,
}

impl AdsbDecoder {
    pub fn new(fix: bool) -> Self {
        Self {
            fix,
            rate: RATE,
            syndromes: syndromes(),
            buf: Vec::with_capacity(CHUNK + FRAME),
            base: 0,
            scan: 0,
            aircraft: Vec::with_capacity(64),
            queue: MsgQueue::new(),
        }
    }

    /// Check the CRC, repairing one bit if allowed. Returns the repaired bit.
    fn check(&self, bytes: &mut [u8; 14]) -> Result<Option<usize>, ()> {
        let r = crc_residual(bytes);
        if r == 0 {
            return Ok(None);
        }
        if self.fix {
            // Never "repair" the DF field: it decided the message length.
            if let Some(i) = (5..112).find(|&i| self.syndromes[i] == r) {
                bytes[i / 8] ^= 0x80 >> (i % 8);
                return Ok(Some(i));
            }
        }
        Err(())
    }

    fn aircraft(&mut self, icao: u32, t: f64) -> &mut Aircraft {
        let i = match self.aircraft.iter().position(|a| a.icao == icao) {
            Some(i) => i,
            None => {
                let fresh = Aircraft {
                    icao,
                    seen: t,
                    even: None,
                    odd: None,
                };
                if self.aircraft.len() < MAX_AIRCRAFT {
                    self.aircraft.push(fresh);
                    self.aircraft.len() - 1
                } else {
                    let (i, _) = self
                        .aircraft
                        .iter()
                        .enumerate()
                        .min_by(|a, b| a.1.seen.total_cmp(&b.1.seen))
                        .expect("not empty");
                    self.aircraft[i] = fresh;
                    i
                }
            }
        };
        let a = &mut self.aircraft[i];
        a.seen = t;
        a
    }

    /// Format one decoded message into the queue.
    fn report(&mut self, msg: ModeS, fixed: Option<usize>, t: f64) {
        let icao = msg.icao();
        let tc = msg.tc();
        self.queue.text(format_args!(
            "t={t:.6} icao={icao:06X} df={} tc={tc}",
            msg.df()
        ));
        if let Some(cs) = msg.callsign() {
            self.queue
                .text(format_args!(" type=ident callsign={}", cs.as_str()));
        } else if let Some(cpr) = msg.cpr() {
            self.queue.text(format_args!(" type=position"));
            if let Some(alt) = msg.altitude_ft() {
                self.queue.text(format_args!(" alt_ft={alt}"));
            }
            let a = self.aircraft(icao, t);
            let other = if cpr.odd {
                a.odd = Some((cpr, t));
                a.even
            } else {
                a.even = Some((cpr, t));
                a.odd
            };
            let pos = other
                .filter(|(_, at)| (t - at).abs() <= CPR_PAIR_S)
                .and_then(|(o, _)| {
                    let (even, odd) = if cpr.odd { (o, cpr) } else { (cpr, o) };
                    cpr_global(even, odd, cpr.odd)
                });
            match pos {
                Some((lat, lon)) => self.queue.text(format_args!(" lat={lat:.5} lon={lon:.5}")),
                None => self.queue.text(format_args!(
                    " cpr={}",
                    if cpr.odd { "odd" } else { "even" }
                )),
            }
        } else if let Some(v) = msg.velocity() {
            let vr = match v {
                Velocity::Ground {
                    speed_kt,
                    track_deg,
                    vrate_fpm,
                } => {
                    self.queue.text(format_args!(
                        " type=velocity gs_kt={speed_kt:.1} track_deg={track_deg:.1}"
                    ));
                    vrate_fpm
                }
                Velocity::Air {
                    speed_kt,
                    true_airspeed,
                    heading_deg,
                    vrate_fpm,
                } => {
                    let kind = if true_airspeed { "tas" } else { "ias" };
                    self.queue
                        .text(format_args!(" type=velocity {kind}_kt={speed_kt:.0}"));
                    if let Some(h) = heading_deg {
                        self.queue.text(format_args!(" heading_deg={h:.1}"));
                    }
                    vrate_fpm
                }
            };
            if let Some(vr) = vr {
                self.queue.text(format_args!(" vrate_fpm={vr}"));
            }
        } else if (5..=8).contains(&tc) {
            self.queue.text(format_args!(" type=surface_position"));
        }
        if let Some(bit) = fixed {
            self.queue.text(format_args!(" fixed_bit={bit}"));
        }
        self.queue.text(format_args!(" raw="));
        for b in msg.bytes {
            self.queue.text(format_args!("{b:02X}"));
        }
        self.queue.line(format_args!(""));
    }

    /// Search `buf` from `scan` until a message is queued or too few samples
    /// remain.
    fn search(&mut self) {
        while self.scan + FRAME <= self.buf.len() {
            let Some(mut bytes) = detect(&self.buf[self.scan..]) else {
                self.scan += 1;
                continue;
            };
            let Ok(fixed) = self.check(&mut bytes) else {
                self.scan += 1;
                continue;
            };
            let t = (self.base + self.scan as u64) as f64 / self.rate;
            self.scan += FRAME;
            self.report(ModeS { bytes }, fixed, t);
            return;
        }
    }
}

impl Block for AdsbDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(rate) = cx.inputs[0].sample_rate {
            if (rate - RATE).abs() > RATE * 1e-3 {
                return Err(Error::Other(format!(
                    "needs 2 MS/s (two samples per Mode S bit) but gets {}; resample or set \
                     the receiver to 2 MS/s",
                    crate::design::fmt_rate(rate)
                )));
            }
            self.rate = rate;
        }
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
            self.search();
            if !self.queue.is_empty() {
                continue;
            }
            // Not enough samples left for a frame: keep the tail, read more.
            let inp = io.inputs[0].slice::<f32>();
            let take = (inp.len() - used).min(CHUNK + FRAME - self.buf.len() + self.scan);
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

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "fix_errors" => {
                self.fix = value.as_bool().unwrap_or(true);
                Ok(())
            }
            _ => Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(AdsbDecoder::new(p.bool("fix_errors")?)),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}

#[cfg(test)]
mod tests {
    use foxygnal_core::{BufferKind, Flowgraph, StreamMeta};
    use serde_json::json;

    use super::*;
    use crate::testing::{VectorSink, VectorSource};

    const KLM: &str = "8D4840D6202CC371C32CE0576098";
    const EVEN: &str = "8D40621D58C382D690C8AC2863A7";
    const ODD: &str = "8D40621D58C386435CC412692AD6";
    const VELOCITY: &str = "8D485020994409940838175B284F";
    const AIRSPEED: &str = "8DA05F219B06B6AF189400CBC33F";

    fn msg(hex: &str) -> ModeS {
        ModeS::from_hex(hex).unwrap()
    }

    #[test]
    fn crc_accepts_real_messages_and_rejects_damage() {
        for hex in [KLM, EVEN, ODD, VELOCITY, AIRSPEED] {
            assert_eq!(crc_residual(&msg(hex).bytes), 0, "{hex}");
        }
        let mut m = msg(KLM);
        m.bytes[6] ^= 0x10;
        assert_ne!(crc_residual(&m.bytes), 0);
    }

    #[test]
    fn decodes_identification() {
        let m = msg(KLM);
        assert_eq!((m.df(), m.icao(), m.tc()), (17, 0x4840D6, 4));
        assert_eq!(m.callsign().unwrap().as_str(), "KLM1023");
        assert_eq!(m.cpr(), None);
    }

    #[test]
    fn decodes_altitude_and_global_cpr_position() {
        let (e, o) = (msg(EVEN), msg(ODD));
        assert_eq!(e.altitude_ft(), Some(38_000));
        let (ce, co) = (e.cpr().unwrap(), o.cpr().unwrap());
        assert!(!ce.odd && co.odd);
        // "The 1090 MHz Riddle": the even frame is the newer one.
        let (lat, lon) = cpr_global(ce, co, false).unwrap();
        assert!((lat - 52.25720).abs() < 1e-4, "{lat}");
        assert!((lon - 3.91937).abs() < 1e-4, "{lon}");
        let (lat, _) = cpr_global(ce, co, true).unwrap();
        assert!((lat - 52.26578).abs() < 1e-4, "{lat}");
    }

    #[test]
    fn decodes_velocities() {
        match msg(VELOCITY).velocity().unwrap() {
            Velocity::Ground {
                speed_kt,
                track_deg,
                vrate_fpm,
            } => {
                assert!((speed_kt - 159.2).abs() < 0.1, "{speed_kt}");
                assert!((track_deg - 182.88).abs() < 0.01, "{track_deg}");
                assert_eq!(vrate_fpm, Some(-832));
            }
            v => panic!("{v:?}"),
        }
        match msg(AIRSPEED).velocity().unwrap() {
            Velocity::Air {
                speed_kt,
                true_airspeed,
                heading_deg,
                vrate_fpm,
            } => {
                assert_eq!(speed_kt, 375.0);
                assert!(true_airspeed);
                assert!((heading_deg.unwrap() - 243.98).abs() < 0.01);
                assert_eq!(vrate_fpm, Some(-2304));
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn nl_matches_the_table() {
        assert_eq!(nl(0.0), 59);
        assert_eq!(nl(52.2572), 36);
        assert_eq!(nl(-52.2572), 36);
        assert_eq!(nl(87.0), 2);
        assert_eq!(nl(89.0), 1);
    }

    /// Small deterministic Gaussian noise (xorshift + Box-Muller).
    struct Noise(u64);
    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let (u, v) = (self.uniform(), self.uniform());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    /// 2 MS/s magnitude samples of `frames` (each after `gap` samples of
    /// noise), as a receiver would see them at amplitude `amp` over complex
    /// noise of standard deviation `sigma` per axis.
    fn modulate(frames: &[[u8; 14]], gap: usize, amp: f64, sigma: f64, seed: u64) -> Vec<f32> {
        let mut clean = Vec::new();
        for f in frames {
            clean.extend(std::iter::repeat_n(0.0, gap));
            let mut pre = [0.0; 16];
            for i in [0, 2, 7, 9] {
                pre[i] = amp;
            }
            clean.extend(pre);
            for i in 0..112 {
                let one = f[i / 8] & (0x80 >> (i % 8)) != 0;
                clean.extend(if one { [amp, 0.0] } else { [0.0, amp] });
            }
        }
        clean.extend(std::iter::repeat_n(0.0, gap));
        let mut n = Noise(seed);
        clean
            .iter()
            .map(|&s| {
                let (i, q) = (s + sigma * n.gauss(), sigma * n.gauss());
                i.hypot(q) as f32
            })
            .collect()
    }

    fn decode(input: Vec<f32>, buffers: BufferKind, cap: usize) -> Vec<String> {
        let reg = crate::registry();
        let mut fg = Flowgraph::new()
            .with_buffer_capacity(cap)
            .with_buffer_kind(buffers);
        let src = fg.add(
            "src",
            VectorSource::instance_with_meta(input, StreamMeta::with_rate(RATE)),
        );
        let dec = fg.add(
            "adsb",
            reg.build("adsb_decoder", json!({}).as_object().unwrap())
                .unwrap(),
        );
        let (sink, out) = VectorSink::<u8>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(src, 0, dec, 0).unwrap();
        fg.connect(dec, 0, sink, 0).unwrap();
        let edges = fg.configure().unwrap();
        let m = edges.iter().find(|e| e.from == "adsb.out").unwrap().meta;
        assert_eq!(m.sample_rate, None, "text has no sample rate");
        fg.start().unwrap().wait().unwrap();
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn decodes_modulated_messages_end_to_end() {
        let frames = [msg(KLM), msg(ODD), msg(EVEN), msg(VELOCITY)].map(|m| m.bytes);
        // 20 dB SNR (amplitude 1, noise 0.1 per axis).
        let input = modulate(&frames, 3000, 1.0, 0.1, 7);
        for (kind, cap) in [(BufferKind::Auto, 64 * 1024), (BufferKind::Plain, 100)] {
            let lines = decode(input.clone(), kind, cap);
            assert_eq!(lines.len(), 4, "{lines:#?}");
            assert!(lines[0].starts_with("t=0.001500 icao=4840D6 df=17 tc=4 type=ident"));
            assert!(lines[0].contains("callsign=KLM1023"), "{}", lines[0]);
            assert!(lines[0].ends_with(&format!("raw={KLM}")), "{}", lines[0]);
            assert!(lines[1].contains("icao=40621D") && lines[1].contains("cpr=odd"));
            assert!(lines[1].contains("alt_ft=38000"), "{}", lines[1]);
            assert!(
                lines[2].contains("lat=52.25720 lon=3.91937"),
                "{}",
                lines[2]
            );
            assert!(
                lines[3].contains("type=velocity gs_kt=159.2 track_deg=182.9 vrate_fpm=-832"),
                "{}",
                lines[3]
            );
        }
    }

    #[test]
    fn repairs_one_bit_and_drops_two() {
        let mut one = msg(KLM).bytes;
        one[7] ^= 0x04; // bit 61
        let mut two = msg(KLM).bytes;
        two[7] ^= 0x04;
        two[9] ^= 0x01;
        let input = modulate(&[one, two], 500, 1.0, 0.02, 3);
        let lines = decode(input, BufferKind::Auto, 64 * 1024);
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(
            lines[0].contains("callsign=KLM1023 fixed_bit=61"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn noise_alone_decodes_nothing() {
        let input = modulate(&[], 400_000, 1.0, 0.3, 11);
        assert!(decode(input, BufferKind::Auto, 64 * 1024).is_empty());
    }

    #[test]
    fn rejects_the_wrong_sample_rate() {
        let reg = crate::registry();
        let mut fg = Flowgraph::new();
        let src = fg.add(
            "src",
            VectorSource::<f32>::instance_with_meta(vec![], StreamMeta::with_rate(2.4e6)),
        );
        let dec = fg.add(
            "adsb",
            reg.build("adsb_decoder", json!({}).as_object().unwrap())
                .unwrap(),
        );
        fg.connect(src, 0, dec, 0).unwrap();
        let err = fg.configure().unwrap_err().to_string();
        assert!(
            err.contains("needs 2 MS/s") && err.contains("2.4 MS/s"),
            "{err}"
        );
    }
}
