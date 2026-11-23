//! Type converters. Every port has a fixed type (not chosen by a parameter),
//! so the UI can find the right converter for a mismatched connection.

use foxygnal_core::{
    Block, BlockDescriptor, Complex32, Configure, Error, Instance, ParamKind, ParamReader,
    ParamSpec, PortSpec, Registry, Sample, StreamType, WorkIo, WorkStatus,
};
use serde_json::json;

const CATEGORY: &str = "Converters";

fn port(name: &'static str, ty: StreamType) -> PortSpec {
    PortSpec { name, ty: Some(ty) }
}

fn scale_param() -> ParamSpec {
    ParamSpec {
        name: "scale",
        label: "Full scale",
        kind: ParamKind::Float {
            min: Some(1e-12),
            max: None,
            unit: None,
        },
        default: json!(1.0),
        help: "The float value that corresponds to integer full scale (±32767).",
        live: false,
    }
}

/// A converter with one input and one output of fixed types.
fn simple(
    r: &mut Registry,
    kind: &'static str,
    label: &'static str,
    help: &'static str,
    (from, to): (StreamType, StreamType),
    params: Vec<ParamSpec>,
    factory: foxygnal_core::registry::Factory,
) {
    r.register(
        BlockDescriptor {
            kind,
            label,
            category: CATEGORY,
            help,
            inputs: vec![port("in", from)],
            outputs: vec![port("out", to)],
            params,
        },
        factory,
    );
}

pub fn register(r: &mut Registry) {
    use StreamType::{C32, F32, I16, U8};
    simple(
        r,
        "c32_to_f32",
        "Complex → Float",
        "Takes one part of a complex stream: real, imaginary, magnitude, magnitude² \
         (power) or phase (radians, −π..π).",
        (C32, F32),
        vec![ParamSpec {
            name: "part",
            label: "Part",
            kind: ParamKind::Enum {
                options: vec!["real", "imag", "magnitude", "magnitude_squared", "phase"],
            },
            default: json!("real"),
            help: "Which part of each complex sample to output.",
            live: true,
        }],
        build_c32_to_f32,
    );
    simple(
        r,
        "f32_to_c32",
        "Float → Complex",
        "Real signal to complex, with the imaginary part 0.",
        (F32, C32),
        vec![],
        |_| Ok(map_instance(|x: f32| Complex32::new(x, 0.0))),
    );
    r.register(
        BlockDescriptor {
            kind: "f32_pair_to_c32",
            label: "Re + Im → Complex",
            category: CATEGORY,
            help: "Builds a complex stream from separate real and imaginary streams.",
            inputs: vec![port("re", F32), port("im", F32)],
            outputs: vec![port("out", C32)],
            params: vec![],
        },
        |_| {
            Ok(Instance {
                block: Box::new(PairToComplex),
                inputs: vec![("re".into(), F32), ("im".into(), F32)],
                outputs: vec![("out".into(), C32)],
            })
        },
    );
    simple(
        r,
        "i16_to_f32",
        "Short → Float",
        "16-bit integers to float: ±32767 becomes ±full scale.",
        (I16, F32),
        vec![scale_param()],
        |p| {
            let k = p.f64("scale")? as f32 / 32767.0;
            Ok(map_instance(move |x: i16| x as f32 * k))
        },
    );
    simple(
        r,
        "f32_to_i16",
        "Float → Short",
        "Float to 16-bit integers: ±full scale becomes ±32767. Values beyond full scale \
         are clipped.",
        (F32, I16),
        vec![scale_param()],
        |p| {
            let k = 32767.0 / p.f64("scale")? as f32;
            Ok(map_instance(move |x: f32| f32_to_i16(x, k)))
        },
    );
    simple(
        r,
        "u8_to_f32",
        "Byte → Float",
        "Unsigned bytes to float, unscaled (0..255).",
        (U8, F32),
        vec![],
        |_| Ok(map_instance(|x: u8| x as f32)),
    );
    simple(
        r,
        "cu8_to_c32",
        "Interleaved Bytes → Complex",
        "Interleaved unsigned I/Q byte pairs (RTL-SDR `cu8`) to complex: (x − 127.5) / 127.5. \
         The input rate counts bytes, so the output rate is half of it.",
        (U8, C32),
        vec![],
        |_| {
            Ok(pairs_instance(|i: u8, q: u8| {
                Complex32::new(
                    (i as f32 - 127.5) * (1.0 / 127.5),
                    (q as f32 - 127.5) * (1.0 / 127.5),
                )
            }))
        },
    );
    simple(
        r,
        "ci16_to_c32",
        "Interleaved Shorts → Complex",
        "Interleaved 16-bit I/Q pairs (`ci16`) to complex: ±32767 becomes ±full scale. The \
         input rate counts integers, so the output rate is half of it.",
        (I16, C32),
        vec![scale_param()],
        |p| {
            let k = p.f64("scale")? as f32 / 32767.0;
            Ok(pairs_instance(move |i: i16, q: i16| {
                Complex32::new(i as f32 * k, q as f32 * k)
            }))
        },
    );
}

