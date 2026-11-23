//! Out-of-tree block plugins: WebAssembly components (WIT world
//! `foxygnal:block/block-plugin`, `wit/block.wit`) run with wasmtime.
//!
//! [`PluginHost`] compiles plugins, and registers their blocks in a
//! [`Registry`] under the kind `<plugin>:<kind>` (e.g. `example:gain`). Each
//! block built from a plugin is a native [`foxygnal_core::Block`] that owns
//! its own wasm instance, so plugin blocks are as isolated from each other
//! as from the engine.
//!
//! Sandbox: plugins import nothing but the four `foxygnal:block/host`
//! functions, so they have no filesystem, network, clock or randomness. Each
//! instance has a memory limit, and every call into a plugin has a deadline
//! (epoch interruption): a plugin that loops forever fails its block after
//! the deadline instead of hanging a worker thread. A trap (panic, out of
//! bounds, out of memory) is an error on that block, never a crash.
//!
//! See docs/PLUGINS.md.

mod block;
mod convert;
mod ticker;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use foxygnal_core::{Error, Registry};
use wasmtime::component::{Component, ComponentExportIndex, InstancePre, Linker};
use wasmtime::{Config, Engine};

pub use block::MAX_PORTS;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "block-plugin",
    });
}

use bindings::foxygnal::block::types as wit_types;

/// Name of the exported interface, as the component model spells it.
const PLUGIN_INTERFACE: &str = "foxygnal:block/plugin@0.1.0";
const HOST_INTERFACE: &str = "foxygnal:block/host@0.1.0";

/// Sandbox limits.
#[derive(Debug, Clone)]
pub struct Options {
    /// Linear memory per block instance, in bytes.
    pub memory_limit: usize,
    /// How long one call into a plugin (`work`, `create`, `configure`, …)
    /// may run before it is interrupted and the block fails.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            memory_limit: 64 << 20,
            timeout: Duration::from_millis(250),
        }
    }
}

/// Compiles and instantiates plugins. One per process is enough; blocks keep
/// what they need alive.
#[derive(Clone)]
pub struct PluginHost {
    shared: Arc<Shared>,
}

pub(crate) struct Shared {
    engine: Engine,
    linker: Linker<block::State>,
    ticker: ticker::Ticker,
    options: Options,
    /// Epoch ticks per call deadline.
    deadline_ticks: u64,
}

/// A loaded plugin: its compiled component and what it provides.
pub struct Plugin {
    name: &'static str,
    version: String,
    path: Option<PathBuf>,
    pre: InstancePre<block::State>,
    work_index: ComponentExportIndex,
    blocks: Vec<wit_types::BlockDescriptor>,
}

impl Plugin {
    /// The plugin's name, which prefixes its block kinds.
    pub fn name(&self) -> &str {
        self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The file it was loaded from, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The engine kinds of its blocks (`<plugin>:<kind>`).
    pub fn kinds(&self) -> Vec<String> {
        self.blocks
            .iter()
            .map(|b| format!("{}:{}", self.name, b.kind))
            .collect()
    }
}

fn err(msg: impl std::fmt::Display) -> Error {
    Error::Other(msg.to_string())
}

impl PluginHost {
    pub fn new(options: Options) -> Result<Self, Error> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        config.wasm_component_model(true);
        // Copy-on-write memory images need memfd_create, which containers
        // and sandboxes often forbid (CI runners do): instantiation then
        // fails outright. Plugins are small, one instance per block, so
        // copying their data segments at startup costs next to nothing.
        config.memory_init_cow(false);
        let engine = Engine::new(&config).map_err(|e| err(format!("wasmtime: {e}")))?;
        let mut linker = Linker::new(&engine);
        block::add_host_functions(
            &mut linker
                .instance(HOST_INTERFACE)
                .map_err(|e| err(format!("wasmtime: {e}")))?,
        )
        .map_err(|e| err(format!("wasmtime: {e}")))?;
        let ticker = ticker::Ticker::new(engine.clone());
        let deadline_ticks =
            (options.timeout.as_micros() / ticker::TICK.as_micros()).max(1) as u64 + 1;
        Ok(Self {
            shared: Arc::new(Shared {
                engine,
                linker,
                ticker,
                options,
                deadline_ticks,
            }),
        })
    }

    pub fn options(&self) -> &Options {
        &self.shared.options
    }

    /// Load a plugin file: a component, or a core module built with the SDK
    /// for `wasm32-unknown-unknown` (turned into a component here).
    pub fn load_file(&self, path: &Path) -> Result<Arc<Plugin>, Error> {
        let bytes = std::fs::read(path)?;
        let mut p = self.load_bytes(&bytes)?;
        if let Some(p) = Arc::get_mut(&mut p) {
            p.path = Some(path.to_owned());
        }
        Ok(p)
    }

