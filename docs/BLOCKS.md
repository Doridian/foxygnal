# Block catalog

✅ implemented · 🔜 planned (milestone in brackets)

## Sources
| block | kind | types | status |
|---|---|---|---|
| Signal Generator | `signal_source` | f32, c32: sine (complex exp. for c32), square, saw, triangle, constant | ✅ |
| Noise | `noise_source` | f32, c32: Gaussian | ✅ |
| File Source | `file_source` | any: raw interleaved LE, loop | ✅ |
| Vector Source | `testing::VectorSource` | any (Rust API only) | ✅ |
| WAV Source | | f32, mono/stereo | 🔜 M3 |
| SigMF Source | | from metadata | 🔜 M3 |
| Audio Source | | f32 | 🔜 M3 |
| SDR Source | | c32 / ci16 / ci8 | 🔜 M4 |
| rtl_tcp / SpyServer | | ci8 → c32 | 🔜 M4 |

## Sinks
| block | kind | types | status |
|---|---|---|---|
| Null Sink | `null_sink` | any | ✅ |
| File Sink | `file_sink` | any | ✅ |
| Vector Sink | `testing::VectorSink` | any (Rust API only) | ✅ |
| Audio Sink | | f32 | 🔜 M3 |
| WAV / SigMF Sink | | | 🔜 M3 |
| SDR Sink (TX) | | c32 | 🔜 M4 |
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
| Type converters | | 🔜 M3 |

## Filters & rate change (M3)
FIR (designer: low/high/band-pass, band-stop, RRC), IIR (biquad), DC blocker, decimator,
interpolator, rational resampler, frequency-translating FIR, Hilbert.

## Demodulation & sync (M3–M6)
AGC, squelch, AM, FM (narrow/wide, de-emphasis), SSB, quadrature demod, PLL/Costas, clock
recovery, FSK/PSK modems, framers.

## Instruments (M2)
These are not graph blocks: they are probes you attach to any edge from the UI. They include
Scope, Spectrum, Waterfall, Constellation and Meter. A block form (`scope_sink`, …) also
exists so a graph file can open instruments automatically.
