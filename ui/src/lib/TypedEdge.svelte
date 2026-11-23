<script lang="ts">
  import { BaseEdge, getBezierPath, useNodesData, type EdgeProps } from '@xyflow/svelte';
  import { portType } from './api';
  import type { BlockData } from './graph';
  import { session } from './session.svelte';
  import { indexErrors } from './errors';

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
  const [path] = $derived(getBezierPath({ sourceX, sourceY, targetX, targetY, sourcePosition, targetPosition }));
  const colour = $derived(error ? 'var(--err)' : type ? `var(--t-${type})` : 'var(--muted)');
</script>

{#if selected}
  <path d={path} fill="none" stroke={colour} stroke-width="9" opacity="0.2" />
{/if}
<BaseEdge
  {id}
  {path}
  style="stroke: {colour}; stroke-width: {selected ? 3 : 2}px;{error ? ' stroke-dasharray: 5 4;' : ''}"
  aria-label={error ?? `${type ?? ''} wire`}
/>
