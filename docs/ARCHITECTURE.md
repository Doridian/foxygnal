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
| `foxygnal-grc` | GNU Radio `.grc` import (see [GRC_IMPORT.md](GRC_IMPORT.md)) | core |
| `foxygnal-server` | REST + WebSocket API, probes and instruments, static UI | core, dsp, sdr, grc |
| `foxygnal-plugin` | block plugins: WebAssembly components run with wasmtime (see "Plugins") | core |
| `foxygnal-plugin-sdk` | guest side: write plugins in Rust (built for `wasm32-unknown-unknown`) | — |
| `foxygnal-mpeg2` | MPEG-1/2 video decoder, built for `wasm32-unknown-unknown` for the browser's Video instrument (see "Digital video") | — |
| `foxygnal-mp2` | MPEG audio Layer I/II (MP2) decoder, built for `wasm32-unknown-unknown` for the same instrument | — |
| `foxygnal` | CLI | all (plugin: cargo feature `plugins`, default on) |

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

## Control inputs and constant inputs (M8)

Settings and streams can trade places: any live number setting can be driven by a wire, and
any stream input can be a constant. Both are generic (no block code), so an `add` can add a
constant, and a peak finder can steer a channel filter (`examples/fm-autotune.fox.toml`).

**Settings as inputs.** A connection to port `@name` drives the block's parameter `name`:

```toml
[[connect]]
from = "peak.out"
to = "channel.@offset"   # freq_xlating_fir's live `offset`, set by an f32 stream
```

- Only parameters that are live and a float, int or bool can be wired
  (`ParamSpec::wireable`); anything else, an unknown parameter, or a stream that isn't f32
  is an `Error::Edge` naming both endpoints (insert a converter for other types; the editor
  offers one). The value in `params` stays the initial value, used until the first control
  value arrives.
- At build time each wired parameter becomes a hidden **control input** of that block
  instance (`Flowgraph::connect_param`): an f32 read cursor the runtime owns. It is not in
  the block's inputs: `work()` never sees it, `configure` doesn't see its metadata, it never
  paces the block, and it is left out of "an input is exhausted → finish".
- **Timing: sample and hold per `work` call.** Before each `work` (right after live params
  from the UI), the runtime drains every control input completely and keeps the last value.
  If it differs from the last one seen, it maps it (float as is, int rounded, bool `> 0.5`),
  checks the parameter's range and calls `Block::set_param`. So a value applies to
  everything the block processes from its next `work` call on, typically within a few
  milliseconds' worth of samples; it is not sample-accurate. NaN is ignored. When the
  producer ends, the parameter keeps its last value.
- A new control value wakes the block (its waker is registered on control cursors too), so a
  change applies promptly even when the stream inputs are idle. No allocation and no locks
  on this path (`serde_json::Value::from(f64)` doesn't allocate; names are the descriptor's
  `&'static str`).
- A value the parameter's range or the block rejects doesn't stop the graph: the block keeps
  its last good value and the runtime records a **warning** once (until a value is accepted
  again), one per block and setting (`Monitor::warnings`, `warning_count`). The server sends
  them as `status.warnings` (pinned to blocks like errors) and the block shows them.
- Control edges count for the topological order, so a loop through a control input is
  refused like any loop (feedback loops would need a delay block; not done).
- An exported input of a hierarchical block may point at `inner.@param`.

**Constant inputs.** A block may give stream inputs a constant instead of a wire:

```toml
[[block]]
id = "add"
kind = "add"
params = { type = "f32" }
const_inputs = { b = "$dc * 2" }   # a number or expression; c32 inputs take [re, im]
```

- Resolved away in `core::resolve`: each constant becomes a hidden block
  `add/const:b` of kind `const_source` (in `foxygnal-blocks`, marked hidden with
  `Registry::set_hidden`, so the catalog serves `"hidden": true` and the palette skips it)
  with the input's stream type, wired to that input. The runtime and the blocks see an
  ordinary graph.
- `const_source` states **no sample rate**, so `StreamMeta::merge` takes the rate from the
  block's other inputs (blocks that read `cx.input_rate(port)` of the constant port itself
  get the usual "nothing upstream sets the rate" error). It fills up to 4096 items per
  `work` call, so it runs in lockstep with whatever consumes it.
- Values are evaluated like params (numbers, SI suffixes, `$vars`, expressions); i16 / u8
  constants are rounded and clamped. `[re, im]` on a non-c32 input, an unknown input, a
  constant on an input that is also wired, and a block whose every input is constant (nothing
  would pace it) are errors naming the block.
- Changing a constant changes the hidden block's live `value` / `imag`, so the server's
  `diff_params` applies it live through `ParamSender`; adding or removing one changes the
  topology (stale). The `/` in the id makes the server treat the hidden block like the inside
  of a hierarchical block: its wire is not drawn, its CPU counts toward `add`, its errors pin
  to `add`.

**Editor.** Each wireable setting has a "⇢ input" toggle in the inspector; on, the block shows
a diamond handle `@name` (labelled with the setting) below its stream inputs, the field says
"wired · initial value", and `ui.wired = ["name"]` keeps the handle while unconnected. Off
removes the handle and its wire. The inspector's *Inputs* section has a "constant" checkbox
and value field per stream input (c32: `re, im`); the node then shows `b = 1.5` instead of a
handle. All descriptor-driven and undoable like any edit.

## Plugins (M6)

Out-of-tree blocks are WebAssembly components (WIT world `foxygnal:block/block-plugin`,
`wit/block.wit`), loaded at start-up by `foxygnal-plugin` with wasmtime (Cranelift, component
model, no WASI). Details, numbers and the guest SDK: [PLUGINS.md](PLUGINS.md).

- **Registry hook.** `Registry::register_plugin(descriptor, plugin, factory)` takes a boxed
  closure (`DynFactory`) instead of a `fn`, refuses kinds that are taken, and marks the catalog
  entry `"plugin": "<name>"`. Kinds are `<plugin>:<kind>` (graph files keep `graph:`).
  Descriptor strings are leaked (`&'static str`), once per plugin load.
- **One instance per block.** The factory instantiates the component in a fresh `Store`, calls
  the plugin's `create`, and returns a `PluginBlock`: a native `Block` that forwards
  `configure`, `set_param` and `work`, and reports `TagPropagation` from the descriptor.
- **Batches, one copy each way.** `work` lowers each input's readable bytes (≤ 8192 items) as a
  `list<u8>` argument (wasmtime copies from the ring into plugin memory); the plugin hands its
  output to the imported `emit`, which the host copies from plugin memory into the ring
  (`WasmList::as_le_slice`), and reports consumption through `consume`. No host allocation per
  call; `work` is skipped when every input is empty.
- **Sandbox.** Imports are only `foxygnal:block/host` (`emit`, `consume`, `add-tag`, `log`). A
  `ResourceLimiter` caps linear memory (64 MiB per instance). Every call has an epoch deadline
  (250 ms); the epoch thread (`fox:plugin-epoch`) ticks every 10 ms while a call runs and parks
  when idle. Traps (including the deadline) become the block's error and stop the graph like
  any block error; the instance is never entered again.
- **Loading.** `--plugins DIR`, else `<project dir>/plugins` (next to the graph for run/bench)
  and `~/.local/share/foxygnal/plugins`. Core modules built with the SDK are turned into
  components at load time (`wit-component`); plugins compile in parallel.

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

