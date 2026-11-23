// The Video instrument's worker: demuxes the transport stream, decodes the
// chosen program's video (WebCodecs for H.264 / HEVC, our WebAssembly decoder
// for MPEG-1/2) and sound (AAC, in ADTS or LATM, with WebCodecs; MPEG audio
// Layer I/II and AC-3 with our WebAssembly decoders), and works out when each
// frame is due. Frames go to the page as transferable VideoFrames, sound as
// PCM.
//
// Timing: the stream arrives in real time, so the program clock (PCR) maps
// to arrival time. `offset` is the lower envelope of arrival − PCR (late data
// only ever makes it larger), and a frame is due at PTS + offset + `delay`,
// the jitter buffer. Without a PCR, the first PTS is mapped to its arrival.

import wasmUrl from './mpeg2.wasm?url';
import mp2Url from './mp2.wasm?url';
import ac3Url from './ac3.wasm?url';
import { Ac3Decoder } from './ac3';
import { Mp2Decoder } from './mp2';
import { Mpeg2Decoder } from './mpeg2';
import { adtsFrames, avcAccessUnit, hevcAccessUnit, LatmReader, mpeg2HasSequenceHeader } from './nal';
import type { AudioStats, FromWorker, Stats, ToWorker } from './protocol';
import { Demuxer, defaultProgram, Unwrapper, type EsInfo, type Pes, type Program } from './ts';

/** Audio codecs played (`EsInfo.codec`). */
const DECODED_AUDIO = new Set(['aac', 'aac-latm', 'mp2', 'ac3']);

interface WorkerScope {
  postMessage(m: unknown, transfer: Transferable[]): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
  close(): void;
}
const scope = self as unknown as WorkerScope;
const post = (m: FromWorker, transfer: Transferable[] = []) => scope.postMessage(m, transfer);

/** Ms since the epoch, comparable between the worker and the page. */
const clock = () => performance.timeOrigin + performance.now();

const MIN_DELAY = 300;
const MAX_DELAY = 3000;

const demux = new Demuxer();
let choice: number | null = null; // the program asked for; null = auto
let trackChoice: number | null = null; // the sound track (PID) asked for; null = auto
let program: Program | null = null;
let videoEs: EsInfo | null = null;
let audioEs: EsInfo | null = null;
let audioOn = false;

// ---- Clock -------------------------------------------------------------------

let offset: number | null = null; // ms: arrival − PCR
let delay = MIN_DELAY;
let lastLate = 0;

function onPcr(pid: number, pcr: number) {
  if (!program || pid !== program.pcrPid) return;
  sample(clock() - pcr / 90);
}

function sample(s: number) {
  if (offset === null || Math.abs(s - offset) > 2000) {
    // First sample, or the clock jumped (a looped file, a new stream).
    if (offset !== null) post({ type: 'flush' });
    offset = s;
  } else if (s < offset) {
    offset = s;
  } else {
    // Let it rise slowly, for a sender clock slower than ours.
    offset += (s - offset) * 0.001;
  }
}

/** When a picture with this PTS (90 kHz) should be shown, or null if unknown. */
function due(pts: number): number {
  offset ??= clock() - pts / 90;
  return pts / 90 + offset + delay;
}

/** A frame came out late: grow the jitter buffer (shrinks back slowly). */
function late(by: number) {
  const now = clock();
  if (now - lastLate > 200) delay = Math.min(MAX_DELAY, delay + Math.min(500, by + 50));
  lastLate = now;
}

// ---- Stats -------------------------------------------------------------------

const stats: Stats = { bitrate: 0, packets: 0, tei: 0, ccErrors: 0, gaps: 0, video: null, audio: null };
let bytes = 0;
let videoBytes = 0;
let audioBytes = 0;
let framesOut = 0;
let lastStats = clock();

function videoStats(note: string | null = null) {
  if (!videoEs) return null;
  return {
    pid: videoEs.pid,
    codec: videoEs.codec,
    decoder: null,
    width: 0,
    height: 0,
    fps: 0,
    bitrate: 0,
    decoded: 0,
    dropped: 0,
    errors: 0,
    decodeMs: 0,
    note,
  };
}

