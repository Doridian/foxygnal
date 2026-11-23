//! Mode S (1090 MHz) decoder: ADS-B extended squitters and the replies to
//! secondary radar interrogations.
//!
//! Input: the magnitude of 1090 MHz baseband at 2 MS/s, i.e. two samples per
//! 1 µs bit. A Mode S reply is an 8 µs preamble (pulses at 0, 1, 3.5 and
//! 4.5 µs) followed by 56 or 112 bits of pulse-position modulation: a pulse in
//! the first half of the bit period is a 1, in the second half a 0.
//!
//! Which messages are believed:
//!
//! - DF11 (all-call reply), DF17 (extended squitter) and DF18 (extended
//!   squitter from non-transponders, TIS-B, ADS-R) carry a plain CRC. One
//!   wrong bit (optionally two, DF17/18 only) is repaired from the syndrome,
//!   as dump1090 does. A DF11 to a numbered interrogator has the interrogator
//!   code in the parity and is accepted only from an aircraft already heard.
//! - DF0/4/5/16/20/21 have address/parity: the CRC XOR the parity *is* the
//!   address, so any garbage "decodes" to some address. Like dump1090's ICAO
//!   filter, such a reply is accepted only if its address was heard in an
//!   intact DF11 or DF17 in the last minute. The table of addresses is fixed
//!   in size (open addressing, oldest entry replaced), so nothing allocates
//!   per message.
//! - Mode A/C replies (no CRC, no address) are decoded only when asked for;
//!   see [`modeac`].

mod commb;
mod cpr;
mod demod;
mod modeac;
mod msg;
#[cfg(test)]
mod tests;

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

pub use commb::CommB;
pub use cpr::{cpr_global, cpr_global_surface, cpr_local};
pub use msg::{Cpr, ModeS, Velocity, ac13_ft, gillham_ft, id13_squawk};

use super::{MsgQueue, idle_status, text_meta};
use demod::{Signal, Timing};

/// The lowest sample rate the decoder works at (two samples per bit), and
/// the rate assumed when the input doesn't say.
pub const RATE: f64 = 2e6;

fn bool_param(
    name: &'static str,
    label: &'static str,
    default: bool,
    help: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        kind: ParamKind::Bool,
        default: json!(default),
        help,
        live: true,
    }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "adsb_decoder",
            label: "ADS-B / Mode S Decoder",
            category: "Digital",
            help: "Decodes Mode S from the magnitude of a 1090 MHz signal sampled at 2 MS/s: \
                   ADS-B extended squitters (DF17/18: identification, airborne and surface \
                   positions, velocities, aircraft and target state, operational status, \
                   TIS-B and ADS-R), all-call replies (DF11), and altitude, identity and \
                   Comm-B replies (DF0/4/5/16/20/21, including BDS 1,0 1,7 2,0 3,0 4,0 5,0 \
                   6,0), whose address is checked against aircraft heard recently. CRC-24 \
                   repairs one wrong bit (optionally two); positions come from even/odd CPR \
                   pairs or, with a receiver position, from single frames. One line of text \
                   per message, e.g. `t=1.234567 icao=4840D6 df=17 tc=4 type=ident \
                   callsign=KLM1023 category=A0 raw=…`; connect it to a Message Log and watch \
                   the wire with the Messages or Map instrument.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::U8),
            }],
            params: vec![
                bool_param(
                    "fix_errors",
                    "Fix 1-bit errors",
                    true,
                    "Repair DF11/17/18 messages with one wrong bit (found from the CRC). \
                     Decodes more weak messages, at a small risk of accepting a corrupted one.",
                ),
                bool_param(
                    "fix_two_bits",
                    "Fix 2-bit errors",
                    false,
                    "Also repair DF17/18 messages with two wrong bits, from aircraft already \
                     heard. More messages, more risk.",
                ),
                ParamSpec {
                    name: "formats",
                    label: "Formats",
                    kind: ParamKind::Enum {
                        options: vec!["all", "extended_squitter"],
                    },
                    default: json!("all"),
                    help: "`all` decodes every Mode S format listed above; \
                           `extended_squitter` only DF17/18 (ADS-B, TIS-B, ADS-R).",
                    live: true,
                },
                bool_param(
                    "mode_ac",
                    "Mode A/C",
                    false,
                    "Also decode Mode A/C replies (squawk or altitude, no address). They have \
                     no checksum, so expect some false ones in noise.",
                ),
                ParamSpec {
                    name: "receiver",
                    label: "Receiver position",
                    kind: ParamKind::String,
                    default: json!(""),
                    help: "`lat, lon` in degrees (e.g. `52.0, 4.4`). Lets single position \
                           frames be decoded (local CPR), surface positions be placed, and \
                           drops positions farther than 300 NM. Empty = unknown.",
                    live: false,
                },
            ],
        },
        build,
    );
}

