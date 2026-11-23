// Transport stream demuxing and codec probing (src/lib/video), run with
// `npm test` (node's test runner; node 23.6+ runs TypeScript as it is).
// The ffmpeg tests make small streams and skip if ffmpeg isn't installed.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { crc32mpeg, defaultProgram, Demuxer, PACKET, type Pes } from '../src/lib/video/ts.ts';
import { adtsFrames, avcAccessUnit, hevcAccessUnit, nalUnits } from '../src/lib/video/nal.ts';
import { Mp2Decoder, type Mp2Frame } from '../src/lib/video/mp2.ts';

// ---- Building streams by hand ------------------------------------------------

function withCrc(sec: number[]): number[] {
  const c = crc32mpeg(new Uint8Array(sec));
  return [...sec, c >>> 24, (c >>> 16) & 255, (c >>> 8) & 255, c & 255];
}

function section(tableId: number, ext: number, body: number[]): number[] {
  const len = 5 + body.length + 4;
  return withCrc([tableId, 0xb0 | (len >> 8), len & 255, ext >> 8, ext & 255, 0xc1, 0, 0, ...body]);
}

/** Packets for one PSI section or PES (payload split over as many packets as needed). */
function packets(pid: number, payload: number[], cc: { n: number }, psi: boolean, opts: { tei?: boolean } = {}): Uint8Array[] {
  const data = psi ? [0, ...payload] : payload;
  const out: Uint8Array[] = [];
  for (let at = 0, first = true; at < data.length || first; first = false) {
    const p = new Uint8Array(PACKET).fill(0xff);
    p[0] = 0x47;
    p[1] = (first ? 0x40 : 0) | (opts.tei ? 0x80 : 0) | (pid >> 8);
    p[2] = pid & 255;
    const room = PACKET - 4;
    const n = Math.min(room, data.length - at);
    if (n < room && !psi) {
      // Pad PES with an adaptation field.
      const af = room - n;
      p[3] = 0x30 | (cc.n++ & 15);
      p[4] = af - 1;
      if (af > 1) p[5] = 0;
      p.set(data.slice(at, at + n), 4 + af);
    } else {
      p[3] = 0x10 | (cc.n++ & 15);
      p.set(data.slice(at, at + n), 4);
    }
    at += n;
    out.push(p);
  }
  return out;
}

function pes(streamId: number, pts: number, es: number[]): number[] {
  const ts = (v: number) => [0x21 | (Math.floor(v / 2 ** 29) & 0x0e), (v >> 22) & 255, ((v >> 14) & 0xfe) | 1, (v >> 7) & 255, ((v << 1) & 0xfe) | 1];
  const len = 3 + 5 + es.length;
  return [0, 0, 1, streamId, len >> 8, len & 255, 0x80, 0x80, 5, ...ts(pts), ...es];
}

function concat(list: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(list.length * PACKET);
  list.forEach((p, i) => out.set(p, i * PACKET));
  return out;
}

function sdtBody(service: number, provider: string, name: string): number[] {
  const enc = (s: string) => [...s].map((c) => c.charCodeAt(0));
  const desc = [0x48, 3 + provider.length + name.length, 0x01, provider.length, ...enc(provider), name.length, ...enc(name)];
  return [0, 1, 0xff, service >> 8, service & 255, 0xfc, 0x80 | (desc.length >> 8), desc.length & 255, ...desc];
}

function fullProgram() {
  const cc = { pat: { n: 0 }, pmt: { n: 0 }, sdt: { n: 0 }, v: { n: 0 }, a: { n: 0 } };
  const pat = section(0x00, 1, [0, 1, 0xf0, 0x00]); // program 1 → PMT PID 0x1000
  const pmt = section(0x02, 1, [0xe1, 0x00, 0xf0, 0x00, 0x1b, 0xe1, 0x00, 0xf0, 0x00, 0x0f, 0xe1, 0x01, 0xf0, 0x06, 0x0a, 0x04, 0x64, 0x65, 0x75, 0x00]);
  const sdt = section(0x42, 1, sdtBody(1, 'Fox', 'Fox One'));
  return { psi: [...packets(0, pat, cc.pat, true), ...packets(0x1000, pmt, cc.pmt, true), ...packets(0x11, sdt, cc.sdt, true)], cc };
}