function sendStats() {
  const now = clock();
  const dt = (now - lastStats) / 1000;
  if (dt < 0.5) return;
  lastStats = now;
  stats.bitrate = (bytes * 8) / dt;
  stats.packets = demux.packets;
  stats.tei = demux.tei;
  stats.ccErrors = demux.ccErrors;
  if (stats.video) {
    stats.video.bitrate = (videoBytes * 8) / dt;
    stats.video.fps = framesOut / dt;
    stats.video.decodeMs = decodeMs;
  }
  if (stats.audio) stats.audio.bitrate = (audioBytes * 8) / dt;
  bytes = videoBytes = audioBytes = framesOut = 0;
  // The jitter buffer shrinks back when nothing has been late for a while.
  if (now - lastLate > 10_000) delay = Math.max(MIN_DELAY, delay * 0.9);
  post({ type: 'stats', stats: structuredClone(stats) });
}

// ---- Video -------------------------------------------------------------------

interface VideoPath {
  pes(p: Pes): void;
  /** Packets were lost. */
  gap(): void;
  close(): void;
}

function emit(frame: VideoFrame, pts90: number, aspect: number) {
  const at = due(pts90);
  const now = clock();
  if (at < now - 20) late(now - at);
  framesOut++;
  const v = stats.video;
  if (v) {
    v.decoded++;
    v.width = frame.displayWidth;
    v.height = frame.displayHeight;
    v.note = null;
  }
  post({ type: 'frame', frame, due: at, aspect }, [frame]);
}

/** H.264 and HEVC through WebCodecs. */
function webCodecsPath(codec: 'h264' | 'hevc'): VideoPath {
  let decoder: VideoDecoder | null = null;
  let configured: string | null = null;
  let pending = false; // checking support
  let unsupported: string | null = null;
  let needKey = true;
  const v = stats.video!;

  function make(codecString: string) {
    decoder?.close();
    decoder = new VideoDecoder({
      output: (f) => emit(f, (f.timestamp * 9) / 100, 0),
      error: (e) => {
        v.errors++;
        v.note = `Decoder error: ${e.message}. Waiting for the next key frame.`;
        decoder = null;
        configured = null;
        needKey = true;
      },
    });
    decoder.configure({ codec: codecString, optimizeForLatency: true });
    configured = codecString;
    v.decoder = codecString;
  }

  return {
    pes(p) {
      const info = codec === 'h264' ? avcAccessUnit(p.data) : hevcAccessUnit(p.data);
      if (unsupported) return;
      if (info.key && info.codec && info.codec !== configured && !pending) {
        const want = info.codec;
        if (typeof VideoDecoder === 'undefined') {
          unsupported = 'This browser has no WebCodecs video decoder.';
          v.note = unsupported;
          return;
        }
        pending = true;
        void VideoDecoder.isConfigSupported({ codec: want }).then(
          (r) => {
            pending = false;
            if (r.supported) make(want);
            else {
              unsupported = `${codec === 'hevc' ? 'HEVC' : 'H.264'} (${want}) isn't supported by this browser.`;
              v.note = unsupported;
            }
          },
          () => {
            pending = false;
            unsupported = `${want} isn't supported by this browser.`;
            v.note = unsupported;
          },
        );
        return;
      }
      if (!decoder || decoder.state !== 'configured') {
        if (!pending && !info.key) v.note ??= 'Waiting for a key frame…';
        return;
      }
      if (needKey && !info.key) {
        // Before the first picture this is just waiting, not dropping.
        if (v.decoded) v.dropped++;
        return;
      }
      if (decoder.decodeQueueSize > 30) {
        // The decoder can't keep up: skip to the next key frame.
        needKey = true;
        v.dropped++;
        return;
      }
      needKey = false;
      if (p.pts === null) return;
      decoder.decode(new EncodedVideoChunk({ type: info.key ? 'key' : 'delta', timestamp: Math.round((p.pts * 100) / 9), data: p.data }));
    },
    gap() {
      needKey = true;
    },
    close() {
      if (decoder && decoder.state !== 'closed') decoder.close();
      decoder = null;
    },
  };
}

const ASPECT = [0, 0, 4 / 3, 16 / 9, 2.21];

