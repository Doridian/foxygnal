//! REST endpoints: the block catalog and filter previews, graph get/put
//! (JSON and TOML), bundled examples, and the server-side project directory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use foxygnal_core::{FrequencyResponse, GraphSpec, Params};
use foxygnal_sdr::Discovery;
use serde::{Deserialize, Serialize};

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

/// Block kinds for the palette: built in (with their `response` flag), and
/// graph files with exports.
pub async fn blocks(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let session = s.session.clone();
    let all = tokio::task::spawn_blocking(move || {
        let reg = session.registry();
        let to_json = |v: Result<serde_json::Value, _>| v.expect("descriptors serialize");
        let mut all: Vec<serde_json::Value> = reg
            .catalog()
            .map(|e| to_json(serde_json::to_value(e)))
            .collect();
        all.extend(
            session
                .graphs()
                .descriptors(reg)
                .into_iter()
                .map(|g| to_json(serde_json::to_value(g))),
        );
        all
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(all))
}

#[derive(Deserialize)]
pub struct ResponseRequest {
    #[serde(default)]
    params: Params,
    /// The block's input sample rate.
    sample_rate: f64,
}

/// The frequency response a block (whose descriptor says `response: true`)
/// would have with these parameters at this input rate, for the inspector's
/// filter preview.
pub async fn block_response(
    State(s): State<Arc<AppState>>,
    UrlPath(kind): UrlPath<String>,
    Json(req): Json<ResponseRequest>,
) -> Result<Json<FrequencyResponse>, ApiError> {
    if !(req.sample_rate > 0.0 && req.sample_rate.is_finite()) {
        return Err(bad_request("sample_rate must be a positive number"));
    }
    if s.session.registry().get(&kind).is_none() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown block kind `{kind}`"),
        ));
    }
    // Long filters take a while to evaluate: keep it off the async workers.
    let session = s.session.clone();
    let kind2 = kind.clone();
    let result = tokio::task::spawn_blocking(move || {
        session
            .registry()
            .response(&kind2, &req.params, req.sample_rate)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match result {
        Ok(Some(r)) => Ok(Json(r)),
        Ok(None) => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("`{kind}` has no frequency response"),
        )),
        Err(e) => Err(bad_request(e.detail())),
    }
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

// ---- Devices ----------------------------------------------------------------

/// How long one driver may take to list its devices.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(4);

/// Drivers whose discovery is still running (from an earlier request that
/// timed out). They are not asked again until it returns.
static DISCOVERING: Mutex<Vec<&str>> = Mutex::new(Vec::new());

/// SDR devices found by every driver. Each driver runs on a blocking thread
/// with a timeout, so a slow or stuck one can't hang the request; its error
/// (library not installed, timed out, …) comes back in its entry.
pub async fn devices() -> Json<Vec<Discovery>> {
    Json(discover().await)
}

/// Ask every driver for its devices (each with a timeout), in parallel.
pub(crate) async fn discover() -> Vec<Discovery> {
    let lock = || DISCOVERING.lock().unwrap_or_else(|e| e.into_inner());
    let tasks = foxygnal_sdr::drivers().iter().map(|d| {
        let (driver, label) = (d.name(), d.label());
        let result = |devices, error| Discovery {
            driver,
            label,
            devices,
            error,
        };
        let busy = {
            let mut l = lock();
            let busy = l.contains(&driver);
            if !busy {
                l.push(driver);
            }
            busy
        };
        let d = d.clone();
        async move {
            if busy {
                return result(vec![], Some("still busy with an earlier search".into()));
            }
            let task = tokio::task::spawn_blocking(move || {
                let r = d.discover();
                lock().retain(|n| *n != driver);
                r
            });
            match tokio::time::timeout(DISCOVERY_TIMEOUT, task).await {
                Ok(Ok(Ok(devices))) => result(devices, None),
                Ok(Ok(Err(e))) => result(vec![], Some(e.to_string())),
                Ok(Err(_)) => {
                    lock().retain(|n| *n != driver);
                    result(vec![], Some("the driver crashed while searching".into()))
                }
                Err(_) => result(vec![], Some("timed out while searching".into())),
            }
        }
    });
    futures_util::future::join_all(tasks).await
}

// ---- Examples -------------------------------------------------------------

pub(crate) struct Example {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub toml: &'static str,
}