    /// Load a plugin from memory (see [`Self::load_file`]).
    pub fn load_bytes(&self, bytes: &[u8]) -> Result<Arc<Plugin>, Error> {
        let component_bytes;
        let bytes = if is_core_module(bytes) {
            component_bytes = wit_component::ComponentEncoder::default()
                .module(bytes)
                .and_then(|e| e.validate(true).encode())
                .map_err(|e| {
                    err(format!(
                        "not a foxygnal plugin (a core module without the plugin's WIT \
                         metadata; build it with foxygnal-plugin-sdk): {e:#}"
                    ))
                })?;
            &component_bytes[..]
        } else {
            bytes
        };
        let engine = &self.shared.engine;
        let component =
            Component::new(engine, bytes).map_err(|e| err(format!("compiling: {e}")))?;
        let pre = self
            .shared
            .linker
            .instantiate_pre(&component)
            .map_err(|e| {
                err(format!(
                    "{e} (plugins may only import `{HOST_INTERFACE}`: no WASI; build for \
                     wasm32-unknown-unknown)"
                ))
            })?;
        let iface = component
            .get_export_index(None, PLUGIN_INTERFACE)
            .ok_or_else(|| {
                err(format!(
                    "not a foxygnal plugin: doesn't export `{PLUGIN_INTERFACE}`"
                ))
            })?;
        let work_index = component
            .get_export_index(Some(&iface), "[method]block.work")
            .ok_or_else(|| err("doesn't export `block.work`"))?;

        // Ask it what it provides, in a throwaway instance.
        let mut store = block::new_store(&self.shared);
        let instance = {
            let _busy = self.shared.ticker.enter();
            store.set_epoch_deadline(self.shared.deadline_ticks);
            pre.instantiate(&mut store)
                .map_err(|e| err(format!("instantiating: {e}")))?
        };
        let guest = bindings::BlockPlugin::new(&mut store, &instance)
            .map_err(|e| err(format!("wrong exports: {e}")))?;
        // Type-check `work` once, so blocks can call it unchecked.
        block::check_work_type(&mut store, &instance, &work_index)?;
        let info = {
            let _busy = self.shared.ticker.enter();
            store.set_epoch_deadline(self.shared.deadline_ticks);
            guest
                .foxygnal_block_plugin()
                .call_info(&mut store)
                .map_err(|e| block::describe_trap(&self.shared, "info", e))?
        };
        let name = convert::check_plugin_name(&info.name)?;
        for b in &info.blocks {
            convert::check_block(b).map_err(|e| err(format!("block `{}`: {e}", b.kind)))?;
        }
        Ok(Arc::new(Plugin {
            name: convert::leak(name),
            version: info.version,
            path: None,
            pre,
            work_index,
            blocks: info.blocks,
        }))
    }

    /// Register every block of `plugin`. Fails without registering anything
    /// if a kind is taken (e.g. two plugins with the same name).
    pub fn register(&self, plugin: &Arc<Plugin>, registry: &mut Registry) -> Result<(), Error> {
        let descs: Vec<_> = plugin
            .blocks
            .iter()
            .map(|b| convert::descriptor(plugin.name, b))
            .collect::<Result<_, _>>()?;
        if let Some(d) = descs.iter().find(|d| registry.get(d.kind).is_some()) {
            return Err(err(format!(
                "block kind `{}` is already registered (two plugins named `{}`?)",
                d.kind, plugin.name
            )));
        }
        for (d, b) in descs.into_iter().zip(&plugin.blocks) {
            let factory = block::factory(
                self.shared.clone(),
                plugin.clone(),
                b.kind.clone(),
                d.params.clone(),
                b.tags == wit_types::TagMode::Manual,
            );
            registry.register_plugin(d, plugin.name, factory)?;
        }
        Ok(())
    }

    /// Load every `*.wasm` in `dirs` (not recursively) and register their
    /// blocks. Directories that don't exist are skipped. Returns what
    /// happened to each file; failures are also logged.
    pub fn load_dirs(&self, dirs: &[PathBuf], registry: &mut Registry) -> Vec<LoadReport> {
        let mut files = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "wasm") && p.is_file())
                .collect();
            found.sort();
            files.extend(found);
        }
        // Compile in parallel: Cranelift dominates start-up.
        let loaded: Vec<(PathBuf, Duration, Result<Arc<Plugin>, Error>)> =
            std::thread::scope(|s| {
                let jobs: Vec<_> = files
                    .into_iter()
                    .map(|path| {
                        s.spawn(move || {
                            let t = Instant::now();
                            let r = self.load_file(&path);
                            (path, t.elapsed(), r)
                        })
                    })
                    .collect();
                jobs.into_iter()
                    .map(|j| j.join().expect("plugin loader panicked"))
                    .collect()
            });
        loaded
            .into_iter()
            .map(|(path, took, r)| {
                let result = r.and_then(|p| {
                    self.register(&p, registry)?;
                    tracing::info!(
                        "plugin `{}` {} from {} ({} blocks, {:.0} ms)",
                        p.name,
                        p.version,
                        path.display(),
                        p.blocks.len(),
                        took.as_secs_f64() * 1e3
                    );
                    Ok(p)
                });
                if let Err(e) = &result {
                    tracing::warn!("plugin {} not loaded: {e}", path.display());
                }
                LoadReport { path, result }
            })
            .collect()
    }
}

/// What happened to one plugin file in [`PluginHost::load_dirs`].
pub struct LoadReport {
    pub path: PathBuf,
    pub result: Result<Arc<Plugin>, Error>,
}

/// Where plugins are looked for by default: `<project>/plugins`, then the
/// user's data directory (`$XDG_DATA_HOME/foxygnal/plugins`, else
/// `~/.local/share/foxygnal/plugins`).
pub fn default_dirs(project_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project_dir.join("plugins")];
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(d) = data {
        dirs.push(d.join("foxygnal/plugins"));
    }
    dirs
}

fn is_core_module(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..4] == *b"\0asm" && bytes[4..8] == [1, 0, 0, 0]
}
