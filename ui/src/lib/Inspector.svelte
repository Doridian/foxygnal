<script lang="ts">
  import { portType } from './api';
  import type { BlockNode, Wire } from './graph';
  import { indexErrors } from './errors';
  import ParamField from './ParamField.svelte';
  import WireInfo from './WireInfo.svelte';
  import { session } from './session.svelte';

  let {
    node,
    edge,
    onparam,
    onrename,
    ondelete,
  }: {
    node: BlockNode | undefined;
    edge: Wire | undefined;
    onparam: (id: string, name: string, value: unknown) => void;
    onrename: (from: string, to: string) => string | null;
    ondelete: () => void;
  } = $props();

  const running = $derived(session.status.state === 'running');
  const errs = $derived(indexErrors(session.status.errors));
  let renameProblem = $state<string | null>(null);

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
      {/if}
      <div class="field">
        <label class="small muted" for="block-id">Block id</label>
        <input id="block-id" class="mono" type="text" value={node.id} onchange={rename} />
        {#if renameProblem}<p class="problem">{renameProblem}</p>{/if}
      </div>
      {#each d.params as p (p.name)}
        <ParamField spec={p} value={node.data.params[p.name] ?? p.default} {running} onchange={(v) => onparam(node.id, p.name, v)} />
      {/each}
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
  .danger {
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
