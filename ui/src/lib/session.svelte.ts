// The engine session over `/api/ws`: graph sync, run control, status.

import type { ClientCommand, GraphSpec, ServerEvent, Status } from './api';

export type GraphHandler = (graph: GraphSpec, name: string | null, kind: 'hello' | 'remote') => void;

export class Session {
  connected = $state(false);
  /** Ever connected: distinguishes "connecting" from "lost connection". */
  everConnected = $state(false);
  status = $state<Status>({ state: 'stopped', stale: false, errors: [] });
  /** The graph's file name; owned by the editor, shared with other tabs. */
  name = $state<string | null>(null);

  #client = 0;
  #ws: WebSocket | null = null;
  #retry = 500;
  #onGraph: GraphHandler = () => {};

  /** Connect and deliver graphs from the server to `onGraph`. */
  start(onGraph: GraphHandler) {
    this.#onGraph = onGraph;
    if (!this.#ws) this.#connect();
  }

  #connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/ws`);
    this.#ws = ws;
    ws.onopen = () => {
      this.connected = true;
      this.everConnected = true;
      this.#retry = 500;
    };
    ws.onmessage = (m) => this.#handle(JSON.parse(m.data) as ServerEvent);
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
        this.#onGraph(ev.graph, ev.name, 'hello');
        break;
      case 'graph':
        if (ev.from !== this.#client) this.#onGraph(ev.graph, ev.name, 'remote');
        break;
      case 'status': {
        const { op: _, ...status } = ev;
        this.status = status;
        break;
      }
    }
  }

  send(cmd: ClientCommand): boolean {
    if (this.#ws?.readyState !== WebSocket.OPEN) return false;
    this.#ws.send(JSON.stringify(cmd));
    return true;
  }

  setGraph(graph: GraphSpec, name?: string | null) {
    return this.send({ op: 'set_graph', graph, name });
  }
  run() {
    this.send({ op: 'run' });
  }
  stop() {
    this.send({ op: 'stop' });
  }
}

export const session = new Session();
