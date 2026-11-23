<script lang="ts">
  import { dismiss, toasts } from './toast.svelte';
</script>

<div class="toasts" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}" role={t.kind === 'error' ? 'alert' : 'status'}>
      <span>{t.message}</span>
      {#each t.actions as a (a.label)}
        <button
          class="action"
          onclick={() => {
            dismiss(t.id);
            a.run();
          }}>{a.label}</button
        >
      {/each}
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
  button.action {
    align-self: center;
    flex: none;
    padding: 4px 10px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--panel);
    color: var(--text);
    font-size: 12px;
    white-space: nowrap;
  }
  button.action:hover {
    border-color: var(--accent);
  }
  button:not(.action) {
    border: 0;
    background: transparent;
    color: var(--muted);
    font-size: 16px;
    line-height: 1;
    padding: 0 2px;
  }
</style>
