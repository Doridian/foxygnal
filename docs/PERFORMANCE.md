# Performance on a Raspberry Pi

M5's targets, to be validated on real hardware:

- a **2.4 MS/s FM receiver under 25% of one Raspberry Pi 4 core**, and
- an **engine RSS under 50 MB**.

`foxygnal bench` is the harness for both. Micro-benchmarks of single kernels and blocks
(criterion) are a separate thing.

## Getting a binary onto the Pi

The Pi needs a **64-bit OS** (Raspberry Pi OS 64-bit, Ubuntu, …: `uname -m` says `aarch64`).
32-bit Raspberry Pi OS (`armv7l`) is not built or tested.

**From CI.** The `aarch64 check` workflow (`.forgejo/workflows/arch.yml`) checks and lints the
workspace for `aarch64-unknown-linux-gnu` on every PR. When the runner can install the cross
linker it also builds a release binary with the UI embedded and uploads it as the
`foxygnal-aarch64-linux` artifact, with the `bench/` graphs next to it. Download it, then:

```sh
scp -r foxygnal-aarch64-linux/ pi@raspberrypi.local:
ssh pi@raspberrypi.local
cd foxygnal-aarch64-linux && chmod +x foxygnal-aarch64-linux
./foxygnal-aarch64-linux bench bench/fm-receiver-sim.fox.toml
```

**Cross-build yourself** (Debian/Ubuntu host):

```sh
sudo apt install gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
(cd ui && npm ci && npm run build)          # optional: embed the UI
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu -p foxygnal --features embed-ui
# → target/aarch64-unknown-linux-gnu/release/foxygnal
```

No C libraries are needed: the SDR drivers load librtlsdr, libhackrf or SoapySDR at run time
if they are installed on the Pi (see [HARDWARE.md](HARDWARE.md)).

**On the Pi** (slow, but simple): install Rust with rustup, then `cargo build --release -p
foxygnal`. Always benchmark a `--release` build; debug builds are many times slower.

## `foxygnal bench`

```
foxygnal bench GRAPH [--seconds 10] [--warmup 1] [--max] [--buffer ITEMS] [--json]
```

Runs the graph headless, waits `--warmup` seconds (filter design, buffers filling), measures
for `--seconds` (or until the graph ends, or Ctrl-C), stops it and reports:

- **process CPU**: user + system time of the whole process (`getrusage`) over the window, as a
  percentage of *one* core. 100% = one core fully busy; a 4-core Pi has 400%.
- **memory**: peak RSS (`VmHWM`), current RSS and thread count (`/proc/self/status`; elsewhere
  only peak RSS from `getrusage`). Peak RSS is for the whole run, start-up included.
- per **block**: `work`, the time spent inside `work()` as a share of the window (wall clock, the
  engine's own counter, also what the UI's *Load* overlay shows), and `thread CPU`, the CPU
  time of the block's own thread (Linux; includes the scheduler's overhead around `work`).
