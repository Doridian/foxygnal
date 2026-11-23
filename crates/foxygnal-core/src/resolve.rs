//! From the graph as written to the graph the runtime builds: evaluate graph
//! variables and parameter expressions, and flatten hierarchical blocks.
//!
//! - `[vars]` hold numbers or expressions ([`crate::expr`]) and may refer to
//!   each other. Float and int parameters may be expressions too: `"$samp_rate
//!   / 10"`. Strings in other parameter kinds are left alone.
//! - A block of kind `graph:NAME.fox.toml` is another graph file used as a
//!   block. Its params override the child's `[vars]` (evaluated in the
//!   parent's scope), its ports are the child's `[export]`s. It is replaced by
//!   the child's blocks, renamed `parent_id/inner_id`, so the runtime never
//!   sees hierarchy.
//!
//! The result keeps, for each connection, the parent connection it came from,
//! so a server can report wires the way the user drew them.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::expr::Expr;
use crate::graph::{BlockSpec, ConnectionSpec, GraphSpec, parse_endpoint};
use crate::registry::{ParamKind, Registry};
use crate::{Error, StreamType};

/// Block kinds starting with this are graph files: `graph:fm-demod.fox.toml`.
pub const GRAPH_KIND: &str = "graph:";
/// File extension of graph files.
pub const GRAPH_EXT: &str = ".fox.toml";
const MAX_NESTING: usize = 16;

/// Finds the graph files hierarchical blocks refer to.
pub trait GraphLoader {
    fn load(&self, name: &str) -> Result<GraphSpec, Error>;
}

/// For contexts without a project directory: sub-graphs are an error.
pub struct NoLoader;

impl GraphLoader for NoLoader {
    fn load(&self, name: &str) -> Result<GraphSpec, Error> {
        Err(Error::Other(format!(
            "can't open sub-graph `{name}` here (no project directory)"
        )))
    }
}

/// Loads `NAME.fox.toml` files from one directory (plain file names only).
pub struct DirLoader(pub PathBuf);

impl GraphLoader for DirLoader {
    fn load(&self, name: &str) -> Result<GraphSpec, Error> {
        check_graph_name(name)?;
        let text = std::fs::read_to_string(self.0.join(name))
            .map_err(|e| Error::Other(format!("can't read `{name}`: {e}")))?;
        GraphSpec::from_toml(&text).map_err(|e| Error::Other(format!("`{name}`: {e}")))
    }
}

/// A graph file name: no directories, not hidden, ending in `.fox.toml`.
pub fn check_graph_name(name: &str) -> Result<(), Error> {
    let stem = name.strip_suffix(GRAPH_EXT).unwrap_or_default();
    let ok = !stem.is_empty()
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
    if ok {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "`{name}` is not a valid graph file name: use letters, digits, `-`, `_`, `.` and end with {GRAPH_EXT}"
        )))
    }
}

/// A graph ready for [`crate::Flowgraph::from_resolved`].
#[derive(Debug, Clone)]
pub struct Resolved {
    /// No vars, no hierarchical blocks, numeric params evaluated.
    pub graph: GraphSpec,
    /// For each of `graph.connections`, the connection of the top-level graph
    /// it came from (with hierarchical block ports by export name), or `None`
    /// for connections inside a hierarchical block.
    pub origins: Vec<Option<ConnectionSpec>>,
    /// The evaluated `[vars]` of the top-level graph.
    pub vars: Vec<(String, f64)>,
    /// Exported inputs, mapped to `block.port` endpoints of `graph`.
    inputs: Vec<(String, Vec<String>)>,
    /// Exported outputs, mapped to a `block.port` endpoint of `graph`.
    outputs: Vec<(String, String)>,
}

/// Evaluate variables and expressions and flatten hierarchical blocks.
pub fn resolve(
    spec: &GraphSpec,
    registry: &Registry,
    loader: &dyn GraphLoader,
) -> Result<Resolved, Error> {
    resolve_inner(spec, &HashMap::new(), registry, loader, &mut Vec::new())
}

/// The top-level id of a flattened block id and the rest:
/// `fm/lp` → (`fm`, Some(`lp`)), `src` → (`src`, None).
pub fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once('/') {
        Some((top, inner)) => (top, Some(inner)),
        None => (id, None),
    }
}

