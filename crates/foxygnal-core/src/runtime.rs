//! Builds a runnable flowgraph and executes it.
//!
//! Blocks run as tasks on a pool of worker threads ([`crate::sched`]); the
//! ring buffers wake a block's task when data or space appears. Blocks that
//! ask for it ([`crate::Block::dedicated_thread`]) get a thread of their own,
//! which parks until woken. See docs/ARCHITECTURE.md.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, mpsc};
use std::task::Waker;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::block::{InputPort, OutputPort, WorkIo, WorkStatus};
use crate::buffer::{self, BufferKind, Reader, Tap};
use crate::graph::GraphSpec;
use crate::graph::param_port;
use crate::meta::{Configure, StreamMeta};
use crate::registry::{Instance, ParamSpec, Registry};
use crate::resolve::{GraphLoader, NoLoader, resolve};
use crate::sched::{Job, Next, Pool, thread_waker};
use crate::tag::TagPropagation;
use crate::{Error, StreamType};

type ParamMsg = (String, serde_json::Value);

/// Handle to a block inside a [`Flowgraph`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(usize);

struct Node {
    name: String,
    inst: Instance,
    /// Parameters wired as control inputs (`block.@name`). Control input `i`
    /// is input index `inst.inputs.len() + i` in [`Edge::to`].
    controls: Vec<ParamSpec>,
}

impl Node {
    /// Name and type of input `idx`: a stream input, or a control input
    /// (`@name`, always f32).
    fn input(&self, idx: usize) -> Option<(String, StreamType)> {
        let n = self.inst.inputs.len();
        match self.inst.inputs.get(idx) {
            Some((name, ty)) => Some((name.clone(), *ty)),
            None => self
                .controls
                .get(idx - n)
                .map(|c| (format!("@{}", c.name), StreamType::F32)),
        }
    }

    fn input_name(&self, idx: usize) -> String {
        self.input(idx).map(|(n, _)| n).unwrap_or_default()
    }
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
    buffer_kind: BufferKind,
    /// Pool size; `None`: one per core.
    workers: Option<usize>,
    thread_per_block: bool,
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

    /// Override the per-connection buffer capacity (items). Double-mapped
    /// buffers round it up to whole memory pages.
    pub fn with_buffer_capacity(mut self, items: usize) -> Self {
        self.buffer_capacity = Some(items);
        self
    }