/// `"lat, lon"`, or `None` for an empty string.
fn parse_position(s: &str) -> Result<Option<(f64, f64)>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let bad = || format!("expected `lat, lon` in degrees, got `{s}`");
    let (a, b) = s.split_once(',').ok_or_else(bad)?;
    let lat: f64 = a.trim().parse().map_err(|_| bad())?;
    let lon: f64 = b.trim().parse().map_err(|_| bad())?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(bad());
    }
    Ok(Some((lat, lon)))
}

// ---- Demodulation --------------------------------------------------------------

/// Samples copied in per pass.
const CHUNK: usize = 16 * 1024;
/// Time constant of the noise floor estimate, in samples.
const NOISE_SAMPLES: f64 = 65_536.0;

/// Whether downlink format `df` is decoded.
fn wanted(df: u8, all: bool) -> bool {
    if all {
        matches!(df, 0 | 4 | 5 | 11 | 16 | 17 | 18 | 20 | 21)
    } else {
        matches!(df, 17 | 18)
    }
}

/// The CRC residual a single wrong bit at each position of a 112-bit
/// message causes. Bit `i` of a 56-bit message gives entry `56 + i`.
fn syndromes() -> [u32; 112] {
    let mut s = [0u32; 112];
    for (i, v) in s.iter_mut().enumerate() {
        let mut b = [0u8; 14];
        b[i / 8] = 0x80 >> (i % 8);
        *v = msg::crc_residual(&b);
    }
    s
}

/// Residuals of every two-bit error outside the DF field of a 112-bit
/// message, sorted for binary search, with the two positions. A residual
/// that two patterns share is left out.
fn two_bit_syndromes(one: &[u32; 112]) -> Vec<(u32, u8, u8)> {
    let mut v = Vec::with_capacity(107 * 106 / 2);
    for i in 5..112 {
        for j in i + 1..112 {
            v.push((one[i] ^ one[j], i as u8, j as u8));
        }
    }
    v.sort_unstable();
    let mut out: Vec<(u32, u8, u8)> = Vec::with_capacity(v.len());
    let mut k = 0;
    while k < v.len() {
        let mut e = k + 1;
        while e < v.len() && v[e].0 == v[k].0 {
            e += 1;
        }
        if e == k + 1 && !one.contains(&v[k].0) {
            out.push(v[k]);
        }
        k = e;
    }
    out
}

// ---- Aircraft table ------------------------------------------------------------

/// Slots in the aircraft table (a power of two).
const SLOTS: usize = 1024;
/// Slots tried per address.
const PROBE: usize = 16;
/// An address/parity reply is believed if its address sent an intact
/// DF11/17 this recently (seconds).
const VERIFIED_S: f64 = 60.0;
/// A slot unused this long is free.
const EXPIRE_S: f64 = 300.0;
/// An even and an odd airborne frame further apart than this aren't paired.
const CPR_PAIR_S: f64 = 10.0;
/// The same for surface frames (they're sent less often).
const CPR_PAIR_SURFACE_S: f64 = 25.0;
/// A position this recent is a reference for local decoding of the next.
const LOCAL_REF_S: f64 = 60.0;
/// Positions farther from the receiver than this are dropped (NM).
const MAX_RANGE_NM: f64 = 300.0;

#[derive(Debug, Clone, Copy)]
struct Aircraft {
    icao: u32,
    used: bool,
    seen: f64,
    /// Last intact DF11/17.
    verified: f64,
    even: Option<(Cpr, f64)>,
    odd: Option<(Cpr, f64)>,
    /// Last decoded position and when.
    pos: Option<(f64, f64, f64)>,
    alt: Option<(i32, f64)>,
    /// ADS-B ground speed and track, and when.
    vel: Option<(f64, f64, f64)>,
}

