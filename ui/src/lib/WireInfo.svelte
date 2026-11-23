<script lang="ts">
  // What a wire carries and how busy it is, plus buttons to watch it. Used by
  // the wire popover and the inspector.
  import type { EdgeRef } from './api';
  import { graphView } from './graphview.svelte';
  import { instruments, type InstrumentKind, type SpectrumSettings } from './instruments.svelte';
  import { fmtRate } from './plot';
  import { session } from './session.svelte';
  import WatchButtons from './instruments/WatchButtons.svelte';

  let { edge, onwatch }: { edge: EdgeRef; onwatch?: (id: number) => void } = $props();

  const type = $derived(graphView.outputType(edge.from));
  const guessed = $derived(graphView.guessRate(edge.from));
  const stat = $derived(session.stats?.edges.get(`${edge.from}->${edge.to}`));
  const cpuOf = (endpoint: string) => session.stats?.cpu.get(endpoint.slice(0, endpoint.lastIndexOf('.')));

  function pick(kind: InstrumentKind, view?: SpectrumSettings['view']) {
    const id = instruments.add(kind, edge, view);
    onwatch?.(id);
  }
</script>

<div class="facts">
  <span class="muted">Type</span><span class="mono" style:color="var(--t-{type})">{type ?? '?'}</span>
  <span class="muted">Rate</span><span class="mono" title={guessed ? `From ${guessed.block}'s sample_rate` : undefined}>{guessed ? fmtRate(guessed.rate) : '—'}</span>
  {#if stat}
    <span class="muted">Flow</span><span class="mono">{fmtRate(stat.rate).replace('S/s', '/s')}</span>
    <span class="muted">Buffer</span><span class="mono" title="Items waiting for {edge.to}. Near 100% means that block is the bottleneck.">{Math.round(stat.fill * 100)}% full</span>
    {@const cpu = cpuOf(edge.to)}
    {#if cpu !== undefined}<span class="muted">CPU (dest.)</span><span class="mono">{(cpu * 100).toFixed(1)}%</span>{/if}
  {:else}
    <span class="muted">Flow</span><span class="muted small">while running</span>
  {/if}
</div>
<div class="label">Watch this wire</div>
<WatchButtons {type} onpick={pick} />

<style>
  .facts {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 4px 12px;
    font-size: 12px;
  }
  .small {
    font-size: 11px;
  }
  .label {
    margin-top: 10px;
    margin-bottom: 6px;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--muted);
  }
</style>
