// The AC-3 (Dolby Digital) decoder: crates/foxygnal-ac3 compiled to
// WebAssembly (scripts/build-mpeg2-wasm.sh). AC-3 is the sound of ATSC
// television and of many DVB services, and WebCodecs can't decode it. It
// plays mono as mono and everything else as stereo (5.1 downmixed, the LFE
// left out). E-AC-3 frames are recognised and skipped. See wasmaudio.ts.

import { loadModule, WasmAudioDecoder, type AudioFrame } from './wasmaudio.ts';

export type Ac3Frame = AudioFrame;

/** What the stream carries: AC-3 or E-AC-3, and its channels. */
export interface Ac3Kind {
  eac3: boolean;
  /** Channels the usual way (2.0, 5.1), or 1+1 for two mono programmes. */
  layout: string;
}

/** Full-bandwidth channels by acmod. */
const CHANNELS = [2, 1, 2, 3, 3, 4, 4, 5];

export class Ac3Decoder extends WasmAudioDecoder {
  static async create(url: string): Promise<Ac3Decoder> {
    return Ac3Decoder.fromModule(await loadModule(url));
  }

  static async fromModule(m: WebAssembly.Module): Promise<Ac3Decoder> {
    return new Ac3Decoder(await WebAssembly.instantiate(m, {}), 'ac3a_');
  }

  /** The last frame header seen (decoded or not), null if none yet. */
  get kind(): Ac3Kind | null {
    const k = this.call('kind');
    if (!k) return null;
    const acmod = (k >> 1) & 7;
    const lfe = k & 1;
    const layout = acmod === 0 ? `1+1${lfe ? '.1' : ''}` : `${CHANNELS[acmod]}.${lfe}`;
    return { eac3: k >> 4 === 2, layout };
  }
}
