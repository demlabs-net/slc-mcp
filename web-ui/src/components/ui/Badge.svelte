<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    color = 'gray',
    class: className = '',
    children,
  }: {
    color?: 'green' | 'red' | 'blue' | 'yellow' | 'gray' | 'purple';
    class?: string;
    children?: Snippet;
  } = $props();
</script>

<span class="badge badge--{color} {className}">
  {@render children?.()}
</span>