const EMPTY: Aircraft = Aircraft {
    icao: 0,
    used: false,
    seen: f64::NEG_INFINITY,
    verified: f64::NEG_INFINITY,
    even: None,
    odd: None,
    pos: None,
    alt: None,
    vel: None,
};

/// Open-addressing table of aircraft, fixed in size.
struct Table {
    slots: Box<[Aircraft]>,
}

impl Table {
    fn new() -> Self {
        Self {
            slots: vec![EMPTY; SLOTS].into_boxed_slice(),
        }
    }

    fn home(icao: u32) -> usize {
        (icao.wrapping_mul(0x9E37_79B1) >> 22) as usize % SLOTS
    }

    fn find(&self, icao: u32, t: f64) -> Option<usize> {
        let h = Self::home(icao);
        (0..PROBE).map(|k| (h + k) % SLOTS).find(|&i| {
            let a = &self.slots[i];
            a.used && a.icao == icao && t - a.seen < EXPIRE_S
        })
    }

    fn get(&mut self, icao: u32, t: f64) -> Option<&mut Aircraft> {
        self.find(icao, t).map(|i| &mut self.slots[i])
    }

    /// The aircraft's slot, made (replacing the stalest in its probe
    /// window) if needed.
    fn entry(&mut self, icao: u32, t: f64) -> &mut Aircraft {
        let i = match self.find(icao, t) {
            Some(i) => i,
            None => {
                let h = Self::home(icao);
                let i = (0..PROBE)
                    .map(|k| (h + k) % SLOTS)
                    .min_by(|&a, &b| {
                        let key = |i: usize| {
                            let s = &self.slots[i];
                            if !s.used || t - s.seen >= EXPIRE_S {
                                f64::NEG_INFINITY
                            } else {
                                s.seen
                            }
                        };
                        key(a).total_cmp(&key(b))
                    })
                    .expect("PROBE > 0");
                self.slots[i] = Aircraft {
                    icao,
                    used: true,
                    seen: t,
                    ..EMPTY
                };
                i
            }
        };
        let a = &mut self.slots[i];
        a.seen = t;
        a
    }

    fn verified(&self, icao: u32, t: f64) -> bool {
        self.find(icao, t)
            .is_some_and(|i| t - self.slots[i].verified <= VERIFIED_S)
    }
}

// ---- The decoder ---------------------------------------------------------------

/// How a message's CRC was made to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fix {
    None,
    One(usize),
    Two(usize, usize),
}

pub struct AdsbDecoder {
    fix: bool,
    fix_two: bool,
    all_formats: bool,
    mode_ac: bool,
    receiver: Option<(f64, f64)>,
    rate: f64,
    syndromes: [u32; 112],
    two_bit: Vec<(u32, u8, u8)>,
    timing: Timing,
    /// Samples a 112-bit frame needs; samples Mode A/C needs; samples kept
    /// before the scan position.
    frame: usize,
    ac_span: usize,
    lead: usize,
    /// Samples being searched; `buf[0]` is stream sample `base`.
    buf: Vec<f32>,
    /// Running sum of `buf`.
    pre: Vec<f64>,
    base: u64,
    /// Next start position in `buf` to test for a preamble.
    scan: usize,
    /// Mean magnitude of the input over the last few tens of milliseconds
    /// (mostly noise).
    noise: f64,
    aircraft: Table,
    queue: MsgQueue,
}

impl AdsbDecoder {
    pub fn new(fix: bool) -> Self {
        let syndromes = syndromes();
        let mut d = Self {
            fix,
            fix_two: false,
            all_formats: true,
            mode_ac: false,
            receiver: None,
            rate: RATE,
            two_bit: two_bit_syndromes(&syndromes),
            syndromes,
            timing: Timing { s: 2.0 },
            frame: 0,
            ac_span: 0,
            lead: 0,
            buf: Vec::new(),
            pre: Vec::new(),
            base: 0,
            scan: 0,
            noise: 0.0,
            aircraft: Table::new(),
            queue: MsgQueue::new(),
        };
        d.set_rate(RATE);
        d
    }

    fn flip(m: &mut ModeS, bit: usize) {
        m.bytes[bit / 8] ^= 0x80 >> (bit % 8);
    }

