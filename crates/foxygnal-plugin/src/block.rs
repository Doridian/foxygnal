//! A plugin block as a native block: one wasm instance per block, and the
//! host side of the `foxygnal:block/host` imports.
//!
//! Data path of one `work` call, with no allocation on the host:
//! 1. each input's readable bytes (at most [`MAX_ITEMS`] items) are lowered
//!    as `list<u8>` arguments: wasmtime copies them from the ring buffer
//!    straight into memory the plugin allocates;
//! 2. the plugin computes into its own (reused) output buffers and calls
//!    `emit(port, bytes)`, which copies from its memory straight into the
//!    output ring buffer, and `consume(port, n)`;
//! 3. `work` returns a status; the host then consumes and produces.

use std::sync::Arc;
use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, Configure, DynFactory, Error, Instance, OutputPort, ParamKind, ParamReader, ParamSpec,
    StreamType, TagPropagation, WorkIo, WorkStatus,
};
use serde_json::{Value, json};
use wasmtime::component::{
    ComponentExportIndex, Func, LinkerInstance, ResourceAny, TypedFunc, WasmList, WasmStr,
};
use wasmtime::{ResourceLimiter, Store, StoreContextMut, Trap};

use crate::convert::{self, stream_type};
use crate::{Plugin, Shared, bindings, wit_types};

/// Most inputs or outputs a plugin block may have.
pub const MAX_PORTS: usize = 8;
/// Most items per port handed to one `work` call. Bounds the plugin's
/// buffers (64 KB per c32 port) and the time one call takes.
pub const MAX_ITEMS: usize = 8192;
/// Log lines a block may write per second.
const LOG_PER_SEC: u32 = 10;

type WorkParams<'a> = (
    ResourceAny,
    &'a [&'a [u8]],
    &'a [u32],
    &'a [wit_types::InputTag],
);
type WorkResults = (Result<wit_types::WorkStatus, String>,);

/// Store data: the sandbox's limits and the state of the current call.
pub struct State {
    limiter: Limiter,
    io: CallIo,
    plugin: &'static str,
    log_window: Instant,
    log_count: u32,
}

/// What the host imports may touch during one `work` call. Outside a call
/// `outputs` is null and the imports fail.
struct CallIo {
    /// A `work` call is in progress.
    active: bool,
    outputs: *mut OutputPort,
    n_out: usize,
    n_in: usize,
    item_out: [usize; MAX_PORTS],
    /// Room per output, in bytes.
    space: [usize; MAX_PORTS],
    /// Bytes emitted per output so far.
    emitted: [usize; MAX_PORTS],
    /// Items readable per input.
    avail: [usize; MAX_PORTS],
    consumed: [usize; MAX_PORTS],
}

impl Default for CallIo {
    fn default() -> Self {
        Self {
            active: false,
            outputs: std::ptr::null_mut(),
            n_out: 0,
            n_in: 0,
            item_out: [0; MAX_PORTS],
            space: [0; MAX_PORTS],
            emitted: [0; MAX_PORTS],
            avail: [0; MAX_PORTS],
            consumed: [0; MAX_PORTS],
        }
    }
}

// SAFETY: `outputs` points at the `WorkIo` outputs of the call in progress,
// on the thread making the call, and is null at all other times.
unsafe impl Send for State {}

struct Limiter {
    memory: usize,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory {
            return Err(wasmtime::Error::msg(format!(
                "plugin ran out of memory (limit {} MiB)",
                self.memory >> 20
            )));
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 100_000)
    }
}

pub fn new_store(shared: &Shared) -> Store<State> {
    let mut store = Store::new(
        &shared.engine,
        State {
            limiter: Limiter {
                memory: shared.options.memory_limit,
            },
            io: CallIo::default(),
            plugin: "",
            log_window: Instant::now(),
            log_count: 0,
        },
    );
    store.limiter(|s| &mut s.limiter);
    store
}

fn host_err(msg: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(msg.into())
}

