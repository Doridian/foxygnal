// Conversion between the server's GraphSpec and Svelte Flow nodes/edges, and
// connection rules with human-readable rejection reasons.

import type { Connection, Edge, Node } from '@xyflow/svelte';
import { GRAPH_KIND, PARAM_PORT, portType, type BlockDescriptor, type GraphSpec, type StreamType } from './api';

export type BlockData = {
  desc: BlockDescriptor;
  params: Record<string, unknown>;
  /** Settings shown as inputs (handle `@name`), connected or not. */
  wired?: string[];
  /** Stream inputs set to a constant (no handle). */
  consts?: Record<string, unknown>;
};
export type BlockNode = Node<BlockData, 'block'>;
export type Wire = Edge<Record<string, unknown>, 'typed'>;

/** Stand-in descriptor for a block kind this engine doesn't know. */
function unknownDesc(kind: string): BlockDescriptor {
  return { kind, label: kind, category: 'Unknown', help: `This engine has no block kind \`${kind}\`.`, inputs: [], outputs: [], params: [] };
}

export const edgeId = (from: string, to: string) => `${from}->${to}`;

/** `block.port` where the port may be a name or an index. Returns the port name. */
function resolvePort(endpoint: string, nodes: Map<string, BlockNode>, dir: 'inputs' | 'outputs') {
  const dot = endpoint.lastIndexOf('.');
  const block = endpoint.slice(0, dot);
  const port = endpoint.slice(dot + 1);
  const ports = nodes.get(block)?.data.desc[dir] ?? [];
  const byIndex = /^\d+$/.test(port) ? ports[Number(port)] : undefined;
  return { block, port: ports.some((p) => p.name === port) ? port : (byIndex?.name ?? port) };
}

/** The graph-level parts of a spec: variables and exports. */
export type GraphExtras = Pick<GraphSpec, 'vars' | 'export'>;

export function extrasOf(g: GraphSpec): GraphExtras {
  return { vars: g.vars, export: g.export };
}

export function fromSpec(g: GraphSpec, catalog: Map<string, BlockDescriptor>, selected = new Set<string>()) {
  const nodes: BlockNode[] = g.block.map((b, i) => ({
    id: b.id,
    type: 'block',
    position: {
      x: typeof b.ui?.x === 'number' ? b.ui.x : (i % 4) * 240,
      y: typeof b.ui?.y === 'number' ? b.ui.y : Math.floor(i / 4) * 170,
    },
    selected: selected.has(b.id),
    data: {
      desc: catalog.get(b.kind) ?? unknownDesc(b.kind),
      params: { ...b.params },
      wired: wiredOf(b.id, b.ui?.wired, g),
      ...(b.const_inputs && Object.keys(b.const_inputs).length ? { consts: { ...b.const_inputs } } : {}),
    },
  }));
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const edges: Wire[] = g.connect.map((c) => {
    const f = resolvePort(c.from, byId, 'outputs');
    const t = resolvePort(c.to, byId, 'inputs');
    const id = edgeId(`${f.block}.${f.port}`, `${t.block}.${t.port}`);
    return { id, type: 'typed', source: f.block, sourceHandle: f.port, target: t.block, targetHandle: t.port, selected: selected.has(id) };
  });
  return { nodes, edges };
}

/** Settings of block `id` shown as inputs: the UI flag plus any `@param` wire. */
function wiredOf(id: string, flag: unknown, g: GraphSpec): string[] {
  const names = new Set(Array.isArray(flag) ? flag.filter((x): x is string => typeof x === 'string') : []);
  for (const c of g.connect) {
    const dot = c.to.lastIndexOf('.');
    const port = c.to.slice(dot + 1);
    if (c.to.slice(0, dot) === id && port.startsWith(PARAM_PORT)) names.add(port.slice(1));
  }
  return [...names];
}

