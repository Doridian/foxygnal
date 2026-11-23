# Block catalog

✅ implemented · 🔜 planned (milestone in brackets)

## Sources
| block | kind | types | status |
|---|---|---|---|
| Signal Generator | `signal_source` | f32, c32: sine (complex exp. for c32), square, saw, triangle, constant | ✅ |
| Noise | `noise_source` | f32, c32: Gaussian | ✅ |
| File Source | `file_source` | any: raw interleaved LE, loop | ✅ |
| Vector Source | `testing::VectorSource` | any (Rust API only) | ✅ |
| WAV Source | `wav_source` | f32 (one channel), c32 (stereo as I/Q), i16; rate from the header, loop | ✅ |
| SigMF Source | `sigmf_source` | c32 / f32 from any `c…` / `r…` datatype; i16 / u8 raw (ci16 / cu8 as interleaved pairs); rate and centre frequency from the metadata; capture segments → `rx_freq` tags, annotations → tags (again on every loop) | ✅ |
| Audio Source | `audio_source` | f32, one channel of the sound card (`audio` feature) | ✅ |
| SDR Source | `sdr_source` | c32 from any device (`sim:`, RTL-SDR, rtl_tcp, SpyServer, SoapySDR…); live frequency / gain / AGC / ppm, tags `hw` (first item), `rx_freq`, `rx_gain`, `overflow` | ✅ |

## Sinks
| block | kind | types | status |
|---|---|---|---|
| Null Sink | `null_sink` | any | ✅ |
| File Sink | `file_sink` | any | ✅ |
| Vector Sink | `testing::VectorSink` | any (Rust API only) | ✅ |
| Audio Sink | `audio_sink` | f32 at the stream's rate; the sound card paces the graph (`audio` feature) | ✅ |
| Browser Audio | `browser_audio` | f32 (mono) or c32 (stereo: real = left, imaginary = right) at 3–768 kS/s, played in the tab that pressed Run (WebAudio); paces at the stream rate unless `pace` is off | ✅ |
| WAV Sink | `wav_sink` | f32, c32 (stereo I/Q), i16 → float32 or pcm16 WAV at the stream's rate | ✅ |
| SigMF Sink | `sigmf_sink` | any: `.sigmf-data` + `.sigmf-meta` with rate, frequency, datetime; tags kept (`rx_freq` → capture segment, `hw` → `core:hw`, others → annotations); metadata written at start, ~1 s while tags arrive, and at stop | ✅ |
| Message Log | `message_log` | u8 text lines (a decoder's output); optionally printed to stdout; watch the wire with the Messages instrument | ✅ |
| SDR Sink (TX) | `sdr_sink` | c32; needs `--allow-tx` and TX armed for the run; live frequency / gain; `sim:` discards | ✅ |

## Network
Raw interleaved little-endian items, the same bytes as a raw file, with no framing. Every
address defaults to `127.0.0.1`; listening on `0.0.0.0` (or a LAN address) is a choice you
type into the block and exposes the stream, unauthenticated, to the network (the engine
prints a note). `foxygnal serve --allow-remote` is about the web UI and doesn't affect these.

| block | kind | types | status |
|---|---|---|---|
| UDP Source | `udp_source` | any: listens on address:port (default 127.0.0.1:2000); items may span datagrams; header none / seq64 / seq32, with lost datagrams (and data dropped because the graph fell behind) tagged `dropped` (int, ≈ items lost); optional sample rate; joins a multicast group | ✅ |
| UDP Sink | `udp_sink` | any: to address:port, datagram size incl. header (default 1472 bytes = one Ethernet frame, whole items), optional seq64 / seq32 header; never waits for a receiver | ✅ |
| TCP Source | `tcp_source` | any: server (one sender at a time) or client; on disconnect, wait for / reconnect to the next sender, or end the stream; first item of each connection tagged `connected` | ✅ |
| TCP Sink | `tcp_sink` | any: server (up to `max_clients` receivers, each with its own bounded queue; a slow one misses data, logged, unless `backpressure` makes the graph wait) or client with reconnect | ✅ |
| ZeroMQ PUB/SUB | | | 🔜 later (see below) |

**Interop.**
- GNU Radio 3.9+ (gr-network): *UDP Sink* → `udp_source` and `udp_sink` → *UDP Source*,
  with matching header: GNU Radio's "None" = `none`, "64-bit Sequence Number" = `seq64` (other
  GNU Radio header types aren't supported). Match the item type (Complex = `c32`, Float =
  `f32`, Short = `i16`, Byte = `u8`). *TCP Sink* in client mode connects to a `tcp_source`
  server; in server mode a `tcp_source` client connects to it.