pub fn add_host_functions(l: &mut LinkerInstance<'_, State>) -> wasmtime::Result<()> {
    l.func_wrap(
        "emit",
        |mut cx: StoreContextMut<'_, State>, (port, data): (u32, WasmList<u8>)| {
            let p = port as usize;
            let io = &cx.data().io;
            if !io.active {
                return Err(host_err("emit called outside work"));
            }
            if p >= io.n_out {
                return Err(host_err(format!(
                    "emit to output {port}, which doesn't exist"
                )));
            }
            let (ptr, at, space) = (io.outputs, io.emitted[p], io.space[p]);
            let bytes = data.as_le_slice(&cx);
            let end = at + bytes.len();
            if end > space {
                return Err(host_err(format!(
                    "emitted more than the room on output {port} ({} items)",
                    space / cx.data().io.item_out[p].max(1)
                )));
            }
            // SAFETY: see `CallIo`; `p < n_out`, and nothing else touches
            // the outputs during the call.
            let out = unsafe { &mut *ptr.add(p) };
            out.bytes_mut()[at..end].copy_from_slice(bytes);
            cx.data_mut().io.emitted[p] = end;
            Ok(())
        },
    )?;
    l.func_wrap(
        "consume",
        |mut cx: StoreContextMut<'_, State>, (port, items): (u32, u32)| {
            let p = port as usize;
            let io = &mut cx.data_mut().io;
            if !io.active {
                return Err(host_err("consume called outside work"));
            }
            if p >= io.n_in {
                return Err(host_err(format!(
                    "consume on input {port}, which doesn't exist"
                )));
            }
            let total = io.consumed[p] + items as usize;
            if total > io.avail[p] {
                return Err(host_err(format!(
                    "consumed {total} items on input {port}, but only {} were passed",
                    io.avail[p]
                )));
            }
            io.consumed[p] = total;
            Ok(())
        },
    )?;
    l.func_wrap(
        "add-tag",
        |cx: StoreContextMut<'_, State>,
         (port, index, key, value): (u32, u32, String, wit_types::TagValue)| {
            let p = port as usize;
            let io = &cx.data().io;
            if !io.active {
                return Err(host_err("add-tag called outside work"));
            }
            if p >= io.n_out {
                return Err(host_err(format!(
                    "tag on output {port}, which doesn't exist"
                )));
            }
            if index as usize * io.item_out[p] >= io.space[p] {
                return Err(host_err(format!(
                    "tag at item {index} of output {port}, beyond its room"
                )));
            }
            // SAFETY: as in `emit`.
            let out = unsafe { &mut *io.outputs.add(p) };
            out.add_tag(index as usize, &key, convert::tag_from_wit(value));
            Ok(())
        },
    )?;
    l.func_wrap(
        "log",
        |mut cx: StoreContextMut<'_, State>, (msg,): (WasmStr,)| {
            let now = Instant::now();
            let st = cx.data_mut();
            if now.duration_since(st.log_window) >= Duration::from_secs(1) {
                st.log_window = now;
                st.log_count = 0;
            }
            st.log_count += 1;
            if st.log_count <= LOG_PER_SEC {
                let plugin = st.plugin;
                let msg = msg.to_str(&cx)?;
                tracing::info!(target: "foxygnal_plugin::guest", "{plugin}: {msg}");
            }
            Ok(())
        },
    )?;
    Ok(())
}

/// Check `work`'s signature once per plugin (blocks then call it unchecked).
pub fn check_work_type(
    store: &mut Store<State>,
    instance: &wasmtime::component::Instance,
    index: &ComponentExportIndex,
) -> Result<(), Error> {
    let f = instance
        .get_func(&mut *store, index)
        .ok_or_else(|| Error::Other("`block.work` isn't a function".into()))?;
    f.typed::<WorkParams<'_>, WorkResults>(&*store)
        .map_err(|e| Error::Other(format!("`block.work` has the wrong type: {e}")))?;
    Ok(())
}

/// A readable message for a failed call into a plugin.
pub fn describe_trap(shared: &Shared, what: &str, e: wasmtime::Error) -> Error {
    if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
        return Error::Other(format!(
            "plugin {what} ran longer than {} ms and was stopped (an endless loop?)",
            shared.options.timeout.as_millis()
        ));
    }
    Error::Other(format!("plugin crashed in {what}: {e:#}"))
}

pub fn factory(
    shared: Arc<Shared>,
    plugin: Arc<Plugin>,
    kind: String,
    params: Vec<ParamSpec>,
    manual_tags: bool,
) -> DynFactory {
    Box::new(move |p: &ParamReader<'_>| {
        let full_kind = format!("{}:{kind}", plugin.name);
        let mut values = Vec::with_capacity(params.len());
        for spec in &params {
            let v: Value = match spec.kind {
                ParamKind::Float { .. } => json!(p.f64(spec.name)?),
                ParamKind::Int { .. } => json!(p.i64(spec.name)?),
                ParamKind::Bool => json!(p.bool(spec.name)?),
                _ => json!(p.str(spec.name)?),
            };
            let param_err = |msg: String| Error::Param {
                block: full_kind.clone(),
                param: spec.name.to_owned(),
                msg,
            };
            spec.check(&v).map_err(param_err)?;
            values.push((
                spec.name.to_owned(),
                convert::param_value(spec, &v).map_err(param_err)?,
            ));
        }
        PluginBlock::build(&shared, &plugin, &kind, &values, &params, manual_tags)
    })
}

