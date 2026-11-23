# Architecture

## Process model

One binary, `foxygnal`, contains the DSP engine and a small web server. The browser is a thin
client: it edits the graph, controls runs and displays instruments. All DSP, including the
math behind scopes and spectra, runs in the engine. The browser only receives display-ready
frames of a few KB each.

```
┌──────────── browser ────────────┐        ┌──────────────── foxygnal ────────────────┐
│ Svelte UI: editor, inspector,   │  HTTP  │ axum server ── session ── Flowgraph       │
│ instruments (WebGL2 canvases)   │◄──────►│   /api/*        │          │ block threads│
│                                 │   WS   │   /api/ws  ◄────┴─ probes ─┘ ring buffers │
└─────────────────────────────────┘        └───────────────────────────────────────────┘
```

`foxygnal run graph.fox.toml` runs the same engine headless, with no server at all.

## Crates

| crate | role | depends on |
|---|---|---|
| `foxygnal-core` | `StreamType`/`Sample`, ring buffers, `Block` trait, `Registry`, `GraphSpec`, runtime | — |
| `foxygnal-blocks` | standard blocks + `registry()` | core |
| `foxygnal-dsp` | DSP math shared by blocks and probes: windows, FFT power spectra, FIR design and responses, polyphase resampler, rotator | — |
| `foxygnal-server` | REST + WebSocket API, probes and instruments, static UI | core, dsp |
| `foxygnal` | CLI | all |

Planned: `foxygnal-sdr` (hardware drivers behind cargo features, so `cargo build` needs no C
libraries by default).

## Stream types

`f32`, `c32` (complex f32 I/Q), `i16`, `u8` today. Every port has exactly one concrete type,
checked when a connection is made, with an error message that names both ends. Planned
additions: `ci16` / `ci8` (native SDR sample formats, so no conversion is needed at the
device), `f64`, and an untyped **message** port kind for packets and events.

## Blocks

```rust
pub trait Block: Send + 'static {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error>;
    fn start(&mut self) -> Result<(), Error> { Ok(()) }
    fn stop(&mut self) -> Result<(), Error> { Ok(()) }
}
```

`work` reads input slices, writes output slices, then calls `consume`/`produce`. It returns
`Ok` (progress), `Blocked` (cannot progress: wait for a neighbour), `Wait(duration)` (nothing
to do until roughly then, e.g. a throttle; the scheduler parks instead of the block sleeping)
or `Done`. Byte-level access
(`bytes()` / `bytes_mut()`) lets type-agnostic blocks such as throttle, head or file I/O be
written once.

Each block kind registers a `BlockDescriptor`: label, category, help text, ports and typed
parameter specs (float with unit, int, enum, bool, path…). The UI's palette and inspector
are generated **entirely** from descriptors (`GET /api/blocks`), so a new block needs no UI
code.

### Planned block API additions

- **Stream metadata (M3, done).** `StreamMeta { sample_rate, center_freq }` (both optional).
  After wiring and before `start`, the runtime calls `Block::configure(&mut Configure)` on
  every block in upstream-first order. `cx.inputs` holds each input's metadata, and
  `cx.outputs` starts as the merge of the inputs (each field from the first input that has
  it), so most blocks implement nothing. A source sets its rate (`cx.set_rate`), a
  decimate-by-4 block divides it, and a throttle at rate 0 takes it from upstream
  (`cx.input_rate(0)?` gives a readable error if nothing upstream sets one). Building and
  configuring must have **no side effects**: files and devices open in `start()`. The
  server builds and configures the graph on every edit and sends `meta {edges, error}`, which
  the UI uses for rate labels on wires and instrument axes.
- **Live parameters (M1, done).** Parameters flagged `live` (frequency, gain, cutoff…) can
  change while running. The runtime delivers them to `Block::set_param` through a per-block
  queue drained before `work`. Any other change marks the run *stale*: it applies on the next
  run. (Later: rebuild only the affected block.)
