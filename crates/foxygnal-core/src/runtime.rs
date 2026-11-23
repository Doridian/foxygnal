//! Builds a runnable flowgraph and executes it.
//!
//! M0 scheduler: one OS thread per block. Blocks park when they cannot make
//! progress and are unparked by their neighbours when data or space appears.
//! See docs/ARCHITECTURE.md for the planned pooled scheduler.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::block::{InputPort, OutputPort, WorkIo, WorkStatus};
use crate::buffer::{self, Reader, Tap};
use crate::graph::GraphSpec;
use crate::meta::{Configure, StreamMeta};
use crate::registry::{Instance, Registry};
use crate::resolve::{GraphLoader, NoLoader, resolve};
use crate::tag::TagPropagation;
use crate::{Error, StreamType};

type ParamMsg = (String, serde_json::Value);

/// Handle to a block inside a [`Flowgraph`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(usize);

struct Node {
    name: String,
    inst: Instance,
}

#[derive(Clone, Copy)]
struct Edge {
    from: (usize, usize),
    to: (usize, usize),
}

/// A graph of blocks under construction.
#[derive(Default)]
pub struct Flowgraph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    buffer_capacity: Option<usize>,
    /// Output metadata per node and port, once configured.
    meta: Option<Vec<Vec<StreamMeta>>>,
}

/// The metadata of one connection, see [`Flowgraph::configure`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EdgeMeta {
    /// `block.port` of the output.
    pub from: String,
    /// `block.port` of the input.
    pub to: String,
    #[serde(flatten)]
    pub meta: StreamMeta,
}

