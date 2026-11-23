<script lang="ts">
  import { SvelteFlow, Background, Controls, useSvelteFlow, type Connection, type OnConnectEnd } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import BlockNodeView from './BlockNode.svelte';
  import TypedEdge from './TypedEdge.svelte';
  import Palette from './Palette.svelte';
  import Inspector from './Inspector.svelte';
  import TopBar from './TopBar.svelte';
  import StartPage from './StartPage.svelte';
  import FileDialog from './FileDialog.svelte';
  import { emptyGraph, fetchBlocks, fetchExample, openFile, parseToml, renderToml, saveFile, type BlockDescriptor, type GraphSpec } from './api';
  import { ID_RE, baseForCopy, baseForKind, edgeId, freshId, fromSpec, rejectReason, toSpec, type BlockNode, type Wire } from './graph';
  import { session } from './session.svelte';
  import { toast } from './toast.svelte';

  const flow = useSvelteFlow();
  const nodeTypes = { block: BlockNodeView };
  const edgeTypes = { typed: TypedEdge };
  const EXT = '.fox.toml';

  let descriptors = $state<BlockDescriptor[]>([]);
  const catalog = $derived(new Map(descriptors.map((d) => [d.kind, d])));
  let loadError = $state<string | null>(null);

  let nodes = $state.raw<BlockNode[]>([]);
  let edges = $state.raw<Wire[]>([]);

  /** Normalized JSON of the graph as last exchanged with the engine. */
  let synced = '';
  /** Normalized JSON at the last open/save, for the unsaved-changes dot. */
  let saved = $state(JSON.stringify(emptyGraph()));
  /** Name to suggest in "Save as" for graphs that came from an example. */
  let suggestedName = $state('');
  let ready = $state(false);
  let helloSeen = false;
  let blankChosen = $state(false);
  let dialog = $state<null | 'open' | 'save'>(null);
  let search = $state('');
  let searchInput = $state<HTMLInputElement>();
  let fileInput = $state<HTMLInputElement>();
  let canvas = $state<HTMLElement>();
  let addCount = 0;

  const undoStack: string[] = [];
  let redoStack: string[] = [];
  let clipboard: GraphSpec | null = null;

  const specJson = $derived(JSON.stringify(toSpec(nodes, edges)));
  const dirty = $derived(specJson !== saved);
  const selectedNode = $derived(nodes.find((n) => n.selected));
  const selectedEdge = $derived(selectedNode ? undefined : edges.find((e) => e.selected));
  const showStart = $derived(ready && nodes.length === 0 && !blankChosen);
  const running = $derived(session.status.state === 'running');

  // ---- Engine sync --------------------------------------------------------

  let pendingGraph: Parameters<typeof onGraph> | null = null;

  fetchBlocks()
    .then((b) => {
      descriptors = b;
      if (pendingGraph) onGraph(...pendingGraph);
    })
    .catch((e) => (loadError = String(e.message ?? e)));

  session.start(onGraph);

  function selectedIds() {
    return new Set([...nodes.filter((n) => n.selected).map((n) => n.id), ...edges.filter((e) => e.selected).map((e) => e.id)]);
  }

  /** Replace the canvas with `g`. Returns its normalized JSON. */
  function load(g: GraphSpec, selected = selectedIds()): string {
    const r = fromSpec(g, catalog, selected);
    nodes = r.nodes;
    edges = r.edges;
    return JSON.stringify(toSpec(r.nodes, r.edges));
  }

  function onGraph(graph: GraphSpec, name: string | null, kind: 'hello' | 'remote') {
    if (!descriptors.length) {
      pendingGraph = [graph, name, kind];
      return;
    }
    if (kind === 'hello' && helloSeen && graph.block.length === 0 && nodes.length > 0) {
      // The engine restarted and lost the graph: give it ours back.
      synced = '';
      commit(false);
      return;
    }
    session.name = name;
    synced = load(graph);
    if (!helloSeen) {
      saved = synced;
      helloSeen = true;
      ready = true;
      fitSoon();
    }
  }

  /** Send local edits to the engine (debounced; also called before run/save). */
  function commit(history = true) {
    const j = specJson;
    if (j === synced || !ready) return;
    if (history) {
      undoStack.push(synced);
      if (undoStack.length > 200) undoStack.shift();
      redoStack = [];
    }
    synced = j;
    session.setGraph(JSON.parse(j), session.name);
  }

  $effect(() => {
    if (!ready || specJson === synced) return;
    const t = setTimeout(() => commit(), 150);
    return () => clearTimeout(t);
  });

  function restore(j: string) {
    synced = load(JSON.parse(j));
    session.setGraph(JSON.parse(synced), session.name);
  }

  function undo() {
    commit();
    const prev = undoStack.pop();
    if (prev === undefined) return;
    redoStack.push(synced);
    restore(prev);
  }

  function redo() {
    commit();
    const next = redoStack.pop();
    if (next === undefined) return;
    undoStack.push(synced);
    restore(next);
  }

  // Tell the user when a run fails, even if the failing block is off-screen.
  let lastState = session.status.state;
  $effect(() => {
    const st = session.status;
    if (st.state === 'error' && lastState !== 'error') {
      const e = st.errors[0];
      if (e) toast(e.block ? `${e.block}: ${e.message}` : e.message, 'error');
    }
    lastState = st.state;
  });

  // ---- Files ---------------------------------------------------------------

  function fitSoon(frames = 30) {
    // Nodes must be measured before the view can fit them.
    requestAnimationFrame(() => {
      const measured = flow.getNodes().every((n) => n.measured?.width);
      if (measured || frames <= 0) flow.fitView({ padding: '48px', maxZoom: 1.25, duration: 200 });
      else fitSoon(frames - 1);
    });
  }

  /** Load a whole new graph as a local, undoable edit. */
  function replaceGraph(g: GraphSpec, name: string | null) {
    commit();
    undoStack.push(synced);
    redoStack = [];
    synced = load(g, new Set());
    saved = synced;
    session.name = name;
    session.setGraph(JSON.parse(synced), name);
    fitSoon();
  }

  function confirmDiscard() {
    return !dirty || nodes.length === 0 || confirm('Discard unsaved changes to this graph?');
  }

  function newGraph() {
    if (!confirmDiscard()) return;
    replaceGraph(emptyGraph(), null);
    suggestedName = '';
    blankChosen = false;
  }

  async function loadExample(name: string) {
    if (!confirmDiscard()) return;
    try {
      replaceGraph(await fetchExample(name), null);
      suggestedName = `${name}${EXT}`;
    } catch (e) {
      toast(`Couldn't load the example: ${(e as Error).message}`, 'error');
    }
  }

  async function open(name: string) {
    dialog = null;
    if (!confirmDiscard()) return;
    try {
      replaceGraph(await openFile(name), name);
    } catch (e) {
      toast(`Couldn't open ${name}: ${(e as Error).message}`, 'error');
    }
  }

  async function save(as = false) {
    commit();
    if (!session.name || as) {
      dialog = 'save';
      return;
    }
    await saveAs(session.name);
  }

  async function saveAs(name: string) {
    dialog = null;
    const json = specJson;
    try {
      await saveFile(name, JSON.parse(json));
      saved = json;
      session.name = name;
      synced = json;
      session.setGraph(JSON.parse(json), name);
      toast(`Saved ${name}`);
    } catch (e) {
      toast(`Couldn't save ${name}: ${(e as Error).message}`, 'error');
    }
  }

  async function download() {
    try {
      const text = await renderToml(JSON.parse(specJson));
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([text], { type: 'application/toml' }));
      a.download = session.name ?? (suggestedName || `untitled${EXT}`);
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    } catch (e) {
      toast(`Couldn't export: ${(e as Error).message}`, 'error');
    }
  }

  function upload() {
    dialog = null;
    fileInput?.click();
  }

  async function onfile(e: Event & { currentTarget: HTMLInputElement }) {
    const file = e.currentTarget.files?.[0];
    e.currentTarget.value = '';
    if (!file || !confirmDiscard()) return;
    try {
      const g = await parseToml(await file.text());
      replaceGraph(g, null);
      suggestedName = file.name.endsWith(EXT) ? file.name : `${file.name.replace(/\.toml$/, '')}${EXT}`;
    } catch (err) {
      toast(`${file.name}: ${(err as Error).message}`, 'error');
    }
  }

  // ---- Editing -------------------------------------------------------------

  function deselected<T extends { selected?: boolean }>(xs: T[]): T[] {
    return xs.map((x) => (x.selected ? { ...x, selected: false } : x));
  }

  function viewCentre() {
    const r = canvas?.getBoundingClientRect();
    if (!r) return { x: 0, y: 0 };
    const p = flow.screenToFlowPosition({ x: r.left + r.width / 2, y: r.top + r.height / 2 });
    const k = addCount++ % 6;
    return { x: p.x - 85 + k * 24, y: p.y - 40 + k * 24 };
  }

  function addBlock(desc: BlockDescriptor, position = viewCentre()) {
    const id = freshId(baseForKind(desc.kind), new Set(nodes.map((n) => n.id)));
    const params = Object.fromEntries(desc.params.map((p) => [p.name, p.default]));
    nodes = [...deselected(nodes), { id, type: 'block', position, selected: true, data: { desc, params } }];
    edges = deselected(edges);
    blankChosen = true;
  }

  function setParam(id: string, name: string, value: unknown) {
    nodes = nodes.map((n) => (n.id === id ? { ...n, data: { ...n.data, params: { ...n.data.params, [name]: value } } } : n));
  }

  function rename(from: string, to: string): string | null {
    if (!ID_RE.test(to)) return 'Use letters, digits, `_` and `-`, starting with a letter.';
    if (nodes.some((n) => n.id === to)) return `There is already a block called \`${to}\`.`;
    nodes = nodes.map((n) => (n.id === from ? { ...n, id: to } : n));
    edges = edges.map((e) => {
      if (e.source !== from && e.target !== from) return e;
      const source = e.source === from ? to : e.source;
      const target = e.target === from ? to : e.target;
      return { ...e, source, target, id: edgeId(`${source}.${e.sourceHandle}`, `${target}.${e.targetHandle}`) };
    });
    return null;
  }

  function deleteSelection() {
    const gone = new Set(nodes.filter((n) => n.selected).map((n) => n.id));
    nodes = nodes.filter((n) => !gone.has(n.id));
    edges = edges.filter((e) => !e.selected && !gone.has(e.source) && !gone.has(e.target));
  }

  function copy(): boolean {
    const sel = nodes.filter((n) => n.selected);
    if (!sel.length) return false;
    const ids = new Set(sel.map((n) => n.id));
    clipboard = toSpec(sel, edges.filter((e) => ids.has(e.source) && ids.has(e.target)));
    return true;
  }

  function paste() {
    if (!clipboard) return;
    const taken = new Set(nodes.map((n) => n.id));
    const ids = new Map<string, string>();
    for (const b of clipboard.block) {
      const id = freshId(baseForCopy(b.id), taken);
      taken.add(id);
      ids.set(b.id, id);
    }
    const move = (ep: string) => {
      const dot = ep.lastIndexOf('.');
      return `${ids.get(ep.slice(0, dot))}${ep.slice(dot)}`;
    };
    // Shift the clipboard too, so repeated pastes cascade.
    clipboard = {
      block: clipboard.block.map((b) => ({ ...b, ui: { x: Number(b.ui?.x ?? 0) + 40, y: Number(b.ui?.y ?? 0) + 40 } })),
      connect: clipboard.connect,
    };
    const g: GraphSpec = {
      block: clipboard.block.map((b) => ({ ...b, id: ids.get(b.id)!, params: structuredClone(b.params) })),
      connect: clipboard.connect.map((c) => ({ from: move(c.from), to: move(c.to) })),
    };
    const r = fromSpec(g, catalog);
    const selected = (xs: { selected?: boolean }[]) => xs.forEach((x) => (x.selected = true));
    selected(r.nodes);
    selected(r.edges);
    nodes = [...deselected(nodes), ...r.nodes];
    edges = [...deselected(edges), ...r.edges];
    blankChosen = true;
  }

  // ---- Connections ---------------------------------------------------------

  const isValidConnection = (c: Wire | Connection) => rejectReason(c as Connection, nodes) === null;

  function onbeforeconnect(c: Connection): Wire {
    return { ...c, id: edgeId(`${c.source}.${c.sourceHandle}`, `${c.target}.${c.targetHandle}`), type: 'typed' };
  }

  function onconnect(c: Connection) {
    // An input takes one wire: the new one replaces the old.
    const id = edgeId(`${c.source}.${c.sourceHandle}`, `${c.target}.${c.targetHandle}`);
    edges = edges.filter((e) => e.id === id || e.target !== c.target || e.targetHandle !== c.targetHandle);
  }

  const onconnectend: OnConnectEnd = (_e, state) => {
    if (state.isValid !== false || !state.toHandle || !state.fromHandle) return;
    const [s, t] = state.fromHandle.type === 'source' ? [state.fromHandle, state.toHandle] : [state.toHandle, state.fromHandle];
    const reason = rejectReason({ source: s.nodeId, sourceHandle: s.id ?? null, target: t.nodeId, targetHandle: t.id ?? null }, nodes);
    if (reason) toast(reason, 'error');
  };

  function ondragover(e: DragEvent) {
    if (e.dataTransfer?.types.includes('application/x-foxygnal-block')) {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'copy';
    }
  }

  function ondrop(e: DragEvent) {
    const desc = catalog.get(e.dataTransfer?.getData('application/x-foxygnal-block') ?? '');
    if (!desc) return;
    e.preventDefault();
    const p = flow.screenToFlowPosition({ x: e.clientX, y: e.clientY });
    addBlock(desc, { x: p.x - 85, y: p.y - 15 });
  }

  // ---- Run & keyboard ------------------------------------------------------

  function run() {
    commit();
    session.run();
  }

  function toggleRun() {
    if (running) session.stop();
    else if (nodes.length) run();
  }

  function onkeydown(e: KeyboardEvent) {
    const mod = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    if (dialog) return;
    if (mod && key === 's') {
      e.preventDefault();
      save(e.shiftKey);
      return;
    }
    if (mod && key === 'o') {
      e.preventDefault();
      dialog = 'open';
      return;
    }
    const t = e.target as HTMLElement | null;
    if (t?.closest('input, textarea, select, [contenteditable="true"]')) return;
    if (e.key === '/') {
      e.preventDefault();
      searchInput?.focus();
    } else if (e.key === ' ' && !mod && !t?.closest('button, a')) {
      e.preventDefault();
      toggleRun();
    } else if (mod && key === 'z') {
      e.preventDefault();
      if (e.shiftKey) redo();
      else undo();
    } else if (mod && key === 'y') {
      e.preventDefault();
      redo();
    } else if (mod && key === 'c') {
      copy();
    } else if (mod && key === 'x') {
      if (copy()) deleteSelection();
    } else if (mod && key === 'v') {
      paste();
    } else if (!mod && key === 'f') {
      flow.fitView({ padding: '48px', maxZoom: 1.25, duration: 200 });
    }
  }

  const actions = [
    { label: 'New graph', run: newGraph },
    { label: 'Open…', keys: 'Ctrl O', run: () => (dialog = 'open') },
    { label: 'Save', keys: 'Ctrl S', run: () => save() },
    { label: 'Save as…', keys: 'Ctrl ⇧ S', run: () => save(true) },
    { label: 'Download .fox.toml', run: download },
    { label: 'Upload .fox.toml…', run: upload },
  ];
