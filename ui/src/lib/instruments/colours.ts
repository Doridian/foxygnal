// Trace colours. The first channel uses the wire's stream-type colour, so a
// trace always matches the wire it came from.

import type { ScopeChannel, StreamType } from '../api';
import { cssVar } from '../plot';

const OTHERS: Record<Exclude<ScopeChannel, 'i'>, string> = {
  q: '#ffd9a6',
  mag: '--t-f32',
  phase: '--t-i16',
};

/** A canvas-ready colour for a scope channel. `first` may be a CSS variable name. */
export function channelColour(ch: ScopeChannel, first: string, el: Element): string {
  const c = ch === 'i' ? first : OTHERS[ch];
  return c.startsWith('--') ? cssVar(el, c) : c;
}

export const typeColour = (t: StreamType | null) => (t ? `--t-${t}` : '--muted');

export const CHANNEL_LABELS: Record<ScopeChannel, string> = { i: 'I', q: 'Q', mag: '|x|', phase: '∠x' };
