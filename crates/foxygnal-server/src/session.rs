//! The engine session: the graph being edited, the running flowgraph, and the
//! event stream every connected tab subscribes to.
//!
//! One session per server. Several tabs share it: edits are broadcast and the
//! last write wins.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use foxygnal_core::graph::BlockSpec;
use foxygnal_core::{
    Error, Flowgraph, GraphSpec, Monitor, ParamKind, ParamSender, Registry, StopHandle,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::probe::{self, Hub, ProbeConfig, Stats};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Stopped,
    Running,
    Error,
}

/// An error the UI pins to a block, an edge, or shows globally.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<EdgeRef>,
    pub message: String,
}

/// A wire, by its `block.port` endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRef {
    pub from: String,
    pub to: String,
}

impl From<&Error> for ErrorInfo {
    fn from(e: &Error) -> Self {
        match e {
            Error::Edge { from, to, msg } => Self {
                block: None,
                edge: Some(EdgeRef {
                    from: from.clone(),
                    to: to.clone(),
                }),
                message: msg.clone(),
            },
            e => Self {
                block: e.block().map(str::to_owned),
                edge: None,
                message: if e.block().is_some() {
                    e.detail()
                } else {
                    e.to_string()
                },
            },
        }
    }
}

/// Run status as the UI sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub state: RunState,
    /// Unix time in ms when the current run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// The graph changed in ways that only apply after a restart.
    pub stale: bool,
    pub errors: Vec<ErrorInfo>,
}

/// Server → client messages (JSON text frames).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Event {
    /// The full session state; sent on connect and after lag.
    Hello {
        client: u64,
        graph: GraphSpec,
        name: Option<String>,
        version: u64,
        status: Status,
    },
    Graph {
        graph: GraphSpec,
        name: Option<String>,
        version: u64,
        /// The client that made the change.
        from: u64,
    },
    Status(Status),
    /// Per-block CPU and per-wire throughput of the running graph.
    Stats(Stats),
}

/// Client → server messages.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    SetGraph {
        graph: GraphSpec,
        /// The graph's file name; `None` for an unsaved graph.
        #[serde(default)]
        name: Option<String>,
    },
    Run,
    Stop,
    /// Watch a wire. `id` is chosen by the client; setting an existing id
    /// changes that probe. Frames arrive as binary messages.
    ProbeSet {
        id: u32,
        edge: EdgeRef,
        probe: ProbeConfig,
        /// Frames per second (1…60).
        #[serde(default = "default_fps")]
        fps: f32,
    },
    ProbeRemove {
        id: u32,
    },
}

fn default_fps() -> f32 {
    30.0
}

struct Run {
    id: u64,
    stop: StopHandle,
    params: ParamSender,
    monitor: Monitor,
    /// The graph as the engine currently has it (live edits applied).
    applied: GraphSpec,
}

struct Inner {
    graph: GraphSpec,
    name: Option<String>,
    version: u64,
    state: RunState,
    started_at: Option<u64>,
    stale: bool,
    errors: Vec<ErrorInfo>,
    run: Option<Run>,
    next_run: u64,
}

