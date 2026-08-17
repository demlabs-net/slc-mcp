<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    open = false,
    size = 'md',
    closeOnOverlay = true,
    onclose,
    children,
  }: {
    open?: boolean;
    size?: 'sm' | 'md' | 'lg' | 'xl' | '2xl' | '4xl' | 'full';
    closeOnOverlay?: boolean;
    onclose?: () => void;
    children?: Snippet;
  } = $props();

  let modalEl: HTMLDivElement | undefined = $state();

  function handleOverlayClick() {
    if (closeOnOverlay) onclose?.();
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      onclose?.();
      return;
    }
    // Focus trap
    if (e.key === 'Tab' && modalEl) {
      const focusable = modalEl.querySelectorAll<HTMLElement>(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
      );
      if (focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (e.shiftKey) {
        if (document.activeElement === first) {
          e.preventDefault();
          last.focus();
        }
      } else {
        if (document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }
    }
  }

  $effect(() => {
    if (open) {
      // Focus first focusable element when modal opens
      setTimeout(() => {
        if (modalEl) {
          const focusable = modalEl.querySelector<HTMLElement>(
            'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])'
          );
          focusable?.focus();
        }
      }, 50);
    }
  });
</script>

{#if open}
  <div class="modal-overlay" onclick={handleOverlayClick} onkeydown={handleKeydown} role="dialog" aria-modal="true" tabindex="-1" bind:this={modalEl}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="modal-panel modal-panel--{size}"
      role="document"
      onclick={(e) => e.stopPropagation()}
      onkeydown={(e) => e.stopPropagation()}
    >
      {@render children?.()}
    </div>
  </div>
{/if}