test('programme list with names, codecs and languages', () => {
  const d = new Demuxer();
  let changes = 0;
  d.onPrograms = () => changes++;
  d.push(concat(fullProgram().psi));
  const p = d.programs.get(1)!;
  assert.equal(p.pmtPid, 0x1000);
  assert.equal(p.pcrPid, 0x100);
  assert.equal(p.name, 'Fox One');
  assert.equal(p.provider, 'Fox');
  assert.deepEqual(
    p.streams.map((s) => [s.pid, s.codec, s.kind, s.lang]),
    [
      [0x100, 'h264', 'video', null],
      [0x101, 'aac', 'audio', 'deu'],
    ],
  );
  assert.equal(defaultProgram(d.programs.values())?.number, 1);
  assert.ok(changes >= 3);
  // The same versions again change nothing.
  const before = changes;
  d.push(concat(fullProgram().psi));
  assert.equal(changes, before);
});

test('a section with a bad CRC is ignored', () => {
  const d = new Demuxer();
  const cc = { n: 0 };
  const pat = section(0x00, 1, [0, 1, 0xf0, 0x00]);
  pat[pat.length - 1] ^= 1;
  d.push(concat(packets(0, pat, cc, true)));
  assert.equal(d.programs.size, 0);
});

test('PES reassembly with PTS, across packets', () => {
  const { psi, cc } = fullProgram();
  const d = new Demuxer();
  d.wanted.add(0x100);
  const got: Pes[] = [];
  d.onPes = (p) => got.push(p);
  const es1 = Array.from({ length: 500 }, (_, i) => i & 255);
  const es2 = [1, 2, 3];
  d.push(concat([...psi, ...packets(0x100, pes(0xe0, 90000, es1), cc.v, false), ...packets(0x100, pes(0xe0, 93600, es2), cc.v, false)]));
  assert.equal(got.length, 2, 'bounded PES are emitted when complete');
  assert.equal(got[0].pts, 90000);
  assert.deepEqual([...got[0].data], es1);
  assert.equal(got[1].pts, 93600);
  assert.deepEqual([...got[1].data], es2);
});

test('PTS beyond 2^32 and unwrapping past 2^33', () => {
  const { cc } = fullProgram();
  const d = new Demuxer();
  d.wanted.add(0x100);
  const got: number[] = [];
  d.onPes = (p) => got.push(p.pts!);
  const near = 2 ** 33 - 1000;
  d.push(concat([...packets(0x100, pes(0xe0, near, [1]), cc.v, false), ...packets(0x100, pes(0xe0, 2600, [1]), cc.v, false)]));
  assert.deepEqual(got, [near, 2 ** 33 + 2600]);
});

test('continuity errors and TEI packets drop the damaged PES', () => {
  const { psi, cc } = fullProgram();
  const d = new Demuxer();
  d.wanted.add(0x100);
  const got: Pes[] = [];
  d.onPes = (p) => got.push(p);
  const big = Array.from({ length: 600 }, () => 7);
  const a = packets(0x100, pes(0xe0, 1000, big), cc.v, false);
  a.splice(1, 1); // lose a packet
  const b = packets(0x100, pes(0xe0, 2000, big), cc.v, false);
  b[1][1] |= 0x80; // FEC failed on this one
  const c = packets(0x100, pes(0xe0, 3000, [9, 9]), cc.v, false);
  d.push(concat([...psi, ...a, ...b, ...c]));
  assert.equal(d.ccErrors, 1);
  assert.equal(d.tei, 1);
  assert.deepEqual(
    got.map((p) => p.pts),
    [3000],
    'only the intact PES survives',
  );
});