**Stereo** travels as one c32 wire: `browser_audio` with `type = "c32"` takes the real part as
left and the imaginary part as right (`fm_stereo`'s two outputs go through `f32_pair_to_c32`).
One wire means one probe and one lossless stream, so the channels can't drift apart or lose
samples independently. The audio probe on a c32 wire sets the frame's stereo flag and
interleaves L, R; the UI builds a two-channel `AudioBuffer`. A real wire stays mono, and the
frame format is unchanged for it (the flag is a new bit; `n` counts sample instants).

The sound-card blocks (`audio_sink` / `audio_source`, `audio` feature) take the same `type`:
with c32 the lock-free ring between `work()` and the cpal callback carries interleaved L/R
pairs (always whole frames), and the callback maps them to the device's channels: L/R to the
first two (silence on any others), or (L+R)/2 on a mono device. A mono sink plays the same
sample on every channel; a stereo source reads the device's first two channels (a mono input
feeds both sides).

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

Every ring also has two **spare read cursors** (`SPARE_READERS`) that are idle (marked done, so
the writer ignores them) until `Tap::attach` claims one while the graph runs. The resulting
`TapReader` is lossless: the writer waits for it like for any downstream block, it gets the
stream's tags, and each `read` that frees space wakes the writer through its registered waker
(like `Reader::consume`). Its owner isn't a block: it polls instead of registering a waker of
its own (wakers are set once per cursor, and a spare changes owner). It is used by recordings
(see "Recording and playback"). The writer may be in the middle of a `work` call that began
before it could see the new cursor, so the first items can still be overwritten;
`TapReader::read` copies with the same seqlock check as a tap and reports a later start if so.

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
until a wake or a timer. The throttle passes 10 ms of stream at a time and sleeps until the
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
  - **Messages** (`text` probe, u8 wires only): every complete text line since the last
    frame, starting with what the ring still holds when it opens. See "Decoded messages".
  - **Image** (`image` probe, frame kind 7, u8 wires only): every whole picture row since the
    last frame, as the raster records themselves (each padded to 4 bytes). See "Decoded
    pictures".
  - **Video** (`ts` probe, frame kind 8, u8 wires only): every whole transport stream packet
    since the last frame. Read on a thread of its own, see "Digital video".
- **Stats** are broadcast twice a second while running: items/s and buffer fill per wire,
  and the share of a core each block spends in `work`. The UI shows them in the wire popover
  and as the *Load* overlay (heat colours on blocks and wires).
- The sample rate (and centre frequency, for absolute spectrum axes) comes from stream
  metadata, or can be set per instrument.

## Decoded messages (M6)

