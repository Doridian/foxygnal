<script lang="ts">
  import { BaseEdge, EdgeLabel, getBezierPath, useNodesData, useViewport, type EdgeProps } from '@xyflow/svelte';
  import { portType } from './api';
  import type { BlockData } from './graph';
  import { session } from './session.svelte';
  import { indexErrors } from './errors';
  import { instruments } from './instruments.svelte';
  import { prefs } from './prefs.svelte';
  import { graphView } from './graphview.svelte';
  import { fmtRate, heat } from './plot';
  import { getContext } from 'svelte';

  // Hit area of at least ~24 screen pixels however far out the view is zoomed.
  const viewport = useViewport();
  const hitWidth = $derived(Math.max(28, 24 / (viewport.current.zoom || 1)));
  const wireclick = getContext<((id: string, e: MouseEvent) => void) | undefined>('wireclick');

  let { id, source, target, sourceHandleId, targetHandleId, sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition, selected }: EdgeProps = $props();

  const ends = useNodesData(() => ({ nodeIds: [source, target] }));
  function typeAt(nodeId: string, handle: string | null | undefined, dir: 'inputs' | 'outputs') {
    const data = ends.current.find((n) => n.id === nodeId)?.data as BlockData | undefined;
    const p = data?.desc[dir].find((p) => p.name === handle);
    return p && data ? portType(p, data.params) : null;
  }
  const type = $derived(typeAt(source, sourceHandleId, 'outputs'));
  const targetType = $derived(typeAt(target, targetHandleId, 'inputs'));
  // A parameter change can break a wire that was fine when it was drawn.
  const error = $derived(
    indexErrors(session.status.errors).edges.get(id) ??
      (type && targetType && type !== targetType ? `${type} → ${targetType}: type mismatch` : undefined),
  );
  const [path, labelX, labelY] = $derived(getBezierPath({ sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition }));
  const colour = $derived(error ? 'var(--err)' : type ? `var(--t-${type})` : 'var(--muted)');
  const from = $derived(`${source}.${sourceHandleId}`);
  const to = $derived(`${target}.${targetHandleId}`);
  const watchers = $derived(instruments.onEdge(from, to));
  const rate = $derived(graphView.rateOf(from));
  const stat = $derived(prefs.overlay ? session.stats?.edges.get(`${from}->${to}`) : undefined);
</script>

{#if selected}
  <path d={path} fill="none" stroke={colour} stroke-width="9" opacity="0.2" />
{/if}
{#if stat}
  <path d={path} fill="none" stroke={heat(stat.fill)} stroke-width="8" opacity="0.28" />
{/if}
<BaseEdge
  {id}
  {path}
  interactionWidth={hitWidth}
  style="stroke: {colour}; stroke-width: {selected ? 3 : 2}px;{error ? ' stroke-dasharray: 5 4;' : ''}"
  aria-label={error ?? `${type ?? ''} wire${watchers.length ? `, watched by instrument ${watchers.join(', ')}` : ''}`}
/>
{#if watchers.length || stat || rate}
  <EdgeLabel
    x={labelX}
    y={labelY}
    transparent
    class="wire-label"
    onclick={(e: MouseEvent) => {
      e.stopPropagation();
      wireclick?.(id, e);
    }}
  >
    <span class="label">
      {#each watchers as n (n)}<span class="badge" title="Instrument {n}">{n}</span>{/each}
      {#if rate && !stat}<span class="rate mono" title="Sample rate">{fmtRate(rate)}</span>{/if}
      {#if stat}
        <span class="stat mono" style:border-color={heat(stat.fill)} title="Items per second · buffer fill for {to}"
          >{fmtRate(stat.rate).replace('S/s', '/s')} · {Math.round(stat.fill * 100)}%</span
        >
      {/if}
    </span>
  </EdgeLabel>
{/if}

<style>
  :global(.svelte-flow__edge-label.wire-label) {
    cursor: pointer;
  }
  :global(.svelte-flow__edge:hover .svelte-flow__edge-path) {
    stroke-width: 4px !important;
  }
  /* xyflow draws the wide hit path with stroke-opacity 0 but no stroke, and
     `pointer-events: visibleStroke` ignores a path whose stroke is none: the
     hit area was only the 2px wire itself. An invisible paint fixes it. */
  :global(.svelte-flow__edge .svelte-flow__edge-interaction) {
    stroke: transparent;
    cursor: pointer;
  }
  .label {
    display: flex;
    align-items: center;
    gap: 4px;
    pointer-events: none;
  }
  .badge {
    width: 18px;
    height: 18px;
    display: grid;
    place-items: center;
    box-sizing: border-box;
    border-radius: 50%;
    background: var(--bg);
    border: 2px solid var(--text);
    color: var(--text);
    font-size: 10px;
    font-weight: 700;
  }
  .rate {
    padding: 0 4px;
    border-radius: 4px;
    background: color-mix(in srgb, var(--bg) 85%, transparent);
    color: var(--muted);
    font-size: 10px;
    white-space: nowrap;
    transform: translateY(-11px);
  }
  .stat {
    padding: 1px 6px;
    border: 1px solid;
    border-radius: 999px;
    background: var(--bg);
    color: var(--text);
    font-size: 10px;
    white-space: nowrap;
  }
</style>
