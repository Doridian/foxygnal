//! Import GNU Radio Companion flowgraphs (`.grc`, the YAML format of GNU
//! Radio 3.8 and later) as foxygnal graphs.
//!
//! [`import`] turns the file into a [`GraphSpec`] plus a list of warnings:
//! variables become graph `[vars]` (their Python expressions translated to
//! foxygnal expressions where possible), common blocks map to foxygnal blocks
//! (see `map.rs` and `docs/GRC_IMPORT.md`), block coordinates become canvas
//! positions, disabled blocks are skipped and bypassed ones wired through.
//! Anything that can't be mapped is named in a warning; nothing is dropped
//! silently.

mod map;
pub mod py;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use foxygnal_core::GraphSpec;
use foxygnal_core::graph::{BlockSpec, ConnectionSpec};
use serde_json::{Value as Json, json};
use yaml_rust2::{Yaml, YamlLoader};

use crate::py::{Py, Value};

/// Import choices.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Map `audio_sink` to foxygnal's sound-card `audio_sink` (needs the
    /// `audio` build feature) instead of `browser_audio`.
    pub audio_device: bool,
}

/// The result of an import.
#[derive(Debug, Clone)]
pub struct Import {
    pub graph: GraphSpec,
    /// Everything that didn't translate exactly, one message each, naming
    /// the GRC block (`name` and GNU Radio block id) it is about.
    pub warnings: Vec<String>,
    /// The flowgraph's title (`options` → `title`), if it has one.
    pub title: Option<String>,
}

/// The file couldn't be read as a GRC flowgraph at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Enabled,
    Disabled,
    Bypassed,
}

#[derive(Debug, Clone)]
pub(crate) struct GrcBlock {
    pub name: String,
    /// The GNU Radio block type, e.g. `analog_sig_source_x`.
    pub id: String,
    /// Parameter values as written (Python expressions).
    pub params: BTreeMap<String, String>,
    state: State,
    coord: Option<(f64, f64)>,
}

#[derive(Debug, Clone)]
struct GrcConn {
    src: String,
    src_port: String,
    dst: String,
    dst_port: String,
}

struct GrcFile {
    title: Option<String>,
    generate: Option<String>,
    blocks: Vec<GrcBlock>,
    connections: Vec<GrcConn>,
}

/// A YAML scalar as the Python text GRC wrote.
fn scalar(y: &Yaml) -> String {
    match y {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => if *b { "True" } else { "False" }.into(),
        Yaml::Array(items) => format!(
            "[{}]",
            items.iter().map(scalar).collect::<Vec<_>>().join(", ")
        ),
        _ => String::new(),
    }
}

fn parse(text: &str) -> Result<GrcFile, Error> {
    let docs =
        YamlLoader::load_from_str(text).map_err(|e| Error(format!("not valid YAML: {e}")))?;
    let doc = docs
        .into_iter()
        .next()
        .ok_or_else(|| Error("the file is empty".into()))?;
    if !doc.is_hash() || doc["blocks"].is_badvalue() && doc["options"].is_badvalue() {
        return Err(Error(
            "not a GNU Radio Companion flowgraph (no `options` or `blocks`); only the YAML \
             format of GNU Radio 3.8 and later can be imported"
                .into(),
        ));
    }
    let opt = &doc["options"]["parameters"];
    let text_of = |y: &Yaml| Some(scalar(y)).filter(|s| !s.is_empty());
    let mut blocks = Vec::new();
    for (i, b) in doc["blocks"].as_vec().into_iter().flatten().enumerate() {
        let name = text_of(&b["name"]).unwrap_or_else(|| format!("block_{i}"));
        let id = text_of(&b["id"])
            .ok_or_else(|| Error(format!("block `{name}` has no `id` (block type)")))?;
        let params = b["parameters"]
            .as_hash()
            .into_iter()
            .flatten()
            .map(|(k, v)| (scalar(k), scalar(v)))
            .collect();
        let states = &b["states"];
        let state = match &states["state"] {
            Yaml::String(s) if s == "disabled" => State::Disabled,
            Yaml::String(s) if s == "bypassed" => State::Bypassed,
            Yaml::Boolean(false) => State::Disabled,
            _ => State::Enabled,
        };
        let coord = match states["coordinate"].as_vec().map(Vec::as_slice) {
            Some([x, y]) => x
                .as_f64()
                .or(x.as_i64().map(|v| v as f64))
                .zip(y.as_f64().or(y.as_i64().map(|v| v as f64))),
            _ => None,
        };
        blocks.push(GrcBlock {
            name,
            id,
            params,
            state,
            coord,
        });
    }
    let mut connections = Vec::new();
    for c in doc["connections"].as_vec().into_iter().flatten() {
        match c.as_vec().map(Vec::as_slice) {
            Some([a, b, c, d]) => connections.push(GrcConn {
                src: scalar(a),
                src_port: scalar(b),
                dst: scalar(c),
                dst_port: scalar(d),
            }),
            _ => {
                return Err(Error(format!(
                    "connection {c:?} isn't [src, port, dst, port]"
                )));
            }
        }
    }
    Ok(GrcFile {
        title: text_of(&opt["title"]),
        generate: text_of(&opt["generate_options"]),
        blocks,
        connections,
    })
}