    /// Check a message's parity, repairing it if allowed. Returns the
    /// aircraft address and the repair, or `None` if it isn't believed.
    fn validate(&mut self, m: &mut ModeS, t: f64) -> Option<(u32, Fix)> {
        let r = m.residual();
        match m.df() {
            17 | 18 => {
                let fix = if r == 0 {
                    Fix::None
                } else if !self.fix {
                    return None;
                } else if let Some(i) = (5..112).find(|&i| self.syndromes[i] == r) {
                    Self::flip(m, i);
                    Fix::One(i)
                } else if self.fix_two
                    && let Ok(k) = self.two_bit.binary_search_by_key(&r, |e| e.0)
                {
                    let (_, i, j) = self.two_bit[k];
                    let mut fixed = *m;
                    Self::flip(&mut fixed, usize::from(i));
                    Self::flip(&mut fixed, usize::from(j));
                    // Too risky for an address never heard intact.
                    if !self.aircraft.verified(fixed.icao(), t) {
                        return None;
                    }
                    *m = fixed;
                    Fix::Two(usize::from(i), usize::from(j))
                } else {
                    return None;
                };
                let icao = m.icao();
                if m.df() == 17 && fix == Fix::None {
                    self.aircraft.entry(icao, t).verified = t;
                }
                Some((icao, fix))
            }
            11 => {
                if r == 0 {
                    let icao = m.icao();
                    self.aircraft.entry(icao, t).verified = t;
                    return Some((icao, Fix::None));
                }
                // A reply to interrogator `r` (II/SI code in the low bits).
                if r & !0x7F == 0 {
                    return self
                        .aircraft
                        .verified(m.icao(), t)
                        .then(|| (m.icao(), Fix::None));
                }
                if !self.fix {
                    return None;
                }
                let i = (5..56).find(|&i| self.syndromes[56 + i] == r)?;
                let mut fixed = *m;
                Self::flip(&mut fixed, i);
                self.aircraft.verified(fixed.icao(), t).then(|| {
                    *m = fixed;
                    (m.icao(), Fix::One(i))
                })
            }
            0 | 4 | 5 | 16 | 20 | 21 => self.aircraft.verified(r, t).then_some((r, Fix::None)),
            _ => None,
        }
    }

    /// Position from a CPR frame, and how it was found.
    fn position(&mut self, icao: u32, cpr: Cpr, t: f64) -> Option<(f64, f64, &'static str)> {
        let receiver = self.receiver;
        let a = self.aircraft.entry(icao, t);
        // Pair only frames of the same kind.
        let keep = |f: Option<(Cpr, f64)>| f.filter(|(c, _)| c.surface == cpr.surface);
        let other = if cpr.odd {
            a.odd = Some((cpr, t));
            keep(a.even)
        } else {
            a.even = Some((cpr, t));
            keep(a.odd)
        };
        let recent_pos = a.pos.filter(|p| t - p.2 <= LOCAL_REF_S).map(|p| (p.0, p.1));
        let reference = recent_pos.or(receiver);
        let pair_s = if cpr.surface {
            CPR_PAIR_SURFACE_S
        } else {
            CPR_PAIR_S
        };
        let global = other
            .filter(|(_, at)| (t - at).abs() <= pair_s)
            .and_then(|(o, _)| {
                let (even, odd) = if cpr.odd { (o, cpr) } else { (cpr, o) };
                if cpr.surface {
                    cpr_global_surface(even, odd, cpr.odd, reference?)
                } else {
                    cpr_global(even, odd, cpr.odd)
                }
            });
        let found = if let Some((lat, lon)) = global {
            Some((lat, lon, "global"))
        } else if let Some(r) = recent_pos {
            let p = cpr_local(cpr, r);
            // The aircraft can't have gone far since.
            (cpr::distance_nm(p, r) < if cpr.surface { 10.0 } else { 50.0 })
                .then_some((p.0, p.1, "local"))
        } else if let Some(r) = receiver {
            let p = cpr_local(cpr, r);
            (cpr::distance_nm(p, r) < if cpr.surface { 45.0 } else { 180.0 })
                .then_some((p.0, p.1, "local"))
        } else {
            None
        };
        let found = found.filter(|&(lat, lon, _)| {
            receiver.is_none_or(|r| cpr::distance_nm((lat, lon), r) <= MAX_RANGE_NM)
        });
        if let Some((lat, lon, _)) = found {
            self.aircraft.entry(icao, t).pos = Some((lat, lon, t));
        }
        found
    }

