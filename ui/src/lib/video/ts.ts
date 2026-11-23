// MPEG transport stream demuxer: PAT, PMT and SDT for the programme list,
// PES reassembly for the streams someone wants, and the counters a person
// wants to see (TEI packets, continuity errors, bitrate per PID).
//
// Plain TypeScript with no imports, so a node script can test it
// (ui/tests/). Input is whole 188-byte packets starting with 0x47, the TS
// wire convention (docs/ARCHITECTURE.md, "Video").

export const PACKET = 188;

export type StreamKind = 'video' | 'audio' | 'data';

export interface EsInfo {
  pid: number;
  /** stream_type from the PMT. */
  type: number;
  /** A short codec name: h264, hevc, mpeg2, mpeg1, aac, aac-latm, mp2, ac3, eac3, teletext, subtitles, … */
  codec: string;
  kind: StreamKind;
  /** ISO 639 language, if the PMT says. */
  lang: string | null;
}

export interface Program {
  number: number;
  pmtPid: number;
  pcrPid: number;
  /** From the SDT, when there is one. */
  name: string | null;
  provider: string | null;
  streams: EsInfo[];
}

export interface Pes {
  pid: number;
  /** 90 kHz, unwrapped (keeps counting past 2^33). */
  pts: number | null;
  dts: number | null;
  data: Uint8Array;
  /** The payload started with the adaptation field's random access indicator set. */
  random: boolean;
}

export interface PidStats {
  packets: number;
  /** Continuity counter jumps. */
  cc: number;
}

/** The 33-bit 90 kHz clock wraps every 26.5 hours; this keeps counting. */
export class Unwrapper {
  #last: number | null = null;
  #base = 0;

  unwrap(v: number): number {
    const WRAP = 2 ** 33;
    if (this.#last !== null) {
      const d = v - this.#last;
      if (d < -WRAP / 2) this.#base += WRAP;
      else if (d > WRAP / 2) this.#base -= WRAP;
    }
    this.#last = v;
    return v + this.#base;
  }

  reset() {
    this.#last = null;
    this.#base = 0;
  }
}

/** What a stream_type (plus descriptors, for private data) carries. */
export function describeStream(type: number, descriptors: Uint8Array): { codec: string; kind: StreamKind } {
  switch (type) {
    case 0x01:
      return { codec: 'mpeg1', kind: 'video' };
    case 0x02:
      return { codec: 'mpeg2', kind: 'video' };
    case 0x10:
      return { codec: 'mpeg4', kind: 'video' };
    case 0x1b:
      return { codec: 'h264', kind: 'video' };
    case 0x24:
      return { codec: 'hevc', kind: 'video' };
    case 0x33:
      return { codec: 'vvc', kind: 'video' };
    case 0x03:
    case 0x04:
      return { codec: 'mp2', kind: 'audio' };
    case 0x0f:
      return { codec: 'aac', kind: 'audio' };
    case 0x11:
      return { codec: 'aac-latm', kind: 'audio' };
    case 0x81:
      return { codec: 'ac3', kind: 'audio' };
    case 0x87:
      return { codec: 'eac3', kind: 'audio' };
  }
  // Private data: DVB says what it is with a descriptor.
  for (const [tag] of eachDescriptor(descriptors)) {
    if (tag === 0x6a) return { codec: 'ac3', kind: 'audio' };
    if (tag === 0x7a) return { codec: 'eac3', kind: 'audio' };
    if (tag === 0x7c) return { codec: 'aac', kind: 'audio' };
    if (tag === 0x56) return { codec: 'teletext', kind: 'data' };
    if (tag === 0x59) return { codec: 'subtitles', kind: 'data' };
  }
  return { codec: `0x${type.toString(16).padStart(2, '0')}`, kind: 'data' };
}

