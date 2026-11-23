//! SSTV (slow-scan television) decoder: Robot, Martin, Scottie, Wraase and PD
//! modes, picked automatically from the VIS header.
//!
//! SSTV sends a picture as audio whose frequency is the brightness: 1500 Hz
//! black to 2300 Hz white, with 1200 Hz sync pulses between lines, one colour
//! component (or luma / chroma) after the other.
//!
//! - **Frequency.** The audio is mixed down by 1900 Hz, low-passed and
//!   decimated to about 10 kS/s, and the frequency is the phase step between
//!   samples. Running sums of the frequency (a prefix sum in a ring) make the
//!   mean over any span O(1), which everything below is built on.
//! - **VIS.** The header (1900 Hz leader, 1200 Hz start bit, seven data bits
//!   LSB first at 1100 Hz = 1 / 1300 Hz = 0, even parity, a stop bit, 30 ms
//!   each) is matched at every sample, the best match in a 15 ms span wins,
//!   and the leader-to-start-bit edge pins down when the picture starts.
//! - **Line clock and slant.** Each line's sync pulse is searched for near
//!   where it should be (the span of `sync` with the lowest mean frequency).
//!   A least-squares line through the pulses found so far gives the start of
//!   every line and the sender's clock against ours, so a sound card that is
//!   a few hundred ppm off doesn't slant the picture. Lines without a pulse
//!   are placed by the fit.
//! - **Pixels** are the mean frequency over each pixel's time, mapped to
//!   0…255. YCrCb modes (Robot, PD) use the full-range (JPEG) matrix.

use foxygnal_core::raster::{LAST, Row};
use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::filter::{Resampler, Rotator};
use foxygnal_dsp::fir::{Band, DesignWindow, FirSpec};

use super::push_row;
use crate::design::{self, fmt_rate};
use crate::digital::{MsgQueue, idle_status, text_meta};
use crate::filters::not_configured;

/// How the scans of a line make pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    /// R, G and B scans: one row per line.
    Rgb,
    /// Y, then R−Y on even lines and B−Y on odd lines: two lines make a pair
    /// of rows that share their chroma.
    Robot36,
    /// Y, R−Y, B−Y: one row per line.
    Ycc,
    /// Y of an even row, R−Y, B−Y, Y of the odd row: two rows per line.
    Pd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chan {
    R,
    G,
    B,
    Y,
    /// The second row's luma (PD).
    Y2,
    Cr,
    Cb,
    /// R−Y or B−Y, whichever this line carries (Robot 36).
    C,
}

#[derive(Debug, Clone, Copy)]
struct Scan {
    chan: Chan,
    /// From the start of the line, in ms.
    at: f64,
    /// Length, in ms.
    len: f64,
}

/// One SSTV mode's timing.
#[derive(Debug)]
pub struct SstvMode {
    /// Short name, used for the `mode` setting.
    pub id: &'static str,
    pub name: &'static str,
    pub vis: u8,
    pub width: u16,
    pub height: u16,
    /// Lines sent (PD sends two rows per line).
    pub lines: u16,
    /// Line length, ms.
    pub line: f64,
    /// Sync pulse length, ms.
    pub sync: f64,
    /// Where the sync pulse starts within the line, ms.
    sync_at: f64,
    /// From the end of the VIS stop bit to the start of line 0, ms.
    start: f64,
    color: Color,
    scans: &'static [Scan],
}

const fn scan(chan: Chan, at: f64, len: f64) -> Scan {
    Scan { chan, at, len }
}

/// Martin: sync, then G, B, R, each after a 0.572 ms gap.
const fn martin(id: &'static str, name: &'static str, vis: u8, t: f64) -> SstvMode {
    const SYNC: f64 = 4.862;
    const GAP: f64 = 0.572;
    SstvMode {
        id,
        name,
        vis,
        width: 320,
        height: 256,
        lines: 256,
        line: SYNC + 4.0 * GAP + 3.0 * t,
        sync: SYNC,
        sync_at: 0.0,
        start: 0.0,
        color: Color::Rgb,
        scans: &[],
    }
}

/// Scottie: G and B, then the sync pulse, then R; an extra sync pulse
/// before the first line.
const fn scottie(id: &'static str, name: &'static str, vis: u8, t: f64) -> SstvMode {
    SstvMode {
        id,
        name,
        vis,
        width: 320,
        height: 256,
        lines: 256,
        line: 3.0 * 1.5 + 9.0 + 3.0 * t,
        sync: 9.0,
        sync_at: 2.0 * 1.5 + 2.0 * t,
        start: 9.0,
        color: Color::Rgb,
        scans: &[],
    }
}

