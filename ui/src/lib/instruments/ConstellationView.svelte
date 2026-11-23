<script lang="ts">
  import type { ConstellationFrame } from '../frames';
  import type { ConstellationSettings } from '../instruments.svelte';
  import { cssVar, fitCanvas, fmtNum } from '../plot';

  let { settings, frame, colour }: { settings: ConstellationSettings; frame: ConstellationFrame | null; colour: string } = $props();

  let canvas = $state<HTMLCanvasElement>();
  let size = $state({ w: 1, h: 1 });

  $effect(() => {
    if (!canvas) return;
    return fitCanvas(canvas, (w, h) => (size = { w, h }));
  });

  function grid(ctx: CanvasRenderingContext2D, cx: number, cy: number, r: number) {
    ctx.strokeStyle = '#2a323d';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(cx - r, cy + 0.5);
    ctx.lineTo(cx + r, cy + 0.5);
    ctx.moveTo(cx + 0.5, cy - r);
    ctx.lineTo(cx + 0.5, cy + r);
    ctx.stroke();
    ctx.strokeStyle = '#1c232c';
    ctx.strokeRect(cx - r + 0.5, cy - r + 0.5, 2 * r, 2 * r);
    ctx.beginPath();
    ctx.arc(cx, cy, r / 2, 0, Math.PI * 2);
    ctx.stroke();
  }

  // Resizing or rescaling starts from a clean plot.
  $effect(() => {
    const ctx = canvas?.getContext('2d');
    void settings.scale;
    if (!ctx) return;
    ctx.fillStyle = '#0a0d11';
    ctx.fillRect(0, 0, size.w, size.h);
  });

  $effect(() => {
    const ctx = canvas?.getContext('2d');
    const f = frame;
    if (!ctx || !canvas || !f) return;
    const { w, h } = size;
    const dpr = window.devicePixelRatio || 1;
    const r = Math.min(w, h) / 2 - 8 * dpr;
    const cx = Math.round(w / 2);
    const cy = Math.round(h / 2);
    // Persistence: fade what's there instead of clearing it.
    ctx.globalAlpha = 1 - Math.min(0.97, Math.max(0, settings.persistence));
    ctx.fillStyle = '#0a0d11';
    ctx.fillRect(0, 0, w, h);
    ctx.globalAlpha = 1;
    grid(ctx, cx, cy, r);
    ctx.fillStyle = cssVar(canvas, colour);
    const k = r / Math.max(1e-9, settings.scale);
    const d = Math.max(1, Math.round(1.5 * dpr));
    for (let i = 0; i + 1 < f.iq.length; i += 2) {
      ctx.fillRect(cx + f.iq[i] * k - d / 2, cy - f.iq[i + 1] * k - d / 2, d, d);
    }
  });
</script>

<div class="constellation">
  <canvas bind:this={canvas} aria-label="Constellation: I across, Q up, ±{fmtNum(settings.scale)} full scale"></canvas>
  <span class="scale mono">±{fmtNum(settings.scale)}</span>
</div>

<style>
  .constellation {
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
  }
  .scale {
    position: absolute;
    right: 8px;
    bottom: 6px;
    font-size: 10px;
    color: var(--faint);
  }
</style>
