<script lang="ts">
  // The SigMF recordings in the project's `recordings/` folder, in the palette.
  // "Add" puts a SigMF Source for one onto the canvas; each one expands into a
  // timeline of its captures and annotations, to play from any point.
  import { deleteRecording, listRecordings, type RecordingInfo } from './api';
  import { fmtHz, fmtRate } from './plot';
  import RecordingTimeline from './RecordingTimeline.svelte';
  import { fmtBytes, session } from './session.svelte';
  import { toast } from './toast.svelte';

  let { onadd }: { onadd: (r: RecordingInfo, start?: number) => void } = $props();

  let expanded = $state<Record<string, boolean>>({});

  let open = $state(true);
  let list = $state<RecordingInfo[] | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);

  async function refresh() {
    loading = true;
    try {
      list = await listRecordings();
      error = null;
    } catch (e) {
      error = String((e as Error).message ?? e);
    } finally {
      loading = false;
    }
  }

  // Progress events replace `session.recordings` twice a second; only the
  // count matters here.
  const active = $derived(Object.keys(session.recordings).length);

  // Load when opened, and again whenever a recording starts or ends.
  $effect(() => {
    void session.recordingsChanged;
    void active;
    if (open) void refresh();
  });

  async function remove(r: RecordingInfo) {
    if (!confirm(`Delete the recording ${r.name}? This removes both SigMF files.`)) return;
    try {
      await deleteRecording(r.name);
    } catch (e) {
      toast(`Can't delete ${r.name}: ${(e as Error).message ?? e}`, 'error');
    }
    void refresh();
  }

  function summary(r: RecordingInfo): string {
    if (r.error) return r.error;
    const parts = [r.datatype ?? '?'];
    if (r.sample_rate) parts.push(fmtRate(r.sample_rate));
    if (r.frequency !== null) parts.push(fmtHz(r.frequency));
    if (r.sample_rate) parts.push(`${(r.samples / r.sample_rate).toFixed(1)} s`);
    else parts.push(`${r.samples} samples`);
    if (r.annotations) parts.push(`${r.annotations} ann.`);
    return parts.join(' · ');
  }

  function details(r: RecordingInfo): string {
    return [r.path, r.description, r.hw, r.datetime, fmtBytes(r.bytes), r.captures > 1 ? `${r.captures} capture segments` : null]
      .filter(Boolean)
      .join('\n');
  }
</script>

<section class="recs" aria-label="Recordings">
  <div class="head">
    <button class="toggle" aria-expanded={open} onclick={() => (open = !open)}>
      <svg class:open width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" aria-hidden="true"><path d="M9 6l6 6-6 6"></path></svg>
      Recordings
    </button>
    {#if open}
      <button class="refresh" title="List recordings again" aria-label="Refresh recordings" disabled={loading} onclick={refresh}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"></path><path d="M21 3v5h-5"></path></svg>
      </button>
    {/if}
  </div>
  {#if open}
    {#if error}
      <p class="err">Can't list recordings: {error}</p>
    {:else if list === null}
      <p class="note">Loading…</p>
    {:else}
      {#each list as r (r.name)}
        {@const canExpand = !r.recording && !r.error}
        <div class="rec" title={details(r)}>
          <button
            class="exp"
            aria-expanded={!!expanded[r.name]}
            aria-label="{expanded[r.name] ? 'Hide' : 'Show'} the timeline of {r.name}"
            title={canExpand ? 'Timeline: captures and annotations' : ''}
            disabled={!canExpand}
            onclick={() => (expanded[r.name] = !expanded[r.name])}
          >
            <svg class:open={expanded[r.name] && canExpand} width="9" height="9" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" aria-hidden="true"><path d="M9 6l6 6-6 6"></path></svg>
          </button>
          <div class="text">
            <span class="name">{r.name}</span>
            <span class="sub mono">{r.recording ? 'recording…' : summary(r)}</span>
          </div>
          {#if !r.recording}
            <button class="add" title="Add a SigMF Source playing this recording" disabled={!!r.error} onclick={() => onadd(r)}>Add</button>
            <button class="del" title="Delete this recording" aria-label="Delete {r.name}" onclick={() => remove(r)}>×</button>
          {/if}
        </div>
        {#if expanded[r.name] && canExpand}
          <RecordingTimeline rec={r} onadd={(start) => onadd(r, start)} />
        {/if}
      {:else}
        <p class="note">None yet. Select a wire while the graph runs and press Record.</p>
      {/each}
    {/if}
  {/if}
</section>

<style>
  .recs {
    margin-top: 8px;
    padding-bottom: 6px;
    border-bottom: 1px solid var(--line);
  }
  .head {
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
  .rec {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 6px 4px 2px;
    border-radius: 6px;
  }
  .rec:hover {
    background: var(--panel-2);
  }
  .exp {
    display: grid;
    place-items: center;
    width: 16px;
    height: 22px;
    padding: 0;
    border: 0;
    background: transparent;
    color: var(--muted);
    flex: none;
  }
  .exp:disabled {
    opacity: 0.3;
    cursor: default;
  }
  .exp svg {
    transition: transform 0.15s;
  }
  .exp svg.open {
    transform: rotate(90deg);
  }
  .text {
    display: flex;
    flex-direction: column;
    min-width: 0;
    flex: 1;
  }
  .name,
  .sub {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub {
    font-size: 10px;
    color: var(--faint);
  }
  .add,
  .del {
    padding: 3px 8px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: transparent;
    font-size: 11px;
  }
  .add {
    color: var(--accent);
  }
  .add:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .del {
    padding: 3px 6px;
    color: var(--muted);
  }
  .del:hover {
    border-color: var(--err);
    color: var(--err);
  }
  .note,
  .err {
    margin: 4px 0 0;
    font-size: 11px;
    color: var(--faint);
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--err);
  }
</style>
