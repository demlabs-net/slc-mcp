<script lang="ts">
  let {
    value = 0,
    max = 100,
    color = 'blue',
    class: className = '',
  }: {
    value?: number;
    max?: number;
    color?: 'blue' | 'green' | 'red' | 'yellow';
    class?: string;
  } = $props();

  const pct = $derived(max > 0 ? Math.min(100, Math.round((value / max) * 100)) : 0);
</script>

<div class="progress-bar {className}" role="progressbar" aria-valuenow={value} aria-valuemin={0} aria-valuemax={max}>
  <div class="progress-bar__fill progress-bar__fill--{color}" style="width: {pct}%"></div>
</div>
