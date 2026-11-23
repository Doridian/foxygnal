// The MPEG audio Layer I/II (MP1/MP2) decoder: crates/foxygnal-mp2 compiled
// to WebAssembly (scripts/build-mpeg2-wasm.sh). MP2 is the sound of most DVB
// services, and WebCodecs can't decode it. See wasmaudio.ts.

import { loadModule, WasmAudioDecoder, type AudioFrame } from './wasmaudio.ts';

export type Mp2Frame = AudioFrame;

export class Mp2Decoder extends WasmAudioDecoder {
  static async create(url: string): Promise<Mp2Decoder> {
    return Mp2Decoder.fromModule(await loadModule(url));
  }

  static async fromModule(m: WebAssembly.Module): Promise<Mp2Decoder> {
    return new Mp2Decoder(await WebAssembly.instantiate(m, {}), 'mp2a_');
  }

  /** The layer of the last frame header seen (3 is never decoded), 0 if none yet. */
  get layer(): number {
    return this.call('layer');
  }
}
