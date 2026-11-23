<script lang="ts">
  import { session } from './session.svelte';
  import { indexErrors } from './errors';
  import { prefs, savePrefs } from './prefs.svelte';

  let {
    dirty,
    blocks,
    onrun,
    onstop,
    actions,
  }: {
    dirty: boolean;
    blocks: number;
    onrun: () => void;
    onstop: () => void;
    actions: { label: string; keys?: string; run: () => void }[];
  } = $props();

  const st = $derived(session.status);
  let now = $state(Date.now());
  let stopping = $state(false);
  let menuOpen = $state(false);
  let menu = $state<HTMLDivElement>();

  $effect(() => {
    if (st.state !== 'running') {
      stopping = false;
      return;
    }
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });

  const elapsed = $derived.by(() => {
    if (!st.started_at) return '';
    const s = Math.max(0, Math.floor((now - st.started_at) / 1000));
    const mm = String(Math.floor(s / 60)).padStart(2, '0');
    return `${mm}:${String(s % 60).padStart(2, '0')}`;
  });

  const globalErrors = $derived(indexErrors(st.errors).global);
  const pill = $derived.by(() => {
    if (!session.connected) return { cls: 'warn', text: session.everConnected ? 'Engine disconnected, retrying…' : 'Connecting…' };
    if (st.state === 'running') return { cls: 'ok', text: stopping ? 'Stopping…' : `Running · ${elapsed}` };
    if (st.state === 'error') return { cls: 'err', text: st.errors.length === 1 ? 'Failed: 1 problem' : `Failed: ${st.errors.length} problems` };
    return { cls: 'idle', text: 'Stopped' };
  });

  function stop() {
    stopping = true;
    onstop();
  }

  function onwindowclick(e: MouseEvent) {
    if (menuOpen && menu && !menu.contains(e.target as Node)) menuOpen = false;
  }
</script>

<svelte:window onclick={onwindowclick} onkeydown={(e) => e.key === 'Escape' && (menuOpen = false)} />

