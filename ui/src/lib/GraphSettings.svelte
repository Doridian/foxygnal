<script lang="ts">
  // Graph-level settings, shown in the inspector when nothing is selected:
  // variables (`[vars]`) and, for graphs used as blocks elsewhere, exported
  // ports (`[export]`).
  import type { Exports } from './api';
  import { VAR_NAME, isDerived } from './expr';
  import type { BlockNode } from './graph';
  import { formatSi, parseSi } from './si';

  let {
    vars,
    exports,
    nodes,
    resolved,
    error,
    onvars,
    onrenamevar,
    onexports,
  }: {
    vars: Record<string, number | string>;
    exports: Exports;
    nodes: BlockNode[];
    resolved: { values: Map<string, number>; errors: Map<string, string> };
    /** A graph-wide error from the engine (not pinned to a block or wire). */
    error: string | null;
    onvars: (vars: Record<string, number | string>) => void;
    onrenamevar: (from: string, to: string) => void;
    onexports: (exports: Exports) => void;
  } = $props();

  let nameProblem = $state<{ name: string; msg: string } | null>(null);
  const entries = $derived(Object.entries(vars));

  function setValue(name: string, text: string) {
    text = text.trim();
    const n = parseSi(text);
    onvars({ ...vars, [name]: n ?? text });
  }

  function rename(from: string, el: HTMLInputElement) {
    const to = el.value.trim();
    if (to === from) return;
    const msg = !VAR_NAME.test(to) ? 'Use letters, digits and `_`, not starting with a digit.' : to in vars ? `\`${to}\` already exists.` : null;
    if (msg) {
      nameProblem = { name: from, msg };
      el.value = from;
      return;
    }
    nameProblem = null;
    onrenamevar(from, to);
  }

  function add() {
    let i = 1;
    while (`var_${i}` in vars) i++;
    onvars({ ...vars, [`var_${i}`]: 0 });
  }

  function remove(name: string) {
    const { [name]: _, ...rest } = vars;
    onvars(rest);
  }

  const shown = (v: number | string) => (typeof v === 'number' ? formatSi(v) : v);

  // ---- Exports ----

  const inputEndpoints = $derived(nodes.flatMap((n) => n.data.desc.inputs.map((p) => `${n.id}.${p.name}`)));
  const outputEndpoints = $derived(nodes.flatMap((n) => n.data.desc.outputs.map((p) => `${n.id}.${p.name}`)));
  const ins = $derived(exports.input ?? []);
  const outs = $derived(exports.output ?? []);
  const hasPorts = $derived(ins.length + outs.length > 0);
  let open = $state(false);

  const split = (text: string) => {
    const eps = text.split(',').map((s) => s.trim()).filter(Boolean);
    return eps.length === 1 ? eps[0] : eps;
  };
  const joined = (to: string | string[]) => (Array.isArray(to) ? to.join(', ') : to);

  function freshPort(base: string, taken: { name: string }[]) {
    if (!taken.some((p) => p.name === base)) return base;
    for (let i = 2; ; i++) if (!taken.some((p) => p.name === `${base}${i}`)) return `${base}${i}`;
  }

  function setText(key: 'label' | 'help', text: string) {
    const { [key]: _, ...rest } = exports;
    onexports(text.trim() ? { ...rest, [key]: text.trim() } : rest);
  }
</script>

