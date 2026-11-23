// Sound decoders compiled from foxygnal's crates to WebAssembly
// (scripts/build-mpeg2-wasm.sh): MPEG audio Layer I/II (mp2.ts) and AC-3
// (ac3.ts). Each module imports nothing and exports the same small C ABI
// under its own prefix (see the crates' `wasm` modules). No imports here
// either, so `npm test` runs them under node.

interface Exports {
  memory: WebAssembly.Memory;
  [name: string]: unknown;
}

type Fn = (...args: number[]) => number;

export interface AudioFrame {
  /** Interleaved, ±1 full scale. A copy. */
  samples: Float32Array<ArrayBuffer>;
  channels: 1 | 2;
  rate: number;
  /** Bits per second. */
  bitrate: number;
  /** 90 kHz (the low 33 bits), or null. */
  pts: number | null;
}

const modules = new Map<string, Promise<WebAssembly.Module>>();

/** Fetch and compile a module once per URL. */
export function loadModule(url: string): Promise<WebAssembly.Module> {
  let m = modules.get(url);
  if (!m) {
    m = fetch(url)
      .then((r) => {
        if (!r.ok) throw new Error(`${url}: ${r.status}`);
        return r.arrayBuffer();
      })
      .then((b) => WebAssembly.compile(b));
    // A failed load may be tried again.
    m.catch(() => modules.delete(url));
    modules.set(url, m);
  }
  return m;
}

export class WasmAudioDecoder {
  #x: Exports;
  #p: string;
  #d: number;

  protected constructor(inst: WebAssembly.Instance, prefix: string) {
    this.#x = inst.exports as unknown as Exports;
    this.#p = prefix;
    this.#d = this.call('new');
  }

  /** Call the module's `<prefix><name>(handle, …args)`. */
  protected call(name: string, ...args: number[]): number {
    const f = this.#x[this.#p + name] as Fn;
    return name === 'new' ? f() : f(this.#d, ...args);
  }

  /** A PES packet's payload; `pts` (90 kHz) belongs to the first frame that starts in it. */
  push(es: Uint8Array, pts: number | null) {
    const at = this.call('input', es.length);
    new Uint8Array(this.#x.memory.buffer, at, es.length).set(es);
    // The decoder keeps 33-bit timestamps; ours are unwrapped, so pass the low 33 bits.
    this.call('push', es.length, pts === null ? -1 : pts % 2 ** 33);
  }

  /** The next frame, or null if it needs more input. Call until null before the next push. */
  next(): AudioFrame | null {
    if (!this.call('decode')) return null;
    const len = this.call('pcm_len');
    const samples = new Float32Array(len);
    samples.set(new Float32Array(this.#x.memory.buffer, this.call('pcm'), len));
    const pts = this.call('pts');
    return {
      samples,
      channels: this.call('channels') === 1 ? 1 : 2,
      rate: this.call('sample_rate'),
      bitrate: this.call('bitrate'),
      pts: pts < 0 ? null : pts,
    };
  }

  /** End of stream: the last frame needn't be followed by another. */
  flush() {
    this.call('flush');
  }

  /** After lost packets: find the frame grid again. */
  reset() {
    this.call('reset');
  }

  /** Frames that failed a CRC (played as silence), and times sync was lost. */
  get errors(): number {
    return this.call('errors');
  }

  free() {
    this.call('free');
  }
}
