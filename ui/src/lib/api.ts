// Types mirror foxygnal-core / foxygnal-server (their serde output).

export type StreamType = 'f32' | 'c32' | 'i16' | 'u8';

export interface PortSpec {
  name: string;
  type: StreamType | null;
}

export type ParamSpec = {
  name: string;
  label: string;
  default: unknown;
  help: string;
  /** Changes apply while running. */
  live?: boolean;
} & (
  | { kind: 'float'; min?: number; max?: number; unit?: string }
  | { kind: 'int'; min?: number; max?: number }
  | { kind: 'bool' }
  | { kind: 'enum'; options: string[] }
  | { kind: 'string' }
  | { kind: 'path' }
  /** An SDR device string; the UI offers discovered devices. */
  | { kind: 'device' }
);

export interface BlockDescriptor {
  kind: string;
  label: string;
  category: string;
  help: string;
  inputs: PortSpec[];
  outputs: PortSpec[];
  params: ParamSpec[];
  /** The block can preview its frequency response (`fetchResponse`). */
  response?: boolean;
  /** The block transmits RF: running it needs TX armed. */
  tx?: boolean;
  /** Extra words the palette search matches (aliases, the operation). */
  keywords?: string[];
}

/** A filter's magnitude response (`POST /api/blocks/{kind}/response`). */
export interface FrequencyResponse {
  /** Hz, increasing. */
  freqs: number[];
  db: number[];
  /** FIR taps; 0 for an IIR filter. */
  taps: number;
}

export interface BlockSpec {
  id: string;
  kind: string;
  params: Record<string, unknown>;
  ui?: Record<string, unknown>;
}

export interface ConnectionSpec {
  from: string;
  to: string;
}

/** `[export]`: the ports (and palette text) of a graph used as a block. */
export interface Exports {
  label?: string;
  help?: string;
  input?: { name: string; to: string | string[] }[];
  output?: { name: string; from: string }[];
}

/** A graph as stored in `.fox.toml` (`[vars]`, `[export]`, `[[block]]`, `[[connect]]`). */
export interface GraphSpec {
  /** Graph variables: numbers or expressions, referenced as `$name`. */
  vars?: Record<string, number | string>;
  export?: Exports;
  block: BlockSpec[];
  connect: ConnectionSpec[];
}

/** Block kinds that are graph files: `graph:fm-demod.fox.toml`. */
export const GRAPH_KIND = 'graph:';

export const emptyGraph = (): GraphSpec => ({ block: [], connect: [] });

export type RunState = 'stopped' | 'running' | 'error';

export interface ErrorInfo {
  block?: string;
  edge?: { from: string; to: string };
  message: string;
}

export interface Status {
  state: RunState;
  started_at?: number;
  stale: boolean;
  errors: ErrorInfo[];
  /** The current run transmits (TX was armed for it). */
  tx_armed?: boolean;
}

/** A block that would transmit, with its resolved settings (`meta.tx`). */
export interface TxInfo {
  block: string;
  kind: string;
  device?: string;
  frequency?: number;
  sample_rate?: number;
  gain?: number;
}

export interface EdgeRef {
  from: string;
  to: string;
}

export interface StreamMeta {
  sample_rate?: number;
  center_freq?: number;
}

/** Stream metadata of every wire (`meta` event and `hello`). */
export interface Meta {
  edges: ({ from: string; to: string } & StreamMeta)[];
  error?: ErrorInfo;
  /** Blocks that would transmit if the graph ran. */
  tx?: TxInfo[];
}

export interface Stats {
  blocks: { block: string; cpu: number }[];
  edges: { from: string; to: string; rate: number; items: number; fill: number }[];
}

export type ScopeChannel = 'i' | 'q' | 'mag' | 'phase';
export type TriggerMode = 'auto' | 'normal' | 'single' | 'off';
export type FftWindow = 'rectangular' | 'hann' | 'hamming' | 'blackman' | 'blackman_harris' | 'flat_top';

/** What the engine computes for a probe (mirrors `probe::ProbeConfig`). */
export type ProbeConfig =
  | {
      kind: 'scope';
      span: number;
      points: number;
      channels: ScopeChannel[];
      trigger: TriggerMode;
      slope: 'rising' | 'falling';
      level: number;
      trigger_channel: ScopeChannel;
      pretrigger: number;
      arm: number;
    }
  | { kind: 'spectrum'; fft_size: number; window: FftWindow; average: number; peak_hold: boolean; bins: number; reset: number }
  | { kind: 'constellation'; points: number }
  | { kind: 'meter' }
  | { kind: 'audio' }
  | { kind: 'text' };

/** A recording of a wire in progress, or just ended (`record` event). */
export interface RecordStatus {
  edge: EdgeRef;
  /** File names without `.sigmf-meta` / `.sigmf-data`. */
  name: string;
  /** The metadata file relative to the project directory. */
  path: string;
  started_at: number;
  elapsed_ms: number;
  samples: number;
  bytes: number;
  /** Stretches of samples lost (only possible right after starting). */
  gaps: number;
  done: boolean;
  error?: string;
}

