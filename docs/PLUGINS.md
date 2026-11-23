# Block plugins

foxygnal loads out-of-tree blocks from **WebAssembly components**. A plugin is one `.wasm`
file that exports block descriptors and a block resource (WIT world
`foxygnal:block/block-plugin`, [`wit/block.wit`](../wit/block.wit)). The engine wraps every
plugin block as a native block, so it shows up in the palette, the inspector, graph files, live
parameters, stream metadata and tags like any built-in block: the UI is generated from the
descriptor, with no per-block UI code.

Why WASM components: plugins are portable (one file runs on x86-64 and the Pi), sandboxed (no
filesystem, network, or way to crash the engine) and need no ABI promises from the engine's
Rust internals. The price is speed: see [Performance](#performance). A block that turns out to
be performance-critical can move into `foxygnal-blocks` instead.

## Writing a plugin

A plugin is a Rust `cdylib` using [`foxygnal-plugin-sdk`](../crates/foxygnal-plugin-sdk). The
SDK's traits look like the engine's: `Block::work` gets typed input slices and output slices and
calls `consume` / `produce`.

```toml
# Cargo.toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
foxygnal-plugin-sdk = { path = "../foxygnal/crates/foxygnal-plugin-sdk" }

[profile.release]
lto = true
panic = "abort"
```

```rust
use foxygnal_plugin_sdk::*;

struct MyPlugin;

impl Plugin for MyPlugin {
    fn info() -> PluginInfo {
        PluginInfo {
            name: "mine".into(), // kinds become `mine:<kind>`
            version: env!("CARGO_PKG_VERSION").into(),
            blocks: vec![BlockDescriptor {
                kind: "scale".into(),
                label: "Scale".into(),
                category: "Math".into(),
                help: "Multiplies by a constant.".into(),
                inputs: vec![PortSpec::new("in", StreamType::F32)],
                outputs: vec![PortSpec::new("out", StreamType::F32)],
                params: vec![ParamSpec::float("k", "Factor", 1.0, "Scale factor.").live()],
                tags: TagMode::AllToAll,
            }],
        }
    }

    fn create(kind: &str, params: &Params) -> Result<Created, String> {
        match kind {
            "scale" => Ok(Created {
                block: Box::new(Scale(params.f64("k")? as f32)),
                inputs: vec![StreamType::F32],
                outputs: vec![StreamType::F32],
            }),
            _ => Err(format!("unknown kind `{kind}`")),
        }
    }
}

export_plugin!(MyPlugin);

struct Scale(f32);

impl Block for Scale {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, String> {
        let (inp, out) = io.in_out::<f32, f32>(0, 0);
        let n = inp.len().min(out.len());
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        for (o, x) in out[..n].iter_mut().zip(&inp[..n]) {
            *o = x * self.0;
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(WorkStatus::Ok)
    }

    fn set_param(&mut self, name: &str, value: &ParamValue) -> Result<(), String> {
        match (name, value.as_f64()) {
            ("k", Some(k)) => {
                self.0 = k as f32;
                Ok(())
            }
            _ => Err(format!("parameter `{name}` cannot change while running")),
        }
    }
}
```

[`plugins/example`](../plugins/example/src/lib.rs) is a complete plugin: a gain and a moving
average, each for `f32` and `c32` with a `type` parameter and a live parameter.
[`plugins/misbehave`](../plugins/misbehave/src/lib.rs) is the test plugin (blocks that trap,
spin, eat memory, tag manually, and change the sample rate in `configure`).

The rules are the engine's ([ARCHITECTURE.md](ARCHITECTURE.md), "Blocks"):

- `work` processes what it can and returns `Ok`, `Blocked` (no input or no room), `Wait(ns)`
  (nothing to do for a while; never sleep) or `Done`. It must never loop forever: the engine
  interrupts it (see [Sandbox](#sandbox)).
- `configure(inputs)` returns each output's `StreamMeta` (sample rate, centre frequency), or an
  empty list to keep the default (each field from the first input that has it). No side
  effects: the server calls `create` and `configure` on every edit to label the wires.
- `set_param` receives parameters flagged `live`, between `work` calls, already checked against
  the descriptor's kind and range. Any other parameter change rebuilds the block on the next run.
- Ports whose type depends on parameters are declared with `PortSpec::any` and get their type
  from `Created`. The usual `type` parameter is `ParamSpec::stream_type(&[...], default)`.
- `TagMode::AllToAll` (the default for most blocks): the engine passes every tag on a consumed
  item to every output, scaled by produced/consumed, and the plugin sees no tags (so it costs
  nothing). `TagMode::Manual`: `io.tags()` lists the tags on the readable input items and
  `io.add_tag(port, index, key, value)` tags output items.
- `foxygnal_plugin_sdk::log(msg)` writes to the engine's log (at most 10 lines a second per
  block).
- Parameter kinds: float (range, unit), int (range), bool, choice (enum), text. Paths and
  device strings are not offered: plugins can't open files or devices.

## Building

Plugins build for `wasm32-unknown-unknown` with plain cargo. No other tools are needed: the
engine turns the core module into a component when it loads it (with `wit-component`, from the
WIT metadata that the SDK embeds).

```sh
rustup target add wasm32-unknown-unknown     # once
cargo build --release --target wasm32-unknown-unknown
# → target/wasm32-unknown-unknown/release/my_plugin.wasm
```

Recommended in the plugin's `.cargo/config.toml` (as in [`plugins/`](../plugins/.cargo/config.toml)):
`rustflags = ["-C", "target-feature=+simd128"]` for `wasm32-unknown-unknown`. wasmtime runs
128-bit SIMD on every platform foxygnal supports, and LLVM vectorizes simple loops with it.

A ready component (from `cargo component` or `wasm-tools component new`) works too, as long as
it imports nothing but `foxygnal:block/host`. **Don't build for `wasm32-wasip1/2`**: those
import WASI (files, clocks, …), which the engine doesn't provide, and the plugin fails to load
with a message saying so.

The plugins in this repository build with `scripts/build-plugins.sh`, which also refreshes the
prebuilt copies in `crates/foxygnal-plugin/tests/fixtures/` that the host's tests and benchmark
load (so `cargo test` needs no wasm toolchain). `plugins/` is a cargo workspace of its own,
excluded from the main one.

## Installing

Copy the `.wasm` into a plugin directory. At start-up, foxygnal loads every `*.wasm` (not
recursively) from:

1. `plugins/` in the project directory (`foxygnal serve --project-dir`, default `.`), or next to
   the graph file for `foxygnal run` / `foxygnal bench`;
2. `$XDG_DATA_HOME/foxygnal/plugins`, else `~/.local/share/foxygnal/plugins`.

`--plugins DIR` (repeatable, any subcommand) replaces both. A plugin that fails to load is
logged and skipped; the rest of the engine starts normally. `foxygnal blocks` lists what
loaded. Plugins load once, at start-up: restart the server after changing one.

Block kinds are **`<plugin name>:<kind>`** (`example:moving_average`): the name comes from the
plugin's `info` (lowercase letters, digits, `_`, `-`; `graph` is reserved for graph files, whose
kinds are `graph:<file>`), not from the file name, so renaming a file doesn't break graphs. Two
plugins with the same name: the second is refused. A plugin can't replace a built-in block.

In the palette, plugin blocks sit in the category their descriptor names, with a small badge
showing the plugin's name; searching "plugin" lists them all. `GET /api/blocks` serves
`"plugin": "<name>"` on their descriptors.

## WIT reference

[`wit/block.wit`](../wit/block.wit) is the contract; the SDK wraps it. In short:

```wit
interface host {                                  // imported by the plugin
    emit: func(port: u32, data: list<u8>);        // output items (raw LE bytes)
    consume: func(port: u32, items: u32);
    add-tag: func(port: u32, index: u32, key: string, value: tag-value);
    log: func(message: string);
}
interface plugin {                                // exported by the plugin
    info: func() -> plugin-info;                  // name, version, block descriptors
    resource block {
        configure: func(inputs: list<stream-meta>) -> result<list<stream-meta>, string>;
        set-param: func(name: string, value: param-value) -> result<_, string>;
        work: func(inputs: list<list<u8>>, space: list<u32>, tags: list<input-tag>)
            -> result<work-status, string>;
    }
    create: func(kind: string, params: list<tuple<string, param-value>>) -> result<created, string>;
}
```

**How buffers cross the boundary.** Never per sample: one `work` call moves a batch of up to
8192 items per port.

- **In:** each input port's readable items go in as one `list<u8>` argument. wasmtime copies
  them from the engine's ring buffer straight into memory the plugin allocates for the call
  (one copy, no host allocation). The SDK views them as `&[f32]` / `&[Complex32]` / … in place.
- **Out:** the block writes into the SDK's per-port scratch buffers (allocated once, reused),
  and the SDK hands each port's produced bytes to the imported `emit`. The host reads them
  from the plugin's memory in place (`WasmList::as_le_slice`) and copies them straight into the
  output ring buffer (one copy, no host allocation).
- Consumption goes through `consume`; `work` returns only a status. Returning the outputs (and
  consumed counts) as `work`'s result was considered and rejected: wasmtime frees a call's
  returned lists as the call ends, so the host would have to lift them into a fresh `Vec` on
  every call.

Items are raw little-endian bytes of the port's type: `f32`, `c32` (interleaved re, im),
`i16`, `u8`. The host checks everything a plugin claims: at most the items it was given are
consumed, at most the room it was given is emitted, only whole items.

Host-side, a plugin block allocates nothing per `work` call (manual tag mode allocates a small
list when input tags are present). When every input is empty, `work` isn't called at all.

## Sandbox

- **No access to anything.** A plugin imports only the four `host` functions above. There is no
  WASI: no filesystem, network, environment, clock or random numbers. The engine is built
  without wasmtime's WASI support at all.
- **Time limit.** Every call into a plugin (`create`, `configure`, `set-param`, `work`) has a
  deadline of 250 ms, enforced with wasmtime's epoch interruption. A plugin that loops forever
  fails its block with "plugin work ran longer than 250 ms and was stopped (an endless loop?)",
  and the graph stops like for any block error, so a worker thread is never held hostage. The
  epoch thread ticks every 10 ms while a plugin call runs and parks (0 CPU) after a second
  without one.
- **Memory limit.** 64 MiB of linear memory per block instance. Growing past it fails the block
  with "plugin ran out of memory (limit 64 MiB)".
- **Crashes are block errors.** A panic, an out-of-bounds access or any other trap becomes an
  error on that block ("plugin crashed in work: …"), shown on the block in the UI, never a
  crash of the engine. The block's instance is not used again; the next run builds a fresh one.
- **Isolation.** Every block gets its own instance (own memory, own state), so blocks of the
  same plugin can't see each other.

`foxygnal_plugin::Options` sets the limits for embedders; the CLI uses the defaults.

## Performance

Measured with `cargo bench -p foxygnal-plugin` (criterion; `VectorSource → block →
null_sink`, 2²⁰ items run to the end of the stream, so the numbers include the source, the sink
and the scheduler), release build, 32-core x86-64 desktop:

| block | native | plugin | plugin / native |
|---|---|---|---|
| gain, f32 | 900 M items/s | 485 M items/s | 0.54 |
| gain, c32 | 415 M items/s | 330 M items/s | 0.79 |
| moving average (16), f32 | 490 M items/s | 186 M items/s | 0.38 |

What it costs:

- **Fixed overhead** per `work` call (entering wasm, lowering the arguments, the plugin's
  allocation of the input lists, two or three host calls): small next to a batch of 8192 items.
- **Two extra copies** per item (in and out). For a block as cheap as a gain, that is most of
  the work, yet still hundreds of millions of items a second.
- **Compute** runs at Cranelift speed: a scalar loop with a serial dependency (the moving
  average) is 2–3× slower than LLVM's native code. Enabling `simd128` helps loops that
  vectorize (+23% for the c32 gain).
- **Start-up**: compiling a plugin takes about 80 ms in a release build on the desktop (a
  ~60 KB plugin; plugins compile in parallel). Building a block instantiates the component and
  calls `create`.

For comparison, a 2.4 MS/s FM receiver needs a few million items a second per block, so a
plugin is fast enough for almost anything at SDR rates. **Go native** (a block in
`foxygnal-blocks`) for heavy per-sample DSP at high rates (long FIR filters, FFTs, resamplers),
where native SIMD kernels ([ARCHITECTURE.md](ARCHITECTURE.md), "DSP kernels") are worth several
times the speed, or when a block needs hardware, files or the network.

## The host crate and build cost

`foxygnal-plugin` (wasmtime 51 with Cranelift and the component model only: no WASI, no async,
no on-disk cache) depends on core, not on the blocks. The CLI uses it through the cargo feature
**`plugins`, on by default**; `cargo build -p foxygnal --no-default-features` leaves wasmtime
out (and `--plugins` then only warns). Cost, measured on the same 32-core desktop:

| | without | with plugins |
|---|---|---|
| clean `cargo build -p foxygnal` (dev) | 12 s | 32 s |
| clean `cargo build --release -p foxygnal` | 34 s | 134 s |
| release binary, stripped | 7.1 MB | 20.9 MB |

The release build is the expensive one (Cranelift under thin LTO with one codegen unit); dev
builds and incremental rebuilds are fine. It stays on by default because a binary that can't
load plugins defeats their purpose; builds for a small device that will never use plugins can
turn it off.

wasmtime needs Rust 1.97 or newer (the rest of the workspace builds with 1.85). The crate is a
workspace member, so `cargo build --workspace` compiles it whatever the feature says.
