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

# UI
(cd ui && npm install && npm run build)
./target/release/foxygnal serve                             # http://127.0.0.1:7355

# Single binary with the UI inside (build the UI first)
cargo build --release --features embed-ui
```

`serve` options: `--project-dir DIR` is where the browser opens and saves `.fox.toml` files
(default: the current directory). `--allow-remote` permits a non-loopback `--bind` and
prints a URL with a random access token; put a TLS reverse proxy in front.

UI development with hot reload: run `foxygnal serve` in one terminal and `npm run dev` in
`ui/` in another, then open the Vite URL (`http://127.0.0.1:5173`). It proxies `/api` to the engine.

## Layout

```
crates/
  foxygnal-core     stream types, ring buffers, Block trait, registry, graph spec, scheduler
  foxygnal-blocks   standard block library
  foxygnal-server   HTTP/WebSocket server (axum) for the UI
  foxygnal          the CLI binary (serve / run / blocks)
ui/                 Svelte 5 + Vite + Svelte Flow browser editor
examples/           *.fox.toml flowgraphs
docs/               plans and design notes
```

## Docs

- [Architecture](docs/ARCHITECTURE.md): runtime model, buffers, probes, protocol, security
- [Roadmap](docs/ROADMAP.md): milestones M0–M6
- [Blocks](docs/BLOCKS.md): block catalog, implemented and planned
- [UI](docs/UI.md): UI concept and interaction principles

## License

[AGPL-3.0-or-later](LICENSE). foxygnal is served over the network, so if you run a modified
version for others, AGPL §13 requires offering them its source. The UI links to the source
for this reason.