    /// Queue `raw=…` and the end of the line.
    fn finish(&mut self, m: &ModeS, fix: Fix) {
        match fix {
            Fix::None => {}
            Fix::One(i) => self.queue.text(format_args!(" fixed_bit={i}")),
            Fix::Two(i, j) => self.queue.text(format_args!(" fixed_bits={i},{j}")),
        }
        self.queue.text(format_args!(" raw="));
        for b in m.data() {
            self.queue.text(format_args!("{b:02X}"));
        }
        self.queue.line(format_args!(""));
    }

    fn alt(&mut self, icao: u32, alt: Option<i32>, t: f64) {
        if let Some(a) = alt {
            self.queue.text(format_args!(" alt_ft={a}"));
            self.aircraft.entry(icao, t).alt = Some((a, t));
        }
    }

    /// Format one decoded message into the queue.
    fn report(&mut self, m: ModeS, icao: u32, fix: Fix, t: f64) {
        let df = m.df();
        self.queue
            .text(format_args!("t={t:.6} icao={icao:06X} df={df}"));
        match df {
            17 | 18 => self.report_es(&m, icao, t),
            11 => {
                self.queue
                    .text(format_args!(" type=all_call ca={}", m.ca()));
                let iid = m.residual();
                if iid != 0 {
                    self.queue.text(format_args!(" iid={iid}"));
                }
            }
            0 | 16 => {
                self.queue.text(format_args!(" type=air_air"));
                self.alt(icao, ac13_ft(m.code13()), t);
                if m.field(6, 6) == 1 {
                    self.queue.text(format_args!(" ground=1"));
                }
                if df == 16
                    && let CommB::Acas(a) = commb::decode(m.me_field(), Default::default())
                {
                    self.queue.text(format_args!(" ra_ara={:04X}", a.ara));
                }
            }
            4 | 5 | 20 | 21 => {
                let kind = if df >= 20 {
                    "commb"
                } else if df == 4 {
                    "altitude"
                } else {
                    "identity"
                };
                self.queue.text(format_args!(" type={kind}"));
                if df == 4 || df == 20 {
                    self.alt(icao, ac13_ft(m.code13()), t);
                } else {
                    self.queue
                        .text(format_args!(" squawk={:04X}", id13_squawk(m.code13())));
                }
                let fs = m.field(6, 8);
                if fs == 1 || fs == 3 {
                    self.queue.text(format_args!(" ground=1"));
                }
                if (2..=4).contains(&fs) {
                    self.queue.text(format_args!(" alert=1"));
                }
                if fs == 4 || fs == 5 {
                    self.queue.text(format_args!(" spi=1"));
                }
                if df >= 20 {
                    self.report_commb(&m, icao, t);
                }
            }
            _ => {}
        }
        self.finish(&m, fix);
    }

