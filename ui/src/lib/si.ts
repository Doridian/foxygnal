// Human numbers: inputs accept `2.4M`, `100k`, `-30dB`, `1.5 ms`; outputs use SI prefixes.

const PREFIX: Record<string, number> = { p: 1e-12, n: 1e-9, u: 1e-6, µ: 1e-6, m: 1e-3, k: 1e3, K: 1e3, M: 1e6, G: 1e9 };

/** Units shown with SI prefixes. Others (dB, plain factors) stay plain. */
const SCALED = new Set(['Hz', 'S/s', 's', 'B']);

const NUM = /^([+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?)\s*(.*)$/i;

/** Parse a number with an optional SI prefix and unit. `null` if it isn't one. */
export function parseSi(text: string, unit?: string): number | null {
  const m = NUM.exec(text.trim().replaceAll('_', '').replaceAll(',', ''));
  if (!m) return null;
  let v = Number(m[1]);
  let rest = m[2].trim();
  if (unit && rest.endsWith(unit)) rest = rest.slice(0, -unit.length).trim();
  if (rest === '') return v;
  // A lone prefix (`k`), or a prefix followed by any unit word (`kHz` for an unknown unit).
  const p = PREFIX[rest[0]];
  if (p !== undefined && (rest.length === 1 || /^[a-zA-Z/]+$/.test(rest.slice(1)))) return v * p;
  // A bare unit we don't scale, e.g. `-30 dB`.
  if (/^[a-zA-Z/]+$/.test(rest)) return v;
  return null;
}

function trim(n: number, digits = 6): string {
  return String(Number(n.toPrecision(digits)));
}

/** Format for an input field: SI prefix for scaled units, else plain. */
export function formatSi(v: number, unit?: string): string {
  if (!Number.isFinite(v)) return String(v);
  if (!unit || !SCALED.has(unit) || v === 0) return trim(v);
  const a = Math.abs(v);
  for (const [p, f] of [['G', 1e9], ['M', 1e6], ['k', 1e3], ['', 1], ['m', 1e-3], ['µ', 1e-6], ['n', 1e-9]] as const) {
    if (a >= f) return trim(v / f) + p;
  }
  return trim(v);
}

/** Format with the unit for display, e.g. `48 kS/s`. */
export function formatWithUnit(v: number, unit?: string): string {
  const s = formatSi(v, unit);
  if (!unit) return s;
  const prefixed = /[a-zA-Zµ]$/.test(s);
  return prefixed ? `${s.slice(0, -1)} ${s.slice(-1)}${unit}` : `${s} ${unit}`;
}