test('a duplicate packet is not a continuity error', () => {
  const { psi, cc } = fullProgram();
  const d = new Demuxer();
  d.wanted.add(0x100);
  const got: Pes[] = [];
  d.onPes = (p) => got.push(p);
  const a = packets(0x100, pes(0xe0, 1000, Array.from({ length: 300 }, () => 1)), cc.v, false);
  d.push(concat([...psi, a[0], a[0], a[1]]));
  assert.equal(d.ccErrors, 0);
  assert.equal(got.length, 1);
  assert.equal(got[0].data.length, 300);
});

test('Annex B: NAL units, codec string and key frames', () => {
  // SPS (High 4.0), PPS, IDR slice.
  const au = new Uint8Array([0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0, 1, 0x65, 0x88, 0x84]);
  assert.equal(nalUnits(au).length, 3);
  assert.deepEqual(avcAccessUnit(au), { codec: 'avc1.640028', key: true });
  // A P slice alone: slice_type 0 → ue(v) "1" after first_mb_in_slice "1": bits 1 010 → 0xa0.
  assert.deepEqual(avcAccessUnit(new Uint8Array([0, 0, 1, 0x41, 0xa0])), { codec: null, key: false });
  // HEVC: an IDR_W_RADL (type 19) is key.
  assert.equal(hevcAccessUnit(new Uint8Array([0, 0, 1, 19 << 1, 1, 0xaf])).key, true);
});

test('ADTS frames', () => {
  // AAC LC, 48 kHz (index 3), stereo, 10-byte frames.
  const f = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x5f, 0xfc, 0, 0, 0];
  const got = adtsFrames(new Uint8Array([...f, ...f]));
  assert.equal(got.length, 2);
  assert.deepEqual([got[0].object, got[0].rate, got[0].channels, got[0].data.length], [2, 48000, 2, 10]);
});

// ---- Real streams from ffmpeg ------------------------------------------------

function ffmpeg(args: string[]): boolean {
  try {
    execFileSync('ffmpeg', ['-v', 'error', '-y', ...args], { stdio: 'pipe' });
    return true;
  } catch {
    return false;
  }
}

const hasFfmpeg = ffmpeg(['-version']);

