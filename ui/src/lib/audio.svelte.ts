// Browser audio: plays `browser_audio` blocks' inputs through WebAudio.
//
// The engine streams every sample of the wire feeding the block as `audio`
// probe frames. Each frame becomes an AudioBuffer at the stream's own rate
// (the browser resamples to the output device), scheduled right after the
// previous one. A small lead absorbs network jitter; if playback falls
// behind or samples were lost it restarts at the lead, and if it drifts too
// far ahead (a clock faster than the sound card's) frames are skipped.
// A c32 wire is stereo: the frames interleave left (real) and right (imag).

const LEAD = 0.15; // seconds queued ahead of the playhead
const MAX_AHEAD = 0.6;

class BrowserAudio {
  #ctx: AudioContext | null = null;
  #gain: GainNode | null = null;
  #next = new Map<number, number>(); // probe id → next start time
  /** The context exists but the browser hasn't let it start yet. */
  blocked = $state(false);
  volume = $state(0.8);
  muted = $state(false);

  /** Call from a click or key handler: browsers only start audio on a gesture. */
  unlock() {
    if (!this.#ctx) {
      try {
        this.#ctx = new AudioContext({ latencyHint: 'playback' });
      } catch {
        return;
      }
      this.#gain = this.#ctx.createGain();
      this.#gain.connect(this.#ctx.destination);
      this.#applyVolume();
      this.#ctx.onstatechange = () => (this.blocked = this.#ctx?.state === 'suspended');
    }
    void this.#ctx.resume().then(() => (this.blocked = false));
    this.blocked = this.#ctx.state === 'suspended';
  }

  setVolume(v: number) {
    this.volume = v;
    this.#applyVolume();
  }

  toggleMute() {
    this.muted = !this.muted;
    this.#applyVolume();
  }

  #applyVolume() {
    if (this.#gain) this.#gain.gain.value = this.muted ? 0 : this.volume * this.volume;
  }

  /** Queue one frame of a stream: mono, or stereo with L, R interleaved. */
  play(id: number, samples: Float32Array, channels: 1 | 2, rate: number, gap: boolean) {
    const ctx = this.#ctx;
    const frames = Math.floor(samples.length / channels);
    if (!ctx || !this.#gain || frames === 0) return;
    if (ctx.state !== 'running') {
      this.blocked = ctx.state === 'suspended';
      return;
    }
    const now = ctx.currentTime;
    let at = this.#next.get(id) ?? 0;
    if (gap || at < now) at = now + LEAD;
    else if (at - now > MAX_AHEAD) return; // too far ahead: skip a frame to catch up
    const buf = ctx.createBuffer(channels, frames, rate);
    if (channels === 1) {
      buf.copyToChannel(samples as Float32Array<ArrayBuffer>, 0);
    } else {
      const left = buf.getChannelData(0);
      const right = buf.getChannelData(1);
      for (let i = 0; i < frames; i++) {
        left[i] = samples[2 * i];
        right[i] = samples[2 * i + 1];
      }
    }
    const src = ctx.createBufferSource();
    src.buffer = buf;
    src.connect(this.#gain);
    src.start(at);
    this.#next.set(id, at + frames / rate);
  }

  forget(id: number) {
    this.#next.delete(id);
  }
}

export const browserAudio = new BrowserAudio();
