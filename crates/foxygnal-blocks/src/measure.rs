//! Measurements that turn a stream into a slow stream of numbers, typically
//! wired into another block's setting (a control input, `block.@param`).

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{PowerSpectrum, Window};
use serde_json::json;

use crate::params;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "peak_frequency",
            label: "Peak Frequency",
            category: "Measurement",
            help: "Finds the strongest signal in a complex stream and outputs its frequency \
                   offset from the centre, in Hz, a few times a second. Averages FFT power \
                   spectra over each update; takes the power-weighted centre of the peak over \
                   the signal bandwidth; keeps the current signal unless another is stronger \
                   by the hysteresis; holds the last value while nothing stands out by the \
                   minimum SNR. Wire it into a setting such as `freq_xlating_fir.@offset` to \
                   follow a signal. Outputs nothing until it first finds a signal.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::C32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::F32),
            }],
            params: vec![
                ParamSpec {
                    name: "fft_size",
                    label: "FFT size",
                    kind: ParamKind::Int {
                        min: Some(64),
                        max: Some(65_536),
                    },
                    default: json!(4096),
                    help: "Bins per spectrum: the frequency resolution is the sample rate / \
                           FFT size.",
                    live: false,
                },
                ParamSpec {
                    name: "update_rate",
                    label: "Updates",
                    kind: ParamKind::Float {
                        min: Some(0.01),
                        max: None,
                        unit: Some("/s"),
                    },
                    default: json!(10.0),
                    help: "Values per second. Every spectrum in between is averaged.",
                    live: false,
                },
                params::float(
                    "min_offset",
                    "Search from",
                    0.0,
                    Some("Hz"),
                    "Lowest offset to search (from the centre). With both limits 0 the whole \
                     band is searched.",
                )
                .live(),
                params::float(
                    "max_offset",
                    "Search to",
                    0.0,
                    Some("Hz"),
                    "Highest offset to search.",
                )
                .live(),
                ParamSpec {
                    name: "bandwidth",
                    label: "Signal width",
                    kind: ParamKind::Float {
                        min: Some(0.0),
                        max: None,
                        unit: Some("Hz"),
                    },
                    default: json!(0.0),
                    help: "Width of the wanted signal (e.g. 150 kHz for broadcast FM). The \
                           frequency is the centre of the power within it, and signals are \
                           compared by that power. 0 = a few bins around the peak (a carrier).",
                    live: true,
                },
                params::float(
                    "min_snr",
                    "Min. SNR",
                    10.0,
                    Some("dB"),
                    "How far the peak must stand above the noise floor (the median bin) to \
                     count. Below it, the last value is held.",
                )
                .live(),
                ParamSpec {
                    name: "hysteresis",
                    label: "Hysteresis",
                    kind: ParamKind::Float {
                        min: Some(0.0),
                        max: None,
                        unit: Some("dB"),
                    },
                    default: json!(3.0),
                    help: "Another signal must be this much stronger than the current one \
                           before the output jumps to it.",
                    live: true,
                },
            ],
        },
        build,
    );
}

/// Tunables that may change while running.
#[derive(Debug, Clone, Copy)]
struct Settings {
    min_offset: f64,
    max_offset: f64,
    bandwidth: f64,
    /// Linear power ratios.
    min_snr: f32,
    hysteresis: f32,
}

pub struct PeakFrequency {
    size: usize,
    update_rate: f64,
    set: Settings,
    rate: f64,
    /// Spectra averaged per output value.
    per_update: usize,
    ps: PowerSpectrum,
    /// The frame being filled.
    frame: Vec<Complex32>,
    filled: usize,
    /// One power spectrum.
    power: Vec<f32>,
    /// Sum of the spectra of this update, centred (index `size / 2` is DC).
    acc: Vec<f32>,
    frames: usize,
    /// Scratch for the median.
    scratch: Vec<f32>,
    /// The offset being output (Hz), once a signal was found.
    held: Option<f64>,
    /// A value waiting for room in the output.
    pending: Option<f32>,
}

impl PeakFrequency {
    fn new(size: usize, update_rate: f64, set: Settings) -> Self {
        Self {
            size,
            update_rate,
            set,
            rate: 0.0,
            per_update: 1,
            ps: PowerSpectrum::new(size, Window::Hann),
            frame: vec![Complex32::default(); size],
            filled: 0,
            power: vec![0.0; size],
            acc: vec![0.0; size],
            frames: 0,
            scratch: vec![0.0; size],
            held: None,
            pending: None,
        }
    }

    /// Centred bin index → offset in Hz.
    fn freq(&self, j: f64) -> f64 {
        (j - (self.size / 2) as f64) * self.rate / self.size as f64
    }

    /// Offset in Hz → centred bin index (fractional).
    fn bin(&self, f: f64) -> f64 {
        f * self.size as f64 / self.rate + (self.size / 2) as f64
    }

    /// The centred bins to search, inclusive.
    fn range(&self) -> (usize, usize) {
        let last = self.size - 1;
        if self.set.min_offset == 0.0 && self.set.max_offset == 0.0 {
            return (0, last);
        }
        let (a, b) = if self.set.min_offset <= self.set.max_offset {
            (self.set.min_offset, self.set.max_offset)
        } else {
            (self.set.max_offset, self.set.min_offset)
        };
        let lo = self.bin(a).ceil().clamp(0.0, last as f64) as usize;
        let hi = self.bin(b).floor().clamp(0.0, last as f64) as usize;
        (lo, hi.max(lo))
    }

    /// Half the signal width in bins (at least 1).
    fn half_width(&self) -> usize {
        ((self.set.bandwidth / 2.0 * self.size as f64 / self.rate).round() as usize).max(1)
    }

