//! The MCP tools: the block catalog, graph editing (checked like the
//! editor checks), project files, running, and looking at signals.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use foxygnal_core::graph::{BlockSpec, ConnectionSpec};
use foxygnal_core::{GraphSpec, ParamKind, Params, StreamType};
use serde_json::{Map, Value, json};

use super::Mcp;
use super::probes::{Collected, MCP_CLIENT, Picture};
use crate::api::{EXAMPLES, EXT, GRC_EXT, project_path};
use crate::probe::ProbeConfig;
use crate::session::{EdgeRef, Meta};

/// Who MCP's edits are attributed to in graph events.
const BY: &str = "assistant";

// ---- Results ----------------------------------------------------------------------

/// A tool's result: content blocks for the model, optional structured JSON.
pub struct ToolOut {
    content: Vec<Value>,
    structured: Option<Value>,
    error: bool,
}

impl ToolOut {
    /// A JSON result, shown to the model as pretty-printed text too.
    /// Structured content must be an object: anything else is wrapped as
    /// `{"result": …}`.
    fn json(v: Value) -> Self {
        let text = serde_json::to_string_pretty(&v).unwrap_or_default();
        let structured = if v.is_object() {
            v
        } else {
            json!({ "result": v })
        };
        Self {
            content: vec![json!({"type": "text", "text": text})],
            structured: Some(structured),
            error: false,
        }
    }

    fn text(t: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": t.into()})],
            structured: None,
            error: false,
        }
    }

    fn error(msg: impl Into<String>) -> Self {
        Self {
            error: true,
            ..Self::text(msg)
        }
    }

    fn with_png(mut self, png: &[u8]) -> Self {
        self.content.push(json!({
            "type": "image",
            "mimeType": "image/png",
            "data": base64(png),
        }));
        self
    }

    pub fn into_json(self) -> Value {
        let mut v = json!({"content": self.content, "isError": self.error});
        if let Some(s) = self.structured {
            v["structuredContent"] = s;
        }
        v
    }
}

type Out = Result<ToolOut, String>;

// ---- The list ---------------------------------------------------------------------

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// Changes nothing.
    read_only: bool,
    schema: fn() -> Value,
}

fn no_args() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn wire_prop() -> Value {
    json!({
        "type": "string",
        "description": "A wire: `from_block.port->to_block.port`, or just the output `block.port` (its first wire).",
    })
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "list_blocks",
        title: "List blocks",
        description: "The block kinds you can use: kind, label, category and a one-line summary. Filter by words (matched against kind, label, help and keywords) and/or a category.",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "Words to look for, e.g. \"fm demod\", \"low pass\", \"decoder\"."},
                "category": {"type": "string", "description": "Only this category, e.g. \"Filters\", \"Digital\", \"Sources\"."},
            }})
        },
    },
    Tool {
        name: "describe_block",
        title: "Describe a block",
        description: "Everything about one block kind: help, input and output ports with their stream types, and every setting (type, unit, range, default, whether it is live).",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "kind": {"type": "string", "description": "The block kind, e.g. \"fm_demod\"."},
            }, "required": ["kind"]})
        },
    },
    Tool {
        name: "list_examples",
        title: "List example graphs",
        description: "The bundled example graphs and templates (FM radio, ADS-B, POCSAG, APRS, SSTV, …): name, title and what each does.",
        read_only: true,
        schema: no_args,
    },
    Tool {
        name: "get_example",
        title: "Get an example graph",
        description: "An example graph as .fox.toml. With open=true it also replaces the open graph (like picking a template in the browser).",
        read_only: false,
        schema: || {
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "The example's name from list_examples."},
                "open": {"type": "boolean", "description": "Make it the open graph.", "default": false},
            }, "required": ["name"]})
        },
    },
    Tool {
        name: "get_graph",
        title: "Get the open graph",
        description: "The graph being edited, as .fox.toml, with a check report: whether it builds (or the first error, naming the block or both ends of the wire), the sample rate and centre frequency on every wire, unconnected inputs, and the run state.",
        read_only: true,
        schema: no_args,
    },
    Tool {
        name: "set_graph",
        title: "Replace the open graph",
        description: "Replace the open graph with a .fox.toml text. Returns the same check report as get_graph. A person watching in the browser sees the change and can undo it.",
        read_only: false,
        schema: || {
            json!({"type": "object", "properties": {
                "toml": {"type": "string", "description": "The whole graph as .fox.toml."},
                "name": {"type": "string", "description": "A file name for it (`name.fox.toml`), shown in the editor. Doesn't save; see save_file."},
            }, "required": ["toml"]})
        },
    },
    Tool {
        name: "check_graph",
        title: "Check a graph",
        description: "Check a .fox.toml text without opening it: the same report as get_graph.",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "toml": {"type": "string", "description": "A graph as .fox.toml."},
            }, "required": ["toml"]})
        },
    },
    Tool {
        name: "edit_graph",
        title: "Edit the open graph",
        description: "Apply edits to the open graph, in order, all or nothing. Operations (field `op`): \
`add_block` {id, kind, params?, const_inputs?, x?, y?}; `remove_block` {id} (and its wires); \
`set_params` {id, params} (a null value resets that setting to its default; live settings change a running graph at once); \
`connect` {from, to, convert?} (`block.port`; the port may be left out if the block has only one; if the two ends' stream types differ and exactly one converter block fits, it is inserted between them unless convert=false, and the reply's `notes` say so or list the choices); `disconnect` {from, to}; \
`set_const_input` {id, input, value} (null removes it); `set_variable` {name, value} (null removes it). \
Returns the check report. While running, a change to anything but live settings applies on the next run (`stale`).",
        read_only: false,
        schema: || {
            json!({"type": "object", "properties": {
                "operations": {"type": "array", "items": {"type": "object", "properties": {
                    "op": {"type": "string", "enum": ["add_block", "remove_block", "set_params", "connect", "disconnect", "set_const_input", "set_variable"]},
                }, "required": ["op"]}},
            }, "required": ["operations"]})
        },
    },
    Tool {
        name: "list_files",
        title: "List project files",
        description: "Graph files (.fox.toml, and GNU Radio .grc) in the project directory, newest first.",
        read_only: true,
        schema: no_args,
    },
    Tool {
        name: "open_file",
        title: "Open a project file",
        description: "Open a graph file from the project directory as the open graph. A .grc file is imported from GNU Radio (warnings say what didn't map).",
        read_only: false,
        schema: || {
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "A file name from list_files."},
            }, "required": ["name"]})
        },
    },
    Tool {
        name: "save_file",
        title: "Save the open graph",
        description: "Save the open graph in the project directory as `name.fox.toml`.",
        read_only: false,
        schema: || {
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "File name ending in .fox.toml."},
                "overwrite": {"type": "boolean", "description": "Replace an existing file.", "default": false},
            }, "required": ["name"]})
        },
    },
    Tool {
        name: "import_grc",
        title: "Import a GNU Radio flowgraph",
        description: "Convert a GNU Radio Companion flowgraph (.grc text, 3.8+ YAML or 3.7 XML) to .fox.toml. Doesn't open it; pass the result to set_graph.",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "grc": {"type": "string", "description": "The .grc file's text."},
            }, "required": ["grc"]})
        },
    },
    Tool {
        name: "export_grc",
        title: "Export to GNU Radio",
        description: "The open graph as a GNU Radio Companion 3.10 flowgraph (.grc YAML), with warnings for what has no exact equivalent.",
        read_only: true,
        schema: no_args,
    },
    Tool {
        name: "run",
        title: "Run the graph",
        description: "Start the open graph (restarts it if running). Returns the run state and any errors a moment after starting. Graphs that transmit are refused.",
        read_only: false,
        schema: no_args,
    },
    Tool {
        name: "stop",
        title: "Stop the graph",
        description: "Stop the running graph.",
        read_only: false,
        schema: no_args,
    },
    Tool {
        name: "status",
        title: "Run status",
        description: "Run state, errors and warnings (pinned to blocks), whether edits are waiting for a restart, and while running: each block's CPU use and each wire's throughput and buffer fill (measured over half a second).",
        read_only: true,
        schema: no_args,
    },
    Tool {
        name: "measure",
        title: "Measure a wire",
        description: "Watch a wire of the running graph for a while and summarize it. \
`spectrum`: noise floor, the signals standing out of it (strongest point and power-weighted centre frequency, bandwidth, level in dBFS, height above the noise) and a coarse outline of the whole spectrum; frequencies are absolute when the wire knows its centre frequency. \
`level`: RMS, peak and DC in dBFS. \
`constellation` (c32 wires): envelope spread, a PSK order estimate (BPSK/QPSK/8PSK from the M-th power test) with EVM and MER, DC offset, and the symbol rate from the spectral line of |x|² (needs more than one sample per symbol, i.e. before clock recovery). \
`messages`: the text lines a decoder wrote (u8 wires), including recent ones from before the call.",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "wire": wire_prop(),
                "kind": {"type": "string", "enum": ["spectrum", "level", "constellation", "messages"]},
                "seconds": {"type": "number", "description": "How long to watch (0.1–30).", "default": 1.0},
                "fft_size": {"type": "integer", "description": "Spectrum resolution: FFT size, a power of two (64–8192).", "default": 4096},
                "signals": {"type": "integer", "description": "Spectrum: at most this many signals (the strongest).", "default": 8},
                "min_snr_db": {"type": "number", "description": "Spectrum: signals whose strongest point is at least this far above the noise floor.", "default": 10.0},
                "max_lines": {"type": "integer", "description": "Messages: at most this many (the newest).", "default": 200},
            }, "required": ["wire", "kind"]})
        },
    },
    Tool {
        name: "watch",
        title: "Start watching a wire",
        description: "Keep collecting a decoder's output while the graph runs (across restarts), for read_watch: `messages` (text lines) or `image` (pictures, e.g. an SSTV decoder's `image` output). Returns a watch id.",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "wire": wire_prop(),
                "kind": {"type": "string", "enum": ["messages", "image"]},
            }, "required": ["wire", "kind"]})
        },
    },
    Tool {
        name: "read_watch",
        title: "Read a watch",
        description: "What a watch collected: for messages, the lines since the last read; for image, the newest picture (in progress or finished) as a PNG, and a list of the pictures kept.",
        read_only: true,
        schema: || {
            json!({"type": "object", "properties": {
                "watch_id": {"type": "integer"},
                "max_lines": {"type": "integer", "default": 500},
                "picture": {"type": "integer", "description": "Image: which kept picture (its number) instead of the newest."},
            }, "required": ["watch_id"]})
        },
    },
    Tool {
        name: "stop_watch",
        title: "Stop a watch",
        description: "Stop collecting for a watch and forget what it collected.",
        read_only: true,
        schema: || json!({"type": "object", "properties": {"watch_id": {"type": "integer"}}, "required": ["watch_id"]}),
    },
    Tool {
        name: "list_devices",
        title: "List SDR devices",
        description: "SDR receivers (and transmitters) found by each driver, with the device strings to put in an SDR Source's `device` setting. `sim:` (a simulated FM band) is always there.",
        read_only: true,
        schema: no_args,
    },
    Tool {
        name: "record",
        title: "Record a wire",
        description: "Record a wire of the running graph to a SigMF recording in the project's recordings/ directory, for a number of seconds; returns its path (usable as a SigMF Source's `path`) and size.",
        read_only: false,
        schema: || {
            json!({"type": "object", "properties": {
                "wire": wire_prop(),
                "seconds": {"type": "number", "description": "0.1–600.", "default": 5.0},
                "name": {"type": "string", "description": "Recording name (default: from the time and wire)."},
            }, "required": ["wire"]})
        },
    },
];

