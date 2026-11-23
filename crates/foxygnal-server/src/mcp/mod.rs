//! A Model Context Protocol server: lets an AI assistant build graphs, run
//! them and look at signals, through the same session the browser edits.
//!
//! Two transports share one implementation ([`Mcp::handle`]):
//!
//! - **Streamable HTTP** at `/api/mcp` on `foxygnal serve`: the session the
//!   browser shows, so an assistant's edits appear live in the editor (and
//!   can be undone there). Plain JSON responses, no server-sent events; the
//!   server's Host / Origin / token checks apply as to every other endpoint.
//! - **stdio** (`foxygnal mcp`): a headless session of its own, for
//!   assistants that start their tools as subprocesses.
//!
//! What it can't do, on purpose: arm TX (a graph that transmits is refused
//! like any unarmed run), or read and write files outside the project
//! directory with its file tools.

mod docs;
mod probes;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::AppState;
use crate::session::Session;
pub use probes::MCP_CLIENT;

/// Protocol versions we speak, newest first. A client asking for one of these
/// gets it; anything else gets the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub struct Mcp {
    session: Arc<Session>,
    project_dir: PathBuf,
    probes: Arc<probes::Probes>,
    watches: tools::Watches,
}

/// A JSON-RPC error.
struct RpcError(i64, String);

impl RpcError {
    fn method(m: &str) -> Self {
        Self(-32601, format!("unknown method `{m}`"))
    }
    fn params(msg: impl Into<String>) -> Self {
        Self(-32602, msg.into())
    }
}

impl Mcp {
    /// Must be called inside a tokio runtime (it starts a task that takes
    /// probe frames).
    pub fn new(session: Arc<Session>, project_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            probes: probes::Probes::start(&session),
            session,
            project_dir,
            watches: tools::Watches::default(),
        })
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Handle one JSON-RPC message (or a batch). `None` when there is nothing
    /// to send back (notifications, responses).
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        if let Value::Array(batch) = msg {
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = Box::pin(self.handle(m)).await {
                    out.push(r);
                }
            }
            return (!out.is_empty()).then_some(Value::Array(out));
        }
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to a request of ours (we send none) needs no answer;
            // anything else with an id is malformed.
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            return id
                .filter(|_| !is_response)
                .map(|id| error_response(id, &RpcError(-32600, "not a JSON-RPC request".into())));
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = id?; // notifications (`notifications/initialized`, …) need no answer
        Some(match self.request(method, params).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => error_response(id, &e),
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or_default();
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {
                        "tools": {"listChanged": false},
                        "resources": {"listChanged": false, "subscribe": false},
                        "prompts": {"listChanged": false},
                    },
                    "serverInfo": {
                        "name": "foxygnal",
                        "title": "foxygnal SDR flowgraphs",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": docs::INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::list()})),
            "tools/call" => {
                let name = params["name"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("`name` is missing"))?;
                if !tools::exists(name) {
                    return Err(RpcError::params(format!("unknown tool `{name}`")));
                }
                let args = match &params["arguments"] {
                    Value::Null => json!({}),
                    a => a.clone(),
                };
                Ok(tools::call(self, name, &args).await.into_json())
            }
            "resources/list" => Ok(json!({"resources": docs::resources()})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "resources/read" => {
                let uri = params["uri"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("`uri` is missing"))?;
                let (mime, text) = docs::read(self, uri)
                    .ok_or_else(|| RpcError(-32002, format!("no resource `{uri}`")))?;
                Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
            }
            "prompts/list" => Ok(json!({"prompts": docs::prompts()})),
            "prompts/get" => {
                let name = params["name"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("`name` is missing"))?;
                docs::prompt(name, &params["arguments"])
                    .ok_or_else(|| RpcError::params(format!("unknown prompt `{name}`")))
            }
            // We don't send log messages; accept the level anyway.
            "logging/setLevel" => Ok(json!({})),
            m => Err(RpcError::method(m)),
        }
    }
}

fn error_response(id: Value, e: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.0, "message": e.1}})
}

fn parse_error(e: &serde_json::Error) -> Value {
    error_response(Value::Null, &RpcError(-32700, format!("not JSON: {e}")))
}

// ---- Streamable HTTP ----------------------------------------------------------

/// `POST /api/mcp`: one JSON-RPC message (or batch) in, its answer out as
/// JSON, or 202 if there is none.
pub(crate) async fn http(State(s): State<Arc<AppState>>, body: Bytes) -> Response {
    let reply = match serde_json::from_slice::<Value>(&body) {
        Ok(msg) => s.mcp.handle(msg).await,
        Err(e) => Some(parse_error(&e)),
    };
    match reply {
        Some(v) => (
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&v).unwrap_or_default(),
        )
            .into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// `GET` / `DELETE /api/mcp`: no server-sent event stream, no sessions.
pub(crate) async fn http_other() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "This MCP endpoint answers POST requests only (no event stream).",
    )
        .into_response()
}

// ---- stdio --------------------------------------------------------------------

/// Serve MCP over newline-delimited JSON on `input` / `output` until the
/// input ends. Requests are handled one at a time, in order (a client that
/// sends `set_graph` then `run` without waiting must get them in that
/// order); each reply is written as one line.
pub async fn serve_stdio<R, W>(mcp: Arc<Mcp>, input: R, mut output: W) -> std::io::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: std::io::Write,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = input.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => mcp.handle(msg).await,
            Err(e) => Some(parse_error(&e)),
        };
        if let Some(v) = reply {
            let mut text = serde_json::to_string(&v).unwrap_or_default();
            text.push('\n');
            output.write_all(text.as_bytes())?;
            output.flush()?;
        }
    }
    Ok(())
}
