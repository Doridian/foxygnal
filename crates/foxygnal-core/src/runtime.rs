//! Builds a runnable flowgraph and executes it.
//!
//! M0 scheduler: one OS thread per block. Blocks park when they cannot make
//! progress and are unparked by their neighbours when data or space appears.
//! See docs/ARCHITECTURE.md for the planned pooled scheduler.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::Error;
use crate::block::{InputPort, OutputPort, WorkIo, WorkStatus};
use crate::buffer::{self, Reader};
use crate::graph::GraphSpec;
use crate::registry::{Instance, Registry};

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
    pub fn from_spec(spec: &GraphSpec, registry: &Registry) -> Result<Self, Error> {
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

    /// Allocate buffers and spawn one thread per block.
    pub fn start(self) -> Result<Running, Error> {
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

        let mut inputs: Vec<Vec<Option<Reader>>> = self
            .nodes
            .iter()
            .map(|n| n.inst.inputs.iter().map(|_| None).collect())
            .collect();
        let mut outputs: Vec<Vec<OutputPort>> = Vec::with_capacity(self.nodes.len());
        for (ni, n) in self.nodes.iter().enumerate() {
            let mut ports = Vec::new();
            for (pi, (_, ty)) in n.inst.outputs.iter().enumerate() {
                let dests: Vec<_> = self.edges.iter().filter(|e| e.from == (ni, pi)).collect();
                let (writer, readers) = buffer::channel(ty.item_size(), cap, dests.len());
                for (e, r) in dests.into_iter().zip(readers) {
                    inputs[e.to.0][e.to.1] = Some(r);
                }
                ports.push(OutputPort { ty: *ty, writer });
            }
            outputs.push(ports);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let mut names = Vec::new();
        let mut param_txs = Vec::new();
        for ((node, ins), outs) in self.nodes.into_iter().zip(inputs).zip(outputs) {
            let ins: Vec<InputPort> = node
                .inst
                .inputs
                .iter()
                .zip(ins)
                .map(|((_, ty), reader)| InputPort { ty: *ty, reader })
                .collect();
            let stop = stop.clone();
            let name = node.name.clone();
            let (ptx, prx) = mpsc::channel::<ParamMsg>();
            names.push(name.clone());
            param_txs.push(ptx);
            let h = std::thread::Builder::new()
                .name(format!("fox:{name}"))
                .spawn(move || {
                    // A panicking block must stop the rest of the graph too.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_block(node.inst, ins, outs, &prx, &stop)
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
        Ok(Running {
            handles,
            names,
            param_txs,
            stop,
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

fn run_block(
    mut inst: Instance,
    mut inputs: Vec<InputPort>,
    mut outputs: Vec<OutputPort>,
    params: &mpsc::Receiver<ParamMsg>,
    stop: &AtomicBool,
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
        let mut io = WorkIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        match inst.block.work(&mut io) {
            Err(e) => break Err(e),
            Ok(WorkStatus::Ok) => {}
            Ok(WorkStatus::Done) => break Ok(()),
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