function* eachDescriptor(d: Uint8Array): Generator<[number, Uint8Array]> {
  let at = 0;
  while (at + 2 <= d.length) {
    const len = d[at + 1];
    if (at + 2 + len > d.length) return;
    yield [d[at], d.subarray(at + 2, at + 2 + len)];
    at += 2 + len;
  }
}

// CRC-32/MPEG-2 (poly 0x04C11DB7, no reflection, init all ones, no final xor).
const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    let c = i << 24;
    for (let k = 0; k < 8; k++) c = c & 0x80000000 ? (c << 1) ^ 0x04c11db7 : c << 1;
    t[i] = c >>> 0;
  }
  return t;
})();

export function crc32mpeg(b: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < b.length; i++) c = ((c << 8) ^ CRC_TABLE[((c >>> 24) ^ b[i]) & 0xff]) >>> 0;
  return c >>> 0;
}

/** DVB text (EN 300 468 annex A): the default table is close enough to Latin-1; 0x15 marks UTF-8. */
function dvbText(b: Uint8Array): string {
  if (!b.length) return '';
  let enc = 'latin1';
  let at = 0;
  if (b[0] === 0x15) {
    enc = 'utf-8';
    at = 1;
  } else if (b[0] >= 0x01 && b[0] <= 0x0b) {
    // ISO 8859-5…15: Latin-1 decoding keeps ASCII right, which is what names mostly are.
    at = 1;
  } else if (b[0] === 0x10) {
    at = 3;
  } else if (b[0] < 0x20) {
    at = 1;
  }
  // Drop DVB control codes (emphasis on/off, CR/LF 0x8A).
  const bytes = Array.from(b.subarray(at)).filter((c) => c >= 0x20 && !(c >= 0x80 && c <= 0x9f));
  try {
    return new TextDecoder(enc).decode(new Uint8Array(bytes)).trim();
  } catch {
    return String.fromCharCode(...bytes).trim();
  }
}

class Section {
  buf: number[] = [];
  /** Waiting for a payload_unit_start before taking bytes. */
  synced = false;
}

class PesBuilder {
  parts: Uint8Array[] = [];
  size = 0;
  /** PES_packet_length + 6, or 0 if unbounded. */
  expected = 0;
  random = false;
  /** Lost data: drop what we have at the next start. */
  broken = false;
  started = false;
}

export class Demuxer {
  programs = new Map<number, Program>();
  /** TS id and network names, when known (from the SDT). */
  transportId: number | null = null;
  pidStats = new Map<number, PidStats>();
  packets = 0;
  /** Packets with transport_error_indicator set (skipped). */
  tei = 0;
  /** Continuity errors over all PIDs. */
  ccErrors = 0;
  /** PES payloads are assembled only for these PIDs. */
  wanted = new Set<number>();
  /** The latest PCR of each PID (90 kHz units, unwrapped), and its packet index. */
  pcr = new Map<number, number>();

  onPes: (p: Pes) => void = () => {};
  /** The programme list changed. */
  onPrograms: () => void = () => {};
  /** A PCR arrived on `pid` (90 kHz, unwrapped; fractional). */
  onPcr: (pid: number, pcr: number) => void = () => {};

  #sections = new Map<number, Section>();
  #pes = new Map<number, PesBuilder>();
  #pmtPids = new Map<number, number>(); // pid → program number
  #versions = new Map<string, number>();
  #pts = new Map<number, Unwrapper>();
  #pcrUnwrap = new Map<number, Unwrapper>();
  #sdt = new Map<number, { name: string; provider: string }>();
  #cc = new Map<number, number>();

  /** Take packets: a whole number of 188-byte packets. */
  push(data: Uint8Array) {
    for (let at = 0; at + PACKET <= data.length; at += PACKET) this.#packet(data.subarray(at, at + PACKET));
  }

