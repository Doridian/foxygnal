// The MPEG-2 video decoder: crates/foxygnal-mpeg2 compiled to WebAssembly
// (scripts/build-mpeg2-wasm.sh). WebCodecs can't decode MPEG-2, which most
// DVB services still use. The module imports nothing; its exports are a small
// C ABI (see the crate's `wasm` module).

interface Exports {
  memory: WebAssembly.Memory;
  mp2v_new(): number;
  mp2v_free(d: number): void;
  mp2v_input(d: number, len: number): number;
  mp2v_push(d: number, len: number, pts: number): void;
  mp2v_decode(d: number): number;
  mp2v_flush(d: number): void;
  mp2v_reset(d: number): void;
  mp2v_width(d: number): number;
  mp2v_height(d: number): number;
  mp2v_data(d: number): number;
  mp2v_data_len(d: number): number;
  mp2v_pts(d: number): number;
  mp2v_frame_rate_num(d: number): number;
  mp2v_frame_rate_den(d: number): number;
  mp2v_aspect(d: number): number;
  mp2v_picture_type(d: number): number;
  mp2v_progressive(d: number): number;
  mp2v_errors(d: number): number;
}

export interface Mpeg2Picture {
  width: number;
  height: number;
  /** I420: Y, then U and V at half size (rounded up). A copy. */
  data: Uint8Array<ArrayBuffer>;
  /** 90 kHz, or null. */
  pts: number | null;
  frameRate: number;
  /** aspect_ratio_information: 1 square pixels, 2 4:3, 3 16:9, 4 2.21:1. */
  aspect: number;
  type: 'I' | 'P' | 'B';
  progressive: boolean;
}

let module: Promise<WebAssembly.Module> | null = null;

export function loadModule(url: string): Promise<WebAssembly.Module> {
  module ??= fetch(url)
    .then((r) => {
      if (!r.ok) throw new Error(`${url}: ${r.status}`);
      return r.arrayBuffer();
    })
    .then((b) => WebAssembly.compile(b));
  return module;
}

export class Mpeg2Decoder {
  #x: Exports;
  #d: number;

  private constructor(x: Exports) {
    this.#x = x;
    this.#d = x.mp2v_new();
  }

  static async create(url: string): Promise<Mpeg2Decoder> {
    const inst = await WebAssembly.instantiate(await loadModule(url), {});
    return new Mpeg2Decoder(inst.exports as unknown as Exports);
  }

  /** Elementary stream bytes; `pts` (90 kHz) belongs to the first picture that starts in them. */
  push(es: Uint8Array, pts: number | null) {
    const x = this.#x;
    const at = x.mp2v_input(this.#d, es.length);
    new Uint8Array(x.memory.buffer, at, es.length).set(es);
    // The decoder keeps 33-bit timestamps; ours are unwrapped, so pass the low 33 bits.
    x.mp2v_push(this.#d, es.length, pts === null ? -1 : pts % 2 ** 33);
  }

  /** The next picture in display order, or null if it needs more input. */
  next(): Mpeg2Picture | null {
    const x = this.#x;
    const d = this.#d;
    if (!x.mp2v_decode(d)) return null;
    const len = x.mp2v_data_len(d);
    const data = new Uint8Array(len);
    data.set(new Uint8Array(x.memory.buffer, x.mp2v_data(d), len));
    const pts = x.mp2v_pts(d);
    const den = x.mp2v_frame_rate_den(d);
    return {
      width: x.mp2v_width(d),
      height: x.mp2v_height(d),
      data,
      pts: pts < 0 ? null : pts,
      frameRate: den ? x.mp2v_frame_rate_num(d) / den : 0,
      aspect: x.mp2v_aspect(d),
      type: (['I', 'P', 'B'] as const)[x.mp2v_picture_type(d) - 1] ?? 'I',
      progressive: x.mp2v_progressive(d) !== 0,
    };
  }

  flush() {
    this.#x.mp2v_flush(this.#d);
  }

  reset() {
    this.#x.mp2v_reset(this.#d);
  }

  get errors(): number {
    return this.#x.mp2v_errors(this.#d);
  }

  free() {
    this.#x.mp2v_free(this.#d);
  }
}
