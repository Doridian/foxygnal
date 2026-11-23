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
);

export interface BlockDescriptor {
  kind: string;
  label: string;
  category: string;
  help: string;
  inputs: PortSpec[];
  outputs: PortSpec[];
  params: ParamSpec[];
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

/** A graph as stored in `.fox.toml` (`[[block]]`, `[[connect]]`). */
export interface GraphSpec {
  block: BlockSpec[];
  connect: ConnectionSpec[];
}

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
}

export interface EdgeRef {
  from: string;
  to: string;
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
  | { kind: 'meter' };

export type ServerEvent =
  | { op: 'hello'; client: number; graph: GraphSpec; name: string | null; version: number; status: Status }
  | { op: 'graph'; graph: GraphSpec; name: string | null; version: number; from: number }
  | ({ op: 'status' } & Status)
  | ({ op: 'stats' } & Stats)
  | { op: 'probe'; id: number; error: string | null };

export type ClientCommand =
  | { op: 'set_graph'; graph: GraphSpec; name?: string | null }
  | { op: 'run' }
  | { op: 'stop' }
  | { op: 'probe_set'; id: number; edge: EdgeRef; probe: ProbeConfig; fps?: number }
  | { op: 'probe_remove'; id: number };

export interface ExampleInfo {
  name: string;
  title: string;
  description: string;
  blocks: number;
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
export const fetchExamples = () => call<ExampleInfo[]>('GET', '/api/examples');
export const fetchExample = (name: string) => call<GraphSpec>('GET', `/api/examples/${encodeURIComponent(name)}`);
export const listFiles = () => call<FileInfo[]>('GET', '/api/files');
export const openFile = (name: string) => call<GraphSpec>('GET', `/api/files/${encodeURIComponent(name)}`);
export const saveFile = (name: string, g: GraphSpec) =>
  call<string>('PUT', `/api/files/${encodeURIComponent(name)}`, json(g), 'application/json');
export const parseToml = (text: string) => call<GraphSpec>('POST', '/api/toml/parse', text, 'text/plain');
export const renderToml = (g: GraphSpec) => call<string>('POST', '/api/toml/render', json(g), 'application/json');

/** A port's concrete type: fixed by the descriptor, else taken from the `type` param. */
export function portType(p: PortSpec, params: Record<string, unknown>): StreamType {
  return p.type ?? ((params.type as StreamType) || 'c32');
}