impl Flowgraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the per-connection buffer capacity (items).
    pub fn with_buffer_capacity(mut self, items: usize) -> Self {
        self.buffer_capacity = Some(items);
        self
    }

    pub fn add(&mut self, name: impl Into<String>, inst: Instance) -> BlockId {
        self.nodes.push(Node {
            name: name.into(),
            inst,
        });
        BlockId(self.nodes.len() - 1)
    }

    /// Connect output port `out` of `from` to input port `inp` of `to`.
    pub fn connect(
        &mut self,
        from: BlockId,
        out: usize,
        to: BlockId,
        inp: usize,
    ) -> Result<(), Error> {
        let src = &self.nodes[from.0];
        let dst = &self.nodes[to.0];
        let (oname, oty) = src
            .inst
            .outputs
            .get(out)
            .ok_or_else(|| Error::Graph(format!("`{}` has no output port #{out}", src.name)))?;
        let (iname, ity) = dst
            .inst
            .inputs
            .get(inp)
            .ok_or_else(|| Error::Graph(format!("`{}` has no input port #{inp}", dst.name)))?;
        let edge_err = |msg: String| Error::Edge {
            from: format!("{}.{oname}", src.name),
            to: format!("{}.{iname}", dst.name),
            msg,
        };
        if oty != ity {
            return Err(edge_err(format!(
                "type mismatch: `{}.{oname}` produces {oty} but `{}.{iname}` expects {ity}",
                src.name, dst.name
            )));
        }
        if self.edges.iter().any(|e| e.to == (to.0, inp)) {
            return Err(edge_err(format!(
                "`{}.{iname}` is already connected; an input takes exactly one connection",
                dst.name
            )));
        }
        self.edges.push(Edge {
            from: (from.0, out),
            to: (to.0, inp),
        });
        Ok(())
    }

    /// Build a flowgraph from a serialized spec using the given registry.
    /// Variables and expressions are evaluated; hierarchical blocks are an
    /// error (use [`Self::from_spec_with`]).
    pub fn from_spec(spec: &GraphSpec, registry: &Registry) -> Result<Self, Error> {
        Self::from_spec_with(spec, registry, &NoLoader)
    }

    /// [`Self::from_spec`], loading sub-graphs of hierarchical blocks with
    /// `loader`.
    pub fn from_spec_with(
        spec: &GraphSpec,
        registry: &Registry,
        loader: &dyn GraphLoader,
    ) -> Result<Self, Error> {
        Self::from_resolved(&resolve(spec, registry, loader)?.graph, registry)
    }

    /// Build from a graph already passed through [`crate::resolve()`].
    pub fn from_resolved(spec: &GraphSpec, registry: &Registry) -> Result<Self, Error> {
        let mut fg = Self::new();
        let mut ids = HashMap::new();
        for b in &spec.blocks {
            if ids.contains_key(b.id.as_str()) {
                return Err(Error::Graph(format!("duplicate block id `{}`", b.id)));
            }
            let inst = registry.build(&b.kind, &b.params).map_err(|e| match e {
                // The registry only knows the kind; name the block instead.
                Error::Param { param, msg, .. } => Error::Param {
                    block: b.id.clone(),
                    param,
                    msg,
                },
                e => Error::Block {
                    block: b.id.clone(),
                    source: Box::new(e),
                },
            })?;
            ids.insert(b.id.as_str(), fg.add(&b.id, inst));
        }
        for c in &spec.connections {
            let edge_err = |msg: String| Error::Edge {
                from: c.from.clone(),
                to: c.to.clone(),
                msg,
            };
            let (f, t) = c.endpoints().map_err(|e| edge_err(e.to_string()))?;
            let lookup = |name: &str| {
                ids.get(name)
                    .copied()
                    .ok_or_else(|| edge_err(format!("unknown block `{name}`")))
            };
            let (fid, tid) = (lookup(f.block)?, lookup(t.block)?);
            let out =
                resolve_port(&fg.nodes[fid.0].inst.outputs, f.port, f.block).map_err(edge_err)?;
            let inp =
                resolve_port(&fg.nodes[tid.0].inst.inputs, t.port, t.block).map_err(edge_err)?;
            fg.connect(fid, out, tid, inp)?;
        }
        Ok(fg)
    }

    /// Work out every stream's metadata: call [`crate::Block::configure`] on
    /// each block, upstream first. Unconnected inputs count as unknown.
    /// Idempotent; [`Self::start`] calls it if needed.
    pub fn configure(&mut self) -> Result<Vec<EdgeMeta>, Error> {
        if self.meta.is_none() {
            let order = self.topo_order()?;
            let mut out: Vec<Vec<StreamMeta>> = self
                .nodes
                .iter()
                .map(|n| vec![StreamMeta::default(); n.inst.outputs.len()])
                .collect();
            for ni in order {
                let inputs: Vec<StreamMeta> = (0..self.nodes[ni].inst.inputs.len())
                    .map(|pi| {
                        self.edges
                            .iter()
                            .find(|e| e.to == (ni, pi))
                            .map(|e| out[e.from.0][e.from.1])
                            .unwrap_or_default()
                    })
                    .collect();
                let node = &mut self.nodes[ni];
                let mut outputs = vec![StreamMeta::merge(&inputs); node.inst.outputs.len()];
                node.inst
                    .block
                    .configure(&mut Configure {
                        inputs: &inputs,
                        outputs: &mut outputs,
                    })
                    .map_err(|e| match e {
                        e @ (Error::Block { .. } | Error::Param { .. }) => e,
                        e => Error::Block {
                            block: node.name.clone(),
                            source: Box::new(e),
                        },
                    })?;
                out[ni] = outputs;
            }
            self.meta = Some(out);
        }
        let meta = self.meta.as_ref().expect("configured above");
        Ok(self
            .edges
            .iter()
            .map(|e| {
                let (src, dst) = (&self.nodes[e.from.0], &self.nodes[e.to.0]);
                EdgeMeta {
                    from: format!("{}.{}", src.name, src.inst.outputs[e.from.1].0),
                    to: format!("{}.{}", dst.name, dst.inst.inputs[e.to.1].0),
                    meta: meta[e.from.0][e.from.1],
                }
            })
            .collect())
    }

    /// Nodes ordered so every block comes after the blocks feeding it.
    fn topo_order(&self) -> Result<Vec<usize>, Error> {
        let n = self.nodes.len();
        let mut indegree = vec![0usize; n];
        for e in &self.edges {
            indegree[e.to.0] += 1;
        }
        let mut ready: Vec<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(i) = ready.pop() {
            order.push(i);
            for e in self.edges.iter().filter(|e| e.from.0 == i) {
                indegree[e.to.0] -= 1;
                if indegree[e.to.0] == 0 {
                    ready.push(e.to.0);
                }
            }
        }
        if order.len() < n {
            let stuck: Vec<&str> = (0..n)
                .filter(|&i| indegree[i] > 0)
                .map(|i| self.nodes[i].name.as_str())
                .collect();
            return Err(Error::Graph(format!(
                "the graph has a loop through {}; streams must flow one way",
                stuck.join(", ")
            )));
        }
        Ok(order)
    }

    /// Allocate buffers and spawn one thread per block.
    pub fn start(mut self) -> Result<Running, Error> {
        let cap = self.buffer_capacity.unwrap_or(buffer::DEFAULT_CAPACITY);

        // Every input must be connected.
        for (ni, n) in self.nodes.iter().enumerate() {
            for (pi, (pname, _)) in n.inst.inputs.iter().enumerate() {
                if !self.edges.iter().any(|e| e.to == (ni, pi)) {
                    return Err(Error::Block {
                        block: n.name.clone(),
                        source: Box::new(Error::Other(format!("input `{pname}` is not connected"))),
                    });
                }
            }
        }
        self.configure()?;

        let mut inputs: Vec<Vec<Option<Reader>>> = self
            .nodes
            .iter()
            .map(|n| n.inst.inputs.iter().map(|_| None).collect())
            .collect();
        let mut outputs: Vec<Vec<OutputPort>> = Vec::with_capacity(self.nodes.len());
        let mut taps = Vec::new();
        let mut edge_stats = Vec::new();
        for (ni, n) in self.nodes.iter().enumerate() {
            let mut ports = Vec::new();
            for (pi, (pname, ty)) in n.inst.outputs.iter().enumerate() {
                let dests: Vec<_> = self.edges.iter().filter(|e| e.from == (ni, pi)).collect();
                let (writer, readers) = buffer::channel(ty.item_size(), cap, dests.len());
                let tap = writer.tap();
                for (reader, (e, r)) in dests.into_iter().zip(readers).enumerate() {
                    inputs[e.to.0][e.to.1] = Some(r);
                    let dst = &self.nodes[e.to.0];
                    edge_stats.push(EdgeTap {
                        from: format!("{}.{pname}", n.name),
                        to: format!("{}.{}", dst.name, dst.inst.inputs[e.to.1].0),
                        tap: tap.clone(),
                        reader,
                    });
                }
                taps.push(OutputTap {
                    block: n.name.clone(),
                    port: pname.clone(),
                    index: pi,
                    ty: *ty,
                    tap,
                });
                ports.push(OutputPort { ty: *ty, writer });
            }
            outputs.push(ports);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let mut names = Vec::new();
        let mut param_txs = Vec::new();
        let mut busy = Vec::new();
        for ((node, ins), outs) in self.nodes.into_iter().zip(inputs).zip(outputs) {
            let ins: Vec<InputPort> = node
                .inst
                .inputs
                .iter()
                .zip(ins)
                .map(|((_, ty), reader)| InputPort::new(*ty, reader))
                .collect();
            let stop = stop.clone();
            let name = node.name.clone();
            let (ptx, prx) = mpsc::channel::<ParamMsg>();
            let busy_ns = Arc::new(AtomicU64::new(0));
            names.push(name.clone());
            param_txs.push(ptx);
            busy.push(busy_ns.clone());
            let h = std::thread::Builder::new()
                .name(format!("fox:{name}"))
                .spawn(move || {
                    // A panicking block must stop the rest of the graph too.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_block(node.inst, ins, outs, &prx, &stop, &busy_ns)
                    }))
                    .unwrap_or(Err(Error::Panic));
                    if r.is_err() {
                        stop.store(true, Ordering::Relaxed);
                    }
                    r.map_err(|e| Error::Block {
                        block: name,
                        source: Box::new(e),
                    })
                })
                .map_err(Error::Io)?;
            handles.push(h);
        }
        let monitor = Monitor {
            inner: Arc::new(MonitorInner {
                blocks: names.iter().cloned().zip(busy).collect(),
                outputs: taps,
                edges: edge_stats,
            }),
        };
        Ok(Running {
            handles,
            names,
            param_txs,
            stop,
            monitor,
        })
    }
}