// ---- Variables ----------------------------------------------------------------

/// GRC blocks that define a variable, and the parameter holding its value.
fn variable_param(id: &str) -> Option<&'static str> {
    Some(match id {
        "variable"
        | "variable_qtgui_range"
        | "qtgui_range"
        | "variable_qtgui_entry"
        | "qtgui_entry"
        | "variable_qtgui_chooser"
        | "qtgui_chooser"
        | "variable_qtgui_check_box"
        | "qtgui_check_box"
        | "variable_qtgui_push_button"
        | "variable_qtgui_toggle_button_msg"
        | "parameter" => "value",
        _ => return None,
    })
}

/// The flowgraph's variables: their expressions and, where they can be
/// evaluated, values.
pub(crate) struct Vars {
    asts: HashMap<String, Py>,
    values: HashMap<String, Value>,
}

impl Vars {
    fn new(asts: HashMap<String, Py>) -> Self {
        let mut v = Vars {
            asts,
            values: HashMap::new(),
        };
        let names: Vec<String> = v.asts.keys().cloned().collect();
        let mut failed = HashSet::new();
        for n in names {
            v.eval_var(&n, &mut Vec::new(), &mut failed);
        }
        v
    }

    fn eval_var(&mut self, name: &str, stack: &mut Vec<String>, failed: &mut HashSet<String>) {
        if self.values.contains_key(name)
            || failed.contains(name)
            || stack.iter().any(|s| s == name)
        {
            return;
        }
        let Some(ast) = self.asts.get(name).cloned() else {
            return;
        };
        stack.push(name.to_owned());
        for dep in ast.names() {
            self.eval_var(dep, stack, failed);
        }
        stack.pop();
        match py::eval(&ast, &mut |n| self.values.get(n).cloned()) {
            Ok(v) => {
                self.values.insert(name.to_owned(), v);
            }
            Err(_) => {
                failed.insert(name.to_owned());
            }
        }
    }

    /// A variable that becomes a foxygnal graph variable (numbers only).
    pub fn is_numeric(&self, name: &str) -> bool {
        matches!(self.values.get(name), Some(Value::Num(_) | Value::Bool(_)))
    }

    pub fn value(&self, name: &str) -> Option<Value> {
        self.values.get(name).cloned()
    }

    /// The expression of a variable (to look inside, e.g. at the
    /// `firdes.low_pass(…)` a `taps` variable holds).
    pub fn ast(&self, name: &str) -> Option<&Py> {
        self.asts.get(name)
    }

    pub fn eval(&self, e: &Py) -> Result<Value, String> {
        py::eval(e, &mut |n| self.value(n))
    }

