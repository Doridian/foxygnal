//! Export a foxygnal graph as a GNU Radio Companion flowgraph (the YAML
//! `.grc` of GNU Radio 3.10).
//!
//! 1. Hierarchical `graph:` blocks are flattened into their blocks; their
//!    parameters become variables (`low_freq`).
//! 2. Graph variables become `variable` blocks, their expressions
//!    translated to Python (`$samp_rate / 10` → `samp_rate / 10`, `2.4M` →
//!    `2.4e6`).
//! 3. The sample rate on every wire is worked out (GNU Radio blocks are told
//!    their rate; foxygnal's take it from the stream), as a Python
//!    expression over the variables.
//! 4. Each block maps to one or more GNU Radio blocks (`map.rs`, listed in
//!    `docs/GRC_IMPORT.md`). Blocks without an equivalent become a
//!    placeholder Python block with the same ports, which the importer turns
//!    back into the original block.
//!
//! Every approximation is reported; nothing is dropped silently.

mod defs;
mod map;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use foxygnal_core::Params;
use foxygnal_core::graph::{BlockSpec, ConnectionSpec, GraphSpec, parse_endpoint};
use foxygnal_core::resolve::{CONST_KIND, GRAPH_KIND, GraphLoader};
use serde_json::Value as Json;
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter};

use crate::Error;
use crate::py::{self, Py};

pub(crate) use map::{Cx, Out};

/// Export choices.
#[derive(Default)]
pub struct ExportOptions<'a> {
    /// Finds the graph files `graph:` blocks refer to. Without one they are
    /// exported as placeholders.
    pub loader: Option<&'a dyn GraphLoader>,
    /// The flowgraph's name (usually the file name without `.fox.toml`):
    /// the GRC title and, made a Python identifier, its id.
    pub name: Option<String>,
}

/// The result of an export.
#[derive(Debug, Clone)]
pub struct Export {
    /// The `.grc` file (GRC 3.10 YAML).
    pub grc: String,
    /// Everything that didn't translate exactly, one message each, naming
    /// the foxygnal block (id and kind) it is about.
    pub warnings: Vec<String>,
    /// foxygnal block id (flattened: `low/tone`) → GRC block name.
    pub ids: Vec<(String, String)>,
}

/// The `.grc` path an exported graph is written to: same directory and stem
/// (`fm.fox.toml` → `fm.grc`).
pub fn grc_name(fox: &Path) -> PathBuf {
    let file = fox
        .file_name()
        .map_or_else(|| "exported".into(), |s| s.to_string_lossy().into_owned());
    let stem = file
        .strip_suffix(".fox.toml")
        .or_else(|| file.strip_suffix(".toml"))
        .unwrap_or(&file);
    fox.with_file_name(format!("{stem}.grc"))
}

// ---- Names ----------------------------------------------------------------------

/// Names GRC refuses as ids (`grc/core/params/dtypes.py`: Python keywords and
/// builtins, `gr.top_block` methods), plus the modules generated code
/// imports, which a variable of the same name would shadow.
const RESERVED: &str = "\
    False None True and as assert async await break case class continue def del elif else \
    except finally for from global if import in is lambda match nonlocal not or pass raise \
    return try type while with yield _ \
    abs aiter all anext any ascii bin bool breakpoint bytearray bytes callable chr classmethod \
    compile complex copyright credits delattr dict dir divmod enumerate eval exec exit filter \
    float format frozenset getattr globals hasattr hash help hex id input int isinstance \
    issubclass iter len license list locals map max memoryview min next object oct open ord \
    pow print property quit range repr reversed round set setattr slice sorted staticmethod \
    str sum super tuple vars zip Ellipsis NotImplemented \
    alias alias_pmt alias_set all_max_output_buffer_p all_min_output_buffer_p check_topology \
    connect delete_head_nowait disconnect disconnect_all dump edge_list empty_handled_p \
    empty_p erase_msg get_iterator get_msg_map handle_sigint has_msg_port identifier \
    input_signature insert_tail lock log_level logger max_noutput_items max_output_buffer \
    message_port_is_hier message_port_is_hier_in message_port_is_hier_out message_port_pub \
    message_port_register_hier_in message_port_register_hier_out message_port_register_in \
    message_port_register_out message_port_sub message_port_unsub message_ports_in \
    message_ports_out message_subscribers min_output_buffer msg_connect msg_disconnect \
    msg_edge_list name nmsgs output_signature post primitive_connect primitive_disconnect \
    primitive_message_port_register_hier_in primitive_message_port_register_hier_out \
    primitive_msg_connect primitive_msg_disconnect processor_affinity run set_block_alias \
    set_log_level set_max_noutput_items set_max_output_buffer set_min_output_buffer \
    set_processor_affinity setup_rpc start stop symbol_name symbolic_id to_basic_block \
    to_hier_block2 to_top_block unique_id unlock unset_processor_affinity wait \
    self gnuradio gr blocks analog digital audio network osmosdr soapy iio firdes window \
    math cmath numpy np pmt qtgui fft sys signal time options top_block eng_notation \
    ArgumentParser";