fn resolve_port(
    ports: &[(String, crate::StreamType)],
    port: &str,
    block: &str,
) -> Result<usize, String> {
    if let Some(i) = ports.iter().position(|(n, _)| n == port) {
        return Ok(i);
    }
    port.parse::<usize>()
        .ok()
        .filter(|i| *i < ports.len())
        .ok_or_else(|| format!("block `{block}` has no port `{port}`"))
}

const IDLE_PARK: Duration = Duration::from_millis(50);

/// Ports per block whose tags the runtime passes on automatically.
const MAX_TAG_PORTS: usize = 16;

/// Pass tags on consumed input items to every output, scaling offsets by the
/// items produced per item consumed in this `work` call.
fn propagate_tags(
    inputs: &[InputPort],
    outputs: &mut [OutputPort],
    in_before: &[u64; MAX_TAG_PORTS],
    out_before: &[u64; MAX_TAG_PORTS],
) {
    if outputs.is_empty() {
        return;
    }
    for (input, &start) in inputs.iter().zip(in_before) {
        let consumed = input.position() - start;
        if consumed == 0 {
            continue;
        }
        // `tags()` starts at the new read position; consumed tags sit before it.
        for tag in input.consumed_tags(start) {
            let rel = tag.offset - start;
            for (o, &ostart) in outputs.iter_mut().zip(out_before) {
                let produced = o.position() - ostart;
                let mut t = tag.clone();
                t.offset = ostart + rel * produced / consumed;
                o.push_tag(&t);
            }
        }
    }
}

