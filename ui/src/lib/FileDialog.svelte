<script lang="ts">
  import { listFiles, type FileInfo } from './api';

  let {
    mode,
    initialName,
    onopen,
    onsave,
    onupload,
    onclose,
  }: {
    mode: 'open' | 'save';
    initialName: string;
    onopen: (name: string) => void;
    onsave: (name: string) => void;
    onupload: () => void;
    onclose: () => void;
  } = $props();

  let dialog = $state<HTMLDialogElement>();
  let files = $state<FileInfo[] | null>(null);
  let loadError = $state<string | null>(null);
  // svelte-ignore state_referenced_locally
  let name = $state(initialName);
  const EXT = '.fox.toml';

  $effect(() => {
    dialog?.showModal();
  });
  // GNU Radio `.grc` files can be opened (imported) but never saved over.
  // svelte-ignore state_referenced_locally
  const opening = mode === 'open';
  listFiles()
    .then((f) => (files = opening ? f : f.filter((x) => x.name.endsWith(EXT))))
    .catch((e) => (loadError = String(e.message ?? e)));

  const fullName = $derived(name.trim() && !name.trim().endsWith(EXT) ? `${name.trim()}${EXT}` : name.trim());
  const exists = $derived(files?.some((f) => f.name === fullName) ?? false);

  function submit(e: SubmitEvent) {
    e.preventDefault();
    if (fullName) onsave(fullName);
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="fd-title">
  <h2 id="fd-title">{mode === 'open' ? 'Open a graph' : 'Save graph as'}</h2>
  <p class="muted small">
    Files in the engine's project folder (<code>--project-dir</code>).{#if mode === 'open'}
      GNU Radio <code>.grc</code> files are converted when opened; save the result as <code>.fox.toml</code>.{/if}
  </p>
  {#if loadError}
    <p class="error">{loadError}</p>
  {:else if files === null}
    <p class="muted small">Loading…</p>
  {:else if files.length === 0}
    <p class="muted small">No <code>.fox.toml</code> files there yet.</p>
  {:else}
    <ul class="files">
      {#each files as f (f.name)}
        <li>
          <button class="mono" onclick={() => (mode === 'open' ? onopen(f.name) : (name = f.name))}>{f.name}</button>
        </li>
      {/each}
    </ul>
  {/if}
  {#if mode === 'save'}
    <form onsubmit={submit}>
      <label for="fd-name" class="small muted">File name</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input id="fd-name" class="mono" type="text" bind:value={name} placeholder="my-receiver.fox.toml" autofocus />
      {#if exists}<p class="warn small">{fullName} exists and will be overwritten.</p>{/if}
      <div class="actions">
        <button type="button" onclick={() => dialog?.close()}>Cancel</button>
        <button type="submit" class="primary" disabled={!fullName}>Save</button>
      </div>
    </form>
  {:else}
    <div class="actions">
      <button type="button" onclick={onupload}>Upload from this computer…</button>
      <button type="button" onclick={() => dialog?.close()}>Cancel</button>
    </div>
  {/if}
</dialog>

<style>
  dialog {
    width: min(460px, calc(100vw - 32px));
    padding: 20px;
    background: var(--panel);
    color: var(--text);
    border: 1px solid var(--line);
    border-radius: 10px;
    box-shadow: 0 20px 48px rgba(0, 0, 0, 0.6);
  }
  dialog::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }
  h2 {
    margin: 0 0 4px;
    font-size: 16px;
  }
  .small {
    font-size: 12px;
  }
  .files {
    list-style: none;
    margin: 12px 0;
    padding: 0;
    max-height: 260px;
    overflow: auto;
    border: 1px solid var(--line);
    border-radius: 6px;
  }
  .files button {
    width: 100%;
    padding: 8px 10px;
    text-align: left;
    background: transparent;
    border: 0;
    border-bottom: 1px solid var(--line);
    color: var(--text);
    font-size: 12px;
  }
  .files li:last-child button {
    border-bottom: 0;
  }
  .files button:hover,
  .files button:focus-visible {
    background: var(--panel-2);
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 12px;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .warn {
    margin: 0;
    color: var(--warn);
  }
  .error {
    color: var(--err);
  }
</style>
