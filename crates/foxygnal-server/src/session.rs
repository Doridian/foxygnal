//! The engine session: the graph being edited, the running flowgraph, and the
//! event stream every connected tab subscribes to.
//!
//! One session per server. Several tabs share it: edits are broadcast and the
//! last write wins.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use foxygnal_core::resolve::{Resolved, diff_params};
use foxygnal_core::{
    EdgeMeta, Error, Flowgraph, GraphSpec, Monitor, ParamKind, ParamSender, Registry, StopHandle,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::graphs::{EdgeMap, ProjectGraphs};
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
        meta: Meta,
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
    /// Stream metadata of every wire, worked out after each graph change.
    Meta(Meta),
}

/// Sample rate and centre frequency on every wire. Computed by building and
/// configuring the graph without running it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Meta {
    pub edges: Vec<EdgeMeta>,
    /// Why metadata couldn't be worked out (the graph doesn't build yet, or a
    /// block needs a rate nothing provides). `None` when all is well.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
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
    /// The resolved (flattened) graph as the engine currently has it, live
    /// edits applied.
    applied: GraphSpec,
    /// From the running graph's wires and blocks to the graph as drawn.
    map: Arc<EdgeMap>,
}

/// A graph resolved, built and configured, ready to start.
struct Built {
    fg: Flowgraph,
    /// The resolved graph.
    graph: GraphSpec,
    /// Edge metadata as drawn.
    edges: Vec<EdgeMeta>,
    map: EdgeMap,
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
    meta: Meta,
}

pub struct Session {
    registry: Arc<Registry>,
    /// Relative `path` parameters resolve against this directory.
    base_dir: PathBuf,
    /// Where `graph:NAME.fox.toml` blocks come from (the same directory).
    graphs: ProjectGraphs,
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
            graphs: ProjectGraphs {
                dir: base_dir.clone(),
            },
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
                meta: Meta::default(),
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

    /// The current run's id and monitor, and how its wires map to the graph
    /// as drawn, if running.
    pub fn monitor(&self) -> Option<(u64, Monitor, Arc<EdgeMap>)> {
        self.lock()
            .run
            .as_ref()
            .map(|r| (r.id, r.monitor.clone(), r.map.clone()))
    }

    pub(crate) fn emit_stats(&self, stats: Stats) {
        self.emit(&Event::Stats(stats));
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Graph files in the project directory (and the bundled library).
    pub fn graphs(&self) -> &ProjectGraphs {
        &self.graphs
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
            meta: inner.meta.clone(),
        }
    }

    /// Resolve variables and sub-graphs, build and configure `graph`.
    /// Errors are pinned to the graph as drawn.
    fn build(&self, graph: &GraphSpec) -> Result<Built, ErrorInfo> {
        let mut r: Resolved = foxygnal_core::resolve(graph, &self.registry, &self.graphs)
            .map_err(|e| EdgeMap::default().error(&e))?;
        r.graph = self.resolve_paths(&r.graph);
        let mut map = EdgeMap::new(&r);
        let mut fg =
            Flowgraph::from_resolved(&r.graph, &self.registry).map_err(|e| map.error(&e))?;
        let edges = fg.configure().map_err(|e| map.error(&e))?;
        map.add_meta(&r, &edges);
        Ok(Built {
            fg,
            edges: map.meta(edges),
            graph: r.graph,
            map,
        })
    }

    /// Stream metadata of a build.
    fn meta_of(built: &Result<Built, ErrorInfo>) -> Meta {
        match built {
            Ok(b) => Meta {
                edges: b.edges.clone(),
                error: None,
            },
            Err(e) => Meta {
                edges: Vec::new(),
                error: Some(e.clone()),
            },
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
        let built = self.build(&graph);
        let mut inner = self.lock();
        let stale_before = inner.stale;
        let all_live = match (&mut inner.run, &built) {
            (Some(run), Ok(b)) => apply_live(&self.registry, run, &b.graph),
            (Some(_), Err(_)) => false,
            (None, _) => true,
        };
        if !all_live {
            inner.stale = true;
        }
        let meta = Self::meta_of(&built);
        let meta_changed = meta.edges != inner.meta.edges
            || meta.error.as_ref().map(|e| &e.message)
                != inner.meta.error.as_ref().map(|e| &e.message);
        inner.meta = meta;
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
        if meta_changed {
            self.emit(&Event::Meta(inner.meta.clone()));
        }
    }

    pub fn run(self: &Arc<Self>) {
        let mut inner = self.lock();
        if inner.run.is_some() {
            return;
        }
        inner.errors.clear();
        inner.stale = false;
        let started = self.build(&inner.graph).and_then(|b| {
            let map = Arc::new(b.map);
            match b.fg.start() {
                Ok(running) => Ok((running, b.graph, map)),
                Err(e) => Err(map.error(&e)),
            }
        });
        match started {
            Err(e) => {
                inner.state = RunState::Error;
                inner.started_at = None;
                inner.errors = vec![e];
            }
            Ok((running, graph, map)) => {
                let id = inner.next_run;
                inner.next_run += 1;
                inner.run = Some(Run {
                    id,
                    stop: running.stop_handle(),
                    params: running.param_sender(),
                    monitor: running.monitor(),
                    applied: graph,
                    map,
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
        let Some(run) = inner.run.take() else { return };
        inner.stale = false;
        inner.started_at = None;
        match result {
            Ok(()) => inner.state = RunState::Stopped,
            Err(e) => {
                inner.state = RunState::Error;
                inner.errors = vec![run.map.error(&e)];
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

/// Push changed live params of `new` (resolved) into the running graph.
/// Returns false if anything else changed (a restart is needed for it to take
/// effect). Comparing resolved graphs means a changed variable reaches every
/// param that uses it.
fn apply_live(registry: &Registry, run: &mut Run, new: &GraphSpec) -> bool {
    let Some(changes) = diff_params(registry, &run.applied, new) else {
        return false;
    };
    let mut all_applied = true;
    for c in changes {
        if c.live && run.params.send(&c.block, c.param, c.value.clone()).is_ok() {
            if let Some(b) = run.applied.blocks.iter_mut().find(|b| b.id == c.block) {
                b.params.insert(c.param.to_owned(), c.value);
            }
        } else {
            all_applied = false;
        }
    }
    all_applied
}