pub fn list() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": {
                    "title": t.title,
                    "readOnlyHint": t.read_only,
                    "destructiveHint": false,
                    "openWorldHint": false,
                },
            })
        })
        .collect()
}

pub fn exists(name: &str) -> bool {
    TOOLS.iter().any(|t| t.name == name)
}

pub async fn call(mcp: &Mcp, name: &str, args: &Value) -> ToolOut {
    let r = match name {
        "list_blocks" => list_blocks(mcp, args),
        "describe_block" => describe_block(mcp, args),
        "list_examples" => Ok(ToolOut::json(json!(
            EXAMPLES
                .iter()
                .map(|e| json!({"name": e.name, "title": e.title, "description": e.description}))
                .collect::<Vec<_>>()
        ))),
        "get_example" => get_example(mcp, args),
        "get_graph" => Ok(graph_out(mcp, &mcp.session.graph(), None)),
        "set_graph" => set_graph(mcp, args),
        "check_graph" => check_graph(mcp, args),
        "edit_graph" => edit_graph(mcp, args),
        "list_files" => list_files(mcp),
        "open_file" => open_file(mcp, args),
        "save_file" => save_file(mcp, args),
        "import_grc" => import_grc(args),
        "export_grc" => export_grc(mcp),
        "run" => run(mcp).await,
        "stop" => {
            mcp.session.stop();
            Ok(ToolOut::json(json!({"state": "stopped"})))
        }
        "status" => status(mcp).await,
        "measure" => measure(mcp, args).await,
        "watch" => watch(mcp, args),
        "read_watch" => read_watch(mcp, args),
        "stop_watch" => stop_watch(mcp, args),
        "list_devices" => Ok(ToolOut::json(json!(crate::api::discover().await))),
        "record" => record(mcp, args).await,
        _ => Err(format!("unknown tool `{name}`")),
    };
    r.unwrap_or_else(ToolOut::error)
}

