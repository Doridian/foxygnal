// The Map instrument's line parsing and track merging (src/lib/map), with
// lines as the decoders write them (see their tests in foxygnal-blocks).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ANONYMOUS, heading, identify, label, parseFields, position, TrackStore } from '../src/lib/map/tracks.ts';
import { destination, distance, latToY, lonToX, niceStep, tileUrl, xToLon, yToLat } from '../src/lib/map/mercator.ts';

const ADSB_IDENT = 't=1.000000 icao=4840d6 df=17 tc=4 type=ident callsign=KLM1023 raw=8D4840D6202CC371C32CE0576098';
const ADSB_POS = 't=2.000000 icao=4840D6 df=17 tc=11 type=position alt_ft=38000 lat=52.25720 lon=3.91937';
const ADSB_VEL = 't=3.000000 icao=4840D6 df=17 tc=19 type=velocity gs_kt=159.2 track_deg=182.9 vrate_fpm=-832';
const AIS_POS = 't=12.345 ch=A mmsi=366053209 type=1 status=restricted lat=37.802118 lon=-122.341618 sog=0.0 cog=219.3 heading=1';
const AIS_STATIC = 't=13.000 ch=B mmsi=366053209 type=5 name="CAPT. \\"JACK\\"" callsign=WDE5199 shiptype=31 length=20 beam=7';
const APRS_POS = 'src=N0CALL-9 dst=APRS path=WIDE1-1,WIDE2-1 type=position lat=49.058333 lon=-72.029167 sym=/> course=88 speed_kt=36.0 comment="Hi there"';
const APRS_OBJ = 'src=N0CALL-9 dst=APRS type=object name="LEADER" state=live time=092345z lat=49.058333 lon=-72.029167 sym=/> course=88 speed_kt=36.0';

test('fields: plain, quoted and escaped values', () => {
  assert.deepEqual(parseFields('a=1 b=two  c="x y" d="q\\"\\\\\\n\\t" e=""'), { a: '1', b: 'two', c: 'x y', d: 'q"\\\n\t', e: '' });
  assert.deepEqual(parseFields('word k=v other'), { k: 'v' });
  assert.deepEqual(parseFields('s="\\x41\\u{1f98a}"'), { s: 'A🦊' });
  // An unterminated quote takes the rest of the line.
  assert.deepEqual(parseFields('a="open b=1'), { a: 'open b=1' });
  assert.deepEqual(parseFields(''), {});
});

test('fields: the decoders lines', () => {
  const f = parseFields(AIS_STATIC);
  assert.equal(f.name, 'CAPT. "JACK"');
  assert.equal(f.callsign, 'WDE5199');
  assert.equal(parseFields(APRS_POS).comment, 'Hi there');
});

test('position: valid lat/lon only, with alternative names', () => {
  assert.deepEqual(position(parseFields(ADSB_POS)), { lat: 52.2572, lon: 3.91937 });
  assert.deepEqual(position({ latitude: '-33.9', longitude: '151.2' }), { lat: -33.9, lon: 151.2 });
  assert.deepEqual(position({ lat: '1', lng: '2' }), { lat: 1, lon: 2 });
  assert.equal(position({ lat: '91', lon: '0' }), null);
  assert.equal(position({ lat: '10', lon: '-181' }), null);
  assert.equal(position({ lat: 'x', lon: '1' }), null);
  assert.equal(position({ lat: '', lon: '1' }), null);
  assert.equal(position({ lat: '10' }), null);
});

test('identify: one id per kind of decoder', () => {
  assert.deepEqual(identify(parseFields(ADSB_IDENT)), { key: 'icao:4840D6', kind: 'aircraft' });
  assert.deepEqual(identify(parseFields(AIS_STATIC)), { key: 'mmsi:366053209', kind: 'ship' });
  assert.deepEqual(identify(parseFields(APRS_POS)), { key: 'src:N0CALL-9', kind: 'station' });
  // An APRS object is its own track, not its sender's.
  assert.deepEqual(identify(parseFields(APRS_OBJ)), { key: 'object:LEADER', kind: 'object' });
  assert.deepEqual(identify({ serial: 'S1234567', lat: '1', lon: '2' }), { key: 'serial:S1234567', kind: 'other' });
  assert.deepEqual(identify({ id: '7' }), { key: 'id:7', kind: 'other' });
  assert.deepEqual(identify({ callsign: 'X' }), { key: 'callsign:X', kind: 'other' });
  assert.equal(identify({ t: '1', text: 'hello' }), null);
});

test('heading: track over ground first, then heading', () => {
  assert.equal(heading(parseFields(AIS_POS)), 219.3);
  assert.equal(heading({ heading: '90' }), 90);
  assert.equal(heading({ heading_deg: '10', track_deg: '20' }), 20);
  assert.equal(heading({ course: '360' }), 0);
  assert.equal(heading({ cog: '400', heading: '5' }), 5);
  assert.equal(heading({}), null);
});

test('store: identity, position and velocity merge into one aircraft', () => {
  const s = new TrackStore();
  assert.ok(s.addLine(ADSB_IDENT, 1000));
  let t = s.tracks.get('icao:4840D6')!;
  assert.equal(t.pos, null);
  assert.equal([...s.positioned()].length, 0);
  s.addLine(ADSB_POS, 2000);
  s.addLine(ADSB_VEL, 3000);
  assert.equal(s.tracks.size, 1);
  t = s.tracks.get('icao:4840D6')!;
  assert.deepEqual(t.pos, { lat: 52.2572, lon: 3.91937, at: 2000 });
  assert.equal(t.fields.callsign, 'KLM1023');
  assert.equal(t.fields.alt_ft, '38000');
  assert.equal(t.fields.type, 'velocity');
  assert.equal(t.seen, 3000);
  assert.equal(t.messages, 3);
  assert.equal(label(t), 'KLM1023');
  assert.equal(heading(t.fields), 182.9);
});

