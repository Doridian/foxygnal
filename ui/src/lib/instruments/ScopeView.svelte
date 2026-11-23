<script lang="ts">
  import type { ScopeFrame } from '../frames';
  import { SCOPE_DIVS, type ScopeSettings } from '../instruments.svelte';
  import { cssVar, fitCanvas, fmtNum, fmtTime, fmtHz } from '../plot';
  import { channelColour } from './colours';

  let {
    settings,
    frame,
    rate,
    colour,
    onchange,
    onwidth,
  }: {
    settings: ScopeSettings;
    frame: ScopeFrame | null;
    rate: number | null;
    /** Colour of the first channel (the stream type colour). */
    colour: string;
    onchange: (patch: Partial<ScopeSettings>) => void;
    onwidth: (px: number) => void;
  } = $props();

  const VDIVS = 8;
  let canvas = $state<HTMLCanvasElement>();
  let size = $state({ w: 1, h: 1 });
  /** Cursor positions as fractions of the width. */
  let cur = $state({ a: 0.3, b: 0.6 });
  let drag: null | 'a' | 'b' | 'level' = null;

  $effect(() => {
    if (!canvas) return;
    return fitCanvas(canvas, (w, h) => {
      size = { w, h };
      onwidth(w);
    });
  });

  const yOf = (v: number) => size.h / 2 - ((v - settings.offset) / settings.voltsPerDiv) * (size.h / VDIVS);
  const vOf = (y: number) => settings.offset + ((size.h / 2 - y) / (size.h / VDIVS)) * settings.voltsPerDiv;

  $effect(() => {
    const ctx = canvas?.getContext('2d');
    if (!ctx || !canvas) return;
    const { w, h } = size;
    const dpr = window.devicePixelRatio || 1;
    // Read everything the drawing depends on, so the effect tracks it.
    const f = frame;
    const s = { ...settings };
    const c = { ...cur };
    const col = (ch: Parameters<typeof channelColour>[0]) => channelColour(ch, colour, canvas!);
    ctx.fillStyle = '#0a0d11';
    ctx.fillRect(0, 0, w, h);
    // Grid.
    ctx.lineWidth = 1;
    for (let i = 0; i <= SCOPE_DIVS; i++) {
      const x = Math.round((i / SCOPE_DIVS) * (w - 1)) + 0.5;
      ctx.strokeStyle = i === SCOPE_DIVS / 2 ? '#2a323d' : '#1c232c';
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
      ctx.stroke();
    }
    for (let i = 0; i <= VDIVS; i++) {
      const y = Math.round((i / VDIVS) * (h - 1)) + 0.5;
      ctx.strokeStyle = i === VDIVS / 2 ? '#2a323d' : '#1c232c';
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }
    // Trigger level, with a handle to drag on the left edge.
    if (s.trigger !== 'off') {
      const y = Math.round(yOf(s.level)) + 0.5;
      ctx.strokeStyle = col(s.triggerChannel);
      ctx.globalAlpha = 0.5;
      ctx.setLineDash([3 * dpr, 5 * dpr]);
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.globalAlpha = 1;
      ctx.fillStyle = ctx.strokeStyle;
      ctx.beginPath();
      ctx.moveTo(0, y - 6 * dpr);
      ctx.lineTo(9 * dpr, y);
      ctx.lineTo(0, y + 6 * dpr);
      ctx.fill();
    }
    if (f) {
      // Traces, last channel first so the first one is on top.
      ctx.lineWidth = 1.5 * dpr;
      ctx.lineJoin = 'round';
      for (const ch of [...f.channels].reverse()) {
        ctx.strokeStyle = col(ch.id);
        ctx.beginPath();
        const d = ch.data;
        if (f.minmax) {
          // Each column spans min..max, which draws the envelope.
          for (let p = 0; p < f.points; p++) {
            const x = ((p + 0.5) / f.points) * w;
            ctx.lineTo(x, yOf(d[2 * p + 1]));
            ctx.lineTo(x, yOf(d[2 * p]));
          }
        } else {
          const n = f.points;
          for (let i = 0; i < n; i++) ctx.lineTo((i / Math.max(1, n - 1)) * w, yOf(d[i]));
        }
        ctx.stroke();
      }
      // Trigger position.
      if (f.trigger >= 0) {
        const x = (f.trigger / f.span) * w;
        ctx.fillStyle = col(s.triggerChannel);
        ctx.beginPath();
        ctx.moveTo(x - 6 * dpr, 0);
        ctx.lineTo(x + 6 * dpr, 0);
        ctx.lineTo(x, 9 * dpr);
        ctx.fill();
      }
    }
    if (s.cursors) {
      ctx.strokeStyle = '#e6edf3';
      ctx.lineWidth = 1;
      ctx.setLineDash([4 * dpr, 3 * dpr]);
      for (const fx of [c.a, c.b]) {
        const x = Math.round(fx * w) + 0.5;
        ctx.beginPath();
        ctx.moveTo(x, 0);
        ctx.lineTo(x, h);
        ctx.stroke();
      }
      ctx.setLineDash([]);
    }
    // Values at the top, middle and bottom grid lines.
    ctx.font = `${10 * dpr}px ${cssVar(canvas, '--mono')}`;
    ctx.fillStyle = '#8693a0';
    ctx.textBaseline = 'top';
    for (const i of [0, VDIVS / 2, VDIVS]) {
      const v = s.offset + (VDIVS / 2 - i) * s.voltsPerDiv;
      ctx.fillText(fmtNum(v), 12 * dpr, Math.min(h - 12 * dpr, (i / VDIVS) * h + 3 * dpr));
    }
  });

  /** Seconds (or samples) from the trigger point to fraction `fx` of the window. */
  function timeAt(fx: number): number {
    const span = frame?.span ?? 0;
    const t0 = frame && frame.trigger >= 0 ? frame.trigger : 0;
    const samples = fx * span - t0;
    return rate ? samples / rate : samples;
  }

  function valueAt(fx: number): number | null {
    const ch = frame?.channels[0];
    if (!frame || !ch) return null;
    const n = frame.points;
    const p = Math.min(n - 1, Math.max(0, frame.minmax ? Math.floor(fx * n) : Math.round(fx * (n - 1))));
    return frame.minmax ? (ch.data[2 * p] + ch.data[2 * p + 1]) / 2 : ch.data[p];
  }

  const fmtT = (t: number) => (rate ? fmtTime(t) : `${fmtNum(t)} smp`);
  const dt = $derived(timeAt(cur.b) - timeAt(cur.a));

  function pos(e: PointerEvent) {
    const r = canvas!.getBoundingClientRect();
    return { fx: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) * (size.h / r.height), px: e.clientX - r.left };
  }

  function onpointerdown(e: PointerEvent) {
    const p = pos(e);
    if (settings.trigger !== 'off' && p.px < 16) drag = 'level';
    else if (settings.cursors) drag = Math.abs(p.fx - cur.a) <= Math.abs(p.fx - cur.b) ? 'a' : 'b';
    else return;
    canvas!.setPointerCapture(e.pointerId);
    onpointermove(e);
  }

  function onpointermove(e: PointerEvent) {
    if (!drag) return;
    const p = pos(e);
    if (drag === 'level') onchange({ level: Number(vOf(p.y).toPrecision(3)) });
    else cur = { ...cur, [drag]: Math.max(0, Math.min(1, p.fx)) };
  }
