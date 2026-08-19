<script lang="ts">
  import type { Snippet } from 'svelte';
  import Spinner from './Spinner.svelte';

  let {
    loading = false,
    empty = false,
    emptyMessage = 'No items found',
    class: className = '',
    header,
    children,
  }: {
    loading?: boolean;
    empty?: boolean;
    emptyMessage?: string;
    class?: string;
    header?: Snippet;
    children?: Snippet;
  } = $props();
</script>

<div class="data-list {className}">
  {#if header}
    <div class="data-list__header">
      {@render header()}
    </div>
  {/if}

  {#if loading}
    <div class="data-list__loading">
      <Spinner size="lg" />
    </div>
  {:else if empty}
    <div class="data-list__empty">
      <p>{emptyMessage}</p>
    </div>
  {:else}
    <div class="data-list__body">
      {@render children?.()}
    </div>
  {/if}
</div>