fn resolve_inner(
    spec: &GraphSpec,
    overrides: &HashMap<String, f64>,
    registry: &Registry,
    loader: &dyn GraphLoader,
    stack: &mut Vec<String>,
) -> Result<Resolved, Error> {
    let vars = eval_vars(&spec.vars, overrides)?;
    let scope: HashMap<&str, f64> = vars.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    let mut graph = GraphSpec::default();
    let mut origins = Vec::new();
    // Hierarchical blocks of this graph: id → their exports, flattened.
    let mut subs: HashMap<&str, Resolved> = HashMap::new();

    for b in &spec.blocks {
        if b.id.contains('/') {
            return Err(Error::Graph(format!(
                "block id `{}` must not contain `/`",
                b.id
            )));
        }
        let Some(file) = b.kind.strip_prefix(GRAPH_KIND) else {
            graph.blocks.push(eval_block(b, &scope, registry)?);
            continue;
        };
        let in_block = |e: Error| Error::Block {
            block: b.id.clone(),
            source: Box::new(e),
        };
        if stack.iter().any(|f| f == file) || stack.len() >= MAX_NESTING {
            let mut chain = stack.clone();
            chain.push(file.to_owned());
            return Err(in_block(Error::Graph(format!(
                "`{file}` contains itself: {}",
                chain.join(" → ")
            ))));
        }
        let child = loader.load(file).map_err(in_block)?;
        // Params override the child's vars, evaluated here. Params the child
        // doesn't have are ignored, as for any other block.
        let mut ov = HashMap::new();
        for (name, v) in &b.params {
            if !child.vars.contains_key(name) {
                continue;
            }
            let x = eval_value(v, &scope).map_err(|msg| Error::Param {
                block: b.id.clone(),
                param: name.clone(),
                msg,
            })?;
            ov.insert(name.clone(), x);
        }
        stack.push(file.to_owned());
        let r = resolve_inner(&child, &ov, registry, loader, stack);
        stack.pop();
        let r = r.map_err(|e| prefix_error(e, &b.id))?;
        let p = |ep: &str| format!("{}/{ep}", b.id);
        for mut ib in r.graph.blocks.iter().cloned() {
            ib.id = p(&ib.id);
            graph.blocks.push(ib);
        }
        for c in &r.graph.connections {
            graph.connections.push(ConnectionSpec {
                from: p(&c.from),
                to: p(&c.to),
            });
            origins.push(None);
        }
        let r = Resolved {
            inputs: r
                .inputs
                .into_iter()
                .map(|(n, eps)| (n, eps.iter().map(|e| p(e)).collect()))
                .collect(),
            outputs: r.outputs.into_iter().map(|(n, e)| (n, p(&e))).collect(),
            graph: GraphSpec::default(),
            origins: Vec::new(),
            vars: Vec::new(),
        };
        subs.insert(&b.id, r);
    }

    for c in &spec.connections {
        let edge_err = |msg: String| Error::Edge {
            from: c.from.clone(),
            to: c.to.clone(),
            msg,
        };
        let (f, t) = c.endpoints().map_err(|e| edge_err(e.to_string()))?;
        let (from, origin_from) = match subs.get(f.block) {
            Some(sub) => {
                let (name, ep) = find_port(&sub.outputs, f.port)
                    .ok_or_else(|| edge_err(format!("`{}` has no output `{}`", f.block, f.port)))?;
                (ep.clone(), format!("{}.{name}", f.block))
            }
            None => (c.from.clone(), c.from.clone()),
        };
        let (tos, origin_to) = match subs.get(t.block) {
            Some(sub) => {
                let (name, eps) = find_port(&sub.inputs, t.port)
                    .ok_or_else(|| edge_err(format!("`{}` has no input `{}`", t.block, t.port)))?;
                (eps.clone(), format!("{}.{name}", t.block))
            }
            None => (vec![c.to.clone()], c.to.clone()),
        };
        for to in tos {
            graph.connections.push(ConnectionSpec {
                from: from.clone(),
                to,
            });
            origins.push(Some(ConnectionSpec {
                from: origin_from.clone(),
                to: origin_to.clone(),
            }));
        }
    }

    // This graph's own exports, in terms of the flattened blocks.
    let mut names = HashSet::new();
    let check = |kind: &str, name: &str, ep: &str| -> Result<(), Error> {
        let msg = |m: String| Error::Graph(format!("exported {kind} `{name}`: {m}"));
        if name.is_empty() || name.contains(['.', '/']) {
            return Err(msg("give it a name without `.` or `/`".into()));
        }
        let e = parse_endpoint(ep).map_err(|e| msg(e.to_string()))?;
        if !spec.blocks.iter().any(|b| b.id == e.block) {
            return Err(msg(format!("there is no block `{}`", e.block)));
        }
        Ok(())
    };
    let mut inputs = Vec::new();
    for x in &spec.export.input {
        let mut eps = Vec::new();
        for ep in x.to.iter() {
            check("input", &x.name, ep)?;
            let e = parse_endpoint(ep)?;
            match subs.get(e.block) {
                Some(sub) => {
                    let (_, inner) = find_port(&sub.inputs, e.port).ok_or_else(|| {
                        Error::Graph(format!(
                            "exported input `{}`: `{}` has no input `{}`",
                            x.name, e.block, e.port
                        ))
                    })?;
                    eps.extend(inner.iter().cloned());
                }
                None => eps.push(ep.to_owned()),
            }
        }
        if !names.insert(("in", x.name.as_str())) {
            return Err(Error::Graph(format!(
                "input `{}` is exported twice",
                x.name
            )));
        }
        inputs.push((x.name.clone(), eps));
    }
    let mut outputs = Vec::new();
    for x in &spec.export.output {
        check("output", &x.name, &x.from)?;
        let e = parse_endpoint(&x.from)?;
        let ep = match subs.get(e.block) {
            Some(sub) => find_port(&sub.outputs, e.port)
                .ok_or_else(|| {
                    Error::Graph(format!(
                        "exported output `{}`: `{}` has no output `{}`",
                        x.name, e.block, e.port
                    ))
                })?
                .1
                .clone(),
            None => x.from.clone(),
        };
        if !names.insert(("out", x.name.as_str())) {
            return Err(Error::Graph(format!(
                "output `{}` is exported twice",
                x.name
            )));
        }
        outputs.push((x.name.clone(), ep));
    }

    Ok(Resolved {
        graph,
        origins,
        vars,
        inputs,
        outputs,
    })
}

