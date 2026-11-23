<script lang="ts">
  // Upcoming satellite passes, in the palette, for the graph's Satellite
  // Tracker blocks: the engine predicts them from the tracker's element sets
  // and position (`GET /api/passes`). Shown only when the graph has a tracker.
  // "Track" points the tracker at a satellite.
  import { fetchPasses, type PassInfo, type Passes } from './api';
  import { graphView } from './graphview.svelte';

  let { ontrack }: { ontrack: (block: string, satellite: string) => void } = $props();

  const trackers = $derived(graphView.spec.block.filter((b) => b.kind === 'sat_tracker'));
  let chosen = $state<string | null>(null);
  const block = $derived(trackers.find((b) => b.id === chosen) ?? trackers[0] ?? null);

  let open = $state(true);
  let data = $state<Passes | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(false);
  let showAll = $state(false);
  let now = $state(Date.now() / 1000);

  async function refresh() {
    const id = block?.id;
    if (!id) return;
    loading = true;
    try {
      data = await fetchPasses(id);
      error = null;
    } catch (e) {
      data = null;
      error = String((e as Error).message ?? e);
    } finally {
      loading = false;
    }
  }

  // Again whenever the tracker's settings change (after the engine has the
  // edit), and every ten minutes.
  const key = $derived(block ? `${block.id}:${JSON.stringify(block.params)}` : '');
  $effect(() => {
    if (!open || !key) return;
    const t = setTimeout(refresh, 400);
    const iv = setInterval(refresh, 600_000);
    return () => {
      clearTimeout(t);
      clearInterval(iv);
    };
  });
  $effect(() => {
    if (!open) return;
    const iv = setInterval(() => (now = Date.now() / 1000), 1000);
    return () => clearInterval(iv);
  });

  // Passes not over yet (relative to the clock, or to the tracker's start time).
  const upcoming = $derived.by(() => {
    if (!data) return [];
    const t = data.live ? now : data.start;
    return data.passes.filter((p) => p.los > t);
  });
  const shown = $derived(showAll ? upcoming : upcoming.slice(0, 8));
  const stale = $derived(data?.satellites.filter((s) => Math.abs(s.age_days) > 14) ?? []);

  const DIRS = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'];
  const dir = (az: number) => DIRS[Math.round(az / 45) % 8];

  function dur(s: number): string {
    s = Math.max(0, Math.round(s));
    if (s < 60) return `${s} s`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m} min`;
    const h = Math.floor(m / 60);
    return h < 48 ? `${h} h ${m % 60} min` : `${Math.round(h / 24)} d`;
  }

  function clock(t: number): string {
    const d = new Date(t * 1000);
    return data?.live
      ? d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' })
      : `${d.toISOString().slice(11, 16)}Z`;
  }

  function when(p: PassInfo): string {
    if (!data) return '';
    const t = data.live ? now : data.start;
    if (p.aos <= t) return `up · ${dur(p.los - t)} left`;
    return data.live ? `in ${dur(p.aos - t)}` : `T+${dur(p.aos - t)}`;
  }

  function details(p: PassInfo): string {
    const d = (t: number) => (data?.live ? new Date(t * 1000).toLocaleString() : new Date(t * 1000).toISOString());
    return [
      `${p.name} (NORAD ${p.norad})`,
      `${p.aos_clipped ? 'Up since before' : 'Rises'} ${d(p.aos)} at ${p.aos_az.toFixed(0)}° (${dir(p.aos_az)})`,
      `Highest ${d(p.tca)}: ${p.max_el.toFixed(1)}° at ${p.tca_az.toFixed(0)}° (${dir(p.tca_az)})`,
      `${p.los_clipped ? 'Still up at' : 'Sets'} ${d(p.los)} at ${p.los_az.toFixed(0)}° (${dir(p.los_az)})`,
    ].join('\n');
  }
</script>

{#if block}
  <section class="passes" aria-label="Satellite passes">
    <div class="head">
      <button class="toggle" aria-expanded={open} onclick={() => (open = !open)}>
        <svg class:open width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" aria-hidden="true"><path d="M9 6l6 6-6 6"></path></svg>
        Passes
      </button>
      {#if open}
        <button class="refresh" title="Predict again" aria-label="Refresh passes" disabled={loading} onclick={refresh}>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"></path><path d="M21 3v5h-5"></path></svg>
        </button>
      {/if}
    </div>
    {#if open}
      {#if trackers.length > 1}
        <select class="pick" aria-label="Tracker" value={block.id} onchange={(e) => (chosen = e.currentTarget.value)}>
          {#each trackers as t (t.id)}<option value={t.id}>{t.id}</option>{/each}
        </select>
      {/if}
      {#if error}
        <p class="err">{error}</p>
      {:else if !data}
        <p class="note">Predicting…</p>
      {:else}
        <p class="note mono" title="The tracker's position and minimum elevation">
          {data.lat.toFixed(2)}°, {data.lon.toFixed(2)}° · ≥ {data.min_elevation}°{data.live ? '' : ` · from ${new Date(data.start * 1000).toISOString().slice(0, 16)}Z`}
        </p>
        {#each shown as p (`${p.norad}:${p.aos}`)}
          {@const tracked = p.norad === data.tracked}
          <div class="pass" class:tracked title={details(p)}>
            <div class="row">
              <span class="name">{p.name}</span>
              <span class="when mono">{when(p)}</span>
            </div>
            <div class="row">
              <span class="sub mono">{clock(p.aos)} · {p.max_el.toFixed(0)}° · {dur(p.los - p.aos)} · {dir(p.aos_az)}→{dir(p.los_az)}</span>
              {#if !tracked}
                <button class="track" title="Point the tracker at {p.name}" onclick={() => ontrack(block.id, p.name)}>Track</button>
              {/if}
            </div>
          </div>
        {:else}
          <p class="note">No pass above {data.min_elevation}° in the next {Math.round((data.end - data.start) / 3600)} h.</p>
        {/each}
        {#if upcoming.length > shown.length || showAll}
          <button class="more" onclick={() => (showAll = !showAll)}>{showAll ? 'Fewer' : `All ${upcoming.length}`}</button>
        {/if}
        {#each stale as s (s.norad)}
          <p class="warn">{s.name}: elements are {Math.round(s.age_days)} days old; times may be off by minutes.</p>
        {/each}
        {#each data.errors as e, i (i)}
          <p class="err">{e}</p>
        {/each}
      {/if}
    {/if}
  </section>
{/if}

<style>
  .passes {
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
  .pick {
    width: 100%;
    margin: 2px 0 4px;
  }
  .pass {
    padding: 4px 6px;
    border-radius: 6px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .row > :first-child {
    flex: 1;
    min-width: 0;
  }
  .pass:hover {
    background: var(--panel-2);
  }
  .pass.tracked .name {
    color: var(--accent);
  }
  .name,
  .sub {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub,
  .when {
    font-size: 10px;
    color: var(--faint);
  }
  .when {
    white-space: nowrap;
  }
  .track {
    padding: 0 6px;
  }
  .more {
    padding: 3px 8px;
  }
  .track,
  .more {
    border: 1px solid var(--line);
    border-radius: 4px;
    background: transparent;
    font-size: 11px;
    color: var(--accent);
  }
  .track:hover,
  .more:hover {
    border-color: var(--accent);
  }
  .more {
    margin: 4px 0 0 6px;
    color: var(--muted);
  }
  .note,
  .err,
  .warn {
    margin: 4px 0 0;
    font-size: 11px;
    color: var(--faint);
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--err);
  }
  .warn {
    color: var(--warn);
  }
</style>
