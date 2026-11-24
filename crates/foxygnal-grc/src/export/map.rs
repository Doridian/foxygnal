//! The export mapping table: foxygnal blocks to GNU Radio 3.10 blocks.
//! `docs/GRC_IMPORT.md` ("Export") lists the same table; `defs.rs` has the
//! GNU Radio block ids and parameter names it may use.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Display;

use serde_json::{Value as Json, json};

use super::{FBlock, to_py};
use crate::py::{self, BinOp, Py};

// ---- Output ---------------------------------------------------------------------

/// One GNU Radio block. Parameter values are Python source.
pub(crate) struct GBlock {
    pub id: &'static str,
    /// Name suffix for blocks after the first (`demod_deemph`).
    pub suffix: String,
    pub params: BTreeMap<String, String>,
    /// From the foxygnal block's position (GRC coordinates).
    pub offset: (f64, f64),
}

impl GBlock {
    fn new(id: &'static str) -> Self {
        GBlock {
            id,
            suffix: String::new(),
            params: BTreeMap::new(),
            offset: (0.0, 0.0),
        }
    }

    fn p(mut self, key: &str, value: impl Into<String>) -> Self {
        self.params.insert(key.to_owned(), value.into());
        self
    }

    fn e(self, key: &str, value: &Py) -> Self {
        self.p(key, py::to_python(value))
    }

    fn s(self, key: &str, value: &str) -> Self {
        self.p(key, py::py_str(value))
    }

    fn b(self, key: &str, value: bool) -> Self {
        self.p(key, if value { "True" } else { "False" })
    }
}

/// Stream outputs the block definitions mark optional (GRC wants the others
/// connected).
const OPTIONAL_OUTPUTS: &[(&str, usize)] = &[
    ("blocks_throttle2", 0),
    ("blocks_head", 0),
    ("blocks_complex_to_float", 1),
    ("digital_costas_loop_cc", 1),
    ("digital_costas_loop_cc", 2),
    ("digital_costas_loop_cc", 3),
    ("digital_symbol_sync_xx", 1),
    ("digital_symbol_sync_xx", 2),
    ("digital_symbol_sync_xx", 3),
];

/// A port: foxygnal name, GNU Radio block (index into `blocks`), port
/// index, GRC dtype.
type Port = (String, usize, usize, &'static str);

/// What a foxygnal block became.
#[derive(Default)]
pub(crate) struct Out {
    pub blocks: Vec<GBlock>,
    /// Wires between them: (block, port) → (block, port).
    pub links: Vec<(usize, usize, usize, usize)>,
    ins: Vec<Port>,
    outs: Vec<Port>,
}

impl Out {
    /// One GNU Radio block; foxygnal ports in order are GRC ports 0, 1, ….
    fn one(g: GBlock, ins: &[(&str, &'static str)], outs: &[(&str, &'static str)]) -> Out {
        let mut o = Out::default();
        o.add(g, "");
        for (k, (p, t)) in ins.iter().enumerate() {
            o.input(p, 0, k, t);
        }
        for (k, (p, t)) in outs.iter().enumerate() {
            o.output(p, 0, k, t);
        }
        o
    }

    /// `in` → block → `out`, both of type `ty`.
    fn through(g: GBlock, ty: &'static str) -> Out {
        Out::one(g, &[("in", ty)], &[("out", ty)])
    }

    /// Add a block; blocks after the first go below it.
    fn add(&mut self, mut g: GBlock, suffix: &str) -> usize {
        let k = self.blocks.len();
        g.suffix = suffix.to_owned();
        g.offset = (0.0, 100.0 * k as f64);
        self.blocks.push(g);
        k
    }

    fn input(&mut self, port: &str, block: usize, idx: usize, ty: &'static str) {
        self.ins.push((port.to_owned(), block, idx, ty));
    }

    fn output(&mut self, port: &str, block: usize, idx: usize, ty: &'static str) {
        self.outs.push((port.to_owned(), block, idx, ty));
    }

    fn link(&mut self, a: usize, ap: usize, b: usize, bp: usize) {
        self.links.push((a, ap, b, bp));
    }

    /// A chain of one-in, one-out blocks: `in` feeds the first, `out` is the
    /// last.
    fn chain(blocks: Vec<(GBlock, &str)>, tin: &'static str, tout: &'static str) -> Out {
        let mut o = Out::default();
        let n = blocks.len();
        for (k, (g, suffix)) in blocks.into_iter().enumerate() {
            let i = o.add(g, suffix);
            if k > 0 {
                o.link(i - 1, 0, i, 0);
            }
        }
        o.input("in", 0, 0, tin);
        o.output("out", n - 1, 0, tout);
        o
    }

    /// The port named `port` (or the `port`-th, for a number).
    fn find<'a>(ports: &'a [Port], port: &str) -> Vec<&'a Port> {
        let named: Vec<&Port> = ports.iter().filter(|p| p.0 == port).collect();
        if !named.is_empty() {
            return named;
        }
        let Ok(k) = port.parse::<usize>() else {
            return Vec::new();
        };
        let mut distinct: Vec<&str> = Vec::new();
        for p in ports {
            if !distinct.contains(&p.0.as_str()) {
                distinct.push(&p.0);
            }
        }
        match distinct.get(k) {
            Some(name) => ports.iter().filter(|p| p.0 == *name).collect(),
            None => Vec::new(),
        }
    }

    pub fn find_output(&self, port: &str) -> Option<(usize, usize)> {
        Out::find(&self.outs, port).first().map(|p| (p.1, p.2))
    }

    pub fn find_input(&self, port: &str) -> Option<Vec<(usize, usize)>> {
        let f = Out::find(&self.ins, port);
        (!f.is_empty()).then(|| f.iter().map(|p| (p.1, p.2)).collect())
    }

    /// Outputs GRC wants connected: (block, port, dtype).
    pub fn required_outputs(&self) -> Vec<(usize, usize, &'static str)> {
        let mut v: Vec<(usize, usize, &'static str)> = Vec::new();
        for p in &self.outs {
            let optional = OPTIONAL_OUTPUTS.contains(&(self.blocks[p.1].id, p.2));
            if !optional && !v.iter().any(|q| q.0 == p.1 && q.1 == p.2) {
                v.push((p.1, p.2, p.3));
            }
        }
        v
    }

    /// Outputs `0..n` of `block` that no foxygnal port stands for.
    fn spare_outputs(&mut self, block: usize, n: usize, ty: &'static str) {
        for k in 0..n {
            self.output(&format!("#{k}"), block, k, ty);
        }
    }

    pub fn port_type(&self, port: &str, input: bool) -> Option<&'static str> {
        let ports = if input { &self.ins } else { &self.outs };
        Out::find(ports, port).first().map(|p| p.3)
    }
}

// ---- Context --------------------------------------------------------------------

/// The block being mapped, with helpers that read its parameters as Python
/// and record warnings about it.
pub(crate) struct Cx<'a> {
    pub b: &'a FBlock,
    pub scope: &'a HashMap<String, String>,
    /// Variable values (Python names).
    pub values: &'a HashMap<String, f64>,
    /// The sample rate into the block, if known.
    pub rate_in: Option<Py>,
    pub fallback: &'a Py,
    pub warned_rate: bool,
    pub warnings: &'a mut Vec<String>,
    pub used_in: BTreeSet<String>,
    pub used_out: BTreeSet<String>,
}