    /// Power above the floor around centred bin `c`, and its centre of mass.
    fn window(&self, c: usize, lo: usize, hi: usize, floor: f32) -> (f32, f64) {
        let h = self.half_width();
        let (a, b) = (c.saturating_sub(h).max(lo), (c + h).min(hi));
        let (mut sum, mut moment) = (0.0f64, 0.0f64);
        for (j, &p) in self.acc[a..=b].iter().enumerate() {
            let w = (p - floor).max(0.0) as f64;
            sum += w;
            moment += w * (a + j) as f64;
        }
        let centre = if sum > 0.0 { moment / sum } else { c as f64 };
        (sum as f32, centre)
    }

    /// [`Self::window`], moved to its own centre of power until it stays put
    /// (a few rounds of mean shift): starting from the strongest bin of a
    /// wide signal, which may be anywhere in it, this finds its middle.
    fn settle(&self, mut c: usize, lo: usize, hi: usize, floor: f32) -> (f32, f64) {
        let mut w = self.window(c, lo, hi, floor);
        for _ in 0..16 {
            let next = (w.1.round() as usize).clamp(lo, hi);
            if next == c {
                break;
            }
            c = next;
            w = self.window(c, lo, hi, floor);
        }
        w
    }

    /// The averaged spectrum is complete: update `held`.
    fn evaluate(&mut self) {
        let (lo, hi) = self.range();
        if let Some(h) = self.held {
            let j = self.bin(h);
            if j < lo as f64 || j > hi as f64 {
                // The search range moved away from it.
                self.held = None;
            }
        }
        let n = hi - lo + 1;
        self.scratch[..n].copy_from_slice(&self.acc[lo..=hi]);
        let (_, &mut floor, _) = self.scratch[..n].select_nth_unstable_by(n / 2, f32::total_cmp);
        let (peak, &peak_p) = self.acc[lo..=hi]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("the range is never empty");
        let peak = lo + peak;
        if peak_p < floor.max(f32::MIN_POSITIVE) * self.set.min_snr {
            return; // Nothing stands out: hold.
        }
        let (p_new, c_new) = self.settle(peak, lo, hi, floor);
        let centre = match self.held {
            Some(h) => {
                let cur = (self.bin(h).round() as usize).clamp(lo, hi);
                let (p_cur, c_cur) = self.settle(cur, lo, hi, floor);
                if p_new > p_cur * self.set.hysteresis {
                    c_new
                } else {
                    c_cur
                }
            }
            None => c_new,
        };
        self.held = Some(self.freq(centre));
    }
}

impl Block for PeakFrequency {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.input_rate(0)?;
        let frames = self.rate / self.update_rate / self.size as f64;
        self.per_update = (frames.round() as usize).max(1);
        let out_rate = self.rate / (self.per_update * self.size) as f64;
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(out_rate);
            o.center_freq = None;
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.rate <= 0.0 {
            return Err(crate::filters::not_configured());
        }
        let mut progress = false;
        if let Some(v) = self.pending {
            let out = io.outputs[0].slice_mut::<f32>();
            if out.is_empty() {
                return Ok(WorkStatus::Blocked);
            }
            out[0] = v;
            io.produce(0, 1);
            self.pending = None;
            progress = true;
        }
        let inp = io.inputs[0].slice::<Complex32>();
        let mut used = 0;
        while used < inp.len() && self.pending.is_none() {
            let n = (self.size - self.filled).min(inp.len() - used);
            self.frame[self.filled..self.filled + n].copy_from_slice(&inp[used..used + n]);
            self.filled += n;
            used += n;
            if self.filled < self.size {
                break;
            }
            self.filled = 0;
            self.ps.compute(&self.frame, &mut self.power);
            // Accumulate centred: FFT bin k lands at (k + size/2) mod size.
            let half = self.size / 2;
            let (upper, lower) = self.power.split_at(self.size - half);
            for (a, p) in self.acc[half..].iter_mut().zip(upper) {
                *a += p;
            }
            for (a, p) in self.acc[..half].iter_mut().zip(lower) {
                *a += p;
            }
            self.frames += 1;
            if self.frames >= self.per_update {
                self.evaluate();
                self.acc.fill(0.0);
                self.frames = 0;
                self.pending = self.held.map(|h| h as f32);
            }
        }
        io.consume(0, used);
        if let Some(v) = self.pending {
            let out = io.outputs[0].slice_mut::<f32>();
            if !out.is_empty() {
                out[0] = v;
                io.produce(0, 1);
                self.pending = None;
            }
        }
        Ok(if used > 0 || progress {
            WorkStatus::Ok
        } else {
            WorkStatus::Blocked
        })
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        match name {
            "min_offset" => self.set.min_offset = v,
            "max_offset" => self.set.max_offset = v,
            "bandwidth" => self.set.bandwidth = v,
            "min_snr" => self.set.min_snr = db(v),
            "hysteresis" => self.set.hysteresis = db(v),
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn db(x: f64) -> f32 {
    10f64.powf(x / 10.0) as f32
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let size = p.u64("fft_size")?.clamp(64, 65_536) as usize;
    let update_rate = p.f64("update_rate")?;
    if update_rate <= 0.0 {
        return Err(Error::Param {
            block: "peak_frequency".into(),
            param: "update_rate".into(),
            msg: "must be above 0".into(),
        });
    }
    let set = Settings {
        min_offset: p.f64("min_offset")?,
        max_offset: p.f64("max_offset")?,
        bandwidth: p.f64("bandwidth")?,
        min_snr: db(p.f64("min_snr")?),
        hysteresis: db(p.f64("hysteresis")?),
    };
    Ok(Instance {
        block: Box::new(PeakFrequency::new(size, update_rate, set)),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::F32)],
    })
}
