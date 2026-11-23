//! What the MCP server tells an assistant up front: instructions, readable
//! resources (docs, the open graph, the catalog, examples) and prompts.

use serde_json::{Value, json};

use super::Mcp;
use crate::api::EXAMPLES;

pub const INSTRUCTIONS: &str = "\
foxygnal is a software-defined radio flowgraph engine. A graph is blocks (sources, filters, \
demodulators, decoders, sinks) joined by typed wires (f32, c32 = complex I/Q, i16, u8). \
Graphs are `.fox.toml` files; `get_graph` shows the open one as TOML.

Workflow:
1. Find blocks with `list_blocks` (search by words like \"fm\", \"filter\", \"decoder\") and read \
their ports and settings with `describe_block`. `list_examples` / `get_example` show complete, \
working graphs; starting from one is usually quickest.
2. Change the graph with `edit_graph` (add/remove blocks, set settings, connect) or replace it \
with `set_graph`. Every change is checked like in the editor: the reply says whether the graph \
builds, the sample rate on every wire, and which inputs are still unconnected. A person may be \
watching the same graph in the browser; your edits show up there and can be undone.
3. `run`, then look: `measure` a wire (the signals in a spectrum and the noise floor, signal level, or the \
text a decoder writes), `watch` a decoder's text or picture output over time, `status` for \
errors, CPU and throughput. `stop` when done.

Notes: a wire is named `from_block.port->to_block.port` (or just the output, `block.port`). \
Settings take numbers with SI suffixes (\"2.4M\", \"100k\") or expressions with graph variables \
(\"$freq - $offset\"). Sample rates flow downstream from sources; a decimating block divides \
them. Live settings (frequency, gain, offsets, …) change a running graph at once; others \
apply on the next run. Transmitting is not possible from here: a graph with a transmitting \
block is refused. `sim:` is a simulated FM-band receiver that needs no hardware.";

const DOCS: &[(&str, &str, &str)] = &[
    (
        "readme",
        "What foxygnal is, how to build and run it",
        include_str!("../../../../README.md"),
    ),
    (
        "architecture",
        "Engine, blocks, stream types, probes, decoded messages and pictures, the server API",
        include_str!("../../../../docs/ARCHITECTURE.md"),
    ),
    (
        "blocks",
        "How blocks are written and what the standard library has",
        include_str!("../../../../docs/BLOCKS.md"),
    ),
    (
        "hardware",
        "SDR devices: RTL-SDR, HackRF, SoapySDR, rtl_tcp, SpyServer; device strings",
        include_str!("../../../../docs/HARDWARE.md"),
    ),
    (
        "grc-import",
        "Importing and exporting GNU Radio Companion flowgraphs",
        include_str!("../../../../docs/GRC_IMPORT.md"),
    ),
    (
        "roadmap",
        "What is done and what is planned",
        include_str!("../../../../docs/ROADMAP.md"),
    ),
];

pub fn resources() -> Vec<Value> {
    let mut out = vec![
        json!({
            "uri": "foxygnal://graph",
            "name": "graph",
            "title": "The open graph",
            "description": "The graph being edited (and run), as .fox.toml",
            "mimeType": "application/toml",
        }),
        json!({
            "uri": "foxygnal://blocks",
            "name": "blocks",
            "title": "Block catalog",
            "description": "Every block kind with its ports and settings (JSON)",
            "mimeType": "application/json",
        }),
    ];
    out.extend(DOCS.iter().map(|(name, desc, _)| {
        json!({
            "uri": format!("foxygnal://docs/{name}"),
            "name": name,
            "description": desc,
            "mimeType": "text/markdown",
        })
    }));
    out.extend(EXAMPLES.iter().map(|e| {
        json!({
            "uri": format!("foxygnal://examples/{}", e.name),
            "name": format!("example-{}", e.name),
            "title": e.title,
            "description": e.description,
            "mimeType": "application/toml",
        })
    }));
    out
}

pub fn read(mcp: &Mcp, uri: &str) -> Option<(&'static str, String)> {
    let rest = uri.strip_prefix("foxygnal://")?;
    match rest {
        "graph" => Some((
            "application/toml",
            mcp.session.graph().to_toml().unwrap_or_default(),
        )),
        "blocks" => {
            let all: Vec<Value> = mcp
                .session
                .registry()
                .catalog()
                .filter(|e| !e.hidden)
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            Some((
                "application/json",
                serde_json::to_string_pretty(&all).unwrap_or_default(),
            ))
        }
        _ => {
            if let Some(name) = rest.strip_prefix("docs/") {
                let (_, _, text) = DOCS.iter().find(|d| d.0 == name)?;
                return Some(("text/markdown", (*text).to_owned()));
            }
            let name = rest.strip_prefix("examples/")?;
            let e = EXAMPLES.iter().find(|e| e.name == name)?;
            Some(("application/toml", e.toml.to_owned()))
        }
    }
}

pub fn prompts() -> Vec<Value> {
    vec![
        json!({
            "name": "build_receiver",
            "title": "Build a receiver",
            "description": "Build and run a graph that receives (and if possible decodes) a kind of signal",
            "arguments": [
                {"name": "signal", "description": "What to receive, e.g. \"FM broadcast at 98.5 MHz\", \"ADS-B\", \"NOAA weather radio\"", "required": true},
                {"name": "device", "description": "SDR device string, e.g. \"rtlsdr:0\" or \"sim:\" (default: the first device found)", "required": false},
            ],
        }),
        json!({
            "name": "find_signals",
            "title": "Find signals in a band",
            "description": "Tune an SDR across a band and report the strongest signals and what they might be",
            "arguments": [
                {"name": "band", "description": "Frequency range, e.g. \"144–146 MHz\" or \"the FM broadcast band\"", "required": true},
                {"name": "device", "description": "SDR device string (default: the first device found)", "required": false},
            ],
        }),
        json!({
            "name": "decode_recording",
            "title": "Decode a recording",
            "description": "Work out what is in an IQ or audio recording and decode it",
            "arguments": [
                {"name": "file", "description": "The file (WAV, SigMF .sigmf-meta, or raw IQ) in the project directory", "required": true},
                {"name": "hint", "description": "Anything known: sample rate, centre frequency, expected signal", "required": false},
            ],
        }),
    ]
}

pub fn prompt(name: &str, args: &Value) -> Option<Value> {
    let arg = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
    let device = match arg("device") {
        "" => "the first device `list_devices` finds (or `sim:` if none)".to_owned(),
        d => format!("the device `{d}`"),
    };
    let (description, text) = match name {
        "build_receiver" => (
            "Build a receiver",
            format!(
                "Build a foxygnal graph that receives {signal} with {device}, run it, and check \
                 that it works.\n\n\
                 Start from the closest example (`list_examples`, `get_example` with open=true) \
                 rather than from scratch. Use `describe_block` before setting a block's \
                 settings. After each `edit_graph`, read the reply: fix build errors and \
                 unconnected inputs before running. Once running, use `measure` on the wires \
                 that matter (spectrum before and after the channel filter, level of the \
                 demodulated audio, or the decoder's messages) to confirm the signal is there. \
                 Tell me what you built and what you saw.",
                signal = arg("signal")
            ),
        ),
        "find_signals" => (
            "Find signals in a band",
            format!(
                "Survey {band} with {device}.\n\n\
                 Build a graph of an SDR source into a null sink (or open the `two-tones` \
                 example and replace its source). For each step across the band: set the \
                 source's frequency (a live setting), wait a moment, and `measure` the wire \
                 out of the source as a spectrum. Collect the signals it reports, with their \
                 frequencies and bandwidths. Then say what each one probably is, from \
                 its frequency, width and shape, and which foxygnal decoder (if any) could \
                 decode it.",
                band = arg("band")
            ),
        ),
        "decode_recording" => (
            "Decode a recording",
            format!(
                "Find out what is in the recording `{file}` and decode it if foxygnal can. \
                 {hint}\n\n\
                 Open it with the matching source block (WAV Source, SigMF Source, or File \
                 Source for raw IQ, where you must give the sample rate and type). Add a \
                 Throttle so it plays in real time, run, and `measure` its spectrum. From what \
                 you see, build the demodulator and decoder chain, and read its output with \
                 `measure` (messages) or `watch` (pictures).",
                file = arg("file"),
                hint = match arg("hint") {
                    "" => String::new(),
                    h => format!("Known: {h}."),
                }
            ),
        ),
        _ => return None,
    };
    Some(json!({
        "description": description,
        "messages": [{"role": "user", "content": {"type": "text", "text": text}}],
    }))
}
