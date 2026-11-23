<script lang="ts">
  import type { ParamSpec } from './api';
  import { formatSi, parseSi } from './si';

  let { spec, value, running, onchange }: { spec: ParamSpec; value: unknown; running: boolean; onchange: (v: unknown) => void } = $props();

  const unit = $derived(spec.kind === 'float' ? spec.unit : undefined);
  const range = $derived(
    (spec.kind === 'float' || spec.kind === 'int') && spec.min !== undefined && spec.max !== undefined ? { min: spec.min, max: spec.max } : null,
  );
  let problem = $state<string | null>(null);
  const fid = $derived(`param-${spec.name}`);

  function commitNumber(text: string, el: HTMLInputElement) {
    const v = parseSi(text, unit);
    if (v === null) {
      problem = `“${text}” isn't a number. Try 2.4M, 100k or 0.5.`;
      return;
    }
    if (spec.kind === 'int' && !Number.isInteger(v)) problem = 'Must be a whole number.';
    else if ((spec.kind === 'float' || spec.kind === 'int') && spec.min !== undefined && v < spec.min) problem = `Must be at least ${formatSi(spec.min, unit)}.`;
    else if ((spec.kind === 'float' || spec.kind === 'int') && spec.max !== undefined && v > spec.max) problem = `Must be at most ${formatSi(spec.max, unit)}.`;
    else problem = null;
    if (problem) return;
    onchange(v);
    el.value = display(v);
  }

  function display(v: unknown) {
    return typeof v === 'number' ? (spec.kind === 'int' ? String(v) : formatSi(v, unit)) : String(v ?? '');
  }
</script>

<div class="field">
  <label class="head" for={fid}>
    <span>{spec.label}{unit ? ` (${unit})` : ''}</span>
    {#if spec.live}
      <span class="live" title="Applies live while running"></span>
    {:else if running}
      <span class="restart">applies on restart</span>
    {/if}
  </label>
  {#if spec.kind === 'enum'}
    <select id={fid} value={value} onchange={(e) => onchange(e.currentTarget.value)}>
      {#each spec.options as o (o)}<option value={o}>{o}</option>{/each}
    </select>
  {:else if spec.kind === 'bool'}
    <input id={fid} type="checkbox" checked={!!value} onchange={(e) => onchange(e.currentTarget.checked)} />
  {:else if spec.kind === 'float' || spec.kind === 'int'}
    <input
      id={fid}
      class="mono"
      class:invalid={!!problem}
      type="text"
      inputmode="decimal"
      value={display(value)}
      aria-invalid={!!problem}
      onchange={(e) => commitNumber(e.currentTarget.value, e.currentTarget)}
      onkeydown={(e) => e.key === 'Enter' && commitNumber(e.currentTarget.value, e.currentTarget)}
    />
    {#if range}
      <input
        type="range"
        aria-label="{spec.label} slider"
        min={range.min}
        max={range.max}
        step={spec.kind === 'int' ? 1 : (range.max - range.min) / 1000}
        value={Number(value)}
        oninput={(e) => onchange(Number(e.currentTarget.value))}
      />
    {/if}
  {:else}
    <input id={fid} class:mono={spec.kind === 'path'} type="text" value={String(value ?? '')} placeholder={spec.kind === 'path' ? 'path on the engine host' : ''} onchange={(e) => onchange(e.currentTarget.value)} />
  {/if}
  {#if problem}<p class="problem" role="alert">{problem}</p>{/if}
  {#if spec.help}<p class="help">{spec.help}</p>{/if}
</div>

<style>
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--muted);
  }
  .live {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--ok);
  }
  .restart {
    margin-left: auto;
    font-size: 11px;
    color: var(--faint);
  }
  input[type='checkbox'] {
    width: auto;
    align-self: flex-start;
  }
  input[type='range'] {
    padding: 0;
    border: 0;
    accent-color: var(--accent);
  }
  .invalid {
    border-color: var(--err-line);
  }
  .problem {
    margin: 0;
    font-size: 12px;
    color: var(--err);
  }
  .help {
    margin: 0;
    font-size: 11px;
    color: var(--faint);
    line-height: 1.4;
  }
</style>
