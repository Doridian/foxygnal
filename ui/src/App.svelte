<script lang="ts">
  import { SvelteFlow, Background, Controls, type Node, type Edge } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import BlockNode from './lib/BlockNode.svelte';
  import { fetchBlocks, type BlockDescriptor } from './lib/api';

  type BlockData = { desc: BlockDescriptor; params: Record<string, unknown> };

  let catalog = $state<BlockDescriptor[]>([]);
  let loadError = $state<string | null>(null);
  let search = $state('');
  let nodes = $state.raw<Node<BlockData>[]>([]);
  let edges = $state.raw<Edge[]>([]);
  let counter = 0;

  const nodeTypes = { block: BlockNode };

  fetchBlocks()
    .then((b) => (catalog = b))
    .catch((e) => (loadError = String(e)));

  const categories = $derived.by(() => {
    const q = search.toLowerCase();
    const map = new Map<string, BlockDescriptor[]>();
    for (const d of catalog) {
      if (q && !`${d.label} ${d.kind} ${d.category}`.toLowerCase().includes(q)) continue;
      map.set(d.category, [...(map.get(d.category) ?? []), d]);
    }
    return [...map.entries()];
  });

  const selected = $derived(nodes.find((n) => n.selected));

  function addBlock(desc: BlockDescriptor) {
    const params = Object.fromEntries(desc.params.map((p) => [p.name, p.default]));
    const id = `${desc.kind}_${++counter}`;
    nodes = [...nodes, { id, type: 'block', position: { x: ((counter - 1) % 4) * 240, y: Math.floor((counter - 1) / 4) * 170 }, data: { desc, params } }];
  }

  function setParam(id: string, name: string, value: unknown) {
    nodes = nodes.map((n) => (n.id === id ? { ...n, data: { ...n.data, params: { ...n.data.params, [name]: value } } } : n));
  }
</script>

<div class="shell">
  <header class="topbar">
    <strong class="logo">foxygnal</strong>
    <span class="muted">untitled.fox.toml</span>
    <span class="spacer"></span>
    <button class="run" disabled title="Run control arrives in M1">▶ Run</button>
  </header>

  <aside class="palette">
    <input placeholder="Search blocks…" bind:value={search} />
    {#if loadError}
      <p class="error">Can't reach the engine: {loadError}. Is <code>foxygnal serve</code> running?</p>
    {/if}
    {#each categories as [cat, items]}
      <h3>{cat}</h3>
      {#each items as d}
        <button class="item" title={d.help} onclick={() => addBlock(d)}>{d.label}</button>
      {/each}
    {/each}
  </aside>

  <main class="canvas">
    <SvelteFlow bind:nodes bind:edges {nodeTypes} forceColorMode="dark">
      <Background />
      <Controls />
    </SvelteFlow>
  </main>

  <aside class="inspector">
    {#if selected}
      <h2>{selected.data.desc.label}</h2>
      <p class="muted">{selected.data.desc.help}</p>
      {#each selected.data.desc.params as p}
        <label>
          <span>{p.label}{p.kind === 'float' && p.unit ? ` (${p.unit})` : ''}</span>
          {#if p.kind === 'enum'}
            <select value={selected.data.params[p.name]} onchange={(e) => setParam(selected.id, p.name, e.currentTarget.value)}>
              {#each p.options as o}<option>{o}</option>{/each}
            </select>
          {:else if p.kind === 'bool'}
            <input type="checkbox" checked={!!selected.data.params[p.name]} onchange={(e) => setParam(selected.id, p.name, e.currentTarget.checked)} />
          {:else if p.kind === 'float' || p.kind === 'int'}
            <input type="number" value={selected.data.params[p.name]} onchange={(e) => setParam(selected.id, p.name, Number(e.currentTarget.value))} />
          {:else}
            <input value={selected.data.params[p.name]} onchange={(e) => setParam(selected.id, p.name, e.currentTarget.value)} />
          {/if}
        </label>
      {/each}
    {:else}
      <p class="muted">Select a block to edit its parameters.</p>
    {/if}
  </aside>
</div>

<style>
  .shell {
    display: grid;
    grid-template: 'top top top' 44px 'pal canvas insp' 1fr / 220px 1fr 280px;
    height: 100%;
  }
  .topbar {
    grid-area: top;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 0 14px;
    background: var(--panel);
    border-bottom: 1px solid var(--line);
  }
  .logo {
    color: var(--accent);
  }
  .spacer {
    flex: 1;
  }
  .muted {
    color: var(--muted);
  }
  .run {
    background: var(--ok);
    color: #04140a;
    border: 0;
    border-radius: 6px;
    padding: 6px 14px;
    font-weight: 600;
  }
  .run:disabled {
    opacity: 0.5;
  }
  .palette,
  .inspector {
    background: var(--panel);
    padding: 12px;
    overflow: auto;
  }
  .palette {
    grid-area: pal;
    border-right: 1px solid var(--line);
  }
  .inspector {
    grid-area: insp;
    border-left: 1px solid var(--line);
  }
  .canvas {
    grid-area: canvas;
    min-width: 0;
  }
  h3 {
    margin: 14px 0 6px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  h2 {
    margin: 0 0 4px;
    font-size: 16px;
  }
  .item {
    display: block;
    width: 100%;
    text-align: left;
    padding: 6px 8px;
    margin: 2px 0;
    background: transparent;
    color: var(--text);
    border: 1px solid transparent;
    border-radius: 6px;
    cursor: pointer;
  }
  .item:hover {
    background: var(--panel-2);
    border-color: var(--line);
  }
  input,
  select {
    width: 100%;
    box-sizing: border-box;
    padding: 6px 8px;
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: 6px;
  }
  input[type='checkbox'] {
    width: auto;
  }
  label {
    display: grid;
    gap: 4px;
    margin: 10px 0;
    font-size: 12px;
    color: var(--muted);
  }
  .error {
    color: #ff7b72;
    font-size: 12px;
  }
</style>
