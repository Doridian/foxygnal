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

- [x] Benchmarks (`criterion`) per block plus whole-graph throughput, run in CI on demand
- [x] Double-mapped ring buffers (contiguous slices)
- [x] Pooled scheduler; dedicated threads only where needed
- [x] SIMD kernels for FIR, FFT glue, complex multiply, and type conversion
- [x] Idle-CPU budget: a paused or throttled graph should use ≈0% CPU
- [ ] Targets (to validate): 2.4 MS/s FM receiver under 25% of one Pi 4 core; engine RSS
      under 50 MB
      — harness ready, to validate on a Pi: `foxygnal bench bench/fm-receiver-sim.fox.toml`,
      aarch64 CI build ([PERFORMANCE.md](PERFORMANCE.md))

## M6: Ecosystem

- [x] Out-of-tree block plugins: WebAssembly components (WIT `foxygnal:block`, wasmtime), with
      a Rust SDK, an example plugin, sandboxing (no WASI, memory limit, call deadline) and
      benchmarks against native blocks ([PLUGINS.md](PLUGINS.md)). Not yet: reloading plugins
      without a restart, an on-disk compile cache
- [x] Digital modes: FSK/PSK modems, clock recovery, packet framing, a few decoders (ADS-B, POCSAG, …)
      — clock recovery (Gardner / M&M), Costas loop, slicer, PSK demapper, 2-FSK, access
      code + packet decoder, ADS-B (DF17/18 only, no Mode S phase correction or local CPR);
      decoded messages are text lines shown by the generic Messages instrument. Not yet:
      POCSAG, AX.25/HDLC, FEC, GFSK/GMSK, multi-level FSK
- [x] Recording and playback workflow (SigMF-annotated captures)
- [x] GNU Radio `.grc` import, common blocks only (`foxygnal import`, or open a `.grc` in the
  browser; [GRC_IMPORT.md](GRC_IMPORT.md))

## M7: Catalog and broadcast

Goal: the blocks people reach for first, and a broadcast FM receiver that shows off.

- [x] Math, flow and filter blocks planned since M3: subtract, divide, conjugate, add const,
      abs, log power; skip, selector, valve, delay; RRC, IIR biquad, DC blocker, Hilbert
- [x] Network I/O: UDP and TCP sources and sinks (ZeroMQ skipped: no mature pure-Rust PUB/SUB)
- [x] Broadcast FM stereo and RDS (PS, RadioText, clock), stereo browser audio, "FM stereo + RDS" template
- [x] Easier wire selection in the editor; resamplers place tags exactly

## M8: More decoders and polish

- [x] Stereo sound-card audio (`audio_sink` / `audio_source` with `type = c32`); recording
      timelines with annotation markers, and SigMF playback from any sample (`start`)
- [x] POCSAG pager decoder (512/1200/2400 Bd, BCH repair); AFSK 1200 demodulator, HDLC deframer,
      AX.25 / APRS decoding; "POCSAG" and "APRS" templates
- [x] `.grc` import reads GNU Radio 3.7 XML too (WX GUI sinks and widgets included), and maps
      the M7 catalog and network blocks
- [x] Settings as inputs (`block.@param`, any live number setting, sample and hold per `work`
      call) and inputs as constants (`const_inputs`); `peak_frequency` and the auto-tuning FM
      template (`fm-autotune`)
- [x] `.grc` export (GRC 3.10 YAML): `foxygnal export`, `POST /api/export` and a file-menu
      action; blocks without an equivalent become placeholder Python blocks that re-import
      exactly

## M9: Pictures

Goal: watch an SSTV picture being drawn in the browser, from the ISS or a recording.

- [x] Pictures on `u8` wires: self-describing row records (`core::raster`), the **Image**
      instrument (live drawing, a strip of earlier pictures, save as PNG), and `image_sink`
      (one PNG per picture, headless)
- [x] SSTV decoder: Robot 36/72, Martin M1/M2, Scottie S1/S2/DX, Wraase SC2-180, PD
      50/90/120/160/180/240/290, from the VIS header or a fixed mode; the line clock follows
      the sync pulses (slant correction, with the clock offset reported in ppm); "SSTV" (ISS,
      145.8 MHz) and "SSTV from a recording" templates
- [ ] Weather fax (HFFAX / WEFAX): 60/90/120/240 LPM, IOC 288/576, start/stop tones and
      phasing; the stations on 3–17 MHz send charts around the clock
- [ ] NOAA APT: sync A/B, telemetry wedges, both channels, false colour. The NOAA POES
      satellites were switched off in 2025, so this is for archived recordings
- [ ] Meteor-M LRPT (Meteor-M N2-3 / N2-4, 137 MHz): QPSK / OQPSK 72 kBd, Viterbi r=1/2,
      CCSDS frames, Reed-Solomon, the JPEG-like image codec; three channels in colour
