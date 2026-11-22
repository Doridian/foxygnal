<script lang="ts">
  import { Handle, Position } from '@xyflow/svelte';
  import { portType, type BlockDescriptor } from './api';

  let { data, selected }: {
    data: { desc: BlockDescriptor; params: Record<string, unknown> };
    selected?: boolean;
  } = $props();

  const rowTop = (i: number) => 40 + i * 22;
</script>

<div class="block" class:selected style:min-height="{40 + Math.max(data.desc.inputs.length, data.desc.outputs.length, 1) * 22}px">
  <header>{data.desc.label}</header>
  {#each data.desc.inputs as p, i}
    <Handle type="target" position={Position.Left} id={p.name} style="top:{rowTop(i)}px; background: var(--t-{portType(p, data.params)})" />
    <span class="port in" style:top="{rowTop(i) - 8}px">{p.name}</span>
  {/each}
  {#each data.desc.outputs as p, i}
    <Handle type="source" position={Position.Right} id={p.name} style="top:{rowTop(i)}px; background: var(--t-{portType(p, data.params)})" />
    <span class="port out" style:top="{rowTop(i) - 8}px">{p.name} · {portType(p, data.params)}</span>
  {/each}
</div>

<style>
  .block {
    position: relative;
    min-width: 170px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 8px;
    color: var(--text);
    font-size: 12px;
  }
  .block.selected {
    border-color: var(--accent);
  }
  header {
    padding: 6px 10px;
    font-weight: 600;
    border-bottom: 1px solid var(--line);
  }
  .port {
    position: absolute;
    color: var(--muted);
    font: 11px ui-monospace, monospace;
  }
  .port.in {
    left: 12px;
  }
  .port.out {
    right: 12px;
  }
</style>
