<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    padded = false,
    class: className = '',
    header,
    children,
  }: {
    padded?: boolean;
    class?: string;
    header?: Snippet;
    children?: Snippet;
  } = $props();
</script>

<div class="{padded ? 'card--padded' : 'card'} {className}">
  {#if header}
    <div class="card__header">
      {@render header()}
    </div>
  {/if}
  {@render children?.()}
</div>
