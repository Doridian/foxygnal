<script lang="ts">
  import { SvelteFlowProvider } from '@xyflow/svelte';
  import Editor from './lib/Editor.svelte';
  import Toasts from './lib/Toasts.svelte';
</script>

<SvelteFlowProvider>
  <Editor />
</SvelteFlowProvider>
<Toasts />
