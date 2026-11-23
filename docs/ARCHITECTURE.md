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
| `foxygnal-dsp` | DSP math shared by blocks and probes: windows, FFT power spectra (later filter design, resamplers) | — |
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

- **Stream metadata propagation (M3).** At build time each block receives upstream metadata
  (sample rate, centre frequency, …) and declares its output metadata. A decimate-by-4 block
  divides the rate by 4, and a throttle picks up the rate from upstream. Users stop typing the
  same sample rate everywhere, and the UI can label every wire with its rate.
- **Live parameters (M1, done).** Parameters flagged `live` (frequency, gain, cutoff…) can
  change while running. The runtime delivers them to `Block::set_param` through a per-block
  queue drained before `work`. Any other change marks the run *stale*: it applies on the next
  run. (Later: rebuild only the affected block.)
- **Tags (M3).** Sample-indexed key/value annotations (`rx_time`, `freq` after a retune,
  `burst_start`), carried alongside the ring buffer.

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
- The sample rate on axes is guessed from the nearest upstream block with a `sample_rate`
  parameter, or set per instrument. Stream metadata (M3) will make it exact.

## Server API

REST:

- `GET /api/health`: name and version
- `GET /api/blocks`: block descriptors
- `GET/PUT /api/graph`: current graph as `GraphSpec` JSON; `GET /api/graph.toml`
- `POST /api/toml/parse`, `POST /api/toml/render`: convert for upload/download
- `GET /api/examples`, `GET /api/examples/{name}`: bundled example graphs
- `GET /api/files`, `GET/PUT /api/files/{name}`: `.fox.toml` files in `--project-dir`
  (plain file names only). Relative `path` parameters also resolve against that directory.

WebSocket `/api/ws`, one per browser tab:

- **Text frames: JSON control messages** with `{"op": …}`. Client: `set_graph {graph, name}`
  (the whole graph; the server diffs it, pushes changed live params and marks other changes
  stale), `run`, `stop`, `probe_set {id, edge {from, to}, probe {kind, …options}, fps}` (the
  client picks `id`; sending it again changes the probe) and `probe_remove {id}`. Server:
  `hello` (full snapshot on connect or after lag), `graph {graph, name, version, from}`,
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
index). The format is plain, diff-friendly and hand-editable (see `examples/`). Planned:
hierarchical blocks (a graph used as a block, with exposed params and ports), and graph-level
variables (`$samp_rate`) referenced from params.

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