test('store: AIS static data names the ship; position-less first is fine', () => {
  const s = new TrackStore();
  s.addText(`${AIS_STATIC}\n${AIS_POS}\n`, 5);
  const t = s.tracks.get('mmsi:366053209')!;
  assert.equal(label(t), 'CAPT. "JACK"');
  assert.equal(t.pos?.lat, 37.802118);
  assert.equal(t.kind, 'ship');
});

test('store: APRS station and object are separate tracks', () => {
  const s = new TrackStore();
  assert.equal(s.addText(`${APRS_POS}\n${APRS_OBJ}\nno fields here\n`, 1), true);
  assert.deepEqual([...s.tracks.keys()].sort(), ['object:LEADER', 'src:N0CALL-9']);
  assert.equal(label(s.tracks.get('object:LEADER')!), 'LEADER');
  assert.equal(label(s.tracks.get('src:N0CALL-9')!), 'N0CALL-9');
  assert.equal(s.addText('nothing to see\n', 2), false);
});

test('store: positions without an id share one anonymous track', () => {
  const s = new TrackStore();
  s.addLine('lat=1 lon=2', 1);
  s.addLine('lat=1.5 lon=2.5', 2);
  assert.equal(s.tracks.size, 1);
  assert.equal(s.tracks.get(ANONYMOUS)!.trail.length, 2);
  assert.equal(label(s.tracks.get(ANONYMOUS)!), '');
});

test('store: trails grow on movement, are capped, and shrink with the setting', () => {
  const s = new TrackStore({ trailLength: 3 });
  for (let i = 0; i < 5; i++) s.addLine(`mmsi=1 lat=${i} lon=0`, i);
  // A repeat of the same position only refreshes it.
  s.addLine('mmsi=1 lat=4 lon=0', 10);
  const t = s.tracks.get('mmsi:1')!;
  assert.deepEqual(
    t.trail.map((p) => p.lat),
    [2, 3, 4],
  );
  assert.equal(t.pos!.at, 10);
  assert.equal(t.pos, t.trail[2]);
  s.setTrailLength(1);
  assert.deepEqual(
    t.trail.map((p) => p.lat),
    [4],
  );
});

test('store: expiry and the track limit', () => {
  const s = new TrackStore({ maxTracks: 2 });
  s.addLine('mmsi=1 lat=0 lon=0', 0);
  s.addLine('mmsi=2 lat=0 lon=0', 10);
  s.addLine('mmsi=1 lat=1 lon=0', 20);
  s.addLine('mmsi=3 lat=0 lon=0', 30);
  // mmsi 2 was silent longest.
  assert.deepEqual([...s.tracks.keys()].sort(), ['mmsi:1', 'mmsi:3']);
  assert.equal(s.expire(40, 15), true);
  assert.deepEqual([...s.tracks.keys()], ['mmsi:3']);
  assert.equal(s.expire(40, 15), false);
  s.clear();
  assert.equal(s.tracks.size, 0);
});

test('mercator: round trips and edges', () => {
  assert.equal(lonToX(-180), 0);
  assert.equal(lonToX(180), 1);
  assert.ok(Math.abs(latToY(0) - 0.5) < 1e-12);
  assert.ok(latToY(85.0511287798) < 1e-9);
  assert.ok(latToY(-90) <= 1 && latToY(-90) > 0.999999);
  for (const lat of [-80, -33.9, 0, 12.5, 52.2572]) assert.ok(Math.abs(yToLat(latToY(lat)) - lat) < 1e-9);
  for (const lon of [-179, 0, 3.9, 151.2]) assert.ok(Math.abs(xToLon(lonToX(lon)) - lon) < 1e-9);
});

test('geodesy: destination and distance agree', () => {
  const p = destination(52, 4, 90, 100_000);
  assert.ok(Math.abs(distance(52, 4, p.lat, p.lon) - 100_000) < 1);
  // One degree of latitude is about 111.2 km.
  assert.ok(Math.abs(distance(0, 0, 1, 0) - 111_195) < 10);
  // Across the date line.
  const q = destination(0, 179.5, 90, 111_195);
  assert.ok(Math.abs(q.lon - -179.5) < 1e-3, String(q.lon));
});

test('nice steps and tile URLs', () => {
  assert.equal(niceStep(7), 5);
  assert.equal(niceStep(30), 20);
  assert.equal(niceStep(0.13), 0.1);
  assert.equal(tileUrl('https://tile.example/{z}/{x}/{y}.png', 3, 4, 5), 'https://tile.example/3/4/5.png');
  assert.equal(tileUrl('https://{s}.tile.example/{z}/{x}/{y}.png', 1, 0, 0), 'https://a.tile.example/1/0/0.png');
  assert.equal(tileUrl('', 1, 0, 0), null);
  assert.equal(tileUrl('file:///x/{z}/{x}/{y}', 1, 0, 0), null);
  assert.equal(tileUrl('https://tile.example/{z}/{x}.png', 1, 0, 0), null);
});