/// Hands out unique Python identifiers.
pub(crate) struct Names {
    used: HashSet<String>,
}

impl Names {
    fn new() -> Self {
        Names {
            used: RESERVED.split_whitespace().map(str::to_owned).collect(),
        }
    }

    /// `want` made an identifier GRC accepts (`[A-Za-z]\w*`), unique.
    pub fn take(&mut self, want: &str) -> String {
        let mut base: String = want
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        if !base.starts_with(|c: char| c.is_ascii_alphabetic()) {
            base.insert_str(0, "b_");
        }
        let mut name = base.clone();
        let mut k = 0;
        while self.used.contains(&name) {
            name = format!("{base}_{k}");
            k += 1;
        }
        self.used.insert(name.clone());
        name
    }
}

// ---- Flattening -----------------------------------------------------------------

/// A block of the flattened graph.
pub(crate) struct FBlock {
    /// `low/tone` for blocks from inside the `graph:` block `low`.
    pub id: String,
    pub kind: String,
    pub params: Params,
    /// Index into [`Flat::scopes`]: how its `$name`s are called in Python.
    pub scope: usize,
    pub pos: (f64, f64),
}

struct Flat {
    /// foxygnal variable name → Python name, per graph file.
    scopes: Vec<HashMap<String, String>>,
    /// Variables: Python name, value (or why it didn't translate), and the
    /// foxygnal block it came from (a hierarchical block's parameter).
    vars: Vec<(String, Result<Py, String>, Option<String>)>,
    blocks: Vec<FBlock>,
    /// `block.port` → `block.port`, flattened ids.
    conns: Vec<(String, String)>,
    names: Names,
    warnings: Vec<String>,
}

/// A parameter value (a number or a foxygnal expression) as Python.
fn to_py(v: &Json, scope: &HashMap<String, String>) -> Result<Py, String> {
    match v {
        Json::Number(n) => n
            .as_f64()
            .map(py::num)
            .ok_or_else(|| "not a number".to_owned()),
        Json::Bool(b) => Ok(Py::Bool(*b)),
        Json::String(s) => py::from_fox(s, &|n| scope.get(n).cloned().unwrap_or(n.to_owned())),
        _ => Err("not a number or an expression".into()),
    }
}

type Ports = (HashMap<String, Vec<String>>, HashMap<String, String>);

