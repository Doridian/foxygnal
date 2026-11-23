# foxygnal

A fast, friendly flowgraph tool for software-defined radio, written in Rust, with a browser UI.

Think GNU Radio Companion, but:

- **Rust all the way down.** The engine and every block are Rust: no Python glue in the
  hot path, low memory use, and it runs on a Raspberry Pi.
- **Browser UI.** `foxygnal serve`, then open `http://127.0.0.1:7355`. It listens on localhost
  only unless you explicitly opt in.
- **Easy to use.** Sensible defaults, sample rates that propagate along the graph instead of
  being typed in ten places, readable errors on the block that caused them, and units with SI
  prefixes (`2.4M`, `-30 dB`).
- **See your signal anywhere.** Click any wire to attach an oscilloscope, spectrum, waterfall
  or constellation view while the graph runs, with no rewiring and no restart.

> Status: **M2.** Build, run, edit live and save graphs from the browser, and click any wire
> to watch it on a scope, spectrum, waterfall, constellation or level meter. Ten basic
> blocks; a real DSP toolbox comes next. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Quick start

```sh
# Engine + CLI
cargo build --release
./target/release/foxygnal blocks                            # list blocks
./target/release/foxygnal run examples/noisy-tone.fox.toml  # headless run, writes noisy-tone.c32
./target/release/foxygnal bench bench/fm-receiver-sim.fox.toml  # CPU, memory, items/s for 10 s

# UI
(cd ui && npm install && npm run build)
./target/release/foxygnal serve                             # http://127.0.0.1:7355

# Single binary with the UI inside (build the UI first)
cargo build --release --features embed-ui
```

**Listening** needs nothing extra: the `browser_audio` block plays in the browser tab that
pressed Run (try the "Live FM radio" template). Build with `--release` for real-time graphs;
debug builds are too slow for a 2.4 MS/s receiver.

**Native audio** (sound card sink and source) is behind the `audio` cargo feature, off by default
because on Linux it needs the ALSA headers (`libasound2-dev` on Debian/Ubuntu, `alsa-lib-devel`
on Fedora, `alsa-lib` on Arch). A plain `cargo build` needs no C libraries.

```sh
cargo run -p foxygnal --features audio -- serve
cargo build --release --features audio,embed-ui
```

`serve` options: `--project-dir DIR` is where the browser opens and saves `.fox.toml` files
(default: the current directory). `--allow-tx` allows graphs that transmit (each run still
has to be armed). `--allow-remote` permits a non-loopback `--bind` and
prints a URL with a random access token; put a TLS reverse proxy in front.

### SDR hardware

The **Devices** list at the top of the palette shows what the engine found (refresh to search
again); **Add** puts an SDR Source for that device on the canvas. A **Simulated FM band**
(`sim:`) is always there, so you can try everything without hardware. Network receivers that
can't be discovered are typed into the source's *Device* field, e.g.
`rtltcp:host=10.0.0.5,port=1234`. An empty device means the first one found.

**Transmitting is off by default.** A graph with an SDR Sink (TX) only runs when the engine
was started with `foxygnal serve --allow-tx` *and* you confirm "Arm TX and run" for that run
(headless: `foxygnal run --arm-tx graph.fox.toml`). TX is disarmed when the run stops.
Transmitting needs a licence and must be legal where you are.

### Recording and playback

Select any wire while the graph runs and press **Record to SigMF** in the inspector: the engine
writes every sample (it never drops any; a slow disk slows the graph instead) to
`recordings/<graph>-<wire>-<UTC time>.sigmf-{meta,data}` in the project directory, with the
rate, centre frequency, device, and the stream's tags (retunes become capture segments,
overflows and other events annotations). The **Recordings** list in the palette shows them;
**Add** puts a SigMF Source for one on the canvas, which plays the tags back too.

UI development with hot reload: run `foxygnal serve` in one terminal and `npm run dev` in
`ui/` in another, then open the Vite URL (`http://127.0.0.1:5173`). It proxies `/api` to the engine.

## Layout

```
crates/
  foxygnal-core     stream types, ring buffers, Block trait, registry, graph spec, scheduler
  foxygnal-blocks   standard block library
  foxygnal-sdr      SDR device API, drivers, discovery, simulator, TX arm switch
  foxygnal-grc      GNU Radio .grc import
  foxygnal-server   HTTP/WebSocket server (axum) for the UI
  foxygnal          the CLI binary (serve / run / bench / import / blocks)
ui/                 Svelte 5 + Vite + Svelte Flow browser editor
examples/           *.fox.toml flowgraphs
bench/              graphs for `foxygnal bench` (docs/PERFORMANCE.md)
docs/               plans and design notes
```

## Docs

- [Architecture](docs/ARCHITECTURE.md): runtime model, buffers, probes, protocol, security
- [Roadmap](docs/ROADMAP.md): milestones M0–M6
- [Performance](docs/PERFORMANCE.md): `foxygnal bench`, Raspberry Pi builds and targets
- [Blocks](docs/BLOCKS.md): block catalog, implemented and planned
- [GRC import](docs/GRC_IMPORT.md): `foxygnal import flowgraph.grc`, the block mapping and its limits
- [UI](docs/UI.md): UI concept and interaction principles

## License

[AGPL-3.0-or-later](LICENSE). foxygnal is served over the network, so if you run a modified
version for others, AGPL §13 requires offering them its source. The UI links to the source
for this reason.
