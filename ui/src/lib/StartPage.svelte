<script lang="ts">
  import { fetchExamples, listFiles, type ExampleInfo, type FileInfo } from './api';

  let { onexample, onfile, onblank }: { onexample: (name: string) => void; onfile: (name: string) => void; onblank: () => void } = $props();

  let examples = $state<ExampleInfo[]>([]);
  let files = $state<FileInfo[]>([]);
  fetchExamples().then((e) => (examples = e)).catch(() => {});
  listFiles().then((f) => (files = f.slice(0, 6))).catch(() => {});

  const ago = (ms: number) => {
    const m = Math.round((Date.now() - ms) / 60000);
    if (m < 1) return 'just now';
    if (m < 60) return `${m} min ago`;
    if (m < 60 * 24) return `${Math.round(m / 60)} h ago`;
    return new Date(ms).toLocaleDateString();
  };
</script>

<section class="start" aria-labelledby="start-title">
  <h1 id="start-title">Start a flowgraph</h1>
  <p class="muted">Pick a template to get a working graph you can change. Or start blank and press <kbd>/</kbd> to add blocks.</p>
  <div class="cards">
    {#each examples as e (e.name)}
      <button class="card" onclick={() => onexample(e.name)}>
        <strong>{e.title}</strong>
        <span class="muted">{e.description}</span>
        <span class="meta mono">{e.blocks} blocks</span>
      </button>
    {/each}
    <button class="card blank" onclick={onblank}>
      <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14"></path></svg>
      <strong>Blank graph</strong>
    </button>
  </div>
  {#if files.length}
    <h2>Recent in this folder</h2>
    <div class="files">
      {#each files as f (f.name)}
        <button class="file" onclick={() => onfile(f.name)}>
          <span class="mono">{f.name}</span><span class="muted">{ago(f.modified)}</span>
        </button>
      {/each}
    </div>
  {/if}
</section>

<style>
  .start {
    position: absolute;
    inset: 0;
    z-index: 5;
    overflow: auto;
    padding: 56px 32px;
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
  }
  h1 {
    margin: 0 0 8px;
    font-size: 24px;
    font-weight: 600;
  }
  h2 {
    margin: 32px 0 10px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  p {
    margin: 0 0 28px;
    max-width: 520px;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 14px;
    width: 100%;
    max-width: 760px;
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    min-height: 140px;
    padding: 16px;
    text-align: left;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 10px;
    color: var(--text);
    font-size: 13px;
    line-height: 1.45;
  }
  .card:hover,
  .card:focus-visible {
    border-color: var(--accent);
  }
  .card strong {
    font-size: 14px;
  }
  .meta {
    margin-top: auto;
    font-size: 11px;
    color: var(--faint);
  }
  .blank {
    align-items: center;
    justify-content: center;
    border-style: dashed;
    background: transparent;
    color: var(--muted);
  }
  .files {
    display: flex;
    flex-direction: column;
    gap: 4px;
    width: 100%;
    max-width: 480px;
  }
  .file {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 12px;
    background: transparent;
    border: 1px solid var(--line);
    border-radius: 6px;
    color: var(--text);
    font-size: 12px;
  }
  .file:hover,
  .file:focus-visible {
    background: var(--panel-2);
  }
</style>