/** MPEG-1/2 through the WebAssembly decoder. */
function wasmPath(): VideoPath {
  const v = stats.video!;
  v.decoder = 'wasm';
  let dec: Mpeg2Decoder | null = null;
  let failed = false;
  let needSeq = true;
  let queue: Pes[] = [];
  const unwrap = new Unwrapper();
  Mpeg2Decoder.create(wasmUrl).then(
    (d) => {
      dec = d;
      const q = queue;
      queue = [];
      for (const p of q) feed(p);
    },
    (e: Error) => {
      failed = true;
      v.note = `The MPEG-2 decoder didn't load: ${e.message}`;
    },
  );

  function feed(p: Pes) {
    if (!dec) return;
    if (needSeq) {
      if (!mpeg2HasSequenceHeader(p.data)) {
        v.note ??= 'Waiting for a sequence header…';
        return;
      }
      needSeq = false;
    }
    // Decoding happens in push and next, and a picture may come out a call
    // after the one that decoded it: time every call, per picture out.
    let t0 = performance.now();
    dec.push(p.data, p.pts);
    for (;;) {
      const pic = dec.next();
      decodeSpent += performance.now() - t0;
      if (!pic) break;
      decodeMs = 0.9 * decodeMs + 0.1 * decodeSpent;
      decodeSpent = 0;
      v.errors = dec.errors;
      if (pic.pts === null) continue;
      const pts = unwrap.unwrap(pic.pts);
      // Even sizes for I420; odd edges are cropped.
      const w = pic.width & ~1;
      const h = pic.height & ~1;
      const cw = (pic.width + 1) >> 1;
      const ch = (pic.height + 1) >> 1;
      try {
        const frame = new VideoFrame(pic.data, {
          format: 'I420',
          codedWidth: w,
          codedHeight: h,
          timestamp: Math.round((pts * 100) / 9),
          layout: [
            { offset: 0, stride: pic.width },
            { offset: pic.width * pic.height, stride: cw },
            { offset: pic.width * pic.height + cw * ch, stride: cw },
          ],
        });
        emit(frame, pts, ASPECT[pic.aspect] ?? 0);
      } catch (e) {
        v.errors++;
        v.note = `Can't make a video frame: ${(e as Error).message}`;
      }
      t0 = performance.now();
    }
  }

  return {
    pes(p) {
      if (failed) return;
      if (!dec) {
        if (queue.length < 200) queue.push(p);
        return;
      }
      feed(p);
    },
    gap() {
      dec?.reset();
      needSeq = true;
    },
    close() {
      dec?.free();
      dec = null;
    },
  };
}

let decodeMs = 0;
let decodeSpent = 0;
let video: VideoPath | null = null;

// ---- Audio -------------------------------------------------------------------
function audioPes(p: Pes) {
  const a = stats.audio;
  if (!a || !audioOn || !audioEs) return;
  switch (audioEs.codec) {
    case 'aac':
      adtsPes(p, a);
      break;
    case 'aac-latm':
      latmPes(p, a);
      break;
    case 'mp2':
    case 'ac3':
      wasmAudioPes(p, a);
      break;
  }
}

// AAC through WebCodecs: ADTS frames go in as they are, LATM's raw access
// units with the AudioSpecificConfig as the description.
let audioDecoder: AudioDecoder | null = null;
let audioConfig = '';
let audioPending = false;

/** Configure the decoder for `key` if it isn't yet; true if it is ready for chunks. */
function aacDecoder(a: AudioStats, key: string, config: AudioDecoderConfig): boolean {
  if (key === audioConfig) return !!audioDecoder && audioDecoder.state === 'configured';
  if (audioPending) return false;
  if (typeof AudioDecoder === 'undefined') {
    a.note = 'This browser has no WebCodecs audio decoder.';
    return false;
  }
  audioPending = true;
  const gen = audioGen;
  const fail = () => {
    if (gen !== audioGen) return;
    audioPending = false;
    audioConfig = key;
    a.note = 'AAC isn’t supported by this browser’s WebCodecs.';
  };
  void AudioDecoder.isConfigSupported(config).then((r) => {
    if (gen !== audioGen) return;
    if (!r.supported) return fail();
    audioPending = false;
    audioConfig = key;
    audioDecoder?.close();
    audioDecoder = new AudioDecoder({ output: pcm, error: (e) => (a.note = `Audio decoder error: ${e.message}`) });
    audioDecoder.configure(config);
    a.decoder = config.codec;
    a.note = null;
  }, fail);
  return false;
}

function adtsPes(p: Pes, a: AudioStats) {
  if (p.pts === null) return;
  const frames = adtsFrames(p.data);
  if (!frames.length) return;
  const f0 = frames[0];
  a.rate = f0.rate;
  a.channels = f0.channels;
  const config = { codec: `mp4a.40.${f0.object}`, sampleRate: f0.rate, numberOfChannels: f0.channels };
  if (!aacDecoder(a, `adts/${f0.object}/${f0.rate}/${f0.channels}`, config)) return;
  let pts = p.pts;
  for (const f of frames) {
    audioDecoder!.decode(new EncodedAudioChunk({ type: 'key', timestamp: Math.round((pts * 100) / 9), data: f.data }));
    pts += (1024 * 90000) / f.rate;
  }
}