- **Tags (M3, done).** Sample-indexed key/value annotations (`rx_time`, `freq` after a retune,
  `burst_start`), carried alongside the ring buffer (`core/src/tag.rs`). Each read cursor has
  a small lock-free SPSC tag queue. A block tags an item with `OutputPort::add_tag(index, key,
  value)` (index into its `slice_mut`); values are bool/int/float/`Arc<str>`, so forwarding a
  tag never allocates. Items a block `produce`s are *staged* and published when `work`
  returns, after their tags, and before each `work` the runtime snapshots every input (items
  published so far plus all their tags). A block therefore always sees an item's tags together
  with the item: `InputPort::tags()` / `tags_in(n)`. By default the runtime passes every tag on
  a consumed input item to every output, scaling its offset by produced/consumed for that call
  (right for 1:1 blocks, decimators and interpolators). Blocks that reorder, delay or drop items
  return `TagPropagation::Manual` from `Block::tag_propagation` and forward tags themselves.
  A full queue drops new tags rather than block (`InputPort::dropped_tags`).

## Buffers

Each output port owns one lock-free **single-writer / multi-reader** ring
(`core/src/buffer.rs`). Every downstream connection gets its own read cursor, so fan-out
needs no copying. The writer can reuse space only after every live reader has consumed it.
When all readers are gone, the writer counts as *abandoned* and the block stops.

A **tap** (`Tap`) is a lossy view of a ring for probes and stats. It is not a reader: the
writer never waits for it. Before writing, the writer publishes how far it may write
(`reserved`); a tap copies what it wants, then checks `reserved` again and drops anything the
writer may have overwritten meanwhile (the seqlock pattern). `Running::monitor()` hands out
taps by output port, plus raw counters: items written and backlog per connection, and time
spent in `work` per block.