/// Float to i16 with gain `k`, rounded and clipped.
pub fn f32_to_i16(x: f32, k: f32) -> i16 {
    (x * k).round().clamp(-32768.0, 32767.0) as i16
}

/// Sample-by-sample conversion with a fixed function.
pub struct Map<I, O, F> {
    f: F,
    _t: std::marker::PhantomData<fn(I) -> O>,
}

impl<I: Sample, O: Sample, F: Fn(I) -> O + Send + 'static> Block for Map<I, O, F> {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<I, O>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for (o, &x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = (self.f)(x);
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

fn map_instance<I: Sample, O: Sample>(f: impl Fn(I) -> O + Send + 'static) -> Instance {
    Instance {
        block: Box::new(Map {
            f,
            _t: std::marker::PhantomData,
        }),
        inputs: vec![("in".into(), I::TYPE)],
        outputs: vec![("out".into(), O::TYPE)],
    }
}

#[derive(Clone, Copy)]
enum Part {
    Real,
    Imag,
    Magnitude,
    MagnitudeSquared,
    Phase,
}

impl Part {
    fn parse(s: &str) -> Result<Self, Error> {
        Ok(match s {
            "real" => Self::Real,
            "imag" => Self::Imag,
            "magnitude" => Self::Magnitude,
            "magnitude_squared" => Self::MagnitudeSquared,
            "phase" => Self::Phase,
            _ => return Err(Error::Other(format!("unknown part `{s}`"))),
        })
    }
}

pub struct ComplexToFloat {
    part: Part,
}

impl Block for ComplexToFloat {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<Complex32, f32>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        let (inp, out) = (&inp[..n], &mut out[..n]);
        // One loop per part keeps the match out of the inner loop.
        match self.part {
            Part::Real => out.iter_mut().zip(inp).for_each(|(o, x)| *o = x.re),
            Part::Imag => out.iter_mut().zip(inp).for_each(|(o, x)| *o = x.im),
            Part::Magnitude => out.iter_mut().zip(inp).for_each(|(o, x)| *o = x.norm()),
            Part::MagnitudeSquared => out.iter_mut().zip(inp).for_each(|(o, x)| *o = x.norm_sqr()),
            Part::Phase => out.iter_mut().zip(inp).for_each(|(o, x)| *o = x.arg()),
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "part" => self.part = Part::parse(value.as_str().unwrap_or_default())?,
            _ => return Err(Error::Other(format!("`{name}` is not a live parameter"))),
        }
        Ok(())
    }
}

fn build_c32_to_f32(p: &ParamReader<'_>) -> Result<Instance, Error> {
    Ok(Instance {
        block: Box::new(ComplexToFloat {
            part: Part::parse(p.str("part")?)?,
        }),
        inputs: vec![("in".into(), StreamType::C32)],
        outputs: vec![("out".into(), StreamType::F32)],
    })
}

pub struct PairToComplex;

