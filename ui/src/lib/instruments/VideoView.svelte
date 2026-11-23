<script lang="ts">
  // Video: an MPEG transport stream from a u8 wire (a DVB demodulator, a .ts
  // file), demuxed and decoded in a worker (../video/worker.ts). Pictures
  // wait in a short queue and are shown when due (their PTS on the stream's
  // clock plus a jitter buffer); sound, if switched on, goes to browser audio
  // with the same clock.
  import { onMount } from 'svelte';
  import { browserAudio } from '../audio.svelte';
  import type { TsFrame } from '../frames';
  import type { VideoSettings } from '../instruments.svelte';
  import type { Program } from '../video/ts';
  import type { FromWorker, Stats, ToWorker } from '../video/protocol';

  let {
    id,
    settings,
    running,
    onchange,
  }: {
    id: number;
    settings: VideoSettings;
    running: boolean;
    onchange: (patch: Partial<VideoSettings>) => void;
  } = $props();

  let canvas = $state<HTMLCanvasElement>();

  let programs = $state.raw<Program[]>([]);
  let selected = $state<number | null>(null);
  let stats = $state.raw<Stats | null>(null);
  let gaps = $state(0);
  let worker: Worker | null = null;
  let lastSeq: number | null = null;

  interface Queued {
    frame: VideoFrame;
    due: number;
    aspect: number;
  }
  let queue: Queued[] = [];
  /** Never hold more than this many decoded pictures (they pin decoder memory). */
  const MAX_QUEUED = 48;
  /** Our sound in browser audio (apart from probe ids). */
  const audioId = () => 1_000_000 + id;

  function send(m: ToWorker, transfer: Transferable[] = []) {
    worker?.postMessage(m, transfer);
  }

  /** Take a probe frame's packets (called by the panel). */
  export function packets(f: TsFrame) {
    const seqGap = lastSeq !== null && f.seq !== ((lastSeq + 1) >>> 0);
    lastSeq = f.seq;
    if (f.gap || seqGap) gaps++;
    if (!f.packets.length && !f.gap) return;
    const copy = f.packets.slice();
    send({ type: 'packets', data: copy.buffer, gap: f.gap || seqGap }, [copy.buffer]);
  }

  function flush() {
    for (const q of queue) q.frame.close();
    queue = [];
  }

  function onmessage(e: MessageEvent<FromWorker>) {
    const m = e.data;
    switch (m.type) {
      case 'programs':
        programs = m.programs;
        selected = m.selected;
        break;
      case 'stats':
        stats = m.stats;
        break;
      case 'frame':
        queue.push({ frame: m.frame, due: m.due, aspect: m.aspect });
        while (queue.length > MAX_QUEUED) queue.shift()?.frame.close();
        break;
      case 'pcm':
        if (settings.audio) browserAudio.playAt(audioId(), m.samples, m.channels, m.rate, m.due);
        break;
      case 'flush':
        flush();
        break;
    }
  }

  /** Show the newest picture that is due; drop the ones it replaces. */
  function present() {
    const now = performance.timeOrigin + performance.now();
    let pick = -1;
    for (let i = 0; i < queue.length; i++) if (queue[i].due <= now) pick = i;
    if (pick < 0) return;
    const q = queue[pick];
    for (let i = 0; i < pick; i++) queue[i].frame.close();
    queue = queue.slice(pick + 1);
    draw(q);
    q.frame.close();
  }

  function draw(q: Queued) {
    const c = canvas;
    const ctx = c?.getContext('2d');
    if (!c || !ctx) return;
    const dpr = window.devicePixelRatio || 1;
    const cw = Math.max(1, Math.round(c.clientWidth * dpr));
    const ch = Math.max(1, Math.round(c.clientHeight * dpr));
    if (c.width !== cw || c.height !== ch) {
      c.width = cw;
      c.height = ch;
    }
    const f = q.frame;
    const aspect = q.aspect || f.displayWidth / Math.max(1, f.displayHeight);
    let w = cw;
    let h = w / aspect;
    if (h > ch) {
      h = ch;
      w = h * aspect;
    }
    ctx.fillStyle = '#000';
    ctx.fillRect(0, 0, cw, ch);
    ctx.drawImage(f, (cw - w) / 2, (ch - h) / 2, w, h);
  }

  onMount(() => {
    worker = new Worker(new URL('../video/worker.ts', import.meta.url), { type: 'module' });
    worker.onmessage = onmessage;
    send({ type: 'program', number: settings.program });
    send({ type: 'audio', on: settings.audio });
    let raf = 0;
    const loop = () => {
      present();
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => {
      cancelAnimationFrame(raf);
      send({ type: 'close' });
      worker?.terminate();
      worker = null;
      flush();
      browserAudio.forget(audioId());
    };
  });

  // A new run starts a new stream.
  let wasRunning = false;
  $effect(() => {
    if (running && !wasRunning) {
      send({ type: 'reset' });
      flush();
      lastSeq = null;
      gaps = 0;
    }
    wasRunning = running;
  });

  $effect(() => send({ type: 'program', number: settings.program }));
  $effect(() => send({ type: 'audio', on: settings.audio }));

  function toggleSound() {
    browserAudio.unlock();
    onchange({ audio: !settings.audio });
  }

  function programLabel(p: Program): string {
    const v = p.streams.find((s) => s.kind === 'video');
    const name = p.name || `Program ${p.number}`;
    return v ? name : `${name} (no video)`;
  }

  const fmtRate = (bps: number) => (bps >= 1e6 ? `${(bps / 1e6).toFixed(2)} Mbit/s` : `${(bps / 1e3).toFixed(0)} kbit/s`);
  const CODEC: Record<string, string> = {
    h264: 'H.264',
    hevc: 'HEVC',
    mpeg2: 'MPEG-2',
    mpeg1: 'MPEG-1',
    aac: 'AAC',
    'aac-latm': 'AAC (LATM)',
    mp2: 'MPEG audio',
    ac3: 'AC-3',
    eac3: 'E-AC-3',
  };
  const codecName = (c: string) => CODEC[c] ?? c;
  const current = $derived(programs.find((p) => p.number === selected) ?? null);
  const note = $derived(stats?.video?.note ?? (stats && !stats.video && current ? 'This program has no video.' : null));
</script>

<div class="video">
  <div class="bar">
    <select
      aria-label="Program"
      value={settings.program === null ? 'auto' : String(settings.program)}
      onchange={(e) => onchange({ program: e.currentTarget.value === 'auto' ? null : Number(e.currentTarget.value) })}
    >
      <option value="auto">{current && settings.program === null ? `Auto: ${programLabel(current)}` : 'Auto (first with video)'}</option>
      {#each programs as p (p.number)}
        <option value={String(p.number)}>{p.number} · {programLabel(p)}</option>
      {/each}
    </select>
    {#if settings.audio && browserAudio.blocked}
      <button type="button" class="enable" onclick={() => browserAudio.unlock()}>Click to enable sound</button>
    {/if}
    <button type="button" class="tog" aria-pressed={settings.audio} aria-label="Sound" title={settings.audio ? 'Sound on' : 'Sound off'} onclick={toggleSound}>
      {settings.audio ? '🔊' : '🔇'}
    </button>
    <button type="button" class="tog" aria-pressed={settings.info} title="Stream details" onclick={() => onchange({ info: !settings.info })}>ⓘ</button>
  </div>
  <div class="screen">
    <canvas bind:this={canvas}></canvas>
    {#if note && running}
      <div class="note" role="status">{note}</div>
    {/if}
    {#if settings.info && stats}
      {@const v = stats.video}
      {@const a = stats.audio}
      <dl class="info mono" aria-label="Stream details">
        {#if current}
          <dt>Service</dt>
          <dd>{current.name ?? `Program ${current.number}`}{current.provider ? ` · ${current.provider}` : ''}</dd>
        {/if}
        {#if v}
          <dt>Video</dt>
          <dd>
            {codecName(v.codec)}
            {#if v.width}· {v.width}×{v.height}{/if}
            {#if v.fps}· {v.fps.toFixed(1)} fps{/if}
            · {fmtRate(v.bitrate)}
          </dd>
          <dt>Decoder</dt>
          <dd>
            {v.decoder === 'wasm' ? `WebAssembly${v.decodeMs ? ` · ${v.decodeMs.toFixed(1)} ms/picture` : ''}` : (v.decoder ?? '–')}
            {#if v.dropped}· {v.dropped} dropped{/if}
            {#if v.errors}· {v.errors} errors{/if}
          </dd>
        {/if}
        {#if a}
          <dt>Audio</dt>
          <dd>
            {codecName(a.codec)}{a.lang ? ` (${a.lang})` : ''}
            {#if a.rate}· {a.rate / 1000} kHz{a.channels ? ` · ${a.channels} ch` : ''}{/if}
            · {fmtRate(a.bitrate)}
            {#if a.note}<br /><span class="warn">{a.note}</span>{/if}
          </dd>
        {/if}
        <dt>Stream</dt>
        <dd>{fmtRate(stats.bitrate)} · {programs.length} program{programs.length === 1 ? '' : 's'}</dd>
        <dt>Errors</dt>
        <dd class:warn={stats.ccErrors + stats.tei + gaps > 0}>
          {stats.ccErrors} continuity · {stats.tei} TEI · {gaps} gaps
        </dd>
      </dl>
    {/if}
  </div>
</div>

<style>
  .video {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bar {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  select {
    flex: 1;
    min-width: 0;
    font-size: 12px;
  }
  .tog {
    width: 30px;
    height: 26px;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: transparent;
    color: var(--text);
    font-size: 13px;
  }
  .enable {
    padding: 3px 8px;
    border: 1px solid var(--accent);
    border-radius: 6px;
    background: transparent;
    color: var(--accent);
    font-size: 11px;
    white-space: nowrap;
  }
  .tog[aria-pressed='true'] {
    border-color: var(--accent);
  }
  .screen {
    position: relative;
    flex: 1;
    min-height: 120px;
    background: #000;
    border-radius: 4px;
    overflow: hidden;
  }
  canvas {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
  }
  .note {
    position: absolute;
    left: 8px;
    right: 8px;
    bottom: 8px;
    padding: 4px 8px;
    font-size: 12px;
    color: var(--muted);
    background: rgba(10, 13, 17, 0.75);
    border-radius: 4px;
    pointer-events: none;
  }
  .info {
    position: absolute;
    top: 6px;
    left: 6px;
    max-width: calc(100% - 12px);
    margin: 0;
    padding: 6px 8px;
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px 8px;
    font-size: 11px;
    color: #e6e9ee;
    background: rgba(10, 13, 17, 0.7);
    border-radius: 4px;
    pointer-events: none;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
  .warn {
    color: var(--warn, #ffb454);
  }
</style>