pub(crate) const EXAMPLES: &[Example] = &[
    Example {
        name: "two-tones",
        title: "Two tones + noise → instruments",
        description: "Runs until stopped. Click a wire to watch it on a scope, spectrum, waterfall or constellation.",
        toml: include_str!("../../../examples/two-tones.fox.toml"),
    },
    Example {
        name: "fm-live",
        title: "Live FM radio",
        description: "An SDR in, audio in this tab. Uses the first device found, or a simulated FM band. Change `station` while it runs.",
        toml: include_str!("../../../examples/fm-live.fox.toml"),
    },
    Example {
        name: "fm-autotune",
        title: "Auto-tuning FM",
        description: "Live FM that latches onto the strongest station in the band: a peak finder wired into the channel filter's offset setting. Uses the first device found, or a simulated FM band.",
        toml: include_str!("../../../examples/fm-autotune.fox.toml"),
    },
    Example {
        name: "fm-stereo-rds",
        title: "FM stereo + RDS",
        description: "Live FM in stereo in this tab, plus the station's RDS name, RadioText and clock in a message log. Uses the first device found, or a simulated FM band.",
        toml: include_str!("../../../examples/fm-stereo-rds.fox.toml"),
    },
    Example {
        name: "adsb",
        title: "ADS-B aircraft decoder",
        description: "An SDR at 1090 MHz, 2 MS/s → ADS-B decoder. Needs a real receiver. Click the wire into the log and choose Messages to read call signs, altitudes and positions.",
        toml: include_str!("../../../examples/adsb.fox.toml"),
    },
    Example {
        name: "pocsag",
        title: "POCSAG pager decoder",
        description: "An SDR on a paging channel → FM demod → POCSAG decoder (512/1200/2400 Bd, numeric and alphanumeric). Needs a real receiver; set `freq` to a paging channel in your area.",
        toml: include_str!("../../../examples/pocsag.fox.toml"),
    },
    Example {
        name: "aprs",
        title: "APRS packet decoder",
        description: "An SDR at 144.39 MHz (144.8 MHz in Europe) → FM demod → AFSK 1200 → HDLC/AX.25 → APRS positions, messages and objects. Needs a real receiver.",
        toml: include_str!("../../../examples/aprs.fox.toml"),
    },
    Example {
        name: "sstv",
        title: "SSTV pictures",
        description: "An SDR at 145.8 MHz (the ISS during ARISS events) → FM demod → SSTV decoder (Robot, Martin, Scottie, PD) → the Image instrument and PNG files. Needs a real receiver.",
        toml: include_str!("../../../examples/sstv.fox.toml"),
    },
    Example {
        name: "sstv-wav",
        title: "SSTV from a recording",
        description: "A WAV file of SSTV audio → SSTV decoder → the Image instrument and PNG files. Choose the file in `wav`.",
        toml: include_str!("../../../examples/sstv-wav.fox.toml"),
    },
    Example {
        name: "fm-receiver",
        title: "FM broadcast receiver",
        description: "An IQ recording in, a WAV file of the station out. Set the file, its rate and centre frequency, and the station offset.",
        toml: include_str!("../../../examples/fm-receiver.fox.toml"),
    },
    Example {
        name: "hier-tones",
        title: "Variables + sub-graphs",
        description: "Two copies of a tone + noise sub-graph, tuned from graph variables. Change `base` while it runs.",
        toml: include_str!("../../../examples/hier-tones.fox.toml"),
    },
    Example {
        name: "add-constant",
        title: "Constant inputs + wired settings",
        description: "An adder with a constant on one input (change `dc` while it runs), and a slow sine wired into a gain's setting. No hardware needed.",
        toml: include_str!("../../../examples/add-constant.fox.toml"),
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

pub(crate) const EXT: &str = ".fox.toml";
pub(crate) const GRC_EXT: &str = ".grc";

/// [`project_file_ext`] for code outside the HTTP handlers (MCP).
pub(crate) fn project_path(dir: &Path, name: &str, ext: &str) -> Result<PathBuf, String> {
    project_file_ext(dir, name, ext).map_err(|e| e.1)
}

/// A file name in the project dir: no paths, no hidden files, `.fox.toml`.
fn project_file(dir: &Path, name: &str) -> Result<PathBuf, ApiError> {
    project_file_ext(dir, name, EXT)
}

/// A file name in the project dir ending with `ext`.
fn project_file_ext(dir: &Path, name: &str, ext: &str) -> Result<PathBuf, ApiError> {
    let stem = name.strip_suffix(ext).unwrap_or_default();
    let ok = !stem.is_empty()
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
    if !ok {
        return Err(bad_request(format!(
            "`{name}` is not a valid file name: use letters, digits, `-`, `_`, `.` and end with {ext}"
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
            // GNU Radio flowgraphs are listed too: opening one imports it.
            let listed = name.ends_with(EXT) || name.ends_with(GRC_EXT);
            if !listed || !entry.file_type()?.is_file() {
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
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(t) => t,
        // Bundled sub-graphs open like project files (and save into it).
        Err(e) => crate::graphs::library(&name)
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("{name}: {e}")))?
            .to_owned(),
    };
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

// ---- GNU Radio import ---------------------------------------------------------

#[derive(Serialize)]
pub struct Imported {
    graph: GraphSpec,
    /// What didn't convert exactly, one message each.
    warnings: Vec<String>,
    title: Option<String>,
    /// The `.fox.toml` name to save it as (next to the `.grc`, which is
    /// never written).
    name: String,
}

fn import_grc(text: &str, grc_name: &str) -> Result<Imported, ApiError> {
    let imp = foxygnal_grc::import(text, &foxygnal_grc::Options::default())
        .map_err(|e| bad_request(format!("{grc_name}: {e}")))?;
    let stem = grc_name.strip_suffix(GRC_EXT).unwrap_or(grc_name);
    Ok(Imported {
        graph: imp.graph,
        warnings: imp.warnings,
        title: imp.title,
        name: format!("{stem}{EXT}"),
    })
}

/// Convert a `.grc` in the project dir (same name rules as `.fox.toml`).
pub async fn import_file(
    State(s): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<Imported>, ApiError> {
    let path = project_file_ext(&s.project_dir, &name, GRC_EXT)?;
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{name}: {e}")))?;
    import_grc(&text, &name).map(Json)
}

/// Convert an uploaded `.grc` (the body) without touching the session.
pub async fn import_upload(body: String) -> Result<Json<Imported>, ApiError> {
    import_grc(&body, "upload.grc").map(Json)
}

// ---- GNU Radio export ---------------------------------------------------------

#[derive(Deserialize)]
pub struct ExportQuery {
    /// The graph's `.fox.toml` name, for the `.grc` name and options id.
    name: Option<String>,
}

#[derive(Serialize)]
pub struct Exported {
    /// GNU Radio Companion 3.10 YAML.
    grc: String,
    /// What didn't convert exactly, one message each.
    warnings: Vec<String>,
    /// The `.grc` name to download it as.
    name: String,
}

/// Convert a graph (the body) to a `.grc`; sub-graphs load from the project.
pub async fn export_grc(
    State(s): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<ExportQuery>,
    Json(g): Json<GraphSpec>,
) -> Result<Json<Exported>, ApiError> {
    let file = q.name.unwrap_or_else(|| "flowgraph.fox.toml".into());
    let name = foxygnal_grc::grc_name(Path::new(&file))
        .to_string_lossy()
        .into_owned();
    let stem = name.strip_suffix(GRC_EXT).unwrap_or(&name).to_owned();
    let loader = crate::graphs::ProjectGraphs {
        dir: s.project_dir.clone(),
    };
    let opts = foxygnal_grc::ExportOptions {
        loader: Some(&loader),
        name: Some(stem),
    };
    let exp = foxygnal_grc::export(&g, &opts).map_err(bad_request)?;
    Ok(Json(Exported {
        grc: exp.grc,
        warnings: exp.warnings,
        name,
    }))
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
        assert!(project_file_ext(d, "fm rx.grc", GRC_EXT).is_ok());
        for bad in ["../fm.grc", "fm.fox.toml", ".grc", "a/b.grc", ".x.grc"] {
            assert!(project_file_ext(d, bad, GRC_EXT).is_err(), "{bad}");
        }
    }

    #[test]
    fn examples_parse() {
        for e in EXAMPLES {
            GraphSpec::from_toml(e.toml).unwrap_or_else(|err| panic!("{}: {err}", e.name));
        }
        for (name, toml) in crate::graphs::LIBRARY {
            GraphSpec::from_toml(toml).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }
}
