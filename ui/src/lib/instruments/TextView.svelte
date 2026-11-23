<script lang="ts">
  // Messages: the text lines of a u8 wire (a decoder's output), newest at the
  // bottom. Follows new lines unless scrolled up; filterable.
  import { tick } from 'svelte';
  import type { TextSettings } from '../instruments.svelte';

  let {
    settings,
    lines,
    onclear,
    onchange,
  }: {
    settings: TextSettings;
    /** Received lines, oldest first (`null` marks lines lost in between). */
    lines: (string | null)[];
    onclear: () => void;
    onchange: (patch: Partial<TextSettings>) => void;
  } = $props();

  let box = $state<HTMLElement>();
  let follow = $state(true);

  const shown = $derived.by(() => {
    const f = settings.filter.trim().toLowerCase();
    return f ? lines.filter((l) => l === null || l.toLowerCase().includes(f)) : lines;
  });

  $effect(() => {
    void shown;
    if (!follow) return;
    tick().then(() => box && (box.scrollTop = box.scrollHeight));
  });

  function onscroll() {
    if (box) follow = box.scrollTop + box.clientHeight >= box.scrollHeight - 4;
  }
</script>

<div class="text">
  <div class="bar">
    <input type="search" placeholder="Filter" aria-label="Filter messages" value={settings.filter} oninput={(e) => onchange({ filter: e.currentTarget.value })} />
    <span class="count mono">{shown.filter((l) => l !== null).length} lines</span>
    {#if !follow}
      <button type="button" onclick={() => (follow = true)}>Follow</button>
    {/if}
    <button type="button" onclick={onclear} disabled={!lines.length}>Clear</button>
  </div>
  <div class="log mono" bind:this={box} {onscroll} role="log" aria-live="off">
    {#each shown as line, i (i)}
      {#if line === null}
        <div class="gap">… some messages were lost here</div>
      {:else}
        <div>{line}</div>
      {/if}
    {/each}
  </div>
</div>

<style>
  .text {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  input {
    flex: 1;
    min-width: 0;
    padding: 4px 8px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--bg);
    color: var(--text);
    font-size: 12px;
  }
  .count {
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
  }
  button {
    padding: 3px 10px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: transparent;
    color: var(--text);
    font-size: 11px;
  }
  button:disabled {
    opacity: 0.45;
  }
  .log {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: 6px 8px;
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: 4px;
    font-size: 11.5px;
    line-height: 1.5;
    white-space: pre;
  }
  .gap {
    color: var(--faint);
    font-style: italic;
  }
</style>
