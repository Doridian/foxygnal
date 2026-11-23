# Benchmarks

Micro-benchmarks with [criterion](https://docs.rs/criterion): single DSP kernels, single blocks,
and the whole FM receiver graph. They answer "did this change make X faster or slower?" on one
machine. For "how much of a Raspberry Pi does a real graph use?" there is the separate
`foxygnal bench` harness (docs/PERFORMANCE.md).

## Running

```sh
cargo bench -p foxygnal-dsp --bench kernels      # kernels, ~3 min
cargo bench -p foxygnal-blocks --bench blocks    # blocks and the FM graph, ~2 min
cargo bench -p foxygnal-dsp --bench kernels -- 'dot/'   # a subset (regex on the name)
cargo bench ... -- --warm-up-time 0.5 --measurement-time 1   # quick and rough
```

Always compare on the same machine, plugged in and otherwise idle. Throughput is in **input
items per second** (criterion's `Throughput::Elements`): samples for kernels and blocks,
bytes/shorts for the `cu8`/`ci16` converters, taps (multiply-accumulates) for `dot/*`.

Before/after a change:

```sh
cargo bench -p foxygnal-dsp -p foxygnal-blocks --bench kernels --bench blocks -- --save-baseline before
# … change things …
cargo bench -p foxygnal-dsp -p foxygnal-blocks --bench kernels --bench blocks -- --save-baseline after
scripts/bench-report.py table --baseline before    # markdown table of "after" vs "before"
```

`scripts/bench-report.py` (Python 3, stdlib only) reads criterion's output in `target/criterion`:

- `table [--baseline NAME]`: the latest results, optionally against a saved baseline.
- `export FILE.json [--baseline NAME]`: save results (mean time per iteration and items).
- `diff OLD.json [NEW.json]`: compare two exports, or an export with the latest results.

`FOXYGNAL_SIMD=baseline` makes the process use the baseline kernels (SSE2 on x86_64) instead of
the best the CPU has, so the effect of AVX2 shows in any benchmark that goes through
`foxygnal_dsp::kernels()`. The `dot/*` and `kernel/*` groups run every kernel set the CPU
supports side by side anyway.

## What is measured

**`foxygnal-dsp` (`benches/kernels.rs`)**, on 16 Ki samples per call (stays in L2, like a
scheduler quantum):

- `fir/{f32,c32}/{31,127,511}`: `Resampler::fir`, i.e. one dot product per output.
- `resampler/*`: ÷10 complex channel decimator (2.4 MS/s → 240 kS/s), the FM receiver's
  frequency-translating filter (rotator + 199 taps ÷10), 240 k → 48 k audio, 48 k → 44.1 k
  (147/160, 5801 taps).
- `rotator/{block,next}`: the SIMD block rotator and the one-sample `next()`.
- `spectrum/power/{1024,8192}`: window, FFT, |X|² (`PowerSpectrum::compute`).
- `dot/<set>/{f32,c32}/{32,128,512}` and `kernel/<set>/*`: every raw kernel for each kernel set
  (`sse2`, `avx2+fma`; `neon` on aarch64). `kernel/scalar/*` are the libm loops the kernels
  replaced (`hypot`, `atan2`), for reference.

**`foxygnal-blocks` (`benches/blocks.rs`)**: real flowgraphs, `VectorSource → block →
null_sink`, 512 Ki items per run, from `start()` to the end of the stream. That includes thread
start-up, the source's copy and the buffers, so `block/passthrough_c32` (no block) is the
ceiling. Blocks: `fir_filter` (c32/f32, 127 taps), `freq_xlating_fir` (198 taps ÷10),
`decimator` ÷10, `rational_resampler` 240 k → 48 k, `fm_demod`, `am_demod`, `agc`, `gain`,
`c32_to_f32` (magnitude, phase), `cu8_to_c32`, `ci16_to_c32`, `signal_source`.
`graph/fm_receiver_2m4` is `examples/fm-receiver.fox.toml` fed from memory: 2.4 MS/s c32 →
`freq_xlating_fir` ÷10 → `fm_demod` → `rational_resampler` to 48 kS/s → null sink, as fast as
it goes. Its throughput divided by 2.4 M is "times faster than real time".

## CI

`.forgejo/workflows/bench.yml` is not part of the normal CI: it only runs when started by hand
(Actions → Benchmarks → Run workflow), in a quick mode (`--warm-up-time 0.5 --measurement-time 1`)
and with a 45-minute timeout:

- Pick the branch to benchmark. If the `base` input (default `main`) names a different branch,
  that branch is benchmarked first in a separate worktree, on the same runner
  (`--save-baseline base`), then the chosen branch (`--save-baseline head`), and it prints
  `bench-report.py table --baseline base`. Comparing two runs on one runner is far more
  meaningful than comparing with numbers from another machine. If the base has no benchmarks
  (or they fail) the table shows the branch alone.
- It also prints the difference from the committed reference numbers
  (`docs/benchmarks/reference-x86_64.json`, this document's machine); only large moves mean
  anything there.
- The table, a JSON export and criterion's whole `target/criterion` directory are uploaded as
  the `criterion-<sha>` artifact (30 days).
- It never fails the build on a slowdown: shared runners are noisy, and quick mode more so.
  Read the ▲/▼ markers (moves beyond ±5%) as "worth a look", not as verdicts.

To refresh the reference numbers, on an idle machine:

```sh
cargo bench -p foxygnal-dsp -p foxygnal-blocks --bench kernels --bench blocks
scripts/bench-report.py export docs/benchmarks/reference-x86_64.json
```

and update the tables below.

## Reference numbers

AMD Ryzen 9 9950X3D (Zen 5, 16 cores / 32 threads), Linux, rustc 1.100, `--release` (thin LTO),
criterion with `--warm-up-time 1 --measurement-time 3`, November 2026. The machine was **not
idle** (load average around 40 from other builds), so treat ±10% as noise; unchanged code moved
by up to −27% (`agc_f32`) between runs. *Before* is the M4 code (scalar loops, libm `atan2` and
`hypot`); *after* is with the SIMD kernels (AVX2+FMA chosen at run time); *SSE2* is the same
build with `FOXYGNAL_SIMD=baseline`, roughly what a 128-bit SIMD unit like the Pi 4's NEON
gets per clock. Full results: `docs/benchmarks/reference-x86_64.json`.

**Whole graph**

| benchmark | before | after (AVX2) | after (SSE2) |
|---|---|---|---|
| `graph/fm_receiver_2m4` (input samples/s) | 52 M/s (22× real time) | 135 M/s (56×) | 65 M/s (27×) |

The graph runs one thread per block, so it is as fast as its slowest block
(`freq_xlating_fir`), minus scheduler overhead: the same filter work outside a graph
(`resampler/xlate_dec10_c32_199taps`) runs at 273 M/s.

**Blocks** (through a flowgraph; items/s)

| block | before | after (AVX2) | after (SSE2) |
|---|---|---|---|
| `fir_filter` c32, 127 taps | 12.9 M | 39.6 M | 15.9 M |
| `fir_filter` f32, 127 taps | 16.0 M | 49.8 M | 25.6 M |
| `freq_xlating_fir` 198 taps ÷10 | 52 M | 131 M | 64 M |
| `decimator` c32 ÷10 | 37.7 M | 110 M | |
| `rational_resampler` f32 240k→48k | 41.2 M | 127 M | |
| `fm_demod` | 57.8 M | 146 M | 121 M |
| `am_demod` | 116 M | 128 M | |
| `c32_to_f32` magnitude | 156 M | 364 M | |
| `c32_to_f32` phase | 78 M | 294 M | |
| `cu8_to_c32` (bytes/s) | 614 M | 773 M | |
| `ci16_to_c32` (shorts/s) | 545 M | 705 M | |
| `gain` c32 / f32 | 331 M / 426 M | 344 M / 452 M | |
| `agc` c32 / f32 (unchanged) | 108 M / 112 M | 107 M / 82 M | |
| `signal_source` c32 / f32 (unchanged) | 36.7 M / 41.9 M | 37.7 M / 42.3 M | |
| `passthrough_c32` (no block: the ceiling) | 463 M | 484 M | |

Simple element-wise blocks (`gain`, the converters) were already vectorized by the compiler and
are bound by the graph's copying and scheduling (`passthrough`), so they barely move.

**Kernels** (16 Ki samples per call, outside any graph)

| benchmark | before | after (AVX2) | SSE2 set |
|---|---|---|---|
| `fir/f32/31` | 74 M/s | 102 M/s | |
| `fir/f32/127` | 21.4 M/s | 77.4 M/s | |
| `fir/f32/511` | 4.58 M/s | 39.6 M/s | |
| `fir/c32/31` | 65.7 M/s | 103 M/s | |
| `fir/c32/127` | 18.7 M/s | 63 M/s | |
| `fir/c32/511` | 4.42 M/s | 21.7 M/s | |
| `resampler/decim10_c32_365taps` | 58.4 M/s | 212 M/s | |
| `resampler/240k_48k_f32_183taps` | 64 M/s | 263 M/s | |
| `resampler/48k_44k1_f32_5801taps` | 67.3 M/s | 100 M/s | |
| `rotator` (`next()` loop → block) | 212 M/s | 2.49 G/s | 767 M/s |
| `spectrum/power/1024` | 820 M/s | 1.0 G/s | |
| `spectrum/power/8192` | 667 M/s | 799 M/s | |
| dot product f32, 512 taps (MAC/s) | ~2.3 G (scalar loop) | 23.2 G | 6.2 G |
| dot product c32, 512 taps (MAC/s) | ~2.3 G (scalar loop) | 12.7 G | 3.2 G |
| FM discriminator (libm `atan2` → fast) | 116 M/s | 1.4 G/s | 494 M/s |
| phase (libm `atan2` → fast) | 144 M/s | 1.96 G/s | 606 M/s |
| magnitude (`hypot` → `sqrt`) | 379 M/s | 7.69 G/s | 3.81 G/s |
| `power_to_db` | – | 3.77 G/s | 1.37 G/s |
| `u8_to_f32` / `i16_to_f32` | – | 28 G/s | 8.6 G / 9.5 G/s |

FFT time dominates `spectrum/power`; the window multiply and |X|² glue are a small part of it.

## Accuracy

Kernels match scalar code to rounding (tests compare every kernel set against a scalar
reference on random data, odd lengths and unaligned starts). The exceptions are deliberate
approximations:

- **atan2** (phase, FM discriminator): worst error 2.9e-7 rad over all directions and
  magnitudes from 1e-6 to 3e4 (`fast_atan2_error_is_tiny`). At the FM demodulator's output
  (±1 = ±deviation) that is under 1e-6, about −120 dB (`fm_demod_matches_an_exact_discriminator`).
  `atan2(±0, −0)` gives ±0 instead of ±π.
- **dB**: within 1e-4 dB of `10·log10` from 1e-24 to 1e6 (floored at −200 dB as before).
- **magnitude**: `sqrt(re² + im²)` instead of `hypot`, which only differs for values beyond
  ±1e19.
