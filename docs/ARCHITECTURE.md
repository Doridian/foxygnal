# Architecture

## Process model

One binary, `foxygnal`, contains the DSP engine and a small web server. The browser is a thin
client: it edits the graph, controls runs and displays instruments. All DSP, including the
math behind scopes and spectra, runs in the engine. The browser only receives display-ready
frames of a few KB each.

```
┌──────────── browser ────────────┐        ┌──────────────── foxygnal ────────────────┐
│ Svelte UI: editor, inspector,   │  HTTP  │ axum server ── session ── Flowgraph       │
│ instruments (WebGL2 canvases)   │◄──────►│   /api/*        │          │ worker pool  │
│                                 │   WS   │   /api/ws  ◄────┴─ probes ─┘ ring buffers │
└─────────────────────────────────┘        └───────────────────────────────────────────┘
```

`foxygnal run graph.fox.toml` runs the same engine headless, with no server at all.

## Crates

| crate | role | depends on |
|---|---|---|
| `foxygnal-core` | `StreamType`/`Sample`, ring buffers, `Block` trait, `Registry`, `GraphSpec`, runtime | — |
| `foxygnal-blocks` | standard blocks + `registry()` | core, dsp, sdr |
| `foxygnal-dsp` | DSP math shared by blocks and probes: windows, FFT power spectra, FIR design and responses, polyphase resampler, rotator, SIMD kernels | — |
| `foxygnal-sdr` | SDR device API, device strings, drivers and discovery, the TX arm switch | — |
| `foxygnal-server` | REST + WebSocket API, probes and instruments, static UI | core, dsp, sdr |
| `foxygnal` | CLI | all |

`foxygnal-sdr` holds the device trait, discovery and the drivers. Drivers that need a C library
(librtlsdr, libhackrf, SoapySDR) load it at run time, so `cargo build` needs no C libraries and no
cargo feature; see [HARDWARE.md](HARDWARE.md).

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
    fn dedicated_thread(&self) -> bool { false } // see "Scheduler"
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

## Browser audio

