// Instruments in the dock: what each one watches and how it's set up.
// Remembered per graph file (localStorage), and shared with pop-out windows
// over a BroadcastChannel.

import type { EdgeRef, FftWindow, ProbeConfig, ScopeChannel, TriggerMode } from './api';

export type InstrumentKind = 'scope' | 'spectrum' | 'constellation' | 'meter' | 'text' | 'image' | 'video';

export interface ScopeSettings {
  channels: ScopeChannel[];
  trigger: TriggerMode;
  slope: 'rising' | 'falling';
  level: number;
  triggerChannel: ScopeChannel;
  pretrigger: number;
  /** Seconds per division when the rate is known. */
  timePerDiv: number;
  /** Samples per division otherwise. */
  samplesPerDiv: number;
  voltsPerDiv: number;
  offset: number;
  cursors: boolean;
  arm: number;
}

export interface SpectrumSettings {
  view: 'spectrum' | 'waterfall' | 'both';
  fftSize: number;
  window: FftWindow;
  average: number;
  peakHold: boolean;
  /** dB at the top of the plot. */
  refLevel: number;
  /** dB from top to bottom. */
  range: number;
  reset: number;
}

export interface ConstellationSettings {
  points: number;
  persistence: number;
  scale: number;
}

export type MeterSettings = Record<string, never>;

export interface TextSettings {
  /** Lines kept on screen; older ones scroll away. */
  maxLines: number;
  /** Show only lines containing this (case-insensitive). */
  filter: string;
}

export interface ImageSettings {
  /** Pictures kept in the strip below the current one. */
  keep: number;
  /** Scale the picture up with hard pixel edges instead of smoothing. */
  pixelated: boolean;
}

export interface VideoSettings {
  /** Program number to show; null = the first with video. */
  program: number | null;
  /** Play the program's sound (AAC) through browser audio. */
  audio: boolean;
  /** Show the stream details over the picture. */
  info: boolean;
}

interface Base {
  /** Also the probe id and the badge number on the wire. */
  id: number;
  edge: EdgeRef;
  /** Sample rate set by hand; `null` to guess from upstream blocks. */
  rate: number | null;
  fps: number;
}

export type Instrument = Base &
  (
    | { kind: 'scope'; settings: ScopeSettings }
    | { kind: 'spectrum'; settings: SpectrumSettings }
    | { kind: 'constellation'; settings: ConstellationSettings }
    | { kind: 'meter'; settings: MeterSettings }
    | { kind: 'text'; settings: TextSettings }
    | { kind: 'image'; settings: ImageSettings }
    | { kind: 'video'; settings: VideoSettings }
  );

export const defaults = {
  scope: (): ScopeSettings => ({
    channels: ['i', 'q'],
    trigger: 'auto',
    slope: 'rising',
    level: 0,
    triggerChannel: 'i',
    pretrigger: 0.1,
    timePerDiv: 1e-3,
    samplesPerDiv: 100,
    voltsPerDiv: 0.25,
    offset: 0,
    cursors: false,
    arm: 0,
  }),
  spectrum: (): SpectrumSettings => ({
    view: 'both',
    fftSize: 2048,
    window: 'hann',
    average: 4,
    peakHold: false,
    refLevel: 0,
    range: 100,
    reset: 0,
  }),
  constellation: (): ConstellationSettings => ({ points: 1024, persistence: 0.6, scale: 1 }),
  meter: (): MeterSettings => ({}),
  text: (): TextSettings => ({ maxLines: 2000, filter: '' }),
  image: (): ImageSettings => ({ keep: 8, pixelated: false }),
  video: (): VideoSettings => ({ program: null, audio: false, info: true }),
};

export const KIND_LABELS: Record<InstrumentKind, string> = {
  scope: 'Scope',
  spectrum: 'Spectrum',
  constellation: 'Constellation',
  meter: 'Level meter',
  text: 'Messages',
  image: 'Image',
  video: 'Video',
};

export function title(inst: Instrument): string {
  if (inst.kind !== 'spectrum') return KIND_LABELS[inst.kind];
  return { spectrum: 'Spectrum', waterfall: 'Waterfall', both: 'Spectrum + waterfall' }[inst.settings.view];
}

