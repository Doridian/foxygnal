# Roadmap

Every milestone ends with something a user can actually do. Order matters more than dates.

## M0: Skeleton ✅ (2026-11-22)

- [x] Cargo workspace: `core`, `blocks`, `server`, `foxygnal` CLI
- [x] Lock-free single-writer/multi-reader ring buffer with fan-out
- [x] `Block` trait, typed + byte-level port access, typed parameter specs
- [x] Registry with descriptors and `.fox.toml` graph spec, with type-checked connections and friendly errors
- [x] Thread-per-block runtime with end-of-stream and stop handling
- [x] Blocks: signal generator, noise, file source/sink, add, multiply, gain, throttle, head, null sink
- [x] `foxygnal run | serve | blocks`; localhost-only by default
- [x] UI shell: palette from `/api/blocks`, node canvas, typed port colours, parameter inspector

## M1: Edit and run from the browser ✅ (2026-11-22)

Goal: build the example graph by hand in the browser, press Run, see it work, save it.

- [x] WebSocket session: graph sync, run/stop, state and error events
- [x] Connect ports in the UI with type validation and readable rejection reasons
- [x] Save/load `.fox.toml` (download/upload and a server-side project dir); example gallery
- [x] Errors pinned to the offending block (build errors and runtime errors)
- [x] Live parameter updates for flagged params (frequency, amplitude, gain)
- [x] Undo/redo, copy/paste, delete, keyboard block search (`/`)
- [x] Host/Origin checks, access token for `--allow-remote`
- [x] Embed UI in the binary (`rust-embed`, `--features embed-ui`)
- [x] CI: fmt, clippy, tests, UI type-check and build

## M2: See the signal

Goal: click a wire and watch it.

- [ ] Lossy probe readers, probe thread, binary WS frames
- [ ] Oscilloscope (trigger modes, I/Q/mag/phase, time/div, cursors)
- [ ] Spectrum (FFT size, window, averaging, peak hold) and waterfall
- [ ] Constellation, level meter
- [ ] Dockable instrument panel with several instruments side by side and pop-out windows
- [ ] Per-edge throughput, buffer fill, per-block CPU, and a heat-map overlay

## M3: A real DSP toolbox

Goal: build an FM broadcast receiver from a recording, with audio out.

- [ ] Stream metadata propagation (sample rate, centre frequency) and rate labels on wires
- [ ] Tags
- [ ] FIR filters with a visual designer (low/high/band-pass, frequency response preview)
- [ ] Rational resampler, decimator/interpolator, frequency-translating FIR
- [ ] AGC, AM/FM/SSB demodulators, squelch
- [ ] Audio sink/source (`cpal`), WAV and SigMF file source/sink
- [ ] Type converters, offered for auto-insertion when the user connects mismatched ports
- [ ] Graph variables (`$samp_rate`) and hierarchical blocks

## M4: Hardware

Goal: a live FM receiver from an RTL-SDR with three clicks.

- [ ] `foxygnal-sdr` crate, device trait, discovery panel in the UI
- [ ] SoapySDR backend (feature-gated, broad coverage)
- [ ] Native RTL-SDR, plus `rtl_tcp` / SpyServer network sources
- [ ] HackRF and others as needed; live retune/gain via live params and tags
- [ ] TX path with the explicit arm-TX safety flow

## M5: Performance

Goal: credible numbers on a Raspberry Pi.

- [ ] Benchmarks (`criterion`) per block plus whole-graph throughput, tracked in CI
- [ ] Double-mapped ring buffers (contiguous slices)
- [ ] Pooled scheduler; dedicated threads only where needed
- [ ] SIMD kernels for FIR, FFT glue, complex multiply, and type conversion
- [ ] Idle-CPU budget: a paused or throttled graph should use ≈0% CPU
- [ ] Targets (to validate): 2.4 MS/s FM receiver under 25% of one Pi 4 core; engine RSS
      under 50 MB

## M6: Ecosystem

- [ ] Out-of-tree block plugins (decide: WASM components vs. Rust dylibs vs. compiled-in crates)
- [ ] Digital modes: FSK/PSK modems, clock recovery, packet framing, a few decoders (ADS-B, POCSAG, …)
- [ ] Recording and playback workflow (SigMF-annotated captures)
- [ ] Maybe: GNU Radio `.grc` import for common blocks

## Open questions

- **Plugin strategy** (M6). WASM gives sandboxing and portability at some per-call cost.
- **Name casing**: `foxygnal` vs `Foxygnal` in the UI.