    /// Size of the worker pool (default: one worker per core, never more
    /// than there are pooled blocks).
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = Some(workers.max(1));
        self
    }

    /// Give every block a thread of its own instead of using the worker
    /// pool (the scheduler before M5; for comparison and as a fallback).
    pub fn with_thread_per_block(mut self) -> Self {
        self.thread_per_block = true;
        self
    }

    /// Choose the buffer layout ([`BufferKind::Plain`] forces the
    /// copy-mirrored fallback, e.g. to test blocks with it).
    pub fn with_buffer_kind(mut self, kind: BufferKind) -> Self {
        self.buffer_kind = kind;
        self
    }

    pub fn add(&mut self, name: impl Into<String>, inst: Instance) -> BlockId {
        self.nodes.push(Node {
            name: name.into(),
            inst,
            controls: Vec::new(),
        });
        BlockId(self.nodes.len() - 1)
    }

    /// The block called `name`, e.g. to wire Rust blocks (sinks in tests)
    /// into a graph built from a spec.
    pub fn block_id(&self, name: &str) -> Option<BlockId> {
        self.nodes.iter().position(|n| n.name == name).map(BlockId)
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
            .input(inp)
            .ok_or_else(|| Error::Graph(format!("`{}` has no input port #{inp}", dst.name)))?;
        let (iname, ity) = (&iname, &ity);
        let edge_err = |msg: String| Error::Edge {
            from: format!("{}.{oname}", src.name),
            to: format!("{}.{iname}", dst.name),
            msg,
        };
        if oty != ity {
            let what = if inp >= dst.inst.inputs.len() {
                " (a setting wired as an input; add a converter to f32)"
            } else {
                ""
            };
            return Err(edge_err(format!(
                "type mismatch: `{}.{oname}` produces {oty} but `{}.{iname}` expects {ity}{what}",
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

    /// Wire parameter `spec` of block `to` as a control input and connect
    /// output `out` of `from` to it (an f32 stream). Before each `work` call
    /// of `to`, the runtime reads everything the control input holds and,
    /// if the last value changed, applies it with [`crate::Block::set_param`]
    /// (sample and hold). The parameter must be [`ParamSpec::wireable`].
    pub fn connect_param(
        &mut self,
        from: BlockId,
        out: usize,
        to: BlockId,
        spec: &ParamSpec,
    ) -> Result<(), Error> {
        if !spec.wireable() {
            let src = &self.nodes[from.0];
            let oname = src.inst.outputs.get(out).map_or("?", |(n, _)| n.as_str());
            let dst = &self.nodes[to.0].name;
            return Err(Error::Edge {
                from: format!("{}.{oname}", src.name),
                to: format!("{dst}.@{}", spec.name),
                msg: format!(
                    "`{}.{oname}` → `{dst}.@{}`: setting `{}` can't be wired as an input \
                     (only settings that change live and are numbers or on/off can)",
                    src.name, spec.name, spec.label
                ),
            });
        }
        let node = &mut self.nodes[to.0];
        let i = match node.controls.iter().position(|c| c.name == spec.name) {
            Some(i) => i,
            None => {
                node.controls.push(spec.clone());
                node.controls.len() - 1
            }
        };
        let inp = node.inst.inputs.len() + i;
        self.connect(from, out, to, inp)
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
            if let Some(param) = param_port(t.port) {
                let kind = &spec.blocks[tid.0].kind;
                let p = registry
                    .get(kind)
                    .and_then(|d| d.params.iter().find(|p| p.name == param))
                    .ok_or_else(|| {
                        edge_err(format!(
                            "`{}` → `{}`: block `{}` has no setting `{param}` to wire",
                            c.from, c.to, t.block
                        ))
                    })?;
                fg.connect_param(fid, out, tid, p)?;
                continue;
            }
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
                    to: format!("{}.{}", dst.name, dst.input_name(e.to.1)),
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

    /// Allocate buffers and start running: pooled blocks on the worker pool,
    /// the others on threads of their own.
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
            .map(|n| {
                (0..n.inst.inputs.len() + n.controls.len())
                    .map(|_| None)
                    .collect()
            })
            .collect();
        let mut outputs: Vec<Vec<OutputPort>> = Vec::with_capacity(self.nodes.len());
        let mut taps = Vec::new();
        let mut edge_stats = Vec::new();
        for (ni, n) in self.nodes.iter().enumerate() {
            let mut ports = Vec::new();
            for (pi, (pname, ty)) in n.inst.outputs.iter().enumerate() {
                let dests: Vec<_> = self.edges.iter().filter(|e| e.from == (ni, pi)).collect();
                let (writer, readers) =
                    buffer::channel_with(ty.item_size(), cap, dests.len(), self.buffer_kind);
                let tap = writer.tap();
                for (reader, (e, r)) in dests.into_iter().zip(readers).enumerate() {
                    inputs[e.to.0][e.to.1] = Some(r);
                    let dst = &self.nodes[e.to.0];
                    edge_stats.push(EdgeTap {
                        from: format!("{}.{pname}", n.name),
                        to: format!("{}.{}", dst.name, dst.input_name(e.to.1)),
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

        let ctl = Arc::new(Control {
            stop: AtomicBool::new(false),
            go: AtomicBool::new(false),
            wakers: OnceLock::new(),
            done: Mutex::new(Done {
                results: (0..self.nodes.len()).map(|_| None).collect(),
                remaining: self.nodes.len(),
            }),
            all_done: Condvar::new(),
            warnings: Arc::new(Warnings::default()),
        });
        let dedicated: Vec<bool> = self
            .nodes
            .iter()
            .map(|n| self.thread_per_block || n.inst.block.dedicated_thread())
            .collect();
        let pooled = dedicated.iter().filter(|d| !**d).count();
        let mut pool = (pooled > 0).then(|| {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            Pool::new(self.workers.unwrap_or(cores).min(pooled))
        });

        let mut names = Vec::new();
        let mut param_txs = Vec::new();
        let mut busy = Vec::new();
        let mut wakers = Vec::new();
        let mut threads = Vec::new();
        for (index, (((node, ins), outs), dedicated)) in self
            .nodes
            .into_iter()
            .zip(inputs)
            .zip(outputs)
            .zip(dedicated)
            .enumerate()
        {
            let mut ins = ins.into_iter();
            let inputs: Vec<InputPort> = node
                .inst
                .inputs
                .iter()
                .zip(ins.by_ref())
                .map(|((_, ty), reader)| InputPort::new(*ty, reader))
                .collect();
            let controls: Vec<ControlIn> = node
                .controls
                .into_iter()
                .zip(ins)
                .filter_map(|(spec, reader)| {
                    Some(ControlIn {
                        reader: reader?,
                        spec,
                        seen: f32::NAN.to_bits(),
                        reported: false,
                    })
                })
                .collect();
            let (ptx, prx) = mpsc::channel::<ParamMsg>();
            let busy_ns = Arc::new(AtomicU64::new(0));
            names.push(node.name.clone());
            param_txs.push(ptx);
            busy.push(busy_ns.clone());
            let run = BlockRun {
                index,
                name: node.name,
                inst: node.inst,
                inputs,
                controls,
                outputs: outs,
                params: prx,
                busy_ns,
                started: false,
                ctl: ctl.clone(),
            };
            if let (false, Some(pool)) = (dedicated, pool.as_mut()) {
                let task = pool.task();
                let waker = task.waker();
                run.register_waker(&waker);
                task.set_job(Box::new(PoolJob(Some(run))));
                wakers.push(waker);
            } else {
                let spawned = std::thread::Builder::new()
                    .name(format!("fox:{}", run.name))
                    .spawn(move || run_dedicated(run));
                match spawned {
                    Ok(h) => {
                        wakers.push(thread_waker(h.thread().clone()));
                        threads.push(h);
                    }
                    Err(e) => {
                        let _ = ctl.wakers.set(wakers);
                        ctl.release(&threads);
                        ctl.stop_all();
                        return Err(Error::Io(e));
                    }
                }
            }
        }
        let _ = ctl.wakers.set(wakers.clone());
        ctl.release(&threads);
        if let Some(pool) = pool.as_mut()
            && let Err(e) = pool.start()
        {
            ctl.stop_all();
            return Err(Error::Io(e));
        }
        let monitor = Monitor {
            inner: Arc::new(MonitorInner {
                blocks: names.iter().cloned().zip(busy).collect(),
                outputs: taps,
                edges: edge_stats,
                warnings: ctl.warnings.clone(),
            }),
        };
        Ok(Running {
            names,
            param_txs,
            wakers,
            ctl,
            monitor,
            pool,
            threads,
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

/// Most `work` calls in one turn of a pooled block before it goes to the back
/// of the queue, so one busy block can't starve the others on its worker.
const QUANTUM_STEPS: usize = 32;
/// Same, as time.
const QUANTUM: Duration = Duration::from_millis(2);

/// Shared by every block of a running graph.
struct Control {
    stop: AtomicBool,
    /// Set once every block's waker is known; dedicated threads wait for it
    /// before calling their block, so a stop can always reach everybody.
    go: AtomicBool,
    /// One per block, to wake them all on stop.
    wakers: OnceLock<Vec<Waker>>,
    done: Mutex<Done>,
    all_done: Condvar,
    warnings: Arc<Warnings>,
}

/// Problems that don't stop the graph, such as a control input value a
/// block rejects. Written only when a new problem appears (rarely), read by
/// [`Monitor::warnings`].
#[derive(Default)]
struct Warnings {
    /// Bumped on every new warning, so readers can tell cheaply whether
    /// anything changed.
    count: AtomicU64,
    list: Mutex<Vec<BlockWarning>>,
}

/// A problem with a running block that doesn't stop it, see
/// [`Monitor::warnings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockWarning {
    pub block: String,
    /// What it is about (`setting \`gain\``); one warning per block and key.
    pub key: String,
    pub message: String,
}

impl Warnings {
    /// Record a warning, replacing an earlier one about the same `key` of
    /// the same block.
    fn report(&self, block: &str, key: String, message: String) {
        let mut list = self.list.lock().unwrap_or_else(PoisonError::into_inner);
        match list.iter_mut().find(|w| w.block == block && w.key == key) {
            Some(w) => w.message = message,
            None => list.push(BlockWarning {
                block: block.to_owned(),
                key,
                message,
            }),
        }
        self.count.fetch_add(1, Ordering::Release);
    }
}

/// A parameter wired as a control input, while running.
struct ControlIn {
    reader: Reader,
    spec: ParamSpec,
    /// Bits of the last control value seen (NaN: none yet), so an unchanged
    /// value isn't applied again.
    seen: u32,
    /// A rejected value was reported; don't report again until a value is
    /// accepted.
    reported: bool,
}

impl ControlIn {
    /// Read everything available, keeping the last value. `Some` if it
    /// differs from the last value seen. No allocation, no locks.
    fn poll(&mut self) -> Option<f32> {
        let mut last = None;
        loop {
            let b = self.reader.slice();
            let n = b.len() / 4;
            if n == 0 {
                break;
            }
            let tail = &b[(n - 1) * 4..n * 4];
            last = Some(f32::from_ne_bytes([tail[0], tail[1], tail[2], tail[3]]));
            self.reader.consume(n);
        }
        // Tags on control streams mean nothing here; don't let them pile up.
        while self.reader.pop_tag().is_some() {}
        let v = last?;
        if v.is_nan() || v.to_bits() == self.seen {
            return None;
        }
        self.seen = v.to_bits();
        Some(v)
    }
}

struct Done {
    /// Per block, once it has finished.
    results: Vec<Option<Result<(), Error>>>,
    remaining: usize,
}

impl Control {
    /// Let the dedicated threads start calling their blocks.
    fn release(&self, threads: &[JoinHandle<()>]) {
        self.go.store(true, Ordering::Release);
        for t in threads {
            t.thread().unpark();
        }
    }

    /// Ask every block to stop, and wake them so they notice.
    fn stop_all(&self) {
        self.stop.store(true, Ordering::Release);
        for w in self.wakers.get().into_iter().flatten() {
            w.wake_by_ref();
        }
    }

    fn finished(&self, index: usize, result: Result<(), Error>) {
        if result.is_err() {
            // One failing block stops the rest of the graph.
            self.stop_all();
        }
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        done.results[index] = Some(result);
        done.remaining -= 1;
        if done.remaining == 0 {
            self.all_done.notify_all();
        }
    }
}

/// What one `work` call asks of the scheduler.
enum Step {
    Again,
    Idle,
    Wait(Duration),
    Finished,
}

/// A block with its ports: everything needed to call it, on whatever thread.
struct BlockRun {
    index: usize,
    name: String,
    inst: Instance,
    inputs: Vec<InputPort>,
    /// Parameters wired as inputs. Not in `inputs`: the block never sees
    /// them, and they never pace or end it.
    controls: Vec<ControlIn>,
    outputs: Vec<OutputPort>,
    params: mpsc::Receiver<ParamMsg>,
    busy_ns: Arc<AtomicU64>,
    /// `Block::start` succeeded, so `Block::stop` is due at the end.
    started: bool,
    ctl: Arc<Control>,
}

impl BlockRun {
    /// Wake `waker` when an input gets data or an output gets space.
    fn register_waker(&self, waker: &Waker) {
        for i in &self.inputs {
            if let Some(r) = &i.reader {
                r.register_waker(waker.clone());
            }
        }
        // A new control value applies promptly, even while the stream
        // inputs are idle.
        for c in &self.controls {
            c.reader.register_waker(waker.clone());
        }
        for o in &self.outputs {
            o.writer.register_waker(waker.clone());
        }
    }

    /// One `work` call (after `start`, the first time), with everything
    /// around it: live parameters, the tag snapshot, tag propagation and
    /// publishing what was produced.
    fn step(&mut self) -> Result<Step, Error> {
        if !self.started {
            self.inst.block.start()?;
            self.started = true;
        }
        if self.ctl.stop.load(Ordering::Acquire) {
            return Ok(Step::Finished);
        }
        // Live parameter changes. `try_recv` never blocks or locks.
        while let Ok((name, value)) = self.params.try_recv() {
            self.inst.block.set_param(&name, &value)?;
        }
        self.apply_controls();
        for i in &mut self.inputs {
            i.sync();
        }
        // Positions before `work`, for passing tags on (fixed-size: no allocation).
        let mut in_before = [0u64; MAX_TAG_PORTS];
        let mut out_before = [0u64; MAX_TAG_PORTS];
        for (b, i) in in_before.iter_mut().zip(&self.inputs) {
            *b = i.position();
        }
        for (b, o) in out_before.iter_mut().zip(&self.outputs) {
            *b = o.position();
        }
        // Whether every output had room going in: blocked with room and
        // ended input means done; blocked on a full output doesn't (see below).
        let had_room = self
            .outputs
            .iter()
            .all(|o| o.writer.abandoned() || o.writer.has_room());
        let mut io = WorkIo {
            inputs: &mut self.inputs,
            outputs: &mut self.outputs,
        };
        let t0 = Instant::now();
        let status = self.inst.block.work(&mut io);
        self.busy_ns
            .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if self.inst.block.tag_propagation() == TagPropagation::AllToAll {
            propagate_tags(&self.inputs, &mut self.outputs, &in_before, &out_before);
        }
        for i in &mut self.inputs {
            i.prune_tags();
        }
        // Publish after tags, so readers see an item's tags with the item.
        for o in &mut self.outputs {
            o.flush();
        }
        Ok(match status? {
            WorkStatus::Ok => Step::Again,
            WorkStatus::Done => Step::Finished,
            WorkStatus::Wait(d) => Step::Wait(d),
            WorkStatus::Blocked => {
                // Every consumer is gone: nobody wants our output.
                let abandoned =
                    !self.outputs.is_empty() && self.outputs.iter().all(|o| o.writer.abandoned());
                // Upstream ended and we made no progress although every
                // output had room: we're done too. Blocked on a full output,
                // a block may still have output due for input it already took
                // (an interpolator): wait for room (consumers wake us).
                let ended = had_room && self.inputs.iter().any(InputPort::exhausted);
                if ended || abandoned {
                    Step::Finished
                } else {
                    Step::Idle
                }
            }
        })
    }

    /// Apply the latest value of every control input that changed. A value
    /// the parameter or the block rejects is reported once (a warning, see
    /// [`Monitor::warnings`]) and otherwise ignored: the block keeps the last
    /// good value and the graph keeps running. Only that report allocates.
    fn apply_controls(&mut self) {
        for c in &mut self.controls {
            let Some(x) = c.poll() else { continue };
            let result = match c.spec.control_value(x) {
                Some(v) => self.inst.block.set_param(c.spec.name, &v).map_err(|e| {
                    format!(
                        "value {x} from the wire was rejected ({}); keeping the last good value",
                        e.detail()
                    )
                }),
                None => Err(format!(
                    "value {x} from the wire is out of range ({}); keeping the last good value",
                    c.spec
                        .check(&serde_json::Value::from(x as f64))
                        .err()
                        .unwrap_or_else(|| "not a valid value".into())
                )),
            };
            match result {
                Ok(()) => c.reported = false,
                Err(msg) if !c.reported => {
                    c.reported = true;
                    let key = format!("setting `{}`", c.spec.name);
                    self.ctl.warnings.report(&self.name, key, msg);
                }
                Err(_) => {}
            }
        }
    }

    /// [`Self::step`], turning a panic into an error.
    fn step_caught(&mut self) -> Result<Step, Finish> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.step())) {
            Ok(Ok(step)) => Ok(step),
            Ok(Err(e)) => Err(Finish::Error(e)),
            Err(_) => Err(Finish::Panic),
        }
    }

    /// The block is done: call `stop`, drop the ports (end of stream
    /// downstream, departure upstream) and report.
    fn finish(self, how: Finish) {
        let BlockRun {
            index,
            name,
            mut inst,
            inputs,
            outputs,
            started,
            ctl,
            ..
        } = self;
        let result = match how {
            // A block that panicked may be in any state: don't call it again.
            Finish::Panic => Err(Error::Panic),
            how => {
                let stopped = if started {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inst.block.stop()))
                        .unwrap_or(Err(Error::Panic))
                } else {
                    Ok(())
                };
                match how {
                    Finish::Error(e) => Err(e),
                    _ => stopped,
                }
            }
        };
        drop(inputs);
        drop(outputs);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(inst)));
        ctl.finished(
            index,
            result.map_err(|e| Error::Block {
                block: name,
                source: Box::new(e),
            }),
        );
    }
}

/// How a block ended.
enum Finish {
    Ok,
    Error(Error),
    Panic,
}

/// A block on the worker pool.
struct PoolJob(Option<BlockRun>);

impl Job for PoolJob {
    fn run(&mut self) -> Next {
        let Some(run) = self.0.as_mut() else {
            return Next::Done;
        };
        let t0 = Instant::now();
        let mut steps = 0;
        let how = loop {
            match run.step_caught() {
                Ok(Step::Again) => {
                    steps += 1;
                    if steps >= QUANTUM_STEPS || t0.elapsed() >= QUANTUM {
                        return Next::Again;
                    }
                }
                Ok(Step::Idle) => return Next::Idle,
                Ok(Step::Wait(d)) => return Next::Until(Instant::now() + d),
                Ok(Step::Finished) => break Finish::Ok,
                Err(how) => break how,
            }
        };
        self.0.take().expect("checked above").finish(how);
        Next::Done
    }
}

/// A block on a thread of its own: `start`, `work`, `set_param` and `stop`
/// all run here.
fn run_dedicated(mut run: BlockRun) {
    run.register_waker(&thread_waker(std::thread::current()));
    while !run.ctl.go.load(Ordering::Acquire) {
        std::thread::park();
    }
    let how = loop {
        match run.step_caught() {
            Ok(Step::Again) => {}
            // Neighbours (or a driver thread) unpark us; no polling.
            Ok(Step::Idle) => std::thread::park(),
            Ok(Step::Wait(d)) => std::thread::park_timeout(d),
            Ok(Step::Finished) => break Finish::Ok,
            Err(how) => break how,
        }
    };
    run.finish(how);
}

/// A started flowgraph.
pub struct Running {
    names: Vec<String>,
    param_txs: Vec<mpsc::Sender<ParamMsg>>,
    /// One per block.
    wakers: Vec<Waker>,
    ctl: Arc<Control>,
    monitor: Monitor,
    pool: Option<Pool>,
    threads: Vec<JoinHandle<()>>,
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
    warnings: Arc<Warnings>,
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

    /// Problems that don't stop the graph (a control input value a block
    /// rejected), one per block and key, latest message.
    pub fn warnings(&self) -> Vec<BlockWarning> {
        self.inner
            .warnings
            .list
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Bumped whenever a warning is reported: compare to see whether
    /// [`Self::warnings`] changed without locking.
    pub fn warning_count(&self) -> u64 {
        self.inner.warnings.count.load(Ordering::Acquire)
    }

    /// Adds each wire's buffer fill (0…1, in [`GraphCounters::edges`]
    /// order) to `acc`, which is resized to the number of wires the first
    /// time. A fill is an instant's snapshot; averaging many of these gives
    /// a fill that means something when blocks move whole buffers at a time.
    /// Allocates nothing once `acc` has its size.
    pub fn add_fills(&self, acc: &mut Vec<f32>) {
        acc.resize(self.inner.edges.len(), 0.0);
        for (a, e) in acc.iter_mut().zip(&self.inner.edges) {
            *a += e.tap.backlog(e.reader) as f32 / e.tap.capacity().max(1) as f32;
        }
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
    blocks: Arc<HashMap<String, (mpsc::Sender<ParamMsg>, Waker)>>,
}

impl ParamSender {
    /// Queue a parameter change for `block`. It applies before the block's
    /// next `work` call. Validate the value against the descriptor first:
    /// a value the block rejects stops the graph with an error.
    pub fn send(&self, block: &str, name: &str, value: serde_json::Value) -> Result<(), Error> {
        let (tx, waker) = self
            .blocks
            .get(block)
            .ok_or_else(|| Error::Graph(format!("unknown block `{block}`")))?;
        tx.send((name.to_owned(), value))
            .map_err(|_| Error::Block {
                block: block.to_owned(),
                source: Box::new(Error::Other("block has finished".into())),
            })?;
        waker.wake_by_ref();
        Ok(())
    }
}

/// Cloneable handle that can stop a [`Running`] graph from anywhere.
#[derive(Clone)]
pub struct StopHandle {
    ctl: Arc<Control>,
}

impl StopHandle {
    /// Ask every block to stop at its next opportunity.
    pub fn stop(&self) {
        self.ctl.stop_all();
    }
}

impl Running {
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            ctl: self.ctl.clone(),
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
            .zip(&self.wakers)
            .map(|((n, tx), w)| (n.clone(), (tx.clone(), w.clone())))
            .collect();
        ParamSender {
            blocks: Arc::new(blocks),
        }
    }

    /// Ask every block to stop at its next opportunity.
    pub fn stop(&self) {
        self.ctl.stop_all();
    }

    /// Every block has finished.
    pub fn is_finished(&self) -> bool {
        let done = self.ctl.done.lock().unwrap_or_else(PoisonError::into_inner);
        done.remaining == 0
    }

    /// Wait for all blocks to finish; returns the first error (in block
    /// order), if any.
    pub fn wait(self) -> Result<(), Error> {
        let results = {
            let mut done = self.ctl.done.lock().unwrap_or_else(PoisonError::into_inner);
            while done.remaining > 0 {
                done = self
                    .ctl
                    .all_done
                    .wait(done)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            std::mem::take(&mut done.results)
        };
        if let Some(pool) = self.pool {
            pool.join();
        }
        for t in self.threads {
            let _ = t.join();
        }
        results
            .into_iter()
            .zip(self.names)
            .map(|(r, block)| {
                r.unwrap_or_else(|| {
                    Err(Error::Block {
                        block,
                        source: Box::new(Error::Panic),
                    })
                })
            })
            .find(Result::is_err)
            .unwrap_or(Ok(()))
    }
}
