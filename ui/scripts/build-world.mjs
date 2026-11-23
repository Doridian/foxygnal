// Builds the Map instrument's offline base map (src/lib/map/world-50m.json)
// from world-atlas's Natural Earth 1:50m TopoJSON (public domain data,
// https://www.naturalearthdata.com/). Run once when updating the data:
//
//   npm pack world-atlas@2.0.2 && tar xzf world-atlas-2.0.2.tgz
//   node scripts/build-world.mjs package/countries-50m.json src/lib/map/world-50m.json
//
// The output keeps TopoJSON's shared, quantized, delta-encoded arcs but only
// what the map draws, flattened:
//   q:       [scaleX, scaleY, translateX, translateY] to turn arc integers into degrees
//   arcs:    one flat [x0, y0, dx1, dy1, …] array per arc
//   land:    rings of arc indices (~i = arc i reversed), filled even-odd
//   coast:   arcs of the land's outline, minus the cuts along ±180° and the South Pole
//   borders: arcs two countries share

import { readFileSync, writeFileSync } from 'node:fs';

const [src, dst] = process.argv.slice(2);
if (!src || !dst) {
  console.error('usage: node scripts/build-world.mjs <countries-50m.json> <out.json>');
  process.exit(2);
}
const topo = JSON.parse(readFileSync(src, 'utf8'));
const { scale, translate } = topo.transform;

/** Every ring (list of arc indices) of a geometry collection. */
function rings(obj) {
  const out = [];
  for (const g of obj.geometries) {
    if (g.type === 'Polygon') out.push(...g.arcs);
    else if (g.type === 'MultiPolygon') for (const p of g.arcs) out.push(...p);
  }
  return out;
}

const arcIndex = (i) => (i < 0 ? ~i : i);

/** Absolute coordinates (degrees) of an arc. */
function coords(arc) {
  let x = 0;
  let y = 0;
  return arc.map(([dx, dy]) => {
    x += dx;
    y += dy;
    return [x * scale[0] + translate[0], y * scale[1] + translate[1]];
  });
}

/** An arc that only runs along the antimeridian or the pole, where Natural Earth cuts polygons. */
function isCut(arc) {
  const pts = coords(arc);
  return pts.every(([lon]) => Math.abs(Math.abs(lon) - 180) < 1e-6) || pts.every(([, lat]) => lat < -89.9);
}

const landRings = rings(topo.objects.land);
const uses = new Map();
for (const r of rings(topo.objects.countries)) for (const i of r) uses.set(arcIndex(i), (uses.get(arcIndex(i)) ?? 0) + 1);

const landArcs = new Set(landRings.flat().map(arcIndex));
const coast = [...landArcs].filter((i) => !isCut(topo.arcs[i]));
const borders = [...uses].filter(([i, n]) => n >= 2 && !landArcs.has(i) && !isCut(topo.arcs[i])).map(([i]) => i);

// Keep only the arcs drawn, renumbered.
const keep = [...new Set([...landArcs, ...borders])].sort((a, b) => a - b);
const renum = new Map(keep.map((old, i) => [old, i]));
const re = (i) => (i < 0 ? ~renum.get(~i) : renum.get(i));

const out = {
  source: 'Natural Earth 1:50m (public domain), via world-atlas 2.0.2',
  q: [scale[0], scale[1], translate[0], translate[1]],
  arcs: keep.map((i) => topo.arcs[i].flat()),
  land: landRings.map((r) => r.map(re)),
  coast: coast.map(re).sort((a, b) => a - b),
  borders: borders.map(re).sort((a, b) => a - b),
};
writeFileSync(dst, JSON.stringify(out));
console.log(`${keep.length} arcs, ${out.land.length} land rings, ${out.coast.length} coast arcs, ${out.borders.length} border arcs`);