    fn report_commb(&mut self, m: &ModeS, icao: u32, t: f64) {
        let mut r = commb::Reference::default();
        if m.df() == 20 {
            r.alt_ft = ac13_ft(m.code13());
        }
        if let Some(a) = self.aircraft.get(icao, t) {
            r.alt_ft = r.alt_ft.or(a.alt.filter(|x| t - x.1 < 30.0).map(|x| x.0));
            if let Some((gs, trk, at)) = a.vel
                && t - at < 30.0
            {
                r.gs_kt = Some(gs);
                r.track_deg = Some(trk);
            }
        }
        let q = &mut self.queue;
        match commb::decode(m.me_field(), r) {
            CommB::Empty => q.text(format_args!(" bds=empty")),
            CommB::Unknown => q.text(format_args!(" bds=unknown")),
            CommB::Ambiguous => q.text(format_args!(" bds=ambiguous")),
            CommB::DataLink => q.text(format_args!(" bds=1,0")),
            CommB::Gicb(caps) => {
                q.text(format_args!(" bds=1,7 caps="));
                let mut first = true;
                for (i, name) in commb::GICB_REGISTERS.iter().enumerate() {
                    if caps & (1 << (23 - i)) != 0 {
                        let sep = if first { "" } else { "|" };
                        q.text(format_args!("{sep}{name}"));
                        first = false;
                    }
                }
            }
            CommB::Ident(cs) => q.text(format_args!(" bds=2,0 callsign={}", cs.as_str())),
            CommB::Acas(a) => {
                q.text(format_args!(
                    " bds=3,0 ara={:04X} rat={} mte={} tti={}",
                    a.ara,
                    u8::from(a.rat),
                    u8::from(a.mte),
                    a.tti
                ));
                if let Some(th) = a.threat {
                    q.text(format_args!(" threat={th:06X}"));
                }
            }
            CommB::Intent(i) => {
                q.text(format_args!(" bds=4,0"));
                if let Some(v) = i.mcp_alt_ft {
                    q.text(format_args!(" mcp_alt_ft={v}"));
                }
                if let Some(v) = i.fms_alt_ft {
                    q.text(format_args!(" fms_alt_ft={v}"));
                }
                if let Some(v) = i.baro_hpa {
                    q.text(format_args!(" baro_hpa={v:.1}"));
                }
            }
            CommB::TrackTurn(x) => {
                q.text(format_args!(" bds=5,0"));
                if let Some(v) = x.roll_deg {
                    q.text(format_args!(" roll_deg={v:.1}"));
                }
                if let Some(v) = x.track_deg {
                    q.text(format_args!(" track_deg={v:.1}"));
                }
                if let Some(v) = x.gs_kt {
                    q.text(format_args!(" gs_kt={v:.0}"));
                }
                if let Some(v) = x.track_rate {
                    q.text(format_args!(" track_rate={v:.2}"));
                }
                if let Some(v) = x.tas_kt {
                    q.text(format_args!(" tas_kt={v:.0}"));
                }
            }
            CommB::HeadingSpeed(x) => {
                q.text(format_args!(" bds=6,0"));
                if let Some(v) = x.heading_deg {
                    q.text(format_args!(" heading_deg={v:.1}"));
                }
                if let Some(v) = x.ias_kt {
                    q.text(format_args!(" ias_kt={v:.0}"));
                }
                if let Some(v) = x.mach {
                    q.text(format_args!(" mach={v:.3}"));
                }
                if let Some(v) = x.baro_rate_fpm {
                    q.text(format_args!(" baro_rate_fpm={v}"));
                }
                if let Some(v) = x.ins_rate_fpm {
                    q.text(format_args!(" ins_rate_fpm={v}"));
                }
            }
        }
    }