- [ ] Hellschreiber (Feld-Hell): text as pictures
- [ ] An SSTV *encoder* (PNG in, audio out), for TX and for tests that don't need a radio

## M10: Assistant (MCP server)

Goal: ask an AI assistant "build me a receiver for X" or "what is on this wire?" and have it
do the work through the same engine the UI uses.

- [x] `foxygnal mcp`: a Model Context Protocol server over stdio (for desktop assistants and
      coding agents), plus the same tools at `/api/mcp` (streamable HTTP) on a running
      `foxygnal serve`, under the same loopback, Host/Origin and token rules as the rest of
      the API
- [x] Tools for building graphs: list and search blocks (descriptors, help text, ports,
      settings), list examples and templates, read, check (the same type-checked errors
      as the editor, naming both endpoints, plus unconnected inputs), create and edit
      `.fox.toml` graphs (a converter is inserted when a wire's ends disagree and exactly
      one fits), import and export `.grc`
- [x] Tools for signals: run and stop, change live settings, read per-block and per-wire stats,
      attach a probe and get a *summary* rather than raw samples (strongest peaks and their
      levels, noise floor, occupied bandwidth, level meter, constellation: PSK order, EVM/MER,
      symbol rate), read decoded messages, fetch the current picture as a PNG, list SDR
      devices, record a short SigMF capture
- [x] Resources: the block catalog, docs, the open graph, examples; prompts for common jobs
      ("build a receiver", "find signals in this band", "decode this recording")
- [x] Safety: transmitting is never possible over MCP: TX blocks stay disarmed until a person
      arms them in the UI. File tools stay inside the project directory. Edits made through
      MCP show up live in an open editor and can be undone there

## M11: Everyday decoders

Goal: the narrowband signals within reach of an RTL-SDR and a simple antenna, each with a
template.

- **Paging and telemetry**
  - [x] FLEX (1600/3200/6400, 2- and 4-level FSK): the paging system used locally, which M8's
        POCSAG test found instead. Speed and polarity from Sync 1, phases A–D, BCH(31,21)
        repair, short and long addresses, alphanumeric / numeric / tone / secure / binary
        pages, fragments marked; "FLEX" template
  - [ ] ISM-band sensors (433/868/915 MHz): an OOK/FSK pulse demodulator, PWM/PPM/Manchester
        slicers, and a table of device formats in the spirit of rtl_433 (weather stations, tire
        pressure sensors, doorbells, power meters)
  - [ ] Radiosondes: Vaisala RS41 (GFSK 4800 Bd, Reed-Solomon, positions and PTU) first,
        then Graw DFM and Meteomodem M10/M20