- **other threads**: CPU of every thread that isn't a block, grouped by name: SDR driver
  threads (`fox:sim-rx`, `rtltcp-rx`, librtlsdr's), the runtime.
- per **wire**: items/s, the stream's nominal rate from metadata, and the ratio (`×realtime`).

`--json` prints the same as one JSON object (see `crates/foxygnal/src/bench.rs`, `Report`).

**Real time vs. maximum.** By default the graph runs as it is: an SDR source, a throttle or
an audio block sets the pace, and the numbers say what that costs. A graph with none of those
runs as fast as it can (the report says so). `--max` turns a paced graph into an unpaced one:
throttles are bypassed and every `sdr_source` becomes an unpaced c32 tone generator at the
same rate (sound card blocks can't be unpaced; that is an error). The source wire's
`×realtime` is then how many times real time the chain sustains.

`--buffer ITEMS` overrides the per-output buffer size for experiments.

### Bundled graphs (`bench/`)

| graph | what |
|---|---|
| `fm-receiver-sim.fox.toml` | **The Pi target.** Simulated RTL-SDR (`sim:`) at 2.4 MS/s → `freq_xlating_fir` ÷10 → `fm_demod` → `rational_resampler` 48 kS/s → null sink. Real time, paced by the simulator. |
| `fm-receiver-max.fox.toml` | The same chain fed by an unpaced c32 tone: raw throughput. Same as `--max` on the first one. |

**The simulator isn't free.** It synthesizes six FM stations plus noise on its own thread,
`fox:sim-rx`, which shows up under *other threads* and counts towards process CPU. A real
RTL-SDR's driver thread only converts bytes to floats, which is much cheaper, so for the
receiver alone take process CPU minus `fox:sim-rx` (or the sum of the block threads). To bench
real hardware, set the source's `device` to `rtlsdr:` (or `""` for the first device found).

### Reference numbers

AMD Ryzen 9 9950X3D (x86_64, 32 threads), release build, thread-per-block scheduler (M0),
before SIMD kernels:

```
$ foxygnal bench bench/fm-receiver-sim.fox.toml
graph    bench/fm-receiver-sim.fox.toml  (realtime)
host     AMD Ryzen 9 9950X3D 16-Core Processor, 32 cores [linux/x86_64]
window   10.00 s after 1.0 s warm-up (ended: time)

process CPU  6.5% of one core  (user 5.2%, system 1.3%)
memory       peak RSS 12.5 MiB, RSS 12.5 MiB, 8 threads

block         work  thread CPU
sdr           0.2%        0.2%
channel       3.1%        2.9%
demod         0.3%        0.3%
resample      0.4%        0.5%
audio         0.0%        0.0%

other thread            CPU
fox:sim-rx             2.5%

wire                          items/s   stream rate  ×realtime
sdr.out → channel.in           2.40 M        2.40 M       1.00
channel.out → demod.in       240.00 k      240.00 k       1.00
demod.out → resample.in      240.00 k      240.00 k       1.00
resample.out → audio.in       48.00 k       48.00 k       1.00
```

So about 4% of a desktop core for the receiver and 2.5% for the simulator. The
channel filter (`freq_xlating_fir`, at the full 2.4 MS/s) is most of it. A Pi 4's Cortex-A72
is very roughly 5–10× slower per core, which would put the receiver somewhere around 20–40%,
that is, around the 25% target: the Pi run has to show which side, and the SIMD kernels and
the pooled scheduler are what should move it below.

`fm-receiver-max` on the same machine: 27 MS/s through the chain (11× real time) at about
90% of one core in total (tone generator 49%, channel filter 33%). No block reaches 100%,
so the M0 scheduler's hand-offs, not the DSP, limit throughput there; rerun after the pooled
scheduler and double-mapped buffers land.

## Memory

RSS on the same machine (release build, no UI embedded; `VmRSS` from `/proc/PID/status`):

| | before | after |
|---|---|---|
| `foxygnal serve`, idle | 7.4 MB, 34 threads | 7.1 MB, 4 threads |
| `serve` running fm-receiver-sim with 3 probes (2 spectra, a scope) | 28.6 MB, 41 threads | 16.2 MB, 11 threads |
| `foxygnal run bench/fm-receiver-sim.fox.toml` | 25.3 MB, 40 threads | 12.8 MB, 8 threads |

What changed:

- **SDR rings sized from the sample rate.** The simulator's ring held 0.5 s, which rounds up to
  2 Mi samples = **16 MiB** at 2.4 MS/s, all of it touched at creation. The network drivers
  (rtl_tcp, SpyServer) used a fixed 1 Mi samples (8 MiB). All drivers now use the hardware
  drivers' sizing: about 1/8 s, rounded up to a power of two (512 Ki samples = 4 MiB at
  2.4 MS/s, 0.22 s of slack).
- **Fewer tokio threads.** The CLI used tokio's default of one worker per core (32 here, 4 on a
  Pi) even for `run`. `run` and `bench` now use a single-threaded runtime; `serve` uses 2
  workers (`--threads N`). Idle thread stacks are mostly virtual, so this is more about
  thread count and per-thread malloc arenas than RSS.

Where the rest goes, for `run` (12.8 MB): about 7 MB is file-backed (the binary's code and
shared libraries, shared with other processes), 4 MiB the simulator's ring, and the graph's
buffers about 0.6 MiB (32 Ki items per output: 256 KiB for the c32 wire, 128 KiB per f32
wire). `serve` adds the HTTP stack, the probe thread and its FFT plans and frames, a few MB.
The embedded UI is about 400 KB of read-only data, paged in only when served.

To measure `serve` yourself: start it, run a graph from the browser (open a scope and a
spectrum), and read `grep -E 'VmRSS|VmHWM|Threads' /proc/$(pidof foxygnal)/status`.
