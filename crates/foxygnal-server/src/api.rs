//! REST endpoints: graph get/put (JSON and TOML), bundled examples, and the
//! server-side project directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use foxygnal_core::{BlockDescriptor, GraphSpec};
use serde::Serialize;

use crate::AppState;

/// Client id used for edits that arrive over REST rather than a WebSocket.
const REST_CLIENT: u64 = 0;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn bad_request(e: impl ToString) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, e.to_string())
}

#[derive(Serialize)]
pub struct Health {
    name: &'static str,
    version: &'static str,
}

pub async fn health() -> Json<Health> {
    Json(Health {
        name: "foxygnal",
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub async fn blocks(State(s): State<Arc<AppState>>) -> Json<Vec<BlockDescriptor>> {
    Json(s.session.registry().descriptors().cloned().collect())
}

pub async fn get_graph(State(s): State<Arc<AppState>>) -> Json<GraphSpec> {
    Json(s.session.graph())
}

pub async fn put_graph(State(s): State<Arc<AppState>>, Json(g): Json<GraphSpec>) -> StatusCode {
    s.session.set_graph(REST_CLIENT, g, s.session.name());
    StatusCode::NO_CONTENT
}

const TOML_TYPE: &str = "application/toml; charset=utf-8";

pub async fn get_graph_toml(State(s): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let text = s.session.graph().to_toml().map_err(bad_request)?;
    Ok(([(header::CONTENT_TYPE, TOML_TYPE)], text).into_response())
}

/// Parse a `.fox.toml` into graph JSON without touching the session (used
/// for uploads, so the client can load it like any other graph).
pub async fn parse_toml(body: String) -> Result<Json<GraphSpec>, ApiError> {
    GraphSpec::from_toml(&body).map(Json).map_err(bad_request)
}

/// Render graph JSON as `.fox.toml` (used for downloads).
pub async fn render_toml(Json(g): Json<GraphSpec>) -> Result<Response, ApiError> {
    let text = g.to_toml().map_err(bad_request)?;
    Ok(([(header::CONTENT_TYPE, TOML_TYPE)], text).into_response())
}

// ---- Examples -------------------------------------------------------------

struct Example {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    toml: &'static str,
}

const EXAMPLES: &[Example] = &[
    Example {
        name: "two-tones",
        title: "Two tones + noise → instruments",
        description: "Runs until stopped. Click a wire to watch it on a scope, spectrum, waterfall or constellation.",
        toml: include_str!("../../../examples/two-tones.fox.toml"),
    },
    Example {
        name: "noisy-tone",
        title: "Tone + noise → file",
        description: "A 1 kHz tone plus noise, throttled to real time, two seconds written to disk. No hardware needed.",
        toml: include_str!("../../../examples/noisy-tone.fox.toml"),
    },
];

#[derive(Serialize)]
pub struct ExampleInfo {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    blocks: usize,
}

pub async fn examples() -> Json<Vec<ExampleInfo>> {
    Json(
        EXAMPLES
            .iter()
            .map(|e| ExampleInfo {
                name: e.name,
                title: e.title,
                description: e.description,
                blocks: GraphSpec::from_toml(e.toml).map_or(0, |g| g.blocks.len()),
            })
            .collect(),
    )
}

pub async fn example(UrlPath(name): UrlPath<String>) -> Result<Json<GraphSpec>, ApiError> {
    let e = EXAMPLES
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no example `{name}`")))?;
    GraphSpec::from_toml(e.toml)
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---- Project directory ----------------------------------------------------

const EXT: &str = ".fox.toml";

/// A file name in the project dir: no paths, no hidden files, `.fox.toml`.
fn project_file(dir: &Path, name: &str) -> Result<PathBuf, ApiError> {
    let stem = name.strip_suffix(EXT).unwrap_or_default();
    let ok = !stem.is_empty()
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
    if !ok {
        return Err(bad_request(format!(
            "`{name}` is not a valid file name: use letters, digits, `-`, `_`, `.` and end with {EXT}"
        )));
    }
    Ok(dir.join(name))
}

#[derive(Serialize)]
pub struct FileInfo {
    name: String,
    /// Unix time in ms.
    modified: u64,
}

pub async fn list_files(State(s): State<Arc<AppState>>) -> Result<Json<Vec<FileInfo>>, ApiError> {
    let dir = s.project_dir.clone();
    let mut files = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<FileInfo>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.ends_with(EXT) || !entry.file_type()?.is_file() {
                continue;
            }
            let modified = entry
                .metadata()?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as u64);
            out.push(FileInfo { name, modified });
        }
        Ok(out)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    Ok(Json(files))
}

pub async fn get_file(
    State(s): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<GraphSpec>, ApiError> {
    let path = project_file(&s.project_dir, &name)?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{name}: {e}")))?;
    GraphSpec::from_toml(&text)
        .map(Json)
        .map_err(|e| bad_request(format!("{name}: {e}")))
}

pub async fn put_file(
    State(s): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    Json(g): Json<GraphSpec>,
) -> Result<StatusCode, ApiError> {
    let path = project_file(&s.project_dir, &name)?;
    let text = g.to_toml().map_err(bad_request)?;
    tokio::fs::write(&path, text)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{name}: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_file_names() {
        let d = Path::new("/p");
        assert!(project_file(d, "fm radio-2.fox.toml").is_ok());
        for bad in [
            "../x.fox.toml",
            "a/b.fox.toml",
            ".hidden.fox.toml",
            ".fox.toml",
            "x.toml",
            "x.fox.toml/..",
            "C:\\x.fox.toml",
        ] {
            assert!(project_file(d, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn examples_parse() {
        for e in EXAMPLES {
            GraphSpec::from_toml(e.toml).unwrap_or_else(|err| panic!("{}: {err}", e.name));
        }
    }
}