- **Maritime and aviation**
  - [ ] AIS (161.975 / 162.025 MHz, GMSK 9600 Bd HDLC; reuses M8's deframer): ship positions
        and static data, both channels at once
  - [ ] ACARS (AM, 2400 bit/s MSK) and VDL Mode 2 (D8PSK, 31.5 kBd, AVLC)
  - [ ] Mode S beyond DF17: Mode A/C, DF4/5/20/21 with Comm-B (BDS 4,0 / 5,0 / 6,0), local
        CPR, error correction; and UAT on 978 MHz (US)
  - [ ] NAVTEX (518 / 490 kHz, SITOR-B 100 Bd FSK) and DSC
- **Ham radio**
  - [ ] CW (Morse) decoder with automatic speed tracking
  - [ ] RTTY (45.45 Bd Baudot, any shift), PSK31/63 (BPSK, Varicode)
  - [ ] FT8 / FT4 / WSPR: time slots, Costas sync, LDPC (174,91) and Fano decoding, a band
        activity view
  - [ ] 9600 Bd G3RUH packet (AX.25 over scrambled GFSK) for amateur satellites
- **Tones and signalling**
  - [ ] DTMF, CTCSS and DCS detection (squelch-able), SELCAL
  - [ ] EAS / SAME (NOAA Weather Radio, 162.4–162.55 MHz): AFSK 520.83 Bd headers
  - [ ] Time signals: DCF77, MSF, WWVB, JJY (time and date as messages)

## M12: Digital voice and trunking

Goal: see who is talking on digital voice channels; hear the open ones.

- [ ] M17 (4FSK 4800 Bd, open Codec 2 voice): full decode with audio, Codec 2 in Rust
- [ ] DMR (Tier II / III): sync, colour code, slot types, talkgroups and IDs, data, Capacity
      Plus / Connect Plus control channels
- [ ] P25 Phase 1 (C4FM / CQPSK): NID, link control, trunking control channel (TSBK); voice
      depends on the codec question (IMBE)
- [ ] NXDN, D-STAR (headers and slow data), Yaesu System Fusion: framing and metadata
- [ ] TETRA: π/4-DQPSK, the broadcast and system information of unencrypted networks
- [ ] LoRa: chirp spread spectrum demodulation (SF7–12, all bandwidths, header and CRC),
      then Meshtastic packets on the public default channel
- [ ] A trunking follower: a control channel decoder that retunes a voice channel's
      `freq_xlating_fir` through a wired setting (M8's settings as inputs)

## M13: Digital broadcast audio

Goal: listen to digital radio stations with an RTL-SDR.

- [ ] DAB / DAB+ (fits the RTL-SDR's 2.048 MS/s exactly): OFDM sync, coarse and fine
      frequency correction, π/4-DQPSK, time and frequency deinterleaving, Viterbi, energy
      dispersal; the FIC (ensemble and service names), MP2 audio for DAB, Reed-Solomon
      superframes for DAB+ (audio per the codec question); dynamic label text, and MOT
      slideshow pictures through M9's pictures
- [ ] HD Radio (NRSC-5, US): the digital sidebands of FM stations, station information and
      artist/title, album art and traffic maps as pictures
- [ ] DRM / DRM+ (HF and VHF digital radio)

## M14: Television

Goal: what the RTL-SDR's chip was made for, and the analog TV that came before.

- [ ] Analog TV: AM vestigial sideband video, line and frame sync, PAL / NTSC / SECAM
      timing; black and white at RTL-SDR rates (a reduced horizontal resolution at 2.4–3.2
      MS/s), full resolution and colour (PAL and NTSC chroma) with a HackRF, Airspy or Pluto.
      Sound carrier as an audio output. Analog FM video too: 5.8 GHz FPV drone cameras and
      amateur ATV
- [ ] A **Video** instrument and a frame-level probe (see open questions)
- [ ] DVB-T: OFDM 2k/8k, channel estimation from pilots, QPSK/16/64-QAM, inner
      deinterleaving, then the DVB-S FEC chain (Viterbi, outer deinterleaving, Reed-Solomon
      (204,188), done) → MPEG transport stream. Needs 9.14 MS/s (HackRF, Airspy, Pluto): in SDR mode an RTL-SDR tops out near
      3.2 MS/s and its DVB-T demodulator isn't reachable through librtlsdr
- [ ] MPEG-TS handling: PAT/PMT/SDT (channel names), EIT (programme guide), PES extraction,
      a TS output over UDP for VLC / ffplay, and H.264 video in the browser via WebCodecs
- [ ] ISDB-T one-segment (Japan, South America): 430 kHz wide, so within an RTL-SDR's
      reach, with H.264 video
- [x] DVB-S (EN 300 421), the mode of amateur DATV: `dvbs_modulator` and
      `dvbs_demodulator` (TS in and out on a `u8` wire, whole 188-byte packets), timing,
      carrier and a 4th-power frequency search, automatic code rate, puncturing phase and
      phase ambiguity; "DATV loopback" and "DATV receiver" templates
- [x] The DVB FEC chain, shared with DVB-T (`blocks/src/dvb/`): energy dispersal,
      RS(204,188), Forney interleaver, K = 7 convolutional code with puncturing, soft
      Viterbi, packet sync
- [ ] DVB-S2 (most of QO-100 now): LDPC + BCH, π/2-BPSK to 32APSK, PL headers
- [ ] Later: ATSC 1.0 (8VSB), DVB-T2

## Open questions

- ~~**Plugin strategy** (M6).~~ Decided: WASM components for out-of-tree blocks (sandboxed,
  portable, one file for every platform); blocks that turn out performance-critical move into
  `foxygnal-blocks` (native). Rust dylibs were rejected (no stable ABI, no sandbox).
- ~~**Name casing**.~~ Decided: always `foxygnal`, all lowercase, in the UI, docs and
  messages (environment variables stay `FOXYGNAL_*`).
- **Proprietary and patented codecs.** DAB+ and HD Radio carry HE-AAC (HD Radio a variant of
  it), DMR / D-STAR / YSF / NXDN voice is AMBE+2, P25 voice is IMBE. Options: decode
  framing and metadata natively and leave voice to a plugin; link a system library at run time
  (as with SoapySDR); or implement the codec where patents have expired (IMBE). Undecided;
  needed by M12 and M13.
- **Video in the browser.** WebCodecs decodes H.264 (and HEVC, AV1 where supported) but not
  MPEG-2, which most DVB-T and DVB-S services still use. Options: H.264 only in the browser,
  with MPEG-2 handed to an external player over UDP; or a software MPEG-2 decoder (in the
  engine, or WASM in the browser). Undecided; needed by M14.
- **Picture frames vs rows.** M9 sends pictures as row records on `u8` wires, which fits
  pictures that take seconds to minutes. Analog TV needs 25–30 frames a second of up to
  720×576: whether rows on `u8` still fit (about 10 MB/s at full PAL resolution), or video
  needs a frame-level probe that drops whole frames when behind, is to be measured in M14.
