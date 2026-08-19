<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    class: className = '',
    children,
  }: {
    class?: string;
    children?: Snippet;
  } = $props();
</script>

<div class="stats-grid {className}">
  {@render children?.()}
</div>
