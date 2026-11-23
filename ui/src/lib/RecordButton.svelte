<script lang="ts">
  // Record a wire of the running graph to SigMF (engine-side, lossless), with
  // the elapsed time and size while it runs. Generic: works on any wire.
  import type { EdgeRef } from './api';
  import { fmtBytes, recordKey, session } from './session.svelte';

  let { edge }: { edge: EdgeRef } = $props();

  const running = $derived(session.status.state === 'running');
  const rec = $derived(session.recordings[recordKey(edge)]);

  function clock(ms: number): string {
    const s = Math.floor(ms / 1000);
    const mm = String(Math.floor(s / 60) % 60).padStart(2, '0');
    const ss = String(s % 60).padStart(2, '0');
    return s >= 3600 ? `${Math.floor(s / 3600)}:${mm}:${ss}` : `${mm}:${ss}`;
  }
</script>

{#if rec}
  <div class="rec" role="status">
    <span class="dot" aria-hidden="true"></span>
    <span class="mono">{clock(rec.elapsed_ms)}</span>
    <span class="mono muted">{fmtBytes(rec.bytes)}</span>
    <button type="button" class="stop" onclick={() => session.stopRecording(edge)}>Stop</button>
  </div>
  <p class="note mono" title={rec.path}>{rec.path}{rec.annotations ? ` · ${rec.annotations} annotation${rec.annotations === 1 ? '' : 's'}` : ''}{rec.gaps ? ` · ${rec.gaps} gap(s)` : ''}</p>
{:else}
  <button type="button" class="start" disabled={!running} onclick={() => session.startRecording(edge)}>
    <span class="dot idle" aria-hidden="true"></span>Record to SigMF
  </button>
  <p class="note">{running ? 'Every sample and tag, into recordings/ in the project folder.' : 'Run the graph to record this wire.'}</p>
{/if}

<style>
  .rec {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--err);
    flex: none;
    animation: blink 1.2s ease-in-out infinite;
  }
  .dot.idle {
    animation: none;
    opacity: 0.8;
  }
  @keyframes blink {
    50% {
      opacity: 0.3;
    }
  }
  .muted {
    color: var(--muted);
  }
  button {
    padding: 6px 10px;
    border-radius: 6px;
    border: 1px solid var(--line);
    background: var(--bg);
    color: var(--text);
    font-size: 12px;
  }
  .start {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    min-height: 36px;
  }
  .stop {
    margin-left: auto;
  }
  button:hover:not(:disabled) {
    border-color: var(--err);
  }
  button:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .note {
    margin: 6px 0 0;
    font-size: 11px;
    color: var(--faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