Decoders (`adsb_decoder`, `rds_decoder`, `pocsag_decoder`, `flex_decoder`, `hdlc_deframer`,
`packet_decoder` in hex mode, …) need a way to show a
person what they found, without per-block UI code. The convention: **a decoded message is one line of
UTF-8 text, `\n`-terminated, on a `u8` output.** Lines are `key=value` fields where that
makes sense (`t=1.234567 icao=4840D6 df=17 tc=4 type=ident callsign=KLM1023 raw=…`), so
they read well and grep well. Free text from the air (a pager message, an APRS comment) goes in
a quoted field with `"`, `\`, line breaks and other control characters escaped
(`digital::text::Escaped`), so a message can never span two lines. Everything downstream is
generic:

- The **Messages** instrument (`text` probe, frame kind 6) shows the lines of any `u8` wire,
  with a filter, follow/scroll-back and a "lines were lost" marker; it is offered in the
  wire popover and the dock for u8 wires. The probe reads a tap like every probe (never
  back-pressures the decoder), sends only whole lines, and starts with whatever history the
  ring still holds, so opening it late still shows recent messages.
- `message_log` (Sinks) is the natural end of a decoder's output; with `stdout = true` it
  prints whole lines on the engine's standard output, for `foxygnal run`.
- `file_sink` on the same wire writes a log file.

Why not tags or a new port kind: taps (and so probes) don't see tags, and the planned
untyped message port would touch the runtime, the editor and the file format. A text stream
needs neither, works headless, and can later be carried by message ports unchanged.

Decoders queue one message at a time (`digital::MsgQueue`, preallocated) and take input only
once it has gone out, so a stalled consumer back-pressures the decoder and formatting does
not allocate on the hot path. If the input ends while a message waits for room downstream,
the decoder waits (`WorkStatus::Wait`) instead of finishing, unless every reader has left
(`OutputPort::abandoned`).

## Decoded pictures (M9)

Pictures (SSTV today; weather fax, satellite images and the slideshows of digital radio
later) follow the same idea as decoded messages: **a decoded picture is a sequence of row
records on a `u8` output** (`core::raster`), so nothing in the runtime, the editor or the file
format knows about pictures.

- A record is a 16-byte header, magic `FXIM`, picture number (`u32`, counting the
  decoder's pictures from 1), row, height (0 = not known yet), width (up to 8192), channels (1
  grey, 3 RGB) and flags (bit 0: the picture's last row), followed by the row's pixels, 8 bits
  per channel. Little-endian. A reader that joins mid-stream or loses bytes finds the next
  record by its magic and a header that makes sense (`raster::next`).
- One row at a time keeps a decoder's queue small (an 800-pixel RGB row is 2.4 kB) and lets
  pictures that take minutes to arrive be drawn as they come in. A decoder queues rows with
  `image::push_row` on a `MsgQueue`, like text, and back-pressure works the same way.
- The **Image** instrument draws rows as they arrive, keeps the last few pictures in a strip,
  and saves the shown one as PNG. A picture starts when the picture number (or width) changes
  and ends at its last row, or when the run stops. Like every probe it reads a lossy tap:
  rows arrive at a few kB/s from a live signal, far below what a tap delivers, but decoding a
  file as fast as the CPU allows can outrun it, so the "from a recording" template plays the
  file through a throttle.
- `image_sink` (Sinks) writes one PNG per picture (`<prefix>-<UTC time>-<n>.png`) when the
  last row arrives, the next picture starts or the run ends; rows that never came are black.
  It runs on a thread of its own, since it writes files.
- A decoder that knows when a picture is cut off (an SSTV transmission that stops) sends its
  last row again with the last-row flag, so sinks and the instrument close the picture.

Digital video (M14) doesn't use row records: a demodulator's output is already a compressed
transport stream (see "DVB-S and the DVB FEC chain"), and the browser decodes it (see
"Digital video"). Analog TV, whose frames the engine draws itself, is still an open question
in the roadmap.

### SSTV decoder

`sstv_decoder` (`blocks/src/image/sstv.rs`) takes audio and has two outputs, `image` (rows)
and `out` (text: `event=start/end/lost/cut`, the mode, and the sender's clock error in ppm).

- The audio is mixed down by 1900 Hz, low-passed (1.1 kHz) and decimated to about 10 kS/s; the
  instantaneous frequency is the phase step between samples. It goes into a ring of running
  sums, so the mean frequency over any recent span costs two lookups. VIS detection, sync
  search and pixels are all means over spans.
- VIS: matched at every sample (leader, start bit, eight bits, stop bit), best match in a
  15 ms window, then the leader-to-start-bit edge pins down the start time.
- Line clock: each line's sync pulse is searched for within ±max(3 ms, 1 % of a line) of
  where the clock predicts it, and a least-squares line through the pulses found gives the
  start of every line and the clock ratio (bounded to ±2 %). With slant correction off the
  fit still steers the search, so a picture isn't dropped as lost; only pixel timing ignores
  it. Ten lines in a row without a pulse end the picture.
- Pixels are the mean frequency over each pixel's time. A scan that runs into a sync or
  separator tone holds its last clean pixel over the final 0.35 ms, which band-limited audio
  smears.

## Digital video (M14)

Digital TV (DVB-T, DVB-S, ISDB-T) delivers an **MPEG transport stream** (ISO/IEC 13818-1),
and the convention is as plain as for messages: **a TS wire is a `u8` stream of whole
188-byte packets, each starting with the sync byte 0x47**, with no other framing. A
demodulator writes the packets its outer decoder (Reed-Solomon) delivers, including those it
couldn't correct, with `transport_error_indicator` set; readers skip or conceal those. A `.ts`
file is already in this form, so `file_source` (u8) → `throttle` plays one in real time, and
`file_sink` records a stream that VLC or ffplay can open.

The engine doesn't demux or decode video: the stream is already compressed, a person watches
it in a browser that has video decoders, and decoding in the engine would cost a core per
viewer and turn a few Mbit/s into hundreds. So the engine forwards packets and the browser
does the rest.

**The `ts` probe** (`server/src/probe/ts.rs`, frame kind 8). Payload: `u32 packets, u8 flags
(bit 0: packets were lost before these), u8[3] 0`, then the packets, 188 bytes each, padded
to 4 bytes. It aligns to packets by finding the sync byte at three packet starts in a row,
then expects one every 188 bytes; a packet without it loses the lock (a gap) and the search
starts again, so a stream that isn't whole packets still yields its intact ones. Video runs
at 1–20 Mbit/s (a DVB-T multiplex up to about 32), while a tap can only rely on the newest
half of the ring (16 KiB for a default `u8` ring) and the shared probe thread polls about 60
times a second, which keeps up to about 1 MB/s. Rather than larger buffers for every `u8`
wire (the runtime has one buffer size for all; per-edge sizes would touch the runtime, the
file format and latency elsewhere), the probe reads its tap on **a thread of its own** that
polls as often as the measured byte rate needs, every 1–4 ms: about 8 MB/s (64 Mbit/s) at
the fastest, with no cost to other probes. The probe thread sends what it collected at the
instrument's frame rate (30 fps). As always it never back-pressures the graph: if it falls
behind, or more than 4 MB waits for a slow tab, packets are dropped in whole and the frame is
marked as a gap. A frame dropped on the way to a slow tab shows up as a jump in the frame's
sequence number, which the instrument also treats as a gap.

**The Video instrument** (`ui/src/lib/instruments/VideoView.svelte`) is offered for u8 wires
next to Messages and Image. Everything but drawing happens in a worker
(`ui/src/lib/video/worker.ts`):

- `video/ts.ts` demuxes: PAT and PMT (with CRC check) for the programs and their streams,
  the SDT for service and provider names, PES reassembly with 33-bit PTS/DTS unwrapping, the
  PCR, and counters (TEI packets, continuity errors, packets per PID). A PES with a lost or
  TEI packet is dropped whole. It and `video/nal.ts` (codec strings, key frames, ADTS) have no
  imports, so `npm test` runs them under node (`ui/tests/video.test.ts`, with streams made by
  ffmpeg when it is installed).
- The program is picked by the user, or the first with video. Its video goes to
  **WebCodecs** for H.264 (`avc1.*` from the SPS, Annex B chunks; decoding starts at an IDR,
  or an I slice with SPS and PPS for streams that use recovery points) and HEVC (`hvc1.*`,
  where the browser supports it, checked with `isConfigSupported`); MPEG-1 and MPEG-2 video
  go to **foxygnal's own decoder compiled to WebAssembly** (below), whose I420 pictures become
  `VideoFrame`s too. Sound plays when the user switches it on: AAC (ADTS) through a WebCodecs
  `AudioDecoder`, MPEG audio Layer I and II (MP1/MP2, stream types 0x03 and 0x04) through
  foxygnal's own decoder in WebAssembly (below), both to browser audio. The track is the
  program's first one in PMT order that can be decoded (there is no track choice yet); the
  overlay counts decoded sound frames and errors.
- Timing: the stream arrives in real time, so the program clock maps to arrival time. The
  worker keeps the lower envelope of (arrival − PCR), which network jitter can only raise, and a
  picture is due at its PTS on that clock plus a jitter buffer (300 ms, growing when pictures
  come out late, shrinking back when they don't). Sound is scheduled on the same clock
  (`browserAudio.playAt`), so pictures and sound stay together. Without a PCR the first PTS
  is mapped to its arrival. A jump of more than 2 s (a looped file, a new stream) restarts the
  clock and drops queued pictures.
- The page keeps at most 48 decoded pictures and on each animation frame draws the newest
  one that is due, scaled to fit with its display aspect ratio, and closes the ones it skips.
- An overlay shows the service, codecs, resolution, measured frame rate, the bit rates of the
  stream, the video and the sound, dropped pictures, decoder errors, and continuity, TEI and
  gap counts. A pop-out window runs its own probe and worker, like every instrument.

**MPEG-2 in the browser.** WebCodecs can't decode MPEG-2 video, and most DVB-T and DVB-S
services use it. `crates/foxygnal-mpeg2` is a small decoder for MPEG-2 main profile (and
MPEG-1 video) with no dependencies:

- 4:2:0 only (4:2:2 pictures are skipped and counted as errors); I, P and B pictures, frame
  and field pictures, frame, field, 16×8 and dual-prime prediction, field DCT, both
  coefficient tables, alternate scan, the non-linear quantiser scale, custom matrices, and
  mismatch control. Interlaced pictures come out woven, without deinterlacing.
- Pictures come out in display order. A PES's PTS belongs to the first picture that starts in
  it; pictures without one get the previous PTS plus a frame period. `reset()` (after a gap)
  waits for a sequence header and an I picture. A broken slice is concealed from the previous
  anchor and counted; garbage never panics (tested with random corruption).
- Tests (`cargo test -p foxygnal-mpeg2`) decode streams made by ffmpeg and compare every
  picture with ffmpeg's own decoding: worst-frame PSNR is 67–76 dB (only IDCT rounding
  differs) for progressive and interlaced, SD, 1080 and odd sizes, MPEG-1, alternate scan,
  non-linear quantiser and custom matrices. ffmpeg can't encode field pictures, so
  `tests/synthetic.rs` has a tiny encoder that writes them (with field, 16×8 and dual-prime
  prediction) and checks them against ffmpeg's decoder. The tests skip without ffmpeg.
- Speed: native about 1000 pictures/s at 720×576 and 235/s at 1080; in Firefox (wasm,
  measured by the instrument in its worker) about 3 ms per 720×576 picture at 5 Mbit/s and
  13 ms per 1080i picture at 15 Mbit/s, so HD plays in real time on one core.

**MP2 in the browser.** WebCodecs has no decoder for MPEG audio Layer II either, the sound of
most DVB services. `crates/foxygnal-mp2` is a small one, `no_std` and without dependencies:

- Layers I and II at every MPEG-1 (32, 44.1, 48 kHz), MPEG-2 (16, 22.05, 24 kHz) and
  MPEG-2.5 (8, 11.025, 12 kHz) rate and bit rate; mono, stereo, joint (intensity) stereo and
  dual channel (played as left and right); the CRC is checked when a frame has one, and a
  frame that fails it plays as silence and is counted. Not decoded: Layer III (the overlay
  says so) and free format, and MPEG-2's multichannel extension (its stereo base plays).
- The synthesis filter bank is the standard's: a 64×32 matrix and the 512-tap window, in
  `f32`, about 0.05 ms per 48 kHz stereo frame (24 ms of sound) natively. Tables that need
  `cos` are computed when a decoder is made, so `core` is enough.
- `Stream` takes elementary stream bytes in any pieces, finds the frame grid (a frame counts
  only once the next one follows where its header says, until sync is found), and gives each
  frame the PTS of the PES it starts in, or the previous frame's plus its length. It is one
  fixed-size struct (about 50 KB) and allocates nothing.
- Tests (`cargo test -p foxygnal-mp2`) compare with ffmpeg's float decoder: tones ffmpeg
  encodes (every rate it can encode, mono and stereo, 8 to 384 kbit/s) at 119–124 dB SNR, and
  252 random streams the test writes itself, which cover what ffmpeg's encoder never writes
  (joint stereo at every bound, dual channel, Layer I, CRCs, MPEG-2.5, every bit rate), at
  108 dB or better (the largest difference 0.3 of a 16-bit step; streams quieter than
  −54 dBFS are held to their absolute error instead). Small streams in
  `tests/data`, with ffmpeg's decoding beside them, keep the comparison going without ffmpeg.

`scripts/build-mpeg2-wasm.sh` builds both crates for `wasm32-unknown-unknown` (paths
remapped and symbols stripped, so the build is reproducible) and copies the modules to
`ui/src/lib/video/mpeg2.wasm` (about 90 KB) and `ui/src/lib/video/mp2.wasm` (about 34 KB,
15 KB compressed), which are committed like the plugin test fixtures: building the UI needs
no Rust toolchain and the workspace build no wasm target. Run it again after changing a
crate. The modules import nothing and export small C ABIs (`mp2v_*` and `mp2a_*`, see
`ui/src/lib/video/mpeg2.ts` and `mp2.ts`). The worker loads the sound decoder only when sound
is switched on for an MPEG audio stream. `npm test` runs `mp2.ts` with the committed module
under node, on the crate's fixtures and on a TS with two MP2 tracks made by ffmpeg.

Not decoded in the browser: MPEG audio Layer III, AC-3 / E-AC-3, AAC in LATM (DVB-T2), and
subtitles and teletext. The instrument shows these streams and says so.

## DVB-S and the DVB FEC chain (M14)

`blocks/src/dvb/` holds a DVB-S modem (EN 300 421) and the FEC chain it shares with DVB-T
(EN 300 744) and, for the outer part, DVB-C. The FEC pieces are plain structs with no block
code, so DVB-T reuses them as they are; nothing in them allocates after construction.

**The TS wire.** An MPEG transport stream travels on a `u8` wire as whole 188-byte packets,
each starting with 0x47, and nothing else. A packet the RS decoder couldn't correct is still
sent, with its transport_error_indicator bit (0x80 in byte 1) set, so a player keeps its
timing. The demodulator's `ts` output and the modulator's input are such wires.

**The chain** (transmit order; the receiver runs it backwards):

| stage | module | notes |
|---|---|---|
| energy dispersal | `scramble` | PRBS 1 + x¹⁴ + x¹⁵ from `100101010000000`, restarted every 8 packets, whose first sync byte is sent inverted (0xB8); sync bytes aren't scrambled but the PRBS runs through them. One 1504-byte XOR table, built at compile time |
| RS(204,188), t = 8 | `rs` | shortened RS(255,239) over GF(2⁸) (`0x11D`), g(x) = Π(x + αⁱ), i = 0…15. Decoder: 16 syndromes by table lookup (one per byte and syndrome, the only cost for a clean packet), then Berlekamp-Massey, a Chien search over the 204 positions that exist, and Forney |
| interleaver | `interleave` | Forney, I = 12, M = 17: branch j a FIFO of 17j bytes (deinterleaver 17(11 − j)); 2244 bytes (11 packets) end to end. Sync bytes ride branch 0, undelayed |
| inner code | `conv` | K = 7, G1 = 171₈ (X), G2 = 133₈ (Y), punctured 1/2 … 7/8 per EN 300 421 table 2; the kept bits go alternately to I and Q |
| decoder | `viterbi` | soft `i8` input (0 = erasure, which is how punctured bits come back), `i16` path metrics renormalized every step, 32 butterflies written as plain array code LLVM vectorizes (compiled twice, the AVX2 copy picked at run time), decisions one byte per state; traceback over 384 steps every 256; the re-encoding for the BER estimate by table |

The receive side adds `outer::TsSync`, which finds the 204-byte grid in the decoded bits
(three sync bytes 1632 bits apart, at any bit offset) and `outer::OuterDecoder`
(deinterleave, RS, descramble). Two details matter:

- **Polarity.** The code maps an inverted input to an inverted output, and 0x47 inverted is
  0xB8, so a receiver locked 180° off decodes a valid-looking stream with every byte
  inverted. Only one sync byte in 8 is 0xB8, so two 0xB8 in a row mean the polarity is wrong:
  `TsSync` flips it, at lock and after a 180° slip of the carrier loop later (which the sync
  bytes alone wouldn't show).
- **Group phase.** Descrambling needs each packet's place in its group of 8. The sync bytes
  are visible before deinterleaving (branch 0 has no delay), so the decoder counts from the
  last 0xB8 seen there and shifts by the deinterleaver's 11 packets; a corrected 0xB8 after
  RS overrides the count. Packets before the first 0xB8 aren't output.

**Modulator** (`dvbs_modulator`): TS in (bytes before a 0x47 are skipped to find the grid),
one packet at a time through the outer encoder and the convolutional encoder into a
preallocated symbol buffer, Gray QPSK (bit 0 → +, I first), then an RRC (α = 0.35, 16
symbols) polyphase interpolator to `sps` samples per symbol, scaled for the RMS `gain`. The
output rate is `sps × symbol_rate`. It doesn't pace itself: the TS has to arrive at the
channel's useful rate, symbol rate × 2 × R × 188/204 bits/s (a throttled file, or a live
encoder at that mux rate).

**Demodulator** (`dvbs_demodulator`), per chunk of ≤ 2048 input samples:

1. An NCO (`Rotator`) removes the carrier offset found so far; the RRC matched filter
   (10 symbols) runs at the input rate, any `sps ≥ 2`.
2. Gardner timing recovery with a cubic interpolator (the same scheme as `clock_recovery`).
3. AGC on the symbols, then a second-order decision-directed QPSK Costas loop.
4. While the TS isn't locked, the symbols' 4th power (which removes the QPSK modulation)
   goes into a 4096-point FFT; a line 25× above the mean bin is 4× the residual offset and
   moves the NCO (range ± symbol rate / 8). Then 1500 symbols to settle.
5. Once locked, the timing and carrier loops narrow (0.008 → 0.002 and 0.015 → 0.004 per
   symbol): at low SNR the wide loops slip cycles.
6. Soft bits (`i8`) to the FEC, which **acquires** first: it collects 14 000 symbols and
   tries every hypothesis: code rate (all five in `auto`), puncturing phase (the period's
   start on a symbol boundary: 1, 3, 2, 3, 4 phases), and 4 of the constellation's 8
   symmetries (a 90° rotation, and a mirrored spectrum; the other 4 are these inverted,
   which `TsSync` handles). Each is Viterbi-decoded over 2048 symbols and ranked by its
   re-encoded bit error rate; the best 6 are decoded over the whole buffer and accepted when
   sync bytes appear 204 bytes apart in 6 of 8 packets. Then it **tracks**: the buffer is
   replayed (so little is lost) and every symbol goes on through the chain. Losing the
   packet grid (8 wrong sync bytes in a row, e.g. after a 90° slip) starts a new acquisition.
7. Decoded packets go into a 64 KB FIFO; input is taken only while 16 KB are free, so a
   slow consumer back-pressures the demodulator and nothing is dropped.

The `status` output follows the decoded-messages convention: a line per second of stream
time (`t=… state=lock rate=3/4 mer_db=… ber=… rs_corrected=… rs_failed=… packets=… pps=…
freq_hz=…`; `state` is `search`, `sync` (rate found, no grid) or `lock`) and one per lock
change (`event=lock rate=… spectrum=normal|inverted freq_hz=…`, `event=unlock`). MER is
from the decisions; `ber` is the channel bit error rate the Viterbi decoder corrected,
measured by re-encoding its output and comparing with the hard decisions of the bits
received; `rs_corrected` counts bytes. Lines are dropped, not queued, if the text consumer
stalls, so a stuck log can't stall the video.

**Measured** (`tests/dvbs.rs`, release build, 250 kBd at 2 samples/symbol, 1.5 kHz off,
30 ppm clock error, 400 packets per point; "clean" = no uncorrectable packet):

| rate | decodes (≥ 98 % of packets) | clean | EN 300 421 QEF, ideal modem |
|---|---|---|---|
| 1/2 | Es/N0 2.5 dB | 3.5 dB | 3.4 dB |
| 2/3 | 4.0 dB | 4.5 dB | 5.1 dB |
| 3/4 | 5.0 dB | 5.5 dB | 6.1 dB |
| 5/6 | 6.0 dB | 6.5 dB | 7.1 dB |
| 7/8 | 6.5 dB | 7.0 dB | 7.7 dB |

(The last column is the standard's required Eb/N0 for quasi-error-free reception, minus its
0.8 dB implementation margin, as Es/N0. QEF means about one error an hour, a stricter test
than 400 clean packets.) Speed on a Ryzen 9 7940HS laptop: the Viterbi decoder alone
decodes about 110 Mbit/s on one core (AVX2), the whole demodulator about 21 MS/s of
input, i.e. 10 MBd at 2 samples per symbol and rate 3/4 (`cargo bench -p foxygnal-blocks
--bench blocks -- dvbs`), acquisition included.

## DVB-T (M14)

`blocks/src/dvb/dvbt/` is the DVB-T modem (EN 300 744), non-hierarchical: 2k and 8k, QPSK,
16-QAM and 64-QAM, code rates 1/2–7/8, guards 1/4–1/32, 5–8 MHz channels. Both ends use the
FEC chain above unchanged; what's new is the OFDM layer.

**Tables** (`frame.rs`, built once per mode into a `OnceLock`): continual pilots (45 / 177,
the 8k ones are the 2k set repeated every 1704 carriers, as are the 17 / 68 TPS carriers),
the pilot PRBS (x¹¹ + x² + 1 from all ones, one bit per carrier, `1 − 2w`), the data carriers
of each of the 4 scattered-pilot phases (1512 / 6048 each), and the symbol interleaver H(q).
The inner interleaver (bit demultiplexer, six 126-bit interleavers, symbol interleaver) is one
gather table per constellation and symbol parity: coded bit i of a symbol is bit e of data
carrier c. The transmitter scatters with it, the receiver gathers with it. The TPS is
68 bits per frame; BCH(67,53) (x¹⁴ + x⁹ + x⁸ + x⁶ + x⁵ + x⁴ + x² + x + 1, t = 2) is decoded
by a syndrome table of all 2278 one- and two-error patterns.

**Modulator** (`dvbt_modulator`): TS packets → outer encoder → convolutional encoder into a
bit buffer; each symbol takes 1512·v / 6048·v bits, scatters them through the interleaver,
maps them (Gray, `y0` the sign of I, `y1` of Q, then the amplitude bits, 0 = outer), adds
the pilots (±4/3) and the TPS (DBPSK, the reference from the PRBS in symbol 0), and an
inverse FFT with the cyclic prefix. The output, at 64/7 MHz × bandwidth / 8, has RMS `gain`.
Like DVB-S it doesn't pace itself.

**Demodulator** (`dvbt_demodulator`):

1. **Resampler.** An arbitrary-ratio polyphase resampler (Kaiser sinc, 256 phases, about
   18 taps per output sample, cut off at half the output rate) takes any input rate ≥ the
   elementary rate down to it. Its step is the nominal ratio × (1 + clock offset), and the
   timing loops move its position: the sampling clock is corrected in the time domain. (At
   50 ppm the outer carriers of the 8k mode are 0.17 carrier spacings off, far too much
   inter-carrier interference for 64-QAM to correct afterwards.)
2. **Detection** over 69 632 samples: the cyclic prefix correlation r(n)·r*(n+N), summed over
   G samples and folded at the period N + G, for every (mode, guard) allowed. The candidate
   with the highest normalized peak wins (a guard too long correlates only partly, a wrong
   period smears the fold); the peak gives the symbol start, its angle the fractional
   frequency offset. The FFT window is placed half a guard into the prefix, so echoes up to
   G/2 early or late stay inside it.
3. **Integer frequency offset**: over 4 symbol pairs, |Σ Y_l·Y*_(l−1)| at the continual pilot
   positions shifted by every m within the band edges (±171 / ±687 carriers); the pilots
   repeat, data doesn't, so the peak (4× the mean at least) is the offset. The NCO then
   removes the whole offset.
4. **Per symbol**: NCO, FFT of the useful part only (the guard interval isn't even
   resampled), then from the continual pilots against the previous symbol: the common phase
   step (→ frequency loop, gain 0.3, and the derotation that keeps the pilots coherent across
   the time interpolation) and the phase slope across the band = the timing drift per symbol
   (→ clock loop, gain 0.3, minus any deliberate window moves).
5. **TPS**: one differential bit per symbol, Re Σ Y·Y*_(l−1) over the TPS carriers after
   removing the common phase and the slope. The sync word (or its inverse) gives the
   frame position; at the end of the frame BCH decodes s1…s53: constellation, code rate,
   guard and mode (checked against detection), the cell id (high byte in odd frames, low in
   even). Three bad frames in a row, a TPS that disagrees with the detected mode, or 24
   symbols in a row of incoherent pilots start over at detection.
6. **Channel estimate**: each symbol's scattered pilots (every 12th carrier, offset 3·(l mod
   4)) update a grid of every third carrier. The symbol 3 back is equalized: the grid
   interpolated linearly in time between its pilots 4 symbols apart, rotated by
   e^{+j2πk'(G/2)/N} (so the delays sit around 0), padded at the band edges by odd
   reflection, and interpolated in frequency by two 16-tap fractional-delay filters (1/3 and
   2/3), cut off at (G/2)/(N/6) + 0.25 of the grid's Nyquist: the grid resolves delays of
   ±N/6, the guard needs ±G/2. The grid's mean phase step is the channel's mean delay, which
   moves the window by at most 0.02 samples a symbol to keep it centred.
7. **Demapping**: z = Y·H*/|H|² on the odd-integer grid, max-log soft bits (`r`,
   `|r| − 2` for 16-QAM, `|r| − 4` and `||r| − 4| − 2` for 64-QAM) × 20 × |H|²/mean|H|²
   (CSI: faded carriers become near-erasures), as `i8`; then the inner deinterleaver (one
   gather), depuncturing (a slice at a time) and the chain: Viterbi, `TsSync`,
   `OuterDecoder`. The puncturing phase isn't signalled, but every symbol holds whole
   periods: after 12 096 soft bits every phase is Viterbi-decoded and the lowest re-encoded
   BER wins.

Status lines follow DVB-S: one per second (`t=… state=search|sync|lock mode=8k guard=1/32
constellation=64qam rate=2/3 hierarchy=0 cell_id=… mer_db=… ber=… rs_corrected=… rs_failed=…
packets=… pps=… freq_hz=… clock_ppm=… tps_corrected=…`), plus `event=tps …` when the TPS
(or the cell id) changes and `event=lock … freq_hz=… clock_ppm=…` / `event=unlock`.
Hierarchical TPS are reported (`hierarchy=α`) but not decoded.

**Measured** (`tests/dvbt.rs`, release build, 1 kHz and 10 ppm off; "decodes" = every packet
of 370–2000 clean; the last column is EN 300 744 table A.1, Gaussian channel, which assumes
perfect channel knowledge and a QEF criterion):

| mode | decodes | some lost | EN 300 744 |
|---|---|---|---|
| 2k QPSK 1/2, 1/4 | C/N 4.0 dB | 3.5 dB | 3.1 dB |
| 2k 16-QAM 1/2, 1/32 | 9.0 dB | 8.5 dB | 8.8 dB |
| 8k 16-QAM 2/3, 1/8 | 11.5 dB | 11.0 dB | 11.1 dB |
| 8k 64-QAM 2/3, 1/32 | 16.0 dB | 15.5 dB | 16.5 dB |
| 2k 64-QAM 7/8, 1/8 | 20.0 dB | 19.5 dB | 20.1 dB |

C/N counts the noise in the occupied band (K/N of the sample rate). The MER the status shows
is about 2 dB below the C/N: the boosted pilots take some of the power, and the linear time
interpolation passes the pilots' noise into the estimate unfiltered.
The tests also run every one of the 120 combinations clean (ignored test `full_matrix`), an
offset of 3.4 carriers with the clock 50 ppm fast (8k 64-QAM), −7.6 carriers and −50 ppm
(2k), an echo at −3 dB 0.3 guard intervals late, and a 7 MHz channel received at 10 MS/s,
all bit-exact.

**Speed** (`cargo bench -p foxygnal-blocks --bench blocks -- dvbt`, Ryzen 9 7940HS, one core):
8k 64-QAM guard 1/32 at 9.14 MS/s, acquisition included: 15.9 MS/s at rate 2/3, 13.8 MS/s at
7/8 (1.7× and 1.5× real time). Locked, a 7/8 symbol costs about 0.7 ms of its 0.92 ms:
Viterbi ≈ 0.32 ms, equalizing and demapping ≈ 0.15 ms, packet sync and RS ≈ 0.09 ms, the
resampler ≈ 0.05 ms, the FFT 0.01 ms.

## Recording and playback (M6)

**Tags ↔ SigMF.** `blocks/src/sigmf.rs` maps tags to SigMF metadata and back. `SigmfWriter`
(used by `sigmf_sink` and by server recordings) writes the data file and builds the metadata:
`rx_freq` starts a new `captures` segment (`core:sample_start`, `core:frequency`, and
`core:datetime` when the rate is known); `hw` sets `global` `core:hw`; `overflow` becomes an
annotation labelled `overflow` with `foxygnal:overflow` (the count) and a comment; any other tag
`key` becomes an annotation labelled `key` with `foxygnal:key` = the value (NaN, which JSON
lacks, is `null`). Recordings also get `core:recorder` "foxygnal", `core:num_channels`, and a
`foxygnal` entry in `core:extensions`. The metadata file is written when the recording starts,
rewritten at most once a second while tags change it, and at stop, each time to a `.tmp` file
renamed over the old one, so a crash leaves a usable recording. `parse_tags` is the reverse:
`sigmf_source` emits `rx_freq` at every capture's start, `hw` at 0, and each annotation's
`foxygnal:*` fields as tags (an annotation without any becomes a `label` tag from
`core:label` / `core:comment`), again on each loop. Interleaved i16 / u8 streams count two
items per sample. Its `start` param (a sample index, the unit annotations use) seeks the data
file to `start × sample size` and shifts the tags back by `start`; earlier tags are dropped,
except `hw` and the `rx_freq` of the capture in effect, which land on item 0 (and set the
output's centre frequency). A loop restarts at `start`.

**One-click recording** (`server/src/record.rs`). Any wire of the running graph can be recorded
without editing the graph. A recording is not a probe: it attaches a lossless `TapReader` to the
wire's output, so a slow disk slows the graph (a hardware source then reports `overflow`, which
lands in the file as an annotation) rather than dropping samples silently. Each recording has its
own thread that copies whatever is readable, handles the tags, writes raw items (any stream
type, recorded as `cf32` / `rf32` / `ri16` / `ru8`), and sleeps 2 ms when there is nothing new.
Items lost right after attaching (see "Buffers") shift the start; a later loss, which shouldn't
happen, would be a `gap` annotation. Rate and centre frequency come from the wire's metadata,
`core:hw` from the nearest upstream block with a device parameter (or an `hw` tag). Recordings
belong to the session, not the tab, so a reload doesn't stop them; they end on `record_stop`,
when the run ends, or on an error, and shutting the server down finishes them first.

Files go to `recordings/` in `--project-dir`, named
`<graph>-<wire>-<UTC time>` (e.g. `fm-live-sdr_out-20261122T134500Z`), with `-2`, `-3`… if taken.
A client-chosen `name` must be a plain name (letters, digits, `-`, `_`, `.`, starting with a
letter or digit, no `..`), as must `{name}` in the REST endpoints, so nothing is written or
read outside that folder.

**Playback.** The palette's *Recordings* panel lists `GET /api/recordings`; *Add* puts a
`sigmf_source` with `path = "recordings/<name>.sigmf-meta"` (relative paths resolve against the
project directory) on the canvas, `c32` for complex recordings and `f32` for real ones.
Each entry expands into a timeline (`GET /api/recordings/{name}`): capture segments shaded and
labelled with their frequency, annotations as markers coloured by label, with tooltips. Clicking
the strip or a marker offers *Add source from here*, a `sigmf_source` with `start` at that
sample. While recording, the `record` events carry an `annotations` count, which the record
button shows.

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

## Network blocks

`blocks/src/net/` (category *Network*): `udp_source`, `udp_sink`, `tcp_source`, `tcp_sink`
move raw little-endian items, so they interoperate with GNU Radio's UDP/TCP blocks, `nc` and
`socat` (see [BLOCKS.md](BLOCKS.md)). They reuse the SDR drivers' pattern and their ring
(`foxygnal_sdr::ring`; blocks already depend on the sdr crate, so no new crate or copy):

- **Sources** (dedicated thread) bind or connect in `start()`'s I/O thread, which pushes bytes
  into a byte ring plus a small event ring (`dropped`, `connected`, stamped with a byte
  offset) and unparks the block. `work()` copies whole items and tags them; it never waits.
  The I/O thread keeps a partial item until the rest arrives and, after a loss, skips to the
  sender's item grid, so items stay aligned. UDP pushes whole datagrams or drops them
  (counted as `dropped`, like a sequence gap); TCP waits for room in the ring, which pushes
  back on the sender through TCP's flow control.
- **`udp_sink`** (pooled) sends from `work()` on a non-blocking connected socket; a partial
  datagram waits up to 10 ms for more items (`Wait`), and a full socket buffer retries after
  1 ms.
- **`tcp_sink`** (dedicated) has a connection thread (accept loop, or connect with retry)
  that gives each client a bounded SPSC queue and a writer thread; new clients reach `work()`
  over a channel (`try_recv`), and gone ones are noticed when their queue is abandoned.
  `work()` copies into every queue: whatever doesn't fit is skipped for that client and
  logged (at most every 5 s), unless `backpressure` makes it consume only what every client
  can take. At the end of the stream the queues get up to 1 s to drain.
- I/O threads poll a stop flag every 50 ms (socket timeouts), so stopping a graph is prompt.
- **Security:** every address defaults to `127.0.0.1`; a listener on any other address must
  be typed into the block, and the engine prints a note that the stream (which has no
  authentication) is reachable. This is separate from the server's `--allow-remote`.

## Server API

REST:

- `GET /api/health`: name and version
- `GET /api/blocks`: block descriptors, plus graph files usable as blocks (see "Graph files")
- A block descriptor with `"response": true` belongs to a block
  registered with `Registry::register_with_response` (filters, resamplers)
- A block descriptor with `"tx": true` transmits (see "TX arming")
- `"hidden": true`: an internal block (`const_source`) the palette leaves out
- `"keywords"` (optional, `Registry::set_keywords`): aliases the palette search also matches
  (`abs`, `envelope` for `c32_to_f32`)
- `GET /api/devices`: per-driver discovery, `[{driver, label, devices [{driver, args, label,
  serial, rx, tx}], error}]`. Each driver searches on a blocking thread with a 4 s timeout,
  so a slow driver can't hang the request; a driver still busy from an earlier timed-out
  search isn't asked again until it returns
- `POST /api/blocks/{kind}/response` `{params, sample_rate}` → `{freqs, db, taps}` (`taps` 0: IIR): the magnitude
  response the block would have at that input rate (400 with a readable message for invalid
  params). The inspector draws it for any block whose descriptor says `response`
- `GET/PUT /api/graph`: current graph as `GraphSpec` JSON; `GET /api/graph.toml`
- `POST /api/toml/parse`, `POST /api/toml/render`: convert for upload/download
- `GET /api/examples`, `GET /api/examples/{name}`: bundled example graphs
- `GET /api/recordings`: SigMF recordings in `recordings/` under `--project-dir`, newest first:
  `[{name, path, modified, datatype, sample_rate, frequency, samples, bytes, captures,
  annotations, datetime, hw, description, recording, error?}]`. `GET /api/recordings/{name}`
  returns the metadata document, `DELETE /api/recordings/{name}` removes both files (409 while
  it is being recorded). Invalid names are a 400
- `GET /api/files`, `GET/PUT /api/files/{name}`: `.fox.toml` files in `--project-dir`
  (plain file names only). Relative `path` parameters and `graph:` blocks also resolve
  against that directory. `GET` falls back to the bundled sub-graph library. The list also
  has the directory's `.grc` files.
- `GET /api/import/{name}` (a `.grc` in `--project-dir`), `POST /api/import` (`.grc` text):
  convert a GNU Radio flowgraph → `{graph, warnings, title, name}`, `name` being the
  `.fox.toml` to save it as. Nothing is written; see [GRC_IMPORT.md](GRC_IMPORT.md).

WebSocket `/api/ws`, one per browser tab:

- **Text frames: JSON control messages** with `{"op": …}`. Client: `set_graph {graph, name}`
  (the whole graph; the server diffs it, pushes changed live params and marks other changes
  stale), `run {arm_tx?}` (`arm_tx: true` arms TX for this run, see "TX arming"), `stop`,
  `probe_set {id, edge {from, to}, probe {kind, …options}, fps}` (the
  client picks `id`; sending it again changes the probe), `probe_remove {id}`,
  `record_start {edge {from, to}, name?}` and `record_stop {edge}` (see "Recording and
  playback"). Server:
  `hello` (full snapshot on connect or after lag, including `meta`, `allow_tx`: the
  server was started with `--allow-tx`, and `recordings`: those in progress),
  `record {edge, name, path, started_at, elapsed_ms, samples, bytes, gaps, annotations, done, error?}` to
  every tab, about twice a second per recording and once with `done: true` when it ends (a
  start that failed has no `name` and an `error`),
  `graph {graph, name, version, from}`, `meta {edges [{from, to, sample_rate?, center_freq?}],
  error?, tx?}` after every graph change (`tx`: blocks that would transmit, `[{block, kind,
  device?, frequency?, sample_rate?, gain?}]`),
  `status {state, started_at, stale, errors[], warnings[]?, tx_armed?}` (`warnings`: rejected control input values, see "Control inputs and constant inputs"), where each error names a `block`, an
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
  - audio (5): `u32 n, u8 flags (1 gap: samples were lost before this frame, 2 stereo), u8[3]`,
    then `f32[n]` (mono, real wires) or, with the stereo flag, `f32[2n]` interleaved L, R (c32
    wires: real = left, imaginary = right): every sample since the previous frame. The UI plays
    these for `browser_audio` blocks (see "Browser audio").
  - text (6): `u32 n, u8 flags (1 gap: lines were lost before this frame), u8[3]`, then
    `n` bytes of UTF-8, whole lines only (each ends in `\n`), padded to 4 bytes. Lossless
    while the probe keeps up, like audio (u8 wires only; see "Decoded messages").

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

## Assistant: the MCP server (M10)

An AI assistant can build graphs, run them and look at signals through a
[Model Context Protocol](https://modelcontextprotocol.io) server (`server/src/mcp/`). It
uses the same session as the browser, not a parallel API, so an assistant's work is
ordinary edits that a person sees and can undo.

- **Transports.**
  - Streamable HTTP at `POST /api/mcp` on `foxygnal serve`: the browser's session.
    Answers are plain JSON (no server-sent events, no MCP sessions). A notification gets
    202, and `GET` / `DELETE` get 405.
  - `foxygnal mcp`: newline-delimited JSON-RPC on stdin/stdout with a headless session of
    its own. Requests are handled in order. Logs go to stderr. On Unix, file descriptor 1
    is pointed at stderr once the real stdout is duplicated for the protocol, so a Message
    Log with "print to stdout" can't corrupt it.
  - Protocol versions 2024-11-05 through 2025-11-25; a client asking for another one gets
    the newest. Batches are accepted.
- **Tools** (`mcp/tools.rs`), grouped by job:
  - **Catalog:** `list_blocks` (word search over kind, label, help and keywords),
    `describe_block` (the descriptor, marking `wireable` settings), `list_examples` /
    `get_example`.
  - **Graphs:** `get_graph`, `set_graph`, `check_graph`, and `edit_graph`, which applies
    operations all or nothing: add or remove blocks, set settings (unknown names are refused,
    listing the real ones), connect and disconnect (a port may be left out if the block has
    only one), constant inputs, variables.
    - When a `connect` joins different stream types (worked out by resolving the graph and
      building the two blocks), the converter block is inserted between them if exactly one
      fits. The reply's `notes` name it and its settings (`c32_to_f32` has `part`), or list
      the choices. `convert: false` leaves the mismatch for the report to name. Every graph tool answers with a **check report**:
    - whether the graph builds, or the first error, pinned like in the editor
    - the sample rate and centre frequency on every wire
    - unconnected inputs (they only fail at `run` otherwise)
    - transmitting blocks
    - the run state, including whether edits are waiting for a restart.
  - **Files:** `list_files` / `open_file` (`.grc` imports) / `save_file` (project directory
    only, same file-name rules as the REST API), `import_grc` / `export_grc`.
  - **Running:** `run` (never arms TX), `stop`, `status` (errors, warnings, CPU per block,
    throughput and buffer fill per wire over half a second).
  - **Signals:** `measure` (a spectrum, level or messages summary), `watch` / `read_watch` /
    `stop_watch` (a decoder's text lines or pictures over time; pictures come back as PNG
    image content), `list_devices`, and `record` (a SigMF capture of n seconds).
- **Probes.** MCP is one more client of the probe hub (client id `u64::MAX`). Its probes are
  ordinary probes: they bind to each new run, rewind to the history the ring holds, and
  never slow the graph down. Frames come back to `mcp/probes.rs` and are folded per probe:
  - spectra averaged in linear power
  - meter readings accumulated
  - text lines kept (up to 5000)
  - picture rows assembled (the last 4 pictures).

  Summaries are built from that, never raw samples:
  - The noise floor is the median bin.
  - A *signal* is a stretch of bins more than 6 dB above the floor, bridging gaps of up to
    3 bins, whose strongest bin clears `min_snr_db`. So an FM broadcast station is one
    signal 200 kHz wide, not a carrier plus sideband lumps.
  - Only bins within 100 dB of the strongest count; below that, a noiseless digital signal
    has float rounding spurs.
  - Each signal reports its strongest point (a parabola through three bins), its
    power-weighted centre, its bandwidth and its level.
  - A 48-slice outline gives the overall shape.
  - **Constellation** (the `constellation` probe, 8192 points per frame, the last 64k kept):
    - RMS, the envelope spread (σ/mean of |x|) and DC.
    - The M-th power test |E[(x/|x|)^M]| for M = 2, 4, 8. With a nearly constant envelope
      (spread < 0.25), the smallest M above 0.6 gives the PSK order. EVM and MER are then
      measured against the ideal points at the derotation angle.
    - The symbol rate is the strongest line in the averaged spectrum of |x|² − mean
      (1024-point chunks, parabola-interpolated), if it stands 12 dB above the median.
      That's present for pulse-shaped linear modulation sampled at more than one sample per
      symbol, so it is measured *before* clock recovery and PSK order *after* it.
- **The editor.** A graph changed over MCP is sent to tabs with `by: "assistant"`. The editor
  puts the previous graph on its undo stack and shows a toast with an Undo button, once per
  burst of edits.
- **Resources:** `foxygnal://graph` (the open graph), `foxygnal://blocks` (the catalog),
  `foxygnal://docs/*` (README, architecture, blocks, hardware, GRC, roadmap) and
  `foxygnal://examples/*`.
