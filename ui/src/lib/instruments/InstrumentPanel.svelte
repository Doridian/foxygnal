<script lang="ts">
  import type { Frame } from '../frames';
  import { graphView } from '../graphview.svelte';
  import { instruments, probeConfig, title, type Instrument } from '../instruments.svelte';
  import { fmtNum, fmtTime } from '../plot';
  import { session } from '../session.svelte';
  import { CHANNEL_LABELS, typeColour } from './colours';
  import ConstellationView from './ConstellationView.svelte';
  import InstrumentSettings from './InstrumentSettings.svelte';
  import MeterView from './MeterView.svelte';
  import ScopeView from './ScopeView.svelte';
  import SpectrumView from './SpectrumView.svelte';
  import TextView from './TextView.svelte';

  let { inst, popout = false }: { inst: Instrument; popout?: boolean } = $props();

  let frame = $state.raw<Frame | null>(null);
  let lastFrame = $state(0);
  let now = $state(performance.now());
  let width = $state(0);
  let settingsOpen = $state(false);
  let panel = $state<HTMLElement>();
  /** Messages instrument: every line received (null = some were lost). */
  let lines = $state.raw<(string | null)[]>([]);

  function addText(gap: boolean, text: string) {
    const fresh: (string | null)[] = text ? text.replace(/\n$/, '').split('\n') : [];
    if (gap && lines.length) fresh.unshift(null);
    if (!fresh.length) return;
    const max = inst.kind === 'text' ? inst.settings.maxLines : 2000;
    const all = lines.concat(fresh);
    lines = all.length > max ? all.slice(all.length - max) : all;
  }

  const type = $derived(graphView.outputType(inst.edge.from));
  const colour = $derived(typeColour(type));
  const streamRate = $derived(graphView.rateOf(inst.edge.from));
  const rate = $derived(inst.rate ?? streamRate);
  const center = $derived(graphView.centerOf(inst.edge.from));
  const running = $derived(session.status.state === 'running');
  const onGraph = $derived(graphView.hasEdge(inst.edge));
  const error = $derived(session.probeErrors[inst.id]);

  // Resizing settles before the probe changes: a new width resets averages.
  let pendingWidth = 0;
  let widthTimer: ReturnType<typeof setTimeout> | undefined;
  function onwidth(px: number) {
    pendingWidth = Math.round(px);
    if (!width) width = pendingWidth;
    clearTimeout(widthTimer);
    widthTimer = setTimeout(() => (width = pendingWidth), 200);
  }

  // The engine side follows the instrument's settings.
  $effect(() => {
    // Only traces depend on the drawn width.
    const w = inst.kind === 'scope' || inst.kind === 'spectrum' ? width : 16;
    if (!w) return;
    const cfg = probeConfig(inst, w, rate);
    session.setProbe(
      inst.id,
      { from: inst.edge.from, to: inst.edge.to },
      cfg,
      (f) => {
        if (f.kind === 'text') addText(f.gap, f.text);
        frame = f;
        lastFrame = performance.now();
      },
      inst.fps,
    );
  });

  $effect(() => {
    const id = inst.id;
    return () => {
      clearTimeout(widthTimer);
      session.removeProbe(id);
    };
  });

  // Ticks for "waiting…" states.
  $effect(() => {
    const t = setInterval(() => (now = performance.now()), 500);
    return () => clearInterval(t);
  });

  const quiet = $derived(now - lastFrame > 1200);
  const captured = $derived(inst.kind === 'scope' && inst.settings.trigger === 'single' && frame !== null && running && quiet);

  const overlay = $derived.by(() => {
    if (!session.connected) return 'Engine disconnected';
    if (!onGraph) return 'This wire is no longer in the graph.';
    if (error) return error;
    if (!running) return frame || lines.length ? null : 'Not running. Press Run to see the signal.';
    if (!quiet || captured) return null;
    if (inst.kind === 'scope' && inst.settings.trigger !== 'off' && inst.settings.trigger !== 'auto') return 'Waiting for a trigger…';
    if (inst.kind === 'text') return frame || lines.length ? null : 'Waiting for messages…';
    return frame ? null : 'Waiting for samples…';
  });

  const chips = $derived.by((): string[] => {
    switch (inst.kind) {
      case 'scope': {
        const s = inst.settings;
        const mode = { auto: 'Auto', normal: 'Normal', single: 'Single', off: 'Free run' }[s.trigger];
        const trig = s.trigger === 'off' ? mode : `${mode} · ${s.slope === 'rising' ? '↑' : '↓'} ${fmtNum(s.level)}`;
        const h = rate ? `${fmtTime(s.timePerDiv)}/div` : `${fmtNum(s.samplesPerDiv)} smp/div`;
        return [trig, h, `${fmtNum(s.voltsPerDiv)}/div`];
      }
      case 'spectrum': {
        const s = inst.settings;
        const win = s.window.replace('_', '-');
        const out = [`FFT ${s.fftSize} · ${win[0].toUpperCase()}${win.slice(1)}${s.average > 1 ? ` · avg ${s.average}` : ''}`];
        if (s.peakHold) out.push('Peak hold');
        return out;
      }
      case 'constellation':
        return [`${inst.settings.points} pts`];
      case 'meter':
      case 'text':
        return [];
    }
  });

  function change(patch: { rate?: number | null; settings?: Record<string, unknown> }) {
    instruments.change(inst.id, patch);
  }

  function onwindowclick(e: MouseEvent) {
    if (settingsOpen && panel && !panel.contains(e.target as Node)) settingsOpen = false;
  }
</script>

<svelte:window onclick={onwindowclick} onkeydown={(e) => e.key === 'Escape' && (settingsOpen = false)} />

