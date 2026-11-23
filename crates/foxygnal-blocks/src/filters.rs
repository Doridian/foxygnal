//! FIR filters: a windowed-sinc designer block and the frequency-translating
//! filter that picks one channel out of a wide stream.

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, FrequencyResponse, Instance, ParamReader,
    PortSpec, Registry, Sample, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{Band, DesignWindow, FilterSample, FirSpec, Resampler, Rotator};

use crate::design::{self, fmt_hz, fmt_rate};
use crate::params;

pub fn register(r: &mut Registry) {
    let [window, attenuation] = design::fir_window();
    r.register_with_response(
        BlockDescriptor {
            kind: "fir_filter",
            label: "FIR Filter",
            category: "Filters",
            help: "Windowed-sinc FIR filter: low-pass, high-pass, band-pass or band-stop. \
                   The tap count follows from the transition width and window.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::float_type(),
                design::choice(
                    "mode",
                    "Mode",
                    vec!["low_pass", "high_pass", "band_pass", "band_stop"],
                    "low_pass",
                    "Which frequencies pass.",
                ),
                design::float_min(
                    "cutoff",
                    "Cutoff",
                    5_000.0,
                    0.0,
                    Some("Hz"),
                    "Cutoff frequency (−6 dB point). For band modes, the lower band edge.",
                )
                .live(),
                design::float_min(
                    "cutoff_high",
                    "High cutoff",
                    10_000.0,
                    0.0,
                    Some("Hz"),
                    "Upper band edge (band-pass and band-stop only).",
                )
                .live(),
                design::float_min(
                    "transition",
                    "Transition",
                    1_000.0,
                    0.0,
                    Some("Hz"),
                    "Width of the transition band. Narrower = more taps.",
                ),
                window.clone(),
                attenuation.clone(),
                params::float("gain", "Gain", 1.0, None, "Linear gain in the passband.").live(),
            ],
        },
        build_fir,
        fir_response,
    );
    r.register_with_response(
        BlockDescriptor {
            kind: "freq_xlating_fir",
            label: "Frequency-Translating FIR",
            category: "Filters",
            help: "Picks one channel out of a wide complex stream: shifts it down by the \
                   offset, low-pass filters and decimates. The output's centre frequency \
                   moves by the offset.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::C32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::C32),
            }],
            params: vec![
                params::float(
                    "offset",
                    "Offset",
                    0.0,
                    Some("Hz"),
                    "Frequency of the wanted channel relative to the input's centre. It \
                     ends up at 0 Hz.",
                )
                .live(),
                design::float_min(
                    "cutoff",
                    "Cutoff",
                    100_000.0,
                    0.0,
                    Some("Hz"),
                    "Low-pass cutoff: half the channel bandwidth.",
                )
                .live(),
                design::float_min(
                    "transition",
                    "Transition",
                    20_000.0,
                    0.0,
                    Some("Hz"),
                    "Width of the transition band. Narrower = more taps.",
                ),
                design::int(
                    "decimation",
                    "Decimation",
                    1,
                    1,
                    "Keep every Nth sample after filtering.",
                ),
                window,
                attenuation,
            ],
        },
        build_xlating,
        xlating_response,
    );
}

// ---- FIR filter ------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Mode {
    LowPass,
    HighPass,
    BandPass,
    BandStop,
}

#[derive(Debug, Clone, Copy)]
struct FirConfig {
    mode: Mode,
    cutoff: f64,
    cutoff_high: f64,
    transition: f64,
    window: DesignWindow,
    gain: f64,
}

impl FirConfig {
    fn from_params(p: &ParamReader<'_>) -> Result<Self, Error> {
        Ok(Self {
            mode: match p.str("mode")? {
                "high_pass" => Mode::HighPass,
                "band_pass" => Mode::BandPass,
                "band_stop" => Mode::BandStop,
                _ => Mode::LowPass,
            },
            cutoff: p.f64("cutoff")?,
            cutoff_high: p.f64("cutoff_high")?,
            transition: p.f64("transition")?,
            window: design::read_fir_window(p)?,
            gain: p.f64("gain")?,
        })
    }

