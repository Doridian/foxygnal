# UI concept

Sketch: <https://claude.ai/artifact/4Yfp1PBSy1oBjfevL9daHW> (private until shared).

## Layout

```
┌ top bar ── project name · ▶ Run / ■ Stop · state · CPU · sample-rate badge ── ⚙ ┐
├ palette ─┬──────────────── graph canvas ────────────────┬─ inspector ─────────┤
│ search   │  blocks + wires, colour = stream type          │ selected block:     │
│ Sources  │  rate labels on wires (M3)                     │  params w/ units,   │
│ Filters  │  probe badges on watched wires                 │  live-edit dot,     │
│ Math     │  errors pinned to blocks                       │  help, ports        │
│ …        │                                                │ or selected wire:   │
│ Devices  │                                                │  type, rate, stats  │
├──────────┴──────────── instrument dock (resizable) ────────┴─────────────────────┤
│  Scope (tap: filter→out)   │  Spectrum + waterfall (tap: sum→out)  │  + add      │
└──────────────────────────────────────────────────────────────────────────────────┘
```

## Interaction principles

1. **Everything is discoverable from the canvas.** Click a wire to open a probe menu (Scope /
   Spectrum / Waterfall / Constellation / Meter). Hovering a wire shows type, rate and items/s.
2. **Colour means type**, consistently across ports, wires and the type badge in the
   inspector: c32 orange, f32 blue, i16 purple, u8 green. Colours also differ in lightness, so
   they stay distinguishable for colour-blind users.
3. **Mistakes explain themselves.** A rejected connection says why ("produces f32, expects
   c32") and offers to insert a converter. Runtime errors attach to the block with the message
   and a "show details" toggle.
4. **Numbers are human.** Inputs accept `2.4M`, `100k`, `-30dB` and display with SI prefixes.
   Sliders appear for params that have min/max.
5. **Live where possible.** Params that can change while running show a small live dot and
   apply instantly. Others show "applies on restart".
6. **Instruments are first-class.** They dock at the bottom, can split side by side, pop out
   into their own browser window (for a second monitor), and remember their settings per
   graph file.
7. **Fast to drive.** `/` searches blocks, `Space` runs/stops, `Del` deletes, `Ctrl+Z`
   undoes, and `F` fits the view.
8. **Empty state teaches.** A new graph offers templates ("Tone + noise → scope", "FM
   receiver from file", "RTL-SDR spectrum") instead of a blank page.

## Visual language

A dark theme by default (light theme later), with a calm UI and bright signals. Inter for UI
text and a monospace font for numbers and port names. Fox-orange accent. Signal traces use the
stream-type colours. Instruments render with WebGL2 at the display refresh rate. Data frames
arrive at the probe rate (30 fps by default).
