//! Recording wires of the running graph to SigMF, and the recordings in the
//! project directory.
//!
//! A recording attaches a lossless read cursor to the wire's output
//! ([`Tap::attach`](foxygnal_core::Tap::attach)), so it holds the graph back
//! like any downstream block instead of dropping samples, and it gets the
//! stream's tags, which become capture segments and annotations
//! ([`SigmfWriter`]). Each recording has its own thread. Files go to
//! `recordings/` in the project directory, named by the server (or by the
//! client, validated: plain names only).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use foxygnal_blocks::sigmf::{self, RecordingInfo, SigmfWriter};
use foxygnal_core::{GraphSpec, ParamKind, Registry, StreamType, Tag, TapReader};
use serde::Serialize;

use crate::AppState;
use crate::session::{EdgeRef, Session};

/// The project subdirectory recordings go to.
pub const DIR: &str = "recordings";

/// How often a recording reports its progress.
const REPORT_EVERY: Duration = Duration::from_millis(500);
/// How long the recorder sleeps when the wire has nothing new.
const IDLE: Duration = Duration::from_millis(2);

/// A recording's progress, sent as the `record` event (and in `hello`).
#[derive(Debug, Clone, Serialize)]
pub struct RecordStatus {
    pub edge: EdgeRef,
    /// The recording's name (file names without `.sigmf-meta` / `-data`).
    pub name: String,
    /// The metadata file relative to the project directory, ready for a
    /// SigMF Source's `path`.
    pub path: String,
    /// Unix time in ms.
    pub started_at: u64,
    pub elapsed_ms: u64,
    pub samples: u64,
    pub bytes: u64,
    /// Stretches of samples lost (only possible right after starting).
    pub gaps: u64,
    /// Annotations written so far (overflows, gaps, packets, other tags).
    pub annotations: u64,
    /// The recording has ended (stopped, the run ended, or an error).
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RecordStatus {
    /// A recording that couldn't start.
    pub fn failed(edge: EdgeRef, error: String) -> Self {
        Self {
            edge,
            name: String::new(),
            path: String::new(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            elapsed_ms: 0,
            samples: 0,
            bytes: 0,
            gaps: 0,
            annotations: 0,
            done: true,
            error: Some(error),
        }
    }
}

struct Active {
    edge: EdgeRef,
    name: String,
    started_at: u64,
    started: Instant,
    samples: AtomicU64,
    bytes: AtomicU64,
    gaps: AtomicU64,
    annotations: AtomicU64,
    stop: AtomicBool,
}

impl Active {
    fn status(&self, done: bool, error: Option<String>) -> RecordStatus {
        RecordStatus {
            edge: self.edge.clone(),
            name: self.name.clone(),
            path: format!("{DIR}/{}.sigmf-meta", self.name),
            started_at: self.started_at,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            samples: self.samples.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            gaps: self.gaps.load(Ordering::Relaxed),
            annotations: self.annotations.load(Ordering::Relaxed),
            done,
            error,
        }
    }
}

fn edge_key(e: &EdgeRef) -> String {
    format!("{}->{}", e.from, e.to)
}

/// The session's recordings in progress, one per wire.
#[derive(Default)]
pub struct Recorders {
    active: Mutex<HashMap<String, Arc<Active>>>,
}

impl Recorders {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Active>>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn statuses(&self) -> Vec<RecordStatus> {
        let mut v: Vec<_> = self
            .lock()
            .values()
            .map(|a| a.status(false, None))
            .collect();
        v.sort_by_key(|s| s.started_at);
        v
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// A recording with this name is being written.
    pub fn is_active(&self, name: &str) -> bool {
        self.lock().values().any(|a| a.name == name)
    }

    pub fn stop(&self, edge: &EdgeRef) {
        if let Some(a) = self.lock().get(&edge_key(edge)) {
            a.stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn stop_all(&self) {
        for a in self.lock().values() {
            a.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// What [`start`] needs from the running graph.
pub struct Source {
    pub reader: TapReader,
    pub ty: StreamType,
    /// Items per read: the wire's buffer capacity.
    pub capacity: usize,
    pub sample_rate: Option<f64>,
    pub frequency: Option<f64>,
    pub hw: Option<String>,
    /// The graph's file name, for the generated recording name.
    pub graph: Option<String>,
}

/// Start recording `edge` from `src` into the project's `recordings/`.
pub fn start(
    session: &Arc<Session>,
    project_dir: &Path,
    edge: EdgeRef,
    name: Option<String>,
    src: Source,
) -> Result<(), String> {
    let recorders = session.recorders();
    if recorders.lock().contains_key(&edge_key(&edge)) {
        return Err("this wire is already being recorded".into());
    }
    let dir = project_dir.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let name = match name {
        Some(n) => {
            let n = valid_name(&n)?.to_owned();
            if dir.join(format!("{n}.sigmf-meta")).exists() || recorders.is_active(&n) {
                return Err(format!("a recording named `{n}` already exists"));
            }
            n
        }
        None => free_name(&dir, &auto_name(src.graph.as_deref(), &edge), recorders),
    };
    let info = RecordingInfo {
        complex: false,
        sample_rate: src.sample_rate,
        frequency: src.frequency,
        hw: src.hw,
        description: Some(format!("{} → {}", edge.from, edge.to)),
    };
    let writer = SigmfWriter::create(&dir.join(&name), src.ty, &info).map_err(|e| e.to_string())?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let active = Arc::new(Active {
        edge: edge.clone(),
        name,
        started_at: started,
        started: Instant::now(),
        samples: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        gaps: AtomicU64::new(0),
        annotations: AtomicU64::new(0),
        stop: AtomicBool::new(false),
    });
    recorders.lock().insert(edge_key(&edge), active.clone());
    session.emit_record(active.status(false, None));
    let weak = Arc::downgrade(session);
    let buf = vec![0u8; src.capacity.max(1024) * src.ty.item_size()];
    std::thread::Builder::new()
        .name("fox:record".into())
        .spawn(move || record(weak, active, src.reader, writer, buf))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// The recorder thread: copy everything the wire carries into the file
/// until stopped or the stream ends.
fn record(
    session: Weak<Session>,
    active: Arc<Active>,
    mut reader: TapReader,
    mut w: SigmfWriter,
    mut buf: Vec<u8>,
) {
    let size = reader.item_size();
    // Stream index of the recording's first item, and items lost since.
    let mut base = reader.position();
    let mut lost = 0u64;
    let mut pending: Vec<Tag> = Vec::new();
    let mut reported = Instant::now();
    let emit = |st: RecordStatus| {
        if let Some(s) = session.upgrade() {
            s.emit_record(st);
        }
    };
    let result = loop {
        if active.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        let got = reader.read(&mut buf);
        let start = got.start as u64;
        let expected = base + lost + w.items();
        if start > expected {
            if w.items() == 0 {
                base = start;
            } else {
                w.gap(w.items(), start - expected);
                lost += start - expected;
                active.gaps.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Tags for every item read are queued by now; later ones wait.
        while let Some(t) = reader.pop_tag() {
            pending.push(t);
        }
        let end = start + got.items as u64;
        if !pending.is_empty() {
            pending.sort_by_key(|t| t.offset);
            let ready = pending.partition_point(|t| t.offset < end);
            for t in pending.drain(..ready) {
                // Tags on items recorded earlier were handled then; older
                // ones are from before the recording or in a lost stretch.
                if t.offset >= start {
                    w.tag(t.offset - base - lost, &t.key, &t.value);
                }
            }
        }
        active
            .annotations
            .store(w.annotations() as u64, Ordering::Relaxed);
        if got.items > 0 {
            if let Err(e) = w.write(&buf[..got.items * size]) {
                break Err(e.to_string());
            }
            active.samples.store(w.samples(), Ordering::Relaxed);
            active.bytes.store(w.bytes(), Ordering::Relaxed);
        } else if reader.finished() {
            break Ok(());
        } else {
            std::thread::park_timeout(IDLE);
        }
        if let Err(e) = w.checkpoint() {
            break Err(e.to_string());
        }
        if reported.elapsed() >= REPORT_EVERY {
            reported = Instant::now();
            emit(active.status(false, None));
        }
    };
    // Let the graph go before the (slower) final write.
    drop(reader);
    let result = result.and_then(|()| w.finish().map_err(|e| e.to_string()));
    if let Some(s) = session.upgrade() {
        s.recorders().lock().remove(&edge_key(&active.edge));
        s.emit_record(active.status(true, result.err()));
    }
}

/// A recording name from the client: a plain file name (letters, digits,
/// `-`, `_`, `.`), optionally ending in `.sigmf-meta` / `.sigmf-data` /
/// `.sigmf`. Returns it without the suffix.
pub fn valid_name(name: &str) -> Result<&str, String> {
    let base = [".sigmf-meta", ".sigmf-data", ".sigmf"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
        .unwrap_or(name);
    let ok = !base.is_empty()
        && base.len() <= 128
        && base
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && !base.contains("..")
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(base)
    } else {
        Err(format!(
            "`{name}` is not a valid recording name: use letters, digits, `-`, `_` and `.`"
        ))
    }
}

/// Keep letters, digits, `-` and `_`; anything else becomes `_`.
fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    s.trim_matches('_').to_owned()
}

/// `<graph>-<wire>-<UTC time>`, e.g. `fm-live-rx_out-20261122T134500Z`.
fn auto_name(graph: Option<&str>, edge: &EdgeRef) -> String {
    let graph = graph
        .map(|g| g.strip_suffix(".fox.toml").unwrap_or(g))
        .map(slug)
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| "untitled".into());
    let mut wire = slug(&edge.from);
    if wire.is_empty() {
        wire = "wire".into();
    }
    // 2026-11-22T13:45:00.123Z → 20261122T134500Z
    let t: String = sigmf::iso8601_now()
        .chars()
        .take(19)
        .filter(|c| *c != '-' && *c != ':')
        .collect();
    format!("{graph}-{wire}-{t}Z")
}

/// `name`, or `name-2`, `name-3`… if taken.
fn free_name(dir: &Path, name: &str, recorders: &Recorders) -> String {
    let taken = |n: &str| dir.join(format!("{n}.sigmf-meta")).exists() || recorders.is_active(n);
    if !taken(name) {
        return name.to_owned();
    }
    (2..)
        .map(|i| format!("{name}-{i}"))
        .find(|n| !taken(n))
        .expect("some suffix is free")
}

/// The device of the nearest block upstream of `block` (in a resolved graph)
/// that has a device parameter, for `core:hw`.
pub fn upstream_device(graph: &GraphSpec, registry: &Registry, block: &str) -> Option<String> {
    let mut queue = vec![block.to_owned()];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let b = graph.blocks.iter().find(|b| b.id == id)?;
        if let Some(desc) = registry.get(&b.kind) {
            for p in desc
                .params
                .iter()
                .filter(|p| matches!(p.kind, ParamKind::Device))
            {
                let v = b.params.get(p.name).unwrap_or(&p.default);
                if let Some(d) = v.as_str() {
                    let d = if d.is_empty() {
                        "first device found"
                    } else {
                        d
                    };
                    return Some(format!("{} ({d})", desc.label));
                }
            }
        }
        let prefix = format!("{id}.");
        for c in graph
            .connections
            .iter()
            .filter(|c| c.to.starts_with(&prefix))
        {
            if let Some((from, _)) = c.from.rsplit_once('.') {
                queue.insert(0, from.to_owned());
            }
        }
    }
    None
}

// ---- REST: the recordings in the project directory -------------------------

pub struct ApiError(StatusCode, String);

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, self.1).into_response()
    }
}

fn internal(e: impl ToString) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// One recording as `GET /api/recordings` lists it.
#[derive(Debug, Serialize)]
pub struct RecordingInfoJson {
    pub name: String,
    /// Relative to the project directory (a SigMF Source's `path`).
    pub path: String,
    /// Unix time in ms.
    pub modified: u64,
    pub datatype: Option<String>,
    pub sample_rate: Option<f64>,
    pub frequency: Option<f64>,
    /// Samples in the data file.
    pub samples: u64,
    pub bytes: u64,
    pub captures: usize,
    pub annotations: usize,
    pub datetime: Option<String>,
    pub hw: Option<String>,
    pub description: Option<String>,
    /// Being written right now.
    pub recording: bool,
    /// Why the metadata can't be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn describe(dir: &Path, name: &str, recorders: &Recorders) -> RecordingInfoJson {
    let (meta_path, data_path) = sigmf::file_pair(&dir.join(name));
    let modified = std::fs::metadata(&meta_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    let bytes = std::fs::metadata(&data_path).map_or(0, |m| m.len());
    let mut info = RecordingInfoJson {
        name: name.to_owned(),
        path: format!("{DIR}/{name}.sigmf-meta"),
        modified,
        datatype: None,
        sample_rate: None,
        frequency: None,
        samples: 0,
        bytes,
        captures: 0,
        annotations: 0,
        datetime: None,
        hw: None,
        description: None,
        recording: recorders.is_active(name),
        error: None,
    };
    let text = match std::fs::read_to_string(&meta_path) {
        Ok(t) => t,
        Err(e) => {
            info.error = Some(e.to_string());
            return info;
        }
    };
    match sigmf::parse_meta(&text) {
        Ok(m) => {
            info.sample_rate = m.sample_rate;
            info.frequency = m.frequency;
            info.samples = bytes / m.datatype.sample_size() as u64;
        }
        Err(e) => info.error = Some(e),
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        let s = |v: &serde_json::Value| v.as_str().map(str::to_owned);
        let g = &v["global"];
        info.datatype = s(&g["core:datatype"]);
        info.hw = s(&g["core:hw"]);
        info.description = s(&g["core:description"]);
        info.datetime = s(&v["captures"][0]["core:datetime"]);
        info.captures = v["captures"].as_array().map_or(0, Vec::len);
        info.annotations = v["annotations"].as_array().map_or(0, Vec::len);
    }
    info
}

/// Recording names in `dir` (newest first).
fn names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let Ok(file) = entry.file_name().into_string() else {
            continue;
        };
        if let Some(name) = file.strip_suffix(".sigmf-meta")
            && valid_name(name).is_ok()
            && entry.file_type()?.is_file()
        {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

fn recordings_dir(s: &AppState) -> PathBuf {
    s.project_dir.join(DIR)
}

/// `GET /api/recordings`: every recording in the project's `recordings/`.
pub(crate) async fn list(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<RecordingInfoJson>>, ApiError> {
    let dir = recordings_dir(&s);
    let session = s.session.clone();
    let mut all = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        Ok(names(&dir)?
            .iter()
            .map(|n| describe(&dir, n, session.recorders()))
            .collect::<Vec<_>>())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    all.sort_by_key(|r| std::cmp::Reverse(r.modified));
    Ok(Json(all))
}

/// `GET /api/recordings/{name}`: the full metadata document.
pub(crate) async fn get(
    State(s): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = valid_name(&name).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    let (meta, _) = sigmf::file_pair(&recordings_dir(&s).join(name));
    let text = tokio::fs::read_to_string(&meta)
        .await
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{name}: {e}")))?;
    serde_json::from_str(&text)
        .map(Json)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{name}: {e}")))
}

/// `DELETE /api/recordings/{name}`: remove both files (not while recording).
pub(crate) async fn delete(
    State(s): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    let name = valid_name(&name).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    if s.session.recorders().is_active(name) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("`{name}` is still being recorded; stop it first"),
        ));
    }
    let (meta, data) = sigmf::file_pair(&recordings_dir(&s).join(name));
    if !meta.exists() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no recording `{name}`"),
        ));
    }
    for p in [meta, data] {
        match tokio::fs::remove_file(&p).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(internal(format!("{}: {e}", p.display()))),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain() {
        assert_eq!(
            valid_name("fm-rx_out-20261122T134500Z"),
            Ok("fm-rx_out-20261122T134500Z")
        );
        assert_eq!(valid_name("cap.v2.sigmf-meta"), Ok("cap.v2"));
        assert_eq!(valid_name("cap.sigmf-data"), Ok("cap"));
        for bad in [
            "",
            "../x",
            "..",
            "a/b",
            "a\\b",
            "/etc/passwd",
            ".hidden",
            "x..y",
            "-flag",
            "a b",
            "C:x",
            ".sigmf-meta",
            "%2e%2e",
        ] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn generated_names() {
        let e = EdgeRef {
            from: "fm/rx.out".into(),
            to: "sink.in".into(),
        };
        let n = auto_name(Some("FM live.fox.toml"), &e);
        assert!(n.starts_with("FM_live-fm_rx_out-2"), "{n}");
        assert!(
            n.ends_with('Z') && n.len() == "FM_live-fm_rx_out-".len() + 16,
            "{n}"
        );
        assert!(valid_name(&n).is_ok());
        assert!(auto_name(None, &e).starts_with("untitled-"));
    }
}