fn run_block(
    mut inst: Instance,
    mut inputs: Vec<InputPort>,
    mut outputs: Vec<OutputPort>,
    params: &mpsc::Receiver<ParamMsg>,
    stop: &AtomicBool,
    busy_ns: &AtomicU64,
) -> Result<(), Error> {
    for i in &inputs {
        if let Some(r) = &i.reader {
            r.register_thread();
        }
    }
    for o in &outputs {
        o.writer.register_thread();
    }
    inst.block.start()?;
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        // Live parameter changes. `try_recv` never blocks or locks.
        let mut failed = None;
        while let Ok((name, value)) = params.try_recv() {
            if let Err(e) = inst.block.set_param(&name, &value) {
                failed = Some(e);
                break;
            }
        }
        if let Some(e) = failed {
            break Err(e);
        }
        for i in &mut inputs {
            i.sync();
        }
        // Positions before `work`, for passing tags on (fixed-size: no allocation).
        let mut in_before = [0u64; MAX_TAG_PORTS];
        let mut out_before = [0u64; MAX_TAG_PORTS];
        for (b, i) in in_before.iter_mut().zip(&inputs) {
            *b = i.position();
        }
        for (b, o) in out_before.iter_mut().zip(&outputs) {
            *b = o.position();
        }
        let mut io = WorkIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let t0 = Instant::now();
        let status = inst.block.work(&mut io);
        busy_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if inst.block.tag_propagation() == TagPropagation::AllToAll {
            propagate_tags(&inputs, &mut outputs, &in_before, &out_before);
        }
        for i in &mut inputs {
            i.prune_tags();
        }
        // Publish after tags, so readers see an item's tags with the item.
        for o in &mut outputs {
            o.flush();
        }
        match status {
            Err(e) => break Err(e),
            Ok(WorkStatus::Ok) => {}
            Ok(WorkStatus::Done) => break Ok(()),
            Ok(WorkStatus::Wait(d)) => std::thread::park_timeout(d.min(IDLE_PARK)),
            Ok(WorkStatus::Blocked) => {
                // Upstream ended and we can't make progress: we're done too.
                if inputs.iter().any(InputPort::exhausted) {
                    break Ok(());
                }
                // Every consumer is gone: nobody wants our output.
                if !outputs.is_empty() && outputs.iter().all(|o| o.writer.abandoned()) {
                    break Ok(());
                }
                std::thread::park_timeout(IDLE_PARK);
            }
        }
    };
    let stopped = inst.block.stop();
    // Dropping ports signals end-of-stream downstream and departure upstream.
    drop(inputs);
    drop(outputs);
    result.and(stopped)
}

/// A started flowgraph.
pub struct Running {
    handles: Vec<JoinHandle<Result<(), Error>>>,
    names: Vec<String>,
    param_txs: Vec<mpsc::Sender<ParamMsg>>,
    stop: Arc<AtomicBool>,
    monitor: Monitor,
}

struct OutputTap {
    block: String,
    port: String,
    index: usize,
    ty: StreamType,
    tap: Tap,
}

struct EdgeTap {
    from: String,
    to: String,
    tap: Tap,
    reader: usize,
}

struct MonitorInner {
    blocks: Vec<(String, Arc<AtomicU64>)>,
    outputs: Vec<OutputTap>,
    edges: Vec<EdgeTap>,
}

/// Cloneable read-only view of a running graph for probes and statistics.
/// Nothing here can slow the graph down: taps are lossy and counters are
/// plain atomics.
#[derive(Clone)]
pub struct Monitor {
    inner: Arc<MonitorInner>,
}

