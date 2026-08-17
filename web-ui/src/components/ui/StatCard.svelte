<script lang="ts">
  let {
    label,
    value,
  }: {
    label: string;
    value: string | number;
  } = $props();
</script>

<div class="stat-card">
  <div class="stat-card__value">{value}</div>
  <div class="stat-card__label">{label}</div>
</div>
