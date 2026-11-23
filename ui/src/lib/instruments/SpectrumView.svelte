<script lang="ts">
  import type { SpectrumFrame } from '../frames';
  import type { SpectrumSettings } from '../instruments.svelte';
  import { cssVar, fitCanvas, fmtDb, fmtHz, fmtNum } from '../plot';
  import { Waterfall } from './waterfall';

  let {
    settings,
    frame,
    rate,
    colour,
    onwidth,
  }: {
    settings: SpectrumSettings;
    frame: SpectrumFrame | null;
    rate: number | null;
    colour: string;
    onwidth: (px: number) => void;
  } = $props();

  let specCanvas = $state<HTMLCanvasElement>();
  let wfCanvas = $state<HTMLCanvasElement>();
  let size = $state({ w: 1, h: 1 });
  let hover = $state<number | null>(null);
  let wf: Waterfall | null = null;
  let wfError = $state<string | null>(null);

  const showSpec = $derived(settings.view !== 'waterfall');
  const showWf = $derived(settings.view !== 'spectrum');

  $effect(() => {
    if (!specCanvas) return;
    return fitCanvas(specCanvas, (w, h) => {
      size = { w, h };
      onwidth(w);
    });
  });

  $effect(() => {
    if (!wfCanvas) return;
    try {
      wf = new Waterfall(wfCanvas);
      wfError = null;
    } catch (e) {
      wfError = `The waterfall needs WebGL2 (${(e as Error).message}).`;
      return;
    }
    const stopFit = fitCanvas(wfCanvas, (w) => {
      if (!specCanvas) onwidth(w);
      wf?.draw(settings.refLevel, settings.range);
    });
    return () => {
      stopFit();
      wf?.destroy();
      wf = null;
    };
  });

  // A new frame becomes the newest waterfall row.
  $effect(() => {
    if (frame && wf) wf.push(frame.db);
  });

  $effect(() => {
    // Track the frame and the range so the waterfall redraws on either.
    void frame;
    if (wf && showWf) wf.draw(settings.refLevel, settings.range);
  });

  /** Frequency at fraction `fx` of the width (Hz, or cycles/sample without a rate). */
  function freqAt(fx: number): number {
    const fs = rate ?? 1;
    return frame && !frame.complex ? fx * (fs / 2) : (fx - 0.5) * fs;
  }

  const fmtF = (f: number) => (rate ? fmtHz(f) : `${fmtNum(f)} cyc/smp`);

  const peak = $derived.by(() => {
    if (!frame) return null;
    let k = 0;
    for (let i = 1; i < frame.db.length; i++) if (frame.db[i] > frame.db[k]) k = i;
    return { fx: (k + 0.5) / frame.db.length, db: frame.db[k] };
  });

  $effect(() => {
    const ctx = specCanvas?.getContext('2d');
    if (!ctx || !specCanvas) return;
    const { w, h } = size;
    const dpr = window.devicePixelRatio || 1;
    const f = frame;
    const ref = settings.refLevel;
    const range = Math.max(1, settings.range);
    const yOf = (db: number) => ((ref - db) / range) * h;
    ctx.fillStyle = '#0a0d11';
    ctx.fillRect(0, 0, w, h);
    // Grid: every 10 dB (or every range/10), and 10 frequency divisions.
    // At least ~22 CSS px between labelled lines.
    const dbStep = [5, 10, 20, 50, 100].find((st) => (st / range) * h >= 22 * dpr) ?? 100;
    ctx.lineWidth = 1;
    ctx.strokeStyle = '#1c232c';
    ctx.font = `${10 * dpr}px ${cssVar(specCanvas, '--mono')}`;
    ctx.fillStyle = '#8693a0';
    ctx.textBaseline = 'top';
    for (let db = Math.floor(ref / dbStep) * dbStep; db >= ref - range; db -= dbStep) {
      const y = Math.round(yOf(db)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
      if (y > 2 && y < h - 14 * dpr) ctx.fillText(`${db}`, 4 * dpr, y + 2 * dpr);
    }
    for (let i = 1; i < 10; i++) {
      const x = Math.round((i / 10) * w) + 0.5;
      ctx.strokeStyle = i === 5 && f?.complex ? '#2a323d' : '#1c232c';
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
      ctx.stroke();
    }
    if (!f) return;
    const n = f.db.length;
    const xOf = (i: number) => ((i + 0.5) / n) * w;
    const trace = cssVar(specCanvas, colour);
    if (f.peak) {
      ctx.strokeStyle = trace;
      ctx.globalAlpha = 0.45;
      ctx.lineWidth = 1 * dpr;
      ctx.beginPath();
      for (let i = 0; i < n; i++) ctx.lineTo(xOf(i), yOf(f.peak[i]));
      ctx.stroke();
      ctx.globalAlpha = 1;
    }
    ctx.beginPath();
    for (let i = 0; i < n; i++) ctx.lineTo(xOf(i), yOf(f.db[i]));
    ctx.strokeStyle = trace;
    ctx.lineWidth = 1.25 * dpr;
    ctx.stroke();
    ctx.lineTo(w, h);
    ctx.lineTo(0, h);
    ctx.closePath();
    ctx.globalAlpha = 0.12;
    ctx.fillStyle = trace;
    ctx.fill();
    ctx.globalAlpha = 1;
    if (hover !== null) {
      ctx.strokeStyle = '#e6edf3';
      ctx.setLineDash([4 * dpr, 3 * dpr]);
      const x = Math.round(hover * w) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
      ctx.stroke();
      ctx.setLineDash([]);
    }
  });

  const hoverDb = $derived.by(() => {
    if (hover === null || !frame) return null;
    const i = Math.min(frame.db.length - 1, Math.max(0, Math.floor(hover * frame.db.length)));
    return frame.db[i];
  });

  function onpointermove(e: PointerEvent) {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    hover = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
  }

  const ticks = $derived.by(() => {
    if (!frame) return [];
    return [0, 0.25, 0.5, 0.75, 1].map((fx) => fmtF(freqAt(fx)));
  });
</script>

<div class="spectrum">
  {#if showSpec}
    <div class="plot" class:grow={settings.view === 'spectrum'}>
      <canvas
        bind:this={specCanvas}
        {onpointermove}
        onpointerleave={() => (hover = null)}

        aria-label="Spectrum{peak ? `, strongest at ${fmtF(freqAt(peak.fx))}, ${fmtDb(peak.db)}` : ''}"
      ></canvas>
      {#if hover !== null && hoverDb !== null}
        <span class="label mono" style:left="{hover * 100}%">{fmtF(freqAt(hover))} {fmtDb(hoverDb)}</span>
      {:else if peak}
        <span class="label mono" style:left="{peak.fx * 100}%">▾ {fmtF(freqAt(peak.fx))} {fmtDb(peak.db)}</span>
      {/if}
    </div>
  {/if}
  {#if showWf}
    <div class="plot grow">
      <canvas bind:this={wfCanvas} aria-label="Waterfall: newest at the top"></canvas>
      {#if wfError}<p class="wf-error">{wfError}</p>{/if}
    </div>
  {/if}
  <div class="ticks mono">
    {#each ticks as t, i (i)}<span>{t}</span>{/each}
  </div>
</div>

<style>
  .spectrum {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .plot {
    position: relative;
    flex: 1 1 0;
    min-height: 40px;
    display: flex;
  }
  .plot.grow {
    flex: 1.3 1 0;
  }
  canvas {
    flex: 1;
    width: 100%;
    height: 100%;
    min-width: 0;
    min-height: 0;
    border-radius: 4px;
    cursor: crosshair;
  }
  .label {
    position: absolute;
    top: 4px;
    transform: translateX(6px);
    font-size: 10px;
    color: #ffd9a6;
    white-space: nowrap;
    pointer-events: none;
  }
  .wf-error {
    position: absolute;
    inset: 0;
    margin: 0;
    display: grid;
    place-items: center;
    color: var(--muted);
    font-size: 12px;
  }
  .ticks {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: var(--faint);
  }
</style>