- **Prompts:** `build_receiver`, `find_signals`, `decode_recording`.
- **Limits:**
  - Nothing over MCP can arm TX: a graph with a transmitting block is refused like any
    unarmed run, and the check report says so.
  - The file tools stay in the project directory. A graph's own settings (a File Sink's
    path) work as in the editor.
  - `/api/mcp` is behind the same Host / Origin / token checks as the rest of the API.

## Security

A local server that can write files (file sink) and later transmit RF needs care:

- It binds `127.0.0.1` by default. Non-loopback binds need `--allow-remote`.
  (Network blocks are separate: their listeners also default to `127.0.0.1`, and anything
  else is typed into the block; see "Network blocks".)
- **Host and Origin checks** on every request and WebSocket upgrade. These block DNS
  rebinding attacks and cross-site requests from pages open in the same browser. This matters
  even on localhost.
- For `--allow-remote`: a random access token printed at startup, like Jupyter. Opening
  `/?token=…` sets an `HttpOnly; SameSite=Strict` cookie; API clients can send
  `Authorization: Bearer …`.
  Recommend a reverse proxy with TLS.
- **MCP** (`/api/mcp`, `foxygnal mcp`) can edit and run graphs but never arm TX, and its file
  tools stay in the project directory. See "Assistant: the MCP server".