impl Flat {
    #[allow(clippy::too_many_arguments)]
    fn graph(
        &mut self,
        spec: &GraphSpec,
        prefix: &str,
        origin: (f64, f64),
        mut overrides: HashMap<String, Result<Py, String>>,
        from: Option<&str>,
        loader: Option<&dyn GraphLoader>,
        stack: &mut Vec<String>,
    ) -> Ports {
        let lowered = lower(spec, prefix, &mut self.warnings);
        let spec = lowered.as_ref().unwrap_or(spec);
        // Variables: named after the hierarchical block they belong to.
        let pre = prefix.replace('/', "_");
        let scope: HashMap<String, String> = spec
            .vars
            .keys()
            .map(|v| (v.clone(), self.names.take(&format!("{pre}{v}"))))
            .collect();
        for (v, value) in &spec.vars {
            let val = overrides.remove(v).unwrap_or_else(|| to_py(value, &scope));
            self.vars
                .push((scope[v].clone(), val, from.map(str::to_owned)));
        }
        let scope_id = self.scopes.len();
        self.scopes.push(scope.clone());

        let mut subs: HashMap<&str, Ports> = HashMap::new();
        for b in &spec.blocks {
            let pos = (
                origin.0 + ui_coord(b.ui.get("x")),
                origin.1 + ui_coord(b.ui.get("y")),
            );
            let full = format!("{prefix}{}", b.id);
            let child = b.kind.strip_prefix(GRAPH_KIND).and_then(|file| {
                let why = if stack.iter().any(|f| f == file) || stack.len() >= 16 {
                    format!("`{file}` contains itself")
                } else {
                    match loader.map(|l| l.load(file)) {
                        Some(Ok(g)) => return Some((file, g)),
                        Some(Err(e)) => format!("`{file}` can't be read ({e})"),
                        None => format!("`{file}` can't be opened here"),
                    }
                };
                self.warnings.push(format!(
                    "`{full}` ({}): {why}, so it isn't flattened",
                    b.kind
                ));
                None
            });
            let Some((file, child)) = child else {
                self.blocks.push(FBlock {
                    id: full,
                    kind: b.kind.clone(),
                    params: b.params.clone(),
                    scope: scope_id,
                    pos,
                });
                continue;
            };
            let ov: HashMap<String, Result<Py, String>> = b
                .params
                .iter()
                .filter(|(k, _)| child.vars.contains_key(*k))
                .map(|(k, v)| (k.clone(), to_py(v, &scope)))
                .collect();
            // Inner blocks are laid out from where the block was.
            let min = |axis: &str| {
                child
                    .blocks
                    .iter()
                    .map(|c| ui_coord(c.ui.get(axis)))
                    .fold(f64::INFINITY, f64::min)
            };
            let (mx, my) = (min("x"), min("y"));
            let inner_origin = (
                pos.0 - if mx.is_finite() { mx } else { 0.0 },
                pos.1 - if my.is_finite() { my } else { 0.0 },
            );
            stack.push(file.to_owned());
            let first = self.blocks.len();
            let ports = self.graph(
                &child,
                &format!("{full}/"),
                inner_origin,
                ov,
                Some(&full),
                loader,
                stack,
            );
            stack.pop();
            let inner: Vec<String> = self.blocks[first..]
                .iter()
                .map(|b| b.id.replace('/', "_"))
                .collect();
            self.warnings.push(format!(
                "`{full}` ({}): hierarchical block flattened into its blocks ({}); its \
                 parameters became variables named `{}_…`",
                b.kind,
                inner.join(", "),
                full.replace('/', "_")
            ));
            subs.insert(&b.id, ports);
        }

        let out_of = |ep: &str| -> Option<String> {
            let e = parse_endpoint(ep).ok()?;
            Some(match subs.get(e.block) {
                Some((_, outs)) => outs.get(e.port)?.clone(),
                None => format!("{prefix}{ep}"),
            })
        };
        let into = |ep: &str| -> Vec<String> {
            let Ok(e) = parse_endpoint(ep) else {
                return Vec::new();
            };
            match subs.get(e.block) {
                Some((ins, _)) => ins.get(e.port).cloned().unwrap_or_default(),
                None => vec![format!("{prefix}{ep}")],
            }
        };
        for c in &spec.connections {
            let Some(from) = out_of(&c.from) else {
                self.warnings.push(format!(
                    "connection {} → {} left out: `{}` isn't a port of the hierarchical block",
                    c.from, c.to, c.from
                ));
                continue;
            };
            let to = into(&c.to);
            if to.is_empty() {
                self.warnings.push(format!(
                    "connection {} → {} left out: `{}` isn't a port of the hierarchical block",
                    c.from, c.to, c.to
                ));
            }
            for t in to {
                self.conns.push((from.clone(), t));
            }
        }
        let ins = spec
            .export
            .input
            .iter()
            .map(|i| (i.name.clone(), i.to.iter().flat_map(&into).collect()))
            .collect();
        let outs = spec
            .export
            .output
            .iter()
            .filter_map(|o| Some((o.name.clone(), out_of(&o.from)?)))
            .collect();
        (ins, outs)
    }
}