/// PD: 20 ms sync, 2.08 ms porch, then Y (even row), R−Y, B−Y, Y (odd row).
const fn pd(
    id: &'static str,
    name: &'static str,
    vis: u8,
    t: f64,
    width: u16,
    height: u16,
) -> SstvMode {
    SstvMode {
        id,
        name,
        vis,
        width,
        height,
        lines: height / 2,
        line: 20.0 + 2.08 + 4.0 * t,
        sync: 20.0,
        sync_at: 0.0,
        start: 0.0,
        color: Color::Pd,
        scans: &[],
    }
}

/// Every mode the decoder knows. Scans are filled in by [`scans`].
pub const SSTV_MODES: &[SstvMode] = &[
    SstvMode {
        id: "robot36",
        name: "Robot 36",
        vis: 8,
        width: 320,
        height: 240,
        lines: 240,
        line: 150.0,
        sync: 9.0,
        sync_at: 0.0,
        start: 0.0,
        color: Color::Robot36,
        scans: &[scan(Chan::Y, 12.0, 88.0), scan(Chan::C, 106.0, 44.0)],
    },
    SstvMode {
        id: "robot72",
        name: "Robot 72",
        vis: 12,
        width: 320,
        height: 240,
        lines: 240,
        line: 300.0,
        sync: 9.0,
        sync_at: 0.0,
        start: 0.0,
        color: Color::Ycc,
        scans: &[
            scan(Chan::Y, 12.0, 138.0),
            scan(Chan::Cr, 156.0, 69.0),
            scan(Chan::Cb, 231.0, 69.0),
        ],
    },
    martin("martin1", "Martin M1", 44, 146.432),
    martin("martin2", "Martin M2", 40, 73.216),
    scottie("scottie1", "Scottie S1", 60, 138.24),
    scottie("scottie2", "Scottie S2", 56, 88.064),
    scottie("scottie_dx", "Scottie DX", 76, 345.6),
    SstvMode {
        id: "wraase_sc2_180",
        name: "Wraase SC2-180",
        vis: 55,
        width: 320,
        height: 256,
        lines: 256,
        line: 5.5225 + 0.5 + 3.0 * 235.0,
        sync: 5.5225,
        sync_at: 0.0,
        start: 0.0,
        color: Color::Rgb,
        scans: &[
            scan(Chan::R, 6.0225, 235.0),
            scan(Chan::G, 6.0225 + 235.0, 235.0),
            scan(Chan::B, 6.0225 + 470.0, 235.0),
        ],
    },
    pd("pd50", "PD 50", 93, 91.52, 320, 256),
    pd("pd90", "PD 90", 99, 170.24, 320, 256),
    pd("pd120", "PD 120", 95, 121.6, 640, 496),
    pd("pd160", "PD 160", 98, 195.584, 512, 400),
    pd("pd180", "PD 180", 96, 183.04, 640, 496),
    pd("pd240", "PD 240", 97, 244.48, 640, 496),
    pd("pd290", "PD 290", 94, 228.8, 800, 616),
];

/// The scans of a mode's line (the families compute theirs from the line
/// length).
fn scans(m: &SstvMode) -> [Option<Scan>; 4] {
    let mut out = [None; 4];
    if !m.scans.is_empty() {
        for (o, s) in out.iter_mut().zip(m.scans) {
            *o = Some(*s);
        }
        return out;
    }
    match (m.color, m.start > 0.0) {
        // Scottie.
        (Color::Rgb, true) => {
            let t = (m.line - 13.5) / 3.0;
            out[0] = Some(scan(Chan::G, 1.5, t));
            out[1] = Some(scan(Chan::B, 3.0 + t, t));
            out[2] = Some(scan(Chan::R, m.sync_at + 10.5, t));
        }
        // Martin.
        (Color::Rgb, false) => {
            let (sync, gap) = (4.862, 0.572);
            let t = (m.line - sync - 4.0 * gap) / 3.0;
            for (i, c) in [Chan::G, Chan::B, Chan::R].into_iter().enumerate() {
                out[i] = Some(scan(c, sync + gap + i as f64 * (t + gap), t));
            }
        }
        (Color::Pd, _) => {
            let t = (m.line - 22.08) / 4.0;
            for (i, c) in [Chan::Y, Chan::Cr, Chan::Cb, Chan::Y2]
                .into_iter()
                .enumerate()
            {
                out[i] = Some(scan(c, 22.08 + i as f64 * t, t));
            }
        }
        _ => unreachable!("mode {} has no scans", m.id),
    }
    out
}

