<script lang="ts">
  let {
    size = 'md',
    class: className = '',
  }: {
    size?: 'sm' | 'md' | 'lg';
    class?: string;
  } = $props();
</script>

<div class="spinner spinner--{size} {className}" role="status" aria-label="Loading">
  <span class="sr-only">Loading...</span>
</div>