/// An exported port by name, or by index like any other port.
fn find_port<'a, T>(ports: &'a [(String, T)], port: &str) -> Option<(&'a str, &'a T)> {
    ports
        .iter()
        .find(|(n, _)| n == port)
        .or_else(|| ports.get(port.parse::<usize>().ok()?))
        .map(|(n, t)| (n.as_str(), t))
}

/// Errors from inside hierarchical block `id` name the inner blocks as
/// `id/inner`.
fn prefix_error(e: Error, id: &str) -> Error {
    let p = |s: &str| format!("{id}/{s}");
    match e {
        Error::Param { block, param, msg } => Error::Param {
            block: p(&block),
            param,
            msg,
        },
        Error::Block { block, source } => Error::Block {
            block: p(&block),
            source,
        },
        Error::Edge { from, to, msg } => Error::Edge {
            from: p(&from),
            to: p(&to),
            msg,
        },
        e => Error::Block {
            block: id.to_owned(),
            source: Box::new(e),
        },
    }
}

/// A block with its float and int params evaluated.
fn eval_block(
    b: &BlockSpec,
    scope: &HashMap<&str, f64>,
    registry: &Registry,
) -> Result<BlockSpec, Error> {
    let mut out = BlockSpec {
        id: b.id.clone(),
        kind: b.kind.clone(),
        params: b.params.clone(),
        ui: Default::default(),
    };
    let Some(desc) = registry.get(&b.kind) else {
        // `from_resolved` reports the unknown kind.
        return Ok(out);
    };
    for spec in &desc.params {
        let int = match spec.kind {
            ParamKind::Float { .. } => false,
            ParamKind::Int { .. } => true,
            _ => continue,
        };
        let Some(Value::String(text)) = b.params.get(spec.name) else {
            continue;
        };
        let err = |msg: String| Error::Param {
            block: b.id.clone(),
            param: spec.name.to_owned(),
            msg,
        };
        let x = eval_str(text, scope).map_err(err)?;
        let v = if int {
            if x.fract() != 0.0 || x.abs() > 9.0e15 {
                return Err(err(format!("`{text}` is {x}, not a whole number")));
            }
            Value::from(x as i64)
        } else {
            Value::from(x)
        };
        out.params.insert(spec.name.to_owned(), v);
    }
    Ok(out)
}

fn eval_str(text: &str, scope: &HashMap<&str, f64>) -> Result<f64, String> {
    crate::expr::eval(text, &mut |name| {
        scope
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown variable `${name}`"))
    })
}

