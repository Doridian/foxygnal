// Tracks for the Map instrument: decoded text lines (`key=value` fields, see
// "Decoded messages" in docs/ARCHITECTURE.md) grouped by whatever identifies
// the thing that sent them, with the latest position and a trail.
//
// Nothing here knows a particular decoder; it goes by field names:
// - id:       icao (ADS-B), mmsi (AIS), name of an APRS object or item,
//             src (APRS / AX.25), serial (radiosondes), id, callsign
// - position: lat/lon (also latitude/longitude, lng)
// - heading:  track_deg, track, cog, course, heading_deg, heading, hdg
// - label:    flight, name, callsign, src, serial, then the id
// Lines with an id but no position (an ADS-B identity, AIS static data) merge
// their fields into the track.

export type TrackKind = 'aircraft' | 'ship' | 'station' | 'object' | 'other';

export interface TrackPoint {
  lat: number;
  lon: number;
  /** Arrival time (ms, the caller's clock). */
  at: number;
}

export interface Track {
  /** Unique: the id field's name and value, e.g. `icao:4840D6`. */
  key: string;
  kind: TrackKind;
  /** Every field seen for this track, latest value wins. */
  fields: Record<string, string>;
  /** Latest position; null until one arrives. */
  pos: TrackPoint | null;
  /** Earlier positions, oldest first, ending with `pos`. */
  trail: TrackPoint[];
  /** Last line received (ms). */
  seen: number;
  messages: number;
}

/** Split a line into its `key=value` fields; quoted values are unescaped. Words without `=` are skipped. */
export function parseFields(line: string): Record<string, string> {
  const out: Record<string, string> = {};
  const n = line.length;
  let i = 0;
  while (i < n) {
    while (i < n && line[i] === ' ') i++;
    const start = i;
    while (i < n && line[i] !== ' ' && line[i] !== '=') i++;
    const key = line.slice(start, i);
    if (i >= n || line[i] !== '=') {
      // A bare word: skip it (and anything up to the next space).
      while (i < n && line[i] !== ' ') i++;
      continue;
    }
    i++; // '='
    let value = '';
    if (line[i] === '"') {
      i++;
      while (i < n && line[i] !== '"') {
        const c = line[i++];
        if (c !== '\\' || i >= n) {
          value += c;
          continue;
        }
        const e = line[i++];
        if (e === 'n') value += '\n';
        else if (e === 'r') value += '\r';
        else if (e === 't') value += '\t';
        else if (e === 'x' && /^[0-9a-fA-F]{2}$/.test(line.slice(i, i + 2))) {
          value += String.fromCharCode(parseInt(line.slice(i, i + 2), 16));
          i += 2;
        } else if (e === 'u' && line[i] === '{') {
          const end = line.indexOf('}', i);
          const cp = end > i ? parseInt(line.slice(i + 1, end), 16) : NaN;
          if (Number.isFinite(cp) && cp <= 0x10ffff) {
            value += String.fromCodePoint(cp);
            i = end + 1;
          } else value += e;
        } else value += e; // \" and \\
      }
      i++; // closing quote
    } else {
      const vs = i;
      while (i < n && line[i] !== ' ') i++;
      value = line.slice(vs, i);
    }
    if (key) out[key] = value;
  }
  return out;
}

function num(v: string | undefined): number | null {
  if (v === undefined || v.trim() === '') return null;
  const x = Number(v);
  return Number.isFinite(x) ? x : null;
}

/** The position in a line's fields, if it has a valid one. */
export function position(f: Record<string, string>): { lat: number; lon: number } | null {
  const lat = num(f.lat ?? f.latitude);
  const lon = num(f.lon ?? f.lng ?? f.longitude);
  if (lat === null || lon === null || Math.abs(lat) > 90 || Math.abs(lon) > 180) return null;
  return { lat, lon };
}

