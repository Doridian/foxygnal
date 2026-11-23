//! Probes: instruments attached to wires of the running graph.
//!
//! A probe reads a lossy [`Tap`](foxygnal_core::Tap) on the wire's output, so
//! it never slows the graph down. One thread per session polls every probe,
//! computes display-ready frames (scope windows, spectra, …) and sends them as
//! binary WebSocket frames to the tab that asked. The same thread samples the
//! graph's counters for the `stats` event.
//!
//! Binary frame layout (little-endian, every section padded to 4 bytes):
//! `u32 probe_id, u32 seq, u8 kind, u8[3] 0`, then the kind's payload; see
//! the instrument modules and docs/ARCHITECTURE.md.

mod constellation;
mod meter;
mod scope;
mod source;
mod spectrum;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use foxygnal_core::{GraphCounters, Monitor, StreamType};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::session::{EdgeRef, Session};
pub use constellation::ConstellationOptions;
pub use scope::ScopeOptions;
use source::{Frame, Source};
pub use spectrum::SpectrumOptions;

/// What a probe shows, with its options.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeConfig {
    Scope(ScopeOptions),
    Spectrum(SpectrumOptions),
    Constellation(ConstellationOptions),
    Meter {},
}

impl ProbeConfig {
    fn kind_id(&self) -> u8 {
        match self {
            Self::Scope(_) => 1,
            Self::Spectrum(_) => 2,
            Self::Constellation(_) => 3,
            Self::Meter {} => 4,
        }
    }

    fn build(&self, ty: StreamType) -> Result<Box<dyn Instrument>, String> {
        let complex = ty == StreamType::C32;
        Ok(match self {
            Self::Scope(o) => Box::new(scope::Scope::new(o, complex)),
            Self::Spectrum(o) => Box::new(spectrum::Spectrum::new(o, complex)),
            Self::Constellation(_) if !complex => {
                return Err(format!(
                    "A constellation needs a complex (c32) wire; this one carries {ty}."
                ));
            }
            Self::Constellation(o) => Box::new(constellation::Constellation::new(o)),
            Self::Meter {} => Box::new(meter::Meter::new(complex)),
        })
    }
}

/// One instrument's engine side.
trait Instrument: Send {
    /// Take in new samples. Called on every tick so streams stay contiguous.
    fn poll(&mut self, src: &mut Source);
    /// Append a frame payload to `out`; false if there's nothing new to show.
    fn frame(&mut self, out: &mut Vec<u8>) -> bool;
}

/// A message for one tab.
pub enum Outgoing {
    Text(Arc<str>),
    Binary(Vec<u8>),
}

/// Per-tab messages in flight. A slow tab drops frames instead of piling up.
pub const CLIENT_QUEUE: usize = 16;

struct Bound {
    run: u64,
    src: Source,
    inst: Box<dyn Instrument>,
}

struct Probe {
    edge: EdgeRef,
    config: ProbeConfig,
    period: Duration,
    bound: Option<Bound>,
    error: Option<String>,
    /// The error state the client last heard about.
    reported: Option<Option<String>>,
    next_frame: Instant,
    seq: u32,
}

#[derive(Default)]
struct HubInner {
    clients: HashMap<u64, mpsc::Sender<Outgoing>>,
    probes: BTreeMap<(u64, u32), Probe>,
}

/// All probes of a session, keyed by (client, probe id).
#[derive(Default)]
pub struct Hub {
    inner: Mutex<HubInner>,
}

#[derive(Serialize)]
#[serde(tag = "op", rename = "probe")]
struct ProbeStatus<'a> {
    id: u32,
    error: Option<&'a str>,
}