impl Cx<'_> {
    fn warn(&mut self, msg: impl Display) {
        self.warnings
            .push(format!("`{}` ({}): {msg}", self.b.id, self.b.kind));
    }

    fn json(&self, key: &str) -> Option<&Json> {
        self.b.params.get(key)
    }

    fn num_q(&mut self, key: &str, default: f64, warn: bool) -> Py {
        let Some(v) = self.json(key) else {
            return py::num(default);
        };
        match to_py(v, self.scope) {
            Ok(Py::Bool(b)) => py::num(f64::from(u8::from(b))),
            Ok(e) => e,
            Err(why) => {
                if warn {
                    self.warn(format!(
                        "parameter `{key}` = {v} doesn't translate ({why}); using {}",
                        py::py_num(default)
                    ));
                }
                py::num(default)
            }
        }
    }

    /// A number parameter as a Python expression.
    fn num(&mut self, key: &str, default: f64) -> Py {
        self.num_q(key, default, true)
    }

    /// An integer parameter: a whole number, or `int(…)` of an expression.
    fn int(&mut self, key: &str, default: i64) -> Py {
        let e = self.num(key, default as f64);
        intify(e)
    }

    fn eval(&self, e: &Py) -> Option<f64> {
        py::eval(e, &mut |n| self.values.get(n).copied().map(py::Value::Num))
            .ok()?
            .as_f64()
    }

    /// A number parameter's value.
    fn f64(&mut self, key: &str, default: f64) -> f64 {
        let e = self.num_q(key, default, false);
        self.eval(&e).unwrap_or(default)
    }

    fn text(&self, key: &str, default: &str) -> String {
        match self.json(key) {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Number(n)) => n.to_string(),
            Some(Json::Bool(b)) => b.to_string(),
            _ => default.to_owned(),
        }
    }

    fn flag(&self, key: &str, default: bool) -> bool {
        match self.json(key) {
            Some(Json::Bool(b)) => *b,
            Some(Json::Number(n)) => n.as_f64().is_some_and(|v| v != 0.0),
            Some(Json::String(s)) => matches!(s.as_str(), "true" | "1" | "True"),
            _ => default,
        }
    }

    /// The `type` parameter as a GRC dtype, if it is one of `allowed`
    /// (foxygnal names).
    fn ty(&self, default: &str, allowed: &[&str]) -> Result<&'static str, String> {
        let t = self.text("type", default);
        if !allowed.contains(&t.as_str()) {
            return Err(format!(
                "stream type `{t}` has no GNU Radio equivalent here (it takes {})",
                allowed.join(" or ")
            ));
        }
        grc_type(&t).ok_or_else(|| format!("stream type `{t}` isn't supported"))
    }

    /// The sample rate into the block.
    fn rate(&mut self) -> Py {
        if let Some(r) = &self.rate_in {
            return r.clone();
        }
        if !self.warned_rate {
            self.warned_rate = true;
            let f = py::to_python(self.fallback);
            self.warn(format!(
                "the sample rate into it isn't known (nothing upstream sets one); assumed \
                 `{f}`: check its rate parameters"
            ));
        }
        self.fallback.clone()
    }
}

/// `int(e)`, or a whole number.
fn intify(e: Py) -> Py {
    match e {
        Py::Num(v, _) => Py::Num(v.round(), format!("{}", v.round() as i64)),
        Py::Call(ref f, ..) if f == "int" => e,
        e => Py::Call("int".into(), vec![e], vec![]),
    }
}

fn div(a: Py, b: Py) -> Py {
    py::bin(BinOp::Div, a, b)
}

fn mul(a: Py, b: Py) -> Py {
    py::bin(BinOp::Mul, a, b)
}

fn n(v: f64) -> Py {
    py::num(v)
}

fn name(s: &str) -> Py {
    Py::Name(s.to_owned())
}

/// foxygnal stream type → GRC dtype.
pub(crate) fn grc_type(t: &str) -> Option<&'static str> {
    Some(match t {
        "c32" => "complex",
        "f32" => "float",
        "i16" => "short",
        "u8" => "byte",
        _ => return None,
    })
}

const ANY: &[&str] = &["c32", "f32", "i16", "u8"];
const CF: &[&str] = &["c32", "f32"];
const C: &str = "complex";
const F: &str = "float";
const S: &str = "short";
const B: &str = "byte";

// ---- Rates ----------------------------------------------------------------------

/// The sample rate out of the block (the same on every output).
pub(crate) fn out_rate(cx: &mut Cx<'_>) -> Option<Py> {
    let nonzero = |cx: &mut Cx<'_>, key: &str| {
        let e = cx.num_q(key, 0.0, false);
        match e {
            Py::Num(0.0, _) => None,
            e => Some(e),
        }
    };
    let rin = cx.rate_in.clone();
    Some(match cx.b.kind.as_str() {
        "signal_source" => cx.num_q("sample_rate", 48000.0, false),
        "sdr_source" => cx.num_q("sample_rate", 2.4e6, false),
        "file_source" | "udp_source" | "tcp_source" => return nonzero(cx, "sample_rate"),
        "audio_source" => n(48000.0),
        "throttle" => return rin.or_else(|| nonzero(cx, "sample_rate")),
        "freq_xlating_fir" | "decimator" => {
            let d = intify(cx.num_q("decimation", 1.0, false));
            div(rin?, d)
        }
        "interpolator" => {
            let i = intify(cx.num_q("interpolation", 4.0, false));
            mul(rin?, i)
        }
        "rational_resampler" => match nonzero(cx, "output_rate") {
            Some(r) => r,
            None => {
                let l = intify(cx.num_q("interpolation", 1.0, false));
                let m = intify(cx.num_q("decimation", 1.0, false));
                div(mul(rin?, l), m)
            }
        },
        "fm_stereo" => cx.num_q("audio_rate", 48000.0, false),
        "clock_recovery" => div(rin?, cx.num_q("sps", 8.0, false)),
        "rrc_filter" if cx.text("mode", "filter") == "interpolate" => {
            mul(rin?, cx.num_q("sps", 4.0, false))
        }
        "cu8_to_c32" | "ci16_to_c32" => div(rin?, n(2.0)),
        _ => return rin,
    })
}

// ---- The table --------------------------------------------------------------------