/// A param value used as a number: a JSON number or an expression.
fn eval_value(v: &Value, scope: &HashMap<&str, f64>) -> Result<f64, String> {
    match v {
        Value::Number(n) => n.as_f64().ok_or_else(|| format!("invalid number {n}")),
        Value::String(s) => eval_str(s, scope),
        v => Err(format!("expected a number or an expression, got {v}")),
    }
}

/// Evaluate `[vars]`, with `overrides` (already numbers) replacing some.
/// Variables may refer to each other in any order; loops are an error.
pub fn eval_vars(
    vars: &crate::Params,
    overrides: &HashMap<String, f64>,
) -> Result<Vec<(String, f64)>, Error> {
    let var_err = |name: &str, msg: String| Error::Graph(format!("variable `{name}`: {msg}"));
    let mut defs: HashMap<&str, Expr> = HashMap::new();
    for (name, v) in vars {
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(Error::Graph(format!(
                "`{name}` is not a valid variable name: use letters, digits and `_`"
            )));
        }
        let e = match (overrides.get(name), v) {
            (Some(x), _) => Expr::Num(*x),
            (None, Value::Number(n)) => Expr::Num(n.as_f64().unwrap_or(f64::NAN)),
            (None, Value::String(s)) => Expr::parse(s).map_err(|m| var_err(name, m))?,
            (None, v) => {
                return Err(var_err(
                    name,
                    format!("must be a number or an expression, not {v}"),
                ));
            }
        };
        defs.insert(name, e);
    }

    fn get(
        name: &str,
        defs: &HashMap<&str, Expr>,
        done: &mut HashMap<String, f64>,
        stack: &mut Vec<String>,
    ) -> Result<f64, Error> {
        if let Some(v) = done.get(name) {
            return Ok(*v);
        }
        let e = &defs[name];
        if let Some(i) = stack.iter().position(|s| s == name) {
            let mut chain: Vec<String> = stack[i..].iter().map(|s| format!("`{s}`")).collect();
            chain.push(format!("`{name}`"));
            return Err(Error::Graph(format!(
                "variable `{name}` depends on itself: {}",
                chain.join(" → ")
            )));
        }
        stack.push(name.to_owned());
        let mut nested = None;
        let r = e.eval(&mut |dep| {
            if !defs.contains_key(dep) {
                return Err(format!("unknown variable `${dep}`"));
            }
            get(dep, defs, done, stack).map_err(|e| {
                nested = Some(e);
                String::new()
            })
        });
        stack.pop();
        let v = match (r, nested) {
            (_, Some(e)) => return Err(e),
            (Err(msg), None) => return Err(Error::Graph(format!("variable `{name}`: {msg}"))),
            (Ok(v), None) => v,
        };
        done.insert(name.to_owned(), v);
        Ok(v)
    }

    let mut done = HashMap::new();
    let mut out = Vec::with_capacity(vars.len());
    for name in vars.keys() {
        let v = get(name, &defs, &mut done, &mut Vec::new())?;
        out.push((name.clone(), v));
    }
    Ok(out)
}

/// A changed parameter between two resolved graphs, see [`diff_params`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParamChange {
    pub block: String,
    pub param: &'static str,
    pub value: Value,
    /// The parameter can change while running.
    pub live: bool,
}

/// The parameter changes from `old` to `new` (both resolved), or `None` if
/// blocks or connections differ (or a block kind is unknown).
pub fn diff_params(
    registry: &Registry,
    old: &GraphSpec,
    new: &GraphSpec,
) -> Option<Vec<ParamChange>> {
    type Topology<'a> = (BTreeSet<(&'a str, &'a str)>, BTreeSet<(&'a str, &'a str)>);
    fn topology(g: &GraphSpec) -> Topology<'_> {
        (
            g.blocks
                .iter()
                .map(|b| (b.id.as_str(), b.kind.as_str()))
                .collect(),
            g.connections
                .iter()
                .map(|c| (c.from.as_str(), c.to.as_str()))
                .collect(),
        )
    }
    if topology(old) != topology(new) {
        return None;
    }
    let mut changes = Vec::new();
    for nb in &new.blocks {
        let ob = old.blocks.iter().find(|b| b.id == nb.id)?;
        let desc = registry.get(&nb.kind)?;
        for spec in &desc.params {
            let o = ob.params.get(spec.name).unwrap_or(&spec.default);
            let n = nb.params.get(spec.name).unwrap_or(&spec.default);
            if o != n {
                changes.push(ParamChange {
                    block: nb.id.clone(),
                    param: spec.name,
                    value: n.clone(),
                    live: spec.live && spec.check(n).is_ok(),
                });
            }
        }
    }
    Some(changes)
}