pub fn register(r: &mut Registry) {
    let mut modes = vec!["auto"];
    modes.extend(SSTV_MODES.iter().map(|m| m.id));
    r.register(
        BlockDescriptor {
            kind: "sstv_decoder",
            label: "SSTV Decoder",
            category: "Pictures",
            help: "Slow-scan television: FM- or SSB-demodulated audio in (any rate from \
                   6 kS/s), pictures out. Robot 36/72, Martin M1/M2, Scottie S1/S2/DX, \
                   Wraase SC2-180 and PD 50…290, chosen from the VIS header. The line clock \
                   follows the sync pulses, so a transmitter whose clock is a little off \
                   doesn't slant the picture. `image` carries the picture (watch it with the \
                   Image instrument, save it with an Image Sink); `out` says when a picture \
                   starts and ends (`t=12.345 event=start mode=\"Martin M1\" vis=44 …`).",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![
                PortSpec {
                    name: "image",
                    ty: Some(StreamType::U8),
                },
                PortSpec {
                    name: "out",
                    ty: Some(StreamType::U8),
                },
            ],
            params: vec![
                design::choice(
                    "mode",
                    "Mode",
                    modes,
                    "auto",
                    "Auto: from the VIS header. A fixed mode decodes every VIS header as that \
                     mode, even one with a bad parity bit or an unknown code.",
                ),
                ParamSpec {
                    name: "slant",
                    label: "Slant correction",
                    kind: foxygnal_core::ParamKind::Bool,
                    default: serde_json::json!(true),
                    help: "Follow the sync pulses to correct for the sender's clock. Off: \
                           nominal timing from the VIS header on.",
                    live: true,
                },
            ],
        },
        build,
    );
    r.set_keywords(
        "sstv_decoder",
        &[
            "sstv",
            "slow scan",
            "picture",
            "image",
            "robot",
            "martin",
            "scottie",
            "pd",
        ],
    );
}

const BLACK: f64 = 1500.0;
const WHITE: f64 = 2300.0;
const SYNC: f64 = 1200.0;
const LEADER: f64 = 1900.0;
/// A span this low on average is a sync pulse.
const SYNC_MAX: f64 = 1350.0;
/// Lines in a row without a sync pulse before a picture is given up.
const MAX_MISSES: u32 = 10;
/// The end of a scan that runs into a sync or separator tone, smeared by band
/// limits.
const EDGE_GUARD_MS: f64 = 0.35;
/// Widest picture of any mode.
const MAX_WIDTH: usize = 800;

/// The mean frequency over any recent span: a ring of running sums.
struct Track {
    /// `cum[i & mask]` = sum of the frequencies before sample `i`.
    cum: Vec<f64>,
    mask: usize,
    /// Samples so far.
    n: usize,
}

impl Track {
    fn new(len: usize) -> Self {
        let len = len.next_power_of_two();
        Self {
            cum: vec![0.0; len],
            mask: len - 1,
            n: 0,
        }
    }

    fn push(&mut self, f: f64) {
        let c = self.cum[self.n & self.mask] + f;
        self.n += 1;
        self.cum[self.n & self.mask] = c;
    }

    /// The running sum at a (fractional) sample time, interpolated.
    fn at(&self, t: f64) -> f64 {
        let oldest = self.n.saturating_sub(self.mask) as f64;
        let t = t.clamp(oldest, self.n as f64);
        let k = t.floor() as usize;
        let c = self.cum[k & self.mask];
        if k == self.n {
            return c;
        }
        c + (t - k as f64) * (self.cum[(k + 1) & self.mask] - c)
    }

    /// Mean frequency over `[a, b)` (sample times).
    fn mean(&self, a: f64, b: f64) -> f64 {
        if b <= a {
            return 0.0;
        }
        (self.at(b) - self.at(a)) / (b - a)
    }
}

/// A VIS header that matched, waiting to see if a better match follows.
struct VisCandidate {
    score: f64,
    /// Start of the start bit (sample time).
    start: f64,
    bits: u8,
    /// Commit at this sample.
    until: usize,
}

/// A picture being decoded.
struct Picture {
    mode: &'static SstvMode,
    scans: [Option<Scan>; 4],
    number: u32,
    /// Next line to decode.
    line: u16,
    /// Sync time of line 0, from the VIS header (sample time).
    origin: f64,
    /// Nominal line length in samples.
    nominal: f64,
    /// Line-clock fit: sync time = a + b·line.
    a: f64,
    b: f64,
    // Least-squares sums over the sync pulses found.
    fit_n: f64,
    fit_k: f64,
    fit_kk: f64,
    fit_y: f64,
    fit_ky: f64,
    first_k: u16,
    last: Option<(u16, f64)>,
    misses: u32,
    /// Robot 36: an even line waiting for its odd partner (luma and R−Y).
    pending_even: Option<u16>,
    /// The last row sent, to repeat it with the last-row flag if the
    /// picture is cut off.
    last_row: Option<u16>,
}

