//! The mapping table: GNU Radio block types to foxygnal blocks, with their
//! parameters translated. `docs/GRC_IMPORT.md` lists the same table.

use std::collections::BTreeSet;
use std::fmt::Display;

use foxygnal_core::Params;
use foxygnal_core::graph::{BlockSpec, ConnectionSpec};
use serde_json::{Value as Json, json};

use crate::py::{self, BinOp, Py, Value};
use crate::{GrcBlock, Vars, node};

pub(crate) struct Config {
    pub audio_device: bool,
    /// The flowgraph has a hardware source, which sets the pace.
    pub has_hardware: bool,
}

pub(crate) enum Mapped {
    Nodes(Out),
    /// Nothing to convert (a comment, an import line).
    Skip,
    /// No mapping for this block type.
    Unmapped,
}

/// What a GRC block became: foxygnal nodes, the wires between them, and
/// which node port each GRC port index stands for.
#[derive(Default)]
pub(crate) struct Out {
    pub nodes: Vec<BlockSpec>,
    /// Canvas offset of each node from the GRC block's position.
    pub offsets: Vec<(f64, f64)>,
    pub links: Vec<ConnectionSpec>,
    /// GRC input port index → the node inputs it feeds.
    pub inputs: Vec<Vec<String>>,
    /// GRC output port index → the node output it is.
    pub outputs: Vec<Option<String>>,
}

impl Out {
    /// Add a node; the first gets the GRC block's name, the others
    /// `name_suffix`. Returns its id.
    fn add(&mut self, base: &str, suffix: &str, kind: &str, params: Params) -> String {
        let id = if self.nodes.is_empty() || suffix.is_empty() {
            base.to_owned()
        } else {
            format!("{base}_{suffix}")
        };
        let k = self.nodes.len() as f64;
        self.offsets.push((k * 240.0, 0.0));
        self.nodes.push(node(id.clone(), kind, params));
        id
    }

    fn input(&mut self, port: usize, endpoint: String) {
        if self.inputs.len() <= port {
            self.inputs.resize(port + 1, Vec::new());
        }
        self.inputs[port].push(endpoint);
    }

    fn output(&mut self, port: usize, endpoint: String) {
        if self.outputs.len() <= port {
            self.outputs.resize(port + 1, None);
        }
        self.outputs[port] = Some(endpoint);
    }

    fn link(&mut self, from: String, to: String) {
        self.links.push(ConnectionSpec { from, to });
    }

    /// One node with input `in` (if `has_in`) and output `out` (if `has_out`).
    fn single(name: &str, kind: &str, params: Params, has_in: bool, has_out: bool) -> Out {
        let mut o = Out::default();
        let id = o.add(name, "", kind, params);
        if has_in {
            o.input(0, format!("{id}.in"));
        }
        if has_out {
            o.output(0, format!("{id}.out"));
        }
        o
    }

    /// A chain of 1-in/1-out nodes: GRC input 0 feeds the first, output 0
    /// is the last.
    fn chain(name: &str, nodes: Vec<(&str, &str, Params)>) -> Out {
        let mut o = Out::default();
        let mut prev: Option<String> = None;
        for (suffix, kind, params) in nodes {
            let id = o.add(name, suffix, kind, params);
            match prev {
                None => o.input(0, format!("{id}.in")),
                Some(p) => o.link(format!("{p}.out"), format!("{id}.in")),
            }
            prev = Some(id);
        }
        if let Some(p) = prev {
            o.output(0, format!("{p}.out"));
        }
        o
    }
}

/// The block being mapped, with helpers that read and translate its
/// parameters and record warnings about it.
pub(crate) struct Cx<'a> {
    pub b: &'a GrcBlock,
    vars: &'a Vars,
    warnings: &'a mut Vec<String>,
    /// Output ports something is connected to.
    pub used_outputs: BTreeSet<usize>,
    /// Input ports something is connected to.
    pub used_inputs: BTreeSet<usize>,
}

impl<'a> Cx<'a> {
    pub fn new(
        b: &'a GrcBlock,
        vars: &'a Vars,
        warnings: &'a mut Vec<String>,
        used_inputs: BTreeSet<usize>,
        used_outputs: BTreeSet<usize>,
    ) -> Self {
        Cx {
            b,
            vars,
            warnings,
            used_outputs,
            used_inputs,
        }
    }