<section class="vars" aria-label="Graph variables">
  <h3>Variables</h3>
  <p class="hint">Use them in any number field as <code>$name</code> or in expressions like <code>$samp_rate / 10</code>.</p>
  {#if entries.length}
    <div class="grid">
      {#each entries as [name, v] (name)}
        {@const err = resolved.errors.get(name)}
        {@const val = resolved.values.get(name)}
        <input class="mono name" type="text" aria-label="Variable name" value={name} onchange={(e) => rename(name, e.currentTarget)} />
        <input
          class="mono"
          class:invalid={!!err}
          type="text"
          aria-label="Value of {name}"
          value={shown(v)}
          onchange={(e) => setValue(name, e.currentTarget.value)}
          onkeydown={(e) => e.key === 'Enter' && setValue(name, e.currentTarget.value)}
        />
        <button class="icon" title="Delete {name}" aria-label="Delete {name}" onclick={() => remove(name)}>×</button>
        {#if nameProblem?.name === name}
          <p class="problem row">{nameProblem.msg}</p>
        {/if}
        {#if err}
          <p class="problem row" role="alert">{err}</p>
        {:else if typeof v === 'string' && val !== undefined}
          <p class="value row mono">= {formatSi(val)}{hasPorts && isDerived(v) ? ' · derived, not a block parameter' : ''}</p>
        {/if}
      {/each}
    </div>
  {/if}
  <button class="add" onclick={add}>+ Add variable</button>
  {#if error}<div class="errbox" role="alert">{error}</div>{/if}
</section>

<section class="exports" aria-label="Use as a block">
  <button class="disclose" aria-expanded={open || hasPorts} onclick={() => (open = !open)} disabled={hasPorts}>
    <h3>Use as a block</h3>
    {#if !hasPorts}<span class="muted">{open ? '−' : '+'}</span>{/if}
  </button>
  {#if open || hasPorts}
    <p class="hint">
      Export ports to use this graph as a block in other graphs (save it in the project folder; it then appears under <em>Graphs</em> in the palette).
      Variables that don't refer to others become the block's parameters.
    </p>
    <datalist id="export-inputs">{#each inputEndpoints as e (e)}<option value={e}></option>{/each}</datalist>
    <datalist id="export-outputs">{#each outputEndpoints as e (e)}<option value={e}></option>{/each}</datalist>
    {#each ins as p, i (i)}
      <div class="port">
        <span class="muted small">in</span>
        <input class="mono" type="text" aria-label="Input port name" value={p.name} onchange={(e) => onexports({ ...exports, input: ins.map((x, j) => (j === i ? { ...x, name: e.currentTarget.value.trim() } : x)) })} />
        <span class="muted">→</span>
        <input class="mono" type="text" list="export-inputs" aria-label="Feeds" placeholder="block.port" value={joined(p.to)} onchange={(e) => onexports({ ...exports, input: ins.map((x, j) => (j === i ? { ...x, to: split(e.currentTarget.value) } : x)) })} />
        <button class="icon" aria-label="Remove input {p.name}" onclick={() => onexports({ ...exports, input: ins.filter((_, j) => j !== i) })}>×</button>
      </div>
    {/each}
    {#each outs as p, i (i)}
      <div class="port">
        <span class="muted small">out</span>
        <input class="mono" type="text" aria-label="Output port name" value={p.name} onchange={(e) => onexports({ ...exports, output: outs.map((x, j) => (j === i ? { ...x, name: e.currentTarget.value.trim() } : x)) })} />
        <span class="muted">←</span>
        <input class="mono" type="text" list="export-outputs" aria-label="Fed by" placeholder="block.port" value={p.from} onchange={(e) => onexports({ ...exports, output: outs.map((x, j) => (j === i ? { ...x, from: e.currentTarget.value.trim() } : x)) })} />
        <button class="icon" aria-label="Remove output {p.name}" onclick={() => onexports({ ...exports, output: outs.filter((_, j) => j !== i) })}>×</button>
      </div>
    {/each}
    <div class="buttons">
      <button class="add" onclick={() => onexports({ ...exports, input: [...ins, { name: freshPort('in', ins), to: inputEndpoints[0] ?? '' }] })}>+ Input</button>
      <button class="add" onclick={() => onexports({ ...exports, output: [...outs, { name: freshPort('out', outs), from: outputEndpoints[0] ?? '' }] })}>+ Output</button>
    </div>
    {#if hasPorts}
      <input type="text" aria-label="Palette label" placeholder="Palette label (default: file name)" value={exports.label ?? ''} onchange={(e) => setText('label', e.currentTarget.value)} />
      <input type="text" aria-label="Help text" placeholder="Help text" value={exports.help ?? ''} onchange={(e) => setText('help', e.currentTarget.value)} />
    {/if}
  {/if}
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  h3 {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--text);
  }
  .hint {
    margin: 0;
    font-size: 11px;
    color: var(--faint);
    line-height: 1.4;
  }
  .grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1.3fr) auto;
    gap: 4px 6px;
    align-items: center;
  }
  .row {
    grid-column: 1 / -1;
    margin: -2px 0 2px;
    font-size: 11px;
  }
  .value {
    color: var(--muted);
  }
  .problem {
    color: var(--err);
  }
  .invalid {
    border-color: var(--err-line);
  }
  .icon {
    padding: 2px 8px;
    line-height: 1;
  }
  .add {
    align-self: flex-start;
    font-size: 12px;
  }
  .buttons {
    display: flex;
    gap: 6px;
  }
  .port {
    display: grid;
    grid-template-columns: 24px minmax(0, 0.8fr) auto minmax(0, 1.2fr) auto;
    gap: 4px;
    align-items: center;
  }
  .small {
    font-size: 11px;
  }
  .disclose {
    display: flex;
    justify-content: space-between;
    align-items: center;
    background: none;
    border: 0;
    padding: 0;
    color: inherit;
    cursor: pointer;
  }
  .disclose:disabled {
    cursor: default;
    opacity: 1;
  }
  .errbox {
    padding: 8px 10px;
    border: 1px solid var(--err-line);
    border-radius: 6px;
    background: var(--err-bg);
    color: var(--err);
    font-size: 12px;
    line-height: 1.4;
  }
</style>