/// What GNU Radio has no form for, rewritten into what it has: a constant
/// input becomes a `const_source` wired to it (exported as
/// `analog_const_source_x`), and a wire into a setting (`block.@param`) is
/// left out, the setting keeping its starting value. `None` when the graph
/// has neither.
fn lower(spec: &GraphSpec, prefix: &str, warnings: &mut Vec<String>) -> Option<GraphSpec> {
    let wired = |c: &ConnectionSpec| parse_endpoint(&c.to).is_ok_and(|e| e.port.starts_with('@'));
    if spec.blocks.iter().all(|b| b.const_inputs.is_empty()) && !spec.connections.iter().any(wired)
    {
        return None;
    }
    let mut g = spec.clone();
    g.connections.retain(|c| {
        if !wired(c) {
            return true;
        }
        warnings.push(format!(
            "connection {prefix}{} → {prefix}{} left out: GNU Radio can't set a block's \
             parameter from a stream, so it keeps its starting value",
            c.from, c.to
        ));
        false
    });
    let ids: HashSet<String> = g.blocks.iter().map(|b| b.id.clone()).collect();
    let mut added = Vec::new();
    for b in &mut g.blocks {
        for (port, v) in std::mem::take(&mut b.const_inputs) {
            let mut id = format!("{}_const_{port}", b.id);
            while ids.contains(&id) {
                id.push('_');
            }
            let (re, im) = match &v {
                Json::Array(xs) if xs.len() == 2 => (xs[0].clone(), xs[1].clone()),
                v => (v.clone(), Json::from(0)),
            };
            let mut params = Params::new();
            params.insert("value".into(), re);
            params.insert("imag".into(), im);
            let mut ui = b.ui.clone();
            let x = ui_coord(b.ui.get("x")) - 220.0;
            ui.insert("x".into(), Json::from(x));
            added.push((
                BlockSpec {
                    id: id.clone(),
                    kind: CONST_KIND.into(),
                    params,
                    const_inputs: Default::default(),
                    ui,
                },
                ConnectionSpec {
                    from: format!("{id}.out"),
                    to: format!("{}.{port}", b.id),
                },
            ));
        }
    }
    for (b, c) in added {
        g.blocks.push(b);
        g.connections.push(c);
    }
    Some(g)
}

fn ui_coord(v: Option<&Json>) -> f64 {
    v.and_then(Json::as_f64).unwrap_or(0.0)
}

// ---- Export ---------------------------------------------------------------------

/// GRC coordinates are the canvas position / this (the importer multiplies).
const SCALE: f64 = 1.25;

