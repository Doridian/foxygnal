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

## M2: See the signal ✅ (2026-11-22)

Goal: click a wire and watch it.

- [x] Lossy probe readers (taps), probe thread, binary WS frames
- [x] Oscilloscope (trigger modes, I/Q/mag/phase, time/div, cursors)
- [x] Spectrum (FFT size, window, averaging, peak hold) and waterfall
- [x] Constellation, level meter
- [x] Dockable instrument panel with several instruments side by side and pop-out windows
- [x] Per-edge throughput, buffer fill, per-block CPU, and a heat-map overlay

## M3: A real DSP toolbox

Goal: build an FM broadcast receiver from a recording, with audio out.

- [x] Stream metadata propagation (sample rate, centre frequency) and rate labels on wires
- [x] Tags
- [x] FIR filters with a visual designer (low/high/band-pass, frequency response preview)
- [x] Rational resampler, decimator/interpolator, frequency-translating FIR
- [x] AGC, AM/FM/SSB demodulators, squelch
- [x] Audio sink/source (`cpal`), WAV and SigMF file source/sink
- [x] Type converters, offered for auto-insertion when the user connects mismatched ports
- [x] Graph variables (`$samp_rate`) and hierarchical blocks

## M4: Hardware

Goal: a live FM receiver from an RTL-SDR with three clicks.

- [x] `foxygnal-sdr` crate, device trait, discovery panel in the UI, simulated receiver
- [x] SoapySDR backend (loaded at run time, so no build dependency; broad coverage, tested on a PlutoSDR)
- [x] Native RTL-SDR, plus `rtl_tcp` / SpyServer network sources
- [x] HackRF and others as needed; live retune/gain via live params and tags
- [x] Browser audio sink, and a "Live FM radio" template (open, pick, Run)
- [x] TX path with the explicit arm-TX safety flow

## M5: Performance

Goal: credible numbers on a Raspberry Pi.

- [x] Benchmarks (`criterion`) per block plus whole-graph throughput, tracked in CI
- [x] Double-mapped ring buffers (contiguous slices)
- [x] Pooled scheduler; dedicated threads only where needed
- [x] SIMD kernels for FIR, FFT glue, complex multiply, and type conversion
- [x] Idle-CPU budget: a paused or throttled graph should use ≈0% CPU
- [ ] Targets (to validate): 2.4 MS/s FM receiver under 25% of one Pi 4 core; engine RSS
      under 50 MB
      — harness ready, to validate on a Pi: `foxygnal bench bench/fm-receiver-sim.fox.toml`,
      aarch64 CI build ([PERFORMANCE.md](PERFORMANCE.md))

## M6: Ecosystem

- [ ] Out-of-tree block plugins (decide: WASM components vs. Rust dylibs vs. compiled-in crates)
- [x] Digital modes: FSK/PSK modems, clock recovery, packet framing, a few decoders (ADS-B, POCSAG, …)
      — clock recovery (Gardner / M&M), Costas loop, slicer, PSK demapper, 2-FSK, access
      code + packet decoder, ADS-B (DF17/18 only, no Mode S phase correction or local CPR);
      decoded messages are text lines shown by the generic Messages instrument. Not yet:
      POCSAG, AX.25/HDLC, FEC, GFSK/GMSK, multi-level FSK
- [x] Recording and playback workflow (SigMF-annotated captures)
- [x] GNU Radio `.grc` import, common blocks only (`foxygnal import`, or open a `.grc` in the
  browser; [GRC_IMPORT.md](GRC_IMPORT.md))

## Open questions

- **Plugin strategy** (M6). WASM gives sandboxing and portability at some per-call cost.
- **Name casing**: `foxygnal` vs `Foxygnal` in the UI.
