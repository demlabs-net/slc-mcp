<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import Alert from '../components/ui/Alert.svelte';
  import Spinner from '../components/ui/Spinner.svelte';
  import Badge from '../components/ui/Badge.svelte';

  interface Seat {
    seat_id: string;
    name?: string;
    status?: string;
    created_at?: string;
    last_accessed?: string;
    active_document_id?: string | null;
    usage_stats?: { total_requests?: number; total_tokens?: number; tools_used?: Record<string, number> };
  }

  let seats = $state<Seat[]>([]);
  let loading = $state(true);
  let error = $state('');
  let selected = $state<Seat | null>(null);

  async function load() {
    try {
      loading = true;
      error = '';
      const r = await api.seats.list();
      seats = r?.seats || [];
    } catch (e: any) {
      error = e?.message || 'Failed to load seats';
    } finally {
      loading = false;
    }
  }

  onMount(load);
</script>

<div class="space-y-6">
  <div>
    <h1 class="text-2xl font-bold" style="color: var(--text-primary)">Seats</h1>
    <p class="text-sm mt-1" style="color: var(--text-secondary)">{seats.length} активных сидов</p>
  </div>

  {#if error}
    <Alert type="error">{error}</Alert>
  {/if}

  {#if loading}
    <div class="flex justify-center py-12"><Spinner size="lg" /></div>
  {:else}
    <div class="space-y-2">
      {#each seats as s (s.seat_id)}
        <div class="flex items-center justify-between p-3 rounded-lg cursor-pointer" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)" onclick={() => selected = s}>
          <div class="min-w-0">
            <div class="text-sm font-medium truncate" style="color: var(--text-primary)">{s.name || s.seat_id}</div>
            <div class="text-xs truncate" style="color: var(--text-tertiary)">{s.seat_id}{s.active_document_id ? ` · активный: ${s.active_document_id}` : ''}</div>
          </div>
          <div class="flex items-center gap-3 shrink-0">
            <span class="text-xs" style="color: var(--text-tertiary)">{s.usage_stats?.total_requests || 0} req</span>
            <Badge>{s.status || 'active'}</Badge>
          </div>
        </div>
      {/each}
      {#if seats.length === 0}
        <p class="text-sm py-10 text-center" style="color: var(--text-tertiary)">Нет сидов</p>
      {/if}
    </div>
  {/if}
</div>

{#if selected}
  <div class="fixed inset-0 z-50 flex items-center justify-center p-4" style="background: rgba(0,0,0,0.5)" onclick={() => selected = null}>
    <div class="rounded-xl p-6 max-w-lg w-full max-h-[80vh] overflow-y-auto" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)" onclick={(e) => e.stopPropagation()}>
      <div class="text-sm font-semibold mb-1" style="color: var(--text-primary)">{selected.name || selected.seat_id}</div>
      <div class="text-xs mb-4 break-all" style="color: var(--text-tertiary)">{selected.seat_id}</div>
      <div class="space-y-2 text-sm">
        <div class="flex justify-between"><span style="color: var(--text-secondary)">Статус</span><span>{selected.status}</span></div>
        <div class="flex justify-between"><span style="color: var(--text-secondary)">Создан</span><span>{String(selected.created_at || '').slice(0, 19).replace('T', ' ')}</span></div>
        <div class="flex justify-between"><span style="color: var(--text-secondary)">Активность</span><span>{String(selected.last_accessed || '').slice(0, 19).replace('T', ' ')}</span></div>
        <div class="flex justify-between"><span style="color: var(--text-secondary)">Активный документ</span><span class="break-all">{selected.active_document_id || '—'}</span></div>
        <div class="flex justify-between"><span style="color: var(--text-secondary)">Запросов</span><span>{selected.usage_stats?.total_requests || 0}</span></div>
      </div>
      {#if selected.usage_stats?.tools_used && Object.keys(selected.usage_stats.tools_used).length > 0}
        <div class="mt-4">
          <div class="form-label mb-1" style="color: var(--text-secondary)">Инструменты</div>
          <div class="flex flex-wrap gap-1">
            {#each Object.entries(selected.usage_stats.tools_used) as [tool, count]}
              <span class="px-2 py-0.5 rounded text-xs" style="background: var(--bg-tertiary); color: var(--text-secondary)">{tool}: {count}</span>
            {/each}
          </div>
        </div>
      {/if}
      <button onclick={() => selected = null} class="btn btn--secondary btn--sm mt-5">Close</button>
    </div>
  </div>
{/if}