struct State {
    rate: f64,
    mixer: Rotator,
    filter: Resampler<Complex32>,
    /// Filtered, decimated samples not processed yet.
    dec: Vec<Complex32>,
    dec_pos: usize,
    prev: Complex32,
    track: Track,
    vis: Option<VisCandidate>,
    /// No VIS search before this sample (just after one).
    vis_holdoff: usize,
    picture: Option<Picture>,
    pictures: u32,
    /// Pixel values per scan channel (R, G, B, Y, Y2, Cr, Cb, C).
    chans: [Vec<f32>; 8],
    /// Robot 36: the even line's luma and R−Y, kept for its partner.
    even_y: Vec<f32>,
    even_cr: Vec<f32>,
    row: Vec<u8>,
}

impl State {
    fn ms(&self, ms: f64) -> f64 {
        ms * self.rate / 1000.0
    }
}

fn chan_index(c: Chan) -> usize {
    match c {
        Chan::R => 0,
        Chan::G => 1,
        Chan::B => 2,
        Chan::Y => 3,
        Chan::Y2 => 4,
        Chan::Cr => 5,
        Chan::Cb => 6,
        Chan::C => 7,
    }
}

fn level(f: f64) -> f32 {
    ((f - BLACK) / (WHITE - BLACK) * 255.0) as f32
}

