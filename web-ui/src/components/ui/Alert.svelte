<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    type = 'info',
    dismissible = false,
    ondismiss,
    children,
  }: {
    type?: 'error' | 'success' | 'info' | 'warning';
    dismissible?: boolean;
    ondismiss?: () => void;
    children?: Snippet;
  } = $props();
</script>

<div class="alert alert--{type}" role="alert">
  <div class="flex items-start justify-between gap-2">
    <div>{@render children?.()}</div>
    {#if dismissible}
      <button
        onclick={() => ondismiss?.()}
        class="shrink-0 text-current opacity-60 hover:opacity-100 cursor-pointer"
        aria-label="Dismiss"
      >
        <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
        </svg>
      </button>
    {/if}
  </div>
</div>
