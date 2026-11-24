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
  /** 1 (mono) or 2 (stereo: `samples` interleaves left, right). */
  channels: 1 | 2;
  samples: Float32Array;
}

export interface TextFrame {
  kind: 'text';
  /** Lines were lost before this frame. */
  gap: boolean;
  /** Whole lines, each ending in `\n` (may be empty with `gap`). */
  text: string;
}

/** One row of a decoded picture (a raster record, see docs/ARCHITECTURE.md). */
export interface ImageRow {
  /** The decoder's picture number (counts from 1 each run). */
  picture: number;
  row: number;
  /** Rows the picture will have; 0 if not known. */
  height: number;
  width: number;
  /** 1 = grey, 3 = RGB. */
  channels: 1 | 3;
  /** The picture's last row. */
  last: boolean;
  pixels: Uint8Array;
}

/** A picture's name (a name record: sent before the picture's first row). */
export interface ImageName {
  picture: number;
  /** `/`-separated parts, e.g. `G18/FD/CH13/G18_FD_CH13_20261123T120020Z`. */
  name: string;
}

export interface ImageFrame {
  kind: 'image';
  /** Rows were lost before this frame. */
  gap: boolean;
  rows: ImageRow[];
  names: ImageName[];
}

export interface TsFrame {
  kind: 'ts';
  /** Packets were lost before this frame: by the engine, or frames dropped on the way (seq jumped). */
  gap: boolean;
  /** Whole 188-byte transport stream packets. */
  packets: Uint8Array;
  seq: number;
}

export type Frame = ScopeFrame | SpectrumFrame | ConstellationFrame | MeterFrame | AudioFrame | TextFrame | ImageFrame | TsFrame;

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
      // n frames; flag 2 = stereo, f32[2n] interleaved L, R.
      const n = v.getUint32(12, true);
      const flags = v.getUint8(16);
      const channels = (flags & 2) !== 0 ? 2 : 1;
      return { id, seq, frame: { kind: 'audio', gap: (flags & 1) !== 0, channels, samples: f32(20, n * channels) } };
    }
    case 6: {
      const n = v.getUint32(12, true);
      return { id, seq, frame: { kind: 'text', gap: (v.getUint8(16) & 1) !== 0, text: utf8.decode(new Uint8Array(buf, 20, n)) } };
    }
    case 7: {
      // n records ("FXIM", u32 picture, u16 row, height, width, u8 channels, flags, pixels), each padded to 4;
      // name records ("FXNM", u32 picture, u16 length, 6 zero bytes, the name) among them.
      const n = v.getUint32(12, true);
      const rows: ImageRow[] = [];
      const names: ImageName[] = [];
      let at = 20;
      for (let i = 0; i < n && at + 16 <= buf.byteLength; i++) {
        if (v.getUint8(at + 1) === 0x58 && v.getUint8(at + 2) === 0x4e) {
          // "FXNM"
          const len = v.getUint16(at + 8, true);
          if (at + 16 + len > buf.byteLength) break;
          names.push({ picture: v.getUint32(at + 4, true), name: utf8.decode(new Uint8Array(buf, at + 16, len)) });
          at += (16 + len + 3) & ~3;
          continue;
        }
        const width = v.getUint16(at + 12, true);
        const channels = v.getUint8(at + 14) === 3 ? 3 : 1;
        const len = width * channels;
        if (at + 16 + len > buf.byteLength) break;
        rows.push({
          picture: v.getUint32(at + 4, true),
          row: v.getUint16(at + 8, true),
          height: v.getUint16(at + 10, true),
          width,
          channels,
          last: (v.getUint8(at + 15) & 1) !== 0,
          pixels: new Uint8Array(buf, at + 16, len),
        });
        at += (16 + len + 3) & ~3;
      }
      return { id, seq, frame: { kind: 'image', gap: (v.getUint8(16) & 1) !== 0, rows, names } };
    }
    case 8: {
      // n packets of 188 bytes.
      const n = Math.min(v.getUint32(12, true), Math.floor((buf.byteLength - 20) / 188));
      return { id, seq, frame: { kind: 'ts', gap: (v.getUint8(16) & 1) !== 0, packets: new Uint8Array(buf, 20, n * 188), seq } };
    }
  }
  return null;
}
