// Short-lived messages in the corner: rejected connections, save results, failures.

export type Toast = { id: number; message: string; kind: 'info' | 'error' };

export const toasts = $state<Toast[]>([]);
let next = 0;

export function toast(message: string, kind: Toast['kind'] = 'info') {
  const id = next++;
  toasts.push({ id, message, kind });
  setTimeout(() => dismiss(id), kind === 'error' ? 8000 : 4000);
}

export function dismiss(id: number) {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}
