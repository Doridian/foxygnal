<script lang="ts">
  // Plays every `browser_audio` block while the graph runs, and shows a
  // small volume control. See audio.svelte.ts.
  import { browserAudio } from './audio.svelte';
  import { graphView } from './graphview.svelte';
  import type { BlockNode, Wire } from './graph';
  import { session } from './session.svelte';

  let { nodes, edges }: { nodes: BlockNode[]; edges: Wire[] } = $props();

  // Probe ids well clear of the instruments' ids.
  const BASE = 0x7f000000;

  const running = $derived(session.status.state === 'running');
  /** The wire into each browser audio block, with its sample rate. */
  const feeds = $derived(
    nodes
      .filter((n) => n.data.desc.kind === 'browser_audio')
      .map((n) => {
        const e = edges.find((w) => w.target === n.id);
        if (!e) return null;
        const from = `${e.source}.${e.sourceHandle}`;
        return { block: n.id, from, to: `${e.target}.${e.targetHandle}`, rate: graphView.rateOf(from) };
      })
      .filter((f) => f !== null && f.rate !== null) as { block: string; from: string; to: string; rate: number }[],
  );

  const key = $derived(running ? feeds.map((f) => `${f.from}>${f.to}@${f.rate}`).join('|') : '');

  $effect(() => {
    if (!key) return;
    const ids: number[] = [];
    feeds.forEach((f, i) => {
      const id = BASE + i;
      ids.push(id);
      const rate = f.rate;
      session.setProbe(id, { from: f.from, to: f.to }, { kind: 'audio' }, (frame) => {
        if (frame.kind === 'audio') browserAudio.play(id, frame.samples, rate, frame.gap);
      }, 60);
    });
    return () => {
      for (const id of ids) {
        session.removeProbe(id);
        browserAudio.forget(id);
      }
    };
  });
</script>

{#if key}
  <div class="player" role="group" aria-label="Browser audio">
    {#if browserAudio.blocked}
      <button type="button" class="enable" onclick={() => browserAudio.unlock()}>🔈 Click to enable audio</button>
    {:else}
      <button type="button" class="mute" aria-pressed={browserAudio.muted} title={browserAudio.muted ? 'Unmute' : 'Mute'} onclick={() => browserAudio.toggleMute()}>
        {browserAudio.muted ? '🔇' : '🔊'}
      </button>
      <input
        type="range"
        min="0"
        max="1"
        step="0.01"
        aria-label="Volume"
        value={browserAudio.volume}
        oninput={(e) => browserAudio.setVolume(Number(e.currentTarget.value))}
      />
    {/if}
  </div>
{/if}

<style>
  .player {
    position: absolute;
    right: 12px;
    top: 12px;
    z-index: 5;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: color-mix(in srgb, var(--panel, #12161c) 92%, transparent);
    font-size: 12px;
  }
  button {
    border: 1px solid var(--line);
    background: var(--bg);
    color: var(--text);
    border-radius: 6px;
    padding: 4px 8px;
    font-size: 12px;
  }
  button:hover {
    border-color: var(--accent);
  }
  .mute {
    padding: 2px 6px;
  }
  input[type='range'] {
    width: 96px;
  }
</style>
