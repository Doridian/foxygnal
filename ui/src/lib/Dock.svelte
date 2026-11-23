<script lang="ts">
  // The instrument dock under the canvas: instruments side by side, a
  // resizable height, and a way to add one without hunting for the wire.
  import { graphView } from './graphview.svelte';
  import { instruments, title, type InstrumentKind, type SpectrumSettings } from './instruments.svelte';
  import InstrumentPanel from './instruments/InstrumentPanel.svelte';
  import WatchButtons from './instruments/WatchButtons.svelte';

  const KEY = 'foxygnal:dock-height';
  let height = $state(readHeight());
  let adding = $state(false);
  let addEdge = $state('');
  let addBox = $state<HTMLElement>();

  function readHeight(): number {
    try {
      const v = Number(localStorage.getItem(KEY));
      return v >= 120 ? v : 300;
    } catch {
      return 300;
    }
  }

  function onresizedown(e: PointerEvent) {
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    const startY = e.clientY;
    const startH = height;
    const move = (ev: PointerEvent) => {
      height = Math.max(120, Math.min(window.innerHeight - 200, startH + startY - ev.clientY));
    };
    const up = () => {
      el.removeEventListener('pointermove', move);
      el.removeEventListener('pointerup', up);
      try {
        localStorage.setItem(KEY, String(Math.round(height)));
      } catch {
        // Not remembered; fine.
      }
    };
    el.addEventListener('pointermove', move);
    el.addEventListener('pointerup', up);
  }

  function onresizekey(e: KeyboardEvent) {
    if (e.key === 'ArrowUp') height = Math.min(window.innerHeight - 200, height + 20);
    else if (e.key === 'ArrowDown') height = Math.max(120, height - 20);
  }

  const wires = $derived(graphView.spec.connect.map((c) => `${c.from} → ${c.to}`));

  function openAdd() {
    adding = !adding;
    if (!wires.includes(addEdge)) addEdge = wires[0] ?? '';
  }

  function add(kind: InstrumentKind, view?: SpectrumSettings['view']) {
    const c = graphView.spec.connect[wires.indexOf(addEdge)];
    if (!c) return;
    instruments.add(kind, c, view);
    adding = false;
  }

  function onwindowclick(e: MouseEvent) {
    if (adding && addBox && !addBox.contains(e.target as Node)) adding = false;
  }

  const shown = $derived(instruments.list);
</script>

<svelte:window onclick={onwindowclick} />

{#if shown.length}
  <section class="dock" style:height="{height}px" aria-label="Instruments">
    <button class="grip" aria-label="Resize the instrument dock (arrow keys)" onpointerdown={onresizedown} onkeydown={onresizekey}></button>
    <div class="row">
      {#each shown as inst (inst.id)}
        {#if instruments.popped[inst.id]}
          <div class="popped">
            <span class="badge">{inst.id}</span>
            <span>{title(inst)} is in its own window.</span>
            <button onclick={() => instruments.bringBack(inst.id)}>Bring back</button>
          </div>
        {:else}
          <InstrumentPanel {inst} />
        {/if}
      {/each}
      <div class="add" bind:this={addBox}>
        <button class="addbtn" onclick={openAdd} aria-expanded={adding}>+ Instrument</button>
        {#if adding}
          <div class="addpop" role="dialog" aria-label="Add an instrument">
            {#if wires.length}
              <label class="muted small" for="add-wire">Wire</label>
              <select id="add-wire" class="mono" bind:value={addEdge}>
                {#each wires as w (w)}<option value={w}>{w}</option>{/each}
              </select>
              <WatchButtons type={graphView.outputType(graphView.spec.connect[wires.indexOf(addEdge)]?.from ?? '')} onpick={add} />
            {:else}
              <p class="muted small">Connect two blocks first, then watch the wire between them.</p>
            {/if}
          </div>
        {/if}
      </div>
    </div>
  </section>
{/if}

<style>
  .dock {
    position: relative;
    grid-area: dock;
    min-height: 0;
    display: flex;
    flex-direction: column;
    background: var(--panel);
    border-top: 1px solid var(--line);
  }
  .grip {
    position: absolute;
    left: 0;
    right: 0;
    top: -4px;
    height: 8px;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: ns-resize;
    z-index: 5;
  }
  .grip:hover,
  .grip:focus-visible {
    background: color-mix(in srgb, var(--accent) 35%, transparent);
    outline: none;
  }
  .row {
    flex: 1;
    min-height: 0;
    display: flex;
    gap: 12px;
    padding: 10px 12px 12px;
    overflow-x: auto;
  }
  .popped {
    flex: 0 0 200px;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    justify-content: center;
    gap: 8px;
    padding: 12px;
    border: 1px dashed var(--line);
    border-radius: 8px;
    color: var(--muted);
    font-size: 12px;
  }
  .badge {
    width: 18px;
    height: 18px;
    display: grid;
    place-items: center;
    border-radius: 50%;
    border: 2px solid var(--text);
    color: var(--text);
    font-size: 10px;
    font-weight: 700;
  }
  .add {
    position: relative;
    flex: 0 0 120px;
    display: flex;
  }
  .addbtn {
    flex: 1;
    border: 1px dashed var(--line);
    border-radius: 8px;
    background: transparent;
    color: var(--muted);
    font-size: 13px;
  }
  .addbtn:hover {
    color: var(--text);
    border-color: var(--muted);
  }
  .addpop {
    position: absolute;
    z-index: 30;
    right: 0;
    bottom: 0;
    width: 260px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 10px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.5);
  }
  .small {
    font-size: 12px;
    margin: 0;
  }
  select {
    font-size: 12px;
    padding: 5px 7px;
  }
</style>
