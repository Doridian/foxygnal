// Web Mercator, in "world units": the whole world is the square [0, 1]²,
// x east from 180° W, y south from about 85.05° N (the square's edge). At
// zoom z the world is 256·2^z CSS pixels wide, as with map tiles.

export const MAX_LAT = 85.0511287798;
export const TILE = 256;
const EARTH_RADIUS_M = 6371008.8;

export function lonToX(lon: number): number {
  return (lon + 180) / 360;
}

export function latToY(lat: number): number {
  const phi = (Math.max(-MAX_LAT, Math.min(MAX_LAT, lat)) * Math.PI) / 180;
  return (1 - Math.log(Math.tan(Math.PI / 4 + phi / 2)) / Math.PI) / 2;
}

export function xToLon(x: number): number {
  return x * 360 - 180;
}

export function yToLat(y: number): number {
  return (Math.atan(Math.sinh(Math.PI * (1 - 2 * y))) * 180) / Math.PI;
}

/** Where `lon`/`lat` is reached going `distance` metres from `lat0`/`lon0` on `bearing` (degrees). */
export function destination(lat0: number, lon0: number, bearing: number, distance: number): { lat: number; lon: number } {
  const d = distance / EARTH_RADIUS_M;
  const b = (bearing * Math.PI) / 180;
  const p1 = (lat0 * Math.PI) / 180;
  const l1 = (lon0 * Math.PI) / 180;
  const p2 = Math.asin(Math.sin(p1) * Math.cos(d) + Math.cos(p1) * Math.sin(d) * Math.cos(b));
  const l2 = l1 + Math.atan2(Math.sin(b) * Math.sin(d) * Math.cos(p1), Math.cos(d) - Math.sin(p1) * Math.sin(p2));
  return { lat: (p2 * 180) / Math.PI, lon: ((((l2 * 180) / Math.PI + 540) % 360) - 180) };
}

/** Great-circle distance in metres. */
export function distance(lat1: number, lon1: number, lat2: number, lon2: number): number {
  const r = Math.PI / 180;
  const dp = (lat2 - lat1) * r;
  const dl = (lon2 - lon1) * r;
  const a = Math.sin(dp / 2) ** 2 + Math.cos(lat1 * r) * Math.cos(lat2 * r) * Math.sin(dl / 2) ** 2;
  return 2 * EARTH_RADIUS_M * Math.asin(Math.min(1, Math.sqrt(a)));
}

/** The 1-2-5 step at or below `x`. */
export function niceStep(x: number): number {
  if (!(x > 0)) return 1;
  const p = 10 ** Math.floor(Math.log10(x));
  const m = x / p;
  return (m >= 5 ? 5 : m >= 2 ? 2 : 1) * p;
}

/**
 * A tile URL for a template with `{z}`, `{x}`, `{y}` (and optionally `{s}`
 * for a subdomain), or null if the template isn't a usable http(s) one.
 */
export function tileUrl(template: string, z: number, x: number, y: number): string | null {
  const t = template.trim();
  if (!/^https?:\/\//i.test(t) || !t.includes('{z}') || !t.includes('{x}') || !t.includes('{y}')) return null;
  const s = 'abc'[(x + y) % 3];
  return t.replaceAll('{z}', String(z)).replaceAll('{x}', String(x)).replaceAll('{y}', String(y)).replaceAll('{s}', s);
}