  /** Packets were lost before the next ones: drop partial PES and sections. */
  discontinuity() {
    for (const b of this.#pes.values()) b.broken = true;
    for (const s of this.#sections.values()) {
      s.synced = false;
      s.buf = [];
    }
    this.#cc.clear();
  }

  /** Forget everything (a new run). */
  reset() {
    this.programs.clear();
    this.pidStats.clear();
    this.packets = this.tei = this.ccErrors = 0;
    this.pcr.clear();
    this.#sections.clear();
    this.#pes.clear();
    this.#pmtPids.clear();
    this.#versions.clear();
    this.#pts.clear();
    this.#pcrUnwrap.clear();
    this.#sdt.clear();
    this.#cc.clear();
    this.transportId = null;
    this.onPrograms();
  }

  /** Emit the PES being assembled for `pid` now (end of stream). */
  flush(pid: number) {
    const b = this.#pes.get(pid);
    if (b) this.#emitPes(pid, b);
  }

  #packet(p: Uint8Array) {
    this.packets++;
    if (p[0] !== 0x47) return;
    const pid = ((p[1] & 0x1f) << 8) | p[2];
    if (p[1] & 0x80) {
      // transport_error_indicator: FEC couldn't fix it. Whatever it belonged to is damaged.
      this.tei++;
      const b = this.#pes.get(pid);
      if (b) b.broken = true;
      // Its counter is probably right: don't count the next packet as lost too.
      if (this.#cc.has(pid)) this.#cc.set(pid, p[3] & 15);
      return;
    }
    if (pid === 0x1fff) return;
    const pusi = (p[1] & 0x40) !== 0;
    const afc = (p[3] >> 4) & 3;
    const cc = p[3] & 15;
    let st = this.pidStats.get(pid);
    if (!st) this.pidStats.set(pid, (st = { packets: 0, cc: 0 }));
    st.packets++;

    let at = 4;
    let random = false;
    let discontinuity = false;
    if (afc & 2) {
      const len = p[4];
      if (len > 0 && 5 + len <= PACKET) {
        const flags = p[5];
        discontinuity = (flags & 0x80) !== 0;
        random = (flags & 0x40) !== 0;
        if (flags & 0x10 && len >= 7) {
          const base = p[6] * 2 ** 25 + p[7] * 2 ** 17 + p[8] * 2 ** 9 + p[9] * 2 + (p[10] >> 7);
          const ext = ((p[10] & 1) << 8) | p[11];
          let u = this.#pcrUnwrap.get(pid);
          if (!u) this.#pcrUnwrap.set(pid, (u = new Unwrapper()));
          if (discontinuity) u.reset();
          const v = u.unwrap(base) + ext / 300;
          this.pcr.set(pid, v);
          this.onPcr(pid, v);
        }
      }
      at = 5 + len;
    }
    const hasPayload = (afc & 1) !== 0;
    if (!hasPayload || at >= PACKET) return;

    // Continuity: the counter steps by one per packet with a payload; a
    // repeated counter is a legal duplicate.
    const isPsi = pid === 0 || pid === 0x11 || this.#pmtPids.has(pid);
    const holder = isPsi ? this.#section(pid) : this.wanted.has(pid) ? this.#builder(pid) : null;
    const prev = this.#cc.get(pid);
    this.#cc.set(pid, cc);
    if (prev !== undefined && !discontinuity) {
      if (cc === prev) return; // duplicate
      if (cc !== ((prev + 1) & 15)) {
        st.cc++;
        this.ccErrors++;
        if (holder instanceof PesBuilder) holder.broken = true;
        if (holder instanceof Section) {
          holder.synced = false;
          holder.buf = [];
        }
      }
    }

    const payload = p.subarray(at);
    if (holder instanceof Section) this.#psi(pid, holder, payload, pusi);
    else if (holder instanceof PesBuilder) this.#pesPayload(pid, holder, payload, pusi, random);
  }

  #section(pid: number): Section {
    let s = this.#sections.get(pid);
    if (!s) this.#sections.set(pid, (s = new Section()));
    return s;
  }