/// Convert a graph to `.grc` text.
pub fn export(spec: &GraphSpec, opts: &ExportOptions<'_>) -> Result<Export, Error> {
    let mut flat = Flat {
        scopes: Vec::new(),
        vars: Vec::new(),
        blocks: Vec::new(),
        conns: Vec::new(),
        names: Names::new(),
        warnings: Vec::new(),
    };
    flat.graph(
        spec,
        "",
        (0.0, 0.0),
        HashMap::new(),
        None,
        opts.loader,
        &mut Vec::new(),
    );
    if spec.export.has_ports() {
        flat.warnings.push(
            "the graph's [export] ports aren't exported (GNU Radio hierarchical blocks use \
             pad_source / pad_sink): it is exported as a top-level flowgraph"
                .into(),
        );
    }
    let mut warnings = std::mem::take(&mut flat.warnings);

    // Variable values, for what must be a number in GNU Radio (resampler
    // ratios, designed IIR taps, port counts).
    let mut values: HashMap<String, f64> = HashMap::new();
    for _ in 0..=flat.vars.len() {
        let mut progress = false;
        for (name, v, _) in &flat.vars {
            if values.contains_key(name) {
                continue;
            }
            if let Ok(e) = v
                && let Ok(x) = py::eval(e, &mut |n| values.get(n).copied().map(py::Value::Num))
                && let Some(x) = x.as_f64()
            {
                values.insert(name.clone(), x);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    // Block names, in file order.
    let gnames: Vec<String> = flat
        .blocks
        .iter()
        .map(|b| flat.names.take(&b.id.replace('/', "_")))
        .collect();
    let index: HashMap<&str, usize> = flat
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.as_str(), i))
        .collect();

    // Wires, by block index.
    let mut wires: Vec<(usize, String, usize, String)> = Vec::new();
    for (from, to) in &flat.conns {
        let (Ok(f), Ok(t)) = (parse_endpoint(from), parse_endpoint(to)) else {
            warnings.push(format!(
                "connection {from} → {to} left out: endpoints look like `block.port`"
            ));
            continue;
        };
        match (index.get(f.block), index.get(t.block)) {
            (Some(&a), Some(&b)) => wires.push((a, f.port.to_owned(), b, t.port.to_owned())),
            _ => warnings.push(format!(
                "connection {from} → {to} left out: it names a block that doesn't exist"
            )),
        }
    }

    // Sample rates, in dependency order.
    let n = flat.blocks.len();
    let order = topo_order(n, &wires);
    let fallback = match flat.scopes.first().and_then(|s| s.get("samp_rate")) {
        Some(name) => Py::Name(name.clone()),
        None => py::num(48000.0),
    };
    let mut rate_out: Vec<Option<Py>> = vec![None; n];
    let mut outs: Vec<Option<Out>> = (0..n).map(|_| None).collect();
    let mut deferred = Vec::new();
    for &i in &order {
        let b = &flat.blocks[i];
        let rate_in = wires
            .iter()
            .filter(|w| w.2 == i)
            .find_map(|w| rate_out[w.0].clone());
        let used_in: BTreeSet<String> = wires
            .iter()
            .filter(|w| w.2 == i)
            .map(|w| w.3.clone())
            .collect();
        let used_out: BTreeSet<String> = wires
            .iter()
            .filter(|w| w.0 == i)
            .map(|w| w.1.clone())
            .collect();
        let mut cx = Cx {
            b,
            scope: &flat.scopes[b.scope],
            values: &values,
            rate_in,
            fallback: &fallback,
            warned_rate: false,
            warnings: &mut warnings,
            used_in,
            used_out,
        };
        rate_out[i] = map::out_rate(&mut cx);
        match map::map(&mut cx) {
            Ok(out) => outs[i] = Some(out),
            Err(why) => deferred.push((i, why, cx.used_in, cx.used_out)),
        }
    }
    // Placeholders last: their port types come from their neighbours.
    for (i, why, used_in, used_out) in deferred {
        let b = &flat.blocks[i];
        let port_type = |port: &str, input: bool| -> Option<&'static str> {
            if let Some(t) = map::known_port(&b.kind, port, input) {
                return Some(t);
            }
            wires.iter().find_map(|w| {
                let (other, oport, mine) = if input {
                    (w.0, &w.1, w.2 == i && w.3 == port)
                } else {
                    (w.2, &w.3, w.0 == i && w.1 == port)
                };
                if !mine {
                    return None;
                }
                match &outs[other] {
                    Some(o) => o.port_type(oport, !input),
                    None => map::known_port(&flat.blocks[other].kind, oport, !input),
                }
            })
        };
        let scope = &flat.scopes[b.scope];
        if b.kind == CONST_KIND {
            // A constant input: typed like the input it feeds.
            let ty = port_type("out", false).unwrap_or("complex");
            outs[i] = Some(map::const_source(b, scope, ty));
            continue;
        }
        let mut ins = map::port_order(&b.kind, true, &used_in);
        let mut outs_p = map::port_order(&b.kind, false, &used_out);
        let fallback_ty =
            map::grc_type(b.params.get("type").and_then(Json::as_str).unwrap_or("c32"))
                .unwrap_or("complex");
        let typed = |ports: &mut Vec<String>, input: bool| -> Vec<(String, &'static str)> {
            ports
                .drain(..)
                .map(|p| {
                    let t = port_type(&p, input).unwrap_or(fallback_ty);
                    (p, t)
                })
                .collect()
        };
        let ins = typed(&mut ins, true);
        let outs_t = typed(&mut outs_p, false);
        outs[i] = Some(map::placeholder(
            b,
            scope,
            &why,
            &ins,
            &outs_t,
            &mut warnings,
        ));
    }

    // Layout: the options block and the variables in rows at the top, the
    // blocks below.
    let per_row = 8usize;
    let rows = flat.vars.len().div_ceil(per_row).max(1);
    let top = 12.0 + 76.0 * rows as f64 + 40.0;
    let min_y = flat
        .blocks
        .iter()
        .map(|b| b.pos.1 / SCALE)
        .fold(f64::INFINITY, f64::min);
    let dy = if min_y.is_finite() && min_y < top {
        top - min_y
    } else {
        0.0
    };
    let min_x = flat
        .blocks
        .iter()
        .map(|b| b.pos.0 / SCALE)
        .fold(f64::INFINITY, f64::min);
    let dx = if min_x.is_finite() && min_x < 8.0 {
        8.0 - min_x
    } else {
        0.0
    };

    let mut blocks_yaml = Vec::new();
    let mut uses_math = false;
    for (k, (name, v, from)) in flat.vars.iter().enumerate() {
        let value = match v {
            Ok(e) => py::to_python(e),
            Err(why) => {
                let what = from
                    .as_deref()
                    .map_or(String::new(), |f| format!(" (a parameter of `{f}`)"));
                warnings.push(format!(
                    "variable `{name}`{what} doesn't translate ({why}); set to 0"
                ));
                "0".into()
            }
        };
        let (row, col) = (k / per_row, k % per_row);
        let mut p = BTreeMap::new();
        p.insert("comment".to_owned(), String::new());
        p.insert("value".to_owned(), value);
        blocks_yaml.push(block_yaml(
            name,
            "variable",
            &p,
            (208.0 + 128.0 * col as f64, 12.0 + 76.0 * row as f64),
        ));
    }

    let mut ids = Vec::new();
    let mut gblock_names: Vec<Vec<String>> = Vec::with_capacity(n);
    let mut gblock_coords: Vec<Vec<(f64, f64)>> = Vec::with_capacity(n);
    let mut coords = Vec::new();
    for (i, b) in flat.blocks.iter().enumerate() {
        let out = outs[i].as_ref().expect("every block is mapped");
        let (x, y) = (b.pos.0 / SCALE + dx, b.pos.1 / SCALE + dy);
        let mut bnames = Vec::new();
        for (k, g) in out.blocks.iter().enumerate() {
            let gname = if k == 0 {
                gnames[i].clone()
            } else {
                flat.names.take(&format!("{}_{}", gnames[i], g.suffix))
            };
            debug_assert!(
                defs::unknown(g.id, g.params.keys()).is_none(),
                "not a verified GNU Radio 3.10 block / parameter: {:?}",
                defs::unknown(g.id, g.params.keys())
            );
            uses_math |= g.params.values().any(|v| v.contains("math."));
            let at = (x + g.offset.0, y + g.offset.1);
            blocks_yaml.push(block_yaml(&gname, g.id, &g.params, at));
            bnames.push(gname);
            coords.push(at);
        }
        ids.push((b.id.clone(), gnames[i].clone()));
        gblock_names.push(bnames);
        gblock_coords.push(std::mem::take(&mut coords));
    }
    if uses_math {
        let name = flat.names.take("import_math");
        let mut p = BTreeMap::new();
        p.insert("imports".to_owned(), "import math".to_owned());
        blocks_yaml.push(block_yaml(
            &name,
            "import_",
            &p,
            (8.0, 12.0 + 76.0 * rows as f64),
        ));
    }

    // Connections.
    let mut conns: Vec<(String, usize, String, usize)> = Vec::new();
    let mut push = |a: &str, ap: usize, b: &str, bp: usize| {
        let c = (a.to_owned(), ap, b.to_owned(), bp);
        if !conns.contains(&c) {
            conns.push(c);
        }
    };
    for (i, out) in outs.iter().enumerate() {
        let out = out.as_ref().expect("mapped");
        for &(a, ap, b, bp) in &out.links {
            push(&gblock_names[i][a], ap, &gblock_names[i][b], bp);
        }
    }
    for (a, ap, b, bp) in &wires {
        let src = outs[*a].as_ref().expect("mapped").find_output(ap);
        let dst = outs[*b].as_ref().expect("mapped").find_input(bp);
        let label = format!("{}.{ap} → {}.{bp}", flat.blocks[*a].id, flat.blocks[*b].id);
        match (src, dst) {
            (Some((sa, sp)), Some(dsts)) if !dsts.is_empty() => {
                for (da, dp) in dsts {
                    push(&gblock_names[*a][sa], sp, &gblock_names[*b][da], dp);
                }
            }
            (None, _) => warnings.push(format!(
                "connection {label} left out: `{}` ({}) has no output `{ap}`",
                flat.blocks[*a].id, flat.blocks[*a].kind
            )),
            _ => warnings.push(format!(
                "connection {label} left out: `{}` ({}) has no input `{bp}`",
                flat.blocks[*b].id, flat.blocks[*b].kind
            )),
        }
    }

    // GRC refuses stream outputs left unconnected (unless optional): end
    // them in Null Sinks.
    for (i, out) in outs.iter().enumerate() {
        let out = out.as_ref().expect("mapped");
        for (blk, port, ty) in out.required_outputs() {
            let src = &gblock_names[i][blk];
            if conns.iter().any(|c| c.0 == *src && c.1 == port) {
                continue;
            }
            let name = flat.names.take(&format!("{src}_null{port}"));
            let mut p = BTreeMap::new();
            for (k, v) in [
                ("type", ty),
                ("num_inputs", "1"),
                ("vlen", "1"),
                ("bus_structure_sink", "[[0,],]"),
            ] {
                p.insert(k.to_owned(), v.to_owned());
            }
            let (x, y) = gblock_coords[i][blk];
            blocks_yaml.push(block_yaml(
                &name,
                "blocks_null_sink",
                &p,
                (x + 200.0, y + 60.0 * port as f64),
            ));
            conns.push((src.clone(), port, name, 0));
        }
    }
    let conns: Vec<Yaml> = conns
        .into_iter()
        .map(|(a, ap, b, bp)| {
            Yaml::Array(vec![
                Yaml::String(a),
                Yaml::String(ap.to_string()),
                Yaml::String(b),
                Yaml::String(bp.to_string()),
            ])
        })
        .collect();

    let title = opts
        .name
        .as_deref()
        .map(|n| n.trim_end_matches(".fox.toml").to_owned())
        .unwrap_or_else(|| "foxygnal graph".into());
    // The flowgraph's id names the generated Python file; blocks keep theirs.
    let fg_id = flat.names.take(
        opts.name
            .as_deref()
            .map(|n| n.trim_end_matches(".fox.toml"))
            .filter(|n| !n.is_empty())
            .unwrap_or("foxygnal_graph"),
    );
    let mut doc = Hash::new();
    doc.insert(ystr("options"), options_yaml(&fg_id, &title));
    doc.insert(ystr("blocks"), Yaml::Array(blocks_yaml));
    doc.insert(ystr("connections"), Yaml::Array(conns));
    let mut meta = Hash::new();
    meta.insert(ystr("file_format"), Yaml::Integer(1));
    doc.insert(ystr("metadata"), Yaml::Hash(meta));

    let mut text = String::new();
    {
        let mut em = YamlEmitter::new(&mut text);
        em.multiline_strings(true);
        em.dump(&Yaml::Hash(doc))
            .map_err(|e| Error(format!("can't write YAML: {e}")))?;
    }
    let mut grc = text
        .strip_prefix("---\n")
        .map(str::to_owned)
        .unwrap_or(text);
    grc.push('\n');
    Ok(Export { grc, warnings, ids })
}