impl Block for PairToComplex {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let re = io.inputs[0].slice::<f32>();
        let im = io.inputs[1].slice::<f32>();
        let out = io.outputs[0].slice_mut::<Complex32>();
        let n = re.len().min(im.len()).min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for ((o, &r), &i) in out[..n].iter_mut().zip(&re[..n]).zip(&im[..n]) {
            *o = Complex32::new(r, i);
        }
        io.consume(0, n);
        io.consume(1, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }
}

/// Interleaved pairs of `I` to one complex sample each. Halves the rate.
pub struct Pairs<I, F> {
    f: F,
    /// The first half of a pair split across a buffer wrap.
    pending: Option<I>,
}

impl<I: Sample, F: Fn(I, I) -> Complex32 + Send + 'static> Block for Pairs<I, F> {
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        if let Some(r) = cx.inputs[0].sample_rate {
            cx.set_rate(r / 2.0);
        }
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let (inp, out) = io.in_out::<I, Complex32>(0, 0);
        if inp.is_empty() || out.is_empty() {
            return Ok(WorkStatus::Blocked);
        }
        let (mut used, mut made) = (0, 0);
        if let Some(i) = self.pending.take() {
            out[0] = (self.f)(i, inp[0]);
            (used, made) = (1, 1);
        }
        let n = ((inp.len() - used) / 2).min(out.len() - made);
        for (o, pair) in out[made..made + n]
            .iter_mut()
            .zip(inp[used..used + 2 * n].chunks_exact(2))
        {
            *o = (self.f)(pair[0], pair[1]);
        }
        used += 2 * n;
        made += n;
        // A lone item (at a buffer wrap) waits for its partner. One left
        // dangling at the end of the stream is dropped.
        if inp.len() - used == 1 && made < out.len() {
            self.pending = Some(inp[used]);
            used += 1;
        }
        io.consume(0, used);
        io.produce(0, made);
        Ok(WorkStatus::Ok)
    }
}

fn pairs_instance<I: Sample>(f: impl Fn(I, I) -> Complex32 + Send + 'static) -> Instance {
    Instance {
        block: Box::new(Pairs { f, pending: None }),
        inputs: vec![("in".into(), I::TYPE)],
        outputs: vec![("out".into(), StreamType::C32)],
    }
}

#[cfg(test)]
mod tests {
    use foxygnal_core::{Flowgraph, Params, StreamMeta};
    use serde_json::json;

    use super::*;
    use crate::testing::{VectorSink, VectorSource};

