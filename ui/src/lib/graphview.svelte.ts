// The graph and block catalog as instruments see them. The editor keeps this
// up to date; a pop-out window fills it from its own session.

import { emptyGraph, portType, type BlockDescriptor, type EdgeRef, type GraphSpec, type StreamType } from './api';

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

  /**
   * The sample rate on a wire, guessed from the nearest upstream block with a
   * `sample_rate` parameter. Stream metadata (M3) will make this exact.
   */
  guessRate(endpoint: string): { rate: number; block: string } | null {
    const seen = new Set<string>();
    let frontier = [endpoint.slice(0, endpoint.lastIndexOf('.'))];
    while (frontier.length) {
      const next: string[] = [];
      for (const id of frontier) {
        if (seen.has(id)) continue;
        seen.add(id);
        const b = this.spec.block.find((b) => b.id === id);
        const r = b?.params.sample_rate;
        if (typeof r === 'number' && r > 0) return { rate: r, block: id };
        for (const c of this.spec.connect) {
          if (c.to.slice(0, c.to.lastIndexOf('.')) === id) next.push(c.from.slice(0, c.from.lastIndexOf('.')));
        }
      }
      frontier = next;
    }
    return null;
  }
}

export const graphView = new GraphView();
