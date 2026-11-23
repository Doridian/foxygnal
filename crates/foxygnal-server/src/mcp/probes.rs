//! MCP's eyes on the running graph: one more client of the probe hub.
//!
//! Probes are set on the session's [`Hub`](crate::probe::Hub) under the
//! client id [`MCP_CLIENT`], exactly like an instrument in a browser tab, so
//! they bind to new runs, rewind to the history the ring holds, and never slow
//! the graph down. The frames they send come back here and are folded into a
//! [`Collected`] per probe: spectra averaged, levels accumulated, text lines
//! and picture rows kept. Tools then turn that into summaries.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use foxygnal_core::Complex32;
use foxygnal_core::raster::{self, Next};
use foxygnal_dsp::{PowerSpectrum, Window};
use tokio::sync::mpsc;

use crate::probe::{CLIENT_QUEUE, Outgoing};
use crate::session::Session;

/// The probe hub's client id for MCP (browser tabs count up from 1).
pub const MCP_CLIENT: u64 = u64::MAX;

/// Lines of text kept per probe.
const MAX_LINES: usize = 5000;
/// Pictures kept per probe.
const MAX_PICTURES: usize = 4;

#[derive(Default)]
pub struct Spectrum {
    pub fft_size: u32,
    pub complex: bool,
    /// Sum of linear power per bin over `frames`, and the highest seen.
    pub sum: Vec<f64>,
    pub max: Vec<f32>,
    pub frames: u32,
}

#[derive(Default)]
pub struct Level {
    pub samples: u64,
    pub sum_sq: f64,
    pub peak: f32,
    pub sum_re: f64,
    pub sum_im: f64,
    pub complex: bool,
}

pub struct Picture {
    pub number: u32,
    pub width: u16,
    pub channels: u8,
    pub height: u16,
    pub pixels: Vec<u8>,
    pub rows: u32,
    pub last_row: u16,
    pub done: bool,
}

impl Picture {
    /// Rows to show: the promised height, or as far as rows arrived.
    pub fn shown_height(&self) -> u16 {
        if self.height > 0 {
            self.height
        } else {
            self.last_row + 1
        }
    }
}

/// Points kept for a constellation summary.
const MAX_POINTS: usize = 1 << 16;
/// FFT size for the symbol-rate line in |x|².
pub const CYCLO_FFT: usize = 1024;

/// I/Q points, and the spectrum of |x|² (whose strongest line, for a
/// linearly modulated signal with more than one sample per symbol, sits at
/// the symbol rate).
#[derive(Default)]
pub struct Constellation {
    pub points: VecDeque<Complex32>,
    /// Summed power spectrum of |x|² − mean, in FFT order.
    pub cyclo: Vec<f64>,
    pub cyclo_frames: u32,
    ps: Option<PowerSpectrum>,
}

impl Constellation {
    fn take(&mut self, pts: &[Complex32]) {
        // Every whole chunk of the frame (frames needn't follow on from
        // each other, so chunks don't straddle them).
        for seg in pts.as_chunks::<CYCLO_FFT>().0 {
            let mean = seg.iter().map(|z| z.norm_sqr()).sum::<f32>() / CYCLO_FFT as f32;
            let env: Vec<Complex32> = seg
                .iter()
                .map(|z| Complex32::new(z.norm_sqr() - mean, 0.0))
                .collect();
            let ps = self
                .ps
                .get_or_insert_with(|| PowerSpectrum::new(CYCLO_FFT, Window::BlackmanHarris));
            let mut out = vec![0.0; CYCLO_FFT];
            ps.compute(&env, &mut out);
            if self.cyclo.len() != CYCLO_FFT {
                self.cyclo = vec![0.0; CYCLO_FFT];
            }
            for (a, b) in self.cyclo.iter_mut().zip(&out) {
                *a += f64::from(*b);
            }
            self.cyclo_frames += 1;
        }
        for &z in pts {
            if self.points.len() == MAX_POINTS {
                self.points.pop_front();
            }
            self.points.push_back(z);
        }
    }
}

#[derive(Default)]
pub struct Collected {
    /// Why the probe can't run (not running, wrong wire type, …).
    pub error: Option<String>,
    pub frames: u32,
    pub spectrum: Spectrum,
    pub level: Level,
    pub constellation: Constellation,
    pub lines: VecDeque<String>,
    /// Lines (or rows) were lost somewhere along the way.
    pub gap: bool,
    pub pictures: VecDeque<Picture>,
}

