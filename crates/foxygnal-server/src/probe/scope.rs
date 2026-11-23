//! Oscilloscope: triggered windows of the signal, min/max-decimated to the
//! canvas width.

use foxygnal_core::Complex32;
use serde::Deserialize;

use super::Instrument;
use super::source::{Frame, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// In-phase (the value itself for real streams).
    I,
    /// Quadrature.
    Q,
    /// Magnitude |x|.
    Mag,
    /// Phase ∠x in radians.
    Phase,
}

impl Channel {
    fn id(self) -> u8 {
        match self {
            Self::I => 0,
            Self::Q => 1,
            Self::Mag => 2,
            Self::Phase => 3,
        }
    }

    fn of(self, x: Complex32) -> f32 {
        match self {
            Self::I => x.re,
            Self::Q => x.im,
            Self::Mag => x.norm(),
            Self::Phase => x.arg(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMode {
    /// Trigger when possible, free-run when no trigger comes.
    Auto,
    /// Only show triggered windows.
    Normal,
    /// Show the first triggered window, then hold (re-arm with a new `arm`).
    Single,
    /// Always show the latest window.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slope {
    Rising,
    Falling,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ScopeOptions {
    /// Window length in samples.
    pub span: u32,
    /// Output points per channel (the canvas width).
    pub points: u32,
    pub channels: Vec<Channel>,
    pub trigger: TriggerMode,
    pub slope: Slope,
    pub level: f32,
    pub trigger_channel: Channel,
    /// Fraction of the window before the trigger point.
    pub pretrigger: f32,
    /// Changing it re-arms single-shot mode.
    pub arm: u32,
}

impl Default for ScopeOptions {
    fn default() -> Self {
        Self {
            span: 1024,
            points: 1024,
            channels: vec![Channel::I, Channel::Q],
            trigger: TriggerMode::Auto,
            slope: Slope::Rising,
            level: 0.0,
            trigger_channel: Channel::I,
            pretrigger: 0.1,
            arm: 0,
        }
    }
}

pub const MAX_SPAN: usize = 1 << 20;
/// Auto mode free-runs after this many frames without a trigger.
const AUTO_PATIENCE: u32 = 3;

pub struct Scope {
    span: usize,
    points: usize,
    pre: usize,
    channels: Vec<Channel>,
    mode: TriggerMode,
    slope: Slope,
    level: f32,
    tch: Channel,
    /// Contiguous recent samples; `hist[0]` is stream index `start`.
    hist: Vec<Complex32>,
    start: usize,
    /// Stream index of the last trigger shown.
    last: Option<usize>,
    misses: u32,
    held: bool,
}

impl Scope {
    pub fn new(o: &ScopeOptions, complex: bool) -> Self {
        let span = (o.span as usize).clamp(2, MAX_SPAN);
        let mut channels: Vec<Channel> = o.channels.iter().copied().take(4).collect();
        if !complex {
            // A real stream has one channel.
            channels = vec![Channel::I];
        }
        Self {
            span,
            points: (o.points as usize).clamp(16, 8192),
            pre: ((span as f32 * o.pretrigger.clamp(0.0, 1.0)) as usize).min(span - 1),
            channels,
            mode: o.trigger,
            slope: o.slope,
            level: o.level,
            tch: if complex {
                o.trigger_channel
            } else {
                Channel::I
            },
            hist: Vec::new(),
            start: 0,
            last: None,
            misses: 0,
            held: false,
        }
    }

    fn end(&self) -> usize {
        self.start + self.hist.len()
    }

    /// The latest trigger point whose whole window is available.
    fn find_trigger(&self) -> Option<usize> {
        // `lo - 1` is read too, to see the first crossing.
        let lo = self.start + self.pre.max(1);
        let hi = (self.end() + self.pre).checked_sub(self.span)?;
        if hi <= lo {
            return None;
        }
        let vals = || {
            self.hist[lo - 1 - self.start..=hi - self.start]
                .iter()
                .map(|&x| self.tch.of(x))
        };
        // Hysteresis keeps noise around the level from re-triggering.
        let (min, max) = vals().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
            (a.min(v), b.max(v))
        });
        let h = (max - min) * 0.02;
        let (level, sign) = match self.slope {
            Slope::Rising => (self.level, 1.0),
            Slope::Falling => (-self.level, -1.0),
        };
        let mut armed = false;
        let mut found = None;
        for (i, v) in vals().enumerate() {
            let v = v * sign;
            if v < level - h {
                armed = true;
            } else if armed && v >= level && i > 0 {
                armed = false;
                found = Some(lo - 1 + i);
            }
        }
        found.filter(|&t| self.last.is_none_or(|l| t > l))
    }

    fn write(&self, at: usize, trigger: Option<usize>, out: &mut Vec<u8>) {
        let w = &self.hist[at - self.start..at - self.start + self.span];
        let minmax = self.span > self.points;
        let points = if minmax { self.points } else { self.span };
        let mut ids = [0u8; 4];
        for (d, c) in ids.iter_mut().zip(&self.channels) {
            *d = c.id();
        }
        let mut f = Frame(out);
        f.u32(self.span as u32)
            .i32(trigger.map_or(-1, |t| (t - at) as i32))
            .bytes4([
                u8::from(trigger.is_some()) | u8::from(minmax) << 1,
                self.channels.len() as u8,
                0,
                0,
            ])
            .u32(points as u32)
            .bytes4(ids);
        for &c in &self.channels {
            if minmax {
                for p in 0..points {
                    let chunk = &w[p * self.span / points..(p + 1) * self.span / points];
                    let (lo, hi) = chunk
                        .iter()
                        .map(|&x| c.of(x))
                        .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
                            (a.min(v), b.max(v))
                        });
                    f.f32(lo).f32(hi);
                }
            } else {
                f.f32s(w.iter().map(|&x| c.of(x)));
            }
        }
    }
}

impl Instrument for Scope {
    fn poll(&mut self, src: &mut Source) {
        let max = self.span * 2 + (1 << 16);
        let mut fresh = Vec::new();
        let contiguous = src.fresh(max, &mut fresh);
        if !contiguous {
            // A gap: older samples don't line up with the new ones. Indices
            // stay monotonic so `last` keeps working.
            self.start = self.end();
            self.hist.clear();
        }
        self.hist.extend_from_slice(&fresh);
        // Keep enough for a trigger near the end to complete later.
        let keep = self.span * 2 + (1 << 16);
        if self.hist.len() > keep {
            let drop = self.hist.len() - keep;
            self.hist.drain(..drop);
            self.start += drop;
        }
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> bool {
        if self.held || self.hist.len() < self.span {
            return false;
        }
        let latest = self.end() - self.span;
        let (at, trigger) = match (self.mode, self.find_trigger()) {
            (TriggerMode::Off, _) => (latest, None),
            (_, Some(t)) => {
                self.last = Some(t);
                self.misses = 0;
                self.held = self.mode == TriggerMode::Single;
                (t - self.pre, Some(t))
            }
            (TriggerMode::Auto, None) => {
                self.misses += 1;
                if self.misses < AUTO_PATIENCE {
                    return false;
                }
                (latest, None)
            }
            (TriggerMode::Normal | TriggerMode::Single, None) => return false,
        };
        self.write(at, trigger, out);
        // Older samples can't be part of a later window.
        let drop = at.saturating_sub(self.start).min(self.hist.len());
        self.hist.drain(..drop);
        self.start += drop;
        true
    }
}