</script>

<svelte:window {onkeydown} onbeforeunload={(e) => { if (dirty && nodes.length) e.preventDefault(); }} />

<div class="shell">
  <div class="top">
    <TopBar {dirty} blocks={nodes.length} onrun={run} onstop={() => session.stop()} {actions} />
  </div>
  <Palette catalog={descriptors} {loadError} onadd={(d) => addBlock(d)} bind:search bind:input={searchInput} />
  <main class="canvas" bind:this={canvas} {ondragover} {ondrop}>
    <SvelteFlow
      bind:nodes
      bind:edges
      {nodeTypes}
      {edgeTypes}
      forceColorMode="dark"
      deleteKey={['Delete', 'Backspace']}
      defaultEdgeOptions={{ type: 'typed' }}
      {isValidConnection}
      {onbeforeconnect}
      {onconnect}
      {onconnectend}
      minZoom={0.2}
      maxZoom={2.5}
    >
      <Background gap={20} />
      <Controls showLock={false} />
    </SvelteFlow>
    {#if showStart}
      <StartPage onexample={loadExample} onfile={open} onblank={() => (blankChosen = true)} />
    {/if}
  </main>
  <Inspector node={selectedNode} edge={selectedEdge} {nodes} onparam={setParam} onrename={rename} ondelete={deleteSelection} />
</div>

<input bind:this={fileInput} type="file" accept=".toml,.fox.toml" hidden onchange={onfile} />

{#if dialog}
  <FileDialog
    mode={dialog}
    initialName={session.name ?? suggestedName}
    onopen={open}
    onsave={saveAs}
    onupload={upload}
    onclose={() => (dialog = null)}
  />
{/if}

<style>
  .shell {
    display: grid;
    grid-template: 'top top top' auto 'pal canvas insp' 1fr / 220px 1fr 300px;
    height: 100%;
  }
  .top {
    grid-area: top;
  }
  .shell > :global(.palette) {
    grid-area: pal;
  }
  .shell > :global(.inspector) {
    grid-area: insp;
  }
  .canvas {
    grid-area: canvas;
    position: relative;
    min-width: 0;
    min-height: 0;
  }
  @media (max-width: 900px) {
    .shell {
      grid-template: 'top' auto 'canvas' 1fr 'insp' minmax(0, 40%) / 1fr;
    }
    .shell > :global(.palette) {
      display: none;
    }
  }
</style>