impl Collected {
    fn take_frame(&mut self, kind: u8, b: &[u8]) {
        self.frames += 1;
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let f32_at = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        match kind {
            // Spectrum: fft size, bins, flags, dB per bin.
            2 if b.len() >= 12 => {
                let (fft, bins, complex) = (u32_at(0), u32_at(4) as usize, b[8] & 1 != 0);
                if b.len() < 12 + 4 * bins {
                    return;
                }
                let s = &mut self.spectrum;
                if s.sum.len() != bins || s.fft_size != fft {
                    *s = Spectrum {
                        fft_size: fft,
                        complex,
                        sum: vec![0.0; bins],
                        max: vec![f32::MIN; bins],
                        frames: 0,
                    };
                }
                for i in 0..bins {
                    let db = f32_at(12 + 4 * i);
                    s.sum[i] += 10f64.powf(f64::from(db) / 10.0);
                    s.max[i] = s.max[i].max(db);
                }
                s.frames += 1;
            }
            // Constellation: count, then I, Q pairs.
            3 if b.len() >= 4 => {
                let n = (u32_at(0) as usize).min((b.len() - 4) / 8);
                let pts: Vec<Complex32> = (0..n)
                    .map(|i| Complex32::new(f32_at(4 + 8 * i), f32_at(8 + 8 * i)))
                    .collect();
                self.constellation.take(&pts);
            }
            // Meter: count, flags, rms, peak, mean re, mean im.
            4 if b.len() >= 24 => {
                let n = u64::from(u32_at(0));
                let l = &mut self.level;
                l.complex = b[4] & 1 != 0;
                let rms = f64::from(f32_at(8));
                l.sum_sq += rms * rms * n as f64;
                l.peak = l.peak.max(f32_at(12));
                l.sum_re += f64::from(f32_at(16)) * n as f64;
                l.sum_im += f64::from(f32_at(20)) * n as f64;
                l.samples += n;
            }
            // Text: length, flags, the lines.
            6 if b.len() >= 8 => {
                let n = (u32_at(0) as usize).min(b.len() - 8);
                if b[4] & 1 != 0 {
                    self.gap = true;
                }
                let text = String::from_utf8_lossy(&b[8..8 + n]);
                for line in text.lines() {
                    if self.lines.len() == MAX_LINES {
                        self.lines.pop_front();
                        self.gap = true;
                    }
                    self.lines.push_back(line.to_owned());
                }
            }
            // Image: row records, each padded to 4 bytes.
            7 if b.len() >= 8 => {
                if b[4] & 1 != 0 {
                    self.gap = true;
                }
                let mut at = 8;
                while at < b.len() {
                    let Next::Row(row, len) = raster::next(&b[at..]) else {
                        break;
                    };
                    self.take_row(row, &b[at + raster::HEADER..at + len]);
                    at += len.next_multiple_of(4);
                }
            }
            _ => {}
        }
    }

    fn take_row(&mut self, r: raster::Row, px: &[u8]) {
        let same = self.pictures.back().is_some_and(|p| {
            p.number == r.picture && p.width == r.width && p.channels == r.channels
        });
        if !same {
            if let Some(p) = self.pictures.back_mut() {
                p.done = true;
            }
            if self.pictures.len() == MAX_PICTURES {
                self.pictures.pop_front();
            }
            self.pictures.push_back(Picture {
                number: r.picture,
                width: r.width,
                channels: r.channels,
                height: r.height,
                pixels: Vec::new(),
                rows: 0,
                last_row: 0,
                done: false,
            });
        }
        let p = self.pictures.back_mut().expect("just pushed");
        let stride = r.pixel_bytes();
        let at = usize::from(r.row) * stride;
        let need = (at + stride).max(usize::from(p.height) * stride);
        if p.pixels.len() < need {
            p.pixels.resize(need, 0);
        }
        p.pixels[at..at + stride].copy_from_slice(px);
        p.rows += 1;
        p.last_row = p.last_row.max(r.row);
        if r.is_last() {
            p.done = true;
        }
    }
}

/// Every MCP probe's data, by probe id.
#[derive(Default)]
pub struct Probes {
    data: Mutex<HashMap<u32, Collected>>,
    next_id: Mutex<u32>,
}

impl Probes {
    /// Register as a hub client and start taking its frames.
    pub fn start(session: &Arc<Session>) -> Arc<Self> {
        let probes = Arc::new(Self::default());
        let (tx, mut rx) = mpsc::channel(CLIENT_QUEUE * 4);
        session.hub().add_client(MCP_CLIENT, tx);
        let weak = Arc::downgrade(&probes);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Some(p) = weak.upgrade() else { return };
                p.take(msg);
            }
        });
        probes
    }

    pub fn lock(&self) -> MutexGuard<'_, HashMap<u32, Collected>> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh probe id with an empty collection.
    pub fn new_id(&self) -> u32 {
        let mut n = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.wrapping_add(1);
        self.lock().insert(*n, Collected::default());
        *n
    }

    fn take(&self, msg: Outgoing) {
        match msg {
            // Probe status: `{"op":"probe","id":…,"error":…}`.
            Outgoing::Text(t) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else {
                    return;
                };
                let Some(id) = v["id"].as_u64() else { return };
                if let Some(c) = self.lock().get_mut(&(id as u32)) {
                    c.error = v["error"].as_str().map(str::to_owned);
                }
            }
            // Header: probe id, sequence number, kind, 3 bytes padding.
            Outgoing::Binary(b) if b.len() >= 12 => {
                let id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if let Some(c) = self.lock().get_mut(&id) {
                    c.take_frame(b[8], &b[12..]);
                }
            }
            Outgoing::Binary(_) => {}
        }
    }
}