pub struct Session {
    registry: Arc<Registry>,
    /// Relative `path` parameters resolve against this directory.
    base_dir: PathBuf,
    inner: Mutex<Inner>,
    events: broadcast::Sender<Arc<str>>,
    hub: Hub,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Session {
    pub fn new(registry: Arc<Registry>, base_dir: PathBuf) -> Arc<Self> {
        let session = Arc::new(Self {
            registry,
            base_dir,
            inner: Mutex::new(Inner {
                graph: GraphSpec::default(),
                name: None,
                version: 0,
                state: RunState::Stopped,
                started_at: None,
                stale: false,
                errors: Vec::new(),
                run: None,
                next_run: 0,
            }),
            events: broadcast::channel(256).0,
            hub: Hub::default(),
        });
        probe::spawn(Arc::downgrade(&session));
        session
    }

    /// Probes of every connected tab.
    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// The current run's id and monitor, if running.
    pub fn monitor(&self) -> Option<(u64, Monitor)> {
        self.lock().run.as_ref().map(|r| (r.id, r.monitor.clone()))
    }

    pub(crate) fn emit_stats(&self, stats: Stats) {
        self.emit(&Event::Stats(stats));
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves plain data behind; keep going.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, ev: &Event) {
        if let Ok(json) = serde_json::to_string(ev) {
            // No subscribers is fine.
            let _ = self.events.send(json.into());
        }
    }

    fn status(inner: &Inner) -> Status {
        Status {
            state: inner.state,
            started_at: inner.started_at,
            stale: inner.stale,
            errors: inner.errors.clone(),
        }
    }

    /// Subscribe to events and get a consistent snapshot to start from.
    pub fn subscribe(&self, client: u64) -> (broadcast::Receiver<Arc<str>>, Event) {
        let inner = self.lock();
        let rx = self.events.subscribe();
        (rx, Self::hello(&inner, client))
    }

    pub fn snapshot(&self, client: u64) -> Event {
        Self::hello(&self.lock(), client)
    }

    fn hello(inner: &Inner, client: u64) -> Event {
        Event::Hello {
            client,
            graph: inner.graph.clone(),
            name: inner.name.clone(),
            version: inner.version,
            status: Self::status(inner),
        }
    }

    pub fn graph(&self) -> GraphSpec {
        self.lock().graph.clone()
    }

    pub fn name(&self) -> Option<String> {
        self.lock().name.clone()
    }

    pub fn handle(self: &Arc<Self>, client: u64, cmd: Command) {
        match cmd {
            Command::SetGraph { graph, name } => self.set_graph(client, graph, name),
            Command::Run => self.run(),
            Command::Stop => self.stop(),
            Command::ProbeSet {
                id,
                edge,
                probe,
                fps,
            } => self.hub.set(client, id, edge, probe, fps),
            Command::ProbeRemove { id } => self.hub.remove(client, id),
        }
    }

    /// Replace the graph. While running, changed live parameters apply
    /// immediately; any other change marks the run as stale.
    pub fn set_graph(&self, client: u64, graph: GraphSpec, name: Option<String>) {
        let mut inner = self.lock();
        let stale_before = inner.stale;
        let all_live = match &mut inner.run {
            Some(run) => apply_live(&self.registry, run, &graph),
            None => true,
        };
        if !all_live {
            inner.stale = true;
        }
        inner.graph = graph;
        inner.name = name;
        inner.version += 1;
        // Errors from the last build refer to a graph that no longer exists.
        if inner.state != RunState::Running {
            inner.errors.clear();
            if inner.state == RunState::Error {
                inner.state = RunState::Stopped;
            }
        }
        self.emit(&Event::Graph {
            graph: inner.graph.clone(),
            name: inner.name.clone(),
            version: inner.version,
            from: client,
        });
        if inner.stale != stale_before || inner.run.is_none() {
            self.emit(&Event::Status(Self::status(&inner)));
        }
    }

    pub fn run(self: &Arc<Self>) {
        let mut inner = self.lock();
        if inner.run.is_some() {
            return;
        }
        inner.errors.clear();
        inner.stale = false;
        let graph = inner.graph.clone();
        let started = Flowgraph::from_spec(&self.resolve_paths(&graph), &self.registry)
            .and_then(|fg| fg.start());
        match started {
            Err(e) => {
                inner.state = RunState::Error;
                inner.started_at = None;
                inner.errors = vec![ErrorInfo::from(&e)];
            }
            Ok(running) => {
                let id = inner.next_run;
                inner.next_run += 1;
                inner.run = Some(Run {
                    id,
                    stop: running.stop_handle(),
                    params: running.param_sender(),
                    monitor: running.monitor(),
                    applied: graph,
                });
                inner.state = RunState::Running;
                inner.started_at = Some(now_ms());
                let session = Arc::clone(self);
                std::thread::Builder::new()
                    .name("fox:session".into())
                    .spawn(move || {
                        let result = running.wait();
                        session.finished(id, result);
                    })
                    .expect("spawning the session thread");
            }
        }
        self.emit(&Event::Status(Self::status(&inner)));
    }

    /// The graph with relative `path` parameters made absolute under `base_dir`,
    /// so a graph saved in the project folder finds its files there.
    fn resolve_paths(&self, graph: &GraphSpec) -> GraphSpec {
        let mut g = graph.clone();
        for b in &mut g.blocks {
            let Some(desc) = self.registry.get(&b.kind) else {
                continue;
            };
            for spec in desc
                .params
                .iter()
                .filter(|p| matches!(p.kind, ParamKind::Path))
            {
                if let Some(serde_json::Value::String(p)) = b.params.get(spec.name) {
                    if !p.is_empty() && std::path::Path::new(p).is_relative() {
                        let abs = self.base_dir.join(p).to_string_lossy().into_owned();
                        b.params.insert(spec.name.to_owned(), abs.into());
                    }
                }
            }
        }
        g
    }

    fn finished(&self, id: u64, result: Result<(), Error>) {
        let mut inner = self.lock();
        if inner.run.as_ref().is_none_or(|r| r.id != id) {
            return;
        }
        inner.run = None;
        inner.stale = false;
        inner.started_at = None;
        match result {
            Ok(()) => inner.state = RunState::Stopped,
            Err(e) => {
                inner.state = RunState::Error;
                inner.errors = vec![ErrorInfo::from(&e)];
            }
        }
        self.emit(&Event::Status(Self::status(&inner)));
    }

    pub fn stop(&self) {
        if let Some(run) = &self.lock().run {
            run.stop.stop();
        }
    }

    /// Stop and wait for the run to end (used on shutdown and in tests).
    pub async fn stop_and_wait(&self) {
        self.stop();
        while self.lock().run.is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Block ids with kinds, and connections: what a running graph can't change.
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

/// Push changed live params of `new` into the running graph. Returns false if
/// anything else changed (a restart is needed for it to take effect).
fn apply_live(registry: &Registry, run: &mut Run, new: &GraphSpec) -> bool {
    if topology(&run.applied) != topology(new) {
        return false;
    }
    let mut all_applied = true;
    for nb in &new.blocks {
        let Some(ob) = run.applied.blocks.iter_mut().find(|b| b.id == nb.id) else {
            return false;
        };
        let Some(desc) = registry.get(&nb.kind) else {
            return false;
        };
        for spec in &desc.params {
            let old = param_or_default(ob, spec.name, &spec.default);
            let new = param_or_default(nb, spec.name, &spec.default);
            if old == new {
                continue;
            }
            let ok = spec.live
                && spec.check(&new).is_ok()
                && run.params.send(&nb.id, spec.name, new.clone()).is_ok();
            if ok {
                ob.params.insert(spec.name.to_owned(), new);
            } else {
                all_applied = false;
            }
        }
    }
    all_applied
}

fn param_or_default(b: &BlockSpec, name: &str, default: &serde_json::Value) -> serde_json::Value {
    b.params.get(name).unwrap_or(default).clone()
}
