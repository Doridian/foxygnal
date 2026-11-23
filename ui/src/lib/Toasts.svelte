<script lang="ts">
  import { dismiss, toasts } from './toast.svelte';
</script>

<div class="toasts" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}" role={t.kind === 'error' ? 'alert' : 'status'}>
      <span>{t.message}</span>
      <button aria-label="Dismiss" onclick={() => dismiss(t.id)}>×</button>
    </div>
  {/each}
</div>

<style>
  .toasts {
    position: fixed;
    right: 16px;
    bottom: 16px;
    z-index: 50;
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-width: min(420px, calc(100vw - 32px));
  }
  .toast {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 8px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.5);
    font-size: 13px;
    line-height: 1.4;
  }
  .toast.error {
    border-color: var(--err-line);
    color: var(--err);
  }
  .toast span {
    flex: 1;
  }
  button {
    border: 0;
    background: transparent;
    color: var(--muted);
    font-size: 16px;
    line-height: 1;
    padding: 0 2px;
  }
</style>