<section class="panel" bind:this={panel} aria-label="{title(inst)} {inst.id}">
  <header>
    <span class="badge" title="Instrument {inst.id}: the same number marks the wire">{inst.id}</span>
    <strong>{title(inst)}</strong>
    <span class="edge mono" title="{inst.edge.from} → {inst.edge.to}">{inst.edge.from} → {inst.edge.to}</span>
    <span class="spacer"></span>
    {#if inst.kind === 'scope'}
      {#each inst.settings.channels as ch (ch)}
        {#if type === 'c32' || ch === 'i'}
          <span class="legend mono" style:color={ch === 'i' ? `var(${colour})` : ch === 'q' ? '#ffd9a6' : ch === 'mag' ? 'var(--t-f32)' : 'var(--t-i16)'}
            >● {type === 'c32' ? CHANNEL_LABELS[ch] : 'x'}</span
          >
        {/if}
      {/each}
      {#if inst.settings.trigger === 'single'}
        <button class="chip arm" onclick={() => inst.kind === 'scope' && change({ settings: { arm: inst.settings.arm + 1 } })} title="Capture the next trigger"
          >{captured ? 'Captured · re-arm' : 'Armed'}</button
        >
      {/if}
    {/if}
    {#each chips as c (c)}
      <button class="chip mono" onclick={() => (settingsOpen = true)}>{c}</button>
    {/each}
    <button class="icon" aria-label="Settings" aria-expanded={settingsOpen} onclick={() => (settingsOpen = !settingsOpen)}>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
        ><path d="M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0"></path><circle cx="16" cy="6" r="2"></circle><circle cx="10" cy="12" r="2"></circle><circle
          cx="18"
          cy="18"
          r="2"
        ></circle></svg
      >
    </button>
    {#if popout}
      <button class="icon" aria-label="Back to the dock" title="Back to the dock" onclick={() => window.close()}>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
          ><path d="M10 20H4v-6M4 20l8-8M6 10V5a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1h-5"></path></svg
        >
      </button>
    {:else}
      <button class="icon" aria-label="Pop out into its own window" title="Pop out" onclick={() => instruments.popOut(inst.id)}>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
          ><path d="M14 4h6v6M20 4l-8 8M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"></path></svg
        >
      </button>
      <button class="icon" aria-label="Close instrument" title="Close" onclick={() => instruments.remove(inst.id)}>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"
          ><path d="M6 6l12 12M18 6L6 18"></path></svg
        >
      </button>
    {/if}
  </header>
  <div class="body" class:dim={!running && frame}>
    {#if inst.kind === 'scope'}
      <ScopeView
        settings={inst.settings}
        frame={frame?.kind === 'scope' ? frame : null}
        {rate}
        {colour}
        {onwidth}
        onchange={(p) => change({ settings: p })}
      />
    {:else if inst.kind === 'spectrum'}
      <SpectrumView settings={inst.settings} frame={frame?.kind === 'spectrum' ? frame : null} {rate} {center} {colour} {onwidth} />
    {:else if inst.kind === 'constellation'}
      <ConstellationView settings={inst.settings} frame={frame?.kind === 'constellation' ? frame : null} {colour} />
    {:else if inst.kind === 'text'}
      <TextView settings={inst.settings} {lines} onclear={() => (lines = [])} onchange={(p) => change({ settings: p })} />
    {:else}
      <MeterView frame={frame?.kind === 'meter' ? frame : null} {colour} />
    {/if}
    {#if overlay}
      <div class="overlay" class:err={!!error || !onGraph} role="status">{overlay}</div>
    {/if}
  </div>
  {#if settingsOpen}
    <div class="popover" role="dialog" aria-label="{title(inst)} settings">
      <InstrumentSettings {inst} {type} {rate} {streamRate} {frame} onchange={change} />
    </div>
  {/if}
</section>

<style>
  .panel {
    position: relative;
    flex: 1 1 380px;
    min-width: 300px;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 10px;
    background: #11151b;
    border: 1px solid var(--line);
    border-radius: 8px;
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    flex-wrap: wrap;
  }
  .badge {
    flex: none;
    width: 18px;
    height: 18px;
    display: grid;
    place-items: center;
    border-radius: 50%;
    border: 2px solid var(--text);
    font-size: 10px;
    font-weight: 700;
  }
  strong {
    font-size: 13px;
    white-space: nowrap;
  }
  .edge {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 11px;
    color: var(--muted);
  }
  .spacer {
    flex: 1;
  }
  .legend {
    font-size: 11px;
  }
  .chip {
    padding: 3px 8px;
    border: 1px solid var(--line);
    border-radius: 999px;
    background: transparent;
    color: var(--text);
    font-size: 11px;
    white-space: nowrap;
  }
  .chip:hover {
    border-color: var(--muted);
  }
  .chip.arm {
    border-color: var(--accent);
    color: var(--accent);
  }
  .icon {
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: transparent;
    color: var(--muted);
  }
  .icon:hover {
    color: var(--text);
  }
  .body {
    position: relative;
    flex: 1;
    min-height: 60px;
    display: flex;
  }
  .body.dim :global(canvas) {
    opacity: 0.55;
  }
  .overlay {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    padding: 12px;
    text-align: center;
    color: var(--muted);
    font-size: 12px;
    background: rgba(10, 13, 17, 0.6);
    border-radius: 4px;
    pointer-events: none;
  }
  .overlay.err {
    color: var(--err);
  }
  .popover {
    position: absolute;
    z-index: 30;
    top: 44px;
    right: 10px;
    width: min(340px, calc(100% - 20px));
    max-height: calc(100% - 54px);
    min-height: 120px;
    overflow: auto;
    box-sizing: border-box;
    padding: 12px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 10px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.5);
  }
</style>
