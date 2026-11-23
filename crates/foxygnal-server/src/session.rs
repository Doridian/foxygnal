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
use crate::record::{self, RecordStatus, Recorders};

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
    /// Problems that don't stop the run, pinned to blocks: a value on a wired
    /// setting (`block.@param`) that the block rejected. One per block and
    /// setting; cleared when a run starts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ErrorInfo>,
    /// The current run transmits: TX was armed for it. The UI shows a red
    /// indicator while this is set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tx_armed: bool,
}

/// A block in the graph that transmits, with the settings it would use, for
/// the arm-TX confirmation. Values are resolved (variables evaluated).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TxInfo {
    /// The block as resolved (`fm/tx` inside a hierarchical block `fm`).
    pub block: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency: Option<f64>,
    /// The rate it would run at (from upstream if the param is 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<f64>,
}

/// Server → client messages (JSON text frames).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
// Short-lived (serialized right away), so the size of `Hello` doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// The full session state; sent on connect and after lag.
    Hello {
        client: u64,
        graph: GraphSpec,
        name: Option<String>,
        version: u64,
        status: Status,
        meta: Meta,
        /// The server was started with `--allow-tx`.
        allow_tx: bool,
        /// Recordings in progress.
        recordings: Vec<RecordStatus>,
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
    /// A recording's progress, about twice a second, and once more when it
    /// ends (`done`, with `error` if it failed or couldn't start).
    Record(RecordStatus),
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
    /// Blocks that would transmit if the graph ran.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tx: Vec<TxInfo>,
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
    Run {
        /// Arm TX for this run only. Needed (with `--allow-tx`) to start a
        /// graph with blocks that transmit; ignored for graphs without.
        #[serde(default)]
        arm_tx: bool,
    },
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
    /// Record a wire of the running graph to SigMF in the project's
    /// `recordings/` (named by the server unless `name` is given).
    RecordStart {
        edge: EdgeRef,
        #[serde(default)]
        name: Option<String>,
    },
    RecordStop {
        edge: EdgeRef,
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
    /// Held while the run lives if it transmits; dropping it disarms TX.
    tx: Option<foxygnal_sdr::TxArm>,
}

/// A graph resolved, built and configured, ready to start.
struct Built {
    fg: Flowgraph,
    /// The resolved graph.
    graph: GraphSpec,
    /// Edge metadata as drawn.
    edges: Vec<EdgeMeta>,
    map: EdgeMap,
    tx: Vec<TxInfo>,
}

struct Inner {
    graph: GraphSpec,
    name: Option<String>,
    version: u64,
    state: RunState,
    started_at: Option<u64>,
    stale: bool,
    errors: Vec<ErrorInfo>,
    warnings: Vec<ErrorInfo>,
    /// [`Monitor::warning_count`] when `warnings` was last updated.
    warning_count: u64,
    run: Option<Run>,
    next_run: u64,
    meta: Meta,
}

pub struct Session {
    registry: Arc<Registry>,
    /// Graphs with TX blocks may run (when armed); `--allow-tx`.
    allow_tx: bool,
    /// Relative `path` parameters resolve against this directory.
    base_dir: PathBuf,
    /// Where `graph:NAME.fox.toml` blocks come from (the same directory).
    graphs: ProjectGraphs,
    inner: Mutex<Inner>,
    events: broadcast::Sender<Arc<str>>,
    hub: Hub,
    recorders: Recorders,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Session {
    pub fn new(registry: Arc<Registry>, base_dir: PathBuf) -> Arc<Self> {
        Self::with_tx(registry, base_dir, false)
    }

    /// A session where graphs that transmit may run, each run only after
    /// the client arms TX for it (`foxygnal serve --allow-tx`).
    pub fn with_tx(registry: Arc<Registry>, base_dir: PathBuf, allow_tx: bool) -> Arc<Self> {
        let session = Arc::new(Self {
            registry,
            allow_tx,
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
                warnings: Vec::new(),
                warning_count: 0,
                run: None,
                next_run: 0,
                meta: Meta::default(),
            }),
            events: broadcast::channel(256).0,
            hub: Hub::default(),
            recorders: Recorders::default(),
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

    /// Pick up new warnings of run `id` (see [`Status::warnings`]) and send
    /// a status if they changed. Cheap when nothing changed.
    pub(crate) fn poll_warnings(&self, id: u64, monitor: &Monitor, map: &EdgeMap) {
        let count = monitor.warning_count();
        let mut inner = self.lock();
        if inner.run.as_ref().is_none_or(|r| r.id != id) || count == inner.warning_count {
            return;
        }
        inner.warning_count = count;
        inner.warnings = monitor
            .warnings()
            .into_iter()
            .map(|w| {
                map.error(&Error::Block {
                    block: w.block,
                    source: Box::new(Error::Other(format!("{}: {}", w.key, w.message))),
                })
            })
            .collect();
        self.emit(&Event::Status(Self::status(&inner)));
    }

    /// Recordings in progress.
    pub fn recorders(&self) -> &Recorders {
        &self.recorders
    }

    pub(crate) fn emit_record(&self, status: RecordStatus) {
        self.emit(&Event::Record(status));
    }

    /// Start recording a wire of the running graph (see [`crate::record`]).
    /// Progress and errors arrive as `record` events.
    pub fn record_start(self: &Arc<Self>, edge: EdgeRef, name: Option<String>) {
        let src = {
            let inner = self.lock();
            let missing = || {
                format!(
                    "`{} → {}` isn't in the running graph. Restart to record it.",
                    edge.from, edge.to
                )
            };
            inner
                .run
                .as_ref()
                .ok_or_else(|| "Run the graph to record a wire.".to_owned())
                .and_then(|run| {
                    let (block, port) =
                        run.map.source(&edge).rsplit_once('.').ok_or_else(missing)?;
                    let (ty, tap) = run.monitor.tap(block, port).ok_or_else(missing)?;
                    let reader = tap.attach().ok_or_else(|| {
                        "this output is already being recorded as many times as it can be"
                            .to_owned()
                    })?;
                    let meta = inner
                        .meta
                        .edges
                        .iter()
                        .find(|e| e.from == edge.from && e.to == edge.to)
                        .map(|e| e.meta)
                        .unwrap_or_default();
                    Ok(record::Source {
                        reader,
                        ty,
                        capacity: tap.capacity(),
                        sample_rate: meta.sample_rate,
                        frequency: meta.center_freq,
                        hw: record::upstream_device(&run.applied, &self.registry, block),
                        graph: inner.name.clone(),
                    })
                })
        };
        let started =
            src.and_then(|src| record::start(self, &self.base_dir, edge.clone(), name, src));
        if let Err(e) = started {
            self.emit_record(RecordStatus::failed(edge, e));
        }
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
            warnings: inner.warnings.clone(),
            tx_armed: inner.run.as_ref().is_some_and(|r| r.tx.is_some()),
        }
    }

    /// Subscribe to events and get a consistent snapshot to start from.
    pub fn subscribe(&self, client: u64) -> (broadcast::Receiver<Arc<str>>, Event) {
        let inner = self.lock();
        let rx = self.events.subscribe();
        (rx, self.hello(&inner, client))
    }

    pub fn snapshot(&self, client: u64) -> Event {
        self.hello(&self.lock(), client)
    }

    fn hello(&self, inner: &Inner, client: u64) -> Event {
        Event::Hello {
            client,
            graph: inner.graph.clone(),
            name: inner.name.clone(),
            version: inner.version,
            status: Self::status(inner),
            meta: inner.meta.clone(),
            allow_tx: self.allow_tx,
            recordings: self.recorders.statuses(),
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
        let tx = self.tx_blocks(&r.graph, &edges);
        Ok(Built {
            fg,
            edges: map.meta(edges),
            graph: r.graph,
            map,
            tx,
        })
    }

    /// The blocks of a resolved graph that transmit, with their settings.
    fn tx_blocks(&self, graph: &GraphSpec, edges: &[EdgeMeta]) -> Vec<TxInfo> {
        graph
            .blocks
            .iter()
            .filter(|b| self.registry.is_tx(&b.kind))
            .map(|b| {
                let desc = self.registry.get(&b.kind);
                let param = |name: &str| {
                    b.params.get(name).cloned().or_else(|| {
                        desc?
                            .params
                            .iter()
                            .find(|p| p.name == name)
                            .map(|p| p.default.clone())
                    })
                };
                let num = |name: &str| param(name).and_then(|v| v.as_f64());
                let prefix = format!("{}.", b.id);
                let upstream = edges
                    .iter()
                    .find(|e| e.to.starts_with(&prefix))
                    .and_then(|e| e.meta.sample_rate);
                TxInfo {
                    block: b.id.clone(),
                    kind: b.kind.clone(),
                    device: param("device").and_then(|v| v.as_str().map(str::to_owned)),
                    frequency: num("frequency"),
                    sample_rate: num("sample_rate").filter(|r| *r > 0.0).or(upstream),
                    gain: num("gain"),
                }
            })
            .collect()
    }

    /// Stream metadata of a build.
    fn meta_of(built: &Result<Built, ErrorInfo>) -> Meta {
        match built {
            Ok(b) => Meta {
                edges: b.edges.clone(),
                error: None,
                tx: b.tx.clone(),
            },
            Err(e) => Meta {
                edges: Vec::new(),
                error: Some(e.clone()),
                tx: Vec::new(),
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
            Command::Run { arm_tx } => self.run_with(arm_tx),
            Command::Stop => self.stop(),
            Command::ProbeSet {
                id,
                edge,
                probe,
                fps,
            } => self.hub.set(client, id, edge, probe, fps),
            Command::ProbeRemove { id } => self.hub.remove(client, id),
            Command::RecordStart { edge, name } => self.record_start(edge, name),
            Command::RecordStop { edge } => self.recorders.stop(&edge),
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
            || meta.tx != inner.meta.tx
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

    /// Run without arming TX (graphs that transmit are refused).
    pub fn run(self: &Arc<Self>) {
        self.run_with(false);
    }

    /// Run the graph. A graph with blocks that transmit only starts if the
    /// server allows TX (`--allow-tx`) and `arm_tx` is set; TX stays armed
    /// until this run ends.
    pub fn run_with(self: &Arc<Self>, arm_tx: bool) {
        let mut inner = self.lock();
        if inner.run.is_some() {
            return;
        }
        inner.errors.clear();
        inner.warnings.clear();
        inner.warning_count = 0;
        inner.stale = false;
        let started = self.build(&inner.graph).and_then(|b| {
            let map = Arc::new(b.map);
            // Arm before starting: TX blocks check the switch in `start()`.
            let tx = self.check_tx(&b.tx, arm_tx).map_err(|e| map.error(&e))?;
            match b.fg.start() {
                Ok(running) => Ok((running, b.graph, map, tx)),
                Err(e) => Err(map.error(&e)),
            }
        });
        match started {
            Err(e) => {
                inner.state = RunState::Error;
                inner.started_at = None;
                inner.errors = vec![e];
            }
            Ok((running, graph, map, tx)) => {
                if tx.is_some() {
                    tracing::warn!("TX armed for this run");
                }
                let id = inner.next_run;
                inner.next_run += 1;
                inner.run = Some(Run {
                    id,
                    stop: running.stop_handle(),
                    params: running.param_sender(),
                    monitor: running.monitor(),
                    applied: graph,
                    map,
                    tx,
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

    /// The safety gate for transmitting: `Ok(Some(guard))` arms TX for a
    /// graph that transmits, `Ok(None)` for one that doesn't, and an error
    /// pinned to the first TX block otherwise.
    fn check_tx(&self, tx: &[TxInfo], arm: bool) -> Result<Option<foxygnal_sdr::TxArm>, Error> {
        let Some(first) = tx.first() else {
            return Ok(None);
        };
        let refuse = |msg: &str| Error::Block {
            block: first.block.clone(),
            source: Box::new(Error::Other(msg.to_owned())),
        };
        if !self.allow_tx {
            return Err(refuse(
                "this block transmits, and the engine was started without --allow-tx. \
                 To allow transmitting, restart it with `foxygnal serve --allow-tx`.",
            ));
        }
        if !arm {
            return Err(refuse(
                "this block transmits: arm TX for this run to start it (press Run and \
                 confirm \"Arm TX and run\").",
            ));
        }
        Ok(Some(foxygnal_sdr::arm_tx()))
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
                if let Some(serde_json::Value::String(p)) = b.params.get(spec.name)
                    && !p.is_empty()
                    && std::path::Path::new(p).is_relative()
                {
                    let abs = self.base_dir.join(p).to_string_lossy().into_owned();
                    b.params.insert(spec.name.to_owned(), abs.into());
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

    /// Stop and wait for the run to end, and recordings to be finished
    /// (used on shutdown and in tests).
    pub async fn stop_and_wait(&self) {
        self.stop();
        while self.lock().run.is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.recorders.stop_all();
        for _ in 0..500 {
            if self.recorders.is_empty() {
                break;
            }
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