/** The engine-side probe for an instrument drawn `width` device pixels wide. */
export function probeConfig(inst: Instrument, width: number, rate: number | null): ProbeConfig {
  const w = Math.max(16, Math.min(4096, Math.round(width)));
  switch (inst.kind) {
    case 'scope': {
      const s = inst.settings;
      return {
        kind: 'scope',
        span: scopeSpan(s, rate),
        points: w,
        channels: s.channels.length ? s.channels : ['i'],
        trigger: s.trigger,
        slope: s.slope,
        level: s.level,
        trigger_channel: s.triggerChannel,
        pretrigger: s.pretrigger,
        arm: s.arm,
      };
    }
    case 'spectrum': {
      const s = inst.settings;
      return { kind: 'spectrum', fft_size: s.fftSize, window: s.window, average: s.average, peak_hold: s.peakHold, bins: w, reset: s.reset };
    }
    case 'constellation':
      return { kind: 'constellation', points: inst.settings.points };
    case 'meter':
      return { kind: 'meter' };
    case 'text':
      return { kind: 'text' };
    case 'image':
      return { kind: 'image' };
    case 'video':
      return { kind: 'ts' };
  }
}

export const SCOPE_DIVS = 10;
export const MAX_SPAN = 1 << 20;

export function scopeSpan(s: ScopeSettings, rate: number | null): number {
  const perDiv = rate ? s.timePerDiv * rate : s.samplesPerDiv;
  return Math.max(10, Math.min(MAX_SPAN, Math.round(perDiv * SCOPE_DIVS)));
}

const KEY = 'foxygnal:instruments:';

function read(name: string | null): Instrument[] | null {
  try {
    const raw = localStorage.getItem(KEY + (name ?? ''));
    return raw ? (JSON.parse(raw) as Instrument[]) : null;
  } catch {
    return null;
  }
}

function write(name: string | null, list: Instrument[]) {
  try {
    localStorage.setItem(KEY + (name ?? ''), JSON.stringify(list));
  } catch {
    // Storage full or blocked: instruments just won't be remembered.
  }
}

type ChannelMsg =
  | { type: 'want'; id: number }
  | { type: 'inst'; inst: Instrument }
  | { type: 'update'; inst: Instrument }
  | { type: 'closed'; id: number }
  | { type: 'main-hello' }
  | { type: 'popped'; id: number };

class Instruments {
  list = $state<Instrument[]>([]);
  /** Instruments shown in their own window (by id). */
  popped = $state<Record<number, boolean>>({});
  #name: string | null = null;
  #loaded = false;
  #windows = new Map<number, Window>();
  #channel: BroadcastChannel | null = typeof BroadcastChannel === 'undefined' ? null : new BroadcastChannel('foxygnal-instruments');

