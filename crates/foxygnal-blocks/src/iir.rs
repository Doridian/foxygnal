//! IIR filters: RBJ-cookbook biquads (cascadable) and a DC blocker.

use std::marker::PhantomData;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, FrequencyResponse, Instance, ParamReader,
    PortSpec, Registry, Sample, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Biquad, BiquadCascade, BiquadKind, DcBlocker};

use crate::design::{self, RESPONSE_POINTS, fmt_hz, fmt_rate};
use crate::filters::not_configured;
use crate::params;

const SHAPES: [(&str, BiquadKind); 8] = [
    ("low_pass", BiquadKind::LowPass),
    ("high_pass", BiquadKind::HighPass),
    ("band_pass", BiquadKind::BandPass),
    ("notch", BiquadKind::Notch),
    ("peaking", BiquadKind::Peaking),
    ("low_shelf", BiquadKind::LowShelf),
    ("high_shelf", BiquadKind::HighShelf),
    ("all_pass", BiquadKind::AllPass),
];

fn ports() -> (Vec<PortSpec>, Vec<PortSpec>) {
    (
        vec![PortSpec {
            name: "in",
            ty: None,
        }],
        vec![PortSpec {
            name: "out",
            ty: None,
        }],
    )
}

pub fn register(r: &mut Registry) {
    let (inputs, outputs) = ports();
    r.register_with_response(
        BlockDescriptor {
            kind: "biquad",
            label: "IIR Biquad",
            category: "Filters",
            help: "Second-order IIR filter from the RBJ Audio EQ Cookbook: low/high-pass, \
                   band-pass (0 dB peak), notch, peaking EQ, low/high shelf or all-pass. \
                   Far fewer operations than an FIR of similar sharpness, but the phase is \
                   not linear. Stages > 1 cascade identical sections for a steeper slope.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                design::choice(
                    "shape",
                    "Shape",
                    SHAPES.iter().map(|(n, _)| *n).collect(),
                    "low_pass",
                    "Filter shape. Gain applies to peaking and shelf shapes only.",
                ),
                design::float_min(
                    "freq",
                    "Frequency",
                    1_000.0,
                    0.0,
                    Some("Hz"),
                    "Cutoff (low/high-pass: |H| = Q here, −3 dB for Q = 0.707), centre \
                     (band-pass, notch, peaking, all-pass) or shelf midpoint.",
                )
                .live(),
                design::float_min(
                    "q",
                    "Q",
                    std::f64::consts::FRAC_1_SQRT_2,
                    0.01,
                    None,
                    "Quality factor: higher = narrower band / sharper peak. 0.707 gives a \
                     Butterworth low/high-pass and a flat shelf.",
                )
                .live(),
                params::float(
                    "gain_db",
                    "Gain",
                    0.0,
                    Some("dB"),
                    "Peak or shelf gain (peaking, low_shelf, high_shelf).",
                )
                .live(),
                design::int(
                    "stages",
                    "Stages",
                    1,
                    1,
                    "Identical biquads in cascade (order 2·stages).",
                ),
            ],
        },
        build_biquad,
        biquad_response,
    );
    r.set_keywords(
        "biquad",
        &[
            "iir",
            "equalizer",
            "eq",
            "notch",
            "peaking",
            "shelf",
            "low pass",
            "high pass",
            "band pass",
            "rbj",
        ],
    );
    let (inputs, outputs) = ports();
    r.register_with_response(
        BlockDescriptor {
            kind: "dc_blocker",
            label: "DC Blocker",
            category: "Filters",
            help: "Removes the DC offset (and very low frequencies) with a single-pole \
                   high-pass: y = g·(x[n] − x[n−1] + R·y[n−1]). Cheap, and unity gain at \
                   Nyquist.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                design::float_min(
                    "cutoff",
                    "Cutoff",
                    10.0,
                    0.0,
                    Some("Hz"),
                    "−3 dB frequency. Lower = slower to settle (time constant about \
                     1/(2π·cutoff)). Must be below a quarter of the sample rate.",
                )
                .live(),
            ],
        },
        build_dc,
        dc_response,
    );
    r.set_keywords(
        "dc_blocker",
        &["dc removal", "offset", "high pass", "dc block"],
    );
}

/// Check `f` is strictly inside (0, `max`) at `rate`.
fn check_freq(what: &str, f: f64, max: f64, max_what: &str, rate: f64) -> Result<f64, Error> {
    if f > 0.0 && f < max {
        Ok(f / rate)
    } else {
        Err(Error::Other(format!(
            "{what} {} must be above 0 Hz and below {} ({max_what} at {})",
            fmt_hz(f),
            fmt_hz(max),
            fmt_rate(rate)
        )))
    }
}

/// Response of a real-coefficient IIR from 0 Hz to Nyquist (`taps` = 0).
fn iir_response(rate: f64, mag: impl Fn(f64) -> f64) -> FrequencyResponse {
    let n = RESPONSE_POINTS;
    let norm: Vec<f64> = (0..n).map(|i| 0.5 * i as f64 / (n - 1) as f64).collect();
    FrequencyResponse {
        db: norm
            .iter()
            .map(|&f| foxygnal_dsp::fir::to_db(mag(f)))
            .collect(),
        freqs: norm.iter().map(|f| f * rate).collect(),
        taps: 0,
    }
}

/// A float-lane filter kernel the IIR blocks run.
pub trait Lanes: Send + 'static {
    fn run(&mut self, x: &[f32], y: &mut [f32]);
}
impl Lanes for BiquadCascade {
    fn run(&mut self, x: &[f32], y: &mut [f32]) {
        self.process(x, y);
    }
}
impl Lanes for DcBlocker {
    fn run(&mut self, x: &[f32], y: &mut [f32]) {
        self.process(x, y);
    }
}

