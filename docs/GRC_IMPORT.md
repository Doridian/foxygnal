# GNU Radio `.grc` import and export

Import is described first; [Export](#export) (foxygnal → GRC 3.10 YAML) follows.

foxygnal converts GNU Radio Companion flowgraphs (the YAML `.grc` format of GNU Radio 3.8
and later, and the XML `.grc` of 3.7) built from **common blocks**. It is a starting point, not a full translation:
everything that doesn't convert exactly is reported, and nothing is dropped silently.

```sh
foxygnal import fm_rx.grc                 # writes fm_rx.fox.toml next to it
foxygnal import fm_rx.grc -o radio.fox.toml
foxygnal import fm_rx.grc -o -            # print to stdout
foxygnal import fm_rx.grc --audio-device  # audio_sink → sound-card audio_sink, not browser_audio
```

Warnings go to stderr and into a comment block at the top of the written file. The command
won't overwrite an existing file without `--force`, and never writes over a `.grc`.

In the browser, **Open** lists the project folder's `.grc` files next to the `.fox.toml` ones
(and **Upload** accepts a `.grc`). Opening one converts it into a new, unsaved graph and shows
the warnings in a notice (**Copy** puts them all on the clipboard); **Save** offers
`<name>.fox.toml` in the same folder. The `.grc` is never written.

The importer is the `foxygnal-grc` crate (`import(text, &Options) → {graph, warnings,
title}`), used by both the CLI and the server (`GET /api/import/{name}`, `POST /api/import`).

## Formats

The format is told from the content, not the name: a file that starts with `<` (an XML
declaration or `<flow_graph>`) is read as GNU Radio 3.7 XML, anything else as YAML. Both are
read into the same intermediate form, so the mapping table and the expression translation
below apply to both. The XML is parsed with [`roxmltree`](https://crates.io/crates/roxmltree)
(pure Rust, read-only, no dependencies); DTDs and entity declarations are refused.

What is specific to 3.7 files:

- The block type is `<key>`; the block name is its `id` param. `_enabled` is `True` / `1`
  (enabled), `False` / `0` (disabled) or `2` (bypassed); `_coordinate` `(x, y)` gives the
  position; `_rotation` and other `_…` bookkeeping params are ignored.
- The `options` block is one of the `<block>`s: its `title` becomes the title, and
  `generate_options` `hb` / `hb_qt_gui` is reported like a 3.8 hierarchical block. `wx_gui`
  needs nothing special.
- Connections are `<source_block_id>` / `<source_key>` → `<sink_block_id>` / `<sink_key>`.
  A numeric key is a stream port index; a named one (`strobe`, `print`, `pdus`) is a message
  port, reported and left out as in 3.8.
- **WX GUI** (3.7 only): `wxgui_fftsink2`, `wxgui_scopesink2`, `wxgui_waterfallsink2`,
  `wxgui_constellationsink2`, `wxgui_numbersink2`, `wxgui_histosink2` become Null Sinks with a
  note naming the foxygnal instrument to open, like the QT sinks. The WX variable widgets
  (`variable_slider`, `variable_text_box`, `variable_chooser`, `variable_check_box`,
  `variable_static_text`) become graph `[vars]`. `notebook` (WX tabs) and `qtgui_tab_widget`
  are layout and are skipped.
- 3.7 string parameters are usually written unquoted (`rtl=0`, `/tmp/out.wav`,
  `127.0.0.1`). As in GRC, a string parameter is evaluated as Python when it yields a string
  (`"rtl=" + str(0)`, a variable) and otherwise taken as written; this applies to YAML files
  too.
- GNU Radio 3.6 block ids still found in old XML files are renamed to their 3.7 equivalents
  first: `gr_sig_source_x`, `gr_noise_source_x`, `gr_throttle`, `gr_head`, `gr_null_sink`,
  `gr_file_source` / `_sink`, `gr_wavfile_source` / `_sink`, `gr_add_xx`, `gr_multiply_xx`,
  `gr_multiply_const_vxx`, `gr_complex_to_mag` / `_mag_squared` / `_float`,
  `gr_float_to_complex`, `gr_quadrature_demod_cf`, `gr_freq_xlating_fir_filter_xxx`,
  `blks2_rational_resampler_xxx`, `blks2_wfm_rcv`, `blks2_nbfm_rx`.
- Otherwise the 3.7 block ids and parameters the table below lists are the same as 3.8's
  (`analog_sig_source_x` with `type` `complex` / `float`, `blocks_throttle`, `osmosdr_source`,
  `qtgui_*` sinks, `firdes.WIN_*` windows…).

## What converts

- **Variables** (`variable`, `variable_qtgui_range` / `qtgui_range`, `…_entry`, `…_chooser`,
  `…_check_box`, `…_label`, `parameter`, 3.7's WX `variable_slider` / `_text_box` /
  `_chooser` / `_check_box` / `_static_text`) become graph `[vars]`, using their value. Python expressions are
  translated to foxygnal expressions: numbers (`2.4e6`, `100_000`, `0x10`), `+ - * /`, unary
  minus, parentheses, variable references (`samp_rate / decim` → `"$samp_rate / $decim"`),
  `math.pi` / `numpy.pi`, `int()` / `float()`. Each translation is checked against the value
  Python would compute: when they differ (`int()` that truncates, `//`, `%`) or there is no
  equivalent (`**`, `round()`, `math.sqrt()`…), the computed **value** is kept, with a
  warning. String variables (file names, device args) and `firdes.*` taps aren't graph
  variables (foxygnal variables are numbers); they're used where blocks refer to them.
- **Block parameters** go through the same translation, so references to variables survive.
- **Positions**: GRC coordinates × 1.25 become canvas positions.
- **Disabled** blocks are skipped (with their connections, as GRC does). **Bypassed** blocks
  with one input and one output are wired through; others are left out with a warning.
- **Virtual sinks / sources** are wired through by stream id.
- **Connections**: GRC port indices map to foxygnal port names. Message connections are
  reported and left out (no message ports yet).
- An input left unconnected because what fed it was left out is reported.

## Mapping

| GNU Radio | foxygnal | notes |
|---|---|---|
| `analog_sig_source_x` | `signal_source` | cos/sin → sine; square, triangle, saw, constant; start phase ignored, except phase π on square / triangle / saw (how the exporter writes them): GNU Radio's 0…amp range becomes amplitude amp/2, offset + amp/2 |
| `analog_noise_source_x`, `analog_fastnoise_source_x` | `noise_source` | Gaussian only |
| `blocks_throttle`, `blocks_throttle2` | `throttle` | |
| `blocks_head` | `head` | |
| `blocks_file_source` / `blocks_file_sink` | `file_source` / `file_sink` | no offset/length, no append |
| `blocks_wavfile_source` | `wav_source` | one node per channel used |
| `blocks_wavfile_sink` | `wav_sink` | 16-bit PCM or float; 2 channels → `f32_pair_to_c32` + c32 (stereo) WAV |
| `blocks_null_sink` | `null_sink` | one per input |
| `blocks_add_xx`, `blocks_multiply_xx`, `blocks_sub_xx`, `blocks_divide_xx` | `add`, `multiply`, `subtract`, `divide` | n inputs → a chain of n−1 |
| `blocks_multiply_const_xx` / `_vxx` | `gain` | real constants only |
| `blocks_add_const_vxx` / `_xx` / `_ff` / `_cc` | `add_const` | real constants only |
| `blocks_abs_xx` (float), `blocks_conjugate_cc` | `abs`, `conjugate` | |
| `blocks_nlog10_ff` | `log_power` (+ `gain`) | n·log10(x) + k = (n/20)·(10·log10(x²) + 20k/n): exact for x > 0; a `gain` of n/20 follows unless n = 20 |
| `blocks_skiphead`, `blocks_delay` | `skip`, `delay` | one port |
| `blocks_copy` | `valve` | `enabled` → open; drops while closed |
| `blocks_selector` | `selector` | 2–4 inputs, one output; `input_index` |
| `blocks_complex_to_mag` / `_mag_squared` / `_real` / `_imag` / `_arg` | `c32_to_f32` | part = magnitude / magnitude_squared / real / imag / phase |
| `blocks_complex_to_float` | `c32_to_f32` ×2 | real and imag nodes, for the outputs in use |
| `blocks_float_to_complex` | `f32_pair_to_c32` (`f32_to_c32` with only the real input) | |
| `low_pass_filter`, `high_pass_filter`, `band_pass_filter`, `band_reject_filter` | `fir_filter` | `decim` → a `decimator` after, `interp` → an `interpolator` before; Hamming/Blackman/Kaiser (β → attenuation), other windows → Hamming |
| `freq_xlating_fir_filter_xxx` | `freq_xlating_fir` | cutoff / transition / window from `firdes.low_pass(…)` / `low_pass_2(…)` (also through a variable), or a symmetric `complex_band_pass(…)`; other taps → a low-pass at 0.4 × the output rate, with a warning. `f` input → `f32_to_c32` first. Taps gain ignored |
| `root_raised_cosine_filter`; `fir_filter_xxx` / `interp_fir_filter_xxx` with `firdes.root_raised_cosine(…)` taps | `rrc_filter` | sps = rate / symbol rate, roll-off = α, span = ntaps / sps; interpolating by sps → `interpolate` mode (other interpolations aren't supported); `decim` → a `decimator` after |
| `fir_filter_xxx` / `interp_fir_filter_xxx` with `firdes.low_pass(…)` / `high_pass(…)` taps | `fir_filter` | like `low_pass_filter`; other taps aren't converted |
| `dc_blocker_xx` (`filter_dc_blocker_xx`) | `dc_blocker` | GNU Radio's length (samples) has no Hz equivalent: cutoff left at 10 Hz, with a warning |
| `hilbert_fc` | `hilbert` | taps, window |
| `iir_filter_*`, `iir_filter_xxx` | — | arbitrary IIR taps aren't converted (the IIR Biquad designs its own) |
| `rational_resampler_xxx` | `rational_resampler` | interp / decim; custom taps ignored |
| `analog_quadrature_demod_cf` | `fm_demod` | deviation from the gain written as `rate / (2*math.pi*dev)` (→ `dev`, also `…*dev/8.0` → `dev / 8.0`); an `analog_fm_deemph` fed only by it becomes `deemphasis` (50 / 75 µs) |
| `analog_wfm_rcv` (`_pll`) | `fm_demod` + `decimator` | 75 kHz, de-emphasis from `deemph_tau`; `_pll`: mono to both outputs |
| `analog_nbfm_rx` | `fm_demod` + `rational_resampler` | deviation = `max_dev`, output rate = `audio_rate` |
| `analog_am_demod_cf` | `am_demod` (+ `decimator`) | `audio_decim` ≠ 1 → a decimator after; filters and DC removal are foxygnal's |
| `digital_costas_loop_cc` | `costas_loop` | order, loop bandwidth; the frequency output is rad/sample in GNU Radio, Hz in foxygnal (warning) |
| `digital_symbol_sync_xx` | `clock_recovery` | type, TED (Gardner, Mueller–Müller, zero crossing), sps, loop bandwidth |
| `digital_binary_slicer_fb` | `binary_slicer` | |
| `digital_correlate_access_code_tag_xx` / `_bb` | `correlate_access_code` | bit string → hex when it's a whole number of nibbles; threshold |
| `blocks_short_to_float`, `blocks_float_to_short`, `blocks_interleaved_short_to_complex`, `blocks_uchar_to_float` | `i16_to_f32`, `f32_to_i16`, `ci16_to_c32`, `u8_to_f32` | scale = 32767 / GNU Radio's scale |
| `epy_block` written by the exporter | the original block | restored from its `# foxygnal-block:` line; other embedded Python blocks aren't converted |
| `analog_agc_xx` / `agc2` / `agc3` | `agc` | reference → target, max gain; rates → defaults (warning) |
| `analog_pwr_squelch_xx` | `squelch` | threshold; `gate` unsupported |
| `audio_sink` | `browser_audio` (`audio_sink` with `--audio-device`) | first channel only (two inputs → `f32_pair_to_c32` + stereo `browser_audio`); `pace` off when an SDR source paces the graph |
| `osmosdr_source`, `rtlsdr_source`, `soapy_*_source`, `soapy_source`, `uhd_usrp_source`, `iio_pluto_source` | `sdr_source` | device string guessed (`rtl=0` → `rtlsdr:`, `rtl_tcp=h:p` → `rtltcp:host=h,port=p`, `hackrf=0` → `hackrf:`, `driver=X` → `soapy:driver=X,…`, UHD → `soapy:driver=uhd,…`); frequency, rate, gain, AGC, ppm, bandwidth, antenna; always a warning to check it |
| `network_udp_source`, `blocks_udp_source` (3.7) | `udp_source` | port; header 0 → none, 1 → seq64; GNU Radio's `network_udp_source` listens on every interface, foxygnal's on 127.0.0.1 unless you set the host (a warning says so); `blocks_udp_source` keeps its `ipaddr` |
| `network_udp_sink`, `blocks_udp_sink` (3.7) | `udp_sink` | host, port, header; payload size + header bytes → payload |
| `network_tcp_source` | `tcp_source` | `server` → mode, host, port |
| `network_tcp_sink`, `blocks_tcp_server_sink` (3.7) | `tcp_sink` | mode (1 client, 2 server; the 3.7 block is a server), host, port |
| `qtgui_freq_sink_x`, `qtgui_time_sink_x`, `qtgui_waterfall_sink_x`, `qtgui_const_sink_x`, (`qtgui_sink_x`, `qtgui_number_sink`, …), 3.7's `wxgui_fftsink2`, `wxgui_scopesink2`, `wxgui_waterfallsink2`, `wxgui_constellationsink2`, `wxgui_numbersink2`, `wxgui_histosink2` | `null_sink` | foxygnal instruments are probes on wires: the warning says which one to open on the wire into it |
| `variable*`, `qtgui_range` / `entry` / `chooser`, `parameter` | `[vars]` | see above |
| `note`, `import`, `import_`, `notebook`, `qtgui_tab_widget` | — | nothing to convert |

Anything else is reported as `` `name` (gr_block_id) has no foxygnal equivalent yet `` and
left out with its connections. Stream types: `complex` → c32, `float` → f32, `short` → i16,
`byte` → u8 where the foxygnal block supports it; `int` and vectors (`vlen` > 1) are reported
and the block left out.

## Limitations

- 3.7 XML: only what the 3.8 table covers, plus the WX GUI sinks and widgets above. WX
  widgets other than variables (`wxgui_termsink`, forms) aren't converted; GRC 3.6 ids
  beyond the renames above aren't recognised.
- Python is evaluated only as far as the subset above: no conditionals, subscripts,
  comprehensions or arbitrary function calls. Untranslatable expressions are evaluated where
  possible and kept as values.
- foxygnal blocks take the rate from the stream, so GRC's per-block `samp_rate` parameters
  (filters, sinks) are ignored; a raw `blocks_file_source` has no rate, so keep the throttle or
  set the File Source's `sample_rate`.
- Hierarchical blocks (`pad_source` / `pad_sink`), embedded Python blocks (other than the
  exporter's placeholders), message ports, tagged-stream blocks and digital blocks beyond the
  table aren't converted.
- GUI layout (`gui_hint`), QT widgets other than variables, and `options` settings other
  than the title are ignored.

## Export

`foxygnal export` writes a GNU Radio Companion 3.10 flowgraph (YAML, `file_format: 1`) that
GRC opens, validates and runs. Like import, it never drops anything silently: every block
that doesn't map exactly is reported.

```sh
foxygnal export fm-live.fox.toml             # writes fm-live.grc next to it
foxygnal export fm-live.fox.toml -o radio.grc
foxygnal export fm-live.fox.toml -o -        # print to stdout
```

Warnings go to stderr. The command won't overwrite an existing file without `--force`.
In the browser, **Export to GNU Radio (.grc)…** in the file menu downloads `<name>.grc` and
lists the warnings in the same notice as import. The server endpoint is `POST /api/export`
(body: the graph as JSON, optional `?name=<graph>.fox.toml`) → `{grc, warnings, name}`; it
writes nothing. The library call is `foxygnal_grc::export(&spec, &ExportOptions) → {grc,
warnings, ids}`.

What gets written:

- An `options` block (`no_gui`, run options *prompt*, id and title from the file name).
- `[vars]` → `variable` blocks in a row at the top. Expressions are translated, not
  evaluated: `$samp_rate / 10` → `samp_rate / 10`, `2.4M` → `2.4e6`, `100 kHz` → `1e5`
  (units dropped, SI prefixes applied). Parameters that are expressions stay expressions in
  the block parameters.
- Block ids become GRC names: invalid characters → `_`, Python keywords, builtins, module
  names and `top_block` attributes get a suffix (`audio` → `audio_0`), so do names taken by
  a variable. A foxygnal block that becomes several GNU Radio blocks keeps its id on the main
  one and suffixes the others (`demod` → `demod_deemph`).
- Port names → indices; stream types `c32` → complex, `f32` → float, `i16` → short,
  `u8` → byte. Positions are the editor's, scaled to GRC's denser canvas.
- Outputs left unconnected that GRC requires connected get a `blocks_null_sink`.
- Rates: foxygnal blocks take their rate from the stream, GNU Radio blocks are told. The
  exporter follows the rate down the graph from each source (an expression such as
  `samp_rate / 10` where it can) and writes it into each block's `samp_rate`; where the rate
  isn't known it falls back to `samp_rate` or 48 kHz with a warning.
- `graph:NAME.fox.toml` blocks are flattened into the parent, with a warning; their
  parameters become variables named `<block>_<param>`. Without a loader (or when the file is
  missing) the block becomes a placeholder.
- `import math` is added (an `import_` block) when an expression uses `math.pi`.

### Mapping

| foxygnal | GNU Radio 3.10 | notes |
|---|---|---|
| `signal_source` | `analog_sig_source_x` | sine → `GR_SIN_WAVE` (float) / `GR_COS_WAVE` (complex); square / triangle / saw are 0…amp in GNU Radio, so amplitude 2A, offset O−A, phase π |
| `noise_source` | `analog_noise_source_x` | Gaussian; seed |
| `throttle` | `blocks_throttle2` | rate from the stream when the parameter is 0 |
| `head`, `skip`, `delay` | `blocks_head`, `blocks_skiphead`, `blocks_delay` | |
| `valve` | `blocks_copy` | `open` → enabled |
| `selector` | `blocks_selector` | |
| `file_source` / `file_sink` | `blocks_file_source` / `blocks_file_sink` | `sample_rate` / `center_freq` metadata lost |
| `sigmf_source` / `sigmf_sink` | `blocks_file_source` / `blocks_file_sink` on `.sigmf-data` | warning: no metadata (gr-sigmf isn't in-tree) |
| `wav_source` / `wav_sink` | `blocks_wavfile_source` / `blocks_wavfile_sink` | c32 (stereo) via `blocks_complex_to_float` / `float_to_complex`; i16 via converters |
| `audio_source`, `audio_sink`, `browser_audio` | `audio_source`, `audio_sink` | c32 → two channels |
| `null_sink` | `blocks_null_sink` | |
| `add`, `subtract`, `multiply`, `divide` | `blocks_add_xx`, `blocks_sub_xx`, `blocks_multiply_xx`, `blocks_divide_xx` | |
| `gain` | `blocks_multiply_const_xx` | |
| `add_const` | `blocks_add_const_vxx` | |
| `abs`, `conjugate` | `blocks_abs_xx`, `blocks_conjugate_cc` | |
| `log_power` | `blocks_complex_to_mag_squared` / `blocks_abs_xx` + `blocks_nlog10_ff` | |
| `c32_to_f32` | `blocks_complex_to_mag` / `_mag_squared` / `_real` / `_imag` / `_arg` | by `part` |
| `f32_to_c32`, `f32_pair_to_c32`, `c32_to_f32_pair` | `blocks_float_to_complex`, `blocks_complex_to_float` | the pair comes back as one `c32_to_f32` per output |
| `i16_to_f32`, `f32_to_i16`, `ci16_to_c32`, `u8_to_f32` | `blocks_short_to_float`, `blocks_float_to_short`, `blocks_interleaved_short_to_complex`, `blocks_uchar_to_float` | scale = 32767 / foxygnal's scale |
| `cu8_to_c32` | `blocks_deinterleave` → 2 × `blocks_uchar_to_float` → `blocks_float_to_complex` → `add_const` → `multiply_const` | (x − 127.5) / 127.5 |
| `fir_filter` | `low_pass_filter`, `high_pass_filter`, `band_pass_filter`, `band_reject_filter` | by `mode`; window → `window.WIN_*`, Kaiser β from the attenuation |
| `freq_xlating_fir` | `freq_xlating_fir_filter_xxx` | taps `firdes.low_pass(1, rate, cutoff, transition, window, β)` |
| `rrc_filter` | `root_raised_cosine_filter` | ntaps = span × sps; `interpolate` mode → interp = sps |
| `hilbert` | `hilbert_fc` | |
| `dc_blocker` | `dc_blocker_xx` | cutoff → moving-average length D (−3 dB at f·D ≈ 0.42), warning |
| `biquad` | `iir_filter_xxx` (one per stage) | RBJ coefficients computed at export (`oldstyle: False`); not re-imported |
| `rational_resampler` | `rational_resampler_xxx` | output rate → reduced interp / decim |
| `decimator`, `interpolator` | `rational_resampler_xxx` | come back as `rational_resampler` |
| `fm_demod` | `analog_quadrature_demod_cf` (+ `analog_fm_deemph`) | gain = rate / (2π·deviation) |
| `am_demod` | `analog_am_demod_cf` | DC removal time lost |
| `agc` | `analog_agc2_xx` | attack / decay → per-sample rates, warning |
| `squelch` | `analog_pwr_squelch_xx` | no hysteresis / averaging / ramp, warning |
| `sdr_source` / `sdr_sink` | `osmosdr_source` / `osmosdr_sink`, `soapy_plutosdr_source`, `soapy_custom_source` / `_sink` | by device string: `rtlsdr:` → `rtl=N` (bias tee, direct sampling, offset tuning), `rtltcp:` → `rtl_tcp=h:p`, `hackrf:` → `hackrf=N`, `soapy:driver=plutosdr` → the Pluto block, other `soapy:` → custom; `sim` / empty → osmosdr with empty args. Always a warning to check it |
| `udp_source` / `udp_sink` | `network_udp_source` / `network_udp_sink` | header `seq64` → 1 |
| `tcp_source` / `tcp_sink` | `network_tcp_source` / `network_tcp_sink` | mode; no reconnect |
| `costas_loop` | `digital_costas_loop_cc` | frequency output × rate / 2π (Hz, like foxygnal) |
| `clock_recovery` | `digital_symbol_sync_xx` | TED, sps, loop bandwidth |
| `binary_slicer` | `digital_binary_slicer_fb` | a non-zero threshold → `add_const` before |
| `correlate_access_code` | `digital_correlate_access_code_tag_xx` | hex → bit string |

### Placeholders

A block with no GNU Radio equivalent (decoders such as `adsb_decoder`, `rds_decoder`,
`pocsag_decoder`, `message_log`, demodulators like `fm_stereo`, plugin blocks, a sub-graph
that can't be loaded) is never dropped. It becomes an **enabled `epy_block`** (embedded
Python block) with the same ports and stream types, named after the original:

- it consumes its inputs and produces nothing (a source sleeps instead), so the flowgraph
  still loads, validates and runs in GRC, and the rest of the graph keeps working;
- its comment says what it stands for, and a warning names it;
- its code carries a `# foxygnal-block: {...}` line with the original kind and parameters,
  so importing the `.grc` back restores the block exactly.

An enabled stand-in was chosen over a disabled block because GRC drops disabled blocks'
connections and then rejects the neighbours' unconnected ports; replacing the block with
something real is up to you.

### Not exact

These don't survive a round trip (the export round-trip tests list them): file / UDP / TCP
source `sample_rate` metadata, UDP host, TCP reconnect settings, throttle 0 (follows the
stream) → the actual rate, `rational_resampler` output rate → interp / decim,
`decimator` / `interpolator` → `rational_resampler`, AGC attack / decay, squelch hysteresis,
averaging and ramp, DC blocker cutoff, non-Kaiser filter attenuation, AM demod DC time,
valve `when_closed`, browser audio `pace`, `binary_slicer` threshold, `log_power` options,
biquads (IIR taps aren't imported), complex `add_const` constants, hierarchy (flattened).

Constant inputs (`const_inputs`) export as a Constant Source (`analog_const_source_x`) wired
to the input, and import folds a Constant Source back into a constant on each input it feeds.
A wire into a setting (`block.@param`) has no GNU Radio form: it is left out with a warning,
and the setting keeps its starting value.

### Verification

Block ids, parameter names and enum values were checked against the GNU Radio 3.10 block
definitions (`grc/*.block.yml`, maint-3.10); the table of what's used is in
`crates/foxygnal-grc/src/export/defs.rs`, and debug builds assert every exported block
against it. Numeric conventions (waveform ranges, IIR coefficient order, short/complex
scaling, DC blocker response) were checked against a GNU Radio 3.10 runtime.

- `scripts/grc-check.py DEFS_DIR FILE.grc…` validates exported files against a directory of
  GNU Radio block YAML (ids, parameters, enum options, port counts and types).
- `scripts/grcc-check.sh` exports every example and compiles each with `grcc` when GNU Radio
  is installed (it skips otherwise).
- `cargo test -p foxygnal-grc --test export` exports every example and import fixture,
  checks the YAML, imports it again and compares blocks, parameters and connections.