    fn name(&self) -> &'a str {
        &self.b.name
    }

    pub fn warn(&mut self, msg: impl Display) {
        self.warnings
            .push(format!("`{}` ({}): {msg}", self.b.name, self.b.id));
    }

    /// The parameter's text, if set and not empty.
    fn raw(&self, key: &str) -> Option<&'a str> {
        self.b
            .params
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    /// The first of `keys` that is set.
    fn first(&self, keys: &[&'static str]) -> Option<&'static str> {
        keys.iter().copied().find(|k| self.raw(k).is_some())
    }

    fn py(&mut self, key: &str) -> Option<Py> {
        let src = self.raw(key)?;
        match Py::parse(src) {
            Ok(e) => Some(e),
            Err(e) => {
                self.warn(format!(
                    "parameter `{key}` = `{src}` isn't understood ({e}); left at the default"
                ));
                None
            }
        }
    }

    fn value_of(&mut self, e: &Py, what: &str) -> Option<Value> {
        match self.vars.eval(e) {
            Ok(v) => Some(v),
            Err(err) => {
                self.warn(format!(
                    "{what} = `{}` can't be evaluated ({err}); left at the default",
                    py::show(e)
                ));
                None
            }
        }
    }

    fn value(&mut self, key: &str) -> Option<Value> {
        let e = self.py(key)?;
        self.value_of(&e, &format!("parameter `{key}`"))
    }

    /// A number as a foxygnal param: a constant, or an expression over
    /// graph variables.
    fn num_of(&mut self, e: &Py, what: &str, int: bool) -> Option<Json> {
        match self.vars.number(e, int) {
            Ok((v, note)) => {
                if let Some(n) = note {
                    self.warn(format!("{what}: {n}"));
                }
                Some(v)
            }
            Err(err) => {
                self.warn(format!("{what}: {err}; left at the default"));
                None
            }
        }
    }

    fn num(&mut self, key: &str) -> Option<Json> {
        let e = self.py(key)?;
        self.num_of(&e, &format!("parameter `{key}`"), false)
    }

    fn int(&mut self, key: &str) -> Option<Json> {
        let e = self.py(key)?;
        self.num_of(&e, &format!("parameter `{key}`"), true)
    }

    /// The parameter's numeric value.
    fn f64(&mut self, key: &str) -> Option<f64> {
        let e = self.py(key)?;
        self.vars.eval(&e).ok()?.as_f64()
    }

    /// A string parameter, as GRC reads one: a Python expression when it
    /// evaluates to a string (`"rtl=" + str(0)`, a variable), else the text
    /// as written (`/tmp/out.wav`, `rtl=0`, `127.0.0.1`).
    fn string(&mut self, key: &str) -> Option<String> {
        let raw = self.raw(key)?;
        let value = Py::parse(raw).ok().and_then(|e| self.vars.eval(&e).ok());
        Some(match value {
            Some(Value::Str(s)) => s,
            Some(Value::Num(n)) => py::fmt_num(n),
            _ => raw.to_owned(),
        })
    }

    fn boolean(&mut self, key: &str) -> Option<bool> {
        Some(self.value(key)?.truthy())
    }

    /// Follow names of non-numeric variables (`taps`) to their expression.
    fn deref(&self, mut e: Py) -> Py {
        for _ in 0..16 {
            match &e {
                Py::Name(n) if !self.vars.is_numeric(n) => match self.vars.ast(n) {
                    Some(a) => e = a.clone(),
                    None => break,
                },
                _ => break,
            }
        }
        e
    }

    /// Replace every variable name by its expression (to pattern-match).
    fn expand(&self, e: &Py, depth: usize) -> Py {
        if depth > 16 {
            return e.clone();
        }
        let ex = |x: &Py| Box::new(self.expand(x, depth + 1));
        match e {
            Py::Name(n) => match self.vars.ast(n) {
                Some(a) => self.expand(a, depth + 1),
                None => e.clone(),
            },
            Py::Neg(x) => Py::Neg(ex(x)),
            Py::Bin(op, a, b) => Py::Bin(*op, ex(a), ex(b)),
            _ => e.clone(),
        }
    }

    /// The stream type from GNU Radio's `type` parameter, if it is one of
    /// `allowed`.
    fn stream_type(&mut self, allowed: &[&'static str]) -> Result<&'static str, String> {
        let raw = self.raw("type").unwrap_or("complex");
        let ty = match raw {
            "complex" | "fc32" | "c" | "cc" | "ccc" | "ccf" => "c32",
            "float" | "f32" | "f" | "ff" | "fff" => "f32",
            "short" | "sc16" | "s" | "ss" => "i16",
            "byte" | "char" | "b" | "bb" | "sc8" => "u8",
            other => return Err(format!("stream type `{other}` isn't supported")),
        };
        if allowed.contains(&ty) {
            Ok(ty)
        } else {
            Err(format!(
                "stream type `{raw}` isn't supported by the foxygnal block (it takes {})",
                allowed.join(" or ")
            ))
        }
    }
}

fn set(p: &mut Params, key: &str, v: Option<Json>) {
    if let Some(v) = v {
        p.insert(key.to_owned(), v);
    }
}

fn params(kv: &[(&str, Json)]) -> Params {
    kv.iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

const ANY: &[&str] = &["c32", "f32", "i16", "u8"];
const CF: &[&str] = &["c32", "f32"];

pub(crate) fn is_hardware_source(id: &str) -> bool {
    matches!(
        id,
        "osmosdr_source" | "rtlsdr_source" | "uhd_usrp_source" | "iio_pluto_source"
    ) || (id.starts_with("soapy_") && id.ends_with("_source"))
}

/// Map one enabled, non-variable GRC block.
pub(crate) fn map(cx: &mut Cx<'_>, cfg: &Config) -> Mapped {
    // Vectors (vlen > 1) have no foxygnal equivalent.
    if let Some(vlen) = cx.raw("vlen").and_then(|_| cx.f64("vlen"))
        && vlen != 1.0
    {
        cx.warn(format!(
            "vector streams (vlen {}) aren't supported; left out, with its connections",
            py::fmt_num(vlen)
        ));
        return Mapped::Skip;
    }
    match try_map(cx, cfg) {
        Ok(m) => m,
        Err(why) => {
            cx.warn(format!("{why}; left out, with its connections"));
            Mapped::Skip
        }
    }
}

fn try_map(cx: &mut Cx<'_>, cfg: &Config) -> Result<Mapped, String> {
    let name = cx.name();
    let id = cx.b.id.clone();
    let out = match id.as_str() {
        // Comments, imports and GUI layout (tabs): nothing to convert.
        "note" | "import" | "import_" | "snippet" | "notebook" | "qtgui_tab_widget" => {
            return Ok(Mapped::Skip);
        }

        // ---- Sources ----
        "analog_sig_source_x" => {
            let ty = cx.stream_type(CF)?;
            let wave = cx.raw("waveform").unwrap_or("analog.GR_COS_WAVE");
            let waveform = if wave.contains("COS") || wave.contains("SIN") {
                "sine"
            } else if wave.contains("SQR") {
                "square"
            } else if wave.contains("TRI") {
                "triangle"
            } else if wave.contains("SAW") {
                "sawtooth"
            } else if wave.contains("CONST") {
                "constant"
            } else {
                cx.warn(format!("waveform `{wave}` isn't supported; using a sine"));
                "sine"
            };
            let mut p = params(&[("type", json!(ty)), ("waveform", json!(waveform))]);
            set(&mut p, "frequency", cx.num("freq"));
            set(&mut p, "sample_rate", cx.num("samp_rate"));
            let phase = cx.f64("phase").unwrap_or(0.0);
            let shifted = matches!(waveform, "square" | "triangle" | "sawtooth")
                && (phase - std::f64::consts::PI).abs() < 1e-9;
            if shifted {
                // GNU Radio's 0..amp wave, half a period on (as `foxygnal
                // export` writes it), is foxygnal's −amp/2..amp/2 one.
                let (amp, off) = (
                    cx.f64("amp").unwrap_or(1.0),
                    cx.f64("offset").unwrap_or(0.0),
                );
                p.insert("amplitude".into(), json!(amp / 2.0));
                p.insert("offset".into(), json!(off + amp / 2.0));
            } else {
                set(&mut p, "amplitude", cx.num("amp"));
                set(&mut p, "offset", cx.num("offset"));
                if waveform != "sine" && waveform != "constant" {
                    cx.warn(
                        "GNU Radio's wave spans 0..amplitude; foxygnal's spans −amplitude..\
                         amplitude: check amplitude and offset",
                    );
                }
                if phase != 0.0 {
                    cx.warn("the start phase isn't supported; it starts at 0");
                }
            }
            Out::single(name, "signal_source", p, false, true)
        }
        "analog_noise_source_x" | "analog_fastnoise_source_x" => {
            let ty = cx.stream_type(CF)?;
            let kind = cx.raw("noise_type").unwrap_or("analog.GR_GAUSSIAN");
            if !kind.contains("GAUSSIAN") {
                cx.warn(format!(
                    "noise type `{kind}` isn't supported; using Gaussian"
                ));
            }
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "amplitude", cx.num("amp"));
            if let Some(Json::Number(n)) = cx.int("seed")
                && n.as_i64().is_some_and(|s| s >= 0)
            {
                p.insert("seed".into(), Json::Number(n));
            }
            Out::single(name, "noise_source", p, false, true)
        }
        "blocks_file_source" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "path", cx.string("file").map(Json::from));
            set(&mut p, "repeat", cx.boolean("repeat").map(Json::from));
            if cx.f64("offset").is_some_and(|v| v != 0.0)
                || cx.f64("length").is_some_and(|v| v != 0.0)
            {
                cx.warn("offset / length aren't supported; the whole file is read");
            }
            Out::single(name, "file_source", p, false, true)
        }
        "blocks_wavfile_source" => {
            let nchan = cx.f64("nchan").unwrap_or(1.0).max(1.0) as usize;
            let path = cx.string("file").map(Json::from);
            let repeat = cx.boolean("repeat").map(Json::from);
            let mut o = Out::default();
            let mut used: Vec<usize> = cx
                .used_outputs
                .iter()
                .copied()
                .filter(|&c| c < nchan)
                .collect();
            if used.is_empty() {
                used.push(0);
            }
            for ch in used {
                let mut p = params(&[("type", json!("f32")), ("channel", json!(ch))]);
                set(&mut p, "path", path.clone());
                set(&mut p, "repeat", repeat.clone());
                let id = o.add(name, &format!("ch{ch}"), "wav_source", p);
                o.output(ch, format!("{id}.out"));
            }
            o
        }

        // ---- Flow ----
        "blocks_throttle" | "blocks_throttle2" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "sample_rate", cx.num("samples_per_second"));
            Out::single(name, "throttle", p, true, true)
        }
        "blocks_head" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "count", cx.int("num_items"));
            Out::single(name, "head", p, true, true)
        }

        // ---- Sinks ----
        "blocks_file_sink" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "path", cx.string("file").map(Json::from));
            if cx.boolean("append") == Some(true) {
                cx.warn("append isn't supported; the file is overwritten");
            }
            Out::single(name, "file_sink", p, true, false)
        }
        "blocks_wavfile_sink" => {
            let nchan = cx.f64("nchan").unwrap_or(1.0) as usize;
            // GNU Radio 3.10 has one `bits_per_sample<N>` per file format.
            let bits = cx
                .raw("bits_per_sample")
                .or_else(|| cx.raw("bits_per_sample1"))
                .unwrap_or("16");
            let fmt = cx.raw("format").unwrap_or("");
            let format = if bits.contains("FLOAT") || bits.contains("DOUBLE") {
                "float32"
            } else {
                if !(bits == "16" || bits.contains("PCM_16")) {
                    cx.warn(format!(
                        "sample format `{bits}` isn't supported; using 16-bit PCM"
                    ));
                }
                "pcm16"
            };
            if !(fmt.is_empty() || fmt.contains("WAV")) {
                cx.warn(format!("file format `{fmt}` isn't supported; writing WAV"));
            }
            let path = cx.string("file").map(Json::from);
            match nchan {
                0 | 1 => {
                    let mut p = params(&[("type", json!("f32")), ("format", json!(format))]);
                    set(&mut p, "path", path);
                    Out::single(name, "wav_sink", p, true, false)
                }
                2 => {
                    // Stereo: the WAV sink writes a c32 stream as left = I,
                    // right = Q.
                    let mut o = Out::default();
                    let mut p = params(&[("type", json!("c32")), ("format", json!(format))]);
                    set(&mut p, "path", path);
                    let sink = o.add(name, "", "wav_sink", p);
                    let pair = o.add(name, "lr", "f32_pair_to_c32", Params::new());
                    o.offsets[1] = (-240.0, 0.0);
                    o.input(0, format!("{pair}.re"));
                    o.input(1, format!("{pair}.im"));
                    o.link(format!("{pair}.out"), format!("{sink}.in"));
                    o
                }
                n => return Err(format!("{n} channels aren't supported (1 or 2)")),
            }
        }
        "blocks_null_sink" => {
            let ty = cx.stream_type(ANY)?;
            let n = cx.f64("num_inputs").unwrap_or(1.0).max(1.0) as usize;
            let mut o = Out::default();
            for i in 0..n {
                let id = o.add(
                    name,
                    &i.to_string(),
                    "null_sink",
                    params(&[("type", json!(ty))]),
                );
                o.offsets[i] = (0.0, i as f64 * 90.0);
                o.input(i, format!("{id}.in"));
            }
            o
        }
        "audio_sink" if !cfg.audio_device && cx.f64("num_inputs") == Some(2.0) => {
            // Stereo: Browser Audio plays a c32 stream as left = I, right = Q.
            let mut o = Out::default();
            let p = params(&[("type", json!("c32")), ("pace", json!(!cfg.has_hardware))]);
            let sink = o.add(name, "", "browser_audio", p);
            let pair = o.add(name, "stereo", "f32_pair_to_c32", Params::new());
            o.offsets[1] = (-240.0, 0.0);
            o.input(0, format!("{pair}.re"));
            o.input(1, format!("{pair}.im"));
            o.link(format!("{pair}.out"), format!("{sink}.in"));
            o
        }
        "audio_sink" => {
            let n = cx.f64("num_inputs").unwrap_or(1.0).max(1.0) as usize;
            let mut o = if cfg.audio_device {
                let mut p = Params::new();
                set(&mut p, "device", cx.string("device_name").map(Json::from));
                Out::single(name, "audio_sink", p, true, false)
            } else {
                // Hardware upstream sets the pace; otherwise the sink does.
                Out::single(
                    name,
                    "browser_audio",
                    params(&[("pace", json!(!cfg.has_hardware))]),
                    true,
                    false,
                )
            };
            if n > 1 {
                cx.warn(format!(
                    "{n} channels: only the first is played, the others go to null sinks"
                ));
                for i in 1..n {
                    let id = o.add(
                        name,
                        &format!("ch{i}"),
                        "null_sink",
                        params(&[("type", json!("f32"))]),
                    );
                    o.offsets[i] = (0.0, i as f64 * 90.0);
                    o.input(i, format!("{id}.in"));
                }
            }
            o
        }

        // ---- Math ----
        "blocks_add_xx"
        | "blocks_add_vxx"
        | "blocks_multiply_xx"
        | "blocks_multiply_vxx"
        | "blocks_sub_xx"
        | "blocks_divide_xx" => {
            let ty = cx.stream_type(CF)?;
            // in0 − in1 − in2 … and in0 / in1 / in2 … chain the same way.
            let kind = match id.as_str() {
                "blocks_sub_xx" => "subtract",
                "blocks_divide_xx" => "divide",
                _ if id.starts_with("blocks_add") => "add",
                _ => "multiply",
            };
            let n = cx.f64("num_inputs").unwrap_or(2.0).max(1.0) as usize;
            let mut o = Out::default();
            if n == 1 {
                let id = o.add(name, "", "gain", params(&[("type", json!(ty))]));
                o.input(0, format!("{id}.in"));
                o.output(0, format!("{id}.out"));
                return Ok(Mapped::Nodes(o));
            }
            // n inputs: a chain of n - 1 two-input blocks.
            let mut prev: Option<String> = None;
            for k in 0..n - 1 {
                let id = o.add(name, &k.to_string(), kind, params(&[("type", json!(ty))]));
                match &prev {
                    None => o.input(0, format!("{id}.a")),
                    Some(p) => o.link(format!("{p}.out"), format!("{id}.a")),
                }
                o.input(k + 1, format!("{id}.b"));
                prev = Some(id);
            }
            o.output(0, format!("{}.out", prev.expect("n >= 2")));
            o
        }
        "blocks_multiply_const_xx"
        | "blocks_multiply_const_vxx"
        | "blocks_multiply_const_vff"
        | "blocks_multiply_const_vcc"
        | "blocks_multiply_const_ff"
        | "blocks_multiply_const_cc" => {
            let ty = if id.ends_with("ff") {
                "f32"
            } else if id.ends_with("cc") {
                "c32"
            } else {
                cx.stream_type(CF)?
            };
            let mut p = params(&[("type", json!(ty))]);
            let g = cx.py("const").ok_or("the constant isn't understood")?;
            set(&mut p, "gain", cx.num_of(&g, "parameter `const`", false));
            Out::single(name, "gain", p, true, true)
        }

        // ---- Converters ----
        "blocks_complex_to_mag"
        | "blocks_complex_to_mag_squared"
        | "blocks_complex_to_real"
        | "blocks_complex_to_imag"
        | "blocks_complex_to_arg" => {
            let part = match id.as_str() {
                "blocks_complex_to_mag" => "magnitude",
                "blocks_complex_to_mag_squared" => "magnitude_squared",
                "blocks_complex_to_real" => "real",
                "blocks_complex_to_imag" => "imag",
                _ => "phase",
            };
            Out::single(
                name,
                "c32_to_f32",
                params(&[("part", json!(part))]),
                true,
                true,
            )
        }
        "blocks_complex_to_float" => {
            let mut o = Out::default();
            let wanted: Vec<usize> = if cx.used_outputs.is_empty() {
                vec![0]
            } else {
                cx.used_outputs.iter().copied().filter(|&p| p < 2).collect()
            };
            for port in wanted {
                let (suffix, part) = if port == 0 {
                    ("re", "real")
                } else {
                    ("im", "imag")
                };
                let id = o.add(name, suffix, "c32_to_f32", params(&[("part", json!(part))]));
                let k = o.offsets.len() - 1;
                o.offsets[k] = (0.0, port as f64 * 90.0);
                o.input(0, format!("{id}.in"));
                o.output(port, format!("{id}.out"));
            }
            o
        }
        "blocks_float_to_complex" => {
            let mut o = Out::default();
            if cx.used_inputs.contains(&1) {
                let id = o.add(name, "", "f32_pair_to_c32", Params::new());
                o.input(0, format!("{id}.re"));
                o.input(1, format!("{id}.im"));
                o.output(0, format!("{id}.out"));
            } else {
                let id = o.add(name, "", "f32_to_c32", Params::new());
                o.input(0, format!("{id}.in"));
                o.output(0, format!("{id}.out"));
            }
            o
        }

        // ---- Filters ----
        "low_pass_filter" | "high_pass_filter" | "band_pass_filter" | "band_reject_filter" => {
            fir(cx, &id)?
        }
        "freq_xlating_fir_filter_xxx"
        | "freq_xlating_fir_filter_ccc"
        | "freq_xlating_fir_filter_ccf"
        | "freq_xlating_fir_filter_fcc"
        | "freq_xlating_fir_filter_fcf" => xlating(cx, &id)?,
        "rational_resampler_xxx" | "rational_resampler_base_xxx" => {
            let raw = cx.raw("type").unwrap_or("ccc");
            let ty = match raw.chars().next() {
                Some('c') if raw.chars().nth(1) == Some('c') => "c32",
                Some('f') if raw.chars().nth(1) == Some('f') => "f32",
                _ => return Err(format!("resampler type `{raw}` isn't supported")),
            };
            let mut p = params(&[("type", json!(ty)), ("output_rate", json!(0.0))]);
            set(&mut p, "interpolation", cx.int("interp"));
            set(&mut p, "decimation", cx.int("decim"));
            if cx.raw("taps").is_some_and(|t| t != "[]" && t != "None") {
                cx.warn("custom taps aren't supported; the resampler designs its own low-pass");
            }
            Out::single(name, "rational_resampler", p, true, true)
        }

        // ---- Demodulators ----
        "analog_quadrature_demod_cf" => {
            let mut p = params(&[("deemphasis", json!("off"))]);
            set(&mut p, "deviation", deviation_from_gain(cx));
            Out::single(name, "fm_demod", p, true, true)
        }
        "analog_wfm_rcv" | "analog_wfm_rcv_pll" => {
            let mut p = params(&[("deviation", json!(75000.0))]);
            let tau = cx
                .raw("deemph_tau")
                .map(|_| cx.f64("deemph_tau"))
                .unwrap_or(Some(75e-6));
            p.insert("deemphasis".into(), json!(deemphasis(cx, tau)));
            if id == "analog_wfm_rcv_pll" {
                cx.warn("stereo isn't decoded: both channels get the mono audio");
            }
            let decim = cx.int("audio_decimation");
            let decim_one = cx.f64("audio_decimation").is_none_or(|d| d == 1.0);
            let mut nodes = vec![("", "fm_demod", p)];
            if !decim_one {
                let mut d = params(&[("type", json!("f32"))]);
                set(&mut d, "decimation", decim);
                nodes.push(("audio", "decimator", d));
            }
            let mut o = Out::chain(name, nodes);
            if id == "analog_wfm_rcv_pll" {
                let last = o.outputs[0].clone();
                o.outputs.push(last);
            }
            o
        }
        "analog_nbfm_rx" => {
            let mut p = Params::new();
            set(
                &mut p,
                "deviation",
                cx.num("max_dev").or(Some(json!(5000.0))),
            );
            let tau = cx.raw("tau").map(|_| cx.f64("tau")).unwrap_or(Some(75e-6));
            p.insert("deemphasis".into(), json!(deemphasis(cx, tau)));
            let mut r = params(&[("type", json!("f32"))]);
            set(&mut r, "output_rate", cx.num("audio_rate"));
            Out::chain(
                name,
                vec![("", "fm_demod", p), ("audio", "rational_resampler", r)],
            )
        }

        // ---- Level ----
        "analog_agc_xx" | "analog_agc2_xx" | "analog_agc3_xx" => {
            let ty = cx.stream_type(CF)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "target", cx.num("reference"));
            if cx.f64("max_gain").is_some_and(|g| g > 0.0) {
                set(&mut p, "max_gain", cx.num("max_gain"));
            }
            cx.warn(
                "GNU Radio's per-sample adaptation rates have no exact equivalent: attack and \
                 decay are left at foxygnal's defaults (seconds)",
            );
            Out::single(name, "agc", p, true, true)
        }
        "analog_pwr_squelch_xx" | "analog_pwr_squelch_cc" | "analog_pwr_squelch_ff" => {
            let ty = if id.ends_with("_cc") {
                "c32"
            } else if id.ends_with("_ff") {
                "f32"
            } else {
                cx.stream_type(CF)?
            };
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "threshold", cx.num("threshold"));
            if cx.boolean("gate") == Some(true) {
                cx.warn("gate isn't supported: the squelch outputs zeros while closed");
            }
            Out::single(name, "squelch", p, true, true)
        }

        "blocks_add_const_vxx"
        | "blocks_add_const_xx"
        | "blocks_add_const_ff"
        | "blocks_add_const_cc" => {
            let ty = if id.ends_with("ff") {
                "f32"
            } else if id.ends_with("cc") {
                "c32"
            } else {
                cx.stream_type(CF)?
            };
            let mut p = params(&[("type", json!(ty))]);
            let c = cx.py("const").ok_or("the constant isn't understood")?;
            set(
                &mut p,
                "constant",
                cx.num_of(&c, "parameter `const`", false),
            );
            Out::single(name, "add_const", p, true, true)
        }
        "blocks_abs_xx" | "blocks_abs_ff" => {
            if id == "blocks_abs_xx" {
                cx.stream_type(&["f32"])?;
            }
            Out::single(name, "abs", Params::new(), true, true)
        }
        "blocks_conjugate_cc" => Out::single(name, "conjugate", Params::new(), true, true),
        "blocks_nlog10_ff" => {
            // n·log10(x) + k. Log Power computes 10·log10(x²) + offset, the
            // same as 20·log10(x) for x > 0: scale by n / 20 after it.
            let n = cx.f64("n").unwrap_or(1.0);
            let k = cx.f64("k").unwrap_or(0.0);
            if n == 0.0 {
                return Err("n = 0 has no foxygnal equivalent".into());
            }
            let p = params(&[("type", json!("f32")), ("offset", json!(k * 20.0 / n))]);
            if n == 20.0 {
                Out::single(name, "log_power", p, true, true)
            } else {
                let g = params(&[("type", json!("f32")), ("gain", json!(n / 20.0))]);
                Out::chain(name, vec![("", "log_power", p), ("scale", "gain", g)])
            }
        }

        // ---- Flow (M7 catalog) ----
        "blocks_skiphead" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "count", cx.int("num_items"));
            Out::single(name, "skip", p, true, true)
        }
        "blocks_delay" => {
            let ty = cx.stream_type(ANY)?;
            if cx.f64("num_ports").is_some_and(|n| n != 1.0) {
                return Err("only one port is supported".into());
            }
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "delay", cx.int("delay"));
            Out::single(name, "delay", p, true, true)
        }
        "blocks_copy" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty)), ("when_closed", json!("drop"))]);
            set(&mut p, "open", cx.boolean("enabled").map(Json::from));
            Out::single(name, "valve", p, true, true)
        }
        "blocks_selector" => {
            let ty = cx.stream_type(ANY)?;
            let n_in = cx.f64("num_inputs").unwrap_or(2.0) as usize;
            if cx.f64("num_outputs").is_some_and(|n| n != 1.0) {
                return Err("only one output is supported".into());
            }
            if !(2..=4).contains(&n_in) {
                return Err(format!("{n_in} inputs aren't supported (2 to 4)"));
            }
            if cx.boolean("enabled") == Some(false) {
                cx.warn("`enabled` = False isn't supported: the selector always passes");
            }
            let mut p = params(&[("type", json!(ty)), ("inputs", json!(n_in))]);
            set(&mut p, "input", cx.int("input_index"));
            let mut o = Out::default();
            let id = o.add(name, "", "selector", p);
            for i in 0..n_in {
                o.input(i, format!("{id}.in{i}"));
            }
            o.output(0, format!("{id}.out"));
            o
        }

        // ---- Filters (M7 catalog) ----
        "root_raised_cosine_filter" => {
            let raw = cx.raw("type").unwrap_or("fir_filter_ccf");
            let ty = filter_type(raw)?;
            let interp = if raw.starts_with("interp_") {
                cx.f64("interp").unwrap_or(1.0)
            } else {
                1.0
            };
            let (Some(fs), Some(sym)) = (cx.py("samp_rate"), cx.py("sym_rate")) else {
                return Err("the sample and symbol rates aren't understood".into());
            };
            let a = RrcArgs {
                gain: cx.py("gain"),
                fs,
                sym,
                alpha: cx.py("alpha"),
                ntaps: cx.py("ntaps"),
            };
            rrc(cx, ty, a, interp)?
        }
        "fir_filter_xxx" | "interp_fir_filter_xxx" => fir_taps(cx, &id)?,
        "filter_dc_blocker_xx" | "dc_blocker_xx" | "dc_blocker_cc" | "dc_blocker_ff" => {
            let ty = match id.as_str() {
                "dc_blocker_cc" => "c32",
                "dc_blocker_ff" => "f32",
                _ => cx.stream_type(CF)?,
            };
            let len = cx.f64("length").map_or("32".into(), py::fmt_num);
            cx.warn(format!(
                "GNU Radio's DC blocker is a {len}-sample moving average; foxygnal's takes a \
                 cutoff in Hz, left at its default (10 Hz)"
            ));
            Out::single(
                name,
                "dc_blocker",
                params(&[("type", json!(ty))]),
                true,
                true,
            )
        }
        "hilbert_fc" => {
            let mut p = Params::new();
            set(&mut p, "taps", cx.int("num_taps"));
            let win = cx.py("win");
            let w = window(cx, win.as_ref());
            p.insert("window".into(), json!(w));
            if w == "kaiser"
                && let Some(beta) = cx.f64("beta")
            {
                p.insert(
                    "attenuation".into(),
                    json!((kaiser_attenuation(beta) * 10.0).round() / 10.0),
                );
            }
            Out::single(name, "hilbert", p, true, true)
        }
        "iir_filter_xxx" | "iir_filter_ffd" | "iir_filter_ccc" | "iir_filter_ccf"
        | "iir_filter_ccd" | "iir_filter_ccz" => {
            return Err(
                "arbitrary IIR taps have no foxygnal equivalent (the IIR Biquad block designs \
                 its own from a shape, frequency and Q)"
                    .into(),
            );
        }

        // ---- Network ----
        "network_udp_source" | "blocks_udp_source" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            set(&mut p, "port", cx.int("port"));
            if id == "blocks_udp_source" {
                set(&mut p, "host", cx.string("ipaddr").map(Json::from));
            } else {
                set(&mut p, "header", udp_header(cx));
                cx.warn(
                    "GNU Radio listens on every interface; foxygnal's UDP Source listens on \
                     127.0.0.1 unless you set its host (0.0.0.0 exposes it to the network)",
                );
            }
            Out::single(name, "udp_source", p, false, true)
        }
        "network_udp_sink" | "blocks_udp_sink" => {
            let ty = cx.stream_type(ANY)?;
            let mut p = params(&[("type", json!(ty))]);
            let (host, size) = if id == "blocks_udp_sink" {
                ("ipaddr", "psize")
            } else {
                ("addr", "payloadsize")
            };
            set(&mut p, "host", cx.string(host).map(Json::from));
            set(&mut p, "port", cx.int("port"));
            let header = if id == "network_udp_sink" {
                udp_header(cx)
            } else {
                None
            };
            // GNU Radio's size excludes the header; foxygnal's includes it.
            let extra = match header.as_ref().and_then(Json::as_str) {
                Some("seq64") => 8.0,
                Some("seq32") => 4.0,
                _ => 0.0,
            };
            if let Some(n) = cx.f64(size) {
                p.insert(
                    "payload".into(),
                    json!(((n + extra).round() as i64).clamp(16, 65507)),
                );
            }
            set(&mut p, "header", header);
            Out::single(name, "udp_sink", p, true, false)
        }
        "network_tcp_source" => {
            let ty = cx.stream_type(ANY)?;
            let server = cx.boolean("server").unwrap_or(false);
            let mut p = params(&[
                ("type", json!(ty)),
                ("mode", json!(if server { "server" } else { "client" })),
            ]);
            set(&mut p, "host", cx.string("addr").map(Json::from));
            set(&mut p, "port", cx.int("port"));
            Out::single(name, "tcp_source", p, false, true)
        }
        "network_tcp_sink" | "blocks_tcp_server_sink" => {
            let ty = cx.stream_type(ANY)?;
            let server = if id == "blocks_tcp_server_sink" {
                true
            } else {
                // gr-network: 1 = client, 2 = server.
                match cx.raw("mode").unwrap_or("2") {
                    "1" => false,
                    "2" => true,
                    m => {
                        let s = m.to_ascii_lowercase();
                        if !s.contains("server") && !s.contains("client") {
                            cx.warn(format!("mode `{m}` isn't understood; using server"));
                        }
                        !s.contains("client")
                    }
                }
            };
            let mut p = params(&[
                ("type", json!(ty)),
                ("mode", json!(if server { "server" } else { "client" })),
            ]);
            let host = if id == "blocks_tcp_server_sink" {
                "ipaddr"
            } else {
                "addr"
            };
            set(&mut p, "host", cx.string(host).map(Json::from));
            set(&mut p, "port", cx.int("port"));
            Out::single(name, "tcp_sink", p, true, false)
        }

        // ---- Converters, demodulators and digital blocks `foxygnal export` writes ----
        "blocks_short_to_float"
        | "blocks_float_to_short"
        | "blocks_interleaved_short_to_complex" => {
            // GNU Radio divides (or multiplies) by its scale; foxygnal's is
            // the float value of ±32767.
            let key = if id == "blocks_interleaved_short_to_complex" {
                "scale_factor"
            } else {
                "scale"
            };
            let s = cx.f64(key).unwrap_or(1.0);
            if s == 0.0 {
                return Err("a scale of 0 has no foxygnal equivalent".into());
            }
            if cx.boolean("swap") == Some(true) || cx.boolean("vector_input") == Some(true) {
                cx.warn("swapped or vector input isn't supported");
            }
            let kind = match id.as_str() {
                "blocks_short_to_float" => "i16_to_f32",
                "blocks_float_to_short" => "f32_to_i16",
                _ => "ci16_to_c32",
            };
            Out::single(
                name,
                kind,
                params(&[("scale", json!(32767.0 / s))]),
                true,
                true,
            )
        }
        "blocks_uchar_to_float" => Out::single(name, "u8_to_f32", Params::new(), true, true),
        "analog_am_demod_cf" => {
            cx.warn(
                "GNU Radio's AM Demod low-passes the audio (audio_pass / audio_stop); \
                 foxygnal's doesn't: add a FIR Filter if you need one",
            );
            let decim = cx.f64("audio_decim").unwrap_or(1.0);
            let mut nodes = vec![("", "am_demod", Params::new())];
            if decim != 1.0 {
                let mut d = params(&[("type", json!("f32"))]);
                set(&mut d, "decimation", cx.int("audio_decim"));
                nodes.push(("audio", "decimator", d));
            }
            Out::chain(name, nodes)
        }
        "digital_costas_loop_cc" => {
            let order = match cx.f64("order").map(|o| o as i64) {
                Some(o @ (2 | 4 | 8)) => o.to_string(),
                o => {
                    cx.warn(format!(
                        "order {} isn't supported (2, 4 or 8); using 2",
                        o.map_or("?".into(), |o| o.to_string())
                    ));
                    "2".into()
                }
            };
            let mut p = params(&[("order", json!(order))]);
            set(&mut p, "loop_bw", cx.num("w"));
            if cx.used_outputs.contains(&1) {
                cx.warn(
                    "the frequency output is in radians per sample in GNU Radio, in Hz in \
                     foxygnal: scale what uses it (× 2π / rate)",
                );
            }
            if cx.used_outputs.iter().any(|&o| o > 1) {
                cx.warn("the phase and error outputs aren't supported");
            }
            let mut o = Out::single(name, "costas_loop", p, true, true);
            o.output(1, format!("{name}.freq"));
            o
        }
        "digital_symbol_sync_xx" => {
            let ty = match cx.raw("type").unwrap_or("cc") {
                "ff" => "f32",
                _ => "c32",
            };
            let ted = cx.raw("ted_type").unwrap_or("digital.TED_GARDNER");
            let ted = if ted.contains("GARDNER") {
                "gardner"
            } else if ted.ends_with("TED_MUELLER_AND_MULLER") {
                "mueller_muller"
            } else {
                cx.warn(format!(
                    "timing error detector `{ted}` isn't available; using Gardner"
                ));
                "gardner"
            };
            let mut p = params(&[("type", json!(ty)), ("ted", json!(ted))]);
            set(&mut p, "sps", cx.num("sps"));
            set(&mut p, "loop_bw", cx.num("loop_bw"));
            if cx.f64("osps").is_some_and(|o| o != 1.0) {
                cx.warn("only one output sample per symbol is supported");
            }
            Out::single(name, "clock_recovery", p, true, true)
        }
        "digital_binary_slicer_fb" => Out::single(name, "binary_slicer", Params::new(), true, true),
        "digital_correlate_access_code_tag_xx" | "digital_correlate_access_code_tag_bb" => {
            if id.ends_with("_xx") && cx.raw("type").is_some_and(|t| t != "byte") {
                return Err("only the byte (hard bits) version is supported".into());
            }
            let code = cx.string("access_code").unwrap_or_default();
            if code.is_empty() || code.len() > 64 || !code.bytes().all(|b| b == b'0' || b == b'1') {
                return Err(format!(
                    "access code `{code}` isn't a bit string of up to 64 bits"
                ));
            }
            // Whole nibbles read better as hex.
            let code = if code.len().is_multiple_of(4) {
                let v = u64::from_str_radix(&code, 2).expect("checked bits");
                format!("0x{v:0width$X}", width = code.len() / 4)
            } else {
                code
            };
            let mut p = params(&[("access_code", json!(code))]);
            set(&mut p, "threshold", cx.int("threshold"));
            if cx.string("tagname").is_some_and(|t| t != "packet") {
                cx.warn("the tag is always named `packet` in foxygnal");
            }
            Out::single(name, "correlate_access_code", p, true, true)
        }
        "epy_block" => match placeholder(cx) {
            Some(o) => o,
            None => return Ok(Mapped::Unmapped),
        },

        // ---- Hardware ----
        _ if is_hardware_source(&id) => sdr_source(cx, &id)?,

        // ---- GUI instruments (QT, and 3.7's WX GUI) ----
        "qtgui_freq_sink_x"
        | "qtgui_time_sink_x"
        | "qtgui_waterfall_sink_x"
        | "qtgui_const_sink_x"
        | "qtgui_sink_x"
        | "qtgui_number_sink"
        | "qtgui_histogram_sink_x"
        | "qtgui_time_raster_sink_x"
        | "qtgui_vector_sink_f"
        | "qtgui_eye_sink_x"
        | "wxgui_fftsink2"
        | "wxgui_scopesink2"
        | "wxgui_waterfallsink2"
        | "wxgui_constellationsink2"
        | "wxgui_numbersink2"
        | "wxgui_histosink2" => {
            let instrument = match id.as_str() {
                "qtgui_freq_sink_x" | "qtgui_sink_x" | "wxgui_fftsink2" => "Spectrum",
                "qtgui_waterfall_sink_x" | "qtgui_time_raster_sink_x" | "wxgui_waterfallsink2" => {
                    "Waterfall"
                }
                "qtgui_const_sink_x" | "wxgui_constellationsink2" => "Constellation",
                "qtgui_number_sink" | "wxgui_numbersink2" => "Meter",
                _ => "Scope",
            };
            let raw = cx.raw("type").unwrap_or("complex");
            if raw.starts_with("msg") {
                cx.warn("message-input instrument left out (foxygnal has no message ports yet)");
                return Ok(Mapped::Skip);
            }
            let ty = if matches!(
                id.as_str(),
                "qtgui_const_sink_x" | "wxgui_constellationsink2"
            ) {
                Ok("c32")
            } else {
                cx.stream_type(CF)
            }?;
            let n = cx
                .first(&["nconnections", "num_inputs"])
                .and_then(|k| cx.f64(k))
                .unwrap_or(1.0)
                .max(1.0) as usize;
            let mut o = Out::default();
            for i in 0..n {
                let id = o.add(
                    name,
                    &i.to_string(),
                    "null_sink",
                    params(&[("type", json!(ty))]),
                );
                o.offsets[i] = (0.0, i as f64 * 90.0);
                o.input(i, format!("{id}.in"));
            }
            let gui = if id.starts_with("wxgui") { "WX" } else { "QT" };
            cx.warn(format!(
                "{gui} GUI sink became a Null Sink: instruments in foxygnal attach to wires, so \
                 click the wire into it and open a {instrument}"
            ));
            o
        }
        _ => return Ok(Mapped::Unmapped),
    };
    Ok(Mapped::Nodes(out))
}

