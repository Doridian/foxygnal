//! Sample-rate changes: decimator, interpolator and rational resampler, all
//! one polyphase kernel with a built-in anti-alias / anti-image low-pass.

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, FrequencyResponse, Instance, ParamReader,
    PortSpec, Registry, Sample, StreamType, WorkIo, WorkStatus,
};
use foxygnal_dsp::{FilterSample, Resampler, fir};

use crate::design::{self, fmt_rate};
use crate::filters::not_configured;
use crate::params;

/// Largest interpolation or decimation factor after reducing the ratio.
const MAX_FACTOR: usize = 10_000;

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
            kind: "decimator",
            label: "Decimator",
            category: "Resamplers",
            help: "Lowers the sample rate by an integer factor, low-pass filtering first so \
                   nothing aliases.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                design::int("decimation", "Decimation", 4, 1, "Keep one sample in N."),
                design::rate_transition(),
            ],
        },
        build,
        response,
    );
    let (inputs, outputs) = ports();
    r.register_with_response(
        BlockDescriptor {
            kind: "interpolator",
            label: "Interpolator",
            category: "Resamplers",
            help: "Raises the sample rate by an integer factor, low-pass filtering out the \
                   images. Amplitudes are kept.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                design::int(
                    "interpolation",
                    "Interpolation",
                    4,
                    1,
                    "Output samples per input sample.",
                ),
                design::rate_transition(),
            ],
        },
        build,
        response,
    );
    let (inputs, outputs) = ports();
    r.register_with_response(
        BlockDescriptor {
            kind: "rational_resampler",
            label: "Rational Resampler",
            category: "Resamplers",
            help: "Changes the sample rate by a ratio of integers (interpolate, filter, \
                   decimate), or to a target output rate worked out from the input rate. \
                   The ratio is reduced first, so 240000 → 48000 is simply ÷5.",
            inputs,
            outputs,
            params: vec![
                params::float_type(),
                design::float_min(
                    "output_rate",
                    "Output rate",
                    48_000.0,
                    0.0,
                    Some("S/s"),
                    "Wanted output rate. 0 = use the interpolation and decimation below.",
                ),
                design::int(
                    "interpolation",
                    "Interpolation",
                    1,
                    1,
                    "Used when the output rate is 0.",
                ),
                design::int(
                    "decimation",
                    "Decimation",
                    1,
                    1,
                    "Used when the output rate is 0.",
                ),
                design::rate_transition(),
            ],
        },
        build,
        response,
    );
}

#[derive(Debug, Clone, Copy)]
enum Ratio {
    Fixed(usize, usize),
    /// Output rate in S/s; the ratio comes from the input rate.
    Target(f64),
}

#[derive(Debug, Clone, Copy)]
struct RateConfig {
    ratio: Ratio,
    transition: f64,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl RateConfig {
    fn from_params(p: &ParamReader<'_>) -> Result<Self, Error> {
        let int = |name: &str| -> Result<usize, Error> {
            if p.has(name) {
                Ok(p.u64(name)?.max(1) as usize)
            } else {
                Ok(1)
            }
        };
        let target = if p.has("output_rate") {
            p.f64("output_rate")?
        } else {
            0.0
        };
        Ok(Self {
            ratio: if target > 0.0 {
                Ratio::Target(target)
            } else {
                Ratio::Fixed(int("interpolation")?, int("decimation")?)
            },
            transition: p.f64("transition")?,
        })
    }

    /// (interpolation, decimation) in lowest terms, for input rate `rate`
    /// (needed only for a target rate).
    fn ratio(&self, rate: Option<f64>) -> Result<(usize, usize), Error> {
        let (l, m) = match self.ratio {
            Ratio::Fixed(l, m) => (l, m),
            Ratio::Target(out) => {
                let rate = rate.ok_or_else(|| {
                    Error::Other(
                        "needs to know the input sample rate to reach an output rate, but \
                         nothing upstream sets it (set one on the source, or use \
                         interpolation and decimation)"
                            .into(),
                    )
                })?;
                let whole = |v: f64| v >= 1.0 && v.fract() == 0.0 && v < 1e12;
                if !whole(rate) || !whole(out) {
                    return Err(Error::Other(format!(
                        "can only resample between whole-number rates ({} → {}); use \
                         interpolation and decimation instead",
                        fmt_rate(rate),
                        fmt_rate(out)
                    )));
                }
                (out as usize, rate as usize)
            }
        };
        let g = gcd(l, m);
        let (l, m) = (l / g, m / g);
        if l > MAX_FACTOR || m > MAX_FACTOR {
            return Err(Error::Other(format!(
                "the ratio {l}/{m} is too complex to resample efficiently (factors above \
                 {MAX_FACTOR}); pick rates with a larger common divisor"
            )));
        }
        Ok((l, m))
    }

    fn taps(&self, l: usize, m: usize, rate: f64) -> Result<Vec<f32>, Error> {
        fir::resampler_lowpass(l, m, self.transition)
            .design()
            .map_err(|e| design::design_error(e, rate * l as f64))
    }
}

/// The prototype low-pass, at the rate it runs at (interpolation × input).
fn response(p: &ParamReader<'_>, rate: f64) -> Result<FrequencyResponse, Error> {
    let cfg = RateConfig::from_params(p)?;
    let (l, m) = cfg.ratio(Some(rate))?;
    let taps = cfg.taps(l, m, rate)?;
    let mut r = design::real_response(&taps, rate * l as f64);
    // Interpolation gain L shows as +20·log10(L) dB; normalize to 0 dB.
    let off = 20.0 * (l as f32).log10();
    r.db.iter_mut().for_each(|d| *d -= off);
    Ok(r)
}

pub struct RateChange<T> {
    cfg: RateConfig,
    kernel: Option<Resampler<T>>,
}

impl<T: FilterSample + Sample> Block for RateChange<T> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let rate = cx.inputs[0].sample_rate;
        let (l, m) = self.cfg.ratio(rate)?;
        let taps = self.cfg.taps(l, m, rate.unwrap_or(1.0))?;
        self.kernel = Some(Resampler::new(l, m, &taps));
        // Unknown in, unknown out: a decimator doesn't need the rate.
        for o in cx.outputs.iter_mut() {
            o.sample_rate = rate.map(|r| r * l as f64 / m as f64);
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let kernel = self.kernel.as_mut().ok_or_else(not_configured)?;
        let (inp, out) = io.in_out::<T, T>(0, 0);
        let (used, made) = kernel.process(inp, out);
        if used == 0 && made == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn build(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let cfg = RateConfig::from_params(p)?;
    let block: Box<dyn Block> = match ty {
        StreamType::C32 => Box::new(RateChange::<Complex32> { cfg, kernel: None }),
        _ => Box::new(RateChange::<f32> { cfg, kernel: None }),
    };
    Ok(Instance {
        block,
        inputs: vec![("in".into(), ty)],
        outputs: vec![("out".into(), ty)],
    })
}
