<script lang="ts">
  // Frequency-response preview for any block whose descriptor says
  // `response: true`. The engine designs the filter from the block's params
  // and the sample rate on its input wire; this only draws the result.
  import { evaluateValue, type Scope } from './expr';
  import { fetchResponse, type BlockDescriptor, type FrequencyResponse } from './api';
  import { graphView } from './graphview.svelte';
  import { cssVar, fitCanvas, fmtDb, fmtHz, fmtRate } from './plot';

  let { id, desc, params, scope }: { id: string; desc: BlockDescriptor; params: Record<string, unknown>; scope: Scope } = $props();

  /** Params with `$var` expressions evaluated, as the engine would see them. */
  function resolved(raw: Record<string, unknown>): { params: Record<string, unknown> } | { error: string } {
    const out: Record<string, unknown> = { ...raw };
    for (const spec of desc.params) {
      const v = raw[spec.name];
      if ((spec.kind === 'float' || spec.kind === 'int') && typeof v === 'string') {
        const r = evaluateValue(v, scope);
        if (r.error !== undefined) return { error: `${spec.name}: ${r.error}` };
        out[spec.name] = r.value;
      }
    }
    return { params: out };
  }

  /** The sample rate arriving at the block's first input, if known. */
  const rate = $derived.by(() => {
    const input = desc.inputs[0];
    if (!input) return null;
    const c = graphView.spec.connect.find((c) => c.to === `${id}.${input.name}` || c.to === `${id}.0`);
    if (!c) return null;
    const direct = graphView.rateOf(c.from);
    if (direct !== null) return direct;
    // The wire may name its source port by index.
    const dot = c.from.lastIndexOf('.');
    const src = graphView.spec.block.find((b) => b.id === c.from.slice(0, dot));
    const port = c.from.slice(dot + 1);
    const name = src && /^\d+$/.test(port) ? graphView.catalog.get(src.kind)?.outputs[Number(port)]?.name : undefined;
    return name ? graphView.rateOf(`${src!.id}.${name}`) : null;
  });

  let resp = $state.raw<FrequencyResponse | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(false);

  // Fetch (debounced) whenever the params or the input rate change. A newer
  // request supersedes an older one still in flight.
  let seq = 0;
  $effect(() => {
    const r = rate;
    const res = resolved($state.snapshot(params));
    const kind = desc.kind;
    const mine = ++seq;
    if (r === null || 'error' in res) {
      resp = null;
      error = 'error' in res ? res.error : null;
      return;
    }
    const p = res.params;
    loading = true;
    const t = setTimeout(async () => {
      try {
        const out = await fetchResponse(kind, p, r);
        if (mine !== seq) return;
        resp = out;
        error = null;
      } catch (e) {
        if (mine !== seq) return;
        resp = null;
        error = (e as Error).message;
      } finally {
        if (mine === seq) loading = false;
      }
    }, 200);
    return () => clearTimeout(t);
  });

  let canvas = $state<HTMLCanvasElement>();
  let size = $state({ w: 1, h: 1 });
  let hover = $state<number | null>(null);

  $effect(() => {
    if (!canvas) return;
    return fitCanvas(canvas, (w, h) => (size = { w, h }));
  });

  /** dB axis: 10 dB above the peak (rounded) down 110 dB. */
  const axis = $derived.by(() => {
    if (!resp) return { top: 10, bottom: -100 };
    const peak = Math.max(...resp.db);
    const top = Math.ceil((peak + 5) / 10) * 10;
    return { top, bottom: top - 110 };
  });

  const fmin = $derived(resp ? resp.freqs[0] : 0);
  const fmax = $derived(resp ? resp.freqs[resp.freqs.length - 1] : 1);

  $effect(() => {
    const ctx = canvas?.getContext('2d');
    if (!ctx || !canvas) return;
    const { w, h } = size;
    const dpr = window.devicePixelRatio || 1;
    const { top, bottom } = axis;
    const yOf = (db: number) => ((top - Math.max(bottom, db)) / (top - bottom)) * h;
    ctx.fillStyle = '#0a0d11';
    ctx.fillRect(0, 0, w, h);
    ctx.lineWidth = 1;
    ctx.strokeStyle = '#1c232c';
    ctx.font = `${10 * dpr}px ${cssVar(canvas, '--mono')}`;
    ctx.fillStyle = '#8693a0';
    ctx.textBaseline = 'top';
    for (let db = top; db >= bottom; db -= 20) {
      const y = Math.round(yOf(db)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
      if (y < h - 12 * dpr) ctx.fillText(`${db}`, 4 * dpr, y + 2 * dpr);
    }
    for (let i = 1; i < 4; i++) {
      const x = Math.round((i / 4) * w) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
      ctx.stroke();
    }
    const r = resp;
    if (!r) return;
    const span = fmax - fmin || 1;
    const xOf = (f: number) => ((f - fmin) / span) * w;
    // −3 dB below the peak: where the passband ends.
    ctx.strokeStyle = '#2a323d';
    ctx.setLineDash([3 * dpr, 3 * dpr]);
    const y3 = Math.round(yOf(Math.max(...r.db) - 3)) + 0.5;
    ctx.beginPath();
    ctx.moveTo(0, y3);
    ctx.lineTo(w, y3);
    ctx.stroke();
    ctx.setLineDash([]);
    const trace = cssVar(canvas, '--accent');
    ctx.beginPath();
    r.freqs.forEach((f, i) => ctx.lineTo(xOf(f), yOf(r.db[i])));
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

  const hoverPoint = $derived.by(() => {
    if (hover === null || !resp) return null;
    const n = resp.freqs.length;
    const i = Math.min(n - 1, Math.max(0, Math.round(hover * (n - 1))));
    return { f: resp.freqs[i], db: resp.db[i] };
  });

  function onpointermove(e: PointerEvent) {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    hover = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
  }

  const ticks = $derived(resp ? [0, 0.5, 1].map((fx) => fmtHz(fmin + fx * (fmax - fmin))) : []);
</script>

<section class="response" aria-label="Frequency response">
  <div class="head">
    <span class="small">Frequency response</span>
    <span class="mono muted small">
      {#if resp}{resp.taps > 0 ? `${resp.taps} taps` : 'IIR'}{/if}
      {#if rate !== null}· {fmtRate(rate)}{/if}
    </span>
  </div>
  {#if rate === null}
    <p class="muted small note">Connect an input with a known sample rate to preview the response.</p>
  {:else if error}
    <p class="problem">{error}</p>
  {:else}
    <div class="plot" class:stale={loading}>
      <canvas
        bind:this={canvas}
        {onpointermove}
        onpointerleave={() => (hover = null)}
        aria-label="Magnitude response from {fmtHz(fmin)} to {fmtHz(fmax)}{resp ? (resp.taps > 0 ? `, ${resp.taps} taps` : ', IIR') : ''}"
      ></canvas>
      {#if hoverPoint}
        <span class="label mono" style:left="{(hover ?? 0) * 100}%" class:flip={(hover ?? 0) > 0.6}>
          {fmtHz(hoverPoint.f)} {fmtDb(hoverPoint.db)}
        </span>
      {/if}
    </div>
    <div class="ticks mono">
      {#each ticks as t, i (i)}<span>{t}</span>{/each}
    </div>
  {/if}
</section>

<style>
  .response {
    border-top: 1px solid var(--line);
    padding-top: 10px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    align-items: baseline;
    gap: 2px 8px;
  }
  .head > span {
    white-space: nowrap;
  }
  .small {
    font-size: 12px;
  }
  .note {
    margin: 0;
  }
  .problem {
    margin: 0;
    font-size: 12px;
    color: var(--err);
    line-height: 1.4;
  }
  .plot {
    position: relative;
    height: 150px;
    display: flex;
    transition: opacity 0.15s;
  }
  .plot.stale {
    opacity: 0.6;
  }
  canvas {
    flex: 1;
    width: 100%;
    height: 100%;
    min-width: 0;
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
  .label.flip {
    transform: translateX(calc(-100% - 6px));
  }
  .ticks {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: var(--faint);
  }
</style>
