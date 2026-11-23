// The offline base map: Natural Earth 1:50m land, coastlines and borders
// (world-50m.json, built by scripts/build-world.mjs), loaded the first time a
// Map instrument opens and turned into paths in world units (see mercator.ts),
// so drawing is one transform and three path operations however far you pan.

import { latToY, lonToX } from './mercator';
// A plain asset, fetched (and parsed as JSON) only when needed.
import worldUrl from './world-50m.json?url';

interface WorldData {
  q: [number, number, number, number];
  arcs: number[][];
  land: number[][];
  coast: number[];
  borders: number[];
}

export interface World {
  land: Path2D;
  coast: Path2D;
  borders: Path2D;
}

let loading: Promise<World> | null = null;

export function loadWorld(): Promise<World> {
  if (!loading) {
    loading = fetch(worldUrl)
      .then((r) => {
        if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
        return r.json() as Promise<WorldData>;
      })
      .then(build);
    // A failed load can be tried again by the next Map.
    loading.catch(() => (loading = null));
  }
  return loading;
}

function build(d: WorldData): World {
  const [sx, sy, tx, ty] = d.q;
  // Each arc's points in world units, as [x0, y0, x1, y1, …].
  const arcs = d.arcs.map((a) => {
    const out = new Float64Array(a.length);
    let x = 0;
    let y = 0;
    for (let i = 0; i < a.length; i += 2) {
      x += a[i];
      y += a[i + 1];
      out[i] = lonToX(x * sx + tx);
      out[i + 1] = latToY(y * sy + ty);
    }
    return out;
  });

  // Natural Earth cuts most land at ±180°, but a few rings and arcs (Chukotka,
  // Fiji) jump across it. Rings are unwrapped to stay continuous (the world is
  // drawn side by side, so the part past the edge shows on the next copy);
  // lines break at the jump. Antarctica, which closes along the South Pole, is
  // drawn as it is.
  const land = new Path2D();
  const pts: number[] = [];
  for (const ring of d.land) {
    pts.length = 0;
    let polar = false;
    for (const [n, i] of ring.entries()) {
      const a = arcs[i < 0 ? ~i : i];
      const len = a.length / 2;
      // Arcs join end to start: skip each one's first point after the first arc.
      for (let k = n === 0 ? 0 : 1; k < len; k++) {
        const j = i < 0 ? len - 1 - k : k;
        pts.push(a[2 * j], a[2 * j + 1]);
        if (a[2 * j + 1] >= 1) polar = true;
      }
    }
    let shift = 0;
    for (let k = 0; k < pts.length; k += 2) {
      if (k > 0 && !polar) {
        const dx = pts[k] + shift - pts[k - 2];
        if (dx > 0.5) shift -= 1;
        else if (dx < -0.5) shift += 1;
      }
      pts[k] += shift;
      if (k === 0) land.moveTo(pts[k], pts[k + 1]);
      else land.lineTo(pts[k], pts[k + 1]);
    }
    land.closePath();
  }

  const lines = (list: number[]) => {
    const p = new Path2D();
    for (const i of list) {
      const a = arcs[i];
      p.moveTo(a[0], a[1]);
      for (let k = 2; k < a.length; k += 2) {
        if (Math.abs(a[k] - a[k - 2]) > 0.5) p.moveTo(a[k], a[k + 1]);
        else p.lineTo(a[k], a[k + 1]);
      }
    }
    return p;
  };

  return { land, coast: lines(d.coast), borders: lines(d.borders) };
}
