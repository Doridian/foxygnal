<script lang="ts">
  import type { ComponentProps } from 'svelte';
  import { GRAPH_KIND, portType, wireable, type StreamType } from './api';
  import { parseSi } from './si';
  import type { BlockNode, Wire } from './graph';
  import { indexErrors } from './errors';
  import type { Scope } from './expr';
  import GraphSettings from './GraphSettings.svelte';
  import ParamField from './ParamField.svelte';
  import ResponsePlot from './ResponsePlot.svelte';
  import WireInfo from './WireInfo.svelte';
  import { session } from './session.svelte';

  let {
    node,
    edge,
    onparam,
    onwire,
    onconst,
    onrename,
    ondelete,
    scope,
    settings,
    onopengraph,
  }: {
    node: BlockNode | undefined;
    edge: Wire | undefined;
    onparam: (id: string, name: string, value: unknown) => void;
    /** Show setting `name` as an input (handle `@name`), or stop. */
    onwire: (id: string, name: string, on: boolean) => void;
    /** Give stream input `port` a constant, or (`undefined`) a wire again. */
    onconst: (id: string, port: string, value: unknown) => void;
    onrename: (from: string, to: string) => string | null;
    ondelete: () => void;
    /** Graph variables, for params written as expressions. */
    scope: Scope;
    /** Variables and exports, edited when nothing is selected. */
    settings: Omit<ComponentProps<typeof GraphSettings>, 'error'>;
    /** Open a graph file (a hierarchical block's). */
    onopengraph: (name: string) => void;
  } = $props();

  const running = $derived(session.status.state === 'running');
  const errs = $derived(indexErrors(session.status.errors));
  /** Why the graph doesn't build right now (variables, sub-graphs…), from the engine. */
  const buildError = $derived(session.metaError);
  let renameProblem = $state<string | null>(null);

  /** A constant as typed: a number (SI prefixes allowed), an expression,
   *  or for c32 `re, im`. */
  function parseConst(text: string, t: StreamType): unknown {
    const one = (s: string) => parseSi(s.trim()) ?? s.trim();
    const parts = text.split(',');
    if (t === 'c32' && parts.length === 2) return [one(parts[0]), one(parts[1])];
    return one(text);
  }
  const showConst = (v: unknown) => (Array.isArray(v) ? v.join(', ') : String(v ?? ''));

  function rename(e: Event & { currentTarget: HTMLInputElement }) {
    if (!node) return;
    const to = e.currentTarget.value.trim();
    renameProblem = to === node.id ? null : onrename(node.id, to);
    if (renameProblem) e.currentTarget.value = node.id;
  }
</script>

