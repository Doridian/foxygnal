// Messages between the Video instrument and its worker (worker.ts).

import type { Program } from './ts';

export type ToWorker =
  | { type: 'packets'; data: ArrayBuffer; gap: boolean }
  /** null: the first program with video. */
  | { type: 'program'; number: number | null }
  /** The sound track's PID; null: the first this page decodes. */
  | { type: 'track'; pid: number | null }
  | { type: 'audio'; on: boolean }
  /** A new run: forget the stream. */
  | { type: 'reset' }
  | { type: 'close' };

export interface VideoStats {
  pid: number;
  codec: string;
  /** The WebCodecs codec string, or `wasm` for our MPEG-2 decoder. */
  decoder: string | null;
  width: number;
  height: number;
  /** Frames per second, measured. */
  fps: number;
  /** Bits per second of this stream's packets. */
  bitrate: number;
  decoded: number;
  /** Frames dropped (late, decoder behind, or waiting for a key frame). */
  dropped: number;
  errors: number;
  /** Mean time to decode a picture in the WebAssembly decoder (0 for WebCodecs). */
  decodeMs: number;
  /** Why nothing is shown, if so ("not supported", "waiting for a key frame", …). */
  note: string | null;
}

export interface AudioStats {
  pid: number;
  codec: string;
  lang: string | null;
  /** The channels sent (AC-3: 2.0, 5.1, …), when played as fewer (stereo); else null. */
  layout: string | null;
  /** The WebCodecs codec string, or `wasm` for our own decoders; null until sound is on. */
  decoder: string | null;
  rate: number;
  channels: number;
  bitrate: number;
  /** Frames decoded (since the stream was chosen). */
  frames: number;
  /** Frames that failed their CRC, and lost frame sync (MPEG audio, AC-3). */
  errors: number;
  note: string | null;
}

export interface Stats {
  /** Bits per second of the whole stream. */
  bitrate: number;
  packets: number;
  tei: number;
  ccErrors: number;
  /** Packets were lost between engine and browser (gaps). */
  gaps: number;
  video: VideoStats | null;
  audio: AudioStats | null;
}

export type FromWorker =
  | { type: 'programs'; programs: Program[]; selected: number | null }
  | { type: 'stats'; stats: Stats }
  /** `due`: when to show it, in ms since the epoch (performance.timeOrigin + now). */
  | { type: 'frame'; frame: VideoFrame; due: number; aspect: number }
  | { type: 'pcm'; samples: Float32Array<ArrayBuffer>; channels: 1 | 2; rate: number; due: number }
  /** Timing jumped (a new run, a looped file): drop queued frames and sound. */
  | { type: 'flush' }
  /** Another sound track: drop the queued sound. */
  | { type: 'audio-flush' };
