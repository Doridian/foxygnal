# Antenna rotator control (plan)

Status: **plan**, not built. Tracked as M16 in [ROADMAP.md](ROADMAP.md).

Goal: a small dish (a Discovery Dish, or any az/el mount) follows a polar orbiter by itself.
foxygnal predicts the pass (M15's SGP4 block), turns it into pointing commands, and sends them
to a rotator: an ESP32 with two motor drivers running firmware from this repo, or any
rotator Hamlib already knows.

## Shape of it

```
 TLEs + observer ──► sat_track (M15) ──az/el settings──► rotator block ──TCP / serial──► ESP32 FW ──► motors
                         │                                    ▲   │                         │
                         └─ doppler ──► freq_xlating_fir      │   └── position, state ◄─────┘
```

- The engine only ever sends **target positions**, never motor steps or speeds. Motion
  profiles, limits and the watchdog live in the firmware, so a stalled network, a crashed
  engine or a paused graph can't leave a motor running.
- Everything speaks an existing protocol, so the pieces can be swapped: Gpredict or SatDump
  can drive our firmware, and foxygnal can drive a commercial rotator through `rotctld`.

## Protocol

**Primary: the Hamlib `rotctld` network protocol** (TCP, line-based text, default port 4533).
It is what Gpredict, SatDump, SDR++ and Hamlib's own tools speak, and it's small enough to
implement completely on an ESP32. The subset we implement and rely on:

| Command | Meaning | Reply |
|---|---|---|
| `P <az> <el>` / `\set_pos` | go to azimuth, elevation (degrees, floats) | `RPRT 0` or `RPRT -n` |
| `p` / `\get_pos` | current position | two lines: az, el |
| `S` / `\stop` | stop now, hold position | `RPRT 0` |
| `K` / `\park` | go to the park position | `RPRT 0` |
| `_` / `\get_info` | model and firmware version | one line |
| `\dump_caps` (minimal) | limits, so clients learn the range | key: value lines |
| `q` | close the connection | — |

Extended responses (`+\get_pos`) are accepted too. Unknown commands answer `RPRT -4` (not
implemented) and are otherwise ignored.

**Secondary: EasyComm II over USB serial** (`AZ123.4 EL45.6`, `AZ EL` to query, `SA SE` to
stop). Hamlib's `easycomm` backend and the SatNOGS rotator firmware use it, so our firmware
on USB works with existing tools, and the `rotator` block can drive a SatNOGS rotator directly.

**foxygnal extensions** (all optional, sent only when `_` reports our firmware): `\get_status`
(motor state, faults, homed/not homed, temperature, supply voltage) and `\set_conf <key>
<value>` for calibration. Anything that changes calibration or limits needs the serial
console or the firmware's setup page, never the tracking connection.

## Engine side

New block `rotator` in `foxygnal-blocks` (module `rotator/`), no stream ports, driven through
settings as inputs (M8):

- **Settings:** `target` (`rotctld:host:port`, `easycomm:/dev/ttyUSB0[@baud]`), `az`, `el`
  (wired from `sat_track`), `min_el` (don't follow below this; default 0°), `deadband`
  (send only when the target moved more than this, default 0.5°), `rate` (at most this many
  commands a second, default 2), `park_after` (park after LOS, default on), `az_range`
  (e.g. `0..450` for a rotator with cable-wrap overtravel), `flip` (allow el 90..180 for
  az/el mounts that can tip over).
- **Outputs:** the measured az/el and the pointing error as live readings (shown in the
  inspector and the passes view), plus a messages output for state changes and faults.
- **I/O on its own thread**, like the network sources: `work()` only hands the latest target to
  it and returns (`WorkStatus::Wait` between updates). A dead or slow rotator never blocks
  the graph; the block shows "rotator not responding" and keeps retrying.

**Pass planning** (in `sat_track`, so the rotator block stays dumb): before AOS, pre-position
to the AOS azimuth at `min_el` so the first minute of the pass isn't spent slewing. For each
pass, choose the azimuth path that never crosses the mechanical stop (using the
`az_range` overtravel: e.g. a pass from 350° to 20° runs 350 → 380 rather than wrapping),
and for overhead passes on an az/el mount without flip, plan around the keyhole
(lead the azimuth, accept a short loss near zenith) or use flip mode when it is allowed. The
plan is a function of the pass and the rotator's limits, tested without hardware.

**Not tied to TX.** Pointing an antenna is part of receiving: a rotator block needs no
`--allow-tx`, no TX arm switch and no separate arm step. A graph with a rotator runs like any
receive graph, from the UI, `foxygnal run` and MCP alike. Safety lives where it can actually
work, in the firmware (soft limits, watchdog, fault latching, hardware e-stop), plus:

1. An `enabled` setting on the block (live, default on). Off, the block still connects and
   reads the position, so the UI shows where the dish points, but sends no `P`/`K`. Handy
   while calibrating or working on the mount.
2. Stopping the graph sends `S`. The firmware's watchdog covers the cases where that `S`
   never arrives.
3. A **Stop rotator** button in the inspector and the passes view sends `S` and sets
   `enabled` off, whatever the graph is doing.

## Firmware (`firmware/rotator-esp32/`)

Rust, `no_std`, `esp-hal` + `embassy` (async tasks for network, serial, motion and sensors),
the same language as the rest of the repo. Target ESP32-S3 first (USB-serial built in,
plenty of RAM), ESP32-C3 as the cheap option. Built in its own Forgejo workflow (not
`managed-rust.yml`), with the motion planner and protocol parser in a host-testable crate
(`foxygnal-rotator-proto`) shared with the engine's `rotator` block, so both sides parse and
format the same protocol code and the tests run on CI without hardware.

**Hardware assumed (documented, not required):**

- Two stepper motors with TMC2209 drivers (STEP/DIR, UART for current, StallGuard for
  sensorless homing). DC gear motors with an H-bridge and encoder are a later driver.
- Position sensing: AS5600 magnetic absolute encoders on each axis (I²C, behind a
  multiplexer or one per bus) **or** endstop switches with homing at boot. With encoders the
  steppers are closed-loop (missed steps get corrected); without, homing is mandatory before
  the first move.
- A hardware e-stop input that cuts driver enable in hardware, and is also read by the
  firmware (reported in `\get_status`).
- The SatNOGS rotator v3 mechanics fit this exactly, and a Discovery Dish (≈ 2 kg with feed)
  is within its load.

**Firmware behaviour:**

- **Motion:** trapezoidal profile per axis (max speed and acceleration configurable), new
  targets retarget smoothly mid-move. Soft limits clamp every target. Az overtravel range
  configurable (cable wrap), tracked in absolute degrees, never wrapped silently.
- **Watchdog:** a tracking target expires after `hold_timeout` (default 30 s) with no new
  command; the axis stops and holds. After `park_timeout` (default 10 min) with no client, it
  parks. Loss of Wi-Fi does not move anything.
- **Faults:** stall, encoder disagreeing with step count, encoder missing, limit hit, e-stop.
  A fault stops both axes and latches until cleared from the console or the setup page.
- **Calibration:** north offset, elevation zero, encoder direction, steps per degree, limits,
  park position. Stored in NVS. Set via the USB serial console or a small setup web page (on
  the device's LAN address, off by default after setup).
- **Network:** Wi-Fi station (credentials over USB on first boot), mDNS name
  `foxrot-<id>.local` so the rotator block can find it, `rotctld` on TCP 4533, one tracking
  client at a time (a second one may read positions only). Optional Ethernet (W5500) for
  outdoor installs where Wi-Fi is unreliable.
- **OTA updates** from the setup page, with a rollback partition.

## Milestones

1. **Protocol crate and engine block against a simulator**: `foxygnal-rotator-proto`
   (rotctld + EasyComm parse/format, the motion planner), a `rotator` block, a simulated
   rotator (`target = sim:`) with realistic slew speed so the whole thing is testable, a stop
   button and the position in the passes view. Also drives a real `rotctld` and so every
   Hamlib rotator from day one.
2. **Pass planning**: pre-positioning, cable-wrap path choice, keyhole and flip handling, a
   pass timeline in the passes view showing the planned and measured pointing.
3. **Firmware v1**: ESP32-S3, TMC2209 steppers, endstop homing, rotctld over Wi-Fi, EasyComm
   over USB, watchdog, soft limits, serial console calibration.
4. **Firmware v2**: AS5600 closed loop, fault latching and `\get_status`, setup page, OTA,
   mDNS discovery shown in the device panel.
5. **Hardware notes**: a wiring diagram and bill of materials for a SatNOGS-style mount
   carrying the Discovery Dish, and a calibration walkthrough (sun-pointing for the north
   offset: the sun's az/el from the same SGP4/ephemeris code, peak the noise level).

## Open questions

- Is a Discovery Dish on a printed SatNOGS-style mount stiff enough in wind? Worst case the
  firmware needs a holding-current setting that's higher when idle.
- Sensorless homing (StallGuard) vs. endstops: StallGuard saves wiring but is unreliable at
  low speed with big gear ratios. Probably endstops by default.
- Whether to track by repeated position targets (simple, what every client does) or by
  sending the pass ahead of time as a timed trajectory (smoother, survives network drops
  mid-pass). Start with position targets; the trajectory upload could be a later extension
  (`\set_track`).