- `nc` / `socat` (header `none`): `socat -u UDP-RECV:2000 - > iq.c32` records what a
  `udp_sink` sends; `socat -u FILE:iq.c32 UDP-SENDTO:127.0.0.1:2000` feeds a `udp_source`
  (unpaced: it overruns a receiver that can't keep up, so throttle it, e.g. with `pv -L`);
  `nc 127.0.0.1 2000 < iq.c32` feeds a server-mode `tcp_source` (TCP paces itself);
  `nc 127.0.0.1 2000 > iq.c32` records from a server-mode `tcp_sink`.
- TCP has no way to mark a gap: when a `tcp_sink` client misses data, the receiver just sees
  the stream continue (still on whole items). Turn on `backpressure` if every sample counts.

**ZeroMQ** is not implemented. The pure-Rust `zeromq` crate (zmq.rs, 0.7) is async-only, so
the blocks would have to carry a tokio runtime, and it is pre-1.0 with no verified
interoperability with libzmq's PUB/SUB as used by gr-zeromq; binding libzmq would add a C
library dependency, which the project avoids. Use the UDP or TCP blocks with GNU Radio
instead.

## Math
The palette search also matches each block's keywords (e.g. `abs`, `envelope`, `phase` find
Complex → Float; `multiply_const` finds Gain).

| block | kind | types | status |
|---|---|---|---|
| Add | `add` | f32, c32: a + b | ✅ |
| Subtract | `subtract` | f32, c32: a − b | ✅ |
| Multiply | `multiply` | f32, c32: a · b | ✅ |
| Divide | `divide` | f32, c32: a / b (IEEE: ÷0 gives ±∞ / NaN) | ✅ |
| Gain | `gain` | f32, c32: × constant, live (multiply-const) | ✅ |
| Add Constant | `add_const` | f32, c32: + constant (re + j·im), live | ✅ |
| Conjugate | `conjugate` | c32 | ✅ |
| Absolute Value | `abs` | f32 | ✅ |
| Log Power (dB) | `log_power` | f32, c32 → f32: 10·log10(\|x\|²) + offset, floor; both live | ✅ |
| Magnitude, Phase, Real, Imag | `c32_to_f32` | see Converters (part: magnitude, phase, real, imag, magnitude²) | ✅ |

## Flow
| block | kind | types | status |
|---|---|---|---|
| Throttle | `throttle` | any | ✅ |
| Head | `head` | any | ✅ |
| Skip | `skip` | any: drops the first N items; later tags move N earlier | ✅ |
| Selector | `selector` | any: 2–4 inputs → 1, live `input`; unselected inputs are drained so upstream never stalls; a switch tags `selected_input` | ✅ |
| Valve | `valve` | any: live `open`; closed drops (default) or blocks (back-pressures upstream) | ✅ |
| Delay | `delay` | any: N zeros first; live: raising inserts zeros, lowering drops input; tags move with their items | ✅ |

## Converters
Fixed port types, so the editor can offer one when you connect mismatched ports.

| block | kind | types | status |
|---|---|---|---|
| Complex → Float | `c32_to_f32` | c32 → f32: real, imag, magnitude, magnitude², phase | ✅ |
| Float → Complex | `f32_to_c32` | f32 → c32 (imag 0) | ✅ |
| Re + Im → Complex | `f32_pair_to_c32` | f32, f32 → c32 | ✅ |
| Complex → Re + Im | `c32_to_f32_pair` | c32 → f32, f32 | ✅ |
| Short → Float | `i16_to_f32` | i16 → f32, ±32767 → ±scale | ✅ |
| Float → Short | `f32_to_i16` | f32 → i16, clipped | ✅ |
| Byte → Float | `u8_to_f32` | u8 → f32 (0..255) | ✅ |
| Interleaved Bytes → Complex | `cu8_to_c32` | u8 pairs → c32, (x − 127.5) / 127.5 (RTL-SDR), halves the rate | ✅ |
| Interleaved Shorts → Complex | `ci16_to_c32` | i16 pairs → c32, halves the rate | ✅ |

