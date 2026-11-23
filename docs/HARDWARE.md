# Hardware

SDR devices live in the `foxygnal-sdr` crate: a `Driver` discovers and opens devices from a
**device string**, `driver:key=value,key=value` (see `crates/foxygnal-sdr/src/args.rs`). Drivers
move samples on their own I/O thread into a lock-free ring; a block's `work()` only calls the
non-blocking `RxStream::read`.

## Network sources

Two drivers receive I/Q over TCP. Neither needs a C library or a local binary. Neither
discovers servers: probing would claim a single-client `rtl_tcp` server (and power up its
dongle) every time the discovery panel refreshes, so you open them by device string.

Both connect with a 3 s timeout, and errors name the server and address
(`could not connect to rtl_tcp at 10.0.0.5:1234: Connection refused`). A stream that loses its
server drains what it has buffered, then ends with `rtl_tcp at 10.0.0.5:1234 closed the
connection` (or `… stopped sending samples` after 5 s of silence).

### `rtltcp`: rtl_tcp

```text
rtltcp:host=10.0.0.5,port=1234
rtltcp:host=pi.local,direct_sampling=2,bias_tee=1
```

| key | default | meaning |
|---|---|---|
| `host` | `127.0.0.1` | server address (name, IPv4 or IPv6) |
| `port` | `1234` | |
| `agc` | `0` | the RTL2832's digital AGC |
| `offset_tuning` | `0` | E4000 offset tuning |
| `bias_tee` | `0` | 4.5 V on the antenna input |
| `direct_sampling` | `0` | `1` = I branch, `2` = Q branch (HF on modified dongles); tunes 0–28.8 MHz |

Frequency and gain ranges come from the tuner type the server reports (R820T, R828D, E4000,
FC0012, FC0013, FC2580), using librtlsdr's gain tables; a gain in dB is rounded to the nearest
tuner step. Sample rates are librtlsdr's: 225–300 kS/s and 0.9–3.2 MS/s. The settings are sent
on every start, since `rtl_tcp` keeps the dongle's state between clients. `rtl_tcp` serves one
client at a time: while another is connected the connection is accepted but no header arrives,
and opening fails after 3 s with a hint saying so.

### `spyserver`: Airspy SpyServer

```text
spyserver:host=10.0.0.5,port=5555
spyserver:host=sdr.example.org,format=uint8
```

| key | default | meaning |
|---|---|---|
| `host` | `127.0.0.1` | |
| `port` | `5555` | |
| `format` | `int16` | I/Q on the wire: `int16`, `uint8` (half the bandwidth), `int24` or `float`; a server that forces a format wins |

Protocol 2.0.1700. The sample rates offered are the device's maximum rate divided by 2ⁿ, for
the decimation stages the server reports (e.g. 10 MS/s, 5 MS/s, … for an Airspy R2), and the
tunable range is the device's. Gain is the device's gain **index** (0 to its maximum, e.g. 21
on an Airspy), not dB; automatic gain leaves the server's setting alone.

