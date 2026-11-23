// The graph and block catalog as instruments see them. The editor keeps this
// up to date; a pop-out window fills it from its own session.

import { emptyGraph, portType, type BlockDescriptor, type EdgeRef, type GraphSpec, type StreamMeta, type StreamType } from './api';

class GraphView {
  spec = $state.raw<GraphSpec>(emptyGraph());
  catalog = $state.raw(new Map<string, BlockDescriptor>());

  hasEdge(e: EdgeRef): boolean {
    return this.spec.connect.some((c) => c.from === e.from && c.to === e.to);
  }

  /** The stream type an output carries, if the block and port are known. */
  outputType(endpoint: string): StreamType | null {
    const dot = endpoint.lastIndexOf('.');
    const b = this.spec.block.find((b) => b.id === endpoint.slice(0, dot));
    const desc = b && this.catalog.get(b.kind);
    const port = endpoint.slice(dot + 1);
    const p = desc?.outputs.find((p, i) => p.name === port || String(i) === port);
    return p && b ? portType(p, b.params) : null;
  }

  /** Stream metadata by output endpoint (`block.port`), as the engine worked it out. */
  meta = $state.raw(new Map<string, StreamMeta>());

  /** The sample rate an output carries, if known. */
  rateOf(endpoint: string): number | null {
    return this.meta.get(endpoint)?.sample_rate ?? null;
  }

  /** The RF frequency 0 Hz corresponds to on an output, if known. */
  centerOf(endpoint: string): number | null {
    return this.meta.get(endpoint)?.center_freq ?? null;
  }

}

export const graphView = new GraphView();
