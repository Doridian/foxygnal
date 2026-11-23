<script lang="ts">
  import type { StreamType } from '../api';
  import type { InstrumentKind, SpectrumSettings } from '../instruments.svelte';

  let { type, onpick }: { type: StreamType | null; onpick: (kind: InstrumentKind, view?: SpectrumSettings['view']) => void } = $props();

  const complex = $derived(type === 'c32');
  const bytes = $derived(type === 'u8');
</script>

<div class="watch" role="group" aria-label="Watch this wire">
  <button type="button" onclick={() => onpick('scope')}>Scope</button>
  <button type="button" onclick={() => onpick('spectrum', 'spectrum')}>Spectrum</button>
  <button type="button" onclick={() => onpick('spectrum', 'waterfall')}>Waterfall</button>
  <button type="button" onclick={() => onpick('meter')}>Level meter</button>
  <button type="button" onclick={() => onpick('constellation')} disabled={!complex}>Constellation</button>
  <button type="button" onclick={() => onpick('text')} disabled={!bytes} title="Text lines, e.g. a decoder's messages">Messages</button>
  <button type="button" onclick={() => onpick('image')} disabled={!bytes} title="Pictures, e.g. an SSTV decoder's image output">Image</button>
</div>
{#if bytes}
  <p class="note">Messages shows the text a decoder writes on this wire; Image the pictures (an SSTV decoder's <code>image</code> output).</p>
{:else if !complex}
  <p class="note">Constellation needs a complex (c32) wire; Messages and Image a u8 wire from a decoder.</p>
{/if}

<style>
  .watch {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 6px;
  }
  button {
    padding: 8px;
    min-height: 36px;
    border-radius: 6px;
    border: 1px solid var(--line);
    background: var(--bg);
    color: var(--text);
    font-size: 12px;
    text-align: left;
  }
  button:hover:not(:disabled) {
    border-color: var(--accent);
  }
  button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .note {
    margin: 6px 0 0;
    font-size: 11px;
    color: var(--faint);
  }
</style>