<header class="topbar" class:transmitting={st.tx_armed}>
  <div class="brand">
    <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="var(--accent)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 4l4 6-2 6 7 5 7-5-2-6 4-6-6 3h-6z"></path><path d="M2 12c2-3 3 3 5 0"></path></svg>
    <strong>foxygnal</strong>
  </div>

  <div class="file" bind:this={menu}>
    <button class="filename" aria-haspopup="menu" aria-expanded={menuOpen} onclick={() => (menuOpen = !menuOpen)}>
      <span>{session.name ?? 'untitled.fox.toml'}</span>
      {#if dirty}<span class="dirty" title="Unsaved changes"></span>{/if}
      <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M6 9l6 6 6-6"></path></svg>
    </button>
    {#if menuOpen}
      <div class="menu" role="menu">
        {#each actions as a (a.label)}
          <button role="menuitem" onclick={() => { menuOpen = false; a.run(); }}>
            <span>{a.label}</span>{#if a.keys}<kbd>{a.keys}</kbd>{/if}
          </button>
        {/each}
        <a class="source" href="https://git.foxden.network/FoxDen/foxygnal" target="_blank" rel="noreferrer">Source code (AGPL-3.0)</a>
      </div>
    {/if}
  </div>

  <span class="spacer"></span>

  <button
    class="overlay"
    class:on={prefs.overlay}
    aria-pressed={prefs.overlay}
    title="Show CPU per block and throughput and buffer fill per wire while running"
    onclick={() => {
      prefs.overlay = !prefs.overlay;
      savePrefs();
    }}
  >
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
      ><path d="M3 20h18M6 16v-5M11 16V7M16 16v-3M21 16V4"></path></svg
    >Load
  </button>

  {#if st.tx_armed}
    <span class="tx" role="status" title="TX is armed for this run: the graph is transmitting. Stop to disarm.">
      <span class="txled"></span>TX · transmitting
    </span>
  {/if}
  {#if st.stale && st.state === 'running'}
    <span class="stale" title="Some changes only take effect after a restart">Restart to apply changes</span>
  {/if}
  <span class="pill {pill.cls}" title={globalErrors.join('\n') || undefined} role="status">
    <span class="led"></span>{pill.text}
  </span>

  {#if st.state === 'running'}
    <button class="stop" onclick={stop} disabled={stopping || !session.connected} title="Stop (Space)">
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><rect x="1" y="1" width="10" height="10" rx="1.5" fill="currentColor"></rect></svg>Stop
    </button>
  {:else}
    <button class="run" onclick={onrun} disabled={!session.connected || blocks === 0} title="Run (Space)">
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M2 1l9 5-9 5z" fill="currentColor"></path></svg>Run
    </button>
  {/if}
</header>
{#if globalErrors.length}
  <div class="banner" role="alert">{globalErrors.join(' · ')}</div>
{/if}

<style>
  .topbar {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 0 16px;
    height: 48px;
    background: var(--panel);
    border-bottom: 1px solid var(--line);
  }
  .topbar.transmitting {
    box-shadow: inset 0 -3px 0 #d1242f;
  }
  .tx {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 12px;
    border-radius: 6px;
    background: #d1242f;
    color: #fff;
    font-weight: 700;
    font-size: 13px;
    letter-spacing: 0.04em;
  }
  .txled {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: #fff;
    animation: txpulse 1s ease-in-out infinite;
  }
  @keyframes txpulse {
    50% {
      opacity: 0.25;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .txled {
      animation: none;
    }
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 15px;
    letter-spacing: -0.01em;
  }
  .file {
    position: relative;
  }
  .filename {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 8px;
    border: 1px solid transparent;
    border-radius: 6px;
    background: transparent;
    color: var(--muted);
  }
  .filename:hover,
  .filename[aria-expanded='true'] {
    border-color: var(--line);
    color: var(--text);
  }
  .dirty {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--muted);
  }
  .menu {
    position: absolute;
    z-index: 20;
    top: calc(100% + 4px);
    left: 0;
    min-width: 240px;
    padding: 4px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 8px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.5);
    display: flex;
    flex-direction: column;
  }
  .menu button {
    display: flex;
    justify-content: space-between;
    gap: 16px;
    padding: 8px 10px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text);
    text-align: left;
  }
  .menu button:hover,
  .menu button:focus-visible {
    background: var(--panel);
  }
  .source {
    margin-top: 4px;
    padding: 8px 10px 6px;
    border-top: 1px solid var(--line);
    font-size: 12px;
  }
  .spacer {
    flex: 1;
  }
  .overlay {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: transparent;
    color: var(--muted);
    font-size: 12px;
  }
  .overlay:hover {
    color: var(--text);
  }
  .overlay.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  .stale {
    font-size: 12px;
    color: var(--warn);
  }
  .pill {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 10px;
    border-radius: 999px;
    font-weight: 500;
    font-size: 13px;
    font-variant-numeric: tabular-nums;
  }
  .led {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
  }
  .pill.ok {
    background: var(--ok-bg);
    color: var(--ok-text);
  }
  .pill.err {
    background: var(--err-bg);
    color: var(--err);
  }
  .pill.warn {
    background: var(--warn-bg);
    color: var(--warn);
  }
  .pill.idle {
    background: var(--panel-2);
    color: var(--muted);
  }
  .run,
  .stop {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 7px 14px;
    border-radius: 6px;
    font-weight: 600;
  }
  .run {
    border: 0;
    background: var(--ok);
    color: #04140a;
  }
  .stop {
    border: 1px solid #3a2a2a;
    background: var(--err-bg);
    color: var(--err);
  }
  .run:disabled,
  .stop:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .banner {
    padding: 6px 16px;
    background: var(--err-bg);
    color: var(--err);
    border-bottom: 1px solid var(--err-line);
    font-size: 12px;
  }
</style>