    fn run<I: Sample, O: Sample>(kind: &str, params: serde_json::Value, input: Vec<I>) -> Vec<O> {
        let reg = crate::registry();
        let mut fg = Flowgraph::new().with_buffer_capacity(5);
        let src = fg.add("src", VectorSource::instance(input));
        let conv = fg.add(
            "conv",
            reg.build(kind, params.as_object().unwrap()).unwrap(),
        );
        let (sink, out) = VectorSink::<O>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(src, 0, conv, 0).unwrap();
        fg.connect(conv, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        out.lock().unwrap().clone()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn complex_parts() {
        let x = vec![Complex32::new(3.0, 4.0), Complex32::new(0.0, -1.0)];
        let part = |p: &str| run::<Complex32, f32>("c32_to_f32", json!({"part": p}), x.clone());
        assert_eq!(part("real"), vec![3.0, 0.0]);
        assert_eq!(part("imag"), vec![4.0, -1.0]);
        assert_eq!(part("magnitude"), vec![5.0, 1.0]);
        assert_eq!(part("magnitude_squared"), vec![25.0, 1.0]);
        let ph = part("phase");
        assert!(close(ph[0], 4f32.atan2(3.0)));
        assert!(close(ph[1], -std::f32::consts::FRAC_PI_2));
    }

    #[test]
    fn real_to_complex() {
        let out = run::<f32, Complex32>("f32_to_c32", json!({}), vec![1.0, -2.0]);
        assert_eq!(
            out,
            vec![Complex32::new(1.0, 0.0), Complex32::new(-2.0, 0.0)]
        );
    }

    #[test]
    fn re_im_to_complex() {
        let mut fg = Flowgraph::new().with_buffer_capacity(3);
        let re = fg.add("re", VectorSource::instance(vec![1.0f32, 2.0, 3.0, 4.0]));
        let im = fg.add("im", VectorSource::instance(vec![-1.0f32, -2.0, -3.0]));
        let conv = fg.add(
            "conv",
            crate::registry()
                .build("f32_pair_to_c32", &Params::new())
                .unwrap(),
        );
        let (sink, out) = VectorSink::<Complex32>::instance();
        let sink = fg.add("sink", sink);
        fg.connect(re, 0, conv, 0).unwrap();
        fg.connect(im, 0, conv, 1).unwrap();
        fg.connect(conv, 0, sink, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        let out = out.lock().unwrap();
        assert_eq!(
            *out,
            vec![
                Complex32::new(1.0, -1.0),
                Complex32::new(2.0, -2.0),
                Complex32::new(3.0, -3.0)
            ]
        );
    }

    #[test]
    fn shorts_and_floats() {
        let out = run::<i16, f32>("i16_to_f32", json!({}), vec![32767, -32767, 0, 16384]);
        assert!(close(out[0], 1.0) && close(out[1], -1.0) && out[2] == 0.0);
        assert!(close(out[3], 16384.0 / 32767.0));
        let out = run::<i16, f32>("i16_to_f32", json!({"scale": 2.0}), vec![32767]);
        assert!(close(out[0], 2.0));

        let out = run::<f32, i16>(
            "f32_to_i16",
            json!({}),
            vec![1.0, -1.0, 0.5, 2.0, -3.0, 0.0],
        );
        assert_eq!(out, vec![32767, -32767, 16384, 32767, -32768, 0]);
        let out = run::<f32, i16>("f32_to_i16", json!({"scale": 0.5}), vec![0.25]);
        assert_eq!(out, vec![16384]);
    }

    #[test]
    fn bytes_to_float() {
        let out = run::<u8, f32>("u8_to_f32", json!({}), vec![0, 128, 255]);
        assert_eq!(out, vec![0.0, 128.0, 255.0]);
    }

    #[test]
    fn interleaved_bytes_to_complex() {
        let out = run::<u8, Complex32>("cu8_to_c32", json!({}), vec![0, 255, 127, 128, 9]);
        // The trailing odd byte is dropped.
        assert_eq!(out.len(), 2);
        assert!(close(out[0].re, -1.0) && close(out[0].im, 1.0));
        assert!(close(out[1].re, -0.5 / 127.5) && close(out[1].im, 0.5 / 127.5));
    }

    #[test]
    fn interleaved_shorts_to_complex() {
        let out = run::<i16, Complex32>(
            "ci16_to_c32",
            json!({}),
            vec![32767, -32767, 0, 16384, 1, 2, 3],
        );
        assert_eq!(out.len(), 3);
        assert!(close(out[0].re, 1.0) && close(out[0].im, -1.0));
        assert!(close(out[1].im, 16384.0 / 32767.0));
    }

    #[test]
    fn interleaved_halves_the_rate() {
        let reg = crate::registry();
        let mut b = reg.build("cu8_to_c32", &Params::new()).unwrap().block;
        let inputs = [StreamMeta::with_rate(4.8e6)];
        let mut outputs = [StreamMeta::merge(&inputs)];
        b.configure(&mut Configure {
            inputs: &inputs,
            outputs: &mut outputs,
        })
        .unwrap();
        assert_eq!(outputs[0].sample_rate, Some(2.4e6));
    }

    #[test]
    fn every_converter_has_fixed_ports() {
        let reg = crate::registry();
        let convs: Vec<_> = reg
            .descriptors()
            .filter(|d| d.category == CATEGORY)
            .collect();
        assert_eq!(convs.len(), 8);
        for d in convs {
            assert!(
                d.inputs.iter().chain(&d.outputs).all(|p| p.ty.is_some()),
                "{} has a param-dependent port",
                d.kind
            );
        }
    }
}
