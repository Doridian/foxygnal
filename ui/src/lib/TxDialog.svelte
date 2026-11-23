<script lang="ts">
  // Confirmation before running a graph that transmits. Arming lasts for
  // this one run; the engine refuses TX without it (and without --allow-tx).
  import type { TxInfo } from './api';
  import { formatWithUnit } from './si';

  let {
    blocks,
    allowTx,
    onarm,
    onclose,
  }: {
    blocks: TxInfo[];
    allowTx: boolean;
    onarm: () => void;
    onclose: () => void;
  } = $props();

  let dialog = $state<HTMLDialogElement>();
  let cancel = $state<HTMLButtonElement>();

  $effect(() => {
    dialog?.showModal();
    // Never the arm button by default: Enter or Space must not transmit.
    cancel?.focus();
  });

  const num = (v: number | undefined, unit: string) => (v === undefined ? 'not set' : formatWithUnit(v, unit));
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="tx-title" aria-describedby="tx-legal">
  <h2 id="tx-title"><span class="badge">TX</span> This graph transmits</h2>
  <table>
    <thead>
      <tr><th>Block</th><th>Device</th><th>Frequency</th><th>Rate</th><th>Gain</th></tr>
    </thead>
    <tbody>
      {#each blocks as b (b.block)}
        <tr>
          <td class="mono">{b.block}</td>
          <td class="mono">{b.device || 'not set'}</td>
          <td class="mono">{num(b.frequency, 'Hz')}</td>
          <td class="mono">{num(b.sample_rate, 'S/s')}</td>
          <td class="mono">{b.gain === undefined ? 'not set' : `${b.gain} dB`}</td>
        </tr>
      {/each}
    </tbody>
  </table>
  <p id="tx-legal" class="legal">
    Transmitting requires a licence for these frequencies and must be legal where you are. You are responsible for what you send. TX stays armed for this run only and is disarmed when it stops.
  </p>
  {#if !allowTx}
    <p class="blocked" role="alert">
      The engine was started without <code>--allow-tx</code>, so it can't transmit. Restart it with <code>foxygnal serve --allow-tx</code> to allow arming.
    </p>
  {/if}
  <div class="actions">
    <button bind:this={cancel} type="button" onclick={() => dialog?.close()}>Cancel</button>
    <button type="button" class="arm" disabled={!allowTx || blocks.length === 0} onclick={onarm}>Arm TX and run</button>
  </div>
</dialog>

<style>
  dialog {
    width: min(560px, calc(100vw - 32px));
    padding: 20px;
    background: var(--panel);
    color: var(--text);
    border: 1px solid var(--err-line);
    border-radius: 10px;
    box-shadow: 0 20px 48px rgba(0, 0, 0, 0.6);
  }
  dialog::backdrop {
    background: rgba(0, 0, 0, 0.55);
  }
  h2 {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 0 0 14px;
    font-size: 16px;
  }
  .badge {
    padding: 2px 8px;
    border-radius: 4px;
    background: #d1242f;
    color: #fff;
    font-weight: 700;
    font-size: 12px;
    letter-spacing: 0.08em;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  th {
    text-align: left;
    font-weight: 500;
    color: var(--muted);
    padding: 4px 8px 6px 0;
    border-bottom: 1px solid var(--line);
  }
  td {
    padding: 6px 8px 6px 0;
    border-bottom: 1px solid var(--line);
    overflow-wrap: anywhere;
  }
  .legal {
    margin: 14px 0 0;
    font-size: 12px;
    line-height: 1.5;
    color: var(--warn);
  }
  .blocked {
    margin: 10px 0 0;
    padding: 8px 10px;
    border-radius: 6px;
    background: var(--err-bg);
    color: var(--err);
    font-size: 12px;
    line-height: 1.5;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 18px;
  }
  .arm {
    padding: 7px 12px;
    border-radius: 6px;
    border: 1px solid #d1242f;
    background: #d1242f;
    color: #fff;
    font-weight: 600;
  }
  .arm:disabled {
    opacity: 0.45;
    cursor: default;
  }
</style>
