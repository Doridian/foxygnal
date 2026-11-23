<script lang="ts">
  import type { BlockDescriptor, DeviceInfo } from './api';
  import { devices } from './devices.svelte';

  let {
    catalog,
    loadError,
    onadd,
    ondevice,
    search = $bindable(''),
    input = $bindable(),
  }: {
    catalog: BlockDescriptor[];
    loadError: string | null;
    onadd: (d: BlockDescriptor) => void;
    /** Add a receiver for a discovered device. */
    ondevice: (d: DeviceInfo) => void;
    search?: string;
    input?: HTMLInputElement;
  } = $props();

  const ORDER = ['Hardware', 'Sources', 'Filters', 'Resamplers', 'Demodulators', 'Level', 'Math', 'Converters', 'Flow', 'Sinks', 'Graphs'];

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

  let devicesOpen = $state(true);
  $effect(() => {
    if (devicesOpen) devices.ensure();
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
  <section class="devices" aria-label="Devices">
    <div class="dhead">
      <button class="toggle" aria-expanded={devicesOpen} onclick={() => (devicesOpen = !devicesOpen)}>
        <svg class:open={devicesOpen} width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" aria-hidden="true"><path d="M9 6l6 6-6 6"></path></svg>
        Devices
      </button>
      {#if devicesOpen}
        <button class="refresh" title="Search for devices again" aria-label="Refresh devices" disabled={devices.loading} onclick={() => devices.refresh()}>
          <svg class:spin={devices.loading} width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"></path><path d="M21 3v5h-5"></path></svg>
        </button>
      {/if}
    </div>
    {#if devicesOpen}
      {#if devices.error}
        <p class="derr">Can't list devices: {devices.error}</p>
      {:else if devices.drivers === null}
        <p class="dnote">Searching…</p>
      {:else}
        {#each devices.all as d (d.args)}
          <div class="device" title={`${d.label}\n${d.args}`}>
            <div class="dtext">
              <span class="dlabel">{d.label}</span>
              <span class="dargs mono">{d.args}{d.tx ? ' · RX/TX' : ''}</span>
            </div>
            {#if d.rx}
              <button class="add" title="Add an SDR Source for this device" onclick={() => ondevice(d)}>Add</button>
            {/if}
          </div>
        {:else}
          <p class="dnote">No devices found.</p>
        {/each}
        {#each devices.drivers.filter((d) => d.error) as d (d.driver)}
          <p class="dnote" title={d.error}>{d.label}: {d.error}</p>
        {/each}
        <p class="dnote">Network receivers: add an SDR Source and type e.g. <code>rtltcp:host=…,port=1234</code>.</p>
      {/if}
    {/if}
  </section>
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
  .devices {
    margin-top: 8px;
    padding-bottom: 6px;
    border-bottom: 1px solid var(--line);
  }
  .dhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 0;
    border: 0;
    background: transparent;
    color: var(--muted);
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .toggle svg {
    transition: transform 0.15s;
  }
  .toggle svg.open {
    transform: rotate(90deg);
  }
  .refresh {
    display: grid;
    place-items: center;
    width: 22px;
    height: 22px;
    padding: 0;
    border: 1px solid transparent;
    border-radius: 4px;
    background: transparent;
    color: var(--muted);
  }
  .refresh:hover:not(:disabled) {
    border-color: var(--line);
    color: var(--text);
  }
  .spin {
    animation: spin 1s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  .device {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 6px 4px 8px;
    border-radius: 6px;
  }
  .device:hover {
    background: var(--panel-2);
  }
  .dtext {
    display: flex;
    flex-direction: column;
    min-width: 0;
    flex: 1;
  }
  .dlabel,
  .dargs {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dargs {
    font-size: 10px;
    color: var(--faint);
  }
  .add {
    padding: 3px 8px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: transparent;
    color: var(--accent);
    font-size: 11px;
  }
  .add:hover {
    border-color: var(--accent);
  }
  .dnote,
  .derr {
    margin: 4px 0 0;
    font-size: 11px;
    color: var(--faint);
    overflow-wrap: anywhere;
  }
  .derr {
    color: var(--err);
  }
</style>
