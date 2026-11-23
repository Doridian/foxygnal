<script lang="ts">
  // A recording's timeline: time on the x axis, capture segments shaded with
  // their frequency, annotations as markers coloured by label. Clicking the
  // strip or a marker picks a sample; "Add source from here" puts a SigMF
  // Source starting there on the canvas.
  import { getRecording, type RecordingInfo, type SigmfMeta } from './api';
  import { fmtHz, fmtTime } from './plot';

  let { rec, onadd }: { rec: RecordingInfo; onadd: (start: number) => void } = $props();

  const H = 30;
  const BAND = 12; // capture band at the bottom; markers stand above it

  let meta = $state<SigmfMeta | null>(null);
  let error = $state<string | null>(null);
  let width = $state(0);
  let hover = $state<number | null>(null);
  let picked = $state<{ sample: number; what: string } | null>(null);

  $effect(() => {
    const name = rec.name;
    void rec.modified;
    getRecording(name).then(
      (m) => {
        meta = m;
        error = null;
      },
      (e) => (error = String((e as Error).message ?? e)),
    );
  });

  const n = $derived(Math.max(1, rec.samples));
  const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
  const start = (o: Record<string, unknown>) => Math.max(0, num(o['core:sample_start']) ?? 0);

  interface Segment {
    from: number;
    to: number;
    freq: number | null;
  }
  const segments = $derived.by((): Segment[] => {
    const caps = (meta?.captures ?? []).map((c) => ({ at: start(c), freq: num(c['core:frequency']) })).sort((a, b) => a.at - b.at);
    return caps.map((c, i) => ({ from: c.at, to: Math.max(c.at, caps[i + 1]?.at ?? n), freq: c.freq }));
  });

  interface Marker {
    sample: number;
    label: string;
    tip: string;
    /** More annotations with this label drawn at the same pixel. */
    more: number;
  }

  function describe(a: Record<string, unknown>): { label: string; detail: string[] } {
    const label = String(a['core:label'] ?? a['core:comment'] ?? 'annotation');
    const detail: string[] = [];
    if (typeof a['core:comment'] === 'string' && a['core:comment'] !== label) detail.push(a['core:comment']);
    for (const [k, v] of Object.entries(a)) {
      if (k.startsWith('foxygnal:')) detail.push(`${k.slice(9)} = ${v === null ? 'NaN' : JSON.stringify(v)}`);
    }
    return { label, detail };
  }

  const x = (sample: number) => (Math.min(sample, n) / n) * width;
  const time = (sample: number) => (rec.sample_rate ? fmtTime(sample / rec.sample_rate) : `sample ${sample}`);
  const where = (sample: number) => (rec.sample_rate ? `sample ${sample} · ${time(sample)}` : `sample ${sample}`);

  // One marker per label and pixel, so long recordings with thousands of
  // overflows stay cheap to draw.
  const markers = $derived.by((): Marker[] => {
    const seen = new Map<string, Marker>();
    for (const a of meta?.annotations ?? []) {
      const s = start(a);
      const { label, detail } = describe(a);
      const key = `${label}@${Math.round(x(s))}`;
      const m = seen.get(key);
      if (m) {
        m.more++;
        continue;
      }
      seen.set(key, { sample: s, label, tip: [label, where(s), ...detail].join('\n'), more: 0 });
    }
    return [...seen.values()];
  });

  const PALETTE = ['#4cc2ff', '#c48cff', '#8bd450', '#ffd166', '#f78fb3', '#5eead4'];
  function colour(label: string): string {
    if (label === 'overflow') return 'var(--err)';
    if (label === 'gap') return 'var(--warn)';
    let h = 0;
    for (const c of label) h = (h * 31 + c.charCodeAt(0)) >>> 0;
    return PALETTE[h % PALETTE.length];
  }
  const labels = $derived([...new Set(markers.map((m) => m.label))].sort());

  function sampleAt(e: MouseEvent): number {
    const r = (e.currentTarget as SVGElement).getBoundingClientRect();
    const f = Math.max(0, Math.min(1, (e.clientX - r.left) / Math.max(1, r.width)));
    return Math.min(n - 1, Math.floor(f * n));
  }

  function pickMarker(e: MouseEvent, m: Marker) {
    e.stopPropagation();
    picked = { sample: Math.min(m.sample, n - 1), what: m.label };
  }

  function onkey(e: KeyboardEvent, m: Marker) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      picked = { sample: Math.min(m.sample, n - 1), what: m.label };
    }
  }

  const segFill = (i: number) => (i % 2 ? 'var(--panel-2)' : 'var(--line)');
</script>