</script>

<div class="scope">
  <canvas
    bind:this={canvas}
    {onpointerdown}
    {onpointermove}
    onpointerup={() => (drag = null)}
    class:grab={settings.cursors}
    aria-label="Oscilloscope trace. Drag the marker on the left edge to set the trigger level."

  ></canvas>
  {#if settings.cursors && frame}
    {@const va = valueAt(cur.a)}
    {@const vb = valueAt(cur.b)}
    <div class="readout mono">
      <span>A {fmtT(timeAt(cur.a))}{va !== null ? ` · ${fmtNum(va)}` : ''}</span>
      <span>B {fmtT(timeAt(cur.b))}{vb !== null ? ` · ${fmtNum(vb)}` : ''}</span>
      <span
        >Δ {fmtT(dt)}{rate && dt !== 0 ? ` · ${fmtHz(1 / Math.abs(dt))}` : ''}{va !== null && vb !== null ? ` · Δv ${fmtNum(vb - va)}` : ''}</span
      >
    </div>
  {/if}
</div>

<style>
  .scope {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
  }
  canvas {
    flex: 1;
    width: 100%;
    height: 100%;
    min-width: 0;
    min-height: 0;
    border-radius: 4px;
    touch-action: none;
  }
  canvas.grab {
    cursor: ew-resize;
  }
  .readout {
    position: absolute;
    right: 8px;
    top: 6px;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 2px;
    padding: 4px 8px;
    font-size: 11px;
    background: rgba(14, 17, 22, 0.85);
    border: 1px solid var(--line);
    border-radius: 6px;
    pointer-events: none;
  }
</style>
