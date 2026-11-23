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
| SigMF Source | `sigmf_source` | c32 / f32 from any `c…` / `r…` datatype; i16 / u8 raw (ci16 / cu8 as interleaved pairs); rate and centre frequency from the metadata; capture segments → `rx_freq` tags, annotations → tags (again on every loop); `start` = sample index (I/Q pairs for complex recordings, as in the annotations) to seek to and loop back to: earlier tags are skipped except `hw` and the active capture's `rx_freq`, tagged on item 0 | ✅ |
| Audio Source | `audio_source` | f32 (one channel of the sound card) or c32 (stereo: first two channels as real = left, imaginary = right) (`audio` feature) | ✅ |
| Satellite Tracker | `sat_tracker` | → f32 `offset`, `az`, `el` at `rate` values/s (default 20) + u8 text `track` (one line a second): SGP4/SDP4 from two-line elements (a file and/or pasted text, checksums checked; `satellite` by name or NORAD number, live), look angles from `lat`/`lon`/`alt` (WGS-84), `offset` = the `offset` setting + the Doppler shift at `frequency` (wire it to `freq_xlating_fir.@offset`); real-time clock, or from a UTC `time`; `track` lines `time=… name="…" id=<norad> lat=… lon=… alt_km=… az=… el=… range_km=… range_rate_kms=… doppler_Hz=…` (a ground track on the Map). The palette's Passes section lists its upcoming passes. No network access | ✅ |
| SDR Source | `sdr_source` | c32 from any device (`sim:`, RTL-SDR, rtl_tcp, SpyServer, SoapySDR…); live frequency / gain / AGC / ppm, tags `hw` (first item), `rx_freq`, `rx_gain`, `overflow` | ✅ |

