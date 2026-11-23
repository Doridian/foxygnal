// The engine session over `/api/ws`: graph sync, run control, status, stats
// and probe frames (binary messages).

import type { ClientCommand, EdgeRef, ErrorInfo, GraphSpec, Meta, ProbeConfig, ServerEvent, Stats, Status, TxInfo } from './api';
import { graphView } from './graphview.svelte';
import { decodeFrame, type Frame } from './frames';

export type GraphHandler = (graph: GraphSpec, name: string | null, kind: 'hello' | 'remote') => void;
export type FrameHandler = (frame: Frame) => void;

interface ProbeEntry {
  cmd: Extract<ClientCommand, { op: 'probe_set' }>;
  onFrame: FrameHandler;
}

/** Stats indexed for the canvas: CPU by block id, wire stats by edge id. */
export interface StatsIndex {
  cpu: Map<string, number>;
  edges: Map<string, Stats['edges'][number]>;
}

export class Session {
  connected = $state(false);
  /** Ever connected: distinguishes "connecting" from "lost connection". */
  everConnected = $state(false);
  status = $state<Status>({ state: 'stopped', stale: false, errors: [] });
  /** The graph's file name; owned by the editor, shared with other tabs. */
  name = $state<string | null>(null);
  /** Load of the running graph; `null` when not running. */
  stats = $state<StatsIndex | null>(null);
  /** Why the graph doesn't build or configure, as of the last edit. */
  metaError = $state<ErrorInfo | null>(null);
  /** The engine was started with `--allow-tx`. */
  allowTx = $state(false);
  /** Blocks that would transmit if the graph ran, as of the last edit. */
  txBlocks = $state<TxInfo[]>([]);
  /** Why a probe can't show anything (by probe id), as the engine reports it. */
  probeErrors = $state<Record<number, string>>({});

  #client = 0;
  #ws: WebSocket | null = null;
  #retry = 500;
  #onGraph: GraphHandler = () => {};
  #probes = new Map<number, ProbeEntry>();

  /** Connect and deliver graphs from the server to `onGraph`. */
  start(onGraph: GraphHandler) {
    this.#onGraph = onGraph;
    if (!this.#ws) this.#connect();
  }

  #connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/ws`);
    ws.binaryType = 'arraybuffer';
    this.#ws = ws;
    ws.onopen = () => {
      this.connected = true;
      this.everConnected = true;
      this.#retry = 500;
      // Probes belong to the connection: set them up again.
      for (const p of this.#probes.values()) ws.send(JSON.stringify(p.cmd));
    };
    ws.onmessage = (m) => {
      if (m.data instanceof ArrayBuffer) this.#frame(m.data);
      else this.#handle(JSON.parse(m.data) as ServerEvent);
    };
    ws.onclose = () => {
      this.connected = false;
      this.#ws = null;
      setTimeout(() => this.#connect(), this.#retry);
      this.#retry = Math.min(this.#retry * 2, 5000);
    };
  }

  #handle(ev: ServerEvent) {
    switch (ev.op) {
      case 'hello':
        this.#client = ev.client;
        this.status = ev.status;
        this.allowTx = ev.allow_tx;
        this.#meta(ev.meta);
        this.#onGraph(ev.graph, ev.name, 'hello');
        break;
      case 'graph':
        if (ev.from !== this.#client) this.#onGraph(ev.graph, ev.name, 'remote');
        break;
      case 'status': {
        const { op: _, ...status } = ev;
        this.status = status;
        if (status.state !== 'running') this.stats = null;
        break;
      }
      case 'meta':
        this.#meta(ev);
        break;
      case 'stats':
        if (this.status.state !== 'running') break;
        this.stats = {
          cpu: new Map(ev.blocks.map((b) => [b.block, b.cpu])),
          edges: new Map(ev.edges.map((e) => [`${e.from}->${e.to}`, e])),
        };
        break;
      case 'probe': {
        const { [ev.id]: _, ...rest } = this.probeErrors;
        this.probeErrors = ev.error ? { ...rest, [ev.id]: ev.error } : rest;
        break;
      }
    }
  }

  #meta(m: Meta) {
    this.metaError = m.error ?? null;
    this.txBlocks = m.tx ?? [];
    graphView.meta = new Map(m.edges.map((e) => [e.from, { sample_rate: e.sample_rate, center_freq: e.center_freq }]));
  }

  #frame(buf: ArrayBuffer) {
    const f = decodeFrame(buf);
    if (f) this.#probes.get(f.id)?.onFrame(f.frame);
  }

  /** Watch a wire. Calling it again with the same id changes the probe. */
  setProbe(id: number, edge: EdgeRef, probe: ProbeConfig, onFrame: FrameHandler, fps = 30) {
    const cmd = { op: 'probe_set' as const, id, edge, probe, fps };
    const old = this.#probes.get(id);
    this.#probes.set(id, { cmd, onFrame });
    if (!old || JSON.stringify(old.cmd) !== JSON.stringify(cmd)) this.send(cmd);
  }

  removeProbe(id: number) {
    if (!this.#probes.delete(id)) return;
    this.send({ op: 'probe_remove', id });
    const { [id]: _, ...rest } = this.probeErrors;
    this.probeErrors = rest;
  }

  send(cmd: ClientCommand): boolean {
    if (this.#ws?.readyState !== WebSocket.OPEN) return false;
    this.#ws.send(JSON.stringify(cmd));
    return true;
  }

  setGraph(graph: GraphSpec, name?: string | null) {
    return this.send({ op: 'set_graph', graph, name });
  }
  /** Run the graph; `armTx` arms transmitting for this run only. */
  run(armTx = false) {
    this.send(armTx ? { op: 'run', arm_tx: true } : { op: 'run' });
  }
  stop() {
    this.send({ op: 'stop' });
  }
}

export const session = new Session();