// ---- Arguments ------------------------------------------------------------------

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{key}` (a string) is missing"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn num_arg(args: &Value, key: &str, default: f64, min: f64, max: f64) -> f64 {
    args.get(key)
        .and_then(Value::as_f64)
        .unwrap_or(default)
        .clamp(min, max)
}

/// A wire of `graph`: `a.out->b.in` (also `→` or ` -> `), or an output
/// `a.out` (its first wire).
fn parse_wire(graph: &GraphSpec, text: &str) -> Result<EdgeRef, String> {
    let text = text.trim();
    let split = ["->", "→"]
        .iter()
        .find_map(|sep| text.split_once(sep))
        .map(|(a, b)| (a.trim(), b.trim()));
    let found = graph.connections.iter().find(|c| match split {
        Some((from, to)) => c.from == from && c.to == to,
        None => c.from == text,
    });
    match found {
        Some(c) => Ok(EdgeRef {
            from: c.from.clone(),
            to: c.to.clone(),
        }),
        None => {
            let wires: Vec<String> = graph
                .connections
                .iter()
                .map(|c| format!("{}->{}", c.from, c.to))
                .collect();
            Err(format!(
                "there is no wire `{text}` in the open graph; its wires are: {}",
                if wires.is_empty() {
                    "none".to_owned()
                } else {
                    wires.join(", ")
                }
            ))
        }
    }
}

fn wire_arg(mcp: &Mcp, args: &Value) -> Result<EdgeRef, String> {
    parse_wire(&mcp.session.graph(), str_arg(args, "wire")?)
}

// ---- Catalog -----------------------------------------------------------------------

fn summary(help: &str) -> &str {
    // The first sentence (help texts are written that way).
    let end = help
        .match_indices(". ")
        .map(|(i, _)| i + 1)
        .next()
        .unwrap_or(help.len());
    &help[..end]
}

fn list_blocks(mcp: &Mcp, args: &Value) -> Out {
    let words: Vec<String> = opt_str(args, "query")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let category = opt_str(args, "category").map(str::to_lowercase);
    let reg = mcp.session.registry();
    let mut out = Vec::new();
    for e in reg.catalog().filter(|e| !e.hidden) {
        let d = e.descriptor;
        if category
            .as_ref()
            .is_some_and(|c| d.category.to_lowercase() != *c)
        {
            continue;
        }
        let hay =
            format!("{} {} {} {}", d.kind, d.label, d.help, e.keywords.join(" ")).to_lowercase();
        if !words.iter().all(|w| hay.contains(w.as_str())) {
            continue;
        }
        let mut b = json!({
            "kind": d.kind,
            "label": d.label,
            "category": d.category,
            "summary": summary(d.help),
        });
        if e.tx {
            b["transmits"] = json!(true);
        }
        out.push(b);
    }
    // Graph files with exports work as blocks too.
    if category.is_none() {
        for g in mcp.session.graphs().descriptors(reg) {
            let v = serde_json::to_value(&g).unwrap_or_default();
            let hay = v.to_string().to_lowercase();
            if words.iter().all(|w| hay.contains(w.as_str())) {
                out.push(json!({
                    "kind": v["kind"], "label": v["label"], "category": v["category"],
                    "summary": v["help"].as_str().map(summary).unwrap_or_default(),
                }));
            }
        }
    }
    if out.is_empty() {
        return Err("no block matches; try fewer or other words, or no category".into());
    }
    Ok(ToolOut::json(json!(out)))
}

fn describe_block(mcp: &Mcp, args: &Value) -> Out {
    let kind = str_arg(args, "kind")?;
    let reg = mcp.session.registry();
    let entry = reg
        .catalog()
        .find(|e| e.descriptor.kind == kind)
        .ok_or_else(|| {
            let close: Vec<&str> = reg
                .descriptors()
                .map(|d| d.kind)
                .filter(|k| k.contains(kind) || kind.contains(k))
                .take(8)
                .collect();
            format!(
                "unknown block kind `{kind}`{}",
                if close.is_empty() {
                    "; use list_blocks to search".to_owned()
                } else {
                    format!("; did you mean {}?", close.join(", "))
                }
            )
        })?;
    let mut v = serde_json::to_value(&entry).map_err(|e| e.to_string())?;
    // Say plainly which settings can be wired as inputs (`block.@name`).
    if let Some(params) = v["params"].as_array_mut() {
        for (p, spec) in params.iter_mut().zip(&entry.descriptor.params) {
            if spec.wireable() {
                p["wireable"] = json!(true);
            }
        }
    }
    v["notes"] = json!(
        "Ports with \"type\": null take their type from the block's `type` setting. \
         `live` settings change a running graph at once; `wireable` ones can also be driven \
         by an f32 wire into `block.@setting`."
    );
    Ok(ToolOut::json(v))
}

fn get_example(mcp: &Mcp, args: &Value) -> Out {
    let name = str_arg(args, "name")?;
    let e = EXAMPLES
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("no example `{name}`; see list_examples"))?;
    if args.get("open").and_then(Value::as_bool) == Some(true) {
        let g = GraphSpec::from_toml(e.toml).map_err(|e| e.to_string())?;
        mcp.session
            .set_graph_by(MCP_CLIENT, g.clone(), None, Some(BY));
        let mut out = graph_report(mcp, &g, &mcp.session.meta_now());
        out["toml"] = json!(e.toml);
        return Ok(ToolOut::json(out));
    }
    Ok(ToolOut::text(e.toml))
}

// ---- Graphs ------------------------------------------------------------------------

/// The check report for `graph` with build metadata `meta`.
fn graph_report(mcp: &Mcp, graph: &GraphSpec, meta: &Meta) -> Value {
    let reg = mcp.session.registry();
    let mut unconnected = Vec::new();
    for b in &graph.blocks {
        let Some(d) = reg.get(&b.kind) else { continue };
        for (i, p) in d.inputs.iter().enumerate() {
            let named = format!("{}.{}", b.id, p.name);
            let indexed = format!("{}.{i}", b.id);
            let wired = graph
                .connections
                .iter()
                .any(|c| c.to == named || c.to == indexed);
            if !wired && !b.const_inputs.contains_key(p.name) {
                unconnected.push(named);
            }
        }
    }
    let wires: Vec<Value> = meta
        .edges
        .iter()
        .map(|e| {
            let mut w = json!({"wire": format!("{}->{}", e.from, e.to)});
            if let Some(r) = e.meta.sample_rate {
                w["sample_rate"] = json!(r);
            }
            if let Some(f) = e.meta.center_freq {
                w["center_freq"] = json!(f);
            }
            w
        })
        .collect();
    let status = mcp.session.status_now();
    let mut v = json!({
        "builds": meta.error.is_none(),
        "blocks": graph.blocks.iter().map(|b| format!("{} ({})", b.id, b.kind)).collect::<Vec<_>>(),
        "wires": wires,
        "run": {"state": status.state, "stale": status.stale},
    });
    if let Some(e) = &meta.error {
        v["error"] = json!(e);
    }
    if !unconnected.is_empty() {
        v["unconnected_inputs"] = json!(unconnected);
        v["hint"] = json!(
            "Every input needs a wire (or a constant: set_const_input) before the graph can run."
        );
    }
    if !meta.tx.is_empty() {
        v["transmits"] = json!(meta.tx);
        v["tx_note"] = json!(
            "This graph transmits; it can only be run by a person who arms TX in the browser."
        );
    }
    v
}

fn graph_out(mcp: &Mcp, graph: &GraphSpec, meta: Option<Meta>) -> ToolOut {
    let meta = meta.unwrap_or_else(|| mcp.session.meta_now());
    let mut v = graph_report(mcp, graph, &meta);
    if let Some(n) = mcp.session.name() {
        v["name"] = json!(n);
    }
    v["toml"] = json!(graph.to_toml().unwrap_or_default());
    ToolOut::json(v)
}

fn parse_toml(text: &str) -> Result<GraphSpec, String> {
    GraphSpec::from_toml(text).map_err(|e| format!("not a valid .fox.toml: {e}"))
}

fn set_graph(mcp: &Mcp, args: &Value) -> Out {
    let g = parse_toml(str_arg(args, "toml")?)?;
    let name = match opt_str(args, "name") {
        Some(n) => {
            project_path(&mcp.project_dir, n, EXT)?;
            Some(n.to_owned())
        }
        None => mcp.session.name(),
    };
    mcp.session.set_graph_by(MCP_CLIENT, g, name, Some(BY));
    let g = mcp.session.graph();
    Ok(ToolOut::json(graph_report(
        mcp,
        &g,
        &mcp.session.meta_now(),
    )))
}

fn check_graph(mcp: &Mcp, args: &Value) -> Out {
    let g = parse_toml(str_arg(args, "toml")?)?;
    let meta = mcp.session.check(&g);
    Ok(ToolOut::json(graph_report(mcp, &g, &meta)))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `block` or `block.port`: fill in the port if the block has exactly one
/// on that side.
fn endpoint(mcp: &Mcp, g: &GraphSpec, text: &str, output: bool) -> Result<String, String> {
    if text.contains('.') {
        return Ok(text.to_owned());
    }
    let b = g
        .blocks
        .iter()
        .find(|b| b.id == text)
        .ok_or_else(|| format!("no block `{text}`"))?;
    let d = mcp
        .session
        .registry()
        .get(&b.kind)
        .ok_or_else(|| format!("`{text}`: give the port as `{text}.port`"))?;
    let ports = if output { &d.outputs } else { &d.inputs };
    match ports.as_slice() {
        [p] => Ok(format!("{text}.{}", p.name)),
        ps => Err(format!(
            "`{text}` has {} {}: say which, e.g. `{text}.{}`",
            ps.len(),
            if output { "outputs" } else { "inputs" },
            ps.first().map_or("port", |p| p.name)
        )),
    }
}

/// The stream types of an output and an input of `g`, from the blocks as
/// they would be built (variables evaluated). `None` if that can't be worked
/// out yet (unknown blocks, sub-graphs, a graph that doesn't resolve).
fn port_types(mcp: &Mcp, g: &GraphSpec, from: &str, to: &str) -> Option<(StreamType, StreamType)> {
    let reg = mcp.session.registry();
    let r = foxygnal_core::resolve(g, reg, mcp.session.graphs()).ok()?;
    let ty = |ep: &str, output: bool| -> Option<StreamType> {
        let (id, port) = ep.rsplit_once('.')?;
        let b = r.graph.blocks.iter().find(|b| b.id == id)?;
        let inst = reg.build(&b.kind, &b.params).ok()?;
        let ports = if output { &inst.outputs } else { &inst.inputs };
        ports
            .iter()
            .enumerate()
            .find(|(i, (name, _))| name == port || i.to_string() == port)
            .map(|(_, (_, t))| *t)
    };
    Some((ty(from, true)?, ty(to, false)?))
}

/// Converter block kinds from `a` to `b` (one fixed-type input and output).
fn converters(mcp: &Mcp, a: StreamType, b: StreamType) -> Vec<&'static str> {
    mcp.session
        .registry()
        .descriptors()
        .filter(|d| {
            d.category == "Converters"
                && d.inputs.len() == 1
                && d.outputs.len() == 1
                && d.inputs[0].ty == Some(a)
                && d.outputs[0].ty == Some(b)
        })
        .map(|d| d.kind)
        .collect()
}

/// Wire `from` → a new `kind` block → `to`; returns the new block's id.
fn insert_converter(g: &mut GraphSpec, kind: &str, from: &str, to: &str) -> String {
    let id = (1..)
        .map(|i| format!("{kind}_{i}"))
        .find(|id| !g.blocks.iter().any(|b| &b.id == id))
        .expect("an unused id");
    let pos = |ep: &str, key: &str| {
        let bid = ep.rsplit_once('.').map_or(ep, |(b, _)| b);
        g.blocks
            .iter()
            .find(|b| b.id == bid)
            .and_then(|b| b.ui.get(key).and_then(Value::as_f64))
    };
    let mid = |key: &str| match (pos(from, key), pos(to, key)) {
        (Some(a), Some(b)) => (a + b) / 2.0,
        (Some(a), None) | (None, Some(a)) => a,
        _ => 0.0,
    };
    let mut ui = std::collections::BTreeMap::new();
    ui.insert("x".to_owned(), json!(mid("x")));
    ui.insert("y".to_owned(), json!(mid("y") + 120.0));
    g.blocks.push(BlockSpec {
        id: id.clone(),
        kind: kind.to_owned(),
        params: Params::new(),
        const_inputs: Params::new(),
        ui,
    });
    g.connections.push(ConnectionSpec {
        from: from.to_owned(),
        to: format!("{id}.in"),
    });
    g.connections.push(ConnectionSpec {
        from: format!("{id}.out"),
        to: to.to_owned(),
    });
    id
}

fn apply_op(
    mcp: &Mcp,
    g: &mut GraphSpec,
    op: &Value,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let reg = mcp.session.registry();
    let block_mut = |g: &mut GraphSpec, id: &str| -> Result<usize, String> {
        g.blocks
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| format!("no block `{id}`"))
    };
    match str_arg(op, "op")? {
        "add_block" => {
            let id = str_arg(op, "id")?;
            let kind = str_arg(op, "kind")?;
            if !valid_id(id) {
                return Err(format!(
                    "`{id}` isn't a usable id: letters, digits, `_` and `-` only"
                ));
            }
            if g.blocks.iter().any(|b| b.id == id) {
                return Err(format!("there is already a block `{id}`"));
            }
            if reg.get(kind).is_none() && !kind.starts_with("graph:") {
                return Err(format!("unknown block kind `{kind}`; see list_blocks"));
            }
            let params: Params = match op.get("params") {
                Some(Value::Object(m)) => m.clone(),
                None | Some(Value::Null) => Params::new(),
                Some(_) => return Err("`params` must be an object".into()),
            };
            check_param_names(mcp, kind, &params)?;
            let const_inputs: Params = match op.get("const_inputs") {
                Some(Value::Object(m)) => m.clone(),
                _ => Params::new(),
            };
            // Right of everything else unless placed.
            let right = g
                .blocks
                .iter()
                .filter_map(|b| b.ui.get("x").and_then(Value::as_f64))
                .fold(f64::NAN, f64::max);
            let x = op["x"]
                .as_f64()
                .unwrap_or(if right.is_nan() { 50.0 } else { right + 330.0 });
            let y = op["y"].as_f64().unwrap_or(120.0);
            let mut ui = std::collections::BTreeMap::new();
            ui.insert("x".to_owned(), json!(x));
            ui.insert("y".to_owned(), json!(y));
            g.blocks.push(BlockSpec {
                id: id.to_owned(),
                kind: kind.to_owned(),
                params,
                const_inputs,
                ui,
            });
        }
        "remove_block" => {
            let id = str_arg(op, "id")?;
            let i = block_mut(g, id)?;
            g.blocks.remove(i);
            let prefix = format!("{id}.");
            g.connections
                .retain(|c| !c.from.starts_with(&prefix) && !c.to.starts_with(&prefix));
        }
        "set_params" => {
            let id = str_arg(op, "id")?;
            let i = block_mut(g, id)?;
            let Some(Value::Object(params)) = op.get("params") else {
                return Err("`params` must be an object".into());
            };
            check_param_names(mcp, &g.blocks[i].kind.clone(), params)?;
            for (k, v) in params {
                if v.is_null() {
                    g.blocks[i].params.remove(k);
                } else {
                    g.blocks[i].params.insert(k.clone(), v.clone());
                }
            }
        }
        "connect" => {
            let from = endpoint(mcp, g, str_arg(op, "from")?, true)?;
            let to = endpoint(mcp, g, str_arg(op, "to")?, false)?;
            if g.connections.iter().any(|c| c.from == from && c.to == to) {
                return Ok(());
            }
            if let Some(c) = g.connections.iter().find(|c| c.to == to) {
                return Err(format!(
                    "`{to}` is already fed by `{}`; disconnect that first (an input takes one wire)",
                    c.from
                ));
            }
            let convert = op.get("convert").and_then(Value::as_bool) != Some(false);
            if let Some((a, b)) = port_types(mcp, g, &from, &to)
                && a != b
                && convert
            {
                let fits = converters(mcp, a, b);
                if let [kind] = fits.as_slice() {
                    let id = insert_converter(g, kind, &from, &to);
                    let settings: Vec<String> = mcp
                        .session
                        .registry()
                        .get(kind)
                        .map(|d| {
                            d.params
                                .iter()
                                .map(|p| format!("{}={}", p.name, p.default))
                                .collect()
                        })
                        .unwrap_or_default();
                    let check = if settings.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; check its settings ({}), e.g. with set_params on `{id}`",
                            settings.join(", ")
                        )
                    };
                    notes.push(format!(
                        "`{from}` carries {a} and `{to}` takes {b}: inserted `{id}` ({kind}) between them{check}"
                    ));
                    return Ok(());
                }
                notes.push(if fits.is_empty() {
                    format!(
                        "`{from}` carries {a} and `{to}` takes {b}, and no single block converts {a} to {b}; set the blocks' `type` to match, or convert in steps"
                    )
                } else {
                    format!(
                        "`{from}` carries {a} and `{to}` takes {b}; several blocks convert that ({}): add the right one and wire it in between",
                        fits.join(", ")
                    )
                });
            }
            g.connections.push(ConnectionSpec { from, to });
        }
        "disconnect" => {
            let from = endpoint(mcp, g, str_arg(op, "from")?, true)?;
            let to = endpoint(mcp, g, str_arg(op, "to")?, false)?;
            let before = g.connections.len();
            g.connections.retain(|c| !(c.from == from && c.to == to));
            if g.connections.len() == before {
                return Err(format!("there is no wire `{from}->{to}`"));
            }
        }
        "set_const_input" => {
            let id = str_arg(op, "id")?;
            let input = str_arg(op, "input")?;
            let i = block_mut(g, id)?;
            match op.get("value") {
                None | Some(Value::Null) => {
                    g.blocks[i].const_inputs.remove(input);
                }
                Some(v) => {
                    g.blocks[i].const_inputs.insert(input.to_owned(), v.clone());
                }
            }
        }
        "set_variable" => {
            let name = str_arg(op, "name")?;
            match op.get("value") {
                None | Some(Value::Null) => {
                    g.vars.remove(name);
                }
                Some(v) => {
                    g.vars.insert(name.to_owned(), v.clone());
                }
            }
        }
        other => return Err(format!("unknown operation `{other}`")),
    }
    Ok(())
}

/// Refuse setting names the block doesn't have, listing the ones it does.
fn check_param_names(mcp: &Mcp, kind: &str, params: &Map<String, Value>) -> Result<(), String> {
    let Some(d) = mcp.session.registry().get(kind) else {
        return Ok(()); // graph files: checked when built
    };
    for k in params.keys() {
        if !d.params.iter().any(|p| p.name == k) {
            let names: Vec<String> = d
                .params
                .iter()
                .map(|p| match &p.kind {
                    ParamKind::Enum { options } => format!("{} ({})", p.name, options.join("|")),
                    _ => p.name.to_owned(),
                })
                .collect();
            return Err(format!(
                "`{kind}` has no setting `{k}`; it has: {}",
                names.join(", ")
            ));
        }
    }
    Ok(())
}

fn edit_graph(mcp: &Mcp, args: &Value) -> Out {
    let ops = args
        .get("operations")
        .and_then(Value::as_array)
        .ok_or("`operations` (an array) is missing")?;
    let mut g = mcp.session.graph();
    let mut notes = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        apply_op(mcp, &mut g, op, &mut notes).map_err(|e| {
            format!(
                "operation {} ({}): {e}. Nothing was changed.",
                i + 1,
                op["op"]
            )
        })?;
    }
    mcp.session
        .set_graph_by(MCP_CLIENT, g.clone(), mcp.session.name(), Some(BY));
    let mut v = graph_report(mcp, &g, &mcp.session.meta_now());
    if !notes.is_empty() {
        v["notes"] = json!(notes);
    }
    Ok(ToolOut::json(v))
}

// ---- Files --------------------------------------------------------------------------

fn list_files(mcp: &Mcp) -> Out {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&mcp.project_dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !(name.ends_with(EXT) || name.ends_with(GRC_EXT)) || name.starts_with('.') {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        files.push((modified, name));
    }
    files.sort_by(|a, b| b.cmp(a));
    Ok(ToolOut::json(json!(
        files.into_iter().map(|(_, n)| n).collect::<Vec<_>>()
    )))
}

fn open_file(mcp: &Mcp, args: &Value) -> Out {
    let name = str_arg(args, "name")?;
    let grc = name.ends_with(GRC_EXT);
    let path = project_path(&mcp.project_dir, name, if grc { GRC_EXT } else { EXT })?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
    let (graph, warnings, saved_as) = if grc {
        let imp = foxygnal_grc::import(&text, &foxygnal_grc::Options::default())
            .map_err(|e| format!("{name}: {e}"))?;
        let fox = foxygnal_grc::fox_name(std::path::Path::new(name))
            .to_string_lossy()
            .into_owned();
        (imp.graph, imp.warnings, fox)
    } else {
        (parse_toml(&text)?, Vec::new(), name.to_owned())
    };
    mcp.session
        .set_graph_by(MCP_CLIENT, graph.clone(), Some(saved_as), Some(BY));
    let mut v = graph_report(mcp, &graph, &mcp.session.meta_now());
    if !warnings.is_empty() {
        v["import_warnings"] = json!(warnings);
    }
    Ok(ToolOut::json(v))
}

fn save_file(mcp: &Mcp, args: &Value) -> Out {
    let name = str_arg(args, "name")?;
    let path = project_path(&mcp.project_dir, name, EXT)?;
    let overwrite = args.get("overwrite").and_then(Value::as_bool) == Some(true);
    if path.exists() && !overwrite {
        return Err(format!("{name} exists; pass overwrite=true to replace it"));
    }
    let graph = mcp.session.graph();
    let text = graph.to_toml().map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{name}: {e}"))?;
    // The editor shows the new name.
    mcp.session
        .set_graph_by(MCP_CLIENT, graph, Some(name.to_owned()), Some(BY));
    Ok(ToolOut::json(json!({"saved": name})))
}

fn import_grc(args: &Value) -> Out {
    let imp = foxygnal_grc::import(str_arg(args, "grc")?, &foxygnal_grc::Options::default())
        .map_err(|e| e.to_string())?;
    let toml = imp.graph.to_toml().map_err(|e| e.to_string())?;
    Ok(ToolOut::json(
        json!({"toml": toml, "warnings": imp.warnings, "title": imp.title}),
    ))
}

fn export_grc(mcp: &Mcp) -> Out {
    let loader = crate::graphs::ProjectGraphs {
        dir: mcp.project_dir.clone(),
    };
    let stem = mcp
        .session
        .name()
        .map(|n| n.strip_suffix(EXT).unwrap_or(&n).to_owned());
    let opts = foxygnal_grc::ExportOptions {
        loader: Some(&loader),
        name: stem,
    };
    let exp = foxygnal_grc::export(&mcp.session.graph(), &opts).map_err(|e| e.to_string())?;
    Ok(ToolOut::json(
        json!({"grc": exp.grc, "warnings": exp.warnings}),
    ))
}

// ---- Running ------------------------------------------------------------------------

async fn run(mcp: &Mcp) -> Out {
    // Never armed: a graph that transmits is refused, with a message saying so.
    mcp.session.run_with(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = mcp.session.status_now();
    Ok(ToolOut::json(json!({
        "state": s.state,
        "errors": s.errors,
        "warnings": s.warnings,
    })))
}

async fn status(mcp: &Mcp) -> Out {
    let s = mcp.session.status_now();
    let mut v = json!({
        "state": s.state,
        "stale": s.stale,
        "errors": s.errors,
        "warnings": s.warnings,
        "recordings": mcp.session.recorders().statuses(),
    });
    if let Some((id, mon, map)) = mcp.session.monitor() {
        let (c0, t0) = (mon.stats(), Instant::now());
        tokio::time::sleep(Duration::from_millis(500)).await;
        if mcp.session.monitor().is_some_and(|(i, ..)| i == id) {
            let st = map.stats(crate::probe::stats(&c0, &mon.stats(), t0.elapsed()));
            v["blocks"] = json!(
                st.blocks
                    .iter()
                    .map(|b| json!({"block": b.block, "cpu_percent": round1(f64::from(b.cpu) * 100.0)}))
                    .collect::<Vec<_>>()
            );
            v["wires"] = json!(
                st.edges
                    .iter()
                    .map(|e| json!({
                        "wire": format!("{}->{}", e.from, e.to),
                        "items_per_second": e.rate.round(),
                        "items": e.items,
                        "buffer_fill_percent": round1(f64::from(e.fill) * 100.0),
                    }))
                    .collect::<Vec<_>>()
            );
        }
    }
    Ok(ToolOut::json(v))
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

// ---- Looking at signals -----------------------------------------------------------

fn not_running() -> String {
    "the graph isn't running; call run first".into()
}

/// Run a probe on the hub for `secs` (longer, up to 2 s, if no frame has come
/// yet) and take what it collected.
async fn sample(
    mcp: &Mcp,
    wire: &EdgeRef,
    config: ProbeConfig,
    fps: f32,
    secs: f64,
) -> Result<Collected, String> {
    if mcp.session.monitor().is_none() {
        return Err(not_running());
    }
    let id = mcp.probes.new_id();
    mcp.session
        .hub()
        .set(MCP_CLIENT, id, wire.clone(), config, fps);
    let until = Instant::now() + Duration::from_secs_f64(secs);
    let patience = until + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = Instant::now();
        let (frames, error) = mcp
            .probes
            .lock()
            .get(&id)
            .map_or((0, None), |c| (c.frames, c.error.clone()));
        if error.is_some() || (now >= until && frames > 0) || now >= patience {
            break;
        }
    }
    mcp.session.hub().remove(MCP_CLIENT, id);
    let c = mcp.probes.lock().remove(&id).unwrap_or_default();
    if let Some(e) = &c.error {
        return Err(e.clone());
    }
    Ok(c)
}

fn probe_config(v: Value) -> ProbeConfig {
    serde_json::from_value(v).expect("probe configs are well-formed")
}

fn db(power: f64) -> f64 {
    if power > 0.0 {
        round1(10.0 * power.log10())
    } else {
        -200.0
    }
}

async fn measure(mcp: &Mcp, args: &Value) -> Out {
    let wire = wire_arg(mcp, args)?;
    let secs = num_arg(args, "seconds", 1.0, 0.1, 30.0);
    let meta = mcp
        .session
        .meta_now()
        .edges
        .into_iter()
        .find(|e| e.from == wire.from && e.to == wire.to)
        .map(|e| e.meta)
        .unwrap_or_default();
    let label = format!("{}->{}", wire.from, wire.to);
    match str_arg(args, "kind")? {
        "spectrum" => {
            let n = (num_arg(args, "fft_size", 4096.0, 64.0, 8192.0) as u32).next_power_of_two();
            let cfg = probe_config(json!({
                "kind": "spectrum", "fft_size": n, "bins": 8192, "average": 1, "window": "blackman_harris",
            }));
            let c = sample(mcp, &wire, cfg, 30.0, secs).await?;
            let max = num_arg(args, "signals", 8.0, 0.0, 64.0) as usize;
            let snr = num_arg(args, "min_snr_db", 10.0, 0.0, 100.0);
            let mut v = spectrum_summary(&c, meta.sample_rate, meta.center_freq, max, snr)?;
            v["wire"] = json!(label);
            Ok(ToolOut::json(v))
        }
        "level" => {
            let c = sample(
                mcp,
                &wire,
                probe_config(json!({"kind": "meter"})),
                15.0,
                secs,
            )
            .await?;
            let l = &c.level;
            if l.samples == 0 {
                return Err("no samples arrived on this wire".into());
            }
            let n = l.samples as f64;
            let mut v = json!({
                "wire": label,
                "samples": l.samples,
                "rms_dbfs": db(l.sum_sq / n),
                "peak_dbfs": db(f64::from(l.peak).powi(2)),
                "dc": if l.complex { json!([l.sum_re / n, l.sum_im / n]) } else { json!(l.sum_re / n) },
                "note": "dBFS: 0 dB is a full-scale (±1) signal.",
            });
            if let Some(r) = meta.sample_rate {
                v["sample_rate"] = json!(r);
            }
            Ok(ToolOut::json(v))
        }
        "messages" => {
            let c = sample(
                mcp,
                &wire,
                probe_config(json!({"kind": "text"})),
                10.0,
                secs,
            )
            .await?;
            let max = num_arg(args, "max_lines", 200.0, 1.0, 5000.0) as usize;
            let total = c.lines.len();
            let lines: Vec<&String> = c.lines.iter().skip(total.saturating_sub(max)).collect();
            Ok(ToolOut::json(json!({
                "wire": label,
                "lines": lines,
                "total": total,
                "some_lost": c.gap,
            })))
        }
        "constellation" => {
            let cfg = probe_config(json!({"kind": "constellation", "points": 8192}));
            let c = sample(mcp, &wire, cfg, 30.0, secs).await?;
            let mut v = constellation_summary(&c, meta.sample_rate)?;
            v["wire"] = json!(label);
            Ok(ToolOut::json(v))
        }
        k => Err(format!(
            "unknown kind `{k}`: spectrum, level, constellation or messages"
        )),
    }
}

/// Noise floor, the signals in it and a coarse outline of an averaged
/// spectrum.
///
/// A *signal* is a stretch of bins more than 6 dB above the noise floor
/// (gaps of a few bins bridged) whose strongest bin clears `min_snr` dB. So a
/// broadcast FM station is one signal 200 kHz wide, not its carrier and
/// sideband lumps as separate peaks.
fn spectrum_summary(
    c: &Collected,
    rate: Option<f64>,
    center: Option<f64>,
    max_signals: usize,
    min_snr: f64,
) -> Result<Value, String> {
    let s = &c.spectrum;
    if s.frames == 0 || s.sum.is_empty() {
        return Err("no spectrum arrived: the wire carries too few samples in that time".into());
    }
    let n = s.sum.len();
    let power: Vec<f64> = s.sum.iter().map(|p| p / f64::from(s.frames)).collect();
    let avg: Vec<f64> = power.iter().map(|&p| db(p)).collect();
    let fft = f64::from(s.fft_size);
    let res = 1.0 / fft;
    // Bin → offset from the centre, in units of the sample rate.
    let offset = |i: f64| {
        if s.complex {
            (i - (n / 2) as f64) / fft
        } else {
            i / fft
        }
    };
    let mut sorted = avg.clone();
    sorted.sort_by(f64::total_cmp);
    let floor = sorted[n / 2];
    let top = sorted[n - 1];

    // Stretches above floor + 6 dB, bridging gaps of up to 3 bins. Within 100
    // dB of the strongest: below that a noiseless digital signal has float
    // rounding spurs, not signals.
    let edge = (floor + 6.0).max(top - 100.0);
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for i in (0..n).filter(|&i| avg[i] > edge) {
        match regions.last_mut() {
            Some((_, hi)) if i <= *hi + 4 => *hi = i,
            _ => regions.push((i, i)),
        }
    }
    let threshold = (floor + min_snr).max(top - 100.0);
    let mut signals: Vec<(f64, Value)> = Vec::new();
    for (lo, hi) in regions {
        let k = (lo..=hi)
            .max_by(|&a, &b| avg[a].total_cmp(&avg[b]))
            .expect("non-empty");
        if avg[k] < threshold {
            continue;
        }
        // The strongest point between bins: a parabola through three bins (dB).
        let (shift, level) = if k > 0 && k + 1 < n {
            let (l, c, r) = (avg[k - 1], avg[k], avg[k + 1]);
            let d = l - 2.0 * c + r;
            if d < 0.0 {
                let x = (0.5 * (l - r) / d).clamp(-0.5, 0.5);
                (x, c - 0.25 * (l - r) * x)
            } else {
                (0.0, c)
            }
        } else {
            (0.0, avg[k])
        };
        let peak = offset(k as f64 + shift);
        // Power-weighted centre (above the noise) and the stretch's width.
        let (mut wsum, mut psum) = (0.0, 0.0);
        let noise = 10f64.powf(floor / 10.0);
        for (i, &pw) in power.iter().enumerate().take(hi + 1).skip(lo) {
            let p = (pw - noise).max(0.0);
            wsum += p * i as f64;
            psum += p;
        }
        let centre = offset(if psum > 0.0 { wsum / psum } else { k as f64 });
        let width = (hi - lo + 1) as f64 * res;
        let mut v = json!({
            "level_dbfs": round1(level),
            "above_noise_db": round1(level - floor),
        });
        match rate {
            Some(r) => {
                v["peak_offset_hz"] = json!(round1(peak * r));
                v["center_offset_hz"] = json!(round1(centre * r));
                v["bandwidth_hz"] = json!((width * r).round());
                if let Some(f) = center {
                    v["peak_freq_hz"] = json!((f + peak * r).round());
                    v["center_freq_hz"] = json!((f + centre * r).round());
                }
            }
            None => {
                v["peak_offset"] = json!(peak);
                v["center_offset"] = json!(centre);
                v["bandwidth"] = json!(width);
            }
        }
        signals.push((level, v));
    }
    signals.sort_by(|a, b| b.0.total_cmp(&a.0));
    let more = signals.len().saturating_sub(max_signals);
    signals.truncate(max_signals);
    // A coarse outline: 48 slices, each its strongest bin.
    let slices = 48.min(n);
    let outline: Vec<i64> = (0..slices)
        .map(|k| {
            let (a, b) = (
                k * n / slices,
                ((k + 1) * n / slices).max(k * n / slices + 1),
            );
            avg[a..b].iter().copied().fold(f64::MIN, f64::max).round() as i64
        })
        .collect();
    let mut v = json!({
        "fft_size": s.fft_size,
        "spectra_averaged": s.frames,
        "noise_floor_dbfs": floor,
        "strongest_dbfs": top,
        "signals": signals.into_iter().map(|s| s.1).collect::<Vec<_>>(),
        "outline_dbfs": outline,
        "outline_note": "The spectrum in 48 equal slices from the lowest to the highest frequency shown, each slice's strongest bin, in dBFS.",
    });
    if more > 0 {
        v["more_signals"] = json!(more);
    }
    match rate {
        Some(r) => {
            v["sample_rate"] = json!(r);
            v["resolution_hz"] = json!(round1(r / fft));
            let (lo, hi) = if s.complex {
                (-r / 2.0, r / 2.0)
            } else {
                (0.0, r / 2.0)
            };
            let shift = center.unwrap_or(0.0);
            v["span_hz"] = json!([lo + shift, hi + shift]);
            if let Some(f) = center {
                v["center_freq"] = json!(f);
            }
        }
        None => {
            v["note"] = json!(
                "The wire's sample rate isn't known: frequencies are fractions of it (cycles per sample)."
            );
        }
    }
    Ok(v)
}

/// What the I/Q points of a wire say about its modulation.
fn constellation_summary(c: &Collected, rate: Option<f64>) -> Result<Value, String> {
    use foxygnal_core::Complex32;
    let k = &c.constellation;
    if k.points.len() < 64 {
        return Err("too few points arrived (is it a c32 wire with a signal on it?)".into());
    }
    let pts: Vec<Complex32> = k.points.iter().copied().collect();
    let n = pts.len() as f64;
    let (mut sum, mut sum_sq, mut sum_mag) = (Complex32::default(), 0.0f64, 0.0f64);
    for z in &pts {
        sum += z;
        sum_sq += f64::from(z.norm_sqr());
        sum_mag += f64::from(z.norm());
    }
    let mean_mag = sum_mag / n;
    let var_mag = (sum_sq / n - mean_mag * mean_mag).max(0.0);
    let spread = var_mag.sqrt() / mean_mag.max(1e-12);
    let dc = sum / n as f32;
    let mut v = json!({
        "points": pts.len(),
        "rms": round3((sum_sq / n).sqrt()),
        "mean_magnitude": round3(mean_mag),
        "envelope_spread": round3(spread),
        "dc": [round3(f64::from(dc.re)), round3(f64::from(dc.im))],
    });

    // The M-th power test: (z/|z|)^M averages to a large value for M-PSK
    // whenever M is a multiple of the order.
    let unit: Vec<Complex32> = pts
        .iter()
        .filter(|z| z.norm() > 1e-9)
        .map(|z| z / z.norm())
        .collect();
    let power = |m: i32| {
        let s: Complex32 = unit.iter().map(|u| u.powi(m)).sum();
        s / unit.len().max(1) as f32
    };
    let tests: Vec<(i32, Complex32)> = [2, 4, 8].into_iter().map(|m| (m, power(m))).collect();
    v["mth_power"] = json!(
        tests
            .iter()
            .map(|(m, p)| json!({"m": m, "strength": round3(f64::from(p.norm()))}))
            .collect::<Vec<_>>()
    );
    // Synchronized PSK (one sample per symbol, at the symbol centres) has a
    // nearly constant envelope and a strong M-th power line.
    let order = if spread < 0.25 {
        tests.iter().find(|(_, p)| p.norm() > 0.6).copied()
    } else {
        None
    };
    match order {
        Some((m, p)) => {
            let name = match m {
                2 => "BPSK",
                4 => "QPSK",
                _ => "8PSK",
            };
            let theta = p.arg() / m as f32;
            let step = std::f32::consts::TAU / m as f32;
            let (mut err, mut refp) = (0.0f64, 0.0f64);
            for z in &pts {
                let k = ((z.arg() - theta) / step).round();
                let ideal = Complex32::from_polar(mean_mag as f32, theta + k * step);
                err += f64::from((z - ideal).norm_sqr());
                refp += f64::from(ideal.norm_sqr());
            }
            let evm = (err / refp).sqrt();
            v["modulation"] = json!(name);
            v["rotation_deg"] = json!(round1(f64::from(theta.to_degrees())));
            v["evm_percent"] = json!(round1(evm * 100.0));
            v["mer_db"] = json!(round1(-20.0 * evm.max(1e-9).log10()));
        }
        None => {
            v["modulation"] = json!(if spread < 0.25 {
                "constant envelope, no PSK clusters (FM/FSK, or not yet synchronized)"
            } else {
                "varying envelope (QAM/ASK/AM, noise, or not yet synchronized)"
            });
        }
    }

    // The symbol-rate line: |x|² of a pulse-shaped linear modulation has a
    // spectral line at the symbol rate.
    if k.cyclo_frames > 0 {
        let nfft = k.cyclo.len();
        let half = nfft / 2;
        let p = &k.cyclo[..half];
        let mut sorted: Vec<f64> = p[4..].to_vec();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2].max(1e-30);
        let (i, &max) = p
            .iter()
            .enumerate()
            .skip(4)
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("non-empty");
        let snr = 10.0 * (max / median).log10();
        if snr > 12.0 {
            let shift = if i + 1 < half {
                let (l, c, r) = (p[i - 1].max(1e-30).ln(), max.ln(), p[i + 1].max(1e-30).ln());
                let d = l - 2.0 * c + r;
                if d < 0.0 {
                    (0.5 * (l - r) / d).clamp(-0.5, 0.5)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let frac = (i as f64 + shift) / nfft as f64;
            v["symbol_rate_line_db"] = json!(round1(snr));
            match rate {
                Some(r) => {
                    v["symbol_rate_hz"] = json!((frac * r).round());
                    v["samples_per_symbol"] = json!(round3(1.0 / frac));
                }
                None => v["symbol_rate_cycles_per_sample"] = json!(frac),
            }
        } else {
            v["symbol_rate"] = json!(
                "no line in |x|² (one sample per symbol, a constant-envelope signal, or too noisy)"
            );
        }
    }
    Ok(v)
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

// ---- Watches ---------------------------------------------------------------------

#[derive(Default)]
pub struct Watches {
    map: Mutex<HashMap<u32, (EdgeRef, &'static str)>>,
}

const MAX_WATCHES: usize = 16;

fn watch_id(args: &Value) -> Result<u32, String> {
    args.get("watch_id")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .ok_or_else(|| "`watch_id` (a number) is missing".into())
}

fn watch(mcp: &Mcp, args: &Value) -> Out {
    let wire = wire_arg(mcp, args)?;
    let (kind, cfg): (&'static str, _) = match str_arg(args, "kind")? {
        "messages" => ("messages", json!({"kind": "text"})),
        "image" => ("image", json!({"kind": "image"})),
        k => return Err(format!("unknown kind `{k}`: messages or image")),
    };
    let mut w = mcp.watches.map.lock().unwrap_or_else(|e| e.into_inner());
    if w.len() >= MAX_WATCHES {
        return Err(format!(
            "{MAX_WATCHES} watches are running already; stop_watch one first"
        ));
    }
    let id = mcp.probes.new_id();
    mcp.session
        .hub()
        .set(MCP_CLIENT, id, wire.clone(), probe_config(cfg), 10.0);
    w.insert(id, (wire.clone(), kind));
    let mut v =
        json!({"watch_id": id, "wire": format!("{}->{}", wire.from, wire.to), "kind": kind});
    if mcp.session.monitor().is_none() {
        v["note"] = json!("Not running yet: collecting starts when the graph runs.");
    }
    Ok(ToolOut::json(v))
}

fn picture_png(p: &Picture) -> Result<Vec<u8>, String> {
    let h = p.shown_height();
    let stride = usize::from(p.width) * usize::from(p.channels);
    let mut px = p.pixels.clone();
    px.resize(usize::from(h) * stride, 0);
    let mut out = Vec::new();
    foxygnal_blocks::image::encode_png(&mut out, u32::from(p.width), u32::from(h), p.channels, &px)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn read_watch(mcp: &Mcp, args: &Value) -> Out {
    let id = watch_id(args)?;
    let (wire, kind) = mcp
        .watches
        .map
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("no watch {id}"))?;
    let mut all = mcp.probes.lock();
    let c = all.get_mut(&id).ok_or_else(|| format!("no watch {id}"))?;
    let label = format!("{}->{}", wire.from, wire.to);
    let running = mcp.session.monitor().is_some();
    if let Some(e) = &c.error {
        return Err(e.clone());
    }
    if kind == "messages" {
        let max = num_arg(args, "max_lines", 500.0, 1.0, 5000.0) as usize;
        let k = max.min(c.lines.len());
        let lines: Vec<String> = c.lines.drain(..k).collect();
        let v = json!({
            "wire": label,
            "lines": lines,
            "more_waiting": c.lines.len(),
            "some_lost": std::mem::take(&mut c.gap),
            "running": running,
        });
        return Ok(ToolOut::json(v));
    }
    let kept: Vec<Value> = c
        .pictures
        .iter()
        .map(|p| {
            json!({
                "picture": p.number, "width": p.width, "height": p.shown_height(),
                "rows_received": p.rows, "finished": p.done,
            })
        })
        .collect();
    let pick = match args.get("picture").and_then(Value::as_u64) {
        Some(n) => c
            .pictures
            .iter()
            .rev()
            .find(|p| u64::from(p.number) == n)
            .ok_or_else(|| format!("picture {n} isn't kept; kept: {kept:?}"))?,
        None => match c.pictures.back() {
            Some(p) => p,
            None => {
                return Ok(ToolOut::json(json!({
                    "wire": label, "pictures": [], "running": running,
                    "note": "No picture yet.",
                })));
            }
        },
    };
    let png = picture_png(pick)?;
    let v = json!({
        "wire": label,
        "shown": pick.number,
        "pictures": kept,
        "running": running,
    });
    Ok(ToolOut::json(v).with_png(&png))
}

fn stop_watch(mcp: &Mcp, args: &Value) -> Out {
    let id = watch_id(args)?;
    let removed = mcp
        .watches
        .map
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if removed.is_none() {
        return Err(format!("no watch {id}"));
    }
    mcp.session.hub().remove(MCP_CLIENT, id);
    mcp.probes.lock().remove(&id);
    Ok(ToolOut::json(json!({"stopped": id})))
}

// ---- Recording --------------------------------------------------------------------

async fn record(mcp: &Mcp, args: &Value) -> Out {
    let wire = wire_arg(mcp, args)?;
    if mcp.session.monitor().is_none() {
        return Err(not_running());
    }
    let secs = num_arg(args, "seconds", 5.0, 0.1, 600.0);
    let mut events = mcp.session.events();
    mcp.session
        .record_start(wire.clone(), opt_str(args, "name").map(str::to_owned));
    let ours = |ev: &Value| {
        ev["op"] == "record" && ev["edge"]["from"] == wire.from && ev["edge"]["to"] == wire.to
    };
    let mut last: Option<Value> = None;
    let mut stop_at = Some(Instant::now() + Duration::from_secs_f64(secs));
    let give_up = Instant::now() + Duration::from_secs_f64(secs + 5.0);
    loop {
        let now = Instant::now();
        if let Some(t) = stop_at
            && now >= t
        {
            mcp.session.recorders().stop(&wire);
            stop_at = None;
        }
        if now >= give_up {
            break;
        }
        let wait = stop_at.unwrap_or(give_up).saturating_duration_since(now);
        match tokio::time::timeout(wait.max(Duration::from_millis(10)), events.recv()).await {
            Ok(Ok(json)) => {
                let Ok(ev) = serde_json::from_str::<Value>(&json) else {
                    continue;
                };
                if !ours(&ev) {
                    continue;
                }
                let done = ev["done"].as_bool() == Some(true);
                last = Some(ev);
                if done {
                    break;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(_)) => break,
            Err(_) => {} // timeout: time to stop, or to give up
        }
    }
    let ev = last.ok_or("the recording didn't report back")?;
    if let Some(e) = ev["error"].as_str() {
        return Err(format!("recording failed: {e}"));
    }
    let mut v = ev;
    if let Some(o) = v.as_object_mut() {
        o.remove("op");
    }
    Ok(ToolOut::json(v))
}

// ---- Helpers -------------------------------------------------------------------------

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn summaries_end_at_the_first_sentence() {
        assert_eq!(summary("Does a thing. Then more."), "Does a thing.");
        assert_eq!(summary("No full stop"), "No full stop");
    }
}