A SpyServer shared by several clients gives control to one. Without control (`can_control` 0 in
the server's CLIENT_SYNC message), setting the gain fails with an error saying so, and tuning is
limited to the band the server reports. Sample rate and format remain per-client.

## Native drivers (librtlsdr, libhackrf, SoapySDR)

Code: `crates/foxygnal-sdr/src/native/`. Registered first in `default_drivers()`, in the order
`rtlsdr`, `hackrf`, `soapy`.

### No C libraries at build time

Each driver `dlopen`s its vendor library (through `libloading`) the first time it is used,
trying the usual names:

| driver | library names tried | missing → error |
|---|---|---|
| `rtlsdr` | `librtlsdr.so.0`, `librtlsdr.so.2`, `librtlsdr.so`, `librtlsdr(.0).dylib`, `(lib)rtlsdr.dll` | `librtlsdr not found (install rtl-sdr)` |
| `hackrf` | `libhackrf.so.0`, `libhackrf.so`, `libhackrf(.0).dylib`, `(lib)hackrf.dll` | `libhackrf not found (install hackrf)` |
| `soapy` | `libSoapySDR.so.0.8`, `libSoapySDR.so`, `libSoapySDR(.0.8).dylib`, `(lib)SoapySDR.dll` | `libSoapySDR not found (install SoapySDR 0.8 …)` |

A missing library only makes that driver's `discover()` and `open()` fail with that message
(the discovery panel shows it). There is no cargo feature. If a library loads but a dependency
of it doesn't (libusb, say), the loader's message is appended. Failures aren't cached, so
installing the library takes effect without a restart. A library is never unloaded once used:
libusb and SoapySDR modules start threads.

The FFI is a few dozen hand-written function signatures per library (the `dl_api!` tables),
copied from `rtl-sdr.h`, `libhackrf/hackrf.h` and `SoapySDR/{Types,Device}.h` (SoapySDR 0.8
ABI: `setupStream` returns the stream pointer). The structs we touch (`SoapySDRKwargs`,
`SoapySDRRange`, `hackrf_transfer`, `hackrf_device_list`) are checked by size and offset in
tests against values from a C compiler. `rtlsdr_set_bias_tee` is optional, so librtlsdr 0.6.0
still loads.

### Threads and lifetimes

- An open device is an `Arc`'d handle that closes the device on drop; streams hold a clone,
  so a stream keeps its device open even if the `Device` is dropped first.
- RTL-SDR: `rtlsdr_read_async` runs on a `rtlsdr-rx` thread; its callback converts and pushes
  into the ring. Drop sets a stop flag and calls `rtlsdr_cancel_async` until the thread ends
  (a cancel before `read_async` started would otherwise be lost).
- HackRF: libhackrf's own transfer thread calls our RX/TX callbacks. `hackrf_stop_rx/tx`
  waits for in-flight transfers before we free the callback state.
- SoapySDR: a `soapy-rx` thread loops on `readStream` (100 ms timeout); a `soapy-tx` thread
  pops from the TX ring and calls `writeStream`.
- Control calls (retune, gain, ppm) take a per-device mutex, never taken by the streaming
  callbacks or loops. They're for `set_param`, not the block's hot path.
- SoapySDR detail: SoapyPlutoSDR (at least) takes a lock inside `readStream` that is also
  taken by `setFrequency`/`setGain`, and an I/O loop calling `readStream` back to back
  starved retunes for 11–19 s. The RX loop now pauses (up to 50 ms) while a control call is
  waiting; the ring holds ~1/8 s, so nothing is lost. Retunes went to ~2 ms median.

Sample conversion: RTL-SDR `u8` → `(x − 127.5) / 127.5`; HackRF `i8` → `x / 128`, and for TX
`round(x · 127)` clamped to ±127. Conversion goes through a preallocated scratch buffer; the
callbacks don't allocate or lock. If the ring is full, whole chunks are dropped and counted
(`overflows()`).

### Device strings

| string | opens |
|---|---|
| `rtlsdr:` / `rtlsdr:index=1` / `rtlsdr:serial=00000001` | an RTL-SDR (discovery uses `serial=` only when the serial is unique, since many dongles share `00000001`) |
| `rtlsdr:…,direct_sampling=1` (`2` for Q) | HF direct sampling; tuning range becomes 0–28.8 MHz |
| `rtlsdr:…,offset_tuning=1`, `bias_tee=1`, `rtl_agc=1` | E4000 offset tuning; bias tee; RTL2832 digital AGC |
| `hackrf:` / `hackrf:serial=…` / `hackrf:index=0` | a HackRF; `serial` may be a unique suffix |
| `hackrf:…,amp=1`, `bias_tee=1` | the ~11 dB RF amp (RX and TX); antenna port power |
| `soapy:<kwargs>` | every pair goes to `SoapySDRDevice_make`, e.g. `soapy:driver=plutosdr,uri=ip:192.168.2.1` |

### Capabilities and gain

- **RTL-SDR**: frequency range per tuner (R820T/R828D 24–1766 MHz, E4000 52–1100 and
  1250–2200 MHz, FC0012 22–948.6 MHz, FC0013 22–1100 MHz, FC2580 146–308 and 438–924 MHz);
  sample rates 225.001–300 kS/s and 0.900001–3.2 MS/s (above 2.4 MS/s many dongles drop
  samples). Gain is the tuner's list (`rtlsdr_get_tuner_gains`); the range is its min–max
  (step 0, since the steps are uneven) and a requested dB value picks the nearest entry.
  `Gain::Auto` is tuner AGC. `ppm` → `rtlsdr_set_freq_correction` (whole ppm).
- **HackRF**: 1 MHz–6 GHz, 2–20 MS/s, baseband filter from `hackrf_compute_baseband_filter_bw`
  of 0.75 × rate unless a bandwidth is given. One overall RX gain of 0–102 dB is split: the
  IF stage ("LNA", 0–40 dB in 8 dB steps) takes as much as it can first, since it sets the
  noise figure; the rest goes to baseband ("VGA", 0–62 dB in 2 dB steps). 30 dB → 24 + 6,
  70 dB → 40 + 30. No AGC, so `Gain::Auto` is LNA 16 + VGA 16. The RF amp is the `amp`
  option, not part of the gain. TX gain is the TX VGA, 0–47 dB. No ppm correction (a nonzero
  ppm is an error). Half duplex: one stream at a time.
- **SoapySDR**: whatever the module reports for RX channel 0: `getFrequencyRange`,
  `getSampleRateRange` (or `listSampleRates`), `getGainRange`, `hasGainMode` (→ AGC),
  `listAntennas`, `getBandwidthRange` (or `listBandwidths`). Gain dB is clamped to the range
  and goes to `setGain` (the module distributes it over its stages). Streams are `CF32`.

### SoapySDR discovery

`SoapySDRDevice_enumerate` can be slow or hang while modules scan the network, so it runs on
a helper thread and discovery gives up after 2 s (one scan at a time; a stuck scan makes the
next discovery report that instead of starting another). Network devices often aren't found
by scanning: with Avahi not running, `enumerate` with
`driver=plutosdr,uri=ip:10.2.11.27` as filter returned nothing, while `make` with the same
arguments works. For those, set hints:

```sh
FOXYGNAL_SOAPY_DEVICES="driver=plutosdr,uri=ip:192.168.2.1;driver=remote,remote=10.0.0.9"
```

(or `SoapyDriver::with_hints`). Each hint is tried as an enumerate filter; if that finds
nothing, the hint is listed anyway as "(configured, not verified)". Making the device to check
would take about a second per device. Discovered SoapySDR devices report `tx: false` because
that isn't known without making the device; `open()` reports the real RX/TX channel counts.
A dongle may appear twice, once natively (`rtlsdr:`) and once through its SoapySDR module
(`soapy:driver=rtlsdr,…`).

### What has been tested

Without hardware (in `cargo test`): library loading for all three (SoapySDR 0.8.1, librtlsdr
2.0.3, libhackrf 0.9.2), discovery with nothing attached (empty, no error), the
missing-library errors (through an internal constructor with a bogus path), struct layouts,
kwargs round trip through libSoapySDR, the bounded scan plus hints, a failed `make`, sample
conversions, the HackRF gain split, the TX fill/underflow logic, and device-string and label
generation.

On real hardware, receive only, with a PlutoSDR over the network
(`cargo run -p foxygnal-sdr --example soapy_rx -- "soapy:driver=plutosdr,uri=ip:…" [freq] [rate]`):

- open ≈ 30 ms, stream start ≈ 0.9 s, stop ≈ 3 ms
- at 2.4 MS/s the achieved rate over 1.5 s was 2.394–2.400 MS/s (the rest is 64 Ki-sample MTU
  granularity), with 0 overflows in any run, including during 40 back-to-back control calls
- FM broadcast around 100 MHz: stations visible ~15–21 dB above the floor that stay at the
  same absolute frequency across retunes and gain changes; mean power follows gain (10 dB:
  −55 dBFS, 40 dB: −26 dBFS, 60 dB: −11 dBFS, near clipping)
- live retune ≈ 2 ms median, ≤ 30 ms max; gain change ≈ 13 ms

Not tested on hardware: the RTL-SDR and HackRF drivers (no device attached; their streaming
paths have only been compiled and reviewed against the headers), and any transmit path. TX is
compiled and unit-tested without a device only, never run against real hardware.
