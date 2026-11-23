<script lang="ts">
  import type { FftWindow, ScopeChannel, StreamType } from '../api';
  import type { Frame } from '../frames';
  import { MAX_SPAN, SCOPE_DIVS, type Instrument } from '../instruments.svelte';
  import { fmtNum, fmtRate, fmtTime, nearestStep, stepBy } from '../plot';
  import { CHANNEL_LABELS } from './colours';
  import NumberInput from './NumberInput.svelte';

  let {
    inst,
    type,
    rate,
    streamRate,
    frame,
    onchange,
  }: {
    inst: Instrument;
    type: StreamType | null;
    /** The rate in use (set by hand, else the stream's). */
    rate: number | null;
    /** The stream's own rate, from the engine. */
    streamRate: number | null;
    frame: Frame | null;
    onchange: (patch: { rate?: number | null; settings?: Record<string, unknown> }) => void;
  } = $props();

  const complex = $derived(type === 'c32');
  const set = (settings: Record<string, unknown>) => onchange({ settings });

  const WINDOWS: [FftWindow, string][] = [
    ['rectangular', 'Rectangular'],
    ['hann', 'Hann'],
    ['hamming', 'Hamming'],
    ['blackman', 'Blackman'],
    ['blackman_harris', 'Blackman-Harris'],
    ['flat_top', 'Flat top'],
  ];
  const FFT_SIZES = [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];
  const AVERAGES = [1, 2, 4, 8, 16, 32, 64, 128];
  const TILE_EXAMPLE = 'https://tile.openstreetmap.org/{z}/{x}/{y}.png';
  const TILE_VARS = '{z} {x} {y}';
  const TILE_SUB = '{s}';

  function toggleChannel(ch: ScopeChannel) {
    if (inst.kind !== 'scope') return;
    const on = inst.settings.channels.includes(ch);
    const next = on ? inst.settings.channels.filter((c) => c !== ch) : [...inst.settings.channels, ch];
    if (next.length) set({ channels: (['i', 'q', 'mag', 'phase'] as const).filter((c) => next.includes(c)) });
  }

  /** Fit V/div and offset to the latest scope frame. */
  function autoScale() {
    if (frame?.kind !== 'scope') return;
    let lo = Infinity;
    let hi = -Infinity;
    for (const ch of frame.channels) for (const v of ch.data) if (Number.isFinite(v)) ((lo = Math.min(lo, v)), (hi = Math.max(hi, v)));
    if (!Number.isFinite(lo)) return;
    const vdiv = nearestStep(Math.max(1e-6, (hi - lo) / 6));
    set({ voltsPerDiv: vdiv, offset: Number(((hi + lo) / 2).toPrecision(2)) });
  }

  function autoConstellation() {
    if (frame?.kind !== 'constellation') return;
    let m = 0;
    for (const v of frame.iq) if (Number.isFinite(v)) m = Math.max(m, Math.abs(v));
    if (m > 0) set({ scale: nearestStep(m * 1.15) });
  }

  // Time/div within what the engine can capture.
  function timeStep(dir: 1 | -1) {
    if (inst.kind !== 'scope' || !rate) return;
    const next = stepBy(inst.settings.timePerDiv, dir);
    if (next * rate * SCOPE_DIVS <= MAX_SPAN && next * rate * SCOPE_DIVS >= 10) set({ timePerDiv: next });
  }

  function sampleStep(dir: 1 | -1) {
    if (inst.kind !== 'scope') return;
    const next = stepBy(inst.settings.samplesPerDiv, dir);
    if (next >= 1 && next * SCOPE_DIVS <= MAX_SPAN) set({ samplesPerDiv: next });
  }
</script>

