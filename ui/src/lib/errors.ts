import type { ErrorInfo } from './api';
import { edgeId } from './graph';

/** Run errors indexed for pinning: by block id, by edge id, and the rest. */
export function indexErrors(errors: ErrorInfo[]) {
  const blocks = new Map<string, string>();
  const edges = new Map<string, string>();
  const global: string[] = [];
  for (const e of errors) {
    if (e.block) blocks.set(e.block, e.message);
    else if (e.edge) edges.set(edgeId(e.edge.from, e.edge.to), e.message);
    else global.push(e.message);
  }
  return { blocks, edges, global };
}