/// Raw counters of one block, see [`Monitor::stats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockCounters {
    pub block: String,
    /// Total time spent inside `work`, in nanoseconds.
    pub busy_ns: u64,
}

/// Raw counters of one connection, see [`Monitor::stats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeCounters {
    /// `block.port` of the output.
    pub from: String,
    /// `block.port` of the input.
    pub to: String,
    /// Items the output has produced so far.
    pub written: u64,
    /// Items waiting in the buffer for this input.
    pub backlog: usize,
    pub capacity: usize,
}

/// A snapshot of every counter. Rates come from comparing two snapshots.
#[derive(Debug, Clone, Default)]
pub struct GraphCounters {
    pub blocks: Vec<BlockCounters>,
    pub edges: Vec<EdgeCounters>,
}

impl Monitor {
    /// A lossy tap on output `port` (a name or an index) of `block`.
    pub fn tap(&self, block: &str, port: &str) -> Option<(StreamType, Tap)> {
        let outs = || self.inner.outputs.iter().filter(|o| o.block == block);
        outs()
            .find(|o| o.port == port)
            .or_else(|| {
                let i = port.parse::<usize>().ok()?;
                outs().find(|o| o.index == i)
            })
            .map(|o| (o.ty, o.tap.clone()))
    }

    pub fn stats(&self) -> GraphCounters {
        GraphCounters {
            blocks: self
                .inner
                .blocks
                .iter()
                .map(|(block, busy)| BlockCounters {
                    block: block.clone(),
                    busy_ns: busy.load(Ordering::Relaxed),
                })
                .collect(),
            edges: self
                .inner
                .edges
                .iter()
                .map(|e| EdgeCounters {
                    from: e.from.clone(),
                    to: e.to.clone(),
                    written: e.tap.written() as u64,
                    backlog: e.tap.backlog(e.reader),
                    capacity: e.tap.capacity(),
                })
                .collect(),
        }
    }
}

/// Cloneable handle that delivers live parameter changes to running blocks.
#[derive(Clone)]
pub struct ParamSender {
    blocks: Arc<HashMap<String, (mpsc::Sender<ParamMsg>, std::thread::Thread)>>,
}

impl ParamSender {
    /// Queue a parameter change for `block`. It applies before the block's
    /// next `work` call. Validate the value against the descriptor first:
    /// a value the block rejects stops the graph with an error.
    pub fn send(&self, block: &str, name: &str, value: serde_json::Value) -> Result<(), Error> {
        let (tx, thread) = self
            .blocks
            .get(block)
            .ok_or_else(|| Error::Graph(format!("unknown block `{block}`")))?;
        tx.send((name.to_owned(), value))
            .map_err(|_| Error::Block {
                block: block.to_owned(),
                source: Box::new(Error::Other("block has finished".into())),
            })?;
        thread.unpark();
        Ok(())
    }
}

/// Cloneable handle that can stop a [`Running`] graph from anywhere.
#[derive(Clone)]
pub struct StopHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::Thread>,
}

impl StopHandle {
    /// Ask every block to stop at its next opportunity.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in &self.threads {
            t.unpark();
        }
    }
}

impl Running {
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            stop: self.stop.clone(),
            threads: self.handles.iter().map(|h| h.thread().clone()).collect(),
        }
    }

    /// Taps and counters for probes and statistics.
    pub fn monitor(&self) -> Monitor {
        self.monitor.clone()
    }

    pub fn param_sender(&self) -> ParamSender {
        let blocks = self
            .names
            .iter()
            .zip(&self.param_txs)
            .zip(&self.handles)
            .map(|((n, tx), h)| (n.clone(), (tx.clone(), h.thread().clone())))
            .collect();
        ParamSender {
            blocks: Arc::new(blocks),
        }
    }

    /// Ask every block to stop at its next opportunity.
    pub fn stop(&self) {
        self.stop_handle().stop();
    }

    pub fn is_finished(&self) -> bool {
        self.handles.iter().all(JoinHandle::is_finished)
    }

    /// Wait for all blocks to finish; returns the first error, if any.
    pub fn wait(self) -> Result<(), Error> {
        let mut first = Ok(());
        for (h, name) in self.handles.into_iter().zip(self.names) {
            let r = h.join().unwrap_or_else(|_| {
                Err(Error::Block {
                    block: name,
                    source: Box::new(Error::Panic),
                })
            });
            if first.is_ok() {
                first = r;
            }
        }
        first
    }
}