    /// The body of an extended squitter (DF17/18).
    fn report_es(&mut self, m: &ModeS, icao: u32, t: f64) {
        let tc = m.tc();
        if m.df() == 18 {
            let cf = m.ca();
            self.queue.text(format_args!(" cf={cf}"));
            match cf {
                3 => return self.queue.text(format_args!(" type=tisb_coarse")),
                4 => return self.queue.text(format_args!(" type=tisb_management")),
                7 => return self.queue.text(format_args!(" type=reserved")),
                _ => {}
            }
            let src = match cf {
                0 => "adsb",
                1 => "adsb_anon",
                2 => "tisb",
                5 => "tisb_anon",
                _ => "adsr",
            };
            self.queue.text(format_args!(" source={src}"));
        }
        self.queue.text(format_args!(" tc={tc}"));
        if let Some(cs) = m.callsign() {
            self.queue
                .text(format_args!(" type=ident callsign={}", cs.as_str()));
            if let Some((set, n)) = m.category() {
                self.queue.text(format_args!(" category={set}{n}"));
            }
        } else if let Some(cpr) = m.cpr() {
            if cpr.surface {
                self.queue.text(format_args!(" type=surface_position"));
                let (gs, trk) = m.surface_movement();
                if let Some(v) = gs {
                    self.queue.text(format_args!(" gs_kt={v:.1}"));
                }
                if let Some(v) = trk {
                    self.queue.text(format_args!(" track_deg={v:.1}"));
                }
            } else {
                self.queue.text(format_args!(" type=position"));
                self.alt(icao, m.altitude_ft(), t);
                if let Some(h) = m.gnss_height_m() {
                    self.queue
                        .text(format_args!(" gnss_alt_ft={:.0}", f64::from(h) / 0.3048));
                }
            }
            match self.position(icao, cpr, t) {
                Some((lat, lon, how)) => self
                    .queue
                    .text(format_args!(" lat={lat:.5} lon={lon:.5} pos={how}")),
                None => self.queue.text(format_args!(
                    " cpr={}",
                    if cpr.odd { "odd" } else { "even" }
                )),
            }
        } else if let Some(v) = m.velocity() {
            let vr = match v {
                Velocity::Ground {
                    speed_kt,
                    track_deg,
                    vrate_fpm,
                } => {
                    self.queue.text(format_args!(
                        " type=velocity gs_kt={speed_kt:.1} track_deg={track_deg:.1}"
                    ));
                    if let Some(a) = self.aircraft.get(icao, t) {
                        a.vel = Some((speed_kt, track_deg, t));
                    }
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
                let src = if m.vrate_baro() { "baro" } else { "gnss" };
                self.queue
                    .text(format_args!(" vrate_fpm={vr} vrate_src={src}"));
            }
            if let Some(d) = m.geo_minus_baro_ft() {
                self.queue.text(format_args!(" geo_minus_baro_ft={d}"));
            }
            if matches!(m.velocity_subtype(), 2 | 4) {
                self.queue.text(format_args!(" supersonic=1"));
            }
        } else {
            match tc {
                0 => self.queue.text(format_args!(" type=no_position")),
                19 => self.queue.text(format_args!(" type=velocity")),
                23 => self.queue.text(format_args!(" type=test")),
                24 => self.queue.text(format_args!(" type=surface_system")),
                28 => match m.emergency() {
                    Some((state, squawk)) => {
                        const STATES: [&str; 8] = [
                            "none", "general", "medical", "fuel", "radio", "unlawful", "downed",
                            "reserved",
                        ];
                        self.queue.text(format_args!(
                            " type=aircraft_status emergency={} squawk={squawk:04X}",
                            STATES[state as usize]
                        ));
                    }
                    None => self.queue.text(format_args!(" type=acas_ra")),
                },
                29 => match m.target_state() {
                    Some(s) => {
                        self.queue.text(format_args!(" type=target_state"));
                        if let Some(a) = s.selected_alt_ft {
                            let src = if s.fms { "fms" } else { "mcp" };
                            self.queue
                                .text(format_args!(" sel_alt_ft={a} sel_alt_src={src}"));
                        }
                        if let Some(b) = s.baro_hpa {
                            self.queue.text(format_args!(" baro_hpa={b:.1}"));
                        }
                        if let Some(h) = s.heading_deg {
                            self.queue.text(format_args!(" sel_heading_deg={h:.1}"));
                        }
                        for (name, v) in [
                            ("autopilot", s.autopilot),
                            ("vnav", s.vnav),
                            ("alt_hold", s.alt_hold),
                            ("approach", s.approach),
                            ("lnav", s.lnav),
                        ] {
                            if let Some(v) = v {
                                self.queue.text(format_args!(" {name}={}", u8::from(v)));
                            }
                        }
                    }
                    None => self.queue.text(format_args!(" type=target_state")),
                },
                31 => match m.op_status() {
                    Some(s) => {
                        self.queue.text(format_args!(
                            " type=op_status version={} nacp={} sil={} nic_a={}",
                            s.version, s.nacp, s.sil, s.nic_a
                        ));
                        if s.surface {
                            self.queue.text(format_args!(" surface=1"));
                        }
                    }
                    None => self.queue.text(format_args!(" type=op_status")),
                },
                _ => self.queue.text(format_args!(" type=reserved")),
            }
        }
    }

    fn report_mode_ac(&mut self, code: u32, spi: bool, t: f64) {
        let squawk = id13_squawk(code);
        self.queue
            .text(format_args!("t={t:.6} type=mode_ac squawk={squawk:04X}"));
        if let Some(a) = gillham_ft(squawk) {
            self.queue.text(format_args!(" alt_ft={a}"));
        }
        if spi {
            self.queue.text(format_args!(" spi=1"));
        }
        self.queue.line(format_args!(" raw={code:04X}"));
    }

    /// Size the buffers for `rate`.
    fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
        self.timing = Timing { s: rate / 1e6 };
        self.frame = self.timing.frame();
        self.ac_span = modeac::span(self.timing.s);
        self.lead = (1.5 * self.timing.s).ceil() as usize + 1;
        let cap = CHUNK + self.frame + self.lead;
        self.buf = Vec::with_capacity(cap);
        self.pre = Vec::with_capacity(cap + 1);
        self.scan = self.lead;
    }