/** A recording in the project's `recordings/` (`GET /api/recordings`). */
export interface RecordingInfo {
  name: string;
  /** Relative to the project directory: a SigMF Source's `path`. */
  path: string;
  modified: number;
  datatype: string | null;
  sample_rate: number | null;
  frequency: number | null;
  samples: number;
  bytes: number;
  captures: number;
  annotations: number;
  datetime: string | null;
  hw: string | null;
  description: string | null;
  /** Being written right now. */
  recording: boolean;
  error?: string;
}

export type ServerEvent =
  | { op: 'hello'; client: number; graph: GraphSpec; name: string | null; version: number; status: Status; meta: Meta; allow_tx: boolean; recordings?: RecordStatus[] }
  | { op: 'graph'; graph: GraphSpec; name: string | null; version: number; from: number }
  | ({ op: 'status' } & Status)
  | ({ op: 'stats' } & Stats)
  | ({ op: 'meta' } & Meta)
  | { op: 'probe'; id: number; error: string | null }
  | ({ op: 'record' } & RecordStatus);

export type ClientCommand =
  | { op: 'set_graph'; graph: GraphSpec; name?: string | null }
  | { op: 'run'; arm_tx?: boolean }
  | { op: 'stop' }
  | { op: 'probe_set'; id: number; edge: EdgeRef; probe: ProbeConfig; fps?: number }
  | { op: 'probe_remove'; id: number }
  | { op: 'record_start'; edge: EdgeRef; name?: string }
  | { op: 'record_stop'; edge: EdgeRef };

export interface ExampleInfo {
  name: string;
  title: string;
  description: string;
  blocks: number;
}

/** A device found by discovery (`GET /api/devices`). */
export interface DeviceInfo {
  driver: string;
  /** The device string that opens exactly this device. */
  args: string;
  label: string;
  serial: string | null;
  rx: boolean;
  tx: boolean;
}

/** One driver's discovery result. */
export interface Discovery {
  driver: string;
  label: string;
  devices: DeviceInfo[];
  /** Why discovery failed (e.g. library not installed). */
  error: string | null;
}

export interface FileInfo {
  name: string;
  modified: number;
}

async function call<T>(method: string, url: string, body?: BodyInit, type?: string): Promise<T> {
  const r = await fetch(url, { method, body, headers: type ? { 'content-type': type } : undefined });
  if (!r.ok) throw new Error((await r.text()) || `${method} ${url}: ${r.status}`);
  const ct = r.headers.get('content-type') ?? '';
  return (ct.includes('json') ? r.json() : r.text()) as Promise<T>;
}

const json = (v: unknown) => JSON.stringify(v);

export const fetchBlocks = () => call<BlockDescriptor[]>('GET', '/api/blocks');
export const fetchDevices = () => call<Discovery[]>('GET', '/api/devices');
export const fetchExamples = () => call<ExampleInfo[]>('GET', '/api/examples');
export const fetchExample = (name: string) => call<GraphSpec>('GET', `/api/examples/${encodeURIComponent(name)}`);
export const listFiles = () => call<FileInfo[]>('GET', '/api/files');
export const openFile = (name: string) => call<GraphSpec>('GET', `/api/files/${encodeURIComponent(name)}`);
export const saveFile = (name: string, g: GraphSpec) =>
  call<string>('PUT', `/api/files/${encodeURIComponent(name)}`, json(g), 'application/json');
export const parseToml = (text: string) => call<GraphSpec>('POST', '/api/toml/parse', text, 'text/plain');

/** A GNU Radio `.grc` converted by the engine. Save it as `name` (the `.grc` itself is never written). */
export interface Imported {
  graph: GraphSpec;
  /** What didn't convert exactly, one message each. */
  warnings: string[];
  title: string | null;
  name: string;
}
export const isGrc = (name: string) => name.toLowerCase().endsWith('.grc');
/** Convert a `.grc` in the project folder. */
export const importFile = (name: string) => call<Imported>('GET', `/api/import/${encodeURIComponent(name)}`);
/** Convert uploaded `.grc` text. */
export const importText = (text: string) => call<Imported>('POST', '/api/import', text, 'text/plain');
export const renderToml = (g: GraphSpec) => call<string>('POST', '/api/toml/render', json(g), 'application/json');
export const listRecordings = () => call<RecordingInfo[]>('GET', '/api/recordings');
export const deleteRecording = (name: string) => call<string>('DELETE', `/api/recordings/${encodeURIComponent(name)}`);
/** The frequency response a block would have with `params` at input rate `sample_rate`. */
export const fetchResponse = (kind: string, params: Record<string, unknown>, sample_rate: number) =>
  call<FrequencyResponse>('POST', `/api/blocks/${encodeURIComponent(kind)}/response`, json({ params, sample_rate }), 'application/json');

/** A port's concrete type: fixed by the descriptor, else taken from the `type` param. */
export function portType(p: PortSpec, params: Record<string, unknown>): StreamType {
  return p.type ?? ((params.type as StreamType) || 'c32');
}
