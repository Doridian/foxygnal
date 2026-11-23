<script lang="ts">
  import type { BlockDescriptor } from './api';

  let {
    catalog,
    loadError,
    onadd,
    search = $bindable(''),
    input = $bindable(),
  }: {
    catalog: BlockDescriptor[];
    loadError: string | null;
    onadd: (d: BlockDescriptor) => void;
    search?: string;
    input?: HTMLInputElement;
  } = $props();

  const ORDER = ['Sources', 'Filters', 'Demodulators', 'Math', 'Flow', 'Sinks'];

  const matches = $derived.by(() => {
    const q = search.trim().toLowerCase();
    return catalog.filter((d) => !q || `${d.label} ${d.kind} ${d.category} ${d.help}`.toLowerCase().includes(q));
  });

  const categories = $derived.by(() => {
    const map = new Map<string, BlockDescriptor[]>();
    for (const d of matches) map.set(d.category, [...(map.get(d.category) ?? []), d]);
    const rank = (c: string) => (ORDER.includes(c) ? ORDER.indexOf(c) : ORDER.length);
    return [...map.entries()].sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b));
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && matches[0]) {
      onadd(matches[0]);
      search = '';
      input?.blur();
    } else if (e.key === 'Escape') {
      search = '';
      input?.blur();
    }
  }

  function ondragstart(e: DragEvent, d: BlockDescriptor) {
    e.dataTransfer?.setData('application/x-foxygnal-block', d.kind);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = 'copy';
  }
</script>

<aside class="palette" aria-label="Block palette">
  <label class="search">
    <span class="sr-only">Search blocks</span>
    <input bind:this={input} bind:value={search} {onkeydown} type="text" placeholder="Search blocks" />
    <kbd>/</kbd>
  </label>
  {#if loadError}
    <p class="error">Can't reach the engine: {loadError}. Is <code>foxygnal serve</code> running?</p>
  {/if}
  {#each categories as [cat, items] (cat)}
    <h3>{cat}</h3>
    {#each items as d (d.kind)}
      <button class="item" title={d.help} draggable="true" ondragstart={(e) => ondragstart(e, d)} onclick={() => onadd(d)}>{d.label}</button>
    {/each}
  {:else}
    {#if catalog.length}<p class="muted">No block matches “{search}”.</p>{/if}
  {/each}
  <p class="hint">Click or drag a block onto the canvas. Enter adds the first match.</p>
</aside>

<style>
  .palette {
    background: var(--panel);
    border-right: 1px solid var(--line);
    padding: 12px;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .search {
    position: relative;
    display: block;
  }
  .search input {
    padding-right: 30px;
  }
  kbd {
    position: absolute;
    right: 8px;
    top: 7px;
    padding: 0 6px;
    border: 1px solid var(--line);
    border-radius: 4px;
    color: var(--muted);
    font: 11px var(--mono);
  }
  h3 {
    margin: 12px 0 4px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .item {
    display: block;
    width: 100%;
    text-align: left;
    padding: 6px 8px;
    background: transparent;
    color: var(--text);
    border: 1px solid transparent;
    border-radius: 6px;
    cursor: grab;
  }
  .item:hover,
  .item:focus-visible {
    background: var(--panel-2);
    border-color: var(--line);
  }
  .hint {
    margin-top: auto;
    padding-top: 16px;
    font-size: 11px;
    color: var(--faint);
  }
  .error {
    color: var(--err);
    font-size: 12px;
  }
</style>