- **Transmit is opt-in twice**: per server (`--allow-tx`) and per run (arm TX, confirmed in
  a dialog); TX blocks also refuse to start unless armed, and the UI shows a red TX indicator
  while armed. See "TX arming". The user is responsible for staying legal.

## UI stack

Svelte 5 + Vite + TypeScript, and **Svelte Flow** (`@xyflow/svelte`) for the node editor.
Instruments are hand-written canvases, not a charting library: Canvas2D for traces (scope,
spectrum, constellation), and WebGL2 for the waterfall, whose history lives in a float
texture used as a ring. A new row is one small upload, and changing the dB range re-colours
the whole history. Video is decoded in a worker (WebCodecs, or the MPEG-2
decoder in WebAssembly) and drawn as `VideoFrame`s on a 2D canvas. Instruments live in a resizable dock under the canvas, are remembered per
graph file (browser storage), and can pop out into their own window (`?popout=<id>`), which
opens its own engine connection and syncs settings back over a `BroadcastChannel`. In release builds the built UI is embedded in the binary with `rust-embed`
(`cargo build --release --features embed-ui`), so you ship a single file. Without the feature
it is served from `ui/dist` (or `--ui-dir`).

## Prior art and positioning

- **GNU Radio / GRC**: the reference. Huge block library, but heavy installs, Python/C++
  friction and a dated desktop UI. foxygnal reads its raw file formats and imports `.grc`
  flowgraphs built from common blocks ([GRC_IMPORT.md](GRC_IMPORT.md)).
- **FutureSDR**: a Rust SDR runtime (async, WASM, GPU). Strong engine work. foxygnal focuses
  on the end-user editing and instrument experience. Worth tracking and borrowing ideas from.
- **SDR++ / SDRangel / GQRX**: receiver apps with good UX but fixed pipelines.
  foxygnal aims for their polish with a free-form graph.