<div class="settings">
  {#if inst.kind === 'scope'}
    {@const s = inst.settings}
    <fieldset>
      <legend>Horizontal</legend>
      <div class="stepper">
        <button type="button" aria-label="Zoom in (less per division)" onclick={() => (rate ? timeStep(-1) : sampleStep(-1))}>−</button>
        <span class="mono">{rate ? `${fmtTime(s.timePerDiv)}/div` : `${fmtNum(s.samplesPerDiv)} smp/div`}</span>
        <button type="button" aria-label="Zoom out (more per division)" onclick={() => (rate ? timeStep(1) : sampleStep(1))}>+</button>
      </div>
      <label>Pre-trigger <NumberInput label="Pre-trigger percent" value={Math.round(s.pretrigger * 100)} onchange={(v) => set({ pretrigger: Math.max(0, Math.min(100, v ?? 10)) / 100 })} /> %</label>
    </fieldset>
    <fieldset>
      <legend>Vertical</legend>
      <div class="stepper">
        <button type="button" aria-label="Smaller volts per division" onclick={() => set({ voltsPerDiv: stepBy(s.voltsPerDiv, -1) })}>−</button>
        <span class="mono">{fmtNum(s.voltsPerDiv)}/div</span>
        <button type="button" aria-label="Larger volts per division" onclick={() => set({ voltsPerDiv: stepBy(s.voltsPerDiv, 1) })}>+</button>
        <button type="button" onclick={autoScale} disabled={frame?.kind !== 'scope'}>Auto</button>
      </div>
      <label>Offset <NumberInput label="Vertical offset" value={s.offset} onchange={(v) => set({ offset: v ?? 0 })} /></label>
      {#if complex}
        <div class="checks" role="group" aria-label="Channels">
          {#each ['i', 'q', 'mag', 'phase'] as const as ch (ch)}
            <label class="check"><input type="checkbox" checked={s.channels.includes(ch)} onchange={() => toggleChannel(ch)} />{CHANNEL_LABELS[ch]}</label>
          {/each}
        </div>
      {/if}
    </fieldset>
    <fieldset>
      <legend>Trigger</legend>
      <div class="row">
        <select aria-label="Trigger mode" value={s.trigger} onchange={(e) => set({ trigger: e.currentTarget.value })}>
          <option value="auto">Auto</option>
          <option value="normal">Normal</option>
          <option value="single">Single</option>
          <option value="off">Off (free run)</option>
        </select>
        <select aria-label="Trigger slope" value={s.slope} onchange={(e) => set({ slope: e.currentTarget.value })} disabled={s.trigger === 'off'}>
          <option value="rising">↑ Rising</option>
          <option value="falling">↓ Falling</option>
        </select>
      </div>
      <div class="row">
        <label>Level <NumberInput label="Trigger level" value={s.level} onchange={(v) => set({ level: v ?? 0 })} /></label>
        {#if complex}
          <select aria-label="Trigger channel" value={s.triggerChannel} onchange={(e) => set({ triggerChannel: e.currentTarget.value })}>
            {#each ['i', 'q', 'mag', 'phase'] as const as ch (ch)}<option value={ch}>on {CHANNEL_LABELS[ch]}</option>{/each}
          </select>
        {/if}
      </div>
      <p class="hint">Tip: drag the marker on the scope's left edge to set the level.</p>
    </fieldset>
    <label class="check"><input type="checkbox" checked={s.cursors} onchange={(e) => set({ cursors: e.currentTarget.checked })} />Cursors (drag on the trace)</label>
  {:else if inst.kind === 'spectrum'}
    {@const s = inst.settings}
    <div class="seg" role="radiogroup" aria-label="View">
      {#each [['spectrum', 'Spectrum'], ['waterfall', 'Waterfall'], ['both', 'Both']] as const as [v, l] (v)}
        <button type="button" role="radio" aria-checked={s.view === v} class:on={s.view === v} onclick={() => set({ view: v })}>{l}</button>
      {/each}
    </div>
    <fieldset>
      <legend>FFT</legend>
      <div class="row">
        <select aria-label="FFT size" value={s.fftSize} onchange={(e) => set({ fftSize: Number(e.currentTarget.value) })}>
          {#each FFT_SIZES as n (n)}<option value={n}>{n} bins</option>{/each}
        </select>
        <select aria-label="Window" value={s.window} onchange={(e) => set({ window: e.currentTarget.value })}>
          {#each WINDOWS as [v, l] (v)}<option value={v}>{l}</option>{/each}
        </select>
      </div>
      <div class="row">
        <label
          >Average
          <select aria-label="Averaging" value={s.average} onchange={(e) => set({ average: Number(e.currentTarget.value) })}>
            {#each AVERAGES as n (n)}<option value={n}>{n === 1 ? 'off' : `×${n}`}</option>{/each}
          </select>
        </label>
        <label class="check"><input type="checkbox" checked={s.peakHold} onchange={(e) => set({ peakHold: e.currentTarget.checked })} />Peak hold</label>
        <button type="button" onclick={() => set({ reset: s.reset + 1 })} title="Clear the average and the peak hold">Reset</button>
      </div>
    </fieldset>
    <fieldset>
      <legend>Display</legend>
      <div class="row">
        <label>Top <NumberInput label="Reference level (dB at the top)" unit="dB" value={s.refLevel} onchange={(v) => set({ refLevel: v ?? 0 })} /></label>
        <label>Range <NumberInput label="Range in dB" unit="dB" value={s.range} onchange={(v) => set({ range: Math.max(5, v ?? 100) })} /></label>
      </div>
    </fieldset>
  {:else if inst.kind === 'constellation'}
    {@const s = inst.settings}
    <div class="row">
      <label
        >Points
        <select aria-label="Points per frame" value={s.points} onchange={(e) => set({ points: Number(e.currentTarget.value) })}>
          {#each [256, 512, 1024, 2048, 4096, 8192] as n (n)}<option value={n}>{n}</option>{/each}
        </select>
      </label>
      <label>Scale ± <NumberInput label="Full scale" value={s.scale} onchange={(v) => set({ scale: Math.max(1e-6, v ?? 1) })} /></label>
      <button type="button" onclick={autoConstellation} disabled={frame?.kind !== 'constellation'}>Auto</button>
    </div>
    <label
      >Persistence
      <input type="range" min="0" max="0.95" step="0.05" value={s.persistence} oninput={(e) => set({ persistence: Number(e.currentTarget.value) })} />
    </label>
  {:else if inst.kind === 'text'}
    {@const s = inst.settings}
    <NumberInput label="Lines kept" value={s.maxLines} onchange={(v) => v && v >= 10 && onchange({ settings: { maxLines: Math.round(v) } })} />
    <p class="hint">Text lines from the wire, such as a decoder's messages. Lines before the instrument opened are shown if the wire's buffer still holds them.</p>
  {:else if inst.kind === 'map'}
    {@const s = inst.settings}
    <fieldset>
      <legend>Tracks</legend>
      <label>Trail <NumberInput label="Trail length in positions" value={s.trail} onchange={(v) => v && v >= 1 && set({ trail: Math.round(v) })} /> positions</label>
      <label>Expire after <NumberInput label="Expiry in seconds" value={s.expiry} onchange={(v) => v && v >= 1 && set({ expiry: v })} /> s</label>
      <div class="row">
        <label class="check"><input type="checkbox" checked={s.labels} onchange={(e) => set({ labels: e.currentTarget.checked })} />Labels</label>
        <select aria-label="Distance units" value={s.units} onchange={(e) => set({ units: e.currentTarget.value })}>
          <option value="km">km</option>
          <option value="nm">nautical miles</option>
        </select>
      </div>
      <p class="hint">
        Plots lines with <code>lat=</code> and <code>lon=</code>, one track per <code>icao</code>, <code>mmsi</code>, APRS <code>src</code> or object name,
        <code>serial</code>, <code>id</code> or <code>callsign</code>. Lines with an id but no position (an aircraft's identity, a ship's name) add to the track's
        details. Tracks fade as they age. Click one for its details.
      </p>
    </fieldset>
    <fieldset>
      <legend>Receiver</legend>
      <div class="row">
        <label
          >Lat <NumberInput
            label="Receiver latitude"
            allowEmpty
            placeholder="none"
            value={s.rxLat}
            onchange={(v) => set({ rxLat: v !== null && Math.abs(v) <= 90 ? v : null })}
          /></label
        >
        <label
          >Lon <NumberInput
            label="Receiver longitude"
            allowEmpty
            placeholder="none"
            value={s.rxLon}
            onchange={(v) => set({ rxLon: v !== null && Math.abs(v) <= 180 ? v : null })}
          /></label
        >
      </div>
      <p class="hint">Decimal degrees, south and west negative, for range rings and distances. Kept in this browser only.</p>
    </fieldset>
    <fieldset>
      <legend>Base map</legend>
      <label class="check"><input type="checkbox" checked={s.tiles} onchange={(e) => set({ tiles: e.currentTarget.checked })} />Map tiles from a server</label>
      <input
        type="url"
        aria-label="Tile URL template"
        placeholder={TILE_EXAMPLE}
        value={s.tileUrl}
        onchange={(e) => set({ tileUrl: e.currentTarget.value.trim() })}
      />
      <p class="hint">
        Off, the map is the built-in Natural Earth world map and nothing leaves this computer. Tiles are fetched by this browser straight from the server you name,
        which learns which area you look at. Use <code>{TILE_VARS}</code> (and <code>{TILE_SUB}</code> for a subdomain), and follow the server's usage policy.
      </p>
    </fieldset>
  {:else if inst.kind === 'image'}
    {@const s = inst.settings}
    <NumberInput label="Pictures kept" value={s.keep} onchange={(v) => v && v >= 1 && onchange({ settings: { keep: Math.round(v) } })} />
    <p class="hint">Pictures a decoder draws on this wire (an SSTV decoder's <code>image</code> output), row by row. Rows sent before the instrument opened are shown if the wire's buffer still holds them. To keep every picture without a browser, add an Image Sink.</p>
  {:else if inst.kind === 'video'}
    {@const s = inst.settings}
    <label class="check"><input type="checkbox" checked={s.audio} onchange={(e) => onchange({ settings: { audio: e.currentTarget.checked } })} />Sound (AAC)</label>
    <label class="check"><input type="checkbox" checked={s.info} onchange={(e) => onchange({ settings: { info: e.currentTarget.checked } })} />Stream details over the picture</label>
    <p class="hint">
      An MPEG transport stream (188-byte packets) on this wire, such as a DVB demodulator's output or a <code>.ts</code> file, decoded in this browser: H.264 and HEVC
      with WebCodecs (HEVC where the browser supports it), MPEG-1/2 with a WebAssembly decoder. Pick the program above the picture.
    </p>
  {:else}
    <p class="hint">RMS, peak and DC over everything since the last update, in dB relative to full scale (±1).</p>
  {/if}
  {#if inst.kind !== 'meter' && inst.kind !== 'text' && inst.kind !== 'map' && inst.kind !== 'image' && inst.kind !== 'video'}
    <fieldset>
      <legend>Sample rate</legend>
      <NumberInput
        label="Sample rate"
        unit="S/s"
        allowEmpty
        value={inst.rate}
        placeholder={streamRate ? `${fmtRate(streamRate)} (from the graph)` : 'unknown: set it for time and Hz axes'}
        onchange={(v) => onchange({ rate: v && v > 0 ? v : null })}
      />
      <p class="hint">{inst.rate ? 'Set by hand. Clear it to use the graph’s rate.' : rate ? 'Taken from the graph.' : 'Axes show samples until a rate is known.'}</p>
    </fieldset>
  {/if}
</div>

<style>
  .settings {
    display: flex;
    flex-direction: column;
    gap: 10px;
    font-size: 12px;
  }
  fieldset {
    margin: 0;
    padding: 0;
    border: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  legend {
    padding: 0 0 4px;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--muted);
  }
  label {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
    white-space: nowrap;
  }
  .row,
  .stepper,
  .checks {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .row > select {
    flex: 1;
    width: auto;
  }
  select {
    padding: 4px 7px;
    font-size: 12px;
    width: auto;
  }
  .stepper span {
    min-width: 110px;
    text-align: center;
  }
  .stepper button,
  .row button,
  .seg button {
    padding: 4px 10px;
    font-size: 12px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--panel-2);
    color: var(--text);
  }
  .check input {
    width: auto;
    margin: 0;
  }
  .seg {
    display: flex;
    gap: 0;
  }
  .seg button {
    flex: 1;
    border-radius: 0;
    margin-left: -1px;
  }
  .seg button:first-child {
    border-radius: 6px 0 0 6px;
  }
  .seg button:last-child {
    border-radius: 0 6px 6px 0;
  }
  .seg button.on {
    background: var(--accent);
    border-color: var(--accent);
    color: #1a1206;
    font-weight: 600;
  }
  .hint {
    margin: 0;
    font-size: 11px;
    color: var(--faint);
    white-space: normal;
  }
  input[type='range'] {
    accent-color: var(--accent);
    padding: 0;
    border: 0;
  }
</style>
