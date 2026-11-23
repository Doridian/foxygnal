//! FIR filters: a windowed-sinc designer block and the frequency-translating
//! filter that picks one channel out of a wide stream.

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, FrequencyResponse, Instance, ParamKind,
    ParamReader, ParamSpec, PortSpec, Registry, Sample, StreamType, TagPropagation, WorkIo,
    WorkStatus,
};
use foxygnal_dsp::{Band, DesignWindow, FilterSample, FirSpec, Resampler, Rotator};
use serde_json::json;

/// Input samples the frequency-translating FIR mixes per pass.
const XLATE_CHUNK: usize = 4096;

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
    register_rrc_hilbert(r);
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
    /// Mixed input, [`XLATE_CHUNK`] samples.
    mixed: Vec<Complex32>,
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
        self.mixed = vec![Complex32::default(); XLATE_CHUNK];
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
        let (inp, out) = io.in_out::<Complex32, Complex32>(0, 0);
        let (mut used, mut made) = (0, 0);
        // Mix a chunk (SIMD rotator), then filter it. Taking exactly what
        // the filter will consume keeps the mixer's phase in step.
        loop {
            let n = (inp.len() - used)
                .min(filter.inputs_for(out.len() - made))
                .min(XLATE_CHUNK);
            if n == 0 {
                break;
            }
            let mixed = &mut self.mixed[..n];
            self.rot.rotate(&inp[used..used + n], mixed);
            let (u, m) = filter.process(mixed, &mut out[made..]);
            debug_assert_eq!(u, n, "the filter took a different amount than inputs_for");
            used += u;
            made += m;
        }
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
            mixed: Vec::new(),
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}

// ---- RRC and Hilbert --------------------------------------------------------

/// Samples the Hilbert transformer filters per pass (its scratch buffer).
const HILBERT_CHUNK: usize = 4096;

fn register_rrc_hilbert(r: &mut Registry) {
    let [window, attenuation] = design::fir_window();
    r.register_with_response(
        BlockDescriptor {
            kind: "rrc_filter",
            label: "RRC Filter",
            category: "Filters",
            help: "Root-raised-cosine FIR, the pulse shape of most linear digital modes \
                   (PSK, QAM). Use it as the receive matched filter (mode filter, unity DC \
                   gain), or on the transmit side to turn one-sample-per-symbol symbols \
                   into shaped pulses (mode interpolate: inserts sps − 1 zeros per symbol, \
                   output rate × sps). A transmit/receive pair has no inter-symbol \
                   interference at the symbol instants, where its peak is 1.",
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
                    vec!["filter", "interpolate"],
                    "filter",
                    "filter: same rate in and out (matched filter). interpolate: symbols \
                     in, sps samples per symbol out (pulse shaping; sps must be whole).",
                ),
                design::float_min(
                    "sps",
                    "Samples/symbol",
                    4.0,
                    1.0,
                    None,
                    "Samples per symbol (of the output in interpolate mode). May be \
                     fractional in filter mode.",
                ),
                ParamSpec {
                    name: "rolloff",
                    label: "Roll-off",
                    kind: ParamKind::Float {
                        min: Some(0.0),
                        max: Some(1.0),
                        unit: None,
                    },
                    default: json!(0.35),
                    help: "Excess bandwidth β: the signal occupies (1 + β) × the symbol \
                           rate. 0.35 (common), 0.22 (DVB-S2), 0.2 (narrower, longer \
                           ringing).",
                    live: false,
                },
                design::int(
                    "span",
                    "Span",
                    11,
                    1,
                    "Length in symbols; taps = span × sps (+1). Longer = closer to ideal.",
                ),
                params::float(
                    "gain",
                    "Gain",
                    1.0,
                    None,
                    "Linear gain (DC gain in filter mode).",
                )
                .live(),
            ],
        },
        build_rrc,
        rrc_response,
    );
    r.set_keywords(
        "rrc_filter",
        &[
            "root raised cosine",
            "matched filter",
            "pulse shaping",
            "rrc",
            "nyquist",
            "psk",
            "qam",
        ],
    );
    r.register_with_response(
        BlockDescriptor {
            kind: "hilbert",
            label: "Hilbert Transform",
            category: "Filters",
            help: "Real signal to its analytic (complex) signal: the imaginary part is the \
                   input shifted by 90° (a windowed FIR Hilbert transformer), the real part \
                   is the input delayed to match, so negative frequencies cancel. Accurate \
                   away from 0 Hz and Nyquist; more taps = narrower edges.",
            inputs: vec![PortSpec {
                name: "in",
                ty: Some(StreamType::F32),
            }],
            outputs: vec![PortSpec {
                name: "out",
                ty: Some(StreamType::C32),
            }],
            params: vec![
                design::int(
                    "taps",
                    "Taps",
                    65,
                    3,
                    "FIR length (made odd). Delay = (taps − 1)/2 samples.",
                ),
                window,
                attenuation,
            ],
        },
        build_hilbert,
        hilbert_response,
    );
    r.set_keywords(
        "hilbert",
        &[
            "analytic signal",
            "90 degree",
            "quadrature",
            "real to complex",
            "ssb",
        ],
    );
}