## Sinks
| block | kind | types | status |
|---|---|---|---|
| Null Sink | `null_sink` | any | ✅ |
| File Sink | `file_sink` | any | ✅ |
| Vector Sink | `testing::VectorSink` | any (Rust API only) | ✅ |
| Audio Sink | `audio_sink` | f32 (mono, on every speaker) or c32 (stereo: real = left, imaginary = right; mixed down on a mono device) at the stream's rate; the sound card paces the graph (`audio` feature) | ✅ |
| Browser Audio | `browser_audio` | f32 (mono) or c32 (stereo: real = left, imaginary = right) at 3–768 kS/s, played in the tab that pressed Run (WebAudio); paces at the stream rate unless `pace` is off | ✅ |
| WAV Sink | `wav_sink` | f32, c32 (stereo I/Q), i16 → float32 or pcm16 WAV at the stream's rate | ✅ |
| SigMF Sink | `sigmf_sink` | any: `.sigmf-data` + `.sigmf-meta` with rate, frequency, datetime; tags kept (`rx_freq` → capture segment, `hw` → `core:hw`, others → annotations); metadata written at start, ~1 s while tags arrive, and at stop | ✅ |
| Message Log | `message_log` | u8 text lines (a decoder's output), 1–4 inputs (`in`, `in1`, …; whole lines, never mixed); optionally printed to stdout; watch the wire with the Messages instrument | ✅ |
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
| ADS-B / Mode S Decoder | `adsb_decoder` | f32 magnitude at ≥ 2 MS/s (any rate; 2.4 MS/s or more recommended) → u8 text: preamble and bits integrated over each pulse at a fractional phase, with decision feedback; DF0/4/5/11/16/17/18/20/21, CRC-24 with 1-bit (optionally 2-bit) repair for DF11/17/18, address/parity replies accepted only from addresses heard intact in the last minute (fixed-size table); AC13 altitude (Q bit and Gillham), squawk; Comm-B BDS 1,0 1,7 2,0 3,0 4,0 5,0 6,0 inferred dump1090-style; extended squitter identification + category, airborne/surface positions, velocities (all subtypes, GNSS−baro), aircraft status, target state, operational status, TIS-B/ADS-R (DF18 CF); CPR global (airborne, surface) and local (from the aircraft's last position or a receiver position); optional Mode A/C replies | ✅ |
| UAT Decoder | `uat_decoder` | f32 FM discriminator (deviation 312.5 kHz, de-emphasis off) at ≥ 2.083 MS/s → u8 text: 978 MHz UAT sync words (ADS-B and uplink, ≤ 4 wrong bits), quarter-sample timing, threshold from the sync bits; RS(30,18) / RS(48,34) ADS-B and 6 × RS(92,72) interleaved uplink; ADS-B header, state vector, mode status, auxiliary state vector; uplink header, information frames, FIS-B APDUs with text products (product 413 METAR/TAF/…, text records of NOTAM/AIRMET/SIGMET products) one line per record, NEXRAD and other graphics counted | ✅ |
| RDS Decoder | `rds_decoder` | f32 MPX (≥ 120 kS/s, whole-number rate) → u8 text: 57 kHz BPSK, Costas loop, biphase matched filter, differential decoding, block sync with offset words A/B/C/C'/D, optional 1-bit / 2-bit-burst repair; PI, PTY (RDS or RBDS names), TP, PS (0A/0B), RadioText (2A/2B), clock time (4A); a line when something new is complete, `verbose` adds one per group | ✅ |
| POCSAG Decoder | `pocsag_decoder` | f32 FM discriminator (de-emphasis off) or NRZ baseband, ≥ 9.6 kS/s → u8 text: 512 / 1200 / 2400 Bd (or `auto`: all three at once), either polarity, sync 0x7CD215D8 and batches, BCH(31,21) repair of up to 2 bits + parity, RIC and function bits, numeric / alphanumeric / both / auto (function bits plus a plausibility check); `t=… baud=1200 ric=… func=3 type=alpha msg="…"` | ✅ |
| FLEX Decoder | `flex_decoder` | f32 FM discriminator (de-emphasis off), ≥ 12.8 kS/s → u8 text: Motorola FLEX 1600 bps 2-FSK, 3200 bps 2-/4-FSK, 6400 bps 4-FSK (speed from the Sync 1 A code), either polarity, one 3200 Bd symbol clock, LMS slicer levels, phases A–D, 8-word block deinterleaving, BCH(31,21) repair of up to 2 bits + parity (repaired BIW/vector words must pass their 4-bit checksum), short and long addresses, alphanumeric / numeric / tone (with short digits) / secure / binary (hex) / instruction, fragments marked `part=`; `t=… baud=1600 levels=2 phase=A cycle=… frame=… capcode=001234567 type=alpha msg="…"` | ✅ |
| AIS Decoder | `ais_decoder` | f32 FM discriminator (de-emphasis off), ≥ 28.8 kS/s → u8 text or NMEA: GMSK 9600 Bd, one-bit matched filter, eight fixed sampling phases (no timing loop to settle on a 26 ms burst; duplicates dropped), slicer centre from the training sequence (frequency offsets don't matter), NRZI, HDLC flags and unstuffing, CRC-16; types 1–3 position, 4/11 base station time, 5 static and voyage (6-bit text), 9 SAR aircraft, 18/19 class B, 21 aid to navigation (with name extension), 24 A/B static, 27 long range, others type + MMSI + `raw=`; `channel` names the channel; `t=… ch=A mmsi=… type=1 status=… lat=… lon=… sog=… cog=… heading=…`, or `!AIVDM` sentences | ✅ |
| ACARS Decoder | `acars_decoder` | f32 AM-demodulated audio, ≥ 9.6 kS/s → u8 text: 2400 bit/s MSK (1200/2400 Hz), coherent detection (mix to 1800 Hz, half-cosine matched filter, bit timing from a one-bit differential phase detector, decision-directed carrier phase loop, polarity from SYN), SYN SYN SOH sync, 7-bit odd-parity characters, CRC-16 (CCITT, reflected) with repair of up to 2 wrong bits (parity + syndrome); `t=… freq=131.550 mode=2 reg=… ack=… label=H1 block=… msgno=… flight=… text="…"`, `more=1` before further blocks | ✅ |
| ISM Sensor Decoder | `ism_decoder` | c32 baseband, 250 kS/s – 3.2 MS/s → u8 text: rtl_433-style sensor decoding. OOK envelope pulse detector (adaptive noise floor, glitch merging) and FSK pulse detector (discriminator, min/max trackers) feed PCM/NRZ, PPM, PWM, Manchester and differential-Manchester slicers with each device's timing; ~25 device formats (Acurite, Ambient Weather, LaCrosse, Fine Offset, Oregon Scientific v2.1/v3, Nexus/Prologue/Rubicson, Schrader/Toyota/Ford/Citroen TPMS, Honeywell/2Gig, Interlogix, EV1527/PT2262, ERT SCM/SCM+/IDM) with their checksums/CRCs; `devices` selects (`all`, a list, or `-key` exclusions); `analyze` summarises unknown packets; `t=… model=… id=… temperature_C=… mic=CRC rssi_dB=… snr_dB=… offset_kHz=…` | ✅ |
| AFSK 1200 Demodulator | `afsk1200_demod` | f32 audio (≥ ~7 kS/s) → u8 bits (mark = 1) or f32 soft at 1200/s: Bell 202 quadrature correlators with per-tone level tracking (pre-/de-emphasis tilt doesn't matter), bit-timing PLL; tones and baud adjustable | ✅ |
| HDLC Deframer | `hdlc_deframer` | u8 bits → u8: NRZI (optional), 0x7E flags, bit unstuffing, abort on seven 1s, CRC-16-CCITT FCS check (or pass bad frames marked `fcs=bad`); output AX.25 text (call signs, path, control/PID, APRS: positions with/without timestamp, uncompressed and compressed, Mic-E, status, message, object, item, others raw), hex lines, or raw bytes tagged `packet` | ✅ |
| DVB-S Modulator | `dvbs_modulator` | u8 TS (188-byte packets) → c32 at sps × symbol rate: EN 300 421 energy dispersal, RS(204,188), Forney interleaver (12 × 17), K = 7 convolutional code punctured 1/2 / 2/3 / 3/4 / 5/6 / 7/8, Gray QPSK, RRC α = 0.35; RMS level setting | ✅ |
| DVB-S Demodulator | `dvbs_demodulator` | c32 (≥ 2 samples/symbol) → u8 TS (`ts`) + u8 text (`status`): RRC matched filter, Gardner timing, AGC, QPSK Costas, 4th-power FFT frequency search (± symbol rate / 8), automatic code rate, puncturing phase and phase ambiguity (also a mirrored spectrum), soft Viterbi, RS; uncorrectable packets keep their place with transport_error_indicator set; status once a second and on lock changes | ✅ |
| DVB-T Modulator | `dvbt_modulator` | u8 TS → c32 at 64/7 MHz × bandwidth / 8 (5–8 MHz channels): EN 300 744, non-hierarchical, 2k / 8k, QPSK / 16-QAM / 64-QAM, code rates 1/2–7/8, guard 1/4–1/32; the DVB FEC chain, bit and symbol interleaving, continual and scattered pilots, TPS with BCH and cell id | ✅ |
| DVB-T Demodulator | `dvbt_demodulator` | c32 (≥ the elementary rate, resampled inside) → u8 TS (`ts`) + u8 text (`status`): mode, guard, timing and fractional offset from the cyclic prefix, integer offset from the continual pilots, sample clock tracking, TPS (BCH), channel estimation from the scattered pilots (time and frequency interpolation), CSI-weighted soft demapping, inner deinterleaving, Viterbi, RS | ✅ |
| ATSC Modulator | `atsc_modulator` | u8 TS → c32 at any rate ≥ 6.25 MS/s (default the symbol rate, 10.762 MS/s): A/53 8VSB, randomizer, RS(207,187), 52-segment byte interleaver, 12-way 2/3 trellis code with precoder, segment and field syncs (PN511, PN63 ×3 with the field-2 inversion, VSB mode, last 12 symbols), pilot, RRC α = 0.1152 VSB shaping, polyphase interpolation | ✅ |
| ATSC Demodulator | `atsc_demodulator` | c32 (≥ 6.25 MS/s, resampled inside) → u8 TS (`ts`) + u8 text (`status`): pilot FFT search (±250 kHz, either spectrum orientation) and PLL, RRC matched filter and resampler in one, segment sync and early/late timing (clock tracking), field sync with polarity and field parity, DFE (64 + 192 taps) LMS-trained on the field syncs then decision-directed, 12 Viterbi decoders, byte deinterleaver, RS, derandomizer; status with MER, frequency and clock offsets, RS counts | ✅ |
| CCSDS Deframer | `ccsds_deframer` | c32 symbols (one per symbol, after Clock Recovery and Costas Loop) → u8 CADUs (`frames`: ASM 1A CF FC 1D + derandomized, corrected codeblock, the `.cadu` format) + u8 text (`status`): sync marker search through every phase symmetry (BPSK ±, QPSK 4 rotations × I/Q swap; on the convolutionally encoded marker for coded links), soft Viterbi r = 1/2 K = 7 with G2 inverted (CCSDS) or not, NRZ-M, CCSDS derandomizer, RS(255,223) interleave 1–5 with virtual fill and the dual basis; bad frames dropped or marked with the inverted ASM; status with lock, frames, RS counts, Viterbi BER | ✅ |
| CCSDS Packets | `ccsds_packets` | u8 CADUs → u8 text (one line per packet) or packet records (`FXSP` header + space packet): VCDU header, insert zone and trailer settings, M_PDU first header pointer, space packets reassembled per virtual channel with frame-counter gap detection; fill frames and idle packets skipped | ✅ |
| CCSDS Framer | `ccsds_framer` | u8 bytes → u8 CADUs: space packets (one APID, sequence counts) multiplexed into AOS transfer frames of one virtual channel, idle-packet fill at the end; RS parity left for the modulator | ✅ |
| CCSDS Modulator | `ccsds_modulator` | u8 CADUs → c32 symbols (one per symbol): RS(255,223), randomizer, NRZ-M, r = 1/2 convolutional code, BPSK or QPSK; for loopback tests | ✅ |
| Multi-level FSK, GFSK/GMSK shaping | | | 🔜 later |

## Level
| block | kind | types | status |
|---|---|---|---|
| AGC | `agc` | f32, c32: peak-following, target, attack/decay (s), max gain; all live | ✅ |
| Power Squelch | `squelch` | f32, c32: threshold dB with hysteresis, averaging, fade ramp; outputs zeros when closed (rate unchanged) | ✅ |

## Measurement
| block | kind | types | status |
|---|---|---|---|
| Peak Frequency | `peak_frequency` | c32 → f32: offset (Hz) of the strongest signal, `update_rate` values/s (the output's rate); averages Hann FFT spectra (`fft_size`) over each update; centre of power over `bandwidth` (mean shift from the peak bin); `hysteresis` dB before jumping to another signal; holds on noise (`min_snr` over the median bin); optional search range `min_offset`…`max_offset`; outputs nothing until it first finds a signal. Made for control inputs, e.g. `freq_xlating_fir.@offset` | ✅ |

**Control inputs and constants (any block).** Every setting marked live that is a number or
on/off can be driven by an f32 wire instead: connect to `block.@name` (in the editor: the
"⇢ input" toggle). The value is applied before each `work` call (sample and hold), the value
in the file is the initial one, and a rejected value is a warning, not an error. Any stream
input can instead be a constant (`const_inputs = { b = 1.5 }`, c32: `[re, im]`), which runs
in step with the block's other inputs. See ARCHITECTURE.md, "Control inputs and constant
inputs".

## Instruments (M2)
These are not graph blocks: they are probes you attach to any edge from the UI. They include
Scope, Spectrum, Waterfall, Constellation and Meter. A block form (`scope_sink`, …) also
exists so a graph file can open instruments automatically.