M0 limitation: slices are contiguous only up to the wrap point. **M5** replaces the backing
store with a *double-mapped* buffer (a memfd mapped twice back to back, the same trick as GNU
Radio's vmcircbuf), so every slice is contiguous. The `Reader`/`Writer` API stays the same.

## Scheduler

**M0: one OS thread per block.** A blocked block parks. Producing data unparks its readers and
consuming data unparks its writer. A 50 ms park timeout is the safety net. End of stream works
like this: a block whose input is exhausted (writer done and drained) and which returns
`Blocked` finishes, and dropping its ports propagates the end of stream downstream.

**M5: pooled scheduler.** N worker threads (default: number of cores) run blocks as tasks. A
block becomes runnable when its inputs have data and its outputs have space. Blocks that call
blocking APIs (hardware, audio, files) can request a dedicated thread. Options: core pinning,
larger quanta for throughput or smaller ones for latency. The thread-per-block model stays
available as a fallback.

## Probes and instruments (M2, done)

A **probe** attaches to any wire at runtime, with no rebuild and no restart
(`server/src/probe/`):

- A probe reads a tap on the wire's output, so it **never** back-pressures the graph. If it
  falls behind (or the tap couldn't keep up), it skips ahead and starts a new contiguous
  stretch. Probes on several wires from the same output read the same data.
- One probe thread per session serves every probe of every tab. It polls taps about 60 times
  a second (so streams stay contiguous up to roughly 1.5 MS/s with the default buffer) and
  sends each probe's frame at its own rate (default 30 fps, the meter 15).
  - **Scope**: trigger auto / normal / single / off, rising/falling, level with a small
    automatic hysteresis, on I, Q, |x| or ∠x, and a pre-trigger fraction. Window length in
    samples (the UI turns time/div into it). Min/max-decimated to the canvas width when the
    window is wider than that. Cursors are drawn in the UI.
  - **Spectrum**: FFT size, window, exponential averaging, peak hold, 50% overlap, bins
    reduced to the canvas width by keeping each group's maximum. dB normalized so a
    full-scale tone reads 0 dB (one-sided for real streams). The **waterfall** is a view of
    the same frames: the UI adds one row per frame.
  - **Constellation**: the latest N points (c32 wires only). Persistence is drawn in the UI.
  - **Meter**: RMS, peak and DC over everything since the last frame.
- **Stats** are broadcast twice a second while running: items/s and buffer fill per wire,
  and the share of a core each block spends in `work`. The UI shows them in the wire popover
  and as the *Load* overlay (heat colours on blocks and wires).
- The sample rate (and centre frequency, for absolute spectrum axes) comes from stream
  metadata, or can be set per instrument.

## Server API

REST:

- `GET /api/health`: name and version
- `GET /api/blocks`: block descriptors, plus graph files usable as blocks (see "Graph files")
- A block descriptor with `"response": true` belongs to a block
  registered with `Registry::register_with_response` (filters, resamplers)
- `POST /api/blocks/{kind}/response` `{params, sample_rate}` → `{freqs, db, taps}`: the magnitude
  response the block would have at that input rate (400 with a readable message for invalid
  params). The inspector draws it for any block whose descriptor says `response`
- `GET/PUT /api/graph`: current graph as `GraphSpec` JSON; `GET /api/graph.toml`
- `POST /api/toml/parse`, `POST /api/toml/render`: convert for upload/download
- `GET /api/examples`, `GET /api/examples/{name}`: bundled example graphs
- `GET /api/files`, `GET/PUT /api/files/{name}`: `.fox.toml` files in `--project-dir`
  (plain file names only). Relative `path` parameters and `graph:` blocks also resolve
  against that directory. `GET` falls back to the bundled sub-graph library.

WebSocket `/api/ws`, one per browser tab:

- **Text frames: JSON control messages** with `{"op": …}`. Client: `set_graph {graph, name}`
  (the whole graph; the server diffs it, pushes changed live params and marks other changes
  stale), `run`, `stop`, `probe_set {id, edge {from, to}, probe {kind, …options}, fps}` (the
  client picks `id`; sending it again changes the probe) and `probe_remove {id}`. Server:
  `hello` (full snapshot on connect or after lag, including `meta`),
  `graph {graph, name, version, from}`, `meta {edges [{from, to, sample_rate?, center_freq?}],
  error?}` after every graph change,
  `status {state, started_at, stale, errors[]}`, where each error names a `block`, an
  `edge {from, to}`, or neither, `stats {blocks[], edges[]}`, and, to the probe's tab only,
  `probe {id, error}` when a probe can't show anything (wire not running, wrong type) or
  recovers. Probes belong to the connection; the UI sets them up again after reconnecting.
- **Binary frames: instrument data**, only to the tab that owns the probe. Little-endian,
  every section 4-byte aligned so the browser views `f32` arrays in place. Header:
  `u32 probe_id, u32 seq, u8 kind, u8[3]`. Payloads:
  - scope (1): `u32 span, i32 trigger_index (-1: none), u8 flags (1 triggered, 2 min/max),
    u8 channels, u8[2], u32 points, u8[4] channel ids (I, Q, |x|, ∠x)`, then per channel
    `f32[points]`, or `f32[2·points]` (min, max) pairs.
  - spectrum (2): `u32 fft_size, u32 bins, u8 flags (1 centred/complex, 2 peak), u8[3]`,
    then `f32[bins]` dB and, with peak hold, `f32[bins]` peak dB.
  - constellation (3): `u32 n`, then `f32[2n]` interleaved I, Q.
  - meter (4): `u32 count, u8 flags (1 complex), u8[3], f32 rms, f32 peak, f32 mean_i,
    f32 mean_q`.

  A tab that can't keep up drops frames (bounded queue) rather than piling them up.

Several tabs can share one engine session: graph edits are broadcast and the last write wins.
Collaborative editing is a non-goal for now.

## Graph files

`*.fox.toml`: `[[block]]` entries (`id`, `kind`, `params`, and editor-only `ui` such as
position) plus `[[connect]]` entries (`from = "block.port"`, where the port is a name or an
index). The format is plain, diff-friendly and hand-editable (see `examples/`).

### Variables and expressions (M3, done)

```toml
[vars]
samp_rate = "2.4M"          # a number, or an expression
cutoff = "$samp_rate / 10"  # may use other vars, in any order

[[block]]
id = "lp"
kind = "gain"
params = { gain = "$cutoff * 2" }
```

Float and int params may be strings holding an expression (`core/src/expr.rs`): numbers with
an optional SI prefix and unit word as the UI accepts them (`2.4M`, `100 kHz`, `-30 dB`),
`$name`, `+ - * /`, unary minus and parentheses; nothing else. Strings in other param kinds
are left alone. Int params must come out whole. Loops between variables are an error that
shows the chain. The file keeps the text as written; resolving happens before building.

### Hierarchical blocks (M3, done)

Any graph file can be used as a block. It declares its ports with `[export]`; its `[vars]`
become the block's parameters:

```toml
# fm-demod.fox.toml
[vars]
quad_rate = 480000
[export]
label = "FM demod"          # optional palette label (default: the file name)
help = "Narrowband FM."     # optional
[[export.input]]
name = "in"
to = "lp.in"                # or a list: to = ["lp.in", "agc.in"]
[[export.output]]
name = "out"
from = "demod.out"
```

The parent uses it as `kind = "graph:fm-demod.fox.toml"` with params overriding the child's
vars (evaluated in the parent's scope, so `quad_rate = "$samp_rate / 5"` works). Variables
whose value refers to other variables are *derived*: they aren't offered as params, though a
hand-written override still applies. Params the child doesn't have are ignored, as for any
block. Sub-graph names are plain file names, resolved in the project directory (the
directory of the graph for `foxygnal run`); a few bundled sub-graphs
(`server/src/graphs.rs` `LIBRARY`, e.g. `tone-and-noise.fox.toml` used by the
`hier-tones` example) are found when the project has no file of that name.

`core::resolve::resolve(spec, registry, loader)` turns a graph into a flat one: vars and
expressions evaluated, every hierarchical block replaced by its blocks renamed
`parent_id/inner_id` (so `/` is not allowed in block ids), its exported ports wired through.
The runtime never sees hierarchy (`Flowgraph::from_spec` resolves vars without a loader,
`from_spec_with` takes a `GraphLoader`, `from_resolved` builds a resolved graph). A graph
that contains itself, directly or not, is an error naming the chain (and nesting stops at
16). Errors inside name the inner block (`block \`fm/lp\`, parameter …`). Port types come
from the inner blocks; stream metadata flows through like anywhere else.

Each resolved connection remembers the top-level connection it came from, so the server
reports everything the way the user drew it (`server/src/graphs.rs` `EdgeMap`): metadata
and stats of the wire `fm.out → sink.in` (not `fm/demod.out → sink.in`), wires inside a
hierarchical block left out, its blocks' CPU summed into it, probes on its wires bound to
the inner output that feeds them, and errors pinned to it with the inner detail in front
(`lp: parameter …`). Live params compare *resolved* graphs, so changing a variable or a
hierarchical block's param live-applies wherever it reaches only live params, and marks the
run stale otherwise.

`GET /api/blocks` also lists every graph file with exported ports as a block in category
*Graphs* (`resolve::describe`: ports typed from the inner blocks; one float param per
non-derived variable, `live` when changing it only changes live params inside). Files that
don't resolve are left out. Not done yet: watching inner wires of a hierarchical block,
units/help per variable, and string (path) variables.

## Security

A local server that can write files (file sink) and later transmit RF needs care:

- It binds `127.0.0.1` by default. Non-loopback binds need `--allow-remote`.
- **Host and Origin checks** on every request and WebSocket upgrade. These block DNS
  rebinding attacks and cross-site requests from pages open in the same browser. This matters
  even on localhost.
- For `--allow-remote`: a random access token printed at startup, like Jupyter. Opening
  `/?token=…` sets an `HttpOnly; SameSite=Strict` cookie; API clients can send
  `Authorization: Bearer …`.
  Recommend a reverse proxy with TLS.
- (M4) **Transmit is opt-in per session.** Any TX block requires an explicit "arm TX" action,
  and the UI shows a persistent red banner while armed. The user is responsible for staying
  legal.

## UI stack

Svelte 5 + Vite + TypeScript, and **Svelte Flow** (`@xyflow/svelte`) for the node editor.
Instruments are hand-written canvases, not a charting library: Canvas2D for traces (scope,
spectrum, constellation), and WebGL2 for the waterfall, whose history lives in a float
texture used as a ring. A new row is one small upload, and changing the dB range re-colours
the whole history. Instruments live in a resizable dock under the canvas, are remembered per
graph file (browser storage), and can pop out into their own window (`?popout=<id>`), which
opens its own engine connection and syncs settings back over a `BroadcastChannel`. In release builds the built UI is embedded in the binary with `rust-embed`
(`cargo build --release --features embed-ui`), so you ship a single file. Without the feature
it is served from `ui/dist` (or `--ui-dir`).

## Prior art and positioning

- **GNU Radio / GRC**: the reference. Huge block library, but heavy installs, Python/C++
  friction and a dated desktop UI. foxygnal reads its raw file formats, and GRC import is an
  M6 maybe.
- **FutureSDR**: a Rust SDR runtime (async, WASM, GPU). Strong engine work. foxygnal focuses
  on the end-user editing and instrument experience. Worth tracking and borrowing ideas from.
- **SDR++ / SDRangel / GQRX**: receiver apps with good UX but fixed pipelines.
  foxygnal aims for their polish with a free-form graph.
