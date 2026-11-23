// The MPEG audio Layer I/II (MP1/MP2) decoder: crates/foxygnal-mp2 compiled
// to WebAssembly (scripts/build-mpeg2-wasm.sh). MP2 is the sound of most DVB
// services, and WebCodecs can't decode it. The module imports nothing; its
// exports are a small C ABI (see the crate's `wasm` module). No imports here
// either, so `npm test` runs it under node.

interface Exports {
  memory: WebAssembly.Memory;
  mp2a_new(): number;
  mp2a_free(d: number): void;
  mp2a_input(d: number, len: number): number;
  mp2a_push(d: number, len: number, pts: number): void;
  mp2a_decode(d: number): number;
  mp2a_flush(d: number): void;
  mp2a_reset(d: number): void;
  mp2a_pcm(d: number): number;
  mp2a_pcm_len(d: number): number;
  mp2a_channels(d: number): number;
  mp2a_sample_rate(d: number): number;
  mp2a_bitrate(d: number): number;
  mp2a_pts(d: number): number;
  mp2a_layer(d: number): number;
  mp2a_errors(d: number): number;
}

export interface Mp2Frame {
  /** Interleaved, ±1 full scale. A copy. */
  samples: Float32Array<ArrayBuffer>;
  channels: 1 | 2;
  rate: number;
  /** Bits per second. */
  bitrate: number;
  /** 90 kHz (the low 33 bits), or null. */
  pts: number | null;
}

let module: Promise<WebAssembly.Module> | null = null;

function loadModule(url: string): Promise<WebAssembly.Module> {
  module ??= fetch(url)
    .then((r) => {
      if (!r.ok) throw new Error(`${url}: ${r.status}`);
      return r.arrayBuffer();
    })
    .then((b) => WebAssembly.compile(b));
  return module;
}

export class Mp2Decoder {
  #x: Exports;
  #d: number;

  private constructor(x: Exports) {
    this.#x = x;
    this.#d = x.mp2a_new();
  }

  static async create(url: string): Promise<Mp2Decoder> {
    return Mp2Decoder.fromModule(await loadModule(url));
  }

  static async fromModule(m: WebAssembly.Module): Promise<Mp2Decoder> {
    const inst = await WebAssembly.instantiate(m, {});
    return new Mp2Decoder(inst.exports as unknown as Exports);
  }

  /** A PES packet's payload; `pts` (90 kHz) belongs to the first frame that starts in it. */
  push(es: Uint8Array, pts: number | null) {
    const x = this.#x;
    const at = x.mp2a_input(this.#d, es.length);
    new Uint8Array(x.memory.buffer, at, es.length).set(es);
    // The decoder keeps 33-bit timestamps; ours are unwrapped, so pass the low 33 bits.
    x.mp2a_push(this.#d, es.length, pts === null ? -1 : pts % 2 ** 33);
  }

  /** The next frame, or null if it needs more input. Call until null before the next push. */
  next(): Mp2Frame | null {
    const x = this.#x;
    const d = this.#d;
    if (!x.mp2a_decode(d)) return null;
    const len = x.mp2a_pcm_len(d);
    const samples = new Float32Array(len);
    samples.set(new Float32Array(x.memory.buffer, x.mp2a_pcm(d), len));
    const pts = x.mp2a_pts(d);
    return {
      samples,
      channels: x.mp2a_channels(d) === 1 ? 1 : 2,
      rate: x.mp2a_sample_rate(d),
      bitrate: x.mp2a_bitrate(d),
      pts: pts < 0 ? null : pts,
    };
  }

  /** End of stream: the last frame needn't be followed by another. */
  flush() {
    this.#x.mp2a_flush(this.#d);
  }

  /** After lost packets: find the frame grid again. */
  reset() {
    this.#x.mp2a_reset(this.#d);
  }

  /** The layer of the last frame header seen (3 is never decoded), 0 if none yet. */
  get layer(): number {
    return this.#x.mp2a_layer(this.#d);
  }

  /** Frames that failed their CRC (played as silence), and times sync was lost. */
  get errors(): number {
    return this.#x.mp2a_errors(this.#d);
  }

  free() {
    this.#x.mp2a_free(this.#d);
  }
}
