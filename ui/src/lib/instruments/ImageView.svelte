<script lang="ts">
  // Image: the pictures a decoder draws (SSTV, …), row by row as they
  // arrive. The newest picture is shown big; earlier ones wait in a strip
  // below, click one to look at it again. Save writes the shown one as PNG.
  import type { ImageRow } from '../frames';
  import type { ImageSettings } from '../instruments.svelte';

  let {
    settings,
    running,
    onchange,
  }: {
    settings: ImageSettings;
    /** The graph is running; when it stops, a picture still being drawn is finished as it is. */
    running: boolean;
    onchange: (patch: Partial<ImageSettings>) => void;
  } = $props();

  interface Picture {
    /** Ours, unique within this panel (the engine's numbers restart each run). */
    key: number;
    /** The engine's picture number. */
    number: number;
    width: number;
    /** Rows promised, or grown to the highest row seen. */
    height: number;
    rgba: Uint8ClampedArray<ArrayBuffer>;
    rows: number;
    lastRow: number;
    done: boolean;
    /** A small PNG, once done. */
    thumb: string | null;
  }

  let pictures = $state.raw<Picture[]>([]);
  /** Shown picture (key); null follows the newest. */
  let picked = $state<number | null>(null);
  /** Bumped whenever pixels change, to redraw and refresh the status line. */
  let version = $state(0);
  let canvas = $state<HTMLCanvasElement>();
  let nextKey = 1;

  const shown = $derived(pictures.find((p) => p.key === picked) ?? pictures[pictures.length - 1] ?? null);

  function start(r: ImageRow): Picture {
    const height = Math.max(r.height, r.row + 1, 1);
    const p: Picture = { key: nextKey++, number: r.picture, width: r.width, height, rgba: blank(r.width, height), rows: 0, lastRow: -1, done: false, thumb: null };
    const keep = Math.max(1, settings.keep);
    const list = pictures.concat(p);
    pictures = list.length > keep ? list.slice(list.length - keep) : list;
    if (picked !== null && !pictures.some((q) => q.key === picked)) picked = null;
    return p;
  }

  function blank(w: number, h: number): Uint8ClampedArray<ArrayBuffer> {
    const a = new Uint8ClampedArray(w * h * 4);
    for (let i = 3; i < a.length; i += 4) a[i] = 255;
    return a;
  }

  /** Take a frame's rows (called by the panel for every frame). */
  export function add(rows: ImageRow[]) {
    if (!rows.length) return;
    let cur = pictures[pictures.length - 1];
    for (const r of rows) {
      if (!cur || cur.number !== r.picture || cur.width !== r.width || cur.done) {
        // A repeat of a finished picture's last row (the engine flags a cut-off
        // picture that way) isn't a new picture.
        if (cur && cur.number === r.picture && cur.done) continue;
        if (cur && !cur.done) finish(cur);
        cur = start(r);
      }
      if (r.row >= cur.height) grow(cur, r.row + 1);
      const at = r.row * cur.width * 4;
      for (let x = 0; x < r.width; x++) {
        const o = at + x * 4;
        if (r.channels === 3) {
          cur.rgba[o] = r.pixels[x * 3];
          cur.rgba[o + 1] = r.pixels[x * 3 + 1];
          cur.rgba[o + 2] = r.pixels[x * 3 + 2];
        } else {
          cur.rgba[o] = cur.rgba[o + 1] = cur.rgba[o + 2] = r.pixels[x];
        }
      }
      cur.rows++;
      cur.lastRow = r.row;
      if (r.last) finish(cur);
    }
    version++;
  }

  function grow(p: Picture, height: number) {
    const h = Math.max(height, p.height * 2);
    const a = blank(p.width, h);
    a.set(p.rgba);
    p.rgba = a;
    p.height = h;
  }

  function finish(p: Picture) {
    p.done = true;
    p.thumb = toCanvas(p).toDataURL('image/png');
    pictures = [...pictures];
  }

  function toCanvas(p: Picture): HTMLCanvasElement {
    const c = document.createElement('canvas');
    c.width = p.width;
    c.height = p.height;
    c.getContext('2d')?.putImageData(new ImageData(p.rgba, p.width, p.height), 0, 0);
    return c;
  }

  export function clear() {
    pictures = [];
    picked = null;
  }

  // A run that ends mid-picture (or whose last row was skipped) ends the picture.
  $effect(() => {
    if (running) return;
    const cur = pictures[pictures.length - 1];
    if (cur && !cur.done && cur.rows > 0) {
      finish(cur);
      version++;
    }
  });

  // Draw the shown picture whenever it or its pixels change.
  $effect(() => {
    void version;
    const p = shown;
    if (!canvas || !p) return;
    if (canvas.width !== p.width || canvas.height !== p.height) {
      canvas.width = p.width;
      canvas.height = p.height;
    }
    canvas.getContext('2d')?.putImageData(new ImageData(p.rgba, p.width, p.height), 0, 0);
  });

  function save() {
    const p = shown;
    if (!p) return;
    toCanvas(p).toBlob((blob) => {
      if (!blob) return;
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = `picture-${p.key}.png`;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    }, 'image/png');
  }

  const status = $derived.by(() => {
    void version;
    const p = shown;
    if (!p) return '';
    const of = p.done ? '' : ` · row ${p.lastRow + 1} of ${p.height}`;
    return `${p.width}×${p.height}${of}${p.done ? ' · done' : ''}`;
  });