<div class="tl">
  {#if error}
    <p class="err">Can't read the metadata: {error}</p>
  {:else}
    <div class="strip" bind:clientWidth={width}>
      {#if width > 0}
        <!-- Clicking anywhere is a mouse shortcut; the markers take the keyboard. -->
        <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
        <svg {width} height={H} role="img" aria-label="Timeline of {rec.name}" onmousemove={(e) => (hover = sampleAt(e))} onmouseleave={() => (hover = null)} onclick={(e) => (picked = { sample: sampleAt(e), what: 'here' })}>
          {#each segments as s, i (i)}
            <rect x={x(s.from)} y={H - BAND} width={Math.max(1, x(s.to) - x(s.from))} height={BAND} fill={segFill(i)}>
              <title>{s.freq !== null ? fmtHz(s.freq) : 'capture'} · from {where(s.from)}</title>
            </rect>
            {#if s.freq !== null && x(s.to) - x(s.from) > 48}
              <text x={x(s.from) + 3} y={H - 3} class="freq">{fmtHz(s.freq)}</text>
            {/if}
            {#if i > 0}
              <line x1={x(s.from)} x2={x(s.from)} y1={H - BAND} y2={H} class="cut" />
            {/if}
          {/each}
          {#each markers as m (`${m.label}@${m.sample}`)}
            <g class="mk" role="button" tabindex="0" aria-label="{m.label} at {where(m.sample)}" onclick={(e) => pickMarker(e, m)} onkeydown={(e) => onkey(e, m)}>
              <title>{m.tip}{m.more ? `\n(+${m.more} more here)` : ''}</title>
              <rect x={x(m.sample) - 3} y="0" width="7" height={H} fill="transparent" />
              <line x1={x(m.sample)} x2={x(m.sample)} y1="5" y2={H} stroke={colour(m.label)} stroke-width="1.5" />
              <circle cx={x(m.sample)} cy="4" r="3" fill={colour(m.label)} />
            </g>
          {/each}
          {#if hover !== null}
            <line x1={x(hover)} x2={x(hover)} y1="0" y2={H} class="hover" />
          {/if}
          {#if picked}
            <line x1={x(picked.sample)} x2={x(picked.sample)} y1="0" y2={H} class="pick" />
          {/if}
        </svg>
      {/if}
    </div>
    <div class="axis mono">
      <span>0</span>
      <span>{hover !== null ? time(hover) : meta ? '' : 'loading…'}</span>
      <span>{time(rec.samples)}</span>
    </div>
    {#if labels.length}
      <div class="legend">
        {#each labels as l (l)}
          <span><i style:background={colour(l)}></i>{l}</span>
        {/each}
      </div>
    {/if}
    {#if picked}
      <div class="pickrow">
        <span class="mono" title="Sample {picked.sample}">{picked.what === 'here' ? '' : `${picked.what} · `}{where(picked.sample)}</span>
        <button class="add" title="Add a SigMF Source that starts at sample {picked.sample}" onclick={() => onadd(picked!.sample)}>Add source from here</button>
      </div>
    {:else}
      <p class="hint">Click the strip or a marker to play from there.</p>
    {/if}
  {/if}
</div>

<style>
  .tl {
    padding: 2px 6px 6px 8px;
  }
  .strip {
    width: 100%;
    height: 30px;
    cursor: crosshair;
  }
  svg {
    display: block;
    border-radius: 3px;
    background: var(--bg);
  }
  .freq {
    font-family: var(--mono);
    font-size: 9px;
    fill: var(--muted);
    pointer-events: none;
  }
  .cut {
    stroke: var(--faint);
    stroke-width: 1;
  }
  .mk {
    cursor: pointer;
    outline: none;
  }
  .mk:focus-visible circle {
    stroke: var(--text);
    stroke-width: 1.5;
  }
  .hover {
    stroke: var(--faint);
    stroke-dasharray: 2 2;
    pointer-events: none;
  }
  .pick {
    stroke: var(--accent);
    stroke-width: 1.5;
    pointer-events: none;
  }
  .axis {
    display: flex;
    justify-content: space-between;
    font-size: 9px;
    color: var(--faint);
  }
  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 2px 8px;
    margin-top: 2px;
    font-size: 10px;
    color: var(--muted);
  }
  .legend i {
    display: inline-block;
    width: 7px;
    height: 7px;
    margin-right: 3px;
    border-radius: 50%;
  }
  .pickrow {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 3px 6px;
    margin-top: 4px;
    font-size: 10px;
    color: var(--muted);
  }
  .pickrow span {
    flex: 1 1 100%;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .add {
    padding: 3px 6px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: transparent;
    color: var(--accent);
    font-size: 10px;
    white-space: nowrap;
  }
  .add:hover {
    border-color: var(--accent);
  }
  .hint,
  .err {
    margin: 4px 0 0;
    font-size: 10px;
    color: var(--faint);
  }
  .err {
    color: var(--err);
  }
</style>