    /// Translate a Python expression into a foxygnal param value: a number
    /// when it is constant, else an expression string over graph variables.
    /// The translation must evaluate to Python's value (so `int()` that
    /// truncates, or `//`, falls back to the value); otherwise the value is
    /// used, and when there is none, `Err` says why.
    pub fn number(&self, e: &Py, int: bool) -> Result<(Json, Option<String>), String> {
        let value = self.eval(e).ok().and_then(|v| v.as_f64());
        let as_json = |v: f64| {
            if int {
                json!(v.round() as i64)
            } else {
                json!(v)
            }
        };
        let whole = |v: f64| !int || v.fract() == 0.0;
        match py::translate(e, &|n| self.is_numeric(n)) {
            Ok(text) => {
                let fx = foxygnal_core::expr::Expr::parse(&text)
                    .map_err(|err| format!("`{text}` didn't translate: {err}"))?;
                let back = fx.eval(&mut |n| {
                    self.value(n)
                        .and_then(|v| v.as_f64())
                        .ok_or_else(|| format!("unknown variable `{n}`"))
                });
                match (back, value) {
                    (Ok(b), Some(v)) if close(b, v) && whole(b) => Ok(if fx.vars().is_empty() {
                        (as_json(v), None)
                    } else {
                        (Json::String(text), None)
                    }),
                    (_, Some(v)) => Ok((
                        as_json(v),
                        Some(format!(
                            "`{}` doesn't translate exactly (int(), // or %): kept its value {}",
                            py::show(e),
                            py::fmt_num(v)
                        )),
                    )),
                    (Ok(_), None) | (Err(_), None) => {
                        if fx.vars().is_empty() {
                            Err(format!("`{}` can't be evaluated", py::show(e)))
                        } else {
                            Ok((Json::String(text), None))
                        }
                    }
                }
            }
            Err(why) => match value {
                Some(v) => Ok((
                    as_json(v),
                    Some(format!(
                        "`{}` kept as its value {} ({why})",
                        py::show(e),
                        py::fmt_num(v)
                    )),
                )),
                None => Err(format!("`{}` can't be used: {why}", py::show(e))),
            },
        }
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1e-30)
}

// ---- Import -------------------------------------------------------------------

/// Canvas scale from GRC coordinates (GRC blocks are drawn smaller).
const SCALE: f64 = 1.25;

