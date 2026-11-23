// Short-lived messages in the corner: rejected connections, save results, failures.

/** A button on a toast. Clicking it runs `run` and dismisses the toast. */
export type ToastAction = { label: string; run: () => void };

export type Toast = { id: number; message: string; kind: 'info' | 'error'; actions: ToastAction[] };

export const toasts = $state<Toast[]>([]);
let next = 0;

export function toast(message: string, kind: Toast['kind'] = 'info', actions: ToastAction[] = []) {
  const id = next++;
  toasts.push({ id, message, kind, actions });
  // Leave time to reach for a button.
  setTimeout(() => dismiss(id), actions.length ? 12000 : kind === 'error' ? 8000 : 4000);
}

export function dismiss(id: number) {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}