/// Blocks in an order where every block comes after what feeds it (as far
/// as cycles allow), else file order.
fn topo_order(n: usize, wires: &[(usize, String, usize, String)]) -> Vec<usize> {
    let mut indeg = vec![0usize; n];
    for w in wires {
        indeg[w.2] += 1;
    }
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n)
            .find(|&i| !done[i] && indeg[i] == 0)
            .or_else(|| (0..n).find(|&i| !done[i]))
            .expect("some block is left");
        done[next] = true;
        order.push(next);
        for w in wires.iter().filter(|w| w.0 == next) {
            indeg[w.2] = indeg[w.2].saturating_sub(1);
        }
    }
    order
}

fn ystr(s: &str) -> Yaml {
    Yaml::String(s.to_owned())
}

fn states(coord: (f64, f64)) -> Yaml {
    let mut s = Hash::new();
    s.insert(ystr("bus_sink"), Yaml::Boolean(false));
    s.insert(ystr("bus_source"), Yaml::Boolean(false));
    s.insert(ystr("bus_structure"), Yaml::Null);
    s.insert(
        ystr("coordinate"),
        Yaml::Array(vec![
            Yaml::Integer(coord.0.round() as i64),
            Yaml::Integer(coord.1.round() as i64),
        ]),
    );
    s.insert(ystr("rotation"), Yaml::Integer(0));
    s.insert(ystr("state"), ystr("enabled"));
    Yaml::Hash(s)
}