## Filters
Blocks marked 📈 preview their frequency response in the inspector (designed by the engine
from the block's parameters and the sample rate on its input wire). Frequencies are in Hz and
checked against the input rate when the graph is configured.

| block | kind | types | status |
|---|---|---|---|
| FIR Filter 📈 | `fir_filter` | f32, c32: windowed-sinc low/high/band-pass, band-stop; Hamming, Blackman, Kaiser; cutoff(s) and gain live | ✅ |
| Frequency-Translating FIR 📈 | `freq_xlating_fir` | c32 → c32: shift by offset, low-pass, decimate; moves `center_freq`; offset and cutoff live | ✅ |
| RRC Filter 📈 | `rrc_filter` | f32, c32: root-raised-cosine (sps, roll-off, span), SIMD FIR; filter mode (matched filter, unity DC gain, fractional sps ok) or interpolate ×sps (pulse shaping); a TX/RX pair peaks at 1 with no ISI; gain live | ✅ |
| IIR Biquad 📈 | `biquad` | f32, c32: RBJ cookbook low/high/band-pass, notch, peaking, low/high shelf, all-pass; frequency, Q, gain live; cascade of N stages | ✅ |
| DC Blocker 📈 | `dc_blocker` | f32, c32: single-pole high-pass, −3 dB cutoff (live), unity gain at Nyquist | ✅ |
| Hilbert Transform 📈 | `hilbert` | f32 → c32 analytic signal: windowed FIR Hilbert on the imaginary part (SIMD), real part delayed (taps − 1)/2 to match | ✅ |

## Resamplers
One polyphase kernel; each has a built-in anti-alias/anti-image low-pass (Kaiser, 60 dB) and
updates the output sample rate.

| block | kind | types | status |
|---|---|---|---|
| Decimator 📈 | `decimator` | f32, c32: ÷N | ✅ |
| Interpolator 📈 | `interpolator` | f32, c32: ×N, amplitude kept | ✅ |
| Rational Resampler 📈 | `rational_resampler` | f32, c32: ×L/M, or a target output rate (ratio reduced by gcd) | ✅ |

## Demodulators
| block | kind | types | status |
|---|---|---|---|
| FM Demodulator | `fm_demod` | c32 → f32: quadrature, ±deviation → ±1, de-emphasis off / 50 µs / 75 µs | ✅ |
| AM Demodulator | `am_demod` | c32 → f32: envelope with DC removal | ✅ |
| SSB Demodulator 📈 | `ssb_demod` | c32 → f32: USB / LSB (Weaver), audio band low/high cut | ✅ |
| FM Stereo Decoder | `fm_stereo` | f32 MPX (FM demod with de-emphasis off, ≥ 106 kS/s) → f32 left + f32 right: 19 kHz pilot PLL, 38 kHz L−R, 15 kHz low-pass, resampled to the audio rate, de-emphasis off / 50 / 75 µs; mono without a pilot (or `mode = mono`); tags `stereo` true/false on change | ✅ |

## Digital
Decoders write one text line per message on a `u8` port (see "Decoded messages" in
ARCHITECTURE.md): watch it with the **Messages** instrument or end it in a Message Log.

| block | kind | types | status |
|---|---|---|---|
| Clock Recovery | `clock_recovery` | f32, c32: symbol timing, cubic interpolator, Gardner or Mueller-Müller detector, fractional samples/symbol, loop bandwidth; output rate = input ÷ sps | ✅ |
| Costas Loop | `costas_loop` | c32 → c32 + f32 frequency (Hz): carrier recovery, order 2 / 4 / 8 | ✅ |
| Binary Slicer | `binary_slicer` | f32 → u8 bits (≥ threshold → 1) | ✅ |
| PSK Demapper | `psk_demod` | c32 → u8: BPSK / QPSK / 8PSK, Gray coded, symbols or bits (MSB first) | ✅ |
| FSK Demodulator | `fsk_demod` | c32 → f32 soft symbols: quadrature discriminator, symbol-long moving average, optional DC correction | ✅ |
| Correlate Access Code | `correlate_access_code` | u8 bits: finds a sync word (hex or bit string, ≤ 64 bits, allowed errors), tags the first payload bit `packet` | ✅ |
| Packet Decoder | `packet_decoder` | u8 bits → u8: N payload bytes after each `packet` tag, as hex text lines or raw bytes | ✅ |
| ADS-B Decoder | `adsb_decoder` | f32 magnitude at 2 MS/s → u8 text: Mode S DF17/18, CRC-24 with 1-bit repair, address, call sign, altitude, velocity, global CPR position | ✅ |
| RDS Decoder | `rds_decoder` | f32 MPX (≥ 120 kS/s, whole-number rate) → u8 text: 57 kHz BPSK, Costas loop, biphase matched filter, differential decoding, block sync with offset words A/B/C/C'/D, optional 1-bit / 2-bit-burst repair; PI, PTY (RDS or RBDS names), TP, PS (0A/0B), RadioText (2A/2B), clock time (4A); a line when something new is complete, `verbose` adds one per group | ✅ |
| Multi-level FSK, GFSK/GMSK shaping, FEC, AX.25/HDLC, POCSAG | | | 🔜 later |

## Level
| block | kind | types | status |
|---|---|---|---|
| AGC | `agc` | f32, c32: peak-following, target, attack/decay (s), max gain; all live | ✅ |
| Power Squelch | `squelch` | f32, c32: threshold dB with hysteresis, averaging, fade ramp; outputs zeros when closed (rate unchanged) | ✅ |

## Instruments (M2)
These are not graph blocks: they are probes you attach to any edge from the UI. They include
Scope, Spectrum, Waterfall, Constellation and Meter. A block form (`scope_sink`, …) also
exists so a graph file can open instruments automatically.
