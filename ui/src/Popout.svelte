<script lang="ts">
  // One instrument in its own window (e.g. on a second monitor). It has its
  // own engine connection for frames; settings sync with the main window.
  import { fetchBlocks } from './lib/api';
  import { graphView } from './lib/graphview.svelte';
  import { instruments, title } from './lib/instruments.svelte';
  import InstrumentPanel from './lib/instruments/InstrumentPanel.svelte';
  import { session } from './lib/session.svelte';

  let { id }: { id: number } = $props();

  // svelte-ignore state_referenced_locally
  instruments.startPopout(id);
  session.start((graph) => (graphView.spec = graph));
  fetchBlocks()
    .then((b) => (graphView.catalog = new Map(b.map((d) => [d.kind, d]))))
    .catch(() => {});

  const inst = $derived(instruments.get(id));
  $effect(() => {
    document.title = inst ? `${inst.id} · ${title(inst)} · foxygnal` : 'foxygnal';
  });
</script>

<main>
  {#if inst}
    <InstrumentPanel {inst} popout />
  {:else}
    <p class="muted">Waiting for the main foxygnal window… (instrument {id})</p>
  {/if}
</main>

<style>
  main {
    height: 100%;
    box-sizing: border-box;
    padding: 10px;
    display: flex;
  }
  p {
    margin: auto;
  }
</style>