// ---- RRC --------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct RrcConfig {
    interpolate: bool,
    sps: f64,
    rolloff: f64,
    span: usize,
    gain: f64,
}

impl RrcConfig {
    fn from_params(p: &ParamReader<'_>) -> Result<Self, Error> {
        let cfg = Self {
            interpolate: p.str("mode")? == "interpolate",
            sps: p.f64("sps")?,
            rolloff: p.f64("rolloff")?,
            span: p.u64("span")?.max(1) as usize,
            gain: p.f64("gain")?,
        };
        if !(cfg.sps >= 1.0 && cfg.sps.is_finite()) {
            return Err(Error::Other(format!(
                "samples per symbol must be at least 1 (got {})",
                cfg.sps
            )));
        }
        if !(0.0..=1.0).contains(&cfg.rolloff) {
            return Err(Error::Other(format!(
                "the roll-off must be between 0 and 1 (got {})",
                cfg.rolloff
            )));
        }
        if cfg.interpolate && cfg.sps.fract() != 0.0 {
            return Err(Error::Other(format!(
                "interpolate mode needs a whole number of samples per symbol (got {}); \
                 use filter mode after a resampler for fractional rates",
                cfg.sps
            )));
        }
        let n = foxygnal_dsp::taps::rrc_len(cfg.sps, cfg.span);
        if n > foxygnal_dsp::fir::MAX_TAPS {
            return Err(Error::Other(format!(
                "span × samples per symbol needs {n} taps (at most {})",
                foxygnal_dsp::fir::MAX_TAPS
            )));
        }
        Ok(cfg)
    }

    /// Interpolation factor (1 in filter mode).
    fn interp(&self) -> usize {
        if self.interpolate {
            self.sps as usize
        } else {
            1
        }
    }

    /// Tap gain: interpolation by L needs gain L to keep the amplitude.
    fn tap_gain(&self) -> f64 {
        self.gain * self.interp() as f64
    }

    fn taps(&self) -> Vec<f32> {
        foxygnal_dsp::taps::rrc(self.sps, self.rolloff, self.span, self.tap_gain())
    }
}

fn rrc_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let cfg = RrcConfig::from_params(p)?;
    let l = cfg.interp();
    let mut r = design::real_response(&cfg.taps(), rate * l as f64);
    // Interpolation gain L shows as +20·log10(L) dB; normalize to 0 dB.
    let off = 20.0 * (l as f32).log10();
    r.db.iter_mut().for_each(|d| *d -= off);
    Ok(r)
}

pub struct RrcFilter<T> {
    cfg: RrcConfig,
    taps: Vec<f32>,
    filter: Resampler<T>,
}