    /// The design at input rate `rate`, or an error in Hz.
    fn spec(&self, rate: f64) -> Result<FirSpec, Error> {
        let nyq = rate / 2.0;
        let check = |what: &str, f: f64| {
            if f > 0.0 && f < nyq {
                Ok(f / rate)
            } else {
                Err(Error::Other(format!(
                    "{what} {} must be above 0 Hz and below {} (the Nyquist frequency at {})",
                    fmt_hz(f),
                    fmt_hz(nyq),
                    fmt_rate(rate)
                )))
            }
        };
        let band = match self.mode {
            Mode::LowPass => Band::LowPass(check("cutoff", self.cutoff)?),
            Mode::HighPass => Band::HighPass(check("cutoff", self.cutoff)?),
            Mode::BandPass | Mode::BandStop => {
                let (a, b) = (
                    check("low cutoff", self.cutoff)?,
                    check("high cutoff", self.cutoff_high)?,
                );
                if a >= b {
                    return Err(Error::Other(format!(
                        "the low cutoff ({}) must be below the high cutoff ({})",
                        fmt_hz(self.cutoff),
                        fmt_hz(self.cutoff_high)
                    )));
                }
                if matches!(self.mode, Mode::BandPass) {
                    Band::BandPass(a, b)
                } else {
                    Band::BandStop(a, b)
                }
            }
        };
        let spec = FirSpec {
            band,
            transition: self.transition / rate,
            window: self.window,
            gain: self.gain,
        };
        spec.validate().map_err(|e| design::design_error(e, rate))?;
        Ok(spec)
    }
}

fn fir_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let taps = FirConfig::from_params(p)?
        .spec(rate)?
        .design()
        .map_err(|e| design::design_error(e, rate))?;
    Ok(design::real_response(&taps, rate))
}

pub struct FirFilter<T> {
    cfg: FirConfig,
    rate: f64,
    taps: Vec<f32>,
    filter: Option<Resampler<T>>,
}

impl<T: FilterSample + Sample> FirFilter<T> {
    fn new(cfg: FirConfig) -> Self {
        Self {
            cfg,
            rate: 0.0,
            taps: Vec::new(),
            filter: None,
        }
    }
}

impl<T: FilterSample + Sample> Block for FirFilter<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.input_rate(0)?;
        self.taps = self
            .cfg
            .spec(self.rate)?
            .design()
            .map_err(|e| design::design_error(e, self.rate))?;
        self.filter = Some(Resampler::fir(&self.taps));
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let filter = self.filter.as_mut().ok_or_else(not_configured)?;
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let (used, made) = filter.process(inp, out);
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        let mut cfg = self.cfg;
        match name {
            "cutoff" => cfg.cutoff = v,
            "cutoff_high" => cfg.cutoff_high = v,
            "gain" => cfg.gain = v,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        // An invalid live value keeps the old taps: the editor shows the
        // error (it configures every edit), and the radio keeps playing.
        if let Ok(spec) = cfg.spec(self.rate) {
            if retune(&spec, &mut self.taps, self.filter.as_mut()) {
                self.cfg = cfg;
            }
        }
        Ok(())
    }
}

/// Redesign a running filter's taps in place. The tap count depends only on
/// the transition width and window, which aren't live, so this doesn't
/// allocate. Returns false (and changes nothing) if the count would change.
pub(crate) fn retune<T: FilterSample>(
    spec: &FirSpec,
    taps: &mut [f32],
    filter: Option<&mut Resampler<T>>,
) -> bool {
    let Some(filter) = filter else {
        return false;
    };
    if spec.num_taps() != taps.len() || spec.design_into(taps).is_err() {
        return false;
    }
    filter.set_taps(taps);
    true
}

pub(crate) fn not_configured() -> Error {
    Error::Other("the block was not configured before running".into())
}

