// Small helpers shared by the instrument views.

import { formatWithUnit } from './si';

/** 1-2-5 steps from 1e-9 to 1e6. */
export const STEPS_125: number[] = (() => {
  const out: number[] = [];
  for (let e = -9; e <= 6; e++) for (const m of [1, 2, 5]) out.push(Number((m * 10 ** e).toPrecision(3)));
  return out;
})();

/** The 1-2-5 step closest to `v` (in log terms). */
export function nearestStep(v: number, steps = STEPS_125): number {
  let best = steps[0];
  for (const s of steps) if (Math.abs(Math.log(s / v)) < Math.abs(Math.log(best / v))) best = s;
  return best;
}

/** The next 1-2-5 step up (`dir` 1) or down (-1). */
export function stepBy(v: number, dir: 1 | -1, steps = STEPS_125): number {
  const i = steps.indexOf(nearestStep(v, steps));
  return steps[Math.max(0, Math.min(steps.length - 1, i + dir))];
}

export const fmtTime = (s: number) => formatWithUnit(Number(s.toPrecision(4)), 's');
export const fmtHz = (hz: number) => formatWithUnit(Number(hz.toPrecision(4)), 'Hz');
export const fmtRate = (r: number) => formatWithUnit(Number(r.toPrecision(3)), 'S/s');

/** A plain number with up to 4 significant digits. */
export const fmtNum = (v: number) => (Math.abs(v) < 1e-4 && v !== 0 ? v.toExponential(2) : String(Number(v.toPrecision(4))));

export const fmtDb = (db: number) => `${db >= 0 ? '+' : '−'}${Math.abs(db).toFixed(1)} dB`;

/** Green → yellow → red for 0 → 1 (buffer fill, CPU load). */
export function heat(x: number): string {
  const t = Math.max(0, Math.min(1, x));
  const stops: [number, number, number][] = [
    [63, 185, 80],
    [227, 179, 65],
    [255, 123, 114],
  ];
  const f = t * 2;
  const [a, b] = f < 1 ? [stops[0], stops[1]] : [stops[1], stops[2]];
  const k = f < 1 ? f : f - 1;
  const c = a.map((v, i) => Math.round(v + (b[i] - v) * k));
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

/** Read a CSS custom property (colours for canvas drawing). */
export function cssVar(el: Element, name: string): string {
  return getComputedStyle(el).getPropertyValue(name).trim();
}

/**
 * Keep a canvas's backing store at its CSS size × devicePixelRatio. Calls
 * `onresize(width, height)` in device pixels. Returns a cleanup function.
 */
export function fitCanvas(canvas: HTMLCanvasElement, onresize: (w: number, h: number) => void): () => void {
  const ro = new ResizeObserver(() => {
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
    const h = Math.max(1, Math.round(canvas.clientHeight * dpr));
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
    onresize(w, h);
  });
  ro.observe(canvas);
  return () => ro.disconnect();
}
