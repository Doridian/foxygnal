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
debug builds are too slow for a 2.4 MS/s receiver. The "FM stereo + RDS" template
(`examples/fm-stereo-rds.fox.toml`) plays a station in stereo and decodes its RDS: station
name, RadioText, programme type and clock time, shown in the Messages instrument.
Decoder templates: "ADS-B aircraft decoder" (`examples/adsb.fox.toml`: ADS-B and the other
Mode S replies, Comm-B, Mode A/C on request), "UAT aircraft and weather decoder"
(`examples/uat.fox.toml`: 978 MHz ADS-B and FIS-B text weather, US), "POCSAG pager
decoder" (`examples/pocsag.fox.toml`), "FLEX pager decoder" (`examples/flex.fox.toml`, the
1600/3200/6400 bps paging used across North America), "APRS packet decoder"
(`examples/aprs.fox.toml`), "AIS ship decoder" (`examples/ais.fox.toml`, both AIS channels
at once), "ACARS aircraft datalink" (`examples/acars.fox.toml`), and, in the spirit of rtl_433, "ISM
sensors at 433 MHz" (`examples/ism-433.fox.toml`: weather stations, thermometers, tire
pressure sensors, door sensors and doorbells) and "ERT meters and 915 MHz sensors"
(`examples/ism-915.fox.toml`), plus "GOES DCS" (`examples/goes-dcs.fox.toml`: the
GOES Data Collection System relay at 1679.9 MHz, river gauges and weather stations bursting
at 300 and 1200 bit/s, decoded many at once) and its no-hardware twin "GOES DCS
(simulated)" (`examples/goes-dcs-sim.fox.toml`).

**Satellites.** The Satellite Tracker follows a satellite from its two-line elements (SGP4,
from a file or pasted in; nothing is downloaded) and steers a channel filter through the
Doppler shift of a pass: "Satellite Doppler tracking" (`examples/sat-doppler.fox.toml`,
Meteor-M at 137.9 MHz). The palette's Passes section lists when the satellites come over,
and the tracker's ground track shows on the Map.