fn build_fir(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let cfg = FirConfig::from_params(p)?;
    let block: Box<dyn Block> = match ty {
        StreamType::C32 => Box::new(FirFilter::<Complex32>::new(cfg)),
        _ => Box::new(FirFilter::<f32>::new(cfg)),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}

// ---- Frequency-translating FIR ----------------------------------------------

#[derive(Debug, Clone, Copy)]
struct XlatingConfig {
    offset: f64,
    cutoff: f64,
    transition: f64,
    decimation: usize,
    window: DesignWindow,
}

impl XlatingConfig {
    fn from_params(p: &ParamReader<'_>) -> Result<Self, Error> {
        Ok(Self {
            offset: p.f64("offset")?,
            cutoff: p.f64("cutoff")?,
            transition: p.f64("transition")?,
            decimation: p.u64("decimation")?.max(1) as usize,
            window: design::read_fir_window(p)?,
        })
    }

    fn check_offset(&self, rate: f64) -> Result<(), Error> {
        if self.offset.abs() >= rate / 2.0 {
            return Err(Error::Other(format!(
                "offset {} is outside the input's band (±{} at {})",
                fmt_hz(self.offset),
                fmt_hz(rate / 2.0),
                fmt_rate(rate)
            )));
        }
        Ok(())
    }

    fn spec(&self, rate: f64) -> Result<FirSpec, Error> {
        self.check_offset(rate)?;
        let out_nyq = rate / self.decimation as f64 / 2.0;
        if self.cutoff >= out_nyq {
            return Err(Error::Other(format!(
                "cutoff {} must be below {}, the output's Nyquist frequency ({} / {} / 2), \
                 or the channel aliases. Lower the cutoff or the decimation",
                fmt_hz(self.cutoff),
                fmt_hz(out_nyq),
                fmt_rate(rate),
                self.decimation,
            )));
        }
        let spec = FirSpec {
            band: Band::LowPass(self.cutoff / rate),
            transition: self.transition / rate,
            window: self.window,
            gain: 1.0,
        };
        spec.validate().map_err(|e| design::design_error(e, rate))?;
        Ok(spec)
    }
}

fn xlating_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let cfg = XlatingConfig::from_params(p)?;
    let taps = cfg
        .spec(rate)?
        .design()
        .map_err(|e| design::design_error(e, rate))?;
    Ok(design::shifted_response(&taps, rate, cfg.offset))
}

pub struct XlatingFir {
    cfg: XlatingConfig,
    rate: f64,
    taps: Vec<f32>,
    rot: Rotator,
    filter: Option<Resampler<Complex32>>,
}

impl Block for XlatingFir {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.rate = cx.input_rate(0)?;
        self.taps = self
            .cfg
            .spec(self.rate)?
            .design()
            .map_err(|e| design::design_error(e, self.rate))?;
        self.filter = Some(Resampler::new(1, self.cfg.decimation, &self.taps));
        self.rot.set_freq(-self.cfg.offset / self.rate);
        let out_rate = self.rate / self.cfg.decimation as f64;
        for o in cx.outputs.iter_mut() {
            o.sample_rate = Some(out_rate);
            o.center_freq = cx.inputs[0].center_freq.map(|c| c + self.cfg.offset);
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let filter = self.filter.as_mut().ok_or_else(not_configured)?;
        let rot = &mut self.rot;
        let (inp, out) = io.in_out::<Complex32, Complex32>(0, 0);
        let (used, made) = filter.process_with(inp, out, |x| rot.next(x));
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        let v = value.as_f64().unwrap_or_default();
        let mut cfg = self.cfg;
        match name {
            "offset" => {
                cfg.offset = v;
                if cfg.check_offset(self.rate).is_ok() {
                    self.rot.set_freq(-v / self.rate);
                    self.cfg = cfg;
                }
            }
            "cutoff" => {
                cfg.cutoff = v;
                if let Ok(spec) = cfg.spec(self.rate) {
                    if retune(&spec, &mut self.taps, self.filter.as_mut()) {
                        self.cfg = cfg;
                    }
                }
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_xlating(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(XlatingFir {
            cfg: XlatingConfig::from_params(p)?,
            rate: 0.0,
            taps: Vec::new(),
            rot: Rotator::new(0.0),
            filter: None,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}