impl<T: FilterSample + Sample> Block for RrcFilter<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let l = self.cfg.interp() as f64;
        // The design is relative to the symbol rate: no rate needed.
        for o in cx.outputs.iter_mut() {
            o.sample_rate = cx.inputs[0].sample_rate.map(|r| r * l);
        }
        Ok(())
    }

    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::Manual
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let (used, made) = self.filter.process(inp, out);
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        // Input item j makes output items j·L … j·L + L − 1 (they may come
        // in the next call when the output is full; the tag waits for them).
        let l = self.cfg.interp() as u64;
        let (inp, out) = (&io.inputs[0], &mut io.outputs[0]);
        let base = out.position();
        for (_, t) in inp.tags_in(used) {
            out.forward_tag((t.offset * l).saturating_sub(base) as usize, t);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "gain" => {
                self.cfg.gain = value.as_f64().unwrap_or_default();
                let c = self.cfg;
                foxygnal_dsp::taps::rrc_into(&mut self.taps, c.sps, c.rolloff, c.tap_gain());
                self.filter.set_taps(&self.taps);
            }
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_rrc(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let cfg = RrcConfig::from_params(p)?;
    fn make<T: FilterSample + Sample>(cfg: RrcConfig) -> Box<dyn Block> {
        let taps = cfg.taps();
        Box::new(RrcFilter::<T> {
            filter: Resampler::new(cfg.interp(), 1, &taps),
            taps,
            cfg,
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

// ---- Hilbert ------------------------------------------------------------------

fn hilbert_taps(p: &ParamReader<'_>) -> Result<Vec<f32>, Error> {
    let n = p.u64("taps")?.max(3) as usize;
    if n > foxygnal_dsp::fir::MAX_TAPS {
        return Err(Error::Other(format!(
            "{n} taps is too many (at most {})",
            foxygnal_dsp::fir::MAX_TAPS
        )));
    }
    Ok(foxygnal_dsp::taps::hilbert(n, design::read_fir_window(p)?))
}

/// The analytic-signal filter `δ[n − D] + j·h[n]` from −Nyquist to
/// +Nyquist: about +6 dB (|1 + 1|) for positive frequencies, a deep null
/// for negative ones.
fn hilbert_response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let taps = hilbert_taps(p)?;
    let d = (taps.len() / 2) as f64;
    let n = design::RESPONSE_POINTS;
    let norm: Vec<f64> = (0..n).map(|i| -0.5 + i as f64 / (n - 1) as f64).collect();
    let db = norm
        .iter()
        .map(|&f| {
            let w = -std::f64::consts::TAU * f;
            // H(f) = Σ h[k]·e^{−jωk}; G = e^{−jωD} + j·H.
            let (mut re, mut im) = ((w * d).cos(), (w * d).sin());
            for (k, &h) in taps.iter().enumerate() {
                let (s, c) = (w * k as f64).sin_cos();
                // j·h·(c + js) = −h·s + j·h·c
                re -= h as f64 * s;
                im += h as f64 * c;
            }
            foxygnal_dsp::fir::to_db(re.hypot(im))
        })
        .collect();
    Ok(FrequencyResponse {
        freqs: norm.iter().map(|f| f * rate).collect(),
        db,
        taps: taps.len(),
    })
}

pub struct Hilbert {
    filter: Resampler<f32>,
    /// The real part's delay line, (taps − 1)/2 long.
    delay: Vec<f32>,
    pos: usize,
    /// Imaginary parts, [`HILBERT_CHUNK`] long.
    scratch: Vec<f32>,
}

impl Block for Hilbert {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<f32, Complex32>(0, 0);
        let n = inp.len().min(out.len()).min(HILBERT_CHUNK);
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let (used, made) = self.filter.process(&inp[..n], &mut self.scratch[..n]);
        debug_assert_eq!((used, made), (n, n), "a plain FIR is one in, one out");
        let len = self.delay.len();
        for ((o, &x), &im) in out[..n].iter_mut().zip(&inp[..n]).zip(&self.scratch[..n]) {
            let re = std::mem::replace(&mut self.delay[self.pos], x);
            self.pos += 1;
            if self.pos == len {
                self.pos = 0;
            }
            *o = Complex32::new(re, im);
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_hilbert(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let taps = hilbert_taps(p)?;
    Ok(Instance {
        block: Box::new(Hilbert {
            filter: Resampler::fir(&taps),
            delay: vec![0.0; taps.len() / 2],
            pos: 0,
            scratch: vec![0.0; HILBERT_CHUNK],
        }),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::C32)],
    })
}