`browser_audio` is a sink that consumes its input (paced at the stream rate unless hardware
upstream sets the pace). While a graph with one runs, the UI opens an `audio` probe on the wire
feeding it and plays the frames with WebAudio: each frame becomes an `AudioBuffer` at the
stream's rate (the browser resamples to the device) scheduled right after the previous one,
about 150 ms ahead of the playhead. If playback falls behind or the frame says samples were
lost, it restarts at that lead; if it runs more than 600 ms ahead (the engine's clock faster
than the sound card's), frames are skipped. Browsers only start audio from a user gesture,
so the Run click unlocks it; otherwise a "Click to enable audio" button appears. Nothing on
the engine's machine needs an audio library, and it works over a remote connection.

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

**Double mapping (M5).** The ring's storage is mapped twice, back to back, in virtual memory
(`core/src/vmem.rs`; the same trick as GNU Radio's vmcircbuf): a `memfd` on Linux, an unlinked
`shm_open` object on other Unix systems, mapped `MAP_SHARED | MAP_FIXED` over both halves of a
`PROT_NONE` reservation. Byte `i` and byte `i + size` are the same memory, so a slice that
starts near the end simply runs on into the second copy: `Reader::slice` returns *every*
available item as one slice, and a block that needs N items at a time (a decimator, an
interleaved-pair converter) can never stall at the wrap point. The capacity is rounded up to
whole pages (item sizes 1/2/4/8 divide any page size; 32768 items is the default). A
`Writer::slice_mut` hands out at most **half** the ring at once: a tap can't trust slots the
writer may be writing into, and this keeps the newest half visible to probes.

Where the mapping is unavailable (Windows, for now, or if the OS refuses), the ring falls back
at run time to one plain allocation whose slices stop at the wrap point
(`BufferKind::Plain`; `Flowgraph::with_buffer_kind` forces it, which tests use to keep both
paths covered). Note that RSS counts a double-mapped page twice although it uses one page of
memory.

## Scheduler

**A pool of worker threads (M5)** runs the blocks (`core/src/sched.rs`). Each block is a task;
`Flowgraph::with_workers(n)` sets the pool size (default: one worker per core, never more than
there are pooled blocks). Workers are named `fox:worker-N`.

- **Waking.** The ring buffers wake through a `std::task::Waker` registered per read cursor
  and per writer: publishing items wakes the readers, consuming (or a reader leaving) wakes the
  writer, finishing wakes the readers. A pooled block's waker schedules its task; a dedicated
  block's waker unparks its thread. Parameter changes and `stop()` wake the block too.
- **Task states.** `IDLE → SCHEDULED → RUNNING → IDLE`, plus `NOTIFIED`: a wake that arrives
  while the block runs marks it, and the worker queues it again instead of letting it go
  idle, so no wake-up is lost. Only waking an idle task queues it, so a task is queued at most
  once and a block never runs on two workers at once (the job sits behind a lock that is
  never contended; `try_lock` checks the invariant). The wake path is one compare-and-swap,
  plus a lock-free queue push when the task was idle.
- **Queues.** Each worker has a FIFO queue (crossbeam-deque). A wake from a worker thread
  goes to that worker's queue (the data the downstream block needs is hot in its cache), other
  wakes to a global queue. Idle workers steal from the global queue and each other; every 31
  tasks a worker looks at the global queue first, so blocks that keep waking each other can't
  starve the rest. A worker out of work searches briefly, then sleeps; a push wakes a sleeper
  only if nobody is searching.
- **Turns.** A block runs until `work` returns something other than `Ok`, or for at most 32
  calls or 2 ms, then goes to the back of the queue.
- **Timers.** `WorkStatus::Wait(d)` arms a timer on the pool's timer thread (`fox:timer`, a
  binary heap); when it fires the block is scheduled. A wake before then runs it early, and
  every run invalidates older timers.
- **Dedicated threads.** A block whose `Block::dedicated_thread()` returns true gets its own
  thread (`fox:<block id>`) that parks while idle (no timeout). `start`, `work`, `set_param`
  and `stop` all run there, so `std::thread::current()` is a valid way to wake it: the SDR and
  sound card blocks use this, as their driver threads and device callbacks unpark the thread
  that ran `start`. Pooled blocks are called by whichever worker runs them (one call at a
  time). File blocks stay pooled: a slow disk occupies one worker, not the graph (a FIFO that
  never delivers would hold its worker until the graph stops).
- **End of stream** is unchanged: a block whose input is exhausted (writer done and drained)
  and which returns `Blocked` finishes, as does one whose readers all left, and dropping its
  ports propagates it. A block returning an error or panicking finishes with an error naming
  it and stops the graph (every block is woken to notice).
- `Flowgraph::with_thread_per_block()` keeps the M0 model (every block dedicated) for
  comparison and as a fallback.

**Idle CPU (M5).** Nothing polls: idle workers and dedicated threads park without a timeout
until a wake or a timer. The throttle passes 2 ms of stream at a time and sleeps until the
next batch is due (passing each item as soon as it was due made it ping-pong with its
neighbours, each of which wakes it). Measured on a 32-core x86-64 desktop (release build,
process CPU from `getrusage` over 3 s; before = thread-per-block with 50 ms polling):

| graph | before | after |
|---|---|---|
| 48 kS/s tone → throttle → null sink | 1.7% of a core | ≈1.1% |
| 1 MS/s c32 tone → throttle → head → null sink | 84% | ≈3% |
| 2.4 MS/s tone → throttle → null sink | 120–170% | ≈5% |
| source that never produces → 5 gains → null sink | 0.05% | 0.00% |
| sim SDR (2.4 MS/s) → FM receiver → null sink | 6.5% | 6.4% |

Most of what is left in the throttled graphs is generating the tone. `idle_cpu.rs` in the
blocks' tests asserts that the 48 kS/s graph stays under 5% of a core and a stalled graph
under 1%.

## DSP kernels (M5)

The inner loops live in `foxygnal-dsp/src/kernels.rs`: FIR dot products (real taps × real or
complex samples), the rotator (complex multiply by a running phasor), complex multiply, window
multiply, |x|², dB, magnitude, phase, the FM discriminator and u8/i16 → f32 conversion.

- **One source, compiled per instruction set.** Each kernel body is plain Rust shaped so LLVM
  vectorizes it: fixed-width accumulator arrays, `chunks_exact`, no branches in the loop body
  (selects instead), FMA only where the target has it. A macro compiles the bodies into a
  *baseline* set (SSE2 on x86_64, NEON on aarch64) and, on x86_64, an *avx2* set: the same code
  inside `#[target_feature(enable = "avx2,fma")]` functions. The one exception is the AVX2 dot
  product, written with intrinsics because LLVM builds the portable version from 128-bit
  vectors (it was half the speed).
- **Dispatch once.** `Kernels` is a table of function pointers to one set. `kernels()` picks
  the best set the CPU supports on first use (`is_x86_feature_detected!`) and caches it; blocks
  and `Resampler`/`Rotator`/`PowerSpectrum` copy the pointers they need when they are built.
  The hot path is an indirect call, with no feature checks. `FOXYGNAL_SIMD=baseline` forces the
  baseline set (for comparisons); `Kernels::all()` lists every set the CPU can run, which the
  tests use to check each one against scalar code.
- **Data layout.** `Resampler` stores each polyphase branch's taps padded with zeros to a whole
  number of 8-float vectors, and for complex samples each tap twice (`h0 h0 h1 h1 …`), so a
  complex dot product is a plain float dot product over the interleaved samples with even lanes
  summing to the real part and odd lanes to the imaginary part. Its delay line stores every
  sample twice so the window is always one contiguous slice.
- **Rotator.** Eight phasors one step apart advance by `step⁸` per group of eight samples
  (`step⁰…step⁸` from f64, so the lanes don't drift) and are renormalized every 512 samples.
  `freq_xlating_fir` mixes a chunk of exactly `Resampler::inputs_for(room)` samples, then
  filters it, so the mixer's phase stays in step with what the filter consumes.
- **Fast atan2** (phase, FM discriminator): octant reduction plus the Abramowitz & Stegun
  4.4.49 polynomial, branch-free; worst error 3e-7 rad (measured), i.e. about −130 dB of full
  deviation at the FM demodulator's output.
- Results differ from a sequential loop only by rounding (sums are reassociated; FMA rounds
  once).

Benchmarks and numbers: [BENCHMARKS.md](BENCHMARKS.md).

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

## Hardware (M4)

`foxygnal-sdr` is the device layer; blocks never talk to hardware directly.

- **Drivers** (`Driver`: `name`, `label`, `discover`, `open`) are collected in a `Drivers`
  set. `default_drivers()` lists them in discovery order: hardware first, then network
  sources, then the simulator. `foxygnal_sdr::drivers()` is the process-wide instance built
  on first use, shared by the blocks (to open) and the server (to discover), so a driver that
  loads a C library (`dlopen`) does so once. A missing library only means that driver
  reports an error in discovery (no C library is needed to build).
- **Device strings** (`args.rs`): `driver:key=value,…`, e.g. `rtlsdr:serial=00000001`,
  `rtltcp:host=10.0.0.5,port=1234`, `soapy:driver=plutosdr,uri=ip:192.168.2.1`, `sim:`.
  Empty opens the first receiver discovery finds, real devices before the simulator.
  Discovery returns the exact string that reopens each device.
- **Capabilities** (frequency ranges, sample rates, gain range, AGC, antennas) are checked by
  the blocks before streaming, with messages in human units ("frequency 2 GHz is out of
  range; tunable: 24 MHz–1.766 GHz").
- **Threading.** A driver moves samples on its own thread into a lock-free SPSC ring
  (`ring.rs`) and unparks the block's thread, which it registered with `set_waker` in
  `start()` (SDR blocks ask for a dedicated thread, see "Scheduler"). `work()` only copies what is in the ring and returns `Blocked` when it's empty,
  so a device never blocks the scheduler. When the graph falls behind, the driver drops whole
  chunks and counts them (`overflows`). Control calls (retune, gain) may take milliseconds and
  run from `set_param`, between `work` calls. TX is the mirror image: `work()` writes what
  fits into the ring, the driver thread drains it at the device's pace and unparks the block
  when space frees up.
- **Blocks** (`blocks/src/sdr.rs`, category *Hardware*): `sdr_source` (c32 out) and
  `sdr_sink` (c32 in, TX). Building and configuring open nothing; the source states its rate
  and centre frequency as stream metadata, and the device opens in `start()`. Live
  `frequency`/`gain`/`agc`/`ppm` changes retune the running stream and tag the next item
  (`rx_freq` float Hz, `rx_gain` float dB, NaN for AGC); dropped samples tag the next item
  `overflow` (int, the number dropped). Errors name the device ("Simulated FM band (sim:):
  …").
- The `device` parameter kind (`ParamKind::Device`) makes the inspector offer discovered
  devices; any string can be typed (network receivers can't always be discovered).
- **Simulator** (`sim.rs`, device `sim:`, always present): a simulated RTL-SDR (24–1766 MHz,
  0.25–3.2 MS/s, gain 0–49.6 dB) that receives six FM broadcast stations (88.5, 94.1, 99.1,
  100.3, 100.9 and 104.7 MHz, 75 kHz deviation, each with its own tone pattern) plus noise,
  paced in real time by a driver thread. Retuning moves stations in and out of the passband;
  gain scales everything and clips like an ADC. Options: `stations=88.5;94.1` (MHz),
  `noise=0.01`. It also accepts a TX stream and discards the samples at the configured rate,
  so the TX path is testable without a transmitter. Integer phase accumulators and a sine
  table keep it cheap (2.4 MS/s with three stations in view uses a few percent of a core).

### TX arming

Transmitting must never happen by accident. Three independent gates:

1. **Marked blocks.** A block that transmits is registered with `Registry::register_tx`; the
   catalog serves `"tx": true` on its descriptor (`CatalogEntry`), and `Registry::is_tx`
   answers for a kind. Checks run on the *resolved* graph, so a TX block inside a
   hierarchical block counts too.
2. **The runner refuses.** The server refuses to start a graph with TX blocks unless it was
   started with `foxygnal serve --allow-tx` (off by default) **and** the run request arms TX
   (`{"op": "run", "arm_tx": true}`). Either refusal is an error pinned to the TX block that
   says how to proceed. `foxygnal run` needs `--arm-tx`.
3. **The block refuses.** `foxygnal_sdr::arm_tx()` returns a `TxArm` guard; while one lives,
   `foxygnal_sdr::tx_armed()` is true. The server takes the guard just before starting an
   armed run and keeps it in the run, dropping it when the run ends for any reason (stop,
   end of stream, error), after every block thread has finished; the CLI holds it for the
   run. `sdr_sink::start()` checks `tx_armed()` and fails otherwise. The switch is
   process-wide, which matches one session per server and one graph per CLI process.

Arming lasts for one run: the next Run has to arm again. The UI asks before every run of a
graph that transmits: a dialog lists each TX block's device, frequency, rate and gain (from
`meta.tx`, so variables are resolved and a rate of 0 shows the upstream rate), reminds that
transmitting needs a licence and must be legal, and offers **Arm TX and run** (never the
default focus; disabled, with an explanation, when the server lacks `--allow-tx`). While
`status.tx_armed` is set, the top bar shows a red **TX** indicator in every tab.

## Server API

REST:

- `GET /api/health`: name and version
- `GET /api/blocks`: block descriptors, plus graph files usable as blocks (see "Graph files")
- A block descriptor with `"response": true` belongs to a block
  registered with `Registry::register_with_response` (filters, resamplers)
- A block descriptor with `"tx": true` transmits (see "TX arming")
- `GET /api/devices`: per-driver discovery, `[{driver, label, devices [{driver, args, label,
  serial, rx, tx}], error}]`. Each driver searches on a blocking thread with a 4 s timeout,
  so a slow driver can't hang the request; a driver still busy from an earlier timed-out
  search isn't asked again until it returns
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
  stale), `run {arm_tx?}` (`arm_tx: true` arms TX for this run, see "TX arming"), `stop`,
  `probe_set {id, edge {from, to}, probe {kind, …options}, fps}` (the
  client picks `id`; sending it again changes the probe) and `probe_remove {id}`. Server:
  `hello` (full snapshot on connect or after lag, including `meta`, and `allow_tx`: the
  server was started with `--allow-tx`),
  `graph {graph, name, version, from}`, `meta {edges [{from, to, sample_rate?, center_freq?}],
  error?, tx?}` after every graph change (`tx`: blocks that would transmit, `[{block, kind,
  device?, frequency?, sample_rate?, gain?}]`),
  `status {state, started_at, stale, errors[], tx_armed?}`, where each error names a `block`, an
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
  - audio (5): `u32 n, u8 flags (1 gap: samples were lost before this frame), u8[3]`, then
    `f32[n]`: every sample since the previous frame (real wires only). The UI plays these for
    `browser_audio` blocks (see "Browser audio").

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
- **Transmit is opt-in twice**: per server (`--allow-tx`) and per run (arm TX, confirmed in
  a dialog); TX blocks also refuse to start unless armed, and the UI shows a red TX indicator
  while armed. See "TX arming". The user is responsible for staying legal.

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