**Maps.** The Map instrument plots the positions any decoder reports (`lat=… lon=…`):
ADS-B aircraft, AIS ships, APRS stations and objects, each with a trail, a heading arrow and
a label, merged with the identity and static data that come without a position (a flight
number, a ship's name), fading out as they go quiet. Click one for everything it sent. The
base map is a built-in Natural Earth world map, so it works offline and nothing leaves your
computer; map tiles from an OpenStreetMap-style server are an opt-in setting, as is a
receiver position for range rings (kept in the browser).

**Pictures.** The SSTV decoder (Robot, Martin, Scottie, Wraase and PD modes, from the VIS
header, with slant correction) draws pictures in the Image instrument as they arrive and
saves each one as a PNG with an Image Sink. Templates: "SSTV pictures" (the ISS on 145.8 MHz,
`examples/sstv.fox.toml`) and "SSTV from a recording" (a WAV file, `examples/sstv-wav.fox.toml`).

**Digital TV (DATV).** A DVB-S modulator and demodulator (EN 300 421, the mode of amateur
digital TV on 23 cm and QO-100) turn an MPEG transport stream into QPSK and back: Viterbi,
Reed-Solomon and the rest of the DVB FEC chain, with the code rate, carrier and phase found
automatically, at several MBd on one core. Templates: "DATV loopback (DVB-S)" (a TS file
through a noisy channel, no hardware, `examples/datv-loopback.fox.toml`) and "DATV receiver
(DVB-S)" (an SDR to a TS over UDP for ffplay or VLC, `examples/datv-rx.fox.toml`).

**Digital TV (DVB-T).** A DVB-T modulator and demodulator (EN 300 744, terrestrial TV in
Europe, Australia and much of the world): 2k and 8k OFDM, QPSK to 64-QAM, every code rate and
guard interval, with pilots and the TPS. The demodulator finds the mode, guard, timing,
carrier and sample clock offsets by itself, reads the constellation, code rate and cell id
from the TPS, estimates the channel from the scattered pilots and decodes an 8 MHz multiplex
in real time on one core. Templates: "DVB-T loopback" (`examples/dvbt-loopback.fox.toml`)
and "DVB-T receiver" (an SDR at 9.14 MS/s, e.g. a PlutoSDR or HackRF, to a TS over UDP,
`examples/dvbt-rx.fox.toml`).

**Digital TV (ATSC).** An ATSC 1.0 modulator and demodulator (A/53 8VSB, terrestrial TV in
North America and South Korea). The demodulator finds the pilot, segment and field syncs, the
sample clock and an inverted spectrum by itself, cancels multipath with a decision-feedback
equalizer trained on the field syncs, and decodes a 19.39 Mbit/s channel in real time on one
core from any rate ≥ 6.25 MS/s (an SDR at 12 MS/s, e.g. a PlutoSDR). It is bit-exact
against GNU Radio's gr-dtv in both directions. Templates: "ATSC loopback"
(`examples/atsc-loopback.fox.toml`) and "ATSC receiver" (`examples/atsc-rx.fox.toml`). The
Video instrument plays the MPEG-2 video with its AC-3 sound, and picks between subchannels
and sound tracks.

**Weather satellite links (CCSDS).** The link layer that Meteor-M LRPT, Metop AHRPT and GOES
HRIT share: a deframer that finds the sync marker through any phase the carrier loop locked
to, decodes the convolutional code (Viterbi), removes the randomizer and corrects the
frames with Reed-Solomon (checked against libfec and GNU Radio), then reassembles CCSDS space
packets per virtual channel. Frames come out as `.cadu` data, so recordings from other tools
play straight into it. The satellites' own decoders build on it. Template: "CCSDS loopback"
(`examples/ccsds-loopback.fox.toml`).

**Watching digital TV.** The Video instrument plays an MPEG transport stream on any `u8` wire (a DVB
demodulator's output, or a `.ts` file): it lists the programs with their names, plays the one
you pick with its sound (choosing between sound tracks by language), and shows codecs,
resolution, frame rate, bit rates and errors. H.264, HEVC and AAC (ADTS or LATM) are decoded
by the browser (WebCodecs); MPEG-2 video and MP2 and AC-3 sound (5.1 downmixed to stereo),
which most DVB and ATSC services use, by foxygnal's own decoders compiled to WebAssembly. Template: "Video from a TS file"
(`examples/ts-video.fox.toml`).

**AI assistants (MCP).** foxygnal is also a [Model Context Protocol](https://modelcontextprotocol.io)
server. An assistant can look up blocks, build and check graphs, run them, and measure what's
on a wire: the signals in a spectrum, levels, decoded messages, pictures. With
`foxygnal serve` running, point it at `http://127.0.0.1:7355/api/mcp`. It then works on the
graph open in your browser: its edits appear live, and Undo takes them back. Or let the
assistant start `foxygnal mcp --project-dir <dir>` itself (stdio, with a session of its
own). For example, with Claude Code:

```sh
claude mcp add --transport http foxygnal http://127.0.0.1:7355/api/mcp   # shares the browser's graph
claude mcp add foxygnal -- foxygnal mcp --project-dir ~/radio            # headless
```

Nothing over MCP can transmit: a graph that transmits must be armed by a person in the
browser. See "Assistant: the MCP server" in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

**Settings ⇄ inputs.** Any live number setting can be driven by a wire (`to = "channel.@offset"`;
"⇢ input" in the inspector), and any stream input can be a constant (`const_inputs = { b = 1.5 }`).
The "Auto-tuning FM" template wires a peak finder into a channel filter's offset, so it latches
onto the strongest station in the band.

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

### Plugins

Blocks can also come from **plugins**: WebAssembly components, sandboxed (no files or network,
a memory limit and a time limit per call), written in Rust with `foxygnal-plugin-sdk` and built
with `cargo build --target wasm32-unknown-unknown`. Drop the `.wasm` into `plugins/` in the
project directory or `~/.local/share/foxygnal/plugins` (or pass `--plugins DIR`) and its blocks
appear in the palette as `<plugin>:<kind>`. See [docs/PLUGINS.md](docs/PLUGINS.md).

UI development with hot reload: run `foxygnal serve` in one terminal and `npm run dev` in
`ui/` in another, then open the Vite URL (`http://127.0.0.1:5173`). It proxies `/api` to the engine.

## Layout

```
crates/
  foxygnal-core     stream types, ring buffers, Block trait, registry, graph spec, scheduler
  foxygnal-blocks   standard block library
  foxygnal-sdr      SDR device API, drivers, discovery, simulator, TX arm switch
  foxygnal-grc      GNU Radio .grc import and export
  foxygnal-server   HTTP/WebSocket server (axum) for the UI
  foxygnal-plugin   block plugins: WebAssembly components run with wasmtime
  foxygnal-plugin-sdk  write plugins (built for wasm32)
  foxygnal          the CLI binary (serve / run / bench / import / export / blocks)
ui/                 Svelte 5 + Vite + Svelte Flow browser editor
examples/           *.fox.toml flowgraphs
plugins/            example and test plugins (a wasm32 workspace of its own)
wit/                the plugin interface (WIT)
bench/              graphs for `foxygnal bench` (docs/PERFORMANCE.md)
docs/               plans and design notes
```

## Docs

- [Architecture](docs/ARCHITECTURE.md): runtime model, buffers, probes, protocol, security
- [Roadmap](docs/ROADMAP.md): milestones M0–M6
- [Performance](docs/PERFORMANCE.md): `foxygnal bench`, Raspberry Pi builds and targets
- [Blocks](docs/BLOCKS.md): block catalog, implemented and planned
- [Plugins](docs/PLUGINS.md): writing, building and installing block plugins, sandbox, performance
- [GRC import and export](docs/GRC_IMPORT.md): `foxygnal import flowgraph.grc`,
  `foxygnal export graph.fox.toml`, the block mappings and their limits
- [UI](docs/UI.md): UI concept and interaction principles

## License

[AGPL-3.0-or-later](LICENSE). foxygnal is served over the network, so if you run a modified
version for others, AGPL §13 requires offering them its source. The UI links to the source
for this reason.