/** What a line is about: its track key and kind, or null if it doesn't say. */
export function identify(f: Record<string, string>): { key: string; kind: TrackKind } | null {
  if (f.icao) return { key: `icao:${f.icao.toUpperCase()}`, kind: 'aircraft' };
  if (f.mmsi) return { key: `mmsi:${f.mmsi}`, kind: 'ship' };
  if ((f.type === 'object' || f.type === 'item') && f.name) return { key: `object:${f.name}`, kind: 'object' };
  if (f.src) return { key: `src:${f.src}`, kind: 'station' };
  for (const k of ['serial', 'id', 'callsign']) if (f[k]) return { key: `${k}:${f[k]}`, kind: 'other' };
  return null;
}

const HEADING_KEYS = ['track_deg', 'track', 'cog', 'course', 'heading_deg', 'heading', 'hdg'];

/** Direction of travel (degrees true) from a track's fields, if known. */
export function heading(f: Record<string, string>): number | null {
  for (const k of HEADING_KEYS) {
    const h = num(f[k]);
    if (h !== null && h >= 0 && h <= 360) return h % 360;
  }
  return null;
}

/** A short name to draw next to the track. */
export function label(t: Track): string {
  for (const k of ['flight', 'name', 'callsign', 'src', 'serial']) {
    const v = t.fields[k]?.trim();
    if (v) return v;
  }
  return t.key.slice(t.key.indexOf(':') + 1);
}

/** Tracks without an id field share this one. */
export const ANONYMOUS = 'anonymous:';

export class TrackStore {
  tracks = new Map<string, Track>();
  /** Points kept per trail (including the latest position). */
  trailLength: number;
  /** Most tracks kept; the longest-silent go first. */
  maxTracks: number;

  constructor(opts: { trailLength?: number; maxTracks?: number } = {}) {
    this.trailLength = opts.trailLength ?? 100;
    this.maxTracks = opts.maxTracks ?? 5000;
  }

  /** Take a chunk of text (whole lines, `\n`-separated). Returns whether any track changed. */
  addText(text: string, now: number): boolean {
    let changed = false;
    for (const line of text.split('\n')) if (line && this.addLine(line, now)) changed = true;
    return changed;
  }

  /** Take one line; returns the track it updated, if any. */
  addLine(line: string, now: number): Track | null {
    const f = parseFields(line);
    const pos = position(f);
    const id = identify(f) ?? (pos ? { key: ANONYMOUS, kind: 'other' as const } : null);
    if (!id) return null;
    let t = this.tracks.get(id.key);
    if (!t) {
      t = { key: id.key, kind: id.kind, fields: {}, pos: null, trail: [], seen: now, messages: 0 };
      this.tracks.set(id.key, t);
      if (this.tracks.size > this.maxTracks) this.#evict();
    }
    Object.assign(t.fields, f);
    t.seen = now;
    t.messages++;
    if (pos) {
      const p = { lat: pos.lat, lon: pos.lon, at: now };
      const last = t.trail[t.trail.length - 1];
      if (last && last.lat === p.lat && last.lon === p.lon) last.at = now;
      else t.trail.push(p);
      const keep = Math.max(1, this.trailLength);
      if (t.trail.length > keep) t.trail.splice(0, t.trail.length - keep);
      t.pos = t.trail[t.trail.length - 1];
    }
    return t;
  }

  #evict() {
    let oldest: Track | null = null;
    for (const t of this.tracks.values()) if (!oldest || t.seen < oldest.seen) oldest = t;
    if (oldest) this.tracks.delete(oldest.key);
  }

  /** Drop tracks silent for longer than `maxAge` ms. Returns whether any went. */
  expire(now: number, maxAge: number): boolean {
    let gone = false;
    for (const [k, t] of this.tracks) {
      if (now - t.seen > maxAge) {
        this.tracks.delete(k);
        gone = true;
      }
    }
    return gone;
  }

  /** Shorten every trail to `n` points. */
  setTrailLength(n: number) {
    this.trailLength = n;
    const keep = Math.max(1, n);
    for (const t of this.tracks.values()) if (t.trail.length > keep) t.trail.splice(0, t.trail.length - keep);
  }

  /** Tracks with a position. */
  *positioned(): Iterable<Track & { pos: TrackPoint }> {
    for (const t of this.tracks.values()) if (t.pos) yield t as Track & { pos: TrackPoint };
  }

  clear() {
    this.tracks.clear();
  }
}