// ---- Graph files as blocks -------------------------------------------------

/// What the palette shows for a graph file used as a block. Serializes like
/// a [`crate::BlockDescriptor`].
#[derive(Debug, Clone, Serialize)]
pub struct GraphDescriptor {
    pub kind: String,
    pub label: String,
    pub category: &'static str,
    pub help: String,
    pub inputs: Vec<GraphPort>,
    pub outputs: Vec<GraphPort>,
    pub params: Vec<GraphParam>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphPort {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: StreamType,
}

/// A variable of the graph, as a float parameter of the block.
#[derive(Debug, Clone, Serialize)]
pub struct GraphParam {
    pub name: String,
    pub label: String,
    /// Always `"float"`.
    pub kind: &'static str,
    pub default: f64,
    pub help: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
}

/// Describe graph file `name` as a block, or `None` if it exports no ports.
///
/// Its params are the variables that don't refer to other variables (the
/// others are derived and stay internal, though a parent may still override
/// them by name). A param is `live` if changing it only changes live params
/// inside. Port types come from the inner blocks.
pub fn describe(
    name: &str,
    registry: &Registry,
    loader: &dyn GraphLoader,
) -> Result<Option<GraphDescriptor>, Error> {
    let spec = loader.load(name)?;
    if !spec.export.has_ports() {
        return Ok(None);
    }
    let mut stack = vec![name.to_owned()];
    let base = resolve_inner(&spec, &HashMap::new(), registry, loader, &mut stack)?;

    let port_type = |ep: &str, input: bool| -> Result<StreamType, Error> {
        let e = parse_endpoint(ep)?;
        let b = base
            .graph
            .blocks
            .iter()
            .find(|b| b.id == e.block)
            .ok_or_else(|| Error::Graph(format!("no block `{}`", e.block)))?;
        let inst = registry.build(&b.kind, &b.params)?;
        let ports = if input { &inst.inputs } else { &inst.outputs };
        ports
            .iter()
            .find(|(n, _)| n == e.port)
            .or_else(|| ports.get(e.port.parse::<usize>().ok()?))
            .map(|(_, t)| *t)
            .ok_or_else(|| Error::Graph(format!("block `{}` has no port `{}`", e.block, e.port)))
    };
    let mut inputs = Vec::new();
    for (n, eps) in &base.inputs {
        let ep = eps
            .first()
            .ok_or_else(|| Error::Graph(format!("exported input `{n}` goes nowhere")))?;
        inputs.push(GraphPort {
            name: n.clone(),
            ty: port_type(ep, true)?,
        });
    }
    let mut outputs = Vec::new();
    for (n, ep) in &base.outputs {
        outputs.push(GraphPort {
            name: n.clone(),
            ty: port_type(ep, false)?,
        });
    }

    let mut params = Vec::new();
    for (var, default) in &base.vars {
        let derived = match &spec.vars[var] {
            Value::String(s) => Expr::parse(s).is_ok_and(|e| !e.vars().is_empty()),
            _ => false,
        };
        if derived {
            continue;
        }
        // Live if a different value only changes live params.
        let probe = HashMap::from([(var.clone(), default * 1.5 + 1.0)]);
        let live = resolve_inner(&spec, &probe, registry, loader, &mut stack)
            .ok()
            .and_then(|r| diff_params(registry, &base.graph, &r.graph))
            .is_some_and(|ch| ch.iter().all(|c| c.live));
        let text = match &spec.vars[var] {
            Value::String(s) => format!(" (`{s}` in the file)"),
            _ => String::new(),
        };
        params.push(GraphParam {
            name: var.clone(),
            label: var.clone(),
            kind: "float",
            default: *default,
            help: format!("Variable `{var}` of {name}{text}."),
            live,
        });
    }

    let stem = name.strip_suffix(GRAPH_EXT).unwrap_or(name);
    Ok(Some(GraphDescriptor {
        kind: format!("{GRAPH_KIND}{name}"),
        label: spec.export.label.clone().unwrap_or_else(|| stem.to_owned()),
        category: "Graphs",
        help: spec.export.help.clone().unwrap_or_else(|| {
            format!("The graph {name}, used as a block. Double-click to open it.")
        }),
        inputs,
        outputs,
        params,
    }))
}