pub struct PluginBlock {
    store: Store<State>,
    shared: Arc<Shared>,
    guest: bindings::BlockPlugin,
    handle: ResourceAny,
    work: Func,
    item_in: Vec<usize>,
    item_out: Vec<usize>,
    params: Vec<ParamSpec>,
    manual_tags: bool,
    /// Input tags for the plugin (manual mode); reused.
    tags: Vec<wit_types::InputTag>,
    /// Set once the plugin trapped: its instance can't be used again.
    dead: Option<String>,
}

impl PluginBlock {
    fn build(
        shared: &Arc<Shared>,
        plugin: &Arc<Plugin>,
        kind: &str,
        values: &[(String, wit_types::ParamValue)],
        params: &[ParamSpec],
        manual_tags: bool,
    ) -> Result<Instance, Error> {
        let desc = plugin
            .blocks
            .iter()
            .find(|b| b.kind == kind)
            .ok_or_else(|| Error::UnknownBlock(kind.to_owned()))?;
        let mut store = new_store(shared);
        store.data_mut().plugin = plugin.name;
        let instance = {
            let _busy = shared.ticker.enter();
            store.set_epoch_deadline(shared.deadline_ticks);
            plugin
                .pre
                .instantiate(&mut store)
                .map_err(|e| describe_trap(shared, "start-up", e))?
        };
        let guest = bindings::BlockPlugin::new(&mut store, &instance)
            .map_err(|e| Error::Other(format!("plugin exports: {e}")))?;
        let created = {
            let _busy = shared.ticker.enter();
            store.set_epoch_deadline(shared.deadline_ticks);
            guest
                .foxygnal_block_plugin()
                .call_create(&mut store, kind, values)
                .map_err(|e| describe_trap(shared, "create", e))?
                .map_err(Error::Other)?
        };
        let check = |what: &str, ports: &[wit_types::PortSpec], got: &[wit_types::StreamType]| {
            if ports.len() != got.len() {
                return Err(Error::Other(format!(
                    "plugin built {} {what}s, its descriptor has {}",
                    got.len(),
                    ports.len()
                )));
            }
            for (p, t) in ports.iter().zip(got) {
                if p.type_.is_some_and(|d| d != *t) {
                    return Err(Error::Other(format!(
                        "plugin built {what} `{}` as {}, its descriptor says {}",
                        p.name,
                        stream_type(*t),
                        stream_type(p.type_.unwrap_or(*t))
                    )));
                }
            }
            Ok(())
        };
        check("input", &desc.inputs, &created.inputs)?;
        check("output", &desc.outputs, &created.outputs)?;
        let work = instance
            .get_func(&mut store, plugin.work_index)
            .ok_or_else(|| Error::Other("`block.work` is missing".into()))?;
        let ports = |specs: &[wit_types::PortSpec], types: &[wit_types::StreamType]| {
            specs
                .iter()
                .zip(types)
                .map(|(p, t)| (p.name.clone(), stream_type(*t)))
                .collect::<Vec<(String, StreamType)>>()
        };
        let inputs = ports(&desc.inputs, &created.inputs);
        let outputs = ports(&desc.outputs, &created.outputs);
        let block = PluginBlock {
            store,
            shared: shared.clone(),
            guest,
            handle: created.block,
            work,
            item_in: inputs.iter().map(|(_, t)| t.item_size()).collect(),
            item_out: outputs.iter().map(|(_, t)| t.item_size()).collect(),
            params: params.to_vec(),
            manual_tags,
            tags: Vec::new(),
            dead: None,
        };
        Ok(Instance {
            block: Box::new(block),
            inputs,
            outputs,
        })
    }

    fn alive(&self) -> Result<(), Error> {
        match &self.dead {
            Some(msg) => Err(Error::Other(msg.clone())),
            None => Ok(()),
        }
    }

    /// Remember a failed call: the instance is unusable after a trap.
    fn fail(&mut self, what: &str, e: wasmtime::Error) -> Error {
        let e = describe_trap(&self.shared, what, e);
        self.dead = Some(e.to_string());
        e
    }
}

impl Block for PluginBlock {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        self.alive()?;
        let n_in = io.inputs.len();
        let n_out = io.outputs.len();

        // Nothing to read on any input: nothing a plugin with inputs can do.
        if n_in > 0 && io.inputs.iter().all(|i| i.bytes().is_empty()) {
            return Ok(WorkStatus::Blocked);
        }