/// Convert a `.grc` file's text.
pub fn import(text: &str, opts: &Options) -> Result<Import, Error> {
    let file = parse(text)?;
    let mut warnings = Vec::new();
    if file.generate.as_deref() == Some("hb") {
        warnings.push(
            "this .grc is a hierarchical block: its pad_source / pad_sink blocks aren't \
             converted to [export] ports"
                .into(),
        );
    }

    // Variables first: everything else refers to them.
    let mut asts = HashMap::new();
    let mut var_blocks = Vec::new();
    for b in &file.blocks {
        let Some(key) = variable_param(&b.id) else {
            continue;
        };
        if b.state == State::Disabled {
            continue;
        }
        let src = b.params.get(key).map(String::as_str).unwrap_or("");
        match Py::parse(src) {
            Ok(ast) => {
                asts.insert(b.name.clone(), ast);
                var_blocks.push(b);
            }
            Err(e) => warnings.push(format!(
                "variable `{}` ({}): `{src}` isn't an expression the importer understands \
                 ({e}); left out",
                b.name, b.id
            )),
        }
    }
    let vars = Vars::new(asts);
    let mut graph = GraphSpec::default();
    for b in var_blocks {
        let name = &b.name;
        let ast = vars.ast(name).expect("parsed above");
        if !vars.is_numeric(name) {
            // Strings (file names, device args) and filter taps are used
            // where they're referenced; a block that can't use one says so.
            let inlined = matches!(vars.value(name), Some(Value::Str(_)))
                || matches!(ast, Py::Call(f, ..) if f.contains("firdes") || f.contains("window"));
            if inlined {
                continue;
            }
            let what = match vars.value(name) {
                Some(Value::Str(s)) => format!("the string '{s}'"),
                Some(Value::List(_)) => "a list".into(),
                Some(_) => "not a number".into(),
                None => format!("`{}`", py::show(ast)),
            };
            warnings.push(format!(
                "variable `{name}` ({}) is {what}: foxygnal variables are numbers, so it isn't \
                 a graph variable; blocks that use it get its value where possible",
                b.id
            ));
            continue;
        }
        match vars.number(ast, false) {
            Ok((v, note)) => {
                if let Some(n) = note {
                    warnings.push(format!("variable `{name}` ({}): {n}", b.id));
                }
                // Expressions stay text; whole numbers are written as such.
                let v = match v.as_f64() {
                    Some(x) if x.fract() == 0.0 && x.abs() < 1e15 => json!(x as i64),
                    _ => v,
                };
                graph.vars.insert(name.clone(), v);
            }
            Err(e) => warnings.push(format!("variable `{name}` ({}): {e}; left out", b.id)),
        }
    }

    // Blocks.
    let enabled: HashMap<&str, &GrcBlock> = file
        .blocks
        .iter()
        .filter(|b| b.state != State::Disabled)
        .map(|b| (b.name.as_str(), b))
        .collect();
    let disabled: HashSet<&str> = file
        .blocks
        .iter()
        .filter(|b| b.state == State::Disabled)
        .map(|b| b.name.as_str())
        .collect();
    let has_hardware = file
        .blocks
        .iter()
        .any(|b| b.state == State::Enabled && map::is_hardware_source(&b.id));
    let cfg = map::Config {
        audio_device: opts.audio_device,
        has_hardware,
    };

    // Wire-through aliases: a bypassed block's output and a virtual source
    // stand for whatever feeds them.
    let mut alias: HashMap<(String, String), (String, String)> = HashMap::new();
    let mut absorbed: HashSet<&str> = HashSet::new();
    let feeds = |name: &str, port: &str| -> Vec<&GrcConn> {
        file.connections
            .iter()
            .filter(|c| c.dst == name && c.dst_port == port)
            .collect()
    };
    for b in file.blocks.iter().filter(|b| b.state == State::Bypassed) {
        let ins: Vec<&GrcConn> = file
            .connections
            .iter()
            .filter(|c| c.dst == b.name)
            .collect();
        let outs_ok = file
            .connections
            .iter()
            .filter(|c| c.src == b.name)
            .all(|c| c.src_port == "0");
        if ins.len() == 1 && ins[0].dst_port == "0" && outs_ok {
            alias.insert(
                (b.name.clone(), "0".into()),
                (ins[0].src.clone(), ins[0].src_port.clone()),
            );
        } else {
            warnings.push(format!(
                "`{}` ({}) is bypassed but doesn't have exactly one input and one output; \
                 left out like a disabled block",
                b.name, b.id
            ));
        }
        absorbed.insert(&b.name);
    }
    let sinks: HashMap<String, &GrcBlock> = file
        .blocks
        .iter()
        .filter(|b| b.id == "virtual_sink" && b.state != State::Disabled)
        .filter_map(|b| Some((b.params.get("stream_id")?.clone(), b)))
        .collect();
    for b in file
        .blocks
        .iter()
        .filter(|b| matches!(b.id.as_str(), "virtual_source" | "virtual_sink"))
    {
        absorbed.insert(&b.name);
        if b.id != "virtual_source" || b.state == State::Disabled {
            continue;
        }
        let sid = b.params.get("stream_id").cloned().unwrap_or_default();
        match sinks.get(&sid).map(|s| feeds(&s.name, "0")) {
            Some(f) if f.len() == 1 => {
                alias.insert(
                    (b.name.clone(), "0".into()),
                    (f[0].src.clone(), f[0].src_port.clone()),
                );
            }
            _ => warnings.push(format!(
                "virtual source `{}` (stream `{sid}`) has no matching, connected virtual sink; \
                 its connections are left out",
                b.name
            )),
        }
    }

    let mut mapped: HashMap<&str, map::Out> = HashMap::new();
    let mut unmapped: HashSet<&str> = HashSet::new();
    for b in &file.blocks {
        if b.state != State::Enabled
            || variable_param(&b.id).is_some()
            || absorbed.contains(b.name.as_str())
        {
            continue;
        }
        let ports = |side: fn(&GrcConn) -> (&str, &str)| {
            file.connections
                .iter()
                .filter_map(|c| {
                    let (n, p) = side(c);
                    (n == b.name).then(|| p.parse::<usize>().ok()).flatten()
                })
                .collect()
        };
        let used_in = ports(|c| (&c.dst, &c.dst_port));
        let used_out = ports(|c| (&c.src, &c.src_port));
        let mut cx = map::Cx::new(b, &vars, &mut warnings, used_in, used_out);
        match map::map(&mut cx, &cfg) {
            map::Mapped::Nodes(out) => {
                mapped.insert(&b.name, out);
            }
            map::Mapped::Skip => {}
            map::Mapped::Unmapped => {
                warnings.push(format!(
                    "`{}` ({}) has no foxygnal equivalent yet: left out, with its connections",
                    b.name, b.id
                ));
                unmapped.insert(&b.name);
            }
        }
    }

    // Place and collect nodes, in file order.
    for b in &file.blocks {
        let Some(out) = mapped.get_mut(b.name.as_str()) else {
            continue;
        };
        let (x, y) = b.coord.unwrap_or((0.0, 0.0));
        for (k, node) in out.nodes.iter_mut().enumerate() {
            let (dx, dy) = out.offsets.get(k).copied().unwrap_or((0.0, 0.0));
            node.ui
                .insert("x".into(), json!((x * SCALE + dx).round() as i64));
            node.ui
                .insert("y".into(), json!((y * SCALE + dy).round() as i64));
        }
        graph.blocks.append(&mut out.nodes);
        graph.connections.append(&mut out.links);
    }

    // Connections.
    let resolve = |name: &str, port: &str| -> (String, String) {
        let mut cur = (name.to_owned(), port.to_owned());
        for _ in 0..64 {
            match alias.get(&cur) {
                Some(next) => cur = next.clone(),
                None => break,
            }
        }
        cur
    };
    let mut seen = HashSet::new();
    for c in &file.connections {
        if disabled.contains(c.src.as_str()) || disabled.contains(c.dst.as_str()) {
            continue;
        }
        // Connections into a bypassed block or a virtual sink are carried by
        // the alias; those out of them are resolved below.
        if absorbed.contains(c.dst.as_str()) {
            continue;
        }
        let (src, sport) = resolve(&c.src, &c.src_port);
        let label = format!("{}:{} → {}:{}", c.src, c.src_port, c.dst, c.dst_port);
        if absorbed.contains(src.as_str()) || disabled.contains(src.as_str()) {
            // An unresolvable alias was already reported.
            continue;
        }
        let (Ok(sp), Ok(dp)) = (sport.parse::<usize>(), c.dst_port.parse::<usize>()) else {
            warnings.push(format!(
                "message connection {label} left out: foxygnal has no message ports yet"
            ));
            continue;
        };
        if unmapped.contains(src.as_str()) || unmapped.contains(c.dst.as_str()) {
            continue;
        }
        if !enabled.contains_key(src.as_str()) || !enabled.contains_key(c.dst.as_str()) {
            warnings.push(format!(
                "connection {label} names a block that doesn't exist"
            ));
            continue;
        }
        let from = mapped
            .get(src.as_str())
            .and_then(|o| o.outputs.get(sp).cloned().flatten());
        let to = mapped
            .get(c.dst.as_str())
            .and_then(|o| o.inputs.get(dp).cloned())
            .unwrap_or_default();
        let (Some(from), false) = (from, to.is_empty()) else {
            // A side that maps to nothing (a dropped instrument, a variable).
            if mapped.contains_key(src.as_str()) && mapped.contains_key(c.dst.as_str()) {
                warnings.push(format!(
                    "connection {label} left out: that port has no foxygnal equivalent"
                ));
            }
            continue;
        };
        for t in to {
            if seen.insert((from.clone(), t.clone())) {
                graph.connections.push(ConnectionSpec {
                    from: from.clone(),
                    to: t,
                });
            }
        }
    }

    // Inputs left hanging (what fed them was left out) stop the graph from
    // running: say so, rather than let it fail later.
    let fed: HashSet<&str> = graph.connections.iter().map(|c| c.to.as_str()).collect();
    for b in &file.blocks {
        let Some(out) = mapped.get(b.name.as_str()) else {
            continue;
        };
        for ep in out.inputs.iter().flatten() {
            if !fed.contains(ep.as_str()) {
                warnings.push(format!(
                    "`{}` ({}): input `{ep}` isn't connected (what fed it was left out); \
                     connect it before running",
                    b.name, b.id
                ));
            }
        }
    }

    Ok(Import {
        graph,
        warnings,
        title: file.title,
    })
}