impl Hub {
    fn lock(&self) -> MutexGuard<'_, HubInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_client(&self, client: u64, tx: mpsc::Sender<Outgoing>) {
        self.lock().clients.insert(client, tx);
    }

    /// Forget a tab and its probes.
    pub fn remove_client(&self, client: u64) {
        let mut h = self.lock();
        h.clients.remove(&client);
        h.probes.retain(|(c, _), _| *c != client);
    }

    /// Add a probe, or change one. A changed probe starts over (clears its
    /// averages, re-arms single shot).
    pub fn set(&self, client: u64, id: u32, edge: EdgeRef, config: ProbeConfig, fps: f32) {
        let period = Duration::from_secs_f32(1.0 / fps.clamp(1.0, 60.0));
        let mut h = self.lock();
        if let Some(p) = h.probes.get_mut(&(client, id)) {
            if p.edge == edge && p.config == config {
                p.period = period;
                return;
            }
        }
        h.probes.insert(
            (client, id),
            Probe {
                edge,
                config,
                period,
                bound: None,
                error: None,
                reported: None,
                next_frame: Instant::now(),
                seq: 0,
            },
        );
    }

    pub fn remove(&self, client: u64, id: u32) {
        self.lock().probes.remove(&(client, id));
    }

    /// Poll every probe; send the frames that are due.
    fn tick(&self, run: Option<(u64, &Monitor)>, now: Instant) {
        let mut h = self.lock();
        let HubInner { clients, probes } = &mut *h;
        let mut buf = Vec::new();
        for (&(client, id), p) in probes.iter_mut() {
            match run {
                None => {
                    p.bound = None;
                    p.error = None;
                }
                Some((run_id, mon)) if p.bound.as_ref().is_none_or(|b| b.run != run_id) => {
                    p.bound = None;
                    p.error = None;
                    match bind(mon, &p.edge, &p.config) {
                        Ok((src, inst)) => {
                            p.bound = Some(Bound {
                                run: run_id,
                                src,
                                inst,
                            })
                        }
                        Err(e) => p.error = Some(e),
                    }
                }
                Some(_) => {}
            }
            let Some(tx) = clients.get(&client) else {
                continue;
            };
            if p.reported.as_ref() != Some(&p.error) {
                let msg = serde_json::to_string(&ProbeStatus {
                    id,
                    error: p.error.as_deref(),
                })
                .unwrap_or_default();
                if tx.try_send(Outgoing::Text(msg.into())).is_ok() {
                    p.reported = Some(p.error.clone());
                }
            }
            let Some(b) = &mut p.bound else { continue };
            b.inst.poll(&mut b.src);
            if now < p.next_frame {
                continue;
            }
            // Don't try to catch up on missed frames.
            p.next_frame = (p.next_frame + p.period).max(now);
            buf.clear();
            Frame(&mut buf)
                .u32(id)
                .u32(p.seq)
                .bytes4([p.config.kind_id(), 0, 0, 0]);
            if b.inst.frame(&mut buf) {
                p.seq = p.seq.wrapping_add(1);
                // A full queue means the tab is slow: drop this frame.
                let _ = tx.try_send(Outgoing::Binary(std::mem::take(&mut buf)));
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.lock().probes.is_empty()
    }
}

fn bind(
    mon: &Monitor,
    edge: &EdgeRef,
    config: &ProbeConfig,
) -> Result<(Source, Box<dyn Instrument>), String> {
    let missing = || {
        format!(
            "`{} → {}` isn't in the running graph. Restart to watch it.",
            edge.from, edge.to
        )
    };
    let (block, port) = edge.from.rsplit_once('.').ok_or_else(missing)?;
    let (ty, tap) = mon.tap(block, port).ok_or_else(missing)?;
    let inst = config.build(ty)?;
    Ok((Source::new(tap, ty), inst))
}

/// Per-block and per-wire load, sent about twice a second while running.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub blocks: Vec<BlockStat>,
    pub edges: Vec<EdgeStat>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockStat {
    pub block: String,
    /// Fraction of one core spent in `work` (0…1, a bit more if it overlaps).
    pub cpu: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeStat {
    pub from: String,
    pub to: String,
    /// Items per second.
    pub rate: f64,
    /// Items produced so far.
    pub items: u64,
    /// Buffer fill for this input, 0…1. Near 1 means the input's block is the
    /// bottleneck.
    pub fill: f32,
}

pub fn stats(prev: &GraphCounters, now: &GraphCounters, dt: Duration) -> Stats {
    let secs = dt.as_secs_f64().max(1e-6);
    Stats {
        blocks: now
            .blocks
            .iter()
            .map(|b| {
                let before = prev
                    .blocks
                    .iter()
                    .find(|p| p.block == b.block)
                    .map_or(b.busy_ns, |p| p.busy_ns);
                BlockStat {
                    block: b.block.clone(),
                    cpu: ((b.busy_ns.saturating_sub(before)) as f64 / 1e9 / secs) as f32,
                }
            })
            .collect(),
        edges: now
            .edges
            .iter()
            .map(|e| {
                let before = prev
                    .edges
                    .iter()
                    .find(|p| p.from == e.from && p.to == e.to)
                    .map_or(e.written, |p| p.written);
                EdgeStat {
                    from: e.from.clone(),
                    to: e.to.clone(),
                    rate: e.written.saturating_sub(before) as f64 / secs,
                    items: e.written,
                    fill: e.backlog as f32 / e.capacity.max(1) as f32,
                }
            })
            .collect(),
    }
}

const TICK: Duration = Duration::from_millis(16);
const IDLE_TICK: Duration = Duration::from_millis(100);
const STATS_EVERY: Duration = Duration::from_millis(500);

/// The session's probe thread. Exits when the session is dropped.
pub(crate) fn spawn(session: Weak<Session>) {
    std::thread::Builder::new()
        .name("fox:probes".into())
        .spawn(move || {
            let mut last: Option<(u64, GraphCounters, Instant)> = None;
            loop {
                let Some(s) = session.upgrade() else { return };
                let run = s.monitor();
                let now = Instant::now();
                s.hub().tick(run.as_ref().map(|(id, m)| (*id, m)), now);
                match &run {
                    None => last = None,
                    Some((id, mon)) => match &last {
                        Some((lid, prev, at)) if lid == id => {
                            if now.duration_since(*at) >= STATS_EVERY {
                                let counters = mon.stats();
                                s.emit_stats(stats(prev, &counters, now.duration_since(*at)));
                                last = Some((*id, counters, now));
                            }
                        }
                        _ => last = Some((*id, mon.stats(), now)),
                    },
                }
                // Stats alone don't need a fast tick.
                let busy = run.is_some() && !s.hub().is_empty();
                drop(s);
                std::thread::sleep(if busy { TICK } else { IDLE_TICK });
            }
        })
        .expect("spawning the probe thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use foxygnal_core::Complex32;
    use foxygnal_core::buffer::{self, Writer};

    /// A c32 ring with a tap-backed source attached, and a way to feed it.
    fn rig(cap: usize) -> (Writer, Vec<buffer::Reader>, Source) {
        let (w, rs) = buffer::channel(8, cap, 1);
        let src = Source::new(w.tap(), StreamType::C32);
        (w, rs, src)
    }

    fn feed(w: &mut Writer, rs: &mut [buffer::Reader], xs: &[Complex32]) {
        let mut done = 0;
        while done < xs.len() {
            let buf = w.slice_mut();
            let n = (buf.len() / 8).min(xs.len() - done);
            for (i, x) in xs[done..done + n].iter().enumerate() {
                buf[i * 8..i * 8 + 4].copy_from_slice(&x.re.to_le_bytes());
                buf[i * 8 + 4..i * 8 + 8].copy_from_slice(&x.im.to_le_bytes());
            }
            w.commit(n);
            done += n;
            for r in rs.iter_mut() {
                let k = r.slice().len() / 8;
                r.consume(k);
            }
        }
    }

    fn f32s(b: &[u8]) -> Vec<f32> {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn tone(n: usize, period: f32, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| Complex32::from_polar(amp, std::f32::consts::TAU * i as f32 / period))
            .collect()
    }

    #[test]
    fn scope_triggers_on_a_rising_edge() {
        let (mut w, mut rs, mut src) = rig(4096);
        let o = ScopeOptions {
            span: 200,
            points: 400,
            channels: vec![scope::Channel::I],
            trigger: scope::TriggerMode::Normal,
            pretrigger: 0.25,
            level: 0.5,
            ..ScopeOptions::default()
        };
        let mut s = scope::Scope::new(&o, true);
        // Phase-shifted so the window can't start at a crossing by accident.
        let xs: Vec<_> = tone(1000, 100.0, 1.0).into_iter().skip(13).collect();
        feed(&mut w, &mut rs, &xs);
        s.poll(&mut src);
        let mut out = Vec::new();
        assert!(s.frame(&mut out));
        assert_eq!(u32_at(&out, 0), 200, "span");
        assert_eq!(u32_at(&out, 4), 50, "trigger at the pretrigger point");
        assert_eq!(out[8], 1, "triggered, not min/max");
        assert_eq!(u32_at(&out, 12), 200, "raw points: span < width");
        let v = f32s(&out[20..]);
        assert_eq!(v.len(), 200);
        assert!(
            v[49] < 0.5 && v[50] >= 0.5,
            "crosses the level at the trigger"
        );
        assert!(v[51] > v[50], "rising");
        // The same edge isn't shown twice.
        out.clear();
        assert!(!s.frame(&mut out));
    }

    #[test]
    fn scope_decimates_with_min_max() {
        let (mut w, mut rs, mut src) = rig(1 << 15);
        let o = ScopeOptions {
            span: 10_000,
            points: 100,
            trigger: scope::TriggerMode::Off,
            ..ScopeOptions::default()
        };
        let mut s = scope::Scope::new(&o, true);
        feed(&mut w, &mut rs, &tone(12_000, 50.0, 0.5));
        s.poll(&mut src);
        let mut out = Vec::new();
        assert!(s.frame(&mut out));
        assert_eq!(out[8], 0b10, "min/max, untriggered");
        assert_eq!(out[9], 2, "I and Q");
        assert_eq!(u32_at(&out, 12), 100);
        let v = f32s(&out[20..]);
        assert_eq!(v.len(), 2 * 2 * 100);
        // Each column spans two periods, so it covers the whole ±0.5 swing.
        assert!(v[..200].chunks(2).all(|mm| mm[0] < -0.49 && mm[1] > 0.49));
    }

    #[test]
    fn spectrum_finds_the_tone() {
        let (mut w, mut rs, mut src) = rig(1 << 15);
        let o = SpectrumOptions {
            fft_size: 1024,
            bins: 1024,
            peak_hold: true,
            ..SpectrumOptions::default()
        };
        let mut sp = spectrum::Spectrum::new(&o, true);
        // A tone at +1/8 of the sample rate, half scale (−6 dBFS).
        feed(&mut w, &mut rs, &tone(4096, 8.0, 0.5));
        sp.poll(&mut src);
        let mut out = Vec::new();
        assert!(sp.frame(&mut out));
        assert_eq!(u32_at(&out, 0), 1024);
        assert_eq!(u32_at(&out, 4), 1024);
        assert_eq!(out[8], 0b11, "complex, with peak hold");
        let db = f32s(&out[12..]);
        assert_eq!(db.len(), 2048);
        let (peak, max) = db[..1024]
            .iter()
            .enumerate()
            .fold((0, f32::MIN), |b, (i, &v)| if v > b.1 { (i, v) } else { b });
        assert_eq!(peak, 512 + 128, "centred: DC is the middle bin");
        assert!((max + 6.02).abs() < 0.1, "{max} dB");
    }

    #[test]
    fn meter_measures_rms_and_peak() {
        let (mut w, mut rs, mut src) = rig(4096);
        let mut m = meter::Meter::new(true);
        feed(&mut w, &mut rs, &tone(1000, 10.0, 0.5));
        m.poll(&mut src);
        let mut out = Vec::new();
        assert!(m.frame(&mut out));
        assert_eq!(u32_at(&out, 0), 1000);
        let v = f32s(&out[8..]);
        assert!((v[0] - 0.5).abs() < 1e-4, "rms {}", v[0]);
        assert!((v[1] - 0.5).abs() < 1e-4, "peak {}", v[1]);
        assert!(v[2].abs() < 1e-3 && v[3].abs() < 1e-3, "no DC");
        out.clear();
        assert!(!m.frame(&mut out), "nothing new");
    }

    #[test]
    fn constellation_wants_complex() {
        let c = ProbeConfig::Constellation(ConstellationOptions::default());
        assert!(c.build(StreamType::F32).is_err());
        assert!(c.build(StreamType::C32).is_ok());
    }

    #[test]
    fn probe_config_parses_with_defaults() {
        let c: ProbeConfig = serde_json::from_str(r#"{"kind":"scope","span":4096}"#).unwrap();
        let ProbeConfig::Scope(o) = c else { panic!() };
        assert_eq!(o.span, 4096);
        assert_eq!(o.points, 1024);
        let c: ProbeConfig = serde_json::from_str(r#"{"kind":"meter"}"#).unwrap();
        assert_eq!(c, ProbeConfig::Meter {});
    }
}