fn block_yaml(name: &str, id: &str, params: &BTreeMap<String, String>, coord: (f64, f64)) -> Yaml {
    let mut b = Hash::new();
    b.insert(ystr("name"), ystr(name));
    b.insert(ystr("id"), ystr(id));
    let mut p = Hash::new();
    for (k, v) in params {
        p.insert(ystr(k), ystr(v));
    }
    b.insert(ystr("parameters"), Yaml::Hash(p));
    b.insert(ystr("states"), states(coord));
    Yaml::Hash(b)
}

fn options_yaml(id: &str, title: &str) -> Yaml {
    let mut p = Hash::new();
    for (k, v) in [
        ("author", ""),
        ("catch_exceptions", "True"),
        ("category", "[GRC Hier Blocks]"),
        ("cmake_opt", ""),
        ("comment", ""),
        ("copyright", ""),
        ("description", "Exported from foxygnal"),
        ("gen_cmake", "On"),
        ("gen_linking", "dynamic"),
        ("generate_options", "no_gui"),
        ("hier_block_src_path", ".:"),
        ("id", id),
        ("max_nouts", "0"),
        ("output_language", "python"),
        ("placement", "(0,0)"),
        ("qt_qss_theme", ""),
        ("realtime_scheduling", ""),
        ("run", "True"),
        ("run_command", "{python} -u {filename}"),
        ("run_options", "prompt"),
        ("sizing_mode", "fixed"),
        ("thread_safe_setters", ""),
        ("title", title),
        ("window_size", "(1000,1000)"),
    ] {
        p.insert(ystr(k), ystr(v));
    }
    let mut o = Hash::new();
    o.insert(ystr("parameters"), Yaml::Hash(p));
    o.insert(ystr("states"), states((8.0, 8.0)));
    Yaml::Hash(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_python_identifiers() {
        let mut n = Names::new();
        assert_eq!(n.take("sum"), "sum_0");
        assert_eq!(n.take("tone-1"), "tone_1");
        assert_eq!(n.take("1st"), "b_1st");
        assert_eq!(n.take("low/tone"), "low_tone");
        assert_eq!(n.take("audio"), "audio_0");
        assert_eq!(n.take("x"), "x");
        assert_eq!(n.take("x"), "x_0");
    }

    #[test]
    fn grc_names() {
        assert_eq!(grc_name(Path::new("a/fm.fox.toml")), Path::new("a/fm.grc"));
        assert_eq!(grc_name(Path::new("x.toml")), Path::new("x.grc"));
    }
}
