# GNU Radio `.grc` import

foxygnal converts GNU Radio Companion flowgraphs (the YAML `.grc` format of GNU Radio 3.8
and later) built from **common blocks**. It is a starting point, not a full translation:
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

## What converts

- **Variables** (`variable`, `variable_qtgui_range` / `qtgui_range`, `…_entry`, `…_chooser`,
  `…_check_box`, `parameter`) become graph `[vars]`, using their value. Python expressions are
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
| `analog_sig_source_x` | `signal_source` | cos/sin → sine; square, triangle, saw, constant; start phase ignored |
| `analog_noise_source_x`, `analog_fastnoise_source_x` | `noise_source` | Gaussian only |
| `blocks_throttle`, `blocks_throttle2` | `throttle` | |
| `blocks_head` | `head` | |
| `blocks_file_source` / `blocks_file_sink` | `file_source` / `file_sink` | no offset/length, no append |
| `blocks_wavfile_source` | `wav_source` | one node per channel used |
| `blocks_wavfile_sink` | `wav_sink` | 16-bit PCM or float; 2 channels → `f32_pair_to_c32` + c32 (stereo) WAV |
| `blocks_null_sink` | `null_sink` | one per input |
| `blocks_add_xx`, `blocks_multiply_xx` | `add`, `multiply` | n inputs → a chain of n−1 |
| `blocks_multiply_const_xx` / `_vxx` | `gain` | real constants only |
| `blocks_complex_to_mag` / `_mag_squared` / `_real` / `_imag` / `_arg` | `c32_to_f32` | part = magnitude / magnitude_squared / real / imag / phase |
| `blocks_complex_to_float` | `c32_to_f32` ×2 | real and imag nodes, for the outputs in use |
| `blocks_float_to_complex` | `f32_pair_to_c32` (`f32_to_c32` with only the real input) | |
| `low_pass_filter`, `high_pass_filter`, `band_pass_filter`, `band_reject_filter` | `fir_filter` | `decim` → a `decimator` after, `interp` → an `interpolator` before; Hamming/Blackman/Kaiser (β → attenuation), other windows → Hamming |
| `freq_xlating_fir_filter_xxx` | `freq_xlating_fir` | cutoff / transition / window from `firdes.low_pass(…)` / `low_pass_2(…)` (also through a variable), or a symmetric `complex_band_pass(…)`; other taps → a low-pass at 0.4 × the output rate, with a warning. `f` input → `f32_to_c32` first. Taps gain ignored |
| `rational_resampler_xxx` | `rational_resampler` | interp / decim; custom taps ignored |
| `analog_quadrature_demod_cf` | `fm_demod` | deviation from the gain written as `rate / (2*math.pi*dev)` (→ `dev`, also `…*dev/8.0` → `dev / 8.0`); no de-emphasis |
| `analog_wfm_rcv` (`_pll`) | `fm_demod` + `decimator` | 75 kHz, de-emphasis from `deemph_tau`; `_pll`: mono to both outputs |
| `analog_nbfm_rx` | `fm_demod` + `rational_resampler` | deviation = `max_dev`, output rate = `audio_rate` |
| `analog_agc_xx` / `agc2` / `agc3` | `agc` | reference → target, max gain; rates → defaults (warning) |
| `analog_pwr_squelch_xx` | `squelch` | threshold; `gate` unsupported |
| `audio_sink` | `browser_audio` (`audio_sink` with `--audio-device`) | first channel only; `pace` off when an SDR source paces the graph |
| `osmosdr_source`, `rtlsdr_source`, `soapy_*_source`, `soapy_source`, `uhd_usrp_source`, `iio_pluto_source` | `sdr_source` | device string guessed (`rtl=0` → `rtlsdr:`, `rtl_tcp=h:p` → `rtltcp:host=h,port=p`, `hackrf=0` → `hackrf:`, `driver=X` → `soapy:driver=X,…`, UHD → `soapy:driver=uhd,…`); frequency, rate, gain, AGC, ppm, bandwidth, antenna; always a warning to check it |
| `qtgui_freq_sink_x`, `qtgui_time_sink_x`, `qtgui_waterfall_sink_x`, `qtgui_const_sink_x`, (`qtgui_sink_x`, `qtgui_number_sink`, …) | `null_sink` | foxygnal instruments are probes on wires: the warning says which one to open on the wire into it |
| `variable*`, `qtgui_range` / `entry` / `chooser`, `parameter` | `[vars]` | see above |
| `note`, `import` | — | nothing to convert |

Anything else is reported as `` `name` (gr_block_id) has no foxygnal equivalent yet `` and
left out with its connections. Stream types: `complex` → c32, `float` → f32, `short` → i16,
`byte` → u8 where the foxygnal block supports it; `int` and vectors (`vlen` > 1) are reported
and the block left out.

## Limitations

- Only the YAML format (GNU Radio 3.8+); the XML `.grc` of 3.7 isn't read.
- Python is evaluated only as far as the subset above: no conditionals, subscripts,
  comprehensions or arbitrary function calls. Untranslatable expressions are evaluated where
  possible and kept as values.
- foxygnal blocks take the rate from the stream, so GRC's per-block `samp_rate` parameters
  (filters, sinks) are ignored; a raw `blocks_file_source` has no rate, so keep the throttle or
  set the File Source's `sample_rate`.
- Hierarchical blocks (`pad_source` / `pad_sink`), embedded Python blocks, message ports,
  tagged-stream and digital blocks aren't converted.
- GUI layout (`gui_hint`), QT widgets other than variables, and `options` settings other
  than the title are ignored.