export function toSpec(nodes: BlockNode[], edges: Wire[], extras: GraphExtras = {}): GraphSpec {
  const ex = extras.export;
  return {
    ...(extras.vars && Object.keys(extras.vars).length ? { vars: extras.vars } : {}),
    ...(ex && (ex.label || ex.help || ex.input?.length || ex.output?.length) ? { export: ex } : {}),
    block: nodes.map((n) => ({
      id: n.id,
      kind: n.data.desc.kind,
      params: n.data.params,
      ...(n.data.consts && Object.keys(n.data.consts).length ? { const_inputs: n.data.consts } : {}),
      ui: { x: Math.round(n.position.x), y: Math.round(n.position.y), ...(n.data.wired?.length ? { wired: n.data.wired } : {}) },
    })),
    connect: edges.map((e) => ({ from: `${e.source}.${e.sourceHandle}`, to: `${e.target}.${e.targetHandle}` })),
  };
}

/** The setting behind handle `@name`, if `handle` is one. */
export function paramOfHandle(node: BlockNode | undefined, handle: string | null | undefined) {
  if (!handle?.startsWith(PARAM_PORT)) return undefined;
  return node?.data.desc.params.find((p) => p.name === handle.slice(1));
}

export function typeOf(node: BlockNode | undefined, handle: string | null | undefined, dir: 'inputs' | 'outputs'): StreamType | null {
  // A setting wired as an input takes f32.
  if (dir === 'inputs' && handle?.startsWith(PARAM_PORT)) return node ? 'f32' : null;
  const p = node?.data.desc[dir].find((p) => p.name === handle);
  return p && node ? portType(p, node.data.params) : null;
}

/** Why a connection can't be made, or `null` if it can. */
export function rejectReason(c: Connection, nodes: BlockNode[]): string | null {
  if (c.source === c.target) return 'A block cannot feed itself.';
  const src = nodes.find((n) => n.id === c.source);
  const dst = nodes.find((n) => n.id === c.target);
  const from = typeOf(src, c.sourceHandle, 'outputs');
  const to = typeOf(dst, c.targetHandle, 'inputs');
  if (!from || !to) return null;
  if (from !== to) {
    const setting = paramOfHandle(dst, c.targetHandle);
    if (setting) {
      return `\`${c.source}.${c.sourceHandle}\` produces ${from}, but \`${c.target}.${c.targetHandle}\` (the ${setting.label} setting, wired as an input) expects f32.`;
    }
    return `\`${c.source}.${c.sourceHandle}\` produces ${from}, but \`${c.target}.${c.targetHandle}\` expects ${to}. Set both blocks to the same type.`;
  }
  return null;
}

/**
 * Converter blocks that turn `from` into `to`: category "Converters", one
 * input and one output, both with fixed types. Offered when a connection is
 * rejected for a type mismatch.
 */
export function convertersFor(from: StreamType, to: StreamType, catalog: Iterable<BlockDescriptor>): BlockDescriptor[] {
  return [...catalog].filter(
    (d) =>
      d.category === 'Converters' &&
      d.inputs.length === 1 &&
      d.outputs.length === 1 &&
      d.inputs[0].type === from &&
      d.outputs[0].type === to,
  );
}

/** Id stem for a new block of `kind`: `signal_source` → `signal`, plugin `example:gain` → `gain`. */
export const baseForKind = (kind: string) =>
  kind.startsWith(GRAPH_KIND)
    ? kind.slice(GRAPH_KIND.length).replace(/\.fox\.toml$/, '')
    : kind.slice(kind.indexOf(':') + 1).replace(/_(source|sink)$/, '');

/** Id stem for a copy of block `id`: `gain_3` → `gain`. */
export const baseForCopy = (id: string) => id.replace(/_\d+$/, '');

/** A fresh block id like `gain_2`, unique in `taken`. */
export function freshId(base: string, taken: Set<string>): string {
  base = base.replace(/[^A-Za-z0-9_-]/g, '_') || 'block';
  for (let i = 1; ; i++) {
    const id = `${base}_${i}`;
    if (!taken.has(id)) return id;
  }
}

export const ID_RE = /^[A-Za-z_][A-Za-z0-9_-]*$/;
