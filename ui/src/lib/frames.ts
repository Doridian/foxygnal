// Decoding the engine's binary probe frames (see docs/ARCHITECTURE.md).
// Header: u32 probe_id, u32 seq, u8 kind, u8[3]; then the kind's payload.
// Everything is little-endian and 4-byte aligned, so f32 arrays are viewed in place.

import type { ScopeChannel } from './api';

const CHANNELS: ScopeChannel[] = ['i', 'q', 'mag', 'phase'];

export interface ScopeFrame {
  kind: 'scope';
  /** Samples in the window. */
  span: number;
  /** Sample index of the trigger point in the window, or -1. */
  trigger: number;
  triggered: boolean;
  /** Each point is a (min, max) pair rather than one sample. */
  minmax: boolean;
  points: number;
  channels: { id: ScopeChannel; data: Float32Array }[];
}

export interface SpectrumFrame {
  kind: 'spectrum';
  fftSize: number;
  /** Centred −fs/2…+fs/2 (complex input) rather than 0…fs/2. */
  complex: boolean;
  /** dB per bin. */
  db: Float32Array;
  peak: Float32Array | null;
}

export interface ConstellationFrame {
  kind: 'constellation';
  /** Interleaved I, Q. */
  iq: Float32Array;
}

export interface MeterFrame {
  kind: 'meter';
  count: number;
  complex: boolean;
  rms: number;
  peak: number;
  mean: [number, number];
}

export interface AudioFrame {
  kind: 'audio';
  /** Samples were lost before this frame. */
  gap: boolean;
  samples: Float32Array;
}

export interface TextFrame {
  kind: 'text';
  /** Lines were lost before this frame. */
  gap: boolean;
  /** Whole lines, each ending in `\n` (may be empty with `gap`). */
  text: string;
}

export type Frame = ScopeFrame | SpectrumFrame | ConstellationFrame | MeterFrame | AudioFrame | TextFrame;

const utf8 = new TextDecoder();

export function decodeFrame(buf: ArrayBuffer): { id: number; seq: number; frame: Frame } | null {
  const v = new DataView(buf);
  if (buf.byteLength < 12) return null;
  const id = v.getUint32(0, true);
  const seq = v.getUint32(4, true);
  const kind = v.getUint8(8);
  const f32 = (at: number, n: number) => new Float32Array(buf, at, n);
  switch (kind) {
    case 1: {
      const span = v.getUint32(12, true);
      const trigger = v.getInt32(16, true);
      const flags = v.getUint8(20);
      const n = v.getUint8(21);
      const points = v.getUint32(24, true);
      const minmax = (flags & 2) !== 0;
      const per = points * (minmax ? 2 : 1);
      const channels = [];
      for (let c = 0; c < n; c++) {
        channels.push({ id: CHANNELS[v.getUint8(28 + c)] ?? 'i', data: f32(32 + c * per * 4, per) });
      }
      return { id, seq, frame: { kind: 'scope', span, trigger, triggered: (flags & 1) !== 0, minmax, points, channels } };
    }
    case 2: {
      const fftSize = v.getUint32(12, true);
      const bins = v.getUint32(16, true);
      const flags = v.getUint8(20);
      const peak = (flags & 2) !== 0 ? f32(24 + bins * 4, bins) : null;
      return { id, seq, frame: { kind: 'spectrum', fftSize, complex: (flags & 1) !== 0, db: f32(24, bins), peak } };
    }
    case 3: {
      const n = v.getUint32(12, true);
      return { id, seq, frame: { kind: 'constellation', iq: f32(16, n * 2) } };
    }
    case 4:
      return {
        id,
        seq,
        frame: {
          kind: 'meter',
          count: v.getUint32(12, true),
          complex: (v.getUint8(16) & 1) !== 0,
          rms: v.getFloat32(20, true),
          peak: v.getFloat32(24, true),
          mean: [v.getFloat32(28, true), v.getFloat32(32, true)],
        },
      };
    case 5: {
      const n = v.getUint32(12, true);
      return { id, seq, frame: { kind: 'audio', gap: (v.getUint8(16) & 1) !== 0, samples: f32(20, n) } };
    }
    case 6: {
      const n = v.getUint32(12, true);
      return { id, seq, frame: { kind: 'text', gap: (v.getUint8(16) & 1) !== 0, text: utf8.decode(new Uint8Array(buf, 20, n)) } };
    }
  }
  return null;
}