for (const [name, vcodec, acodec, vtype, atype] of [
  ['H.264 + AAC', 'libx264', 'aac', 'h264', 'aac'],
  ['MPEG-2 + MP2', 'mpeg2video', 'mp2', 'mpeg2', 'mp2'],
] as const) {
  test(`ffmpeg stream: ${name}`, { skip: !hasFfmpeg && 'ffmpeg not installed' }, () => {
    const dir = mkdtempSync(join(tmpdir(), 'fox-ts-'));
    try {
      const file = join(dir, 'a.ts');
      const ok = ffmpeg([
        '-f', 'lavfi', '-i', 'testsrc=size=320x240:rate=25', '-f', 'lavfi', '-i', 'sine', '-t', '2',
        '-c:v', vcodec, '-pix_fmt', 'yuv420p', '-g', '25', '-c:a', acodec, '-metadata', 'service_name=Test One', '-f', 'mpegts', file,
      ]);
      if (!ok) return; // this ffmpeg lacks the encoder
      const data = new Uint8Array(readFileSync(file));
      assert.equal(data.length % PACKET, 0);
      const d = new Demuxer();
      const prog = () => defaultProgram(d.programs.values());
      const got = new Map<number, Pes[]>();
      d.onPrograms = () => {
        for (const s of prog()?.streams ?? []) d.wanted.add(s.pid);
      };
      d.onPes = (p) => got.set(p.pid, [...(got.get(p.pid) ?? []), p]);
      // In odd-sized pieces, as frames arrive.
      for (let at = 0; at < data.length; at += 188 * 37) d.push(data.subarray(at, at + 188 * 37));
      const p = prog()!;
      for (const s of p.streams) d.flush(s.pid);
      assert.equal(p.name, 'Test One');
      const v = p.streams.find((s) => s.kind === 'video')!;
      const a = p.streams.find((s) => s.kind === 'audio')!;
      assert.equal(v.codec, vtype);
      assert.equal(a.codec, atype);
      assert.equal(d.ccErrors, 0);
      assert.equal(d.tei, 0);
      const vp = got.get(v.pid)!;
      assert.ok(vp.length >= 48 && vp.length <= 50, `${vp.length} video PES`);
      assert.ok(vp.every((x) => x.pts !== null));
      if (vtype === 'h264') {
        const first = avcAccessUnit(vp[0].data);
        assert.equal(first.key, true);
        assert.match(first.codec ?? '', /^avc1\.[0-9a-f]{6}$/);
        assert.ok(adtsFrames(got.get(a.pid)![0].data).length > 0);
      } else {
        const d0 = vp[0].data;
        assert.deepEqual([...d0.subarray(0, 4)], [0, 0, 1, 0xb3], 'starts with a sequence header');
      }
      assert.ok(d.pcr.has(p.pcrPid));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
}

// ---- MPEG audio (MP2) through the WebAssembly decoder ------------------------
// What the worker does with an MP2 PES (worker.ts itself needs a browser).

const mp2Module = WebAssembly.compile(readFileSync(new URL('../src/lib/video/mp2.wasm', import.meta.url)));
const mp2Fixtures = new URL('../../crates/foxygnal-mp2/tests/data/', import.meta.url);

test('MP2 decoder: a fixture against ffmpeg, with timestamps', async () => {
  const dec = await Mp2Decoder.fromModule(await mp2Module);
  const data = new Uint8Array(readFileSync(new URL('48k-stereo-192k.mp2', mp2Fixtures)));
  // Every 7th value of ffmpeg's decoding, 16-bit (see the crate's tests).
  const ref = new Int16Array(new Uint8Array(readFileSync(new URL('48k-stereo-192k.mp2.ref', mp2Fixtures))).buffer);
  const frames: Mp2Frame[] = [];
  // Pieces that don't line up with frames, each with a PTS like a PES.
  const piece = 700;
  for (let at = 0; at < data.length; at += piece) {
    dec.push(data.subarray(at, at + piece), 90000 + at);
    for (let f = dec.next(); f; f = dec.next()) frames.push(f);
  }
  dec.flush();
  for (let f = dec.next(); f; f = dec.next()) frames.push(f);
  assert.equal(frames.length, 3);
  assert.equal(dec.errors, 0);
  assert.equal(dec.layer, 2);
  assert.deepEqual([frames[0].rate, frames[0].channels, frames[0].bitrate, frames[0].samples.length], [48000, 2, 192000, 2304]);
  // 576-byte frames start at 0, 576 and 1152: a frame takes the PTS of the
  // piece it starts in, if it is the first to, else the previous frame's
  // plus 1152 samples.
  assert.deepEqual(
    frames.map((f) => f.pts),
    [90000, 90000 + 2160, 90700],
  );
  const pcm = new Float32Array(frames.flatMap((f) => [...f.samples]));
  assert.equal(Math.ceil(pcm.length / 7), ref.length);
  let max = 0;
  for (let i = 0, k = 0; i < pcm.length; i += 7, k++) {
    max = Math.max(max, Math.abs(Math.max(-32768, Math.min(32767, Math.round(pcm[i] * 32768))) - ref[k]));
  }
  assert.ok(max <= 1, `max error ${max} LSB`);
  dec.free();
});

test('MP2 decoder: garbage, then a reset', async () => {
  const dec = await Mp2Decoder.fromModule(await mp2Module);
  const junk = new Uint8Array(5000).map((_, i) => ((i * 7919) & 255) | (i % 5 === 0 ? 0xe0 : 0));
  dec.push(junk, 0);
  while (dec.next());
  dec.reset();
  dec.push(new Uint8Array(readFileSync(new URL('22k-mono-64k.mp2', mp2Fixtures))), 1000);
  const frames: Mp2Frame[] = [];
  for (let f = dec.next(); f; f = dec.next()) frames.push(f);
  assert.ok(frames.length >= 2, `${frames.length} frames`);
  assert.equal(frames[0].pts, 1000);
  assert.ok(frames.every((f) => f.channels === 1 && f.rate === 22050));
  dec.free();
});

test('ffmpeg stream: MP2 sound decodes in step with its PTS', { skip: !hasFfmpeg && 'ffmpeg not installed' }, async () => {
  const dir = mkdtempSync(join(tmpdir(), 'fox-ts-'));
  try {
    const file = join(dir, 'a.ts');
    // MPEG-2 video and two MP2 tracks (48 kHz stereo, then 24 kHz mono).
    const ok = ffmpeg([
      '-f', 'lavfi', '-i', 'testsrc=size=320x240:rate=25', '-f', 'lavfi', '-i', 'sine=frequency=1000:sample_rate=48000',
      '-f', 'lavfi', '-i', 'sine=frequency=500:sample_rate=24000', '-t', '2', '-map', '0', '-map', '1', '-map', '2',
      '-c:v', 'mpeg2video', '-c:a', 'mp2', '-ac:a:0', '2', '-b:a:0', '192k', '-b:a:1', '64k', '-f', 'mpegts', file,
    ]);
    if (!ok) return;
    const d = new Demuxer();
    const got = new Map<number, Pes[]>();
    d.onPrograms = () => {
      for (const s of defaultProgram(d.programs.values())?.streams ?? []) d.wanted.add(s.pid);
    };
    d.onPes = (p) => got.set(p.pid, [...(got.get(p.pid) ?? []), p]);
    d.push(new Uint8Array(readFileSync(file)));
    const prog = defaultProgram(d.programs.values())!;
    for (const s of prog.streams) d.flush(s.pid);
    const audio = prog.streams.filter((s) => s.kind === 'audio');
    assert.deepEqual(
      audio.map((s) => s.codec),
      ['mp2', 'mp2'],
    );
    for (const [i, s] of audio.entries()) {
      const dec = await Mp2Decoder.fromModule(await mp2Module);
      const [rate, channels] = i === 0 ? [48000, 2] : [24000, 1];
      let frames = 0;
      let sum = 0;
      let count = 0;
      let last: number | null = null;
      for (const p of got.get(s.pid)!) {
        dec.push(p.data, p.pts);
        for (let f = dec.next(); f; f = dec.next()) {
          frames++;
          assert.equal(f.rate, rate);
          assert.equal(f.channels, channels);
          // Every frame has a time, 1152 samples after the last one's.
          assert.notEqual(f.pts, null);
          if (last !== null) assert.ok(Math.abs(f.pts! - last - (1152 * 90000) / rate) <= 1, `${f.pts} after ${last}`);
          last = f.pts;
          for (const v of f.samples) {
            sum += v * v;
            count++;
          }
        }
      }
      assert.equal(dec.errors, 0);
      // 2 s of 1152-sample frames.
      const want = (2 * rate) / 1152;
      assert.ok(Math.abs(frames - want) <= 2, `${frames} frames, want about ${want}`);
      // lavfi's sine is at 1/8 of full scale (RMS 0.088), 3 dB down when
      // ffmpeg makes stereo of it.
      const rms = Math.sqrt(sum / count);
      const wantRms = 0.125 * Math.SQRT1_2 * (i === 0 ? Math.SQRT1_2 : 1);
      assert.ok(Math.abs(rms - wantRms) < 0.003, `RMS ${rms}, want ${wantRms}`);
      dec.free();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