const latm = new LatmReader();
let latmNext: number | null = null; // 90 kHz: when the next access unit plays

function latmPes(p: Pes, a: AudioStats) {
  // A PES's PTS is its first frame's if no frame straddles into it.
  if (p.pts !== null && (latm.buffered === 0 || latmNext === null || Math.abs(latmNext - p.pts) > 9000)) latmNext = p.pts;
  for (const f of latm.push(p.data)) {
    const c = f.config;
    a.rate = c.rate;
    a.channels = c.channels;
    const hex = [...c.asc].map((v) => v.toString(16).padStart(2, '0')).join('');
    const config = { codec: `mp4a.40.${c.object}`, sampleRate: c.rate, numberOfChannels: c.channels, description: c.asc };
    // An access unit is 1024 samples at the core rate: 2048 at the output rate with SBR.
    const samples = c.object === 5 || c.object === 29 ? 2048 : 1024;
    const at = latmNext;
    if (latmNext !== null) latmNext += (samples * 90000) / c.rate;
    if (at === null || !aacDecoder(a, `latm/${hex}`, config)) continue;
    audioDecoder!.decode(new EncodedAudioChunk({ type: 'key', timestamp: Math.round((at * 100) / 9), data: f.data }));
  }
  if (!latm.config && !a.note) a.note = 'Waiting for the LATM stream’s configuration…';
  else if (latm.config && a.note?.startsWith('Waiting')) a.note = null;
}

function pcm(d: AudioData) {
  const n = d.numberOfFrames;
  const channels: 1 | 2 = d.numberOfChannels >= 2 ? 2 : 1;
  const out = new Float32Array(n * channels);
  const plane = new Float32Array(n);
  for (let c = 0; c < channels; c++) {
    d.copyTo(plane, { planeIndex: c, format: 'f32-planar' });
    for (let i = 0; i < n; i++) out[i * channels + c] = plane[i];
  }
  const at = due((d.timestamp * 9) / 100);
  const rate = d.sampleRate;
  d.close();
  if (stats.audio) stats.audio.frames++;
  post({ type: 'pcm', samples: out, channels, rate, due: at }, [out.buffer]);
}

// MPEG audio Layer I/II and AC-3 through our WebAssembly decoders. Each
// loads when sound is first switched on for such a stream; PES packets wait
// meanwhile.
let wasmAudio: Mp2Decoder | Ac3Decoder | null = null;
let audioGen = 0; // bumped by closeAudio, so a load finishing late is dropped
let wasmLoading: number | null = null; // the generation a load is in flight for
let wasmQueue: Pes[] = [];
const wasmUnwrap = new Unwrapper();

function wasmAudioPes(p: Pes, a: AudioStats) {
  const ac3 = audioEs?.codec === 'ac3';
  if (!wasmAudio) {
    if (wasmQueue.length < 50) wasmQueue.push(p);
    if (wasmLoading === audioGen) return;
    const gen = (wasmLoading = audioGen);
    const load: Promise<Mp2Decoder | Ac3Decoder> = ac3 ? Ac3Decoder.create(ac3Url) : Mp2Decoder.create(mp2Url);
    load.then(
      (d) => {
        if (gen !== audioGen) {
          d.free(); // closed meanwhile
          return;
        }
        wasmAudio = d;
        wasmLoading = null;
        a.decoder = 'wasm';
        const q = wasmQueue;
        wasmQueue = [];
        for (const p of q) wasmAudioPes(p, a);
      },
      (e: Error) => {
        if (gen === audioGen) a.note = `The ${ac3 ? 'AC-3' : 'MPEG audio'} decoder didn't load: ${e.message}`;
      },
    );
    return;
  }
  const dec = wasmAudio;
  dec.push(p.data, p.pts);
  for (let f = dec.next(); f; f = dec.next()) {
    a.frames++;
    a.rate = f.rate;
    a.channels = f.channels;
    if (f.pts === null) continue;
    post({ type: 'pcm', samples: f.samples, channels: f.channels, rate: f.rate, due: due(wasmUnwrap.unwrap(f.pts)) }, [f.samples.buffer]);
  }
  a.errors = dec.errors;
  if (dec instanceof Ac3Decoder) {
    const k = dec.kind;
    a.layout = k?.layout ?? null;
    a.note = k?.eac3 ? 'E-AC-3 isn’t decoded in the browser.' : null;
  } else if (dec.layer === 3) {
    a.note = 'MPEG audio Layer III (MP3) isn’t decoded in the browser.';
  }
}

