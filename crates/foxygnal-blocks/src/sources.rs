//! Signal generators.

use std::f64::consts::TAU;

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

use crate::params;

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "signal_source",
            label: "Signal Generator",
            category: "Sources",
            help: "Periodic waveform. For c32, `sine` is a complex exponential e^(j2πft).",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::float_type(),
                ParamSpec {
                    name: "waveform",
                    label: "Waveform",
                    kind: ParamKind::Enum {
                        options: vec!["sine", "square", "sawtooth", "triangle", "constant"],
                    },
                    default: json!("sine"),
                    help: "Shape of the generated signal.",
                },
                params::float(
                    "frequency",
                    "Frequency",
                    1_000.0,
                    Some("Hz"),
                    "Signal frequency.",
                ),
                params::float("amplitude", "Amplitude", 1.0, None, "Peak amplitude."),
                params::float(
                    "offset",
                    "Offset",
                    0.0,
                    None,
                    "DC offset added to the output.",
                ),
                params::sample_rate(),
            ],
        },
        build_signal,
    );
    r.register(
        BlockDescriptor {
            kind: "noise_source",
            label: "Noise",
            category: "Sources",
            help: "Gaussian white noise. For c32, the power is split evenly between I and Q.",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::float_type(),
                params::float(
                    "amplitude",
                    "Amplitude (σ)",
                    0.1,
                    None,
                    "Standard deviation.",
                ),
                ParamSpec {
                    name: "seed",
                    label: "Seed",
                    kind: ParamKind::Int {
                        min: Some(0),
                        max: None,
                    },
                    default: json!(0),
                    help: "0 = random seed each run.",
                },
            ],
        },
        build_noise,
    );
}

#[derive(Clone, Copy)]
enum Waveform {
    Sine,
    Square,
    Sawtooth,
    Triangle,
    Constant,
}

pub struct SignalSource {
    ty: StreamType,
    wave: Waveform,
    /// Phase in cycles, [0, 1).
    phase: f64,
    step: f64,
    amplitude: f32,
    offset: f32,
}

impl SignalSource {
    fn real(&self, phase: f64) -> f32 {
        let v = match self.wave {
            Waveform::Sine => (phase * TAU).sin(),
            Waveform::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Sawtooth => 2.0 * phase - 1.0,
            Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
            Waveform::Constant => 1.0,
        };
        v as f32 * self.amplitude + self.offset
    }

    fn advance(&mut self) -> f64 {
        let p = self.phase;
        self.phase = (self.phase + self.step).rem_euclid(1.0);
        p
    }
}

impl Block for SignalSource {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let n = match self.ty {
            StreamType::C32 => {
                let out = io.outputs[0].slice_mut::<Complex32>();
                for v in out.iter_mut() {
                    let p = self.advance();
                    *v = match self.wave {
                        Waveform::Sine => {
                            let (s, c) = (p * TAU).sin_cos();
                            Complex32::new(
                                c as f32 * self.amplitude + self.offset,
                                s as f32 * self.amplitude,
                            )
                        }
                        _ => Complex32::new(self.real(p), 0.0),
                    };
                }
                out.len()
            }
            _ => {
                let out = io.outputs[0].slice_mut::<f32>();
                for v in out.iter_mut() {
                    let p = self.advance();
                    *v = self.real(p);
                }
                out.len()
            }
        };
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_signal(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let wave = match p.str("waveform")? {
        "sine" => Waveform::Sine,
        "square" => Waveform::Square,
        "sawtooth" => Waveform::Sawtooth,
        "triangle" => Waveform::Triangle,
        "constant" => Waveform::Constant,
        other => return Err(Error::Other(format!("unknown waveform `{other}`"))),
    };
    let fs = p.f64("sample_rate")?;
    Ok(Instance {
        block: Box::new(SignalSource {
            ty,
            wave,
            phase: 0.0,
            step: p.f64("frequency")? / fs,
            amplitude: p.f64("amplitude")? as f32,
            offset: p.f64("offset")? as f32,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

/// Small, fast, non-cryptographic PRNG (xoshiro256+), plenty for noise.
pub struct Rng([u64; 4]);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed;
        let mut next = || {
            // splitmix64
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        };
        Self([next(), next(), next(), next()])
    }

    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.0;
        let result = s[0].wrapping_add(s[3]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform in (0, 1].
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) * (1.0 / (1u64 << 53) as f64)
    }

    /// A pair of independent standard normal samples (Box–Muller).
    pub fn gaussian_pair(&mut self) -> (f32, f32) {
        let r = (-2.0 * self.unit().ln()).sqrt();
        let (s, c) = (TAU * self.unit()).sin_cos();
        ((r * c) as f32, (r * s) as f32)
    }
}

pub struct NoiseSource {
    ty: StreamType,
    sigma: f32,
    rng: Rng,
}

impl Block for NoiseSource {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let n = match self.ty {
            StreamType::C32 => {
                let s = self.sigma * std::f32::consts::FRAC_1_SQRT_2;
                let out = io.outputs[0].slice_mut::<Complex32>();
                for v in out.iter_mut() {
                    let (a, b) = self.rng.gaussian_pair();
                    *v = Complex32::new(a * s, b * s);
                }
                out.len()
            }
            _ => {
                let out = io.outputs[0].slice_mut::<f32>();
                for pair in out.chunks_mut(2) {
                    let (a, b) = self.rng.gaussian_pair();
                    pair[0] = a * self.sigma;
                    if let Some(x) = pair.get_mut(1) {
                        *x = b * self.sigma;
                    }
                }
                out.len()
            }
        };
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn build_noise(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    let seed = match p.u64("seed")? {
        0 => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1),
        s => s,
    };
    Ok(Instance {
        block: Box::new(NoiseSource {
            ty,
            sigma: p.f64("amplitude")? as f32,
            rng: Rng::new(seed),
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}