    /// Try to read a Mode S reply whose preamble starts near `n`.
    fn mode_s_at(&mut self, n: usize) -> Option<(ModeS, u32, Fix, f64)> {
        let sig = Signal {
            m: &self.buf,
            pre: &self.pre,
        };
        let (t0, amp) = self.timing.find_preamble(&sig, n, self.noise)?;
        let quarter = 0.25;
        let mut tried = [None; 3];
        for (k, t) in [t0, t0 - quarter, t0 + quarter].into_iter().enumerate() {
            let sig = Signal {
                m: &self.buf,
                pre: &self.pre,
            };
            let mut bytes = [0u8; 14];
            self.timing.bits(&sig, t, amp, &mut bytes[..1], 8);
            let df = bytes[0] >> 3;
            if !wanted(df, self.all_formats) {
                continue;
            }
            let len = msg::len_for_df(df);
            self.timing.bits(&sig, t, amp, &mut bytes[..len], len * 8);
            let mut m = ModeS::new(&bytes[..len]);
            // The same bits from a neighbouring phase needn't be checked again.
            if tried[..k].contains(&Some(m)) {
                continue;
            }
            tried[k] = Some(m);
            let at = (self.base as f64 + t) / self.rate;
            if let Some((icao, fix)) = self.validate(&mut m, at) {
                return Some((m, icao, fix, t));
            }
        }
        None
    }

    /// Search `buf` from `scan` until a message is queued or too few samples
    /// remain.
    fn search(&mut self) {
        while self.scan + self.frame <= self.buf.len() {
            let n = self.scan;
            if let Some((m, icao, fix, t)) = self.mode_s_at(n) {
                let at = (self.base as f64 + t) / self.rate;
                self.scan = (t + (8 + 8 * m.len) as f64 * self.timing.s) as usize;
                self.report(m, icao, fix, at);
                return;
            }
            if self.mode_ac {
                let sig = Signal {
                    m: &self.buf,
                    pre: &self.pre,
                };
                if let Some((code, spi)) = modeac::detect(&sig, n, self.timing.s, self.noise) {
                    let at = (self.base + n as u64) as f64 / self.rate;
                    self.scan += self.ac_span;
                    self.report_mode_ac(code, spi, at);
                    return;
                }
            }
            self.scan += 1;
        }
    }
}

impl Block for AdsbDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs[0].sample_rate.unwrap_or(RATE);
        if rate < RATE * 0.999 {
            return Err(Error::Other(format!(
                "needs at least 2 MS/s (two samples per Mode S bit) but gets {}; set the \
                 receiver to 2 MS/s or more",
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
            self.search();
            if !self.queue.is_empty() {
                continue;
            }
            // Not enough samples left for a frame: keep the tail, read more.
            let inp = io.inputs[0].slice::<f32>();
            let drop = self.scan - self.lead;
            let room = self.buf.capacity() - self.buf.len() + drop;
            let take = (inp.len() - used).min(room);
            if take == 0 {
                break;
            }
            self.buf.drain(..drop);
            self.base += drop as u64;
            self.scan = self.lead;
            let new = &inp[used..used + take];
            // A slow average (time constant NOISE_SAMPLES), so a short read
            // that is all signal doesn't raise the floor.
            let mean = new.iter().map(|&v| f64::from(v)).sum::<f64>() / take as f64;
            if self.noise == 0.0 {
                self.noise = mean;
            } else {
                let a = 1.0 - (-(take as f64) / NOISE_SAMPLES).exp();
                self.noise += a * (mean - self.noise);
            }
            self.buf.extend_from_slice(new);
            demod::prefix(&self.buf, &mut self.pre);
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
            "fix_errors" => self.fix = value.as_bool().unwrap_or(true),
            "fix_two_bits" => self.fix_two = value.as_bool().unwrap_or(false),
            "mode_ac" => self.mode_ac = value.as_bool().unwrap_or(false),
            "formats" => self.all_formats = value.as_str() != Some("extended_squitter"),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let mut d = AdsbDecoder::new(p.bool("fix_errors")?);
    d.fix_two = p.bool("fix_two_bits")?;
    d.mode_ac = p.bool("mode_ac")?;
    d.all_formats = p.str("formats")? != "extended_squitter";
    d.receiver = parse_position(p.str("receiver")?).map_err(|msg| Error::Param {
        block: "adsb_decoder".into(),
        param: "receiver".into(),
        msg,
    })?;
    Ok(Instance {
        block: Box::new(d),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::U8)],
    })
}