/// Lanes per sample type (1 real, 2 complex).
fn lanes<T: Sample>() -> usize {
    std::mem::size_of::<T>() / std::mem::size_of::<f32>()
}

/// Run `k` over input 0 into output 0, sample by sample.
fn run_lanes<T: Sample, K: Lanes>(k: &mut K, io: &mut WorkIo<'_>) -> WorkStatus {
    let (inp, out) = io.in_out::<T, T>(0, 0);
    let n = inp.len().min(out.len());
    if n == 0 {
        return WorkStatus::Blocked;
    }
    k.run(
        bytemuck::cast_slice(&inp[..n]),
        bytemuck::cast_slice_mut(&mut out[..n]),
    );
    io.consume(0, n);
    io.produce(0, n);
    WorkStatus::Ok
}

// ---- Biquad -----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct BiquadConfig {
    shape: BiquadKind,
    freq: f64,
    q: f64,
    gain_db: f64,
    stages: usize,
}

impl BiquadConfig {
    fn from_params(p: &ParamReader<'_>) -> Result<Self, Error> {
        let shape = p.str("shape")?;
        Ok(Self {
            shape: SHAPES
                .iter()
                .find(|(n, _)| *n == shape)
                .map(|(_, k)| *k)
                .ok_or_else(|| Error::Other(format!("unknown shape `{shape}`")))?,
            freq: p.f64("freq")?,
            q: p.f64("q")?,
            gain_db: p.f64("gain_db")?,
            stages: (p.u64("stages")? as usize).clamp(1, 64),
        })
    }

    fn design(&self, rate: f64) -> Result<Biquad, Error> {
        let f0 = check_freq(
            "frequency",
            self.freq,
            rate / 2.0,
            "the Nyquist frequency",
            rate,
        )?;
        if !(self.q > 0.0 && self.q.is_finite()) {
            return Err(Error::Other(format!("Q must be above 0 (got {})", self.q)));
        }
        Ok(Biquad::design(self.shape, f0, self.q, self.gain_db))
    }
}

fn biquad_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let cfg = BiquadConfig::from_params(p)?;
    let c = cfg.design(rate)?;
    Ok(iir_response(rate, |f| {
        c.magnitude_at(f).powi(cfg.stages as i32)
    }))
}

pub struct BiquadFilter<T> {
    cfg: BiquadConfig,
    rate: f64,
    filter: Option<BiquadCascade>,
    _t: PhantomData<T>,
}

impl<T: Sample> Block for BiquadFilter<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.input_rate(0)?;
        let c = self.cfg.design(self.rate)?;
        self.filter = Some(BiquadCascade::new(c, self.cfg.stages, lanes::<T>()));
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let f = self.filter.as_mut().ok_or_else(not_configured)?;
        Ok(run_lanes::<T, _>(f, io))
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        let mut cfg = self.cfg;
        match name {
            "freq" => cfg.freq = v,
            "q" => cfg.q = v,
            "gain_db" => cfg.gain_db = v,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        // An invalid value keeps the old filter (the editor shows the error).
        if let (Ok(c), Some(f)) = (cfg.design(self.rate), self.filter.as_mut()) {
            f.set(c);
            self.cfg = cfg;
        }
        Ok(())
    }
}

fn build_biquad(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let cfg = BiquadConfig::from_params(p)?;
    fn make<T: Sample>(cfg: BiquadConfig) -> Box<dyn Block> {
        Box::new(BiquadFilter::<T> {
            cfg,
            rate: 0.0,
            filter: None,
            _t: PhantomData,
        })
    }
    let block = match ty {
        StreamType::C32 => make::<Complex32>(cfg),
        _ => make::<f32>(cfg),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

// ---- DC blocker ---------------------------------------------------------------

fn dc_cutoff(cutoff: f64, rate: f64) -> Result<f64, Error> {
    check_freq(
        "cutoff",
        cutoff,
        rate / 4.0,
        "a quarter of the sample rate",
        rate,
    )
}

fn dc_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let d = DcBlocker::new(dc_cutoff(p.f64("cutoff")?, rate)?, 1);
    Ok(iir_response(rate, |f| d.magnitude_at(f)))
}

pub struct DcBlock<T> {
    cutoff: f64,
    rate: f64,
    filter: Option<DcBlocker>,
    _t: PhantomData<T>,
}

impl<T: Sample> Block for DcBlock<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.input_rate(0)?;
        let fc = dc_cutoff(self.cutoff, self.rate)?;
        self.filter = Some(DcBlocker::new(fc, lanes::<T>()));
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let f = self.filter.as_mut().ok_or_else(not_configured)?;
        Ok(run_lanes::<T, _>(f, io))
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "cutoff" => {
                let v = value.as_f64().unwrap_or_default();
                if let (Ok(fc), Some(f)) = (dc_cutoff(v, self.rate), self.filter.as_mut()) {
                    f.set_cutoff(fc);
                    self.cutoff = v;
                }
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_dc(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let cutoff = p.f64("cutoff")?;
    fn make<T: Sample>(cutoff: f64) -> Box<dyn Block> {
        Box::new(DcBlock::<T> {
            cutoff,
            rate: 0.0,
            filter: None,
            _t: PhantomData,
        })
    }
    let block = match ty {
        StreamType::C32 => make::<Complex32>(cutoff),
        _ => make::<f32>(cutoff),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}
