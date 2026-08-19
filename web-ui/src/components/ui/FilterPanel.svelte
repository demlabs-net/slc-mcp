<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    title = 'Filters',
    class: className = '',
    children,
  }: {
    title?: string;
    class?: string;
    children?: Snippet;
  } = $props();
</script>

<div class="filter-panel {className}">
  <div class="filter-panel__title">{title}</div>
  {@render children?.()}
</div>