        let mut space = [0u32; MAX_PORTS];
        let mut state = CallIo {
            active: true,
            n_in,
            n_out,
            ..CallIo::default()
        };
        for (o, port) in io.outputs.iter_mut().enumerate() {
            let size = self.item_out[o];
            let items = (port.bytes_mut().len() / size).min(MAX_ITEMS);
            space[o] = items as u32;
            state.item_out[o] = size;
            state.space[o] = items * size;
        }
        let mut inputs: [&[u8]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (i, port) in io.inputs.iter().enumerate() {
            let size = self.item_in[i];
            let b = port.bytes();
            let items = (b.len() / size).min(MAX_ITEMS);
            inputs[i] = &b[..items * size];
            state.avail[i] = items;
        }
        self.tags.clear();
        if self.manual_tags {
            for (i, port) in io.inputs.iter().enumerate() {
                for (index, t) in port.tags_in(state.avail[i]) {
                    self.tags.push(wit_types::InputTag {
                        port: i as u32,
                        index: index as u32,
                        key: t.key.to_string(),
                        value: convert::tag_to_wit(&t.value),
                    });
                }
            }
        }
        state.outputs = io.outputs.as_mut_ptr();
        self.store.data_mut().io = state;

        // SAFETY: the type was checked when the plugin loaded
        // (`check_work_type`), and `work` comes from this store's instance.
        let f = unsafe { TypedFunc::<WorkParams<'_>, WorkResults>::new_unchecked(self.work) };
        let result = {
            let _busy = self.shared.ticker.enter();
            self.store.set_epoch_deadline(self.shared.deadline_ticks);
            f.call(
                &mut self.store,
                (self.handle, &inputs[..n_in], &space[..n_out], &self.tags),
            )
        };
        let state = std::mem::take(&mut self.store.data_mut().io);
        let status = match result {
            Ok((Ok(status),)) => status,
            Ok((Err(msg),)) => return Err(Error::Other(msg)),
            Err(e) => return Err(self.fail("work", e)),
        };

        for (o, port) in io.outputs.iter_mut().enumerate() {
            let bytes = state.emitted[o];
            if !bytes.is_multiple_of(self.item_out[o]) {
                let msg = format!("plugin emitted a partial item on output {o}");
                self.dead = Some(msg.clone());
                return Err(Error::Other(msg));
            }
            if bytes > 0 {
                port.produce(bytes / self.item_out[o]);
            }
        }
        for (i, port) in io.inputs.iter_mut().enumerate() {
            if state.consumed[i] > 0 {
                port.consume(state.consumed[i]);
            }
        }
        Ok(match status {
            wit_types::WorkStatus::Ok => WorkStatus::Ok,
            wit_types::WorkStatus::Blocked => WorkStatus::Blocked,
            wit_types::WorkStatus::Wait(ns) => WorkStatus::Wait(Duration::from_nanos(ns)),
            wit_types::WorkStatus::Done => WorkStatus::Done,
        })
    }

    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        self.alive()?;
        let inputs: Vec<_> = cx.inputs.iter().map(convert::meta_to_wit).collect();
        let result = {
            let _busy = self.shared.ticker.enter();
            self.store.set_epoch_deadline(self.shared.deadline_ticks);
            self.guest.foxygnal_block_plugin().block().call_configure(
                &mut self.store,
                self.handle,
                &inputs,
            )
        };
        let outputs = match result {
            Ok(r) => r.map_err(Error::Other)?,
            Err(e) => return Err(self.fail("configure", e)),
        };
        if outputs.is_empty() {
            return Ok(());
        }
        if outputs.len() != cx.outputs.len() {
            return Err(Error::Other(format!(
                "plugin configure returned {} outputs, the block has {}",
                outputs.len(),
                cx.outputs.len()
            )));
        }
        for (o, m) in cx.outputs.iter_mut().zip(&outputs) {
            *o = convert::meta_from_wit(m);
        }
        Ok(())
    }

    fn set_param(&mut self, name: &str, value: &Value) -> Result<(), Error> {
        self.alive()?;
        let spec = self
            .params
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| Error::Other(format!("no parameter `{name}`")))?;
        let v = convert::param_value(spec, value).map_err(Error::Other)?;
        let result = {
            let _busy = self.shared.ticker.enter();
            self.store.set_epoch_deadline(self.shared.deadline_ticks);
            self.guest.foxygnal_block_plugin().block().call_set_param(
                &mut self.store,
                self.handle,
                name,
                &v,
            )
        };
        match result {
            Ok(r) => r.map_err(Error::Other),
            Err(e) => Err(self.fail("set_param", e)),
        }
    }

    fn tag_propagation(&self) -> TagPropagation {
        if self.manual_tags {
            TagPropagation::Manual
        } else {
            TagPropagation::AllToAll
        }
    }
}