  /** In the main window: answer pop-outs and remember instruments per file. */
  startMain() {
    this.#channel?.addEventListener('message', (e: MessageEvent<ChannelMsg>) => {
      const m = e.data;
      if (m.type === 'want') {
        const inst = this.get(m.id);
        if (inst) this.#post({ type: 'inst', inst: $state.snapshot(inst) as Instrument });
      } else if (m.type === 'update') {
        this.#replace(m.inst);
      } else if (m.type === 'closed') {
        this.#unpop(m.id);
      } else if (m.type === 'popped' && this.get(m.id)) {
        this.popped = { ...this.popped, [m.id]: true };
      }
    });
    // Pop-outs that survived a reload of this window check in again.
    this.#post({ type: 'main-hello' });
    setInterval(() => {
      for (const [id, w] of this.#windows) if (w.closed) this.#unpop(id);
    }, 500);
  }

  #post(m: ChannelMsg) {
    this.#channel?.postMessage(m);
  }

  /**
   * Switch to the instruments remembered for graph file `name`.
   * - `switch`: the name changed (connect, another tab): load its instruments.
   * - `carry`: the graph was saved under a new name: keep the current ones.
   * - `fresh`: a different graph was loaded: load its instruments, or none.
   */
  useGraph(name: string | null, mode: 'switch' | 'carry' | 'fresh' = 'switch') {
    if (mode === 'switch' && this.#loaded && name === this.#name) return;
    if (mode !== 'carry') {
      for (const id of Object.keys(this.popped)) this.bringBack(Number(id));
      this.list = (name !== null || mode === 'switch' ? read(name) : null) ?? [];
    }
    this.#name = name;
    this.#loaded = true;
    this.save();
  }

  save() {
    if (this.#loaded) write(this.#name, $state.snapshot(this.list) as Instrument[]);
  }

  get(id: number): Instrument | undefined {
    return this.list.find((i) => i.id === id);
  }

  add(kind: InstrumentKind, edge: EdgeRef, view?: SpectrumSettings['view']): number {
    const id = Math.max(0, ...this.list.map((i) => i.id)) + 1;
    const base = { id, edge: { ...edge }, rate: null, fps: 30 };
    let inst: Instrument;
    if (kind === 'spectrum') inst = { ...base, kind, settings: { ...defaults.spectrum(), ...(view ? { view } : {}) } };
    else if (kind === 'scope') inst = { ...base, kind, settings: defaults.scope() };
    else if (kind === 'constellation') inst = { ...base, kind, settings: defaults.constellation() };
    else if (kind === 'text') inst = { ...base, kind, settings: defaults.text(), fps: 10 };
    else if (kind === 'image') inst = { ...base, kind, settings: defaults.image(), fps: 10 };
    // Packets every 1/30 s keep the jitter buffer short.
    else if (kind === 'video') inst = { ...base, kind, settings: defaults.video(), fps: 30 };
    else inst = { ...base, kind, settings: defaults.meter(), fps: 15 };
    this.list.push(inst);
    this.save();
    return id;
  }

  /** Apply a change to an instrument's fields or settings. */
  update(id: number, patch: Partial<Base> & { settings?: Record<string, unknown> }) {
    const inst = this.get(id);
    if (!inst) return;
    const { settings, ...rest } = patch;
    Object.assign(inst, rest);
    if (settings) Object.assign(inst.settings, settings);
    this.save();
  }

  #replace(inst: Instrument) {
    const i = this.list.findIndex((x) => x.id === inst.id);
    if (i < 0) return;
    this.list[i] = inst;
    this.save();
  }

  remove(id: number) {
    this.#windows.get(id)?.close();
    this.list = this.list.filter((i) => i.id !== id);
    this.#unpop(id);
    this.save();
  }

  /** Instrument numbers watching a wire, for its badge. */
  onEdge(from: string, to: string): number[] {
    return this.list.filter((i) => i.edge.from === from && i.edge.to === to).map((i) => i.id);
  }

  popOut(id: number) {
    const w = window.open(`${location.pathname}?popout=${id}`, `foxygnal-instrument-${id}`, 'width=960,height=540');
    if (!w) return false;
    this.#windows.set(id, w);
    this.popped = { ...this.popped, [id]: true };
    return true;
  }

  bringBack(id: number) {
    this.#windows.get(id)?.close();
    this.#unpop(id);
  }

  #unpop(id: number) {
    this.#windows.delete(id);
    if (!this.popped[id]) return;
    const { [id]: _, ...rest } = this.popped;
    this.popped = rest;
  }

  // ---- In a pop-out window --------------------------------------------------

  /** Ask the main window for instrument `id`; keep it in sync from then on. */
  startPopout(id: number) {
    this.#channel?.addEventListener('message', (e: MessageEvent<ChannelMsg>) => {
      const m = e.data;
      if ((m.type === 'inst' || m.type === 'update') && m.inst.id === id) this.list = [m.inst];
      else if (m.type === 'main-hello') this.#post({ type: 'popped', id });
    });
    this.#post({ type: 'want', id });
    window.addEventListener('pagehide', () => this.#post({ type: 'closed', id }));
  }

  /** In a pop-out: change the instrument and tell the main window. */
  updatePopout(id: number, patch: Partial<Base> & { settings?: Record<string, unknown> }) {
    const inst = this.get(id);
    if (!inst) return;
    const { settings, ...rest } = patch;
    Object.assign(inst, rest);
    if (settings) Object.assign(inst.settings, settings);
    this.#post({ type: 'update', inst: $state.snapshot(inst) as Instrument });
  }

  /** The window this code runs in shows a single popped-out instrument. */
  isPopout = typeof location !== 'undefined' && new URLSearchParams(location.search).has('popout');

  /** Change an instrument wherever we are. */
  change(id: number, patch: Partial<Base> & { settings?: Record<string, unknown> }) {
    if (this.isPopout) this.updatePopout(id, patch);
    else this.update(id, patch);
  }
}

export const instruments = new Instruments();