/// A new node for a GRC block.
pub(crate) fn node(id: String, kind: &str, params: foxygnal_core::Params) -> BlockSpec {
    BlockSpec {
        id,
        kind: kind.to_owned(),
        params,
        ui: BTreeMap::new(),
    }
}

/// The `.fox.toml` path an imported `.grc` is saved as: same directory and
/// stem (`fm_rx.grc` → `fm_rx.fox.toml`).
pub fn fox_name(grc: &std::path::Path) -> std::path::PathBuf {
    let stem = grc
        .file_stem()
        .map_or_else(|| "imported".into(), |s| s.to_string_lossy().into_owned());
    grc.with_file_name(format!("{stem}.fox.toml"))
}

/// Render an import as `.fox.toml` text with a header comment that names
/// the source and lists the warnings.
pub fn to_toml(imp: &Import, source: &str) -> Result<String, Error> {
    let body = imp.graph.to_toml().map_err(|e| Error(e.to_string()))?;
    let mut out = format!("# Imported from {source} by `foxygnal import`.\n");
    if let Some(t) = &imp.title {
        out.push_str(&format!("# GRC title: {t}\n"));
    }
    if !imp.warnings.is_empty() {
        out.push_str("#\n# Import warnings:\n");
        for w in &imp.warnings {
            out.push_str(&format!("# - {}\n", w.replace('\n', " ")));
        }
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}