/// The marker `foxygnal export` writes into a placeholder Python block.
const MARKER: &str = "# foxygnal-block: ";

/// A placeholder Python block written by `foxygnal export` → the foxygnal
/// block it stands for (kind, params and port names are in its source).
fn placeholder(cx: &mut Cx<'_>) -> Option<Out> {
    let src = cx.b.params.get("_source_code")?;
    let line = src.lines().find_map(|l| l.trim().strip_prefix(MARKER))?;
    let Ok(Json::Object(orig)) = serde_json::from_str::<Json>(line) else {
        cx.warn("the foxygnal placeholder line isn't readable");
        return None;
    };
    let kind = orig.get("kind")?.as_str()?;
    let p: Params = match orig.get("params") {
        Some(Json::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => Params::new(),
    };
    let ports = |key: &str| -> Vec<String> {
        orig.get(key)
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let name = cx.name();
    let mut o = Out::default();
    let id = o.add(name, "", kind, p);
    for (k, port) in ports("inputs").iter().enumerate() {
        o.input(k, format!("{id}.{port}"));
    }
    for (k, port) in ports("outputs").iter().enumerate() {
        o.output(k, format!("{id}.{port}"));
    }
    Some(o)
}

/// De-emphasis time constant → `off` / `50us` / `75us`.
fn deemphasis(cx: &mut Cx<'_>, tau: Option<f64>) -> &'static str {
    match tau {
        Some(t) if t <= 0.0 => "off",
        Some(t) if (t - 50e-6).abs() < 1e-7 => "50us",
        Some(t) if (t - 75e-6).abs() < 1e-7 => "75us",
        Some(t) => {
            let pick = if t < 62.5e-6 { "50us" } else { "75us" };
            cx.warn(format!(
                "de-emphasis {} µs isn't available; using {pick}",
                py::fmt_num(t * 1e6)
            ));
            pick
        }
        None => "75us",
    }
}

fn window(cx: &mut Cx<'_>, e: Option<&Py>) -> &'static str {
    let text = match e {
        Some(Py::Name(n)) => n.as_str(),
        Some(other) => {
            cx.warn(format!(
                "window `{}` isn't understood; using Hamming",
                py::show(other)
            ));
            return "hamming";
        }
        None => return "hamming",
    };
    let up = text.to_ascii_uppercase();
    if up.contains("HAMMING") {
        "hamming"
    } else if up.contains("BLACKMAN") {
        "blackman"
    } else if up.contains("KAISER") {
        "kaiser"
    } else {
        cx.warn(format!("window `{text}` isn't available; using Hamming"));
        "hamming"
    }
}

/// Kaiser β → stopband attenuation (dB), inverting Kaiser's formula.
fn kaiser_attenuation(beta: f64) -> f64 {
    if beta > 4.55 {
        beta / 0.1102 + 8.7
    } else {
        // Below 50 dB the formula is β = 0.5842 (A − 21)^0.4 + 0.07886 (A − 21).
        // Solve numerically on 21..50 dB.
        let f = |a: f64| 0.5842 * (a - 21.0).powf(0.4) + 0.07886 * (a - 21.0);
        let (mut lo, mut hi) = (21.0, 50.0);
        for _ in 0..50 {
            let mid = 0.5 * (lo + hi);
            if f(mid) < beta {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }
}

/// `low_pass_filter` and friends → `fir_filter`, with an interpolator before
/// and a decimator after when GRC asks for them.
fn fir(cx: &mut Cx<'_>, id: &str) -> Result<Out, String> {
    let name = cx.name();
    let raw = cx.raw("type").unwrap_or("fir_filter_ccf");
    let interp_type = raw.starts_with("interp_");
    let code = raw.rsplit('_').next().unwrap_or("");
    let ty = match code.get(..2) {
        Some("cc") => "c32",
        Some("ff") => "f32",
        _ => {
            return Err(format!(
                "filter type `{raw}` isn't supported (ccf, ccc or fff)"
            ));
        }
    };
    let mode = match id {
        "low_pass_filter" => "low_pass",
        "high_pass_filter" => "high_pass",
        "band_pass_filter" => "band_pass",
        _ => "band_stop",
    };
    if code == "ccc" && mode == "band_pass" {
        cx.warn(
            "complex band-pass taps (one side of the spectrum) become a real band-pass \
             (both sides)",
        );
    }
    let mut p = params(&[("type", json!(ty)), ("mode", json!(mode))]);
    if mode == "low_pass" || mode == "high_pass" {
        set(&mut p, "cutoff", cx.num("cutoff_freq"));
    } else {
        set(&mut p, "cutoff", cx.num("low_cutoff_freq"));
        set(&mut p, "cutoff_high", cx.num("high_cutoff_freq"));
    }
    set(&mut p, "transition", cx.num("width"));
    set(&mut p, "gain", cx.num("gain"));
    let win = cx.py("win");
    let w = window(cx, win.as_ref());
    p.insert("window".into(), json!(w));
    if w == "kaiser"
        && let Some(beta) = cx.f64("beta")
    {
        p.insert(
            "attenuation".into(),
            json!((kaiser_attenuation(beta) * 10.0).round() / 10.0),
        );
    }
    let mut nodes = Vec::new();
    if interp_type && cx.f64("interp").is_some_and(|i| i != 1.0) {
        let mut ip = params(&[("type", json!(ty))]);
        set(&mut ip, "interpolation", cx.int("interp"));
        nodes.push(("interp", "interpolator", ip));
        cx.warn("interpolation is done by a separate Interpolator before the filter");
    }
    nodes.push(("filter", "fir_filter", p));
    if cx.f64("decim").is_some_and(|d| d != 1.0) {
        let mut dp = params(&[("type", json!(ty))]);
        set(&mut dp, "decimation", cx.int("decim"));
        nodes.push(("decim", "decimator", dp));
    }
    // The block's own name goes to the filter node.
    let mut o = Out::chain(name, nodes);
    if o.nodes.len() > 1 {
        rename_primary(&mut o, name, "filter");
    }
    Ok(o)
}

/// The stream type of a filter from its GNU Radio type code
/// (`fir_filter_ccf`, `interp_fir_filter_fff`, `ccc`, …).
fn filter_type(raw: &str) -> Result<&'static str, String> {
    let code = raw.rsplit('_').next().unwrap_or("");
    match code {
        "ccf" | "ccc" => Ok("c32"),
        "fff" => Ok("f32"),
        _ => Err(format!(
            "filter type `{raw}` isn't supported (ccf, ccc or fff)"
        )),
    }
}

/// `firdes.root_raised_cosine(gain, sampling_freq, symbol_rate, alpha, ntaps)`.
struct RrcArgs {
    gain: Option<Py>,
    fs: Py,
    sym: Py,
    alpha: Option<Py>,
    ntaps: Option<Py>,
}

/// A root-raised-cosine filter → `rrc_filter` (interpolating when GRC
/// interpolates by the samples per symbol), with a decimator after when
/// the block decimates.
fn rrc(cx: &mut Cx<'_>, ty: &str, a: RrcArgs, interp: f64) -> Result<Out, String> {
    let name = cx.name();
    let sps_e = Py::Bin(BinOp::Div, Box::new(a.fs), Box::new(a.sym));
    let sps = cx
        .vars
        .eval(&sps_e)
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|s| s.is_finite() && *s > 0.0)
        .ok_or("the samples per symbol (sample rate / symbol rate) can't be evaluated")?;
    let mut p = params(&[("type", json!(ty))]);
    set(
        &mut p,
        "sps",
        cx.num_of(&sps_e, "samples per symbol", false),
    );
    if let Some(alpha) = &a.alpha {
        set(
            &mut p,
            "rolloff",
            cx.num_of(alpha, "roll-off (alpha)", false),
        );
    }
    if let Some(n) = a
        .ntaps
        .as_ref()
        .and_then(|n| cx.vars.eval(n).ok()?.as_f64())
    {
        p.insert("span".into(), json!(((n / sps).round() as i64).max(1)));
    }
    let gain = a.gain.as_ref().and_then(|g| cx.vars.eval(g).ok()?.as_f64());
    if interp != 1.0 {
        if (interp - sps).abs() > 1e-9 {
            return Err(format!(
                "interpolation by {} with {} samples per symbol isn't supported (they must \
                 be equal)",
                py::fmt_num(interp),
                py::fmt_num(sps)
            ));
        }
        p.insert("mode".into(), json!("interpolate"));
        // GNU Radio's taps sum to the gain (usually sps, for unit peaks);
        // foxygnal's interpolating RRC already peaks at 1.
        if let Some(g) = gain.map(|g| g / sps)
            && (g - 1.0).abs() > 1e-9
        {
            p.insert("gain".into(), json!(g));
        }
    } else {
        p.insert("mode".into(), json!("filter"));
        if let Some(g) = &a.gain {
            set(&mut p, "gain", cx.num_of(g, "taps gain", false));
        }
    }
    let mut nodes = vec![("filter", "rrc_filter", p)];
    if cx.f64("decim").is_some_and(|d| d != 1.0) {
        let mut dp = params(&[("type", json!(ty))]);
        set(&mut dp, "decimation", cx.int("decim"));
        nodes.push(("decim", "decimator", dp));
    }
    let mut o = Out::chain(name, nodes);
    if o.nodes.len() > 1 {
        rename_primary(&mut o, name, "filter");
    }
    Ok(o)
}

/// `fir_filter_xxx` / `interp_fir_filter_xxx` with taps from
/// `firdes.root_raised_cosine(…)` (→ `rrc_filter`) or
/// `firdes.low_pass(…)` / `high_pass(…)` (→ `fir_filter`).
fn fir_taps(cx: &mut Cx<'_>, id: &str) -> Result<Out, String> {
    let name = cx.name();
    let ty = filter_type(cx.raw("type").unwrap_or("ccc"))?;
    let interp = if id == "interp_fir_filter_xxx" {
        cx.f64("interp").unwrap_or(1.0)
    } else {
        1.0
    };
    let taps = cx.py("taps").ok_or("the taps aren't understood")?;
    let taps = cx.deref(taps);
    let Py::Call(f, args, kwargs) = &taps else {
        return Err(format!(
            "taps `{}` aren't a firdes.low_pass / high_pass / root_raised_cosine(…) call",
            short(&py::show(&taps))
        ));
    };
    if f.ends_with("root_raised_cosine") {
        let (Some(fs), Some(sym)) = (
            arg(args, kwargs, 1, "sampling_freq").cloned(),
            arg(args, kwargs, 2, "symbol_rate").cloned(),
        ) else {
            return Err("root_raised_cosine taps without sample and symbol rates".into());
        };
        let a = RrcArgs {
            gain: arg(args, kwargs, 0, "gain").cloned(),
            fs,
            sym,
            alpha: arg(args, kwargs, 3, "alpha").cloned(),
            ntaps: arg(args, kwargs, 4, "ntaps").cloned(),
        };
        return rrc(cx, ty, a, interp);
    }
    let mode = if f.ends_with("low_pass") {
        "low_pass"
    } else if f.ends_with("high_pass") {
        "high_pass"
    } else {
        return Err(format!(
            "taps `{}` aren't a firdes.low_pass / high_pass / root_raised_cosine(…) call",
            short(&py::show(&taps))
        ));
    };
    let mut p = params(&[("type", json!(ty)), ("mode", json!(mode))]);
    if let Some(g) = arg(args, kwargs, 0, "gain") {
        set(&mut p, "gain", cx.num_of(g, "taps gain", false));
    }
    if let Some(c) = arg(args, kwargs, 2, "cutoff_freq") {
        set(&mut p, "cutoff", cx.num_of(c, "taps cutoff", false));
    }
    if let Some(t) = arg(args, kwargs, 3, "transition_width") {
        set(&mut p, "transition", cx.num_of(t, "taps transition", false));
    }
    let w = arg(args, kwargs, 4, "window").cloned();
    let w = window(cx, w.as_ref());
    p.insert("window".into(), json!(w));
    if w == "kaiser"
        && let Some(beta) = arg(args, kwargs, 5, "beta")
        && let Some(b) = cx.vars.eval(beta).ok().and_then(|v| v.as_f64())
    {
        p.insert(
            "attenuation".into(),
            json!((kaiser_attenuation(b) * 10.0).round() / 10.0),
        );
    }
    let mut nodes = Vec::new();
    if interp != 1.0 {
        let mut ip = params(&[("type", json!(ty))]);
        set(&mut ip, "interpolation", cx.int("interp"));
        nodes.push(("interp", "interpolator", ip));
        cx.warn("interpolation is done by a separate Interpolator before the filter");
    }
    nodes.push(("filter", "fir_filter", p));
    if cx.f64("decim").is_some_and(|d| d != 1.0) {
        let mut dp = params(&[("type", json!(ty))]);
        set(&mut dp, "decimation", cx.int("decim"));
        nodes.push(("decim", "decimator", dp));
    }
    let mut o = Out::chain(name, nodes);
    if o.nodes.len() > 1 {
        rename_primary(&mut o, name, "filter");
    }
    Ok(o)
}

/// gr-network's UDP `header`: 0 none, 1 a 64-bit sequence number; the
/// other formats have no foxygnal equivalent.
fn udp_header(cx: &mut Cx<'_>) -> Option<Json> {
    let h = cx.f64("header")?;
    Some(json!(if h == 1.0 {
        "seq64"
    } else {
        if h != 0.0 {
            cx.warn(format!(
                "UDP header type {} isn't supported; using none (raw samples)",
                py::fmt_num(h)
            ));
        }
        "none"
    }))
}

/// Give the node with `suffix` the GRC name and the first node its suffix
/// instead, so the GRC name stays on the block that does the work.
fn rename_primary(o: &mut Out, name: &str, suffix: &str) {
    let want = format!("{name}_{suffix}");
    let Some(k) = o.nodes.iter().position(|n| n.id == want) else {
        return;
    };
    let first_new = format!(
        "{name}_{}",
        if o.nodes[0].kind == "interpolator" {
            "interp"
        } else {
            "pre"
        }
    );
    let swap = |s: &str| -> String {
        let (blk, port) = s.rsplit_once('.').unwrap_or((s, ""));
        let blk = if blk == name {
            first_new.as_str()
        } else if blk == want {
            name
        } else {
            blk
        };
        format!("{blk}.{port}")
    };
    for (i, n) in o.nodes.iter_mut().enumerate() {
        if i == k {
            n.id = name.to_owned();
        } else if n.id == name {
            n.id = first_new.clone();
        }
    }
    for l in &mut o.links {
        l.from = swap(&l.from);
        l.to = swap(&l.to);
    }
    for ins in &mut o.inputs {
        for e in ins.iter_mut() {
            *e = swap(e);
        }
    }
    for e in o.outputs.iter_mut().flatten() {
        *e = swap(e);
    }
}

/// An argument of a call, by position or keyword.
fn arg<'e>(args: &'e [Py], kwargs: &'e [(String, Py)], pos: usize, key: &str) -> Option<&'e Py> {
    kwargs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .or_else(|| args.get(pos))
}

/// `freq_xlating_fir_filter_xxx` → `freq_xlating_fir`, reading the cutoff
/// and transition out of `firdes.low_pass(...)` taps.
fn xlating(cx: &mut Cx<'_>, id: &str) -> Result<Out, String> {
    let name = cx.name();
    let raw = cx
        .raw("type")
        .map(str::to_owned)
        .unwrap_or_else(|| id.rsplit('_').next().unwrap_or("ccc").to_owned());
    let real_in = match raw.get(..2) {
        Some("cc") => false,
        Some("fc") => true,
        _ => {
            return Err(format!(
                "type `{raw}` isn't supported (ccc, ccf, fcc or fcf)"
            ));
        }
    };
    let mut p = Params::new();
    set(&mut p, "offset", cx.num("center_freq"));
    set(&mut p, "decimation", cx.int("decim"));
    if let Some(taps) = cx.py("taps") {
        let taps = cx.deref(taps);
        match &taps {
            Py::Call(f, args, kwargs) if f.ends_with("low_pass") || f.ends_with("low_pass_2") => {
                let two = f.ends_with("low_pass_2");
                if let Some(c) = arg(args, kwargs, 2, "cutoff_freq") {
                    set(&mut p, "cutoff", cx.num_of(c, "taps cutoff", false));
                }
                if let Some(t) = arg(args, kwargs, 3, "transition_width") {
                    set(&mut p, "transition", cx.num_of(t, "taps transition", false));
                }
                if two && let Some(a) = arg(args, kwargs, 4, "attenuation_dB") {
                    set(
                        &mut p,
                        "attenuation",
                        cx.num_of(a, "taps attenuation", false),
                    );
                }
                let w = arg(args, kwargs, if two { 5 } else { 4 }, "window").cloned();
                let w = window(cx, w.as_ref());
                p.insert("window".into(), json!(w));
                if !two
                    && w == "kaiser"
                    && let Some(beta) = arg(args, kwargs, 5, "beta")
                    && let Some(b) = cx.vars.eval(beta).ok().and_then(|v| v.as_f64())
                {
                    p.insert(
                        "attenuation".into(),
                        json!((kaiser_attenuation(b) * 10.0).round() / 10.0),
                    );
                }
                if let Some(g) = arg(args, kwargs, 0, "gain")
                    && cx
                        .vars
                        .eval(g)
                        .ok()
                        .and_then(|v| v.as_f64())
                        .is_some_and(|g| g != 1.0)
                {
                    cx.warn(format!(
                        "taps gain `{}` isn't applied (the filter has unity gain)",
                        py::show(g)
                    ));
                }
            }
            Py::Call(f, args, kwargs) if f.ends_with("band_pass") => {
                let lo = arg(args, kwargs, 2, "low_cutoff_freq").cloned();
                let hi = arg(args, kwargs, 3, "high_cutoff_freq").cloned();
                if let (Some(lo), Some(hi)) = (lo, hi) {
                    // A band-pass of -x..x is a low-pass at x; otherwise
                    // keep the width, centred.
                    let half = Py::Bin(
                        BinOp::Div,
                        Box::new(Py::Bin(
                            BinOp::Sub,
                            Box::new(hi.clone()),
                            Box::new(lo.clone()),
                        )),
                        Box::new(Py::Num(2.0, "2".into())),
                    );
                    let sym = match (cx.vars.eval(&lo), cx.vars.eval(&hi)) {
                        (Ok(l), Ok(h)) => l
                            .as_f64()
                            .zip(h.as_f64())
                            .is_some_and(|(l, h)| (l + h).abs() < 1e-9 * h.abs().max(1.0)),
                        _ => false,
                    };
                    let cut = if sym { hi } else { half };
                    if !sym {
                        cx.warn(
                            "band-pass taps that aren't centred on 0 Hz become a low-pass of \
                             the same width, centred",
                        );
                    }
                    set(&mut p, "cutoff", cx.num_of(&cut, "taps cutoff", false));
                }
                if let Some(t) = arg(args, kwargs, 4, "transition_width") {
                    set(&mut p, "transition", cx.num_of(t, "taps transition", false));
                }
                let w = arg(args, kwargs, 5, "window").cloned();
                let w = window(cx, w.as_ref());
                p.insert("window".into(), json!(w));
            }
            other => {
                // Plain taps (`[1]`, a list of numbers): derive a low-pass
                // that keeps the decimated band.
                let rate = cx.py("samp_rate");
                let decim = cx.py("decim");
                if let (Some(rate), Some(decim)) = (rate, decim) {
                    let cut = Py::Bin(
                        BinOp::Div,
                        Box::new(rate.clone()),
                        Box::new(Py::Bin(
                            BinOp::Mul,
                            Box::new(Py::Num(2.5, "2.5".into())),
                            Box::new(decim.clone()),
                        )),
                    );
                    let tw = Py::Bin(
                        BinOp::Div,
                        Box::new(rate),
                        Box::new(Py::Bin(
                            BinOp::Mul,
                            Box::new(Py::Num(10.0, "10".into())),
                            Box::new(decim),
                        )),
                    );
                    set(
                        &mut p,
                        "cutoff",
                        cx.vars.number(&cut, false).ok().map(|(v, _)| v),
                    );
                    set(
                        &mut p,
                        "transition",
                        cx.vars.number(&tw, false).ok().map(|(v, _)| v),
                    );
                }
                cx.warn(format!(
                    "taps `{}` aren't a firdes.low_pass(…) call; the filter is designed from \
                     a cutoff of 0.4 × the output rate instead: check cutoff and transition",
                    short(&py::show(other))
                ));
            }
        }
    }
    let mut o = Out::default();
    let xl = o.add(name, "", "freq_xlating_fir", p);
    if real_in {
        let conv = o.add(name, "c32", "f32_to_c32", Params::new());
        o.offsets[1] = (-240.0, 0.0);
        o.input(0, format!("{conv}.in"));
        o.link(format!("{conv}.out"), format!("{xl}.in"));
    } else {
        o.input(0, format!("{xl}.in"));
    }
    o.output(0, format!("{xl}.out"));
    Ok(o)
}

fn short(s: &str) -> String {
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(57).collect::<String>())
    } else {
        s.to_owned()
    }
}

fn is_pi(e: &Py) -> bool {
    matches!(e, Py::Name(n) if py::constant(n) == Some(std::f64::consts::PI))
}

fn is_two(e: &Py) -> bool {
    matches!(e, Py::Num(v, _) if *v == 2.0)
}

/// Flatten a product/quotient into numerator and denominator factors.
fn factors(e: &Py, num: &mut Vec<Py>, den: &mut Vec<Py>, flip: bool) {
    match e {
        Py::Bin(BinOp::Mul, a, b) => {
            factors(a, num, den, flip);
            factors(b, num, den, flip);
        }
        Py::Bin(BinOp::Div, a, b) => {
            factors(a, num, den, flip);
            factors(b, num, den, !flip);
        }
        Py::Call(f, args, kw) if kw.is_empty() && args.len() == 1 && f == "float" => {
            factors(&args[0], num, den, flip);
        }
        other => {
            if flip {
                den.push(other.clone());
            } else {
                num.push(other.clone());
            }
        }
    }
}

fn product(mut fs: Vec<Py>) -> Option<Py> {
    let first = if fs.is_empty() {
        return None;
    } else {
        fs.remove(0)
    };
    Some(
        fs.into_iter()
            .fold(first, |a, b| Py::Bin(BinOp::Mul, Box::new(a), Box::new(b))),
    )
}

/// The deviation a quadrature demod gain implies. GRC writes the gain as
/// `rate / (2 * math.pi * deviation)`; the rate is the stream's, which
/// foxygnal's `fm_demod` knows, so only the deviation is kept.
fn deviation_from_gain(cx: &mut Cx<'_>) -> Option<Json> {
    let gain = cx.py("gain")?;
    for e in [gain.clone(), cx.expand(&gain, 0)] {
        // gain = rate / D, where D holds 2π: the deviation is D / 2π.
        let Py::Bin(BinOp::Div, _, d) = &e else {
            continue;
        };
        let (mut num, mut den) = (Vec::new(), Vec::new());
        factors(d, &mut num, &mut den, false);
        let Some(pi) = num.iter().position(is_pi) else {
            continue;
        };
        num.remove(pi);
        // The 2 next to π (`2*math.pi` flattens to neighbours).
        let Some(two) = num
            .iter()
            .enumerate()
            .filter(|(_, f)| is_two(f))
            .min_by_key(|(k, _)| k.abs_diff(pi))
            .map(|(k, _)| k)
        else {
            continue;
        };
        num.remove(two);
        if num.is_empty() {
            continue;
        }
        let dev = match (product(num), product(den)) {
            (Some(d), None) => d,
            (Some(d), Some(n)) => Py::Bin(BinOp::Div, Box::new(d), Box::new(n)),
            _ => continue,
        };
        return cx.num_of(&dev, "deviation (from the gain)", false);
    }
    cx.warn(format!(
        "can't tell the deviation from gain `{}` (expected rate / (2 * math.pi * deviation)); \
         set the FM demodulator's deviation by hand (deviation = input rate / (2π × gain))",
        py::show(&gain)
    ));
    None
}

/// Split `key=value` pairs separated by commas or spaces.
fn kwargs(s: &str) -> Vec<(String, String)> {
    s.split([',', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.trim().to_owned(), v.trim().to_owned()),
            None => (p.to_owned(), String::new()),
        })
        .collect()
}

/// A foxygnal device string from osmosdr / Soapy / UHD arguments.
pub(crate) fn guess_device(args: &str, driver_hint: Option<&str>) -> String {
    let kv = kwargs(args);
    let get = |k: &str| kv.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let index_or_serial = |v: &str| {
        if v.is_empty() {
            String::new()
        } else if v.len() <= 2 && v.chars().all(|c| c.is_ascii_digit()) {
            if v == "0" {
                String::new()
            } else {
                format!("index={v}")
            }
        } else {
            format!("serial={v}")
        }
    };
    let join = |driver: &str, opts: Vec<String>| {
        let opts: Vec<String> = opts.into_iter().filter(|o| !o.is_empty()).collect();
        format!("{driver}:{}", opts.join(","))
    };
    let driver = get("driver").or(driver_hint);
    if let Some(v) = get("rtl_tcp") {
        let (host, port) = v.split_once(':').unwrap_or((v, ""));
        let mut o = vec![format!(
            "host={}",
            if host.is_empty() { "127.0.0.1" } else { host }
        )];
        if !port.is_empty() {
            o.push(format!("port={port}"));
        }
        return join("rtltcp", o);
    }
    if get("rtl").is_some() || driver == Some("rtlsdr") {
        let mut o = vec![index_or_serial(get("rtl").or(get("serial")).unwrap_or(""))];
        if let Some(d) = get("direct_samp") {
            o.push(format!("direct_sampling={d}"));
        }
        if get("offset_tune") == Some("1") {
            o.push("offset_tuning=1".into());
        }
        if get("bias") == Some("1") {
            o.push("bias_tee=1".into());
        }
        return join("rtlsdr", o);
    }
    if get("hackrf").is_some() || driver == Some("hackrf") {
        let mut o = vec![index_or_serial(
            get("hackrf").or(get("serial")).unwrap_or(""),
        )];
        if get("bias") == Some("1") {
            o.push("bias_tee=1".into());
        }
        return join("hackrf", o);
    }
    // osmosdr keys that name a SoapySDR driver.
    let soapy_driver = driver.map(str::to_owned).or_else(|| {
        [
            "airspy",
            "airspyhf",
            "bladerf",
            "uhd",
            "sdrplay",
            "plutosdr",
            "lime",
            "redpitaya",
        ]
        .iter()
        .find(|k| get(k).is_some())
        .map(|k| (*k).to_owned())
    });
    match soapy_driver {
        Some(d) => {
            let mut o = vec![format!("driver={d}")];
            for (k, v) in &kv {
                if matches!(
                    k.as_str(),
                    "driver" | "numchan" | "buflen" | "buffers" | "soapy"
                ) || *k == d
                    || v.is_empty()
                {
                    continue;
                }
                o.push(format!("{k}={v}"));
            }
            join("soapy", o)
        }
        None => String::new(),
    }
}

/// SDR sources → `sdr_source`.
fn sdr_source(cx: &mut Cx<'_>, id: &str) -> Result<Out, String> {
    let name = cx.name();
    let pick = |cx: &Cx<'_>, keys: &[&'static str]| cx.first(keys);
    let mut p = Params::new();
    if let Some(k) = pick(cx, &["sample_rate", "samp_rate"]) {
        set(&mut p, "sample_rate", cx.num(k));
    }
    if let Some(k) = pick(
        cx,
        &["freq0", "center_freq0", "center_freq", "freq", "frequency"],
    ) {
        set(&mut p, "frequency", cx.num(k));
    }
    if let Some(k) = pick(
        cx,
        &["gain0", "overall_gain0", "gain", "rf_gain0", "rf_gain"],
    ) {
        set(&mut p, "gain", cx.num(k));
    }
    if let Some(k) = pick(cx, &["corr0", "freq_correction", "ppm", "freq_correction0"]) {
        set(&mut p, "ppm", cx.num(k));
    }
    if let Some(k) = pick(cx, &["bw0", "bandwidth0", "bandwidth", "bw"]) {
        set(&mut p, "bandwidth", cx.num(k));
    }
    if let Some(k) = pick(cx, &["ant0", "antenna0", "antenna"]) {
        set(&mut p, "antenna", cx.string(k).map(Json::from));
    }
    let agc = match pick(cx, &["gain_mode0", "agc0", "agc"]) {
        Some(k) => cx.boolean(k).unwrap_or(false),
        None => false,
    };
    if agc {
        p.insert("agc".into(), json!(true));
    }
    if id == "uhd_usrp_source" && cx.boolean("norm_gain0") == Some(true) {
        cx.warn("normalized gain (0–1) is used as dB; check the gain");
    }
    if cx.f64("nchan").is_some_and(|n| n > 1.0) {
        cx.warn("only the first channel is received");
    }
    // The device string.
    let (args, hint) = match id {
        "osmosdr_source" => (cx.string("args").unwrap_or_default(), None),
        "rtlsdr_source" => (cx.string("args").unwrap_or_default(), Some("rtlsdr")),
        "uhd_usrp_source" => {
            let mut a = String::from("driver=uhd");
            for k in ["dev_addr", "dev_args"] {
                if let Some(s) = cx.string(k).filter(|s| !s.is_empty()) {
                    a.push(',');
                    a.push_str(&s);
                }
            }
            (a, None)
        }
        "iio_pluto_source" => {
            let uri = cx.string("uri").unwrap_or_default();
            let a = if uri.is_empty() {
                "driver=plutosdr".to_owned()
            } else {
                format!("driver=plutosdr,uri={uri}")
            };
            (a, None)
        }
        _ => {
            // soapy_<driver>_source, soapy_source, soapy_custom_source.
            let mut a = String::new();
            for k in ["dev", "driver", "dev_args", "args"] {
                if let Some(s) = cx.string(k).filter(|s| !s.is_empty()) {
                    if !a.is_empty() {
                        a.push(',');
                    }
                    // soapy_custom_source's `driver` is the bare name.
                    if k == "driver" && !s.contains('=') {
                        a.push_str("driver=");
                    }
                    a.push_str(&s);
                }
            }
            let hint = id
                .strip_prefix("soapy_")
                .and_then(|s| s.strip_suffix("_source"))
                .filter(|d| !matches!(*d, "" | "custom"));
            let hint = hint.map(|d| match d {
                "rtlsdr" => "rtlsdr",
                "hackrf" => "hackrf",
                "airspy" => "airspy",
                "airspyhf" => "airspyhf",
                "bladerf" => "bladerf",
                "plutosdr" => "plutosdr",
                "limesdr" => "lime",
                "sdrplay" => "sdrplay",
                other => other,
            });
            (a, hint)
        }
    };
    // `dev` is often `driver=rtlsdr`; `driver=` inside args wins over the hint.
    let device = guess_device(&args, hint);
    p.insert("device".into(), json!(device));
    if device.is_empty() {
        cx.warn(
            "device left empty: the first SDR found is used (or the simulator); pick one in \
             the inspector",
        );
    } else {
        cx.warn(format!(
            "device `{device}` guessed from `{}`; check it in the inspector",
            if args.is_empty() { id } else { &args }
        ));
    }
    Ok(Out::single(name, "sdr_source", p, false, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_strings() {
        assert_eq!(guess_device("numchan=1 rtl=0", None), "rtlsdr:");
        assert_eq!(guess_device("rtl=1", None), "rtlsdr:index=1");
        assert_eq!(
            guess_device("rtl=00000042,direct_samp=2", None),
            "rtlsdr:serial=00000042,direct_sampling=2"
        );
        assert_eq!(
            guess_device("rtl_tcp=10.0.0.5:1234", None),
            "rtltcp:host=10.0.0.5,port=1234"
        );
        assert_eq!(guess_device("hackrf=0,bias=1", None), "hackrf:bias_tee=1");
        assert_eq!(guess_device("airspy=0", None), "soapy:driver=airspy");
        assert_eq!(guess_device("driver=rtlsdr", None), "rtlsdr:");
        assert_eq!(
            guess_device("driver=plutosdr,uri=ip:192.168.2.1", None),
            "soapy:driver=plutosdr,uri=ip:192.168.2.1"
        );
        assert_eq!(guess_device("", Some("rtlsdr")), "rtlsdr:");
        assert_eq!(
            guess_device("driver=uhd,addr=192.168.10.2", None),
            "soapy:driver=uhd,addr=192.168.10.2"
        );
        assert_eq!(guess_device("", None), "");
    }

    #[test]
    fn kaiser_beta() {
        assert!((kaiser_attenuation(5.653) - 60.0).abs() < 0.1);
        assert!((kaiser_attenuation(3.395) - 40.0).abs() < 0.5);
    }
}