  #builder(pid: number): PesBuilder {
    let b = this.#pes.get(pid);
    if (!b) this.#pes.set(pid, (b = new PesBuilder()));
    return b;
  }

  #psi(pid: number, s: Section, payload: Uint8Array, pusi: boolean) {
    let data = payload;
    if (pusi) {
      const pointer = data[0];
      // The end of the previous section comes first.
      if (s.synced) {
        for (let i = 1; i <= pointer && i < data.length; i++) s.buf.push(data[i]);
        this.#sectionsIn(pid, s);
      }
      s.buf = [];
      s.synced = true;
      data = data.subarray(1 + pointer);
    } else if (!s.synced) {
      return;
    }
    for (let i = 0; i < data.length; i++) s.buf.push(data[i]);
    this.#sectionsIn(pid, s);
  }

  /** Parse every complete section in the buffer. */
  #sectionsIn(pid: number, s: Section) {
    while (s.buf.length >= 3) {
      if (s.buf[0] === 0xff) {
        s.buf = [];
        return; // stuffing
      }
      const len = ((s.buf[1] & 0x0f) << 8) | s.buf[2];
      if (s.buf.length < 3 + len) return;
      const sec = new Uint8Array(s.buf.slice(0, 3 + len));
      s.buf = s.buf.slice(3 + len);
      if (len >= 5 && crc32mpeg(sec) === 0) this.#table(pid, sec);
    }
  }

  #table(pid: number, sec: Uint8Array) {
    const tableId = sec[0];
    const ext = (sec[3] << 8) | sec[4];
    const version = (sec[5] >> 1) & 0x1f;
    const current = (sec[5] & 1) !== 0;
    if (!current) return;
    const key = `${pid}:${tableId}:${ext}:${sec[6]}`;
    if (this.#versions.get(key) === version) return;
    this.#versions.set(key, version);
    const body = sec.subarray(8, sec.length - 4);
    if (pid === 0 && tableId === 0x00) this.#pat(body);
    else if (tableId === 0x02 && this.#pmtPids.has(pid)) this.#pmt(ext, body);
    else if (pid === 0x11 && tableId === 0x42) this.#sdtTable(ext, body);
  }

  #pat(body: Uint8Array) {
    const seen = new Set<number>();
    for (let i = 0; i + 4 <= body.length; i += 4) {
      const number = (body[i] << 8) | body[i + 1];
      const pmtPid = ((body[i + 2] & 0x1f) << 8) | body[i + 3];
      if (number === 0) continue; // network PID
      seen.add(number);
      this.#pmtPids.set(pmtPid, number);
      const old = this.programs.get(number);
      if (!old || old.pmtPid !== pmtPid) {
        const sdt = this.#sdt.get(number);
        this.programs.set(number, { number, pmtPid, pcrPid: 0x1fff, name: sdt?.name ?? null, provider: sdt?.provider ?? null, streams: [] });
      }
    }
    for (const n of [...this.programs.keys()]) if (!seen.has(n)) this.programs.delete(n);
    this.onPrograms();
  }

  #pmt(number: number, body: Uint8Array) {
    const prog = this.programs.get(number);
    if (!prog || body.length < 4) return;
    prog.pcrPid = ((body[0] & 0x1f) << 8) | body[1];
    const infoLen = ((body[2] & 0x0f) << 8) | body[3];
    let at = 4 + infoLen;
    const streams: EsInfo[] = [];
    while (at + 5 <= body.length) {
      const type = body[at];
      const pid = ((body[at + 1] & 0x1f) << 8) | body[at + 2];
      const len = ((body[at + 3] & 0x0f) << 8) | body[at + 4];
      const desc = body.subarray(at + 5, Math.min(body.length, at + 5 + len));
      let lang: string | null = null;
      for (const [tag, d] of eachDescriptor(desc)) {
        if (tag === 0x0a && d.length >= 3) lang = String.fromCharCode(d[0], d[1], d[2]);
      }
      streams.push({ pid, type, lang, ...describeStream(type, desc) });
      at += 5 + len;
    }
    prog.streams = streams;
    this.onPrograms();
  }

  #sdtTable(tsId: number, body: Uint8Array) {
    // original_network_id (2), reserved (1), then services.
    this.transportId = tsId;
    let at = 3;
    let changed = false;
    while (at + 5 <= body.length) {
      const service = (body[at] << 8) | body[at + 1];
      const len = ((body[at + 3] & 0x0f) << 8) | body[at + 4];
      const desc = body.subarray(at + 5, Math.min(body.length, at + 5 + len));
      for (const [tag, d] of eachDescriptor(desc)) {
        if (tag !== 0x48 || d.length < 2) continue;
        const plen = d[1];
        const provider = dvbText(d.subarray(2, 2 + plen));
        const nlen = d[2 + plen] ?? 0;
        const name = dvbText(d.subarray(3 + plen, 3 + plen + nlen));
        this.#sdt.set(service, { name, provider });
        const prog = this.programs.get(service);
        if (prog && (prog.name !== name || prog.provider !== provider)) {
          prog.name = name;
          prog.provider = provider;
          changed = true;
        }
      }
      at += 5 + len;
    }
    if (changed) this.onPrograms();
  }

  #pesPayload(pid: number, b: PesBuilder, payload: Uint8Array, pusi: boolean, random: boolean) {
    if (pusi) {
      if (b.started) this.#emitPes(pid, b);
      b.parts = [];
      b.size = 0;
      b.broken = false;
      b.started = true;
      b.random = random;
      b.expected = payload.length >= 6 ? ((payload[4] << 8) | payload[5]) : 0;
      if (b.expected) b.expected += 6;
    } else if (!b.started || b.broken) {
      return;
    }
    b.parts.push(payload.slice());
    b.size += payload.length;
    if (b.expected && b.size >= b.expected) {
      this.#emitPes(pid, b);
      b.started = false;
    }
  }

  #emitPes(pid: number, b: PesBuilder) {
    const parts = b.parts;
    const size = b.size;
    b.parts = [];
    b.size = 0;
    if (b.broken || !b.started || size < 9) {
      b.started = false;
      return;
    }
    b.started = false;
    const all = new Uint8Array(size);
    let o = 0;
    for (const p of parts) {
      all.set(p, o);
      o += p.length;
    }
    if (all[0] !== 0 || all[1] !== 0 || all[2] !== 1) return;
    const hdrLen = all[8];
    const flags = all[7];
    let pts: number | null = null;
    let dts: number | null = null;
    let u = this.#pts.get(pid);
    if (!u) this.#pts.set(pid, (u = new Unwrapper()));
    if (flags & 0x80 && hdrLen >= 5) pts = u.unwrap(readTimestamp(all, 9));
    if ((flags & 0xc0) === 0xc0 && hdrLen >= 10) dts = u.unwrap(readTimestamp(all, 14));
    const end = b.expected ? Math.min(b.expected, size) : size;
    const start = 9 + hdrLen;
    if (start > end) return;
    this.onPes({ pid, pts, dts, data: all.subarray(start, end), random: b.random });
  }
}

function readTimestamp(b: Uint8Array, at: number): number {
  return (
    ((b[at] >> 1) & 7) * 2 ** 30 + ((b[at + 1] << 7) | (b[at + 2] >> 1)) * 2 ** 15 + ((b[at + 3] << 7) | (b[at + 4] >> 1))
  );
}

/** The first program with a video stream, else the first one. */
export function defaultProgram(programs: Iterable<Program>): Program | null {
  const list = [...programs].sort((a, b) => a.number - b.number);
  return list.find((p) => p.streams.some((s) => s.kind === 'video')) ?? list[0] ?? null;
}
