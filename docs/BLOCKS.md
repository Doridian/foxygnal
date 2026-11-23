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
| Browser Audio | `browser_audio` | f32 at 3–768 kS/s, played in the tab that pressed Run (WebAudio); paces at the stream rate unless `pace` is off | ✅ |
| WAV Sink | `wav_sink` | f32, c32 (stereo I/Q), i16 → float32 or pcm16 WAV at the stream's rate | ✅ |
| SigMF Sink | `sigmf_sink` | any: `.sigmf-data` + `.sigmf-meta` with rate, frequency, datetime; tags kept (`rx_freq` → capture segment, `hw` → `core:hw`, others → annotations); metadata written at start, ~1 s while tags arrive, and at stop | ✅ |
| Message Log | `message_log` | u8 text lines (a decoder's output); optionally printed to stdout; watch the wire with the Messages instrument | ✅ |
| SDR Sink (TX) | `sdr_sink` | c32; needs `--allow-tx` and TX armed for the run; live frequency / gain; `sim:` discards | ✅ |
| UDP / TCP / ZeroMQ | | any | 🔜 later |

## Math
| block | kind | status |
|---|---|---|
| Add | `add` | ✅ |
| Multiply | `multiply` | ✅ |
| Gain | `gain` | ✅ |
| Subtract, Divide, Conjugate, Magnitude, Phase, Complex↔Real/Imag | | 🔜 M3 |
| Log power (dB) | | 🔜 M3 |

## Flow
| block | kind | status |
|---|---|---|
| Throttle | `throttle` | ✅ |
| Head | `head` | ✅ |
| Skip, Selector, Valve, Delay | | 🔜 M3 |

## Converters
Fixed port types, so the editor can offer one when you connect mismatched ports.

| block | kind | types | status |
|---|---|---|---|
| Complex → Float | `c32_to_f32` | c32 → f32: real, imag, magnitude, magnitude², phase | ✅ |
| Float → Complex | `f32_to_c32` | f32 → c32 (imag 0) | ✅ |
| Re + Im → Complex | `f32_pair_to_c32` | f32, f32 → c32 | ✅ |
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
| RRC, IIR (biquad), DC blocker, Hilbert | | | 🔜 M3 |

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
| Multi-level FSK, GFSK/GMSK shaping, RRC matched filter, FEC, AX.25/HDLC, POCSAG | | | 🔜 later |

## Level
| block | kind | types | status |
|---|---|---|---|
| AGC | `agc` | f32, c32: peak-following, target, attack/decay (s), max gain; all live | ✅ |
| Power Squelch | `squelch` | f32, c32: threshold dB with hysteresis, averaging, fade ramp; outputs zeros when closed (rate unchanged) | ✅ |

## Instruments (M2)
These are not graph blocks: they are probes you attach to any edge from the UI. They include
Scope, Spectrum, Waterfall, Constellation and Meter. A block form (`scope_sink`, …) also
exists so a graph file can open instruments automatically.