</script>

<div class="image">
  <div class="bar">
    <span class="status mono">{status}</span>
    {#if picked !== null}
      <button type="button" onclick={() => (picked = null)} title="Show the newest picture as it arrives">Newest</button>
    {/if}
    <label class="check"><input type="checkbox" checked={settings.pixelated} onchange={(e) => onchange({ pixelated: e.currentTarget.checked })} /> Sharp pixels</label>
    <button type="button" onclick={save} disabled={!shown}>Save PNG</button>
    <button type="button" onclick={clear} disabled={!pictures.length}>Clear</button>
  </div>
  <div class="stage">
    {#if shown}
      <canvas bind:this={canvas} class:pixelated={settings.pixelated} aria-label="Decoded picture"></canvas>
    {/if}
  </div>
  {#if pictures.length > 1}
    <div class="strip" role="listbox" aria-label="Earlier pictures">
      {#each pictures as p (p.key)}
        <!-- Pictures are plain objects: `version` says when one changed. -->
        {@const thumb = version >= 0 ? p.thumb : null}
        <button type="button" role="option" aria-selected={p === shown} class:sel={p === shown} onclick={() => (picked = p.key)} title="{p.width}×{p.height}">
          {#if thumb}
            <img src={thumb} alt="Picture {p.key}" />
          {:else}
            <span class="live">…</span>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .image {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bar {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .status {
    flex: 1;
    min-width: 0;
    font-size: 11px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .check {
    white-space: nowrap;
    display: flex;
    align-items: center;
    gap: 4px;
    font-size: 11px;
    color: var(--muted);
  }
  button {
    padding: 3px 10px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: transparent;
    color: var(--text);
    font-size: 11px;
  }
  button:disabled {
    opacity: 0.45;
  }
  .stage {
    flex: 1;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: 4px;
    overflow: hidden;
  }
  canvas {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
  canvas.pixelated {
    image-rendering: pixelated;
  }
  .strip {
    display: flex;
    gap: 6px;
    overflow-x: auto;
    flex: none;
  }
  .strip button {
    padding: 2px;
    width: 64px;
    height: 52px;
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .strip button.sel {
    border-color: var(--accent);
  }
  .strip img {
    max-width: 100%;
    max-height: 100%;
  }
  .live {
    color: var(--faint);
  }
</style>
