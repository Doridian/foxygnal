// Just enough H.264 / HEVC (Annex B) and AAC (ADTS) parsing to configure
// WebCodecs: the codec string, whether an access unit can start decoding,
// and the audio format. No imports, so node tests can load it.

/** The NAL units of an Annex B access unit (start codes removed). */
export function nalUnits(b: Uint8Array): Uint8Array[] {
  const out: Uint8Array[] = [];
  let start = -1;
  let i = 0;
  while (i + 3 <= b.length) {
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1) {
      if (start >= 0) {
        let end = i;
        while (end > start && b[end - 1] === 0) end--; // trailing zero / 4-byte start code
        out.push(b.subarray(start, end));
      }
      i += 3;
      start = i;
    } else {
      i++;
    }
  }
  if (start >= 0 && start < b.length) out.push(b.subarray(start));
  return out;
}

/** Remove emulation prevention bytes (00 00 03 → 00 00). */
export function rbsp(nal: Uint8Array): Uint8Array {
  const out = new Uint8Array(nal.length);
  let n = 0;
  let zeros = 0;
  for (let i = 0; i < nal.length; i++) {
    const v = nal[i];
    if (zeros >= 2 && v === 3) {
      zeros = 0;
      continue;
    }
    zeros = v === 0 ? zeros + 1 : 0;
    out[n++] = v;
  }
  return out.subarray(0, n);
}

export class Bits {
  #b: Uint8Array;
  pos = 0;
  constructor(b: Uint8Array) {
    this.#b = b;
  }
  bit(): number {
    const v = (this.#b[this.pos >> 3] ?? 0) >> (7 - (this.pos & 7));
    this.pos++;
    return v & 1;
  }
  bits(n: number): number {
    let v = 0;
    for (let i = 0; i < n; i++) v = v * 2 + this.bit();
    return v;
  }
  ue(): number {
    let zeros = 0;
    while (this.bit() === 0 && zeros < 32) zeros++;
    return 2 ** zeros - 1 + this.bits(zeros);
  }
}

const hex2 = (v: number) => v.toString(16).padStart(2, '0');

export interface AvcInfo {
  /** `avc1.PPCCLL`, from the SPS. */
  codec: string | null;
  /** The AU holds an IDR picture, or an I slice with SPS and PPS (a recovery point). */
  key: boolean;
}

export function avcAccessUnit(au: Uint8Array): AvcInfo {
  let codec: string | null = null;
  let idr = false;
  let iSlice = false;
  let pps = false;
  for (const nal of nalUnits(au)) {
    const type = nal[0] & 0x1f;
    if (type === 7 && nal.length >= 4) codec = `avc1.${hex2(nal[1])}${hex2(nal[2])}${hex2(nal[3])}`;
    else if (type === 8) pps = true;
    else if (type === 5) idr = true;
    else if (type === 1 && !iSlice) {
      const r = new Bits(rbsp(nal.subarray(1, 16)));
      r.ue(); // first_mb_in_slice
      const sliceType = r.ue() % 5;
      if (sliceType === 2 || sliceType === 4) iSlice = true;
    }
  }
  return { codec, key: idr || (iSlice && codec !== null && pps) };
}

export interface HevcInfo {
  /** `hvc1.…` (ISO/IEC 14496-15 annex E), from the SPS. */
  codec: string | null;
  /** The AU holds an IRAP picture (IDR, CRA, BLA). */
  key: boolean;
}

export function hevcAccessUnit(au: Uint8Array): HevcInfo {
  let codec: string | null = null;
  let key = false;
  for (const nal of nalUnits(au)) {
    const type = (nal[0] >> 1) & 0x3f;
    if (type >= 16 && type <= 21) key = true;
    if (type === 33 && nal.length > 15) codec = hevcCodec(rbsp(nal.subarray(2)));
  }
  return { codec, key };
}

function hevcCodec(sps: Uint8Array): string {
  const r = new Bits(sps);
  r.bits(4); // sps_video_parameter_set_id
  r.bits(3); // max_sub_layers_minus1
  r.bits(1);
  const space = r.bits(2);
  const tier = r.bits(1);
  const profile = r.bits(5);
  let compat = 0;
  for (let i = 0; i < 32; i++) compat |= r.bit() << i; // reversed bit order
  const constraints: number[] = [];
  for (let i = 0; i < 6; i++) constraints.push(r.bits(8));
  const level = r.bits(8);
  while (constraints.length && constraints[constraints.length - 1] === 0) constraints.pop();
  const parts = [
    'hvc1',
    `${space ? String.fromCharCode(64 + space) : ''}${profile}`,
    (compat >>> 0).toString(16).toUpperCase(),
    `${tier ? 'H' : 'L'}${level}`,
    ...constraints.map((c) => c.toString(16).toUpperCase()),
  ];
  return parts.join('.');
}

export interface AdtsFrame {
  /** Audio object type (2 = AAC LC). */
  object: number;
  rate: number;
  channels: number;
  /** The whole frame, header included (WebCodecs takes ADTS as it is). */
  data: Uint8Array;
}

const ADTS_RATES = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/** Split a PES payload into ADTS frames. */
export function adtsFrames(b: Uint8Array): AdtsFrame[] {
  const out: AdtsFrame[] = [];
  let at = 0;
  while (at + 7 <= b.length) {
    if (b[at] !== 0xff || (b[at + 1] & 0xf6) !== 0xf0) {
      at++;
      continue;
    }
    const object = (b[at + 2] >> 6) + 1;
    const rate = ADTS_RATES[(b[at + 2] >> 2) & 15] ?? 0;
    const channels = ((b[at + 2] & 1) << 2) | (b[at + 3] >> 6);
    const len = ((b[at + 3] & 3) << 11) | (b[at + 4] << 3) | (b[at + 5] >> 5);
    if (len < 7 || at + len > b.length || !rate) break;
    out.push({ object, rate, channels: channels || 2, data: b.subarray(at, at + len) });
    at += len;
  }
  return out;
}

/** MPEG-2 video: does this PES hold a sequence header (where decoding can start)? */
export function mpeg2HasSequenceHeader(b: Uint8Array): boolean {
  for (let i = 0; i + 3 < b.length; i++) {
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1 && b[i + 3] === 0xb3) return true;
  }
  return false;
}
