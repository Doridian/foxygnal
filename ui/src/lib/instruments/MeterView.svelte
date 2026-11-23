<script lang="ts">
  import type { MeterFrame } from '../frames';
  import { fmtNum } from '../plot';

  let { frame, colour }: { frame: MeterFrame | null; colour: string } = $props();

  const FLOOR = -100;
  const db = (v: number) => (v > 0 ? 20 * Math.log10(v) : -Infinity);
  const pct = (d: number) => `${Math.max(0, Math.min(100, ((d - FLOOR) / -FLOOR) * 100))}%`;
  const fmtDbfs = (d: number) => (Number.isFinite(d) ? `${d.toFixed(1)} dBFS` : '−∞ dBFS');

  // A peak marker that holds for a moment, then falls.
  let held = $state(-Infinity);
  let heldAt = 0;
  $effect(() => {
    if (!frame) return;
    const p = db(frame.peak);
    const now = performance.now();
    if (p >= held || now - heldAt > 1500) {
      held = p;
      heldAt = now;
    }
  });

  const rms = $derived(frame ? db(frame.rms) : -Infinity);
  const peak = $derived(frame ? db(frame.peak) : -Infinity);
</script>

<div class="meter">
  <div class="row">
    <span class="name">RMS</span>
    <div class="bar" role="meter" aria-label="RMS level" aria-valuemin={FLOOR} aria-valuemax={0} aria-valuenow={Number.isFinite(rms) ? rms : FLOOR}>
      <div class="fill" style:width={pct(rms)} style:background="var({colour})"></div>
    </div>
    <span class="val mono">{fmtDbfs(rms)}</span>
  </div>
  <div class="row">
    <span class="name">Peak</span>
    <div class="bar" role="meter" aria-label="Peak level" aria-valuemin={FLOOR} aria-valuemax={0} aria-valuenow={Number.isFinite(peak) ? peak : FLOOR}>
      <div class="fill peak" style:width={pct(peak)}></div>
      {#if Number.isFinite(held)}<div class="hold" style:left={pct(held)}></div>{/if}
    </div>
    <span class="val mono">{fmtDbfs(peak)}</span>
  </div>
  <div class="scale mono" aria-hidden="true">
    {#each [-100, -80, -60, -40, -20, 0] as d (d)}<span>{d}</span>{/each}
  </div>
  {#if frame}
    <dl class="mono">
      <dt>RMS</dt><dd>{fmtNum(frame.rms)}</dd>
      <dt>Peak</dt><dd>{fmtNum(frame.peak)}</dd>
      <dt>{frame.complex ? 'DC (I, Q)' : 'DC'}</dt>
      <dd>{frame.complex ? `${fmtNum(frame.mean[0])}, ${fmtNum(frame.mean[1])}` : fmtNum(frame.mean[0])}</dd>
    </dl>
  {/if}
</div>

<style>
  .meter {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 10px;
    padding: 4px 8px;
  }
  .row {
    display: grid;
    grid-template-columns: 40px 1fr 96px;
    align-items: center;
    gap: 10px;
    font-size: 12px;
  }
  .name {
    color: var(--muted);
  }
  .bar {
    position: relative;
    height: 14px;
    background: #0a0d11;
    border: 1px solid var(--line);
    border-radius: 3px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    transition: width 60ms linear;
  }
  .fill.peak {
    background: color-mix(in srgb, var(--accent) 70%, transparent);
  }
  .hold {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: var(--text);
  }
  .val {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .scale {
    display: flex;
    justify-content: space-between;
    margin: -6px 106px 0 50px;
    font-size: 10px;
    color: var(--faint);
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px 12px;
    margin: 0;
    font-size: 11px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
</style>