/** After lost packets: the sound decoders find their frames again. */
function audioGap() {
  wasmAudio?.reset();
  latm.reset();
  latmNext = null;
}

function closeAudio() {
  if (audioDecoder && audioDecoder.state !== 'closed') audioDecoder.close();
  audioDecoder = null;
  audioConfig = '';
  audioPending = false;
  latm.reset();
  latm.config = null;
  latmNext = null;
  wasmAudio?.free();
  wasmAudio = null;
  audioGen++;
  wasmLoading = null;
  wasmQueue = [];
  wasmUnwrap.reset();
}

// ---- Program choice ------------------------------------------------------------

function setAudio(a: EsInfo | null) {
  closeAudio();
  audioEs = a;
  demux.wanted = new Set([videoEs?.pid, a?.pid].filter((x): x is number => x !== undefined));
  stats.audio = a
    ? { pid: a.pid, codec: a.codec, lang: a.lang, layout: null, decoder: null, rate: 0, channels: 0, bitrate: 0, frames: 0, errors: 0, note: null }
    : null;
  if (stats.audio && a && !DECODED_AUDIO.has(a.codec)) stats.audio.note = `${a.codec === 'eac3' ? 'E-AC-3' : a.codec.toUpperCase()} audio isn’t decoded in the browser.`;
}

function choose() {
  const progs = [...demux.programs.values()];
  const next = choice === null ? defaultProgram(progs) : (demux.programs.get(choice) ?? null);
  const v = next?.streams.find((s) => s.kind === 'video') ?? null;
  const tracks = next?.streams.filter((s) => s.kind === 'audio') ?? [];
  // The track asked for if this program has it, else the first this page
  // can decode, in PMT order, else the first at all (to name it).
  const a = tracks.find((s) => s.pid === trackChoice) ?? tracks.find((s) => DECODED_AUDIO.has(s.codec)) ?? tracks[0] ?? null;
  const sameVideo = next?.number === program?.number && v?.pid === videoEs?.pid && v?.type === videoEs?.type;
  const sameAudio = a?.pid === audioEs?.pid && a?.type === audioEs?.type;
  program = next;
  post({ type: 'programs', programs: progs, selected: program?.number ?? null });
  if (sameVideo && sameAudio) return;
  if (sameVideo) {
    // Another sound track: the pictures and the clock carry on.
    setAudio(a);
    post({ type: 'audio-flush' });
    return;
  }
  video?.close();
  video = null;
  videoEs = v;
  setAudio(a);
  offset = null;
  stats.video = videoStats();
  post({ type: 'flush' });
  if (!v) return;
  if (v.codec === 'h264' || v.codec === 'hevc') video = webCodecsPath(v.codec);
  else if (v.codec === 'mpeg2' || v.codec === 'mpeg1') video = wasmPath();
  else stats.video!.note = `${v.codec.toUpperCase()} video can't be decoded here.`;
}

demux.onPrograms = choose;
demux.onPcr = onPcr;
demux.onPes = (p) => {
  if (videoEs && p.pid === videoEs.pid) video?.pes(p);
  else if (audioEs && p.pid === audioEs.pid) audioPes(p);
};

scope.onmessage = (e: MessageEvent<ToWorker>) => {
  const m = e.data;
  switch (m.type) {
    case 'packets': {
      if (m.gap) {
        stats.gaps++;
        demux.discontinuity();
        video?.gap();
        audioGap();
      }
      const data = new Uint8Array(m.data);
      bytes += data.length;
      // Per-stream rates from the packet counts.
      const vBefore = videoEs ? (demux.pidStats.get(videoEs.pid)?.packets ?? 0) : 0;
      const aBefore = audioEs ? (demux.pidStats.get(audioEs.pid)?.packets ?? 0) : 0;
      demux.push(data);
      if (videoEs) videoBytes += ((demux.pidStats.get(videoEs.pid)?.packets ?? 0) - vBefore) * 188;
      if (audioEs) audioBytes += ((demux.pidStats.get(audioEs.pid)?.packets ?? 0) - aBefore) * 188;
      sendStats();
      break;
    }
    case 'program':
      choice = m.number;
      choose();
      break;
    case 'track':
      trackChoice = m.pid;
      choose();
      break;
    case 'audio':
      audioOn = m.on;
      if (!audioOn) closeAudio();
      break;
    case 'reset':
      video?.close();
      video = null;
      closeAudio();
      program = null;
      videoEs = audioEs = null;
      offset = null;
      demux.reset();
      stats.gaps = 0;
      break;
    case 'close':
      video?.close();
      closeAudio();
      scope.close();
      break;
  }
};