<aside class="inspector" aria-label="Inspector">
  {#if node}
    {@const d = node.data.desc}
    {#key node.id}
      <div>
        <div class="title">
          <h2>{d.label}</h2>
          <span class="mono muted small">{d.kind}</span>
        </div>
        <p class="muted small help">{d.help}</p>
      </div>
      {#if errs.blocks.get(node.id)}
        <div class="errbox" role="alert">{errs.blocks.get(node.id)}</div>
      {:else if buildError?.block === node.id}
        <div class="errbox" role="alert">{buildError.message}</div>
      {/if}
      {#if d.kind.startsWith(GRAPH_KIND)}
        <button class="open" onclick={() => onopengraph(d.kind.slice(GRAPH_KIND.length))}>Open {d.kind.slice(GRAPH_KIND.length)}</button>
      {/if}
      <div class="field">
        <label class="small muted" for="block-id">Block id</label>
        <input id="block-id" class="mono" type="text" value={node.id} onchange={rename} />
        {#if renameProblem}<p class="problem">{renameProblem}</p>{/if}
      </div>
      {#each d.params as p (p.name)}
        {@const isWired = node.data.wired?.includes(p.name) ?? false}
        <div class="param">
          <ParamField spec={p} value={node.data.params[p.name] ?? p.default} {running} {scope} wired={isWired} onchange={(v) => onparam(node.id, p.name, v)} />
          {#if wireable(p)}
            <button
              class="wire"
              class:on={isWired}
              aria-pressed={isWired}
              title={isWired
                ? 'Stop using this setting as an input (removes its wire)'
                : 'Turn this setting into an input: an f32 wire sets it while running'}
              onclick={() => onwire(node.id, p.name, !isWired)}>⇢ input</button
            >
          {/if}
        </div>
      {/each}
      {#if d.inputs.length}
        <div class="inputs">
          <h3 class="small muted">Inputs</h3>
          {#each d.inputs as p (p.name)}
            {@const t = portType(p, node.data.params)}
            {@const c = node.data.consts?.[p.name]}
            <div class="input">
              <label class="small">
                <input type="checkbox" checked={c !== undefined} onchange={(e) => onconst(node.id, p.name, e.currentTarget.checked ? (t === 'c32' ? [1, 0] : 1) : undefined)} />
                <span class="mono">{p.name}</span> <span class="muted">· {t} ·</span> constant
              </label>
              {#if c !== undefined}
                <input
                  class="mono"
                  type="text"
                  aria-label="Constant for input {p.name}"
                  value={showConst(c)}
                  placeholder={t === 'c32' ? 're, im' : 'value or $var'}
                  onchange={(e) => onconst(node.id, p.name, parseConst(e.currentTarget.value, t))}
                />
              {/if}
            </div>
          {/each}
          <p class="muted tiny">A constant input needs no wire. It runs in step with the block's other inputs, which set the rate. Changes apply live.</p>
        </div>
      {/if}
      {#if d.response}
        <ResponsePlot id={node.id} desc={d} params={node.data.params} {scope} />
      {/if}
      {#if d.inputs.length || d.outputs.length}
        <div class="ports">
          {#each d.inputs as p (p.name)}
            {@const t = portType(p, node.data.params)}
            <span class="muted">in · {p.name}</span><span class="mono" style:color="var(--t-{t})">{t}</span>
          {/each}
          {#each d.outputs as p (p.name)}
            {@const t = portType(p, node.data.params)}
            <span class="muted">out · {p.name}</span><span class="mono" style:color="var(--t-{t})">{t}</span>
          {/each}
        </div>
      {/if}
    {/key}
  {:else if edge}
    <div class="title"><h2>Wire</h2></div>
    <p class="mono small">{edge.source}.{edge.sourceHandle} → {edge.target}.{edge.targetHandle}</p>
    {#if errs.edges.get(edge.id)}
      <div class="errbox" role="alert">{errs.edges.get(edge.id)}</div>
    {/if}
    <div>
      <WireInfo edge={{ from: `${edge.source}.${edge.sourceHandle}`, to: `${edge.target}.${edge.targetHandle}` }} />
    </div>
    <button class="danger" onclick={ondelete}>Delete wire</button>
  {:else}
    <GraphSettings {...settings} error={buildError && !buildError.block && !buildError.edge ? buildError.message : null} />
    <div class="empty">
      <p>Select a block to edit its parameters, or click a wire to see what it carries and watch it with a scope or spectrum.</p>
      <dl class="keys">
        <dt><kbd>/</kbd></dt><dd>search blocks</dd>
        <dt><kbd>Space</kbd></dt><dd>run / stop</dd>
        <dt><kbd>Del</kbd></dt><dd>delete selection</dd>
        <dt><kbd>Ctrl</kbd> <kbd>Z</kbd></dt><dd>undo (<kbd>Shift</kbd> to redo)</dd>
        <dt><kbd>Ctrl</kbd> <kbd>C</kbd> / <kbd>V</kbd></dt><dd>copy / paste</dd>
        <dt><kbd>Ctrl</kbd> <kbd>S</kbd></dt><dd>save</dd>
        <dt><kbd>F</kbd></dt><dd>fit view</dd>
      </dl>
    </div>
  {/if}
</aside>

<style>
  .inspector {
    background: var(--panel);
    border-left: 1px solid var(--line);
    padding: 14px;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .title {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
  }
  h2 {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
  }
  .help {
    margin: 4px 0 0;
    line-height: 1.45;
  }
  .small {
    font-size: 12px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .problem {
    margin: 0;
    font-size: 12px;
    color: var(--err);
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
  .ports {
    border-top: 1px solid var(--line);
    padding-top: 10px;
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 4px 10px;
    font-size: 12px;
  }
  .param {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .wire {
    align-self: flex-start;
    padding: 1px 6px;
    font-size: 11px;
    color: var(--muted);
  }
  .wire.on {
    color: var(--warn);
    border-color: var(--warn);
  }
  .inputs {
    border-top: 1px solid var(--line);
    padding-top: 10px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .inputs h3 {
    margin: 0;
    font-weight: 600;
  }
  .input {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .input label {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .input input[type='checkbox'] {
    width: auto;
  }
  .tiny {
    margin: 0;
    font-size: 11px;
    line-height: 1.4;
  }
  .danger,
  .open {
    align-self: flex-start;
  }
  .empty {
    color: var(--muted);
    font-size: 13px;
    line-height: 1.5;
  }
  .keys {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 6px 10px;
    font-size: 12px;
  }
  .keys dt {
    white-space: nowrap;
  }
  .keys dd {
    margin: 0;
  }
</style>
