<script lang="ts">
  import { Handle, Position } from '@xyflow/svelte';
  import { PARAM_PORT, portType } from './api';
  import type { BlockData } from './graph';
  import { session } from './session.svelte';
  import { indexErrors } from './errors';
  import { prefs } from './prefs.svelte';
  import { heat } from './plot';

  let { id, data, selected }: { id: string; data: BlockData; selected?: boolean } = $props();

  const ROW = 22;
  const TOP = 42;
  const rowTop = (i: number) => TOP + i * ROW;
  /** Settings wired as inputs: rows below the stream inputs. */
  const wired = $derived((data.wired ?? []).map((n) => data.desc.params.find((p) => p.name === n)).filter((p) => p !== undefined));
  const rows = $derived(Math.max(data.desc.inputs.length + wired.length, data.desc.outputs.length, 1));
  const error = $derived(
    data.desc.category === 'Unknown' ? `Unknown block kind \`${data.desc.kind}\`` : indexErrors(session.status.errors).blocks.get(id),
  );
  const warning = $derived(indexErrors(session.status.warnings ?? []).blocks.get(id));
  /** A constant input's value, short. */
  const constText = (v: unknown) => (Array.isArray(v) ? `${v[0]}${Number(v[1]) < 0 ? '−' : '+'}${Math.abs(Number(v[1])) || v[1]}j` : String(v));
  const running = $derived(session.status.state === 'running');
  const cpu = $derived(prefs.overlay ? session.stats?.cpu.get(id) : undefined);

  const CATEGORY_COLOURS: Record<string, string> = {
    Sources: 'var(--ok)',
    Math: 'var(--t-i16)',
    Filters: 'var(--t-f32)',
    Demodulators: '#ff9bd2',
    Flow: 'var(--warn)',
    Sinks: 'var(--muted)',
    Hardware: 'var(--accent)',
  };
</script>

<div
  class="block"
  class:selected
  class:error={!!error}
  style:min-height="{TOP + rows * ROW + 18}px"
  style:box-shadow={cpu !== undefined && !selected ? `0 0 0 2px ${heat(Math.min(1, cpu))}` : undefined}
>
  <header title={data.desc.help}>
    <span class="dot" style:background={CATEGORY_COLOURS[data.desc.category] ?? 'var(--muted)'}></span>
    {data.desc.label}
    {#if data.desc.tx}<span class="txchip" title="Transmits: running it needs TX armed">TX</span>{/if}
    {#if cpu !== undefined}
      <span class="cpu mono" style:color={heat(Math.min(1, cpu))} title="Share of one CPU core spent in this block">{(cpu * 100).toFixed(cpu < 0.1 ? 1 : 0)}%</span>
    {:else if running && !error}<span class="live" title="Running"></span>{/if}
  </header>
  {#each data.desc.inputs as p, i (p.name)}
    {@const t = portType(p, data.params)}
    {#if data.consts && p.name in data.consts}
      <span class="port in const" style:top="{rowTop(i) - 8}px" title="Input {p.name} is the constant {constText(data.consts[p.name])} (edit it in the inspector)"
        >{p.name} = {constText(data.consts[p.name])}</span
      >
    {:else}
      <Handle type="target" position={Position.Left} id={p.name} style="top:{rowTop(i)}px; --port: var(--t-{t})" title="{p.name} · {t}" />
      <span class="port in" style:top="{rowTop(i) - 8}px">{p.name}</span>
    {/if}
  {/each}
  {#each wired as p, k (p.name)}
    {@const i = data.desc.inputs.length + k}
    <Handle
      type="target"
      position={Position.Left}
      id="{PARAM_PORT}{p.name}"
      class="setting"
      style="top:{rowTop(i)}px"
      title="Setting {p.label}, wired as an input (f32). Until a value arrives it is {String(data.params[p.name] ?? p.default)}."
    />
    <span class="port in setting" style:top="{rowTop(i) - 8}px">{p.label}</span>
  {/each}
  {#each data.desc.outputs as p, i (p.name)}
    {@const t = portType(p, data.params)}
    <Handle type="source" position={Position.Right} id={p.name} style="top:{rowTop(i)}px; --port: var(--t-{t})" title="{p.name} · {t}" />
    <span class="port out" style:top="{rowTop(i) - 8}px">{p.name} · <span style:color="var(--t-{t})">{t}</span></span>
  {/each}
  <footer>
    {#if error}
      <span class="err" title={error}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="10"></circle><path d="M12 7v6M12 17h.01"></path></svg>
        <span>{error}</span>
      </span>
    {:else if warning}
      <span class="warn" title={warning}>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" aria-hidden="true"><path d="M12 3l10 18H2z"></path><path d="M12 10v4M12 18h.01"></path></svg>
        <span>{warning}</span>
      </span>
    {:else}
      <span class="id">{id}</span>
    {/if}
  </footer>
</div>

<style>
  .txchip {
    padding: 0 4px;
    border-radius: 3px;
    background: #d1242f;
    color: #fff;
    font-size: 10px;
    font-weight: 700;
  }
  .block {
    position: relative;
    width: 170px;
    box-sizing: border-box;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 8px;
    color: var(--text);
    font-size: 12px;
  }
  .block.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 20%, transparent);
  }
  .block.error {
    border-color: var(--err-line);
  }
  header {
    height: 30px;
    box-sizing: border-box;
    padding: 0 10px;
    display: flex;
    align-items: center;
    gap: 6px;
    border-bottom: 1px solid var(--line);
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
  }
  .dot {
    flex: none;
    width: 6px;
    height: 6px;
    border-radius: 2px;
  }
  .live {
    margin-left: auto;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--ok);
  }
  .cpu {
    margin-left: auto;
    font-size: 11px;
    font-weight: 500;
  }
  .port {
    position: absolute;
    color: var(--muted);
    font: 11px var(--mono);
    pointer-events: none;
  }
  .port.in {
    left: 12px;
  }
  .port.out {
    right: 12px;
  }
  .port.const {
    left: 8px;
    max-width: 150px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text);
    font-style: italic;
  }
  .port.setting {
    color: var(--warn);
    font-family: inherit;
  }
  /* A setting wired as an input: a diamond, not a dot. */
  .block :global(.svelte-flow__handle.setting) {
    --port: var(--warn);
    border-radius: 2px;
    transform: translate(-50%, -50%) rotate(45deg);
  }
  footer {
    position: absolute;
    left: 10px;
    right: 10px;
    bottom: 5px;
    font-size: 11px;
    overflow: hidden;
  }
  .id {
    font: 10px var(--mono);
    color: var(--faint);
  }
  .err {
    display: flex;
    align-items: center;
    gap: 4px;
    color: var(--err);
  }
  .err span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .err svg {
    flex: none;
  }
  .warn {
    display: flex;
    align-items: center;
    gap: 4px;
    color: var(--warn);
  }
  .warn span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .warn svg {
    flex: none;
  }
  :global(.svelte-flow__handle) {
    width: 10px;
    height: 10px;
    background: var(--port, var(--muted));
    border: 2px solid var(--bg);
  }
  :global(.svelte-flow__handle.connectingto.valid),
  :global(.svelte-flow__handle.valid) {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--ok) 50%, transparent);
  }
  :global(.svelte-flow__handle.connectingto:not(.valid)) {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--err) 60%, transparent);
  }
</style>