fn byte(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Full-range (JPEG) YCrCb to RGB, 0…255 in and out.
fn ycc_rgb(y: f32, cr: f32, cb: f32) -> [u8; 3] {
    let (cr, cb) = (cr - 128.0, cb - 128.0);
    [
        byte(y + 1.402 * cr),
        byte(y - 0.344_136 * cb - 0.714_136 * cr),
        byte(y + 1.772 * cb),
    ]
}

pub struct SstvDecoder {
    forced: Option<&'static SstvMode>,
    slant: bool,
    state: Option<State>,
    rows: MsgQueue,
    msgs: MsgQueue,
}

impl SstvDecoder {
    /// Seconds since the start of the stream, at a sample time.
    fn secs(st: &State, t: f64) -> f64 {
        t / st.rate
    }

    /// Take one filtered sample.
    fn sample(&mut self, z: Complex32) {
        let st = self.state.as_mut().expect("configured");
        let d = z * st.prev.conj();
        st.prev = z;
        let f = LEADER + f64::from(d.im.atan2(d.re)) * st.rate / std::f64::consts::TAU;
        // Clamp wild steps (silence, clicks) so one sample can't swamp a mean.
        st.track.push(f.clamp(900.0, 2700.0));
        self.check_vis();
        self.step_picture();
    }

    /// Look for a VIS header ending at the newest sample.
    fn check_vis(&mut self) {
        let st = self.state.as_mut().expect("configured");
        let n = st.track.n;
        if let Some(c) = &st.vis
            && n >= c.until
        {
            let c = st.vis.take().expect("checked");
            st.vis_holdoff = n + st.ms(500.0) as usize;
            self.vis_found(c);
            return;
        }
        if n < st.vis_holdoff || (n as f64) < st.ms(640.0) {
            return;
        }
        // The start bit begins 300 ms before the end of the stop bit.
        let s = n as f64 - st.ms(300.0);
        let seg = |a: f64, b: f64| st.track.mean(s + st.ms(a), s + st.ms(b));
        let leader = seg(-280.0, -20.0);
        if (leader - LEADER).abs() > 100.0 {
            return;
        }
        let start = seg(5.0, 25.0);
        let stop = seg(275.0, 295.0);
        if (start - SYNC).abs() > 100.0 || (stop - SYNC).abs() > 100.0 {
            return;
        }
        let mut score = (leader - LEADER).powi(2) + (start - SYNC).powi(2) + (stop - SYNC).powi(2);
        let mut bits = 0u8;
        for i in 0..8 {
            let at = 30.0 + 30.0 * f64::from(i);
            let m = seg(at + 5.0, at + 25.0);
            if !(1000.0..=1400.0).contains(&m) {
                return;
            }
            let one = m < SYNC;
            bits |= u8::from(one) << i;
            score += (m - if one { 1100.0 } else { 1300.0 }).powi(2);
        }
        match &mut st.vis {
            Some(c) if c.score <= score => {}
            Some(c) => {
                c.score = score;
                c.start = s;
                c.bits = bits;
            }
            None => {
                st.vis = Some(VisCandidate {
                    score,
                    start: s,
                    bits,
                    until: n + st.ms(15.0) as usize,
                })
            }
        }
    }

    fn vis_found(&mut self, c: VisCandidate) {
        let st = self.state.as_mut().expect("configured");
        // The leader → start bit edge: the biggest drop between the 10 ms
        // before and after.
        let step = 1.0;
        let (mut best, mut edge) = (f64::MIN, c.start);
        let mut tau = c.start - st.ms(8.0);
        while tau <= c.start + st.ms(8.0) {
            let drop =
                st.track.mean(tau - st.ms(10.0), tau) - st.track.mean(tau, tau + st.ms(10.0));
            if drop > best {
                (best, edge) = (drop, tau);
            }
            tau += step;
        }
        let end = edge + st.ms(300.0);
        let code = c.bits & 0x7f;
        let parity_ok = c.bits.count_ones().is_multiple_of(2);
        let t = Self::secs(st, end);
        let mode = match self.forced {
            Some(m) => Some(m),
            None if !parity_ok => {
                self.msgs
                    .line(format_args!("t={t:.3} event=vis_error vis={code}"));
                None
            }
            None => {
                let m = SSTV_MODES.iter().find(|m| m.vis == code);
                if m.is_none() {
                    self.msgs
                        .line(format_args!("t={t:.3} event=unknown_mode vis={code}"));
                }
                m
            }
        };
        let Some(mode) = mode else { return };
        self.end_picture("lost");
        let st = self.state.as_mut().expect("configured");
        st.pictures += 1;
        let nominal = st.ms(mode.line);
        let origin = end + st.ms(mode.start + mode.sync_at);
        st.picture = Some(Picture {
            mode,
            scans: scans(mode),
            number: st.pictures,
            line: 0,
            origin,
            nominal,
            a: origin,
            b: nominal,
            fit_n: 0.0,
            fit_k: 0.0,
            fit_kk: 0.0,
            fit_y: 0.0,
            fit_ky: 0.0,
            first_k: 0,
            last: None,
            misses: 0,
            pending_even: None,
            last_row: None,
        });
        self.msgs.line(format_args!(
            "t={t:.3} event=start mode=\"{}\" vis={} width={} height={}",
            mode.name, mode.vis, mode.width, mode.height
        ));
    }

    /// Decode the next line if all of it has arrived.
    fn step_picture(&mut self) {
        let st = self.state.as_mut().expect("configured");
        let Some(p) = &st.picture else { return };
        let m = p.mode;
        // How far from its predicted place a sync pulse is looked for.
        let window = st.ms(3.0).max(0.01 * p.nominal);
        let expect = p.a + f64::from(p.line) * p.b;
        let line_start = expect - st.ms(m.sync_at) * p.b / p.nominal;
        let ready = line_start + p.b + window + 2.0;
        if (st.track.n as f64) < ready {
            return;
        }
        self.decode_line(window, expect);
    }

    fn decode_line(&mut self, window: f64, expect: f64) {
        let slant = self.slant;
        let st = self.state.as_mut().expect("configured");
        let rate = st.rate;
        let ms = move |x: f64| x * rate / 1000.0;
        let p = st.picture.as_mut().expect("decoding");
        let m = p.mode;
        let k = p.line;

        // The sync pulse: the span with the lowest mean frequency near where
        // it should be. Line 0's pulse follows the VIS stop bit (also 1200
        // Hz) in most modes, so there the header's timing is used. Pulses
        // are looked for even without slant correction: a picture whose
        // pulses stop has ended.
        let found = if k == 0 && m.start == 0.0 {
            Some(p.origin)
        } else {
            let len = ms(m.sync);
            let (mut best, mut at) = (f64::MAX, expect);
            let mut tau = (expect - window).floor();
            while tau <= expect + window {
                let v = st.track.mean(tau, tau + len);
                if v < best {
                    (best, at) = (v, tau);
                }
                tau += 1.0;
            }
            (best < SYNC_MAX).then_some(at)
        };
        match found {
            Some(y) => {
                p.misses = 0;
                let kf = f64::from(k);
                if p.fit_n == 0.0 {
                    p.first_k = k;
                }
                p.fit_n += 1.0;
                p.fit_k += kf;
                p.fit_kk += kf * kf;
                p.fit_y += y;
                p.fit_ky += kf * y;
                p.last = Some((k, y));
            }
            None => p.misses += 1,
        }
        // The fit is kept up without slant correction too, so the search
        // for pulses (and noticing that they stopped) follows them; only the
        // pixels then ignore it.
        if let Some((lk, ly)) = p.last {
            let spread = lk - p.first_k;
            let var = p.fit_n * p.fit_kk - p.fit_k * p.fit_k;
            let mut b = p.nominal;
            if spread >= 8 && var > 0.0 {
                let slope = (p.fit_n * p.fit_ky - p.fit_k * p.fit_y) / var;
                // More than 2 % off isn't a clock error but a bad fit.
                if (slope / p.nominal - 1.0).abs() < 0.02 {
                    b = slope;
                }
            }
            p.b = b;
            p.a = if spread >= 8 && b != p.nominal {
                (p.fit_y - b * p.fit_k) / p.fit_n
            } else {
                ly - f64::from(lk) * b
            };
        }

        // Pixels.
        let (a, b) = if slant {
            (p.a, p.b)
        } else {
            (p.origin, p.nominal)
        };
        let scale = b / p.nominal;
        let line_start = a + f64::from(k) * b - ms(m.sync_at) * scale;
        let width = usize::from(m.width);
        for s in p.scans.iter().flatten() {
            let ch = &mut st.chans[chan_index(s.chan)];
            let t0 = line_start + ms(s.at) * scale;
            let pw = ms(s.len) * scale / width as f64;
            for (i, v) in ch[..width].iter_mut().enumerate() {
                let a = t0 + i as f64 * pw;
                *v = level(st.track.mean(a, a + pw));
            }
            // A scan followed by something other than another scan (a sync
            // pulse, a separator or porch tone): the band-limited audio, and
            // our filter, smear that tone over the scan's last fraction of a
            // millisecond, which would draw a stripe down the edge (green, in
            // chroma). Hold the last clean pixel over it instead.
            let end = s.at + s.len;
            let followed = p.scans.iter().flatten().any(|o| (o.at - end).abs() < 0.01);
            if !followed {
                let held = (EDGE_GUARD_MS / (s.len / width as f64)).ceil() as usize;
                if held > 0 && held < width {
                    let keep = ch[width - 1 - held];
                    ch[width - held..width].fill(keep);
                }
            }
        }
        let number = p.number;
        let last_line = k + 1 == m.lines;
        let row = |row: u16, last: bool| Row {
            picture: number,
            row,
            height: m.height,
            width: m.width,
            channels: 3,
            flags: if last { LAST } else { 0 },
        };
        let [r, g, b, y, y2, cr, cb, c] = &st.chans;
        let out = &mut st.row;
        let mut sent = None;
        match m.color {
            Color::Rgb => {
                for i in 0..width {
                    out[3 * i..3 * i + 3].copy_from_slice(&[byte(r[i]), byte(g[i]), byte(b[i])]);
                }
                push_row(&mut self.rows, row(k, last_line), &out[..3 * width]);
                sent = Some(k);
            }
            Color::Ycc => {
                for i in 0..width {
                    out[3 * i..3 * i + 3].copy_from_slice(&ycc_rgb(y[i], cr[i], cb[i]));
                }
                push_row(&mut self.rows, row(k, last_line), &out[..3 * width]);
                sent = Some(k);
            }
            Color::Pd => {
                for (j, luma) in [y, y2].into_iter().enumerate() {
                    for i in 0..width {
                        out[3 * i..3 * i + 3].copy_from_slice(&ycc_rgb(luma[i], cr[i], cb[i]));
                    }
                    let rk = 2 * k + j as u16;
                    push_row(
                        &mut self.rows,
                        row(rk, last_line && j == 1),
                        &out[..3 * width],
                    );
                    sent = Some(rk);
                }
            }
            Color::Robot36 => {
                // The separator before the chroma: 1500 Hz before R−Y, 2300
                // Hz before B−Y. Fall back on the line number if unclear.
                let sep = st.track.mean(
                    line_start + ms(100.0) * scale,
                    line_start + ms(104.5) * scale,
                );
                let is_cr = if (sep - LEADER).abs() > 150.0 {
                    sep < LEADER
                } else {
                    k.is_multiple_of(2)
                };
                if is_cr {
                    // An even line without a partner yet: send it alone first.
                    if let Some(ek) = p.pending_even.take() {
                        robot_row(out, &st.even_y, &st.even_cr, None, width);
                        push_row(&mut self.rows, row(ek, false), &out[..3 * width]);
                        sent = Some(ek);
                    }
                    st.even_y[..width].copy_from_slice(&y[..width]);
                    st.even_cr[..width].copy_from_slice(&c[..width]);
                    p.pending_even = Some(k);
                    if last_line {
                        robot_row(out, &st.even_y, &st.even_cr, None, width);
                        push_row(&mut self.rows, row(k, true), &out[..3 * width]);
                        p.pending_even = None;
                        sent = Some(k);
                    }
                } else {
                    // A B−Y line: completes the even line before it, if any.
                    if let Some(ek) = p.pending_even.take() {
                        robot_row(out, &st.even_y, &st.even_cr, Some(c), width);
                        push_row(&mut self.rows, row(ek, false), &out[..3 * width]);
                        robot_row(out, y, &st.even_cr, Some(c), width);
                    } else {
                        // No R−Y for this line: neutral.
                        for i in 0..width {
                            out[3 * i..3 * i + 3].copy_from_slice(&ycc_rgb(y[i], 128.0, c[i]));
                        }
                    }
                    push_row(&mut self.rows, row(k, last_line), &out[..3 * width]);
                    sent = Some(k);
                }
            }
        }
        if sent.is_some() {
            p.last_row = sent;
        }
        p.line += 1;
        if last_line {
            let clock = (p.b / p.nominal - 1.0) * 1e6;
            let t = Self::secs(st, st.track.n as f64);
            self.msgs.line(format_args!(
                "t={t:.3} event=end mode=\"{}\" lines={} clock_ppm={clock:+.0}",
                m.name, m.lines
            ));
            st.picture = None;
        } else if p.misses >= MAX_MISSES {
            self.end_picture("lost");
        }
    }

    /// Give up on the picture being decoded (if any): mark its last row
    /// sent as the last, and say why.
    fn end_picture(&mut self, why: &str) {
        let st = self.state.as_mut().expect("configured");
        let Some(p) = st.picture.take() else { return };
        let m = p.mode;
        if let Some(r) = p.last_row {
            // Repeat the last row with the flag; its pixels are still in `row`.
            let width = usize::from(m.width);
            push_row(
                &mut self.rows,
                Row {
                    picture: p.number,
                    row: r,
                    height: m.height,
                    width: m.width,
                    channels: 3,
                    flags: LAST,
                },
                &st.row[..3 * width],
            );
        }
        let t = Self::secs(st, st.track.n as f64);
        self.msgs.line(format_args!(
            "t={t:.3} event={why} mode=\"{}\" lines={}",
            m.name, p.line
        ));
    }
}

/// A Robot 36 row from its luma, R−Y and (if known) B−Y.
fn robot_row(out: &mut [u8], y: &[f32], cr: &[f32], cb: Option<&[f32]>, width: usize) {
    for i in 0..width {
        let cb = cb.map_or(128.0, |c| c[i]);
        out[3 * i..3 * i + 3].copy_from_slice(&ycc_rgb(y[i], cr[i], cb));
    }
}

impl Block for SstvDecoder {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let fs = cx.input_rate(0)?;
        if fs < 6000.0 {
            return Err(Error::Other(format!(
                "a {} input is too slow for SSTV (tones up to 2300 Hz): needs at least 6 kS/s",
                fmt_rate(fs)
            )));
        }
        let decim = ((fs / 10_000.0).floor() as usize).max(1);
        let rate = fs / decim as f64;
        let taps = FirSpec {
            band: Band::LowPass(1100.0 / fs),
            transition: 500.0 / fs,
            window: DesignWindow::Kaiser { attenuation: 60.0 },
            gain: 1.0,
        }
        .design()
        .map_err(|e| design::design_error(e, fs))?;
        // Long enough for the longest line plus the VIS header and margins.
        let track = Track::new((rate * 4.0) as usize);
        self.state = Some(State {
            rate,

            mixer: Rotator::new(-LEADER / fs),
            filter: Resampler::new(1, decim, &taps),
            dec: vec![Complex32::default(); 4096],
            dec_pos: 0,
            prev: Complex32::new(1.0, 0.0),
            track,
            vis: None,
            vis_holdoff: 0,
            picture: None,
            pictures: 0,
            chans: std::array::from_fn(|_| vec![0.0; MAX_WIDTH]),
            even_y: vec![0.0; MAX_WIDTH],
            even_cr: vec![0.0; MAX_WIDTH],
            row: vec![0; 3 * MAX_WIDTH],
        });
        self.state.as_mut().expect("just set").dec.clear();
        text_meta(cx, 0);
        text_meta(cx, 1);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.state.is_none() {
            return Err(not_configured());
        }
        let (mut used, mut rows_made, mut msgs_made) = (0, 0, 0);
        loop {
            rows_made += self
                .rows
                .flush(&mut io.outputs[0].slice_mut::<u8>()[rows_made..]);
            msgs_made += self
                .msgs
                .flush(&mut io.outputs[1].slice_mut::<u8>()[msgs_made..]);
            if !self.rows.is_empty() || !self.msgs.is_empty() {
                break;
            }
            let st = self.state.as_mut().expect("checked");
            if st.dec_pos == st.dec.len() {
                // Filter the next stretch of input.
                let inp = io.inputs[0].slice::<f32>();
                if used == inp.len() {
                    break;
                }
                let cap = st.dec.capacity();
                st.dec.resize(cap, Complex32::default());
                let mixer = &mut st.mixer;
                let (took, made) = st.filter.process_with(&inp[used..], &mut st.dec, |x| {
                    mixer.next(Complex32::new(x, 0.0))
                });
                used += took;
                st.dec.truncate(made);
                st.dec_pos = 0;
                continue;
            }
            while let Some(st) = self.state.as_mut()
                && st.dec_pos < st.dec.len()
            {
                let z = st.dec[st.dec_pos];
                st.dec_pos += 1;
                self.sample(z);
                if !self.rows.is_empty() || !self.msgs.is_empty() {
                    break;
                }
            }
        }
        if used == 0 && rows_made == 0 && msgs_made == 0 {
            let st = self.state.as_ref().expect("checked");
            if io.inputs[0].exhausted()
                && st.dec_pos == st.dec.len()
                && self.rows.is_empty()
                && self.msgs.is_empty()
                && st.picture.is_some()
            {
                // The input ended in the middle of a picture.
                self.end_picture("cut");
                return Ok(WorkStatus::Ok);
            }
            let status = idle_status(io, &self.rows, 0);
            return Ok(match status {
                WorkStatus::Blocked => idle_status(io, &self.msgs, 1),
                s => s,
            });
        }
        io.consume(0, used);
        io.produce(0, rows_made);
        io.produce(1, msgs_made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "slant" => self.slant = value.as_bool().unwrap_or(true),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let forced = match p.str("mode")? {
        "auto" => None,
        id => Some(
            SSTV_MODES
                .iter()
                .find(|m| m.id == id)
                .ok_or_else(|| Error::Other(format!("unknown SSTV mode `{id}`")))?,
        ),
    };
    Ok(Instance {
        block: Box::new(SstvDecoder {
            forced,
            slant: p.bool("slant")?,
            state: None,
            rows: MsgQueue::new(),
            msgs: MsgQueue::new(),
        }),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![
            ("image".into(), StreamType::U8),
            ("out".into(), StreamType::U8),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_timings_add_up() {
        for m in SSTV_MODES {
            let sc = scans(m);
            let mut end: f64 = 0.0;
            for s in sc.iter().flatten() {
                assert!(s.at >= 0.0 && s.len > 0.0, "{}", m.id);
                end = end.max(s.at + s.len);
            }
            assert!(
                end <= m.line + 1e-9,
                "{}: scans end at {end}, line {}",
                m.id,
                m.line
            );
            assert!(usize::from(m.width) <= MAX_WIDTH);
        }
        let find = |id: &str| SSTV_MODES.iter().find(|m| m.id == id).unwrap();
        assert!((find("martin1").line - 446.446).abs() < 1e-6);
        assert!((find("scottie1").line - 428.22).abs() < 1e-6);
        assert!((find("scottie_dx").line - 1050.3).abs() < 1e-6);
        assert!((find("pd120").line - 508.48).abs() < 1e-6);
        // Scottie's R scan starts 10.5 ms after its sync pulse.
        let s1 = scans(find("scottie1"));
        assert!((s1[2].unwrap().at - 289.98).abs() < 1e-6);
        // VIS codes are unique and 7-bit.
        for (i, a) in SSTV_MODES.iter().enumerate() {
            assert!(a.vis < 128);
            assert!(SSTV_MODES[i + 1..].iter().all(|b| b.vis != a.vis));
        }
    }

    #[test]
    fn ycc_grey_and_primaries() {
        assert_eq!(ycc_rgb(128.0, 128.0, 128.0), [128, 128, 128]);
        // Pure red in full-range YCbCr: Y 76.2, Cb 85, Cr 255.5.
        let [r, g, b] = ycc_rgb(76.245, 255.5, 84.97);
        assert!(r >= 254 && g <= 1 && b <= 1, "{r} {g} {b}");
    }
}
