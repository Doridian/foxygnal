// Types mirror foxygnal-core's registry (serde output of BlockDescriptor).

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

export async function fetchBlocks(): Promise<BlockDescriptor[]> {
  const r = await fetch('/api/blocks');
  if (!r.ok) throw new Error(`GET /api/blocks: ${r.status}`);
  return r.json();
}

/** A port's concrete type: fixed by the descriptor, else taken from the `type` param. */
export function portType(p: PortSpec, params: Record<string, unknown>): StreamType {
  return p.type ?? ((params.type as StreamType) || 'c32');
}