/// Map one block. `Err` says why it has no GNU Radio equivalent; it then
/// becomes a placeholder.
pub(crate) fn map(cx: &mut Cx<'_>) -> Result<Out, String> {
    let kind = cx.b.kind.clone();
    let out = match kind.as_str() {
        // ---- Sources ----
        "signal_source" => {
            let ty = cx.ty("c32", CF)?;
            let wave = cx.text("waveform", "sine");
            let amp = cx.num("amplitude", 1.0);
            let offset = cx.num("offset", 0.0);
            let mut g = GBlock::new("analog_sig_source_x")
                .p("type", ty)
                .e("samp_rate", &cx.num("sample_rate", 48000.0))
                .e("freq", &cx.num("frequency", 1000.0))
                .p("showports", "False");
            g = match wave.as_str() {
                "sine" | "constant" => {
                    let w = match (wave.as_str(), ty) {
                        ("constant", _) => "analog.GR_CONST_WAVE",
                        (_, C) => "analog.GR_COS_WAVE",
                        _ => "analog.GR_SIN_WAVE",
                    };
                    g.p("waveform", w)
                        .e("amp", &amp)
                        .e("offset", &offset)
                        .p("phase", "0")
                }
                _ => {
                    // GNU Radio's square, triangle and sawtooth span 0..amp
                    // and start half a period later than foxygnal's −1..1
                    // ones: twice the amplitude, offset down, phase π.
                    let w = match wave.as_str() {
                        "square" => "analog.GR_SQR_WAVE",
                        "triangle" => "analog.GR_TRI_WAVE",
                        _ => "analog.GR_SAW_WAVE",
                    };
                    if ty == C {
                        cx.warn(format!(
                            "a complex {wave} wave differs in GNU Radio (the imaginary part \
                             isn't the same shape)"
                        ));
                    }
                    let amp2 = mul(n(2.0), amp.clone());
                    let off = py::bin(BinOp::Sub, offset, amp);
                    g.p("waveform", w)
                        .e("amp", &amp2)
                        .e("offset", &off)
                        .p("phase", "math.pi")
                }
            };
            Out::one(g, &[], &[("out", ty)])
        }
        "noise_source" => {
            let ty = cx.ty("c32", CF)?;
            let g = GBlock::new("analog_noise_source_x")
                .p("type", ty)
                .p("noise_type", "analog.GR_GAUSSIAN")
                .e("amp", &cx.num("amplitude", 0.1))
                .e("seed", &cx.int("seed", 0));
            Out::one(g, &[], &[("out", ty)])
        }
        "file_source" => {
            let ty = cx.ty("c32", ANY)?;
            if cx.f64("sample_rate", 0.0) != 0.0 || cx.f64("center_freq", 0.0) != 0.0 {
                cx.warn(
                    "GNU Radio's File Source has no sample rate or centre frequency: they \
                     set the rates of the blocks after it, but aren't kept",
                );
            }
            let g = GBlock::new("blocks_file_source")
                .s("file", &cx.text("path", ""))
                .p("type", ty)
                .b("repeat", cx.flag("repeat", false))
                .p("offset", "0")
                .p("length", "0")
                .p("begin_tag", "pmt.PMT_NIL")
                .p("vlen", "1");
            Out::one(g, &[], &[("out", ty)])
        }
        "sigmf_source" => {
            let ty = cx.ty("c32", ANY)?;
            let data = sigmf_data(&cx.text("path", ""));
            cx.warn(format!(
                "gr-sigmf isn't part of GNU Radio: reads the raw samples of `{data}` with a \
                 File Source (no metadata: rate, frequency and annotations are lost, and \
                 the file's datatype must match `{ty}`)"
            ));
            let g = GBlock::new("blocks_file_source")
                .s("file", &data)
                .p("type", ty)
                .b("repeat", cx.flag("repeat", false))
                .p("offset", "0")
                .p("length", "0")
                .p("begin_tag", "pmt.PMT_NIL")
                .p("vlen", "1");
            Out::one(g, &[], &[("out", ty)])
        }
        "wav_source" => {
            let t = cx.text("type", "f32");
            let channel = cx.f64("channel", 0.0).max(0.0) as usize;
            let path = cx.text("path", "");
            let repeat = cx.flag("repeat", false);
            let wav = |nchan: usize| {
                GBlock::new("blocks_wavfile_source")
                    .s("file", &path)
                    .b("repeat", repeat)
                    .p("nchan", nchan.to_string())
            };
            match t.as_str() {
                "f32" => {
                    let mut o = Out::default();
                    o.add(wav(channel + 1), "");
                    o.output("out", 0, channel, F);
                    o.spare_outputs(0, channel, F);
                    o
                }
                "c32" => {
                    let mut o = Out::default();
                    let w = o.add(wav(2), "");
                    let c = o.add(GBlock::new("blocks_float_to_complex").p("vlen", "1"), "iq");
                    o.link(w, 0, c, 0);
                    o.link(w, 1, c, 1);
                    o.output("out", c, 0, C);
                    o
                }
                "i16" => {
                    let mut o = Out::default();
                    let w = o.add(wav(channel + 1), "");
                    let c = o.add(
                        GBlock::new("blocks_float_to_short")
                            .p("vlen", "1")
                            .p("scale", "32767"),
                        "i16",
                    );
                    o.link(w, channel, c, 0);
                    o.output("out", c, 0, S);
                    o.spare_outputs(w, channel, F);
                    o
                }
                t => return Err(format!("stream type `{t}` isn't supported")),
            }
        }
        "sdr_source" => sdr(cx, false)?,
        "audio_source" => {
            let device = cx.text("device", "");
            let channel = cx.f64("channel", 0.0).max(0.0) as usize;
            let mut o = Out::default();
            o.add(
                GBlock::new("audio_source")
                    .p("samp_rate", "48000")
                    .s("device_name", &device)
                    .p("ok_to_block", "True")
                    .p("num_outputs", (channel + 1).to_string()),
                "",
            );
            o.output("out", 0, channel, F);
            o.spare_outputs(0, channel, F);
            cx.warn("the sound card's rate is set to 48000 in GNU Radio; check it");
            o
        }
        "udp_source" => {
            let ty = cx.ty("c32", ANY)?;
            let host = cx.text("host", "127.0.0.1");
            if host != "0.0.0.0" {
                cx.warn(format!(
                    "GNU Radio's UDP Source listens on every interface, not only `{host}`: \
                     the stream is open to the network"
                ));
            }
            if !cx.text("interface", "").is_empty() {
                cx.warn("multicast isn't supported by GNU Radio's UDP Source");
            }
            let header = udp_header(cx);
            let g = GBlock::new("network_udp_source")
                .p("type", ty)
                .e("port", &cx.int("port", 2000))
                .p("header", header)
                .p("payloadsize", "1472")
                .p("notify_missed", "False")
                .p("src_zeros", "False")
                .p("ipv6", "False")
                .p("vlen", "1");
            Out::one(g, &[], &[("out", ty)])
        }
        "tcp_source" => {
            let ty = cx.ty("c32", ANY)?;
            let server = cx.text("mode", "server") == "server";
            let g = GBlock::new("network_tcp_source")
                .p("type", ty)
                .s("addr", &cx.text("host", "127.0.0.1"))
                .e("port", &cx.int("port", 2000))
                .b("server", server)
                .p("vlen", "1");
            Out::one(g, &[], &[("out", ty)])
        }

        // ---- Flow ----
        "throttle" => {
            let ty = cx.ty("c32", ANY)?;
            let rate = match cx.num_q("sample_rate", 0.0, true) {
                Py::Num(0.0, _) => cx.rate(),
                e => e,
            };
            let g = GBlock::new("blocks_throttle2")
                .p("type", ty)
                .e("samples_per_second", &rate)
                .p("ignoretag", "True")
                .p("limit", "auto")
                .p("maximum", "0.1")
                .p("vlen", "1");
            Out::through(g, ty)
        }
        "head" | "skip" => {
            let ty = cx.ty("c32", ANY)?;
            let id = if kind == "head" {
                "blocks_head"
            } else {
                "blocks_skiphead"
            };
            let g = GBlock::new(id)
                .p("type", ty)
                .e("num_items", &cx.int("count", 48000))
                .p("vlen", "1");
            Out::through(g, ty)
        }
        "delay" => {
            let ty = cx.ty("c32", ANY)?;
            let g = GBlock::new("blocks_delay")
                .p("type", ty)
                .e("delay", &cx.int("delay", 0))
                .p("num_ports", "1")
                .p("vlen", "1")
                .p("showports", "False");
            Out::through(g, ty)
        }
        "valve" => {
            let ty = cx.ty("c32", ANY)?;
            if cx.text("when_closed", "drop") == "block" {
                cx.warn("GNU Radio's Copy drops while disabled; `when_closed = block` isn't kept");
            }
            let g = GBlock::new("blocks_copy")
                .p("type", ty)
                .b("enabled", cx.flag("open", true))
                .p("vlen", "1")
                .p("showports", "False");
            Out::through(g, ty)
        }
        "selector" => {
            let ty = cx.ty("c32", ANY)?;
            let k = (cx.f64("inputs", 2.0) as usize).clamp(2, 4);
            let g = GBlock::new("blocks_selector")
                .p("type", ty)
                .p("enabled", "True")
                .p("num_inputs", k.to_string())
                .p("num_outputs", "1")
                .e("input_index", &cx.int("input", 0))
                .p("output_index", "0")
                .p("vlen", "1")
                .p("showports", "True");
            let ins: Vec<(String, &'static str)> = (0..k).map(|i| (format!("in{i}"), ty)).collect();
            let ins: Vec<(&str, &'static str)> =
                ins.iter().map(|(p, t)| (p.as_str(), *t)).collect();
            Out::one(g, &ins, &[("out", ty)])
        }

        // ---- Sinks ----
        "null_sink" => {
            let ty = cx.ty("c32", ANY)?;
            let g = GBlock::new("blocks_null_sink")
                .p("type", ty)
                .p("num_inputs", "1")
                .p("vlen", "1")
                .p("bus_structure_sink", "[[0,],]");
            Out::one(g, &[("in", ty)], &[])
        }
        "file_sink" => {
            let ty = cx.ty("c32", ANY)?;
            let g = file_sink(&cx.text("path", ""), ty);
            Out::one(g, &[("in", ty)], &[])
        }
        "sigmf_sink" => {
            let ty = cx.ty("c32", ANY)?;
            let data = sigmf_data(&cx.text("path", ""));
            cx.warn(format!(
                "gr-sigmf isn't part of GNU Radio: writes the raw samples to `{data}` with a \
                 File Sink (no .sigmf-meta: rate, frequency and tags are lost)"
            ));
            Out::one(file_sink(&data, ty), &[("in", ty)], &[])
        }
        "wav_sink" => {
            let t = cx.text("type", "f32");
            let bits = if cx.text("format", "float32") == "pcm16" {
                "FORMAT_PCM_16"
            } else {
                "FORMAT_FLOAT"
            };
            let rate = intify(cx.rate());
            let nchan = if t == "c32" { 2 } else { 1 };
            let g = GBlock::new("blocks_wavfile_sink")
                .s("file", &cx.text("path", ""))
                .p("nchan", nchan.to_string())
                .e("samp_rate", &rate)
                .p("format", "FORMAT_WAV")
                .p("bits_per_sample1", bits)
                .p("append", "False");
            let mut o = Out::default();
            let w = o.add(g, "");
            match t.as_str() {
                "f32" => o.input("in", w, 0, F),
                "c32" => {
                    let c = stereo_split(&mut o, w);
                    o.input("in", c, 0, C);
                }
                "i16" => {
                    let c = o.add(
                        GBlock::new("blocks_short_to_float")
                            .p("vlen", "1")
                            .p("scale", "32767"),
                        "f32",
                    );
                    o.link(c, 0, w, 0);
                    o.input("in", c, 0, S);
                }
                t => return Err(format!("stream type `{t}` isn't supported")),
            }
            o
        }
        "audio_sink" | "browser_audio" => {
            let t = if kind == "audio_sink" {
                "f32".to_owned()
            } else {
                cx.text("type", "f32")
            };
            let rate = intify(cx.rate());
            let device = if kind == "audio_sink" {
                cx.text("device", "")
            } else {
                String::new()
            };
            // Paced by hardware upstream: don't block on the sound card.
            let ok_to_block = kind == "audio_sink" || cx.flag("pace", true);
            let g = GBlock::new("audio_sink")
                .e("samp_rate", &rate)
                .s("device_name", &device)
                .b("ok_to_block", ok_to_block)
                .p("num_inputs", if t == "c32" { "2" } else { "1" });
            let mut o = Out::default();
            let a = o.add(g, "");
            if t == "c32" {
                let c = stereo_split(&mut o, a);
                o.input("in", c, 0, C);
            } else {
                o.input("in", a, 0, F);
            }
            o
        }
        "udp_sink" => {
            let ty = cx.ty("c32", ANY)?;
            let header = udp_header(cx);
            // foxygnal's datagram size includes the header; GNU Radio's doesn't.
            let extra = if header == "1" { 8 } else { 0 };
            let payload = (cx.f64("payload", 1472.0) as i64 - extra).max(1);
            let g = GBlock::new("network_udp_sink")
                .p("type", ty)
                .s("addr", &cx.text("host", "127.0.0.1"))
                .e("port", &cx.int("port", 2000))
                .p("header", header)
                .p("payloadsize", payload.to_string())
                .p("send_eof", "False")
                .p("vlen", "1");
            Out::one(g, &[("in", ty)], &[])
        }
        "tcp_sink" => {
            let ty = cx.ty("c32", ANY)?;
            let server = cx.text("mode", "server") == "server";
            if !server && cx.flag("reconnect", true) {
                cx.warn("GNU Radio's TCP Sink client doesn't reconnect");
            }
            let g = GBlock::new("network_tcp_sink")
                .p("type", ty)
                .p("mode", if server { "2" } else { "1" })
                .s("addr", &cx.text("host", "127.0.0.1"))
                .e("port", &cx.int("port", 2000))
                .p("vlen", "1");
            Out::one(g, &[("in", ty)], &[])
        }
        "sdr_sink" => sdr(cx, true)?,

        // ---- Math ----
        "add" | "subtract" | "multiply" | "divide" => {
            let ty = cx.ty("c32", CF)?;
            let id = match kind.as_str() {
                "add" => "blocks_add_xx",
                "subtract" => "blocks_sub_xx",
                "multiply" => "blocks_multiply_xx",
                _ => "blocks_divide_xx",
            };
            let g = GBlock::new(id)
                .p("type", ty)
                .p("num_inputs", "2")
                .p("vlen", "1");
            Out::one(g, &[("a", ty), ("b", ty)], &[("out", ty)])
        }
        "gain" => {
            let ty = cx.ty("c32", CF)?;
            let g = GBlock::new("blocks_multiply_const_xx")
                .p("type", ty)
                .e("const", &cx.num("gain", 1.0))
                .p("vlen", "1");
            Out::through(g, ty)
        }
        "add_const" => {
            let ty = cx.ty("c32", CF)?;
            let re = cx.num("constant", 0.0);
            let im = cx.num("constant_imag", 0.0);
            let k = if ty == C && !matches!(im, Py::Num(v, _) if v == 0.0) {
                cx.warn(
                    "a complex constant: GNU Radio takes it as `complex(re, im)`, which the \
                     importer can't read back",
                );
                Py::Call("complex".into(), vec![re, im], vec![])
            } else {
                re
            };
            let g = GBlock::new("blocks_add_const_vxx")
                .p("type", ty)
                .e("const", &k)
                .p("vlen", "1");
            Out::through(g, ty)
        }
        "conjugate" => Out::through(GBlock::new("blocks_conjugate_cc"), C),
        "abs" => Out::through(GBlock::new("blocks_abs_xx").p("type", F).p("vlen", "1"), F),
        "log_power" => {
            let ty = cx.ty("c32", CF)?;
            let offset = cx.num("offset", 0.0);
            if cx.f64("floor", -200.0) != -200.0 {
                cx.warn("GNU Radio's Log10 has no floor: silence gives −inf");
            }
            // 10·log10(|x|²) + k: |x|² then n = 10, or |x| then n = 20.
            let (pre, nn) = if ty == C {
                (
                    GBlock::new("blocks_complex_to_mag_squared").p("vlen", "1"),
                    "10",
                )
            } else {
                (
                    GBlock::new("blocks_abs_xx").p("type", F).p("vlen", "1"),
                    "20",
                )
            };
            let log = GBlock::new("blocks_nlog10_ff")
                .p("n", nn)
                .e("k", &offset)
                .p("vlen", "1");
            // The block's name goes to the Log10; the magnitude goes before it.
            let mut o = Out::default();
            let l = o.add(log, "");
            let m = o.add(pre, "mag");
            o.link(m, 0, l, 0);
            o.input("in", m, 0, ty);
            o.output("out", l, 0, F);
            o
        }

        // ---- Converters ----
        "c32_to_f32" => {
            let id = match cx.text("part", "real").as_str() {
                "imag" => "blocks_complex_to_imag",
                "magnitude" => "blocks_complex_to_mag",
                "magnitude_squared" => "blocks_complex_to_mag_squared",
                "phase" => "blocks_complex_to_arg",
                _ => "blocks_complex_to_real",
            };
            Out::one(GBlock::new(id).p("vlen", "1"), &[("in", C)], &[("out", F)])
        }
        "f32_to_c32" => Out::one(
            GBlock::new("blocks_float_to_complex").p("vlen", "1"),
            &[("in", F)],
            &[("out", C)],
        ),
        "f32_pair_to_c32" => Out::one(
            GBlock::new("blocks_float_to_complex").p("vlen", "1"),
            &[("re", F), ("im", F)],
            &[("out", C)],
        ),
        "c32_to_f32_pair" => Out::one(
            GBlock::new("blocks_complex_to_float").p("vlen", "1"),
            &[("in", C)],
            &[("re", F), ("im", F)],
        ),
        "i16_to_f32" => {
            // foxygnal: ±32767 → ±scale; GNU Radio divides by its scale.
            let scale = div(n(32767.0), cx.num("scale", 1.0));
            Out::one(
                GBlock::new("blocks_short_to_float")
                    .p("vlen", "1")
                    .e("scale", &scale),
                &[("in", S)],
                &[("out", F)],
            )
        }
        "f32_to_i16" => {
            let scale = div(n(32767.0), cx.num("scale", 1.0));
            Out::one(
                GBlock::new("blocks_float_to_short")
                    .p("vlen", "1")
                    .e("scale", &scale),
                &[("in", F)],
                &[("out", S)],
            )
        }
        "u8_to_f32" => Out::one(
            GBlock::new("blocks_uchar_to_float"),
            &[("in", B)],
            &[("out", F)],
        ),
        "ci16_to_c32" => {
            let scale = div(n(32767.0), cx.num("scale", 1.0));
            Out::one(
                GBlock::new("blocks_interleaved_short_to_complex")
                    .e("scale_factor", &scale)
                    .p("vector_input", "False")
                    .p("swap", "False"),
                &[("in", S)],
                &[("out", C)],
            )
        }
        "cu8_to_c32" => {
            // (x − 127.5) / 127.5 on deinterleaved unsigned bytes.
            let mut o = Out::default();
            let d = o.add(
                GBlock::new("blocks_deinterleave")
                    .p("type", B)
                    .p("num_streams", "2")
                    .p("blocksize", "1")
                    .p("vlen", "1"),
                "",
            );
            let re = o.add(GBlock::new("blocks_uchar_to_float"), "re");
            let im = o.add(GBlock::new("blocks_uchar_to_float"), "im");
            let fc = o.add(GBlock::new("blocks_float_to_complex").p("vlen", "1"), "iq");
            let ac = o.add(
                GBlock::new("blocks_add_const_vxx")
                    .p("type", C)
                    .p("const", "complex(-127.5, -127.5)")
                    .p("vlen", "1"),
                "center",
            );
            let mc = o.add(
                GBlock::new("blocks_multiply_const_xx")
                    .p("type", C)
                    .p("const", "1 / 127.5")
                    .p("vlen", "1"),
                "scale",
            );
            o.link(d, 0, re, 0);
            o.link(d, 1, im, 0);
            o.link(re, 0, fc, 0);
            o.link(im, 0, fc, 1);
            o.link(fc, 0, ac, 0);
            o.link(ac, 0, mc, 0);
            o.input("in", d, 0, B);
            o.output("out", mc, 0, C);
            o
        }

        // ---- Filters ----
        "fir_filter" => {
            let ty = cx.ty("c32", CF)?;
            let mode = cx.text("mode", "low_pass");
            let id = match mode.as_str() {
                "high_pass" => "high_pass_filter",
                "band_pass" => "band_pass_filter",
                "band_stop" => "band_reject_filter",
                _ => "low_pass_filter",
            };
            let (win, beta) = window(cx);
            let mut g = GBlock::new(id)
                .p(
                    "type",
                    if ty == C {
                        "fir_filter_ccf"
                    } else {
                        "fir_filter_fff"
                    },
                )
                .p("decim", "1")
                .p("interp", "1")
                .e("gain", &cx.num("gain", 1.0))
                .e("samp_rate", &cx.rate())
                .e("width", &cx.num("transition", 1000.0))
                .p("win", win)
                .e("beta", &beta);
            g = if matches!(mode.as_str(), "band_pass" | "band_stop") {
                g.e("low_cutoff_freq", &cx.num("cutoff", 5000.0))
                    .e("high_cutoff_freq", &cx.num("cutoff_high", 10000.0))
            } else {
                g.e("cutoff_freq", &cx.num("cutoff", 5000.0))
            };
            Out::through(g, ty)
        }
        "freq_xlating_fir" => {
            let rate = cx.rate();
            let (win, beta) = window(cx);
            let taps = Py::Call(
                "firdes.low_pass".into(),
                vec![
                    n(1.0),
                    rate.clone(),
                    cx.num("cutoff", 100000.0),
                    cx.num("transition", 20000.0),
                    name(win),
                    beta,
                ],
                vec![],
            );
            let g = GBlock::new("freq_xlating_fir_filter_xxx")
                .p("type", "ccf")
                .e("decim", &cx.int("decimation", 1))
                .e("taps", &taps)
                .e("center_freq", &cx.num("offset", 0.0))
                .e("samp_rate", &rate);
            Out::through(g, C)
        }
        "rrc_filter" => {
            let ty = cx.ty("c32", CF)?;
            let interpolate = cx.text("mode", "filter") == "interpolate";
            let sps = cx.num("sps", 4.0);
            let sps_v = cx.eval(&sps).unwrap_or(4.0);
            let span = cx.num("span", 11.0);
            let gain = cx.num("gain", 1.0);
            let rin = cx.rate();
            let code = if ty == C { "ccf" } else { "fff" };
            let mut g = GBlock::new("root_raised_cosine_filter")
                .p("decim", "1")
                .e("alpha", &cx.num("rolloff", 0.35))
                .e("ntaps", &intify(mul(span, sps.clone())));
            if interpolate {
                if sps_v.fract() != 0.0 {
                    cx.warn(format!(
                        "GNU Radio interpolates by a whole number: {} samples per symbol \
                         rounded",
                        py::py_num(sps_v)
                    ));
                }
                // Taps at the output rate; GNU Radio's sum to the gain, so a
                // gain of sps peaks at 1 like foxygnal's.
                let out_rate = mul(rin.clone(), sps.clone());
                g = g
                    .p("type", format!("interp_fir_filter_{code}"))
                    .e("interp", &intify(sps.clone()))
                    .e("gain", &mul(gain, sps))
                    .e("samp_rate", &out_rate)
                    .e("sym_rate", &rin);
            } else {
                g = g
                    .p("type", format!("fir_filter_{code}"))
                    .p("interp", "1")
                    .e("gain", &gain)
                    .e("samp_rate", &rin)
                    .e("sym_rate", &div(rin.clone(), sps));
            }
            Out::through(g, ty)
        }
        "biquad" => biquad(cx)?,
        "dc_blocker" => {
            let ty = cx.ty("c32", CF)?;
            let rate = cx.rate();
            let cutoff = cx.f64("cutoff", 10.0).max(1e-9);
            // GNU Radio's DC blocker is two moving averages of D samples:
            // −3 dB near 0.42 · rate / D.
            let d = match cx.eval(&rate) {
                Some(r) => ((0.42 * r / cutoff).round() as i64).max(2),
                None => 32,
            };
            cx.warn(format!(
                "GNU Radio's DC Blocker is a moving average: a length of {d} samples comes \
                 close to the {} Hz cutoff",
                py::py_num(cutoff)
            ));
            let g = GBlock::new("dc_blocker_xx")
                .p("type", if ty == C { "cc" } else { "ff" })
                .p("length", d.to_string())
                .p("long_form", "True");
            Out::through(g, ty)
        }
        "hilbert" => {
            let (win, beta) = window(cx);
            let g = GBlock::new("hilbert_fc")
                .e("num_taps", &cx.int("taps", 65))
                .p("win", win)
                .e("beta", &beta);
            Out::one(g, &[("in", F)], &[("out", C)])
        }

        // ---- Resamplers ----
        "rational_resampler" => {
            let ty = cx.ty("c32", CF)?;
            let (l, m) = match cx.num_q("output_rate", 48000.0, true) {
                Py::Num(0.0, _) => (cx.int("interpolation", 1), cx.int("decimation", 1)),
                out => {
                    let rin = cx.rate();
                    match (cx.eval(&out), cx.eval(&rin)) {
                        (Some(o), Some(i)) if o > 0.0 && i > 0.0 => {
                            let (l, m) = ratio(o, i);
                            (n(l as f64), n(m as f64))
                        }
                        _ => {
                            cx.warn(
                                "the output rate can't be worked out as a ratio: interpolation \
                                 and decimation are set from the rates as they are",
                            );
                            (intify(out), intify(rin))
                        }
                    }
                }
            };
            resampler(ty, l, m)
        }
        "decimator" => {
            let ty = cx.ty("c32", CF)?;
            let m = cx.int("decimation", 4);
            resampler(ty, n(1.0), m)
        }
        "interpolator" => {
            let ty = cx.ty("c32", CF)?;
            let l = cx.int("interpolation", 4);
            resampler(ty, l, n(1.0))
        }

        // ---- Demodulators ----
        "fm_demod" => {
            let rate = cx.rate();
            let dev = cx.num("deviation", 75000.0);
            let gain = div(rate.clone(), mul(mul(n(2.0), name("math.pi")), dev));
            let demod = GBlock::new("analog_quadrature_demod_cf").e("gain", &gain);
            let tau = match cx.text("deemphasis", "50us").as_str() {
                "50us" => Some("50e-6"),
                "75us" => Some("75e-6"),
                _ => None,
            };
            match tau {
                None => Out::one(demod, &[("in", C)], &[("out", F)]),
                Some(tau) => {
                    let de = GBlock::new("analog_fm_deemph")
                        .e("samp_rate", &rate)
                        .p("tau", tau);
                    Out::chain(vec![(demod, ""), (de, "deemph")], C, F)
                }
            }
        }
        "am_demod" => {
            let rate = cx.rate();
            cx.warn(
                "GNU Radio's AM Demod low-passes the audio: set to 0.4 and 0.45 × the rate, \
                 i.e. no real limit; DC removal is its own",
            );
            let g = GBlock::new("analog_am_demod_cf")
                .e("chan_rate", &rate)
                .p("audio_decim", "1")
                .e("audio_pass", &mul(rate.clone(), n(0.4)))
                .e("audio_stop", &mul(rate, n(0.45)));
            Out::one(g, &[("in", C)], &[("out", F)])
        }

        // ---- Level ----
        "agc" => {
            let ty = cx.ty("c32", CF)?;
            let rate = cx.rate();
            // Seconds → GNU Radio's per-sample adaptation rates.
            let per_sample = |cx: &mut Cx<'_>, key: &str, default: f64| {
                let t = cx.num(key, default);
                match (cx.eval(&t), cx.eval(&rate)) {
                    (Some(t), Some(r)) if t > 0.0 && r > 0.0 => n((1.0 / (t * r)).min(1.0)),
                    _ => div(n(1.0), mul(t, rate.clone())),
                }
            };
            let attack = per_sample(cx, "attack", 0.001);
            let decay = per_sample(cx, "decay", 0.1);
            cx.warn(
                "attack and decay (seconds) became GNU Radio's per-sample rates (1 / (time × \
                 rate)); the loops differ, so check the levels",
            );
            let g = GBlock::new("analog_agc2_xx")
                .p("type", ty)
                .e("attack_rate", &attack)
                .e("decay_rate", &decay)
                .e("reference", &cx.num("target", 1.0))
                .p("gain", "1.0")
                .e("max_gain", &cx.num("max_gain", 100000.0));
            Out::through(g, ty)
        }
        "squelch" => {
            let ty = cx.ty("c32", CF)?;
            let rate = cx.rate();
            let avg = cx.f64("averaging", 0.01);
            let ramp = cx.f64("ramp", 0.002);
            let (alpha, ramp_n) = match cx.eval(&rate) {
                Some(r) if r > 0.0 => (
                    n((1.0 / (avg.max(1e-9) * r)).min(1.0)),
                    n((ramp * r).round()),
                ),
                _ => (n(1e-4), n(0.0)),
            };
            if cx.f64("hysteresis", 3.0) != 0.0 {
                cx.warn("GNU Radio's Power Squelch has no hysteresis");
            }
            let g = GBlock::new("analog_pwr_squelch_xx")
                .p("type", ty)
                .e("threshold", &cx.num("threshold", -40.0))
                .e("alpha", &alpha)
                .e("ramp", &ramp_n)
                .p("gate", "False");
            Out::through(g, ty)
        }

        // ---- Digital ----
        "costas_loop" => {
            let order = cx.text("order", "2");
            let g = GBlock::new("digital_costas_loop_cc")
                .e("w", &cx.num("loop_bw", 0.0628))
                .p("order", order)
                .p("use_snr", "False");
            let mut o = Out::default();
            let c = o.add(g, "");
            o.input("in", c, 0, C);
            o.output("out", c, 0, C);
            if cx.used_out.contains("freq") || cx.used_out.contains("1") {
                // GNU Radio's frequency output is in rad/sample; foxygnal's in Hz.
                let hz = div(cx.rate(), mul(n(2.0), name("math.pi")));
                let m = GBlock::new("blocks_multiply_const_xx")
                    .p("type", F)
                    .e("const", &hz)
                    .p("vlen", "1");
                let k = o.add(m, "hz");
                o.link(c, 1, k, 0);
                o.output("freq", k, 0, F);
            } else {
                o.output("freq", c, 1, F);
            }
            o
        }
        "clock_recovery" => {
            let ty = cx.ty("c32", CF)?;
            let ted = if cx.text("ted", "gardner") == "mueller_muller" {
                "digital.TED_MUELLER_AND_MULLER"
            } else {
                "digital.TED_GARDNER"
            };
            let g = GBlock::new("digital_symbol_sync_xx")
                .p("type", if ty == C { "cc" } else { "ff" })
                .p("ted_type", ted)
                .p("constellation", "digital.constellation_bpsk().base()")
                .e("sps", &cx.num("sps", 8.0))
                .p("ted_gain", "1.0")
                .e("loop_bw", &cx.num("loop_bw", 0.01))
                .p("damping", "1.0")
                .p("max_dev", "1.5")
                .p("osps", "1")
                .p("resamp_type", "digital.IR_MMSE_8TAP")
                .p("nfilters", "128")
                .p("pfb_mf_taps", "[]");
            Out::through(g, ty)
        }
        "binary_slicer" => {
            let th = cx.num("threshold", 0.0);
            let slicer = GBlock::new("digital_binary_slicer_fb");
            if matches!(th, Py::Num(v, _) if v == 0.0) {
                Out::one(slicer, &[("in", F)], &[("out", B)])
            } else {
                // GNU Radio slices at 0: shift by the threshold first.
                let shift = GBlock::new("blocks_add_const_vxx")
                    .p("type", F)
                    .e("const", &Py::Neg(Box::new(th)))
                    .p("vlen", "1");
                let mut o = Out::default();
                let s = o.add(slicer, "");
                let a = o.add(shift, "threshold");
                o.link(a, 0, s, 0);
                o.input("in", a, 0, F);
                o.output("out", s, 0, B);
                o
            }
        }
        "correlate_access_code" => {
            let code = cx.text("access_code", "0x1ACFFC1D");
            let bits = access_code_bits(&code)
                .ok_or_else(|| format!("access code `{code}` isn't hex or a bit string"))?;
            let g = GBlock::new("digital_correlate_access_code_tag_xx")
                .p("type", B)
                .s("access_code", &bits)
                .e("threshold", &cx.int("threshold", 2))
                .s("tagname", "packet");
            Out::through(g, B)
        }

        k if k.starts_with(super::GRAPH_KIND) => {
            return Err("the sub-graph couldn't be flattened".into());
        }
        "adsb_decoder" | "rds_decoder" | "packet_decoder" | "message_log" | "pocsag_decoder"
        | "flex_decoder" | "aprs_decoder" | "ais_decoder" | "acars_decoder" | "sstv_decoder"
        | "image_sink" | "dvbs_modulator" | "dvbs_demodulator" | "ism_decoder"
        | "ccsds_deframer" | "ccsds_packets" | "ccsds_framer" | "ccsds_modulator"
        | "goes_dcs_decoder" | "goes_dcs_sim" | "xrit_decoder" | "xrit_source" => {
            return Err("a foxygnal decoder with no GNU Radio equivalent".into());
        }
        "fm_stereo" | "ssb_demod" | "fsk_demod" | "psk_demod" => {
            return Err("GNU Radio has no block that does the same".into());
        }
        _ => return Err("no GNU Radio equivalent".into()),
    };
    Ok(out)
}

fn file_sink(path: &str, ty: &'static str) -> GBlock {
    GBlock::new("blocks_file_sink")
        .s("file", path)
        .p("type", ty)
        .p("unbuffered", "False")
        .p("append", "False")
        .p("vlen", "1")
}

/// A complex stereo stream (left = I, right = Q) into inputs 0 and 1 of
/// block `sink`, through a Complex To Float. Returns the converter.
fn stereo_split(o: &mut Out, sink: usize) -> usize {
    let g = GBlock::new("blocks_complex_to_float").p("vlen", "1");
    let c = o.add(g, "lr");
    o.link(c, 0, sink, 0);
    o.link(c, 1, sink, 1);
    c
}

fn resampler(ty: &'static str, l: Py, m: Py) -> Out {
    let g = GBlock::new("rational_resampler_xxx")
        .p("type", if ty == C { "ccc" } else { "fff" })
        .e("interp", &l)
        .e("decim", &m)
        .p("taps", "[]")
        .p("fbw", "0");
    Out::through(g, ty)
}

/// `out / in` as a reduced fraction (rates in Hz, rounded to 1 mHz).
fn ratio(out: f64, inp: f64) -> (u64, u64) {
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let (a, b) = ((out * 1000.0).round() as u64, (inp * 1000.0).round() as u64);
    let g = gcd(a, b).max(1);
    (a / g, b / g)
}

/// The window of a FIR design and Kaiser's β for its attenuation.
fn window(cx: &mut Cx<'_>) -> (&'static str, Py) {
    let w = cx.text("window", "hamming");
    let att = cx.f64("attenuation", 60.0);
    let beta = if att > 50.0 {
        0.1102 * (att - 8.7)
    } else if att >= 21.0 {
        0.5842 * (att - 21.0).powf(0.4) + 0.07886 * (att - 21.0)
    } else {
        0.0
    };
    match w.as_str() {
        "blackman" => ("window.WIN_BLACKMAN", n(6.76)),
        "kaiser" => ("window.WIN_KAISER", n((beta * 1000.0).round() / 1000.0)),
        _ => ("window.WIN_HAMMING", n(6.76)),
    }
}

fn udp_header(cx: &mut Cx<'_>) -> &'static str {
    match cx.text("header", "none").as_str() {
        "seq64" => "1",
        "none" => "0",
        h => {
            cx.warn(format!(
                "header `{h}` has no GNU Radio equivalent: no header (raw samples)"
            ));
            "0"
        }
    }
}

/// The `.sigmf-data` file of a SigMF recording (`rec`, `rec.sigmf-meta`).
fn sigmf_data(path: &str) -> String {
    let base = [".sigmf-meta", ".sigmf-data", ".sigmf"]
        .iter()
        .find_map(|e| path.strip_suffix(e))
        .unwrap_or(path);
    format!("{base}.sigmf-data")
}

/// An access code (`0x1ACFFC1D` or `1010…`) as GNU Radio's bit string.
fn access_code_bits(code: &str) -> Option<String> {
    let c = code.trim();
    if let Some(hex) = c.strip_prefix("0x").or_else(|| c.strip_prefix("0X")) {
        if hex.is_empty() || hex.len() > 16 {
            return None;
        }
        let v = u64::from_str_radix(hex, 16).ok()?;
        return Some(format!("{v:0width$b}", width = hex.len() * 4));
    }
    (!c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b == b'0' || b == b'1'))
        .then(|| c.to_owned())
}

/// RBJ biquads, designed here (GNU Radio has no such block), as IIR Filters
/// (one per stage).
fn biquad(cx: &mut Cx<'_>) -> Result<Out, String> {
    let ty = cx.ty("c32", CF)?;
    let rate = cx.rate();
    let r = cx
        .eval(&rate)
        .ok_or("the sample rate isn't a number, so the taps can't be designed")?;
    let f0 = cx.f64("freq", 1000.0) / r;
    if !(f0 > 0.0 && f0 < 0.5) {
        return Err("the frequency is outside 0 … Nyquist".into());
    }
    let q = cx.f64("q", std::f64::consts::FRAC_1_SQRT_2).max(1e-6);
    let gain_db = cx.f64("gain_db", 0.0);
    let shape = cx.text("shape", "low_pass");
    let w0 = std::f64::consts::TAU * f0;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q);
    let a = 10f64.powf(gain_db / 40.0);
    let sa = 2.0 * a.sqrt() * alpha;
    let (b0, b1, b2, a0, a1, a2) = match shape.as_str() {
        "high_pass" => (
            (1.0 + cos) / 2.0,
            -(1.0 + cos),
            (1.0 + cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        "band_pass" => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        "notch" => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        "all_pass" => (
            1.0 - alpha,
            -2.0 * cos,
            1.0 + alpha,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
        "peaking" => (
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        ),
        "low_shelf" => (
            a * ((a + 1.0) - (a - 1.0) * cos + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - sa),
            (a + 1.0) + (a - 1.0) * cos + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - sa,
        ),
        "high_shelf" => (
            a * ((a + 1.0) + (a - 1.0) * cos + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - sa),
            (a + 1.0) - (a - 1.0) * cos + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - sa,
        ),
        _ => (
            (1.0 - cos) / 2.0,
            1.0 - cos,
            (1.0 - cos) / 2.0,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        ),
    };
    let list = |v: &[f64]| {
        format!(
            "[{}]",
            v.iter()
                .map(|x| format!("{x:e}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let ff = list(&[b0 / a0, b1 / a0, b2 / a0]);
    let fb = list(&[1.0, a1 / a0, a2 / a0]);
    let stages = (cx.f64("stages", 1.0).round() as usize).clamp(1, 16);
    cx.warn(format!(
        "designed here as {stages} IIR Filter stage(s) for {} S/s: the taps don't follow \
         changes to the rate or the parameters",
        py::py_num(r)
    ));
    let mut o = Out::default();
    for k in 0..stages {
        let g = GBlock::new("iir_filter_xxx")
            .p("type", if ty == C { "ccd" } else { "ffd" })
            .p("fftaps", ff.clone())
            .p("fbtaps", fb.clone())
            .p("oldstyle", "False");
        let i = o.add(g, &format!("s{k}"));
        if k > 0 {
            o.link(i - 1, 0, i, 0);
        }
    }
    o.input("in", 0, 0, ty);
    o.output("out", stages - 1, 0, ty);
    Ok(o)
}

// ---- Hardware ---------------------------------------------------------------------

/// `driver:key=value,…` → (driver, [(key, value)]).
fn parse_device(dev: &str) -> (String, Vec<(String, String)>) {
    let (driver, rest) = dev.split_once(':').unwrap_or((dev, ""));
    let kv = rest
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.trim().to_owned(), v.trim().to_owned()),
            None => (p.to_owned(), String::new()),
        })
        .collect();
    (driver.trim().to_owned(), kv)
}

/// SDR source / sink → osmocom (RTL-SDR, rtl_tcp, HackRF, or no device) or
/// gr-soapy (SoapySDR devices).
fn sdr(cx: &mut Cx<'_>, tx: bool) -> Result<Out, String> {
    let device = cx.text("device", "");
    let (driver, kv) = parse_device(&device);
    let get = |k: &str| kv.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    let rate = cx.num("sample_rate", if tx { 0.0 } else { 2.4e6 });
    let rate = if tx && matches!(rate, Py::Num(v, _) if v == 0.0) {
        cx.rate()
    } else {
        rate
    };
    let freq = cx.num("frequency", if tx { 0.0 } else { 100e6 });
    let gain = cx.num("gain", if tx { 0.0 } else { 30.0 });
    let bw = cx.num("bandwidth", 0.0);
    let antenna = cx.text("antenna", "");
    let agc = !tx && cx.flag("agc", false);
    let ppm = cx.num("ppm", 0.0);
    let dir = if tx { "sink" } else { "source" };

    if driver == "soapy" {
        let soapy_driver = get("driver").unwrap_or_default();
        let args: Vec<String> = kv
            .iter()
            .filter(|(k, _)| k != "driver")
            .map(|(k, v)| {
                if v.is_empty() {
                    k.clone()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        let args = args.join(",");
        let g = if soapy_driver == "plutosdr" && !tx {
            GBlock::new("soapy_plutosdr_source")
                .p("type", "fc32")
                .s("dev_args", &args)
                .e("samp_rate", &rate)
                .e("bandwidth", &bw)
                .e("center_freq", &freq)
                .b("agc", agc)
                .e("gain", &gain)
        } else {
            let id = if tx {
                "soapy_custom_sink"
            } else {
                "soapy_custom_source"
            };
            let mut g = GBlock::new(id)
                .s("driver", &soapy_driver)
                .p("type", "fc32")
                .p("nchan", "1")
                .s("dev_args", &args)
                .s("stream_args", "")
                .e("samp_rate", &rate)
                .s("tune_args0", "")
                .s("settings0", "")
                .e("bandwidth0", &bw)
                .s("antenna0", &antenna)
                .e("center_freq0", &freq)
                .e("freq_correction0", &ppm)
                .e("gain0", &gain)
                .p("dc_offset0", "0")
                .p("iq_balance0", "0");
            if !tx {
                g = g.b("agc0", agc).p("dc_removal0", "True");
            }
            g
        };
        cx.warn(format!(
            "SoapySDR device `{soapy_driver}`: check the device arguments and gain in GNU Radio"
        ));
        return Ok(sdr_out(g, tx));
    }

    let args = match driver.as_str() {
        "rtlsdr" => {
            let mut a = vec![format!(
                "rtl={}",
                get("serial").or_else(|| get("index")).unwrap_or("0".into())
            )];
            if let Some(d) = get("direct_sampling") {
                a.push(format!("direct_samp={d}"));
            }
            if get("offset_tuning").as_deref() == Some("1") {
                a.push("offset_tune=1".into());
            }
            if get("bias_tee").as_deref() == Some("1") {
                a.push("bias=1".into());
            }
            a.join(",")
        }
        "rtltcp" => format!(
            "rtl_tcp={}:{}",
            get("host").unwrap_or("127.0.0.1".into()),
            get("port").unwrap_or("1234".into())
        ),
        "hackrf" => {
            let mut a = vec![format!(
                "hackrf={}",
                get("serial").or_else(|| get("index")).unwrap_or("0".into())
            )];
            if get("bias_tee").as_deref() == Some("1") {
                a.push("bias=1".into());
            }
            a.join(",")
        }
        "" => {
            cx.warn("no device set: osmocom picks the first one it finds");
            String::new()
        }
        other => {
            cx.warn(format!(
                "device `{device}`: GNU Radio has no driver for `{other}`; exported as an \
                 osmocom {dir} with no device arguments"
            ));
            String::new()
        }
    };
    let mut g = GBlock::new(if tx { "osmosdr_sink" } else { "osmosdr_source" })
        .p("type", "fc32")
        .s("args", &args)
        .p("sync", "sync")
        .p("num_mboards", "1")
        .s("clock_source0", "")
        .s("time_source0", "")
        .p("nchan", "1")
        .e("sample_rate", &rate)
        .e("freq0", &freq)
        .e("corr0", &ppm)
        .e("gain0", &gain)
        .p("if_gain0", "20")
        .p("bb_gain0", "20")
        .s("ant0", &antenna)
        .e("bw0", &bw);
    if !tx {
        g = g
            .p("dc_offset_mode0", "0")
            .p("iq_balance_mode0", "0")
            .b("gain_mode0", agc);
    }
    if !args.is_empty() {
        cx.warn(format!(
            "device `{device}` → osmocom `{args}`: check it in GNU Radio"
        ));
    }
    Ok(sdr_out(g, tx))
}

fn sdr_out(g: GBlock, tx: bool) -> Out {
    if tx {
        Out::one(g, &[("in", C)], &[])
    } else {
        Out::one(g, &[], &[("out", C)])
    }
}

// ---- Placeholders -------------------------------------------------------------------

/// Ports of foxygnal blocks that have no GNU Radio equivalent, in order:
/// kind, inputs, outputs (name, GRC dtype).
type KnownPorts = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [(&'static str, &'static str)],
);
const KNOWN: &[KnownPorts] = &[
    ("adsb_decoder", &[("in", F)], &[("out", B)]),
    ("rds_decoder", &[("in", F)], &[("out", B)]),
    ("packet_decoder", &[("in", B)], &[("out", B)]),
    ("ais_decoder", &[("in", F)], &[("out", B)]),
    ("acars_decoder", &[("in", F)], &[("out", B)]),
    (
        "message_log",
        &[("in", B), ("in1", B), ("in2", B), ("in3", B)],
        &[],
    ),
    ("psk_demod", &[("in", C)], &[("out", B)]),
    ("fsk_demod", &[("in", C)], &[("out", F)]),
    ("ssb_demod", &[("in", C)], &[("out", F)]),
    ("fm_stereo", &[("in", F)], &[("left", F), ("right", F)]),
];

/// The GRC dtype of a port of a block that becomes a placeholder.
pub(crate) fn known_port(kind: &str, port: &str, input: bool) -> Option<&'static str> {
    let (_, ins, outs) = KNOWN.iter().find(|k| k.0 == kind)?;
    let ports = if input { ins } else { outs };
    ports
        .iter()
        .find(|p| p.0 == port)
        .or_else(|| port.parse::<usize>().ok().and_then(|k| ports.get(k)))
        .map(|p| p.1)
}

/// A placeholder's ports: the known ones in order, else those in use.
pub(crate) fn port_order(kind: &str, input: bool, used: &BTreeSet<String>) -> Vec<String> {
    if let Some((_, ins, outs)) = KNOWN.iter().find(|k| k.0 == kind) {
        let ports = if input { ins } else { outs };
        return ports.iter().map(|p| p.0.to_owned()).collect();
    }
    used.iter().cloned().collect()
}

fn numpy(t: &str) -> &'static str {
    match t {
        "complex" => "numpy.complex64",
        "short" => "numpy.int16",
        "byte" => "numpy.uint8",
        _ => "numpy.float32",
    }
}

/// The marker line the importer reads back.
pub(crate) const MARKER: &str = "# foxygnal-block: ";

/// A constant input (see `lower`): `analog_const_source_x` of the type of
/// the input it feeds.
pub(crate) fn const_source(b: &FBlock, scope: &HashMap<String, String>, ty: &'static str) -> Out {
    let get = |k: &str| {
        b.params
            .get(k)
            .map_or(Ok(py::num(0.0)), |v| to_py(v, scope))
            .unwrap_or_else(|_| py::num(0.0))
    };
    let value = match ty {
        "complex" => Py::Call("complex".into(), vec![get("value"), get("imag")], vec![]),
        _ => get("value"),
    };
    let g = GBlock::new("analog_const_source_x")
        .p("type", ty)
        .e("const", &value);
    Out::one(g, &[], &[("out", ty)])
}

/// A block with no GNU Radio equivalent: an embedded Python block with the
/// same ports that consumes its input and produces nothing, so the file
/// opens, validates and runs in GRC. The original block (kind, params, port
/// names) is kept in a comment line, which the importer reads back.
pub(crate) fn placeholder(
    b: &FBlock,
    scope: &HashMap<String, String>,
    why: &str,
    ins: &[(String, &'static str)],
    outs: &[(String, &'static str)],
    warnings: &mut Vec<String>,
) -> Out {
    // Params as written, with variables renamed to their Python names.
    let params: serde_json::Map<String, Json> = b
        .params
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Json::String(s) if s.contains('$') => {
                    match py::from_fox(s, &|n| scope.get(n).cloned().unwrap_or(n.to_owned()))
                        .and_then(|e| py::translate(&e, &|_| true))
                    {
                        Ok(t) => Json::String(t),
                        Err(_) => v.clone(),
                    }
                }
                v => v.clone(),
            };
            (k.clone(), v)
        })
        .collect();
    let original = json!({
        "kind": b.kind,
        "params": params,
        "inputs": ins.iter().map(|p| &p.0).collect::<Vec<_>>(),
        "outputs": outs.iter().map(|p| &p.0).collect::<Vec<_>>(),
    });
    let sig = |ports: &[(String, &'static str)]| {
        if ports.is_empty() {
            "None".to_owned()
        } else {
            format!(
                "[{}]",
                ports
                    .iter()
                    .map(|p| numpy(p.1))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    let label = format!("foxygnal {} (placeholder)", b.kind);
    let body = if ins.is_empty() {
        "        time.sleep(0.01)\n        return 0\n"
    } else {
        "        for i, items in enumerate(input_items):\n            \
         self.consume(i, len(items))\n        return 0\n"
    };
    let src = format!(
        "\"\"\"Placeholder for the foxygnal block `{id}` ({kind}): {why}.\n\
         \n\
         It has the same ports, consumes its input and produces nothing.\n\
         Replace it with a GNU Radio implementation. foxygnal's importer turns\n\
         it back into the original block (the line below).\n\
         \"\"\"\n\
         {MARKER}{original}\n\
         \n\
         import time\n\
         \n\
         import numpy\n\
         from gnuradio import gr\n\
         \n\
         \n\
         class blk(gr.basic_block):\n    \
             \"\"\"foxygnal {kind} `{id}`: no GNU Radio equivalent (placeholder)\"\"\"\n\
         \n    \
             def __init__(self):\n        \
                 gr.basic_block.__init__(\n            \
                     self,\n            \
                     name={label:?},\n            \
                     in_sig={ins_sig},\n            \
                     out_sig={outs_sig},\n        \
                 )\n\
         \n    \
             def general_work(self, input_items, output_items):\n\
         {body}",
        id = b.id,
        kind = b.kind,
        ins_sig = sig(ins),
        outs_sig = sig(outs),
    );
    warnings.push(format!(
        "`{}` ({}): {why}; exported as a placeholder Python block with the same ports that \
         passes nothing (the importer restores the original)",
        b.id, b.kind
    ));
    let g = GBlock::new("epy_block")
        .p("_source_code", src)
        .p("comment", format!("foxygnal {} placeholder: {why}", b.kind));
    let mut o = Out::default();
    o.add(g, "");
    for (k, (p, t)) in ins.iter().enumerate() {
        o.input(p, 0, k, t);
    }
    for (k, (p, t)) in outs.iter().enumerate() {
        o.output(p, 0, k, t);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_codes() {
        assert_eq!(
            access_code_bits("0x1ACFFC1D").as_deref(),
            Some("00011010110011111111110000011101")
        );
        assert_eq!(access_code_bits("0x0F").as_deref(), Some("00001111"));
        assert_eq!(access_code_bits("1011").as_deref(), Some("1011"));
        assert_eq!(access_code_bits("hello"), None);
    }

    #[test]
    fn ratios() {
        assert_eq!(ratio(48000.0, 240000.0), (1, 5));
        assert_eq!(ratio(48000.0, 250000.0), (24, 125));
        assert_eq!(ratio(44100.0, 48000.0), (147, 160));
    }

    #[test]
    fn sigmf_names() {
        assert_eq!(sigmf_data("rec"), "rec.sigmf-data");
        assert_eq!(sigmf_data("rec.sigmf-meta"), "rec.sigmf-data");
        assert_eq!(sigmf_data("a/rec.sigmf-data"), "a/rec.sigmf-data");
    }
}
