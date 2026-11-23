<script lang="ts">
  import { formatSi, parseSi } from '../si';

  let {
    value,
    unit,
    placeholder,
    label,
    allowEmpty = false,
    onchange,
  }: {
    value: number | null;
    unit?: string;
    placeholder?: string;
    label: string;
    /** An empty field means `null` (e.g. "guess the rate"). */
    allowEmpty?: boolean;
    onchange: (v: number | null) => void;
  } = $props();

  let bad = $state(false);
  const text = $derived(value === null ? '' : formatSi(value, unit));

  function commit(e: Event & { currentTarget: HTMLInputElement }) {
    const raw = e.currentTarget.value.trim();
    if (raw === '' && allowEmpty) {
      bad = false;
      onchange(null);
      return;
    }
    const v = parseSi(raw, unit);
    bad = v === null;
    if (v === null) e.currentTarget.value = text;
    else onchange(v);
  }
</script>

<input class="mono" class:bad type="text" value={text} {placeholder} aria-label={label} onchange={commit} />

<style>
  input {
    padding: 4px 7px;
    font-size: 12px;
  }
  .bad {
    border-color: var(--err-line);
  }
</style>
