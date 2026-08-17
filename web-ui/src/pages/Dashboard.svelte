<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import Alert from '../components/ui/Alert.svelte';
  import Spinner from '../components/ui/Spinner.svelte';

  interface Task {
    task_id: string;
    name?: string;
    status?: string;
    project_id?: string | null;
  }

  let stats = $state<any>({});
  let seats = $state<any[]>([]);
  let recentTasks = $state<Task[]>([]);
  let loading = $state(true);
  let loadError = $state('');

  async function loadData() {
    try {
      loadError = '';
      const [s, tasks, seatsData] = await Promise.all([
        api.stats(),
        api.tasks.list({ limit: 5 }),
        api.seats.list(),
      ]);
      stats = s;
      recentTasks = tasks?.tasks || [];
      seats = seatsData?.seats || [];
    } catch (error: any) {
      loadError = error?.message || 'Failed to load dashboard data';
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    loadData();
    const interval = setInterval(loadData, 10000);
    return () => clearInterval(interval);
  });

  function goTo(route: string) {
    window.location.hash = `#/${route}`;
  }

  let statItems = $derived([
    {
      label: 'Documents',
      value: stats?.total || 0,
      color: 'var(--accent-primary)',
      route: 'knowledge',
    },
    {
      label: 'Tasks',
      value: stats?.tasks || 0,
      color: 'var(--accent-ai)',
      route: 'tasks',
    },
    {
      label: 'Projects',
      value: stats?.projects || 0,
      color: 'var(--accent-success)',
      route: 'projects',
    },
    {
      label: 'Seats',
      value: seats.length || 0,
      color: 'var(--accent-warning)',
      route: 'seats',
    },
  ]);
</script>

<div class="space-y-8">
  <!-- Header -->
  <div>
    <h1 class="text-2xl font-bold" style="color: var(--text-primary)">Dashboard</h1>
    <p class="text-sm mt-1" style="color: var(--text-secondary)">База знаний SLC: документы, задачи, проекты, сиды</p>
  </div>

  {#if loadError}
    <div aria-live="polite">
      <Alert type="error">{loadError}</Alert>
    </div>
  {/if}

  <!-- Quick Actions -->
  <div class="flex gap-3">
    <button
      onclick={() => goTo('tasks')}
      class="flex items-center gap-2 px-4 py-2.5 rounded-lg text-sm font-medium transition-all cursor-pointer" style="background: var(--accent-ai); color: white"
      onmouseenter={(e) => e.currentTarget.style.background = 'var(--accent-ai-hover)'}
      onmouseleave={(e) => e.currentTarget.style.background = 'var(--accent-ai)'}
    >
      <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M12 4v16m8-8H4" /></svg>
      New Task
    </button>
    <button
      onclick={() => goTo('knowledge')}
      class="flex items-center gap-2 px-4 py-2.5 rounded-lg text-sm font-medium transition-all cursor-pointer" style="background: var(--bg-tertiary); color: var(--text-primary); border: 1px solid var(--border-subtle)"
      onmouseenter={(e) => e.currentTarget.style.borderColor = 'var(--border-default)'}
      onmouseleave={(e) => e.currentTarget.style.borderColor = 'var(--border-subtle)'}
    >
      <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-8l-4-4m0 0L8 8m4-4v12" /></svg>
      New Document
    </button>
  </div>

  <!-- Stats Row -->
  {#if loading}
    <div class="flex items-center justify-center py-12">
      <Spinner size="lg" />
    </div>
  {:else}
    <div class="grid grid-cols-2 lg:grid-cols-4 gap-4">
      {#each statItems as item (item.label)}
        <button
          class="rounded-xl p-5 cursor-pointer transition-all hover:scale-[1.02] text-left w-full" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)"
          onclick={() => goTo(item.route)}
        >
          <div class="form-label" style="color: var(--text-secondary)">{item.label}</div>
          <div class="text-3xl font-bold" style="color: {item.color}">{item.value}</div>
        </button>
      {/each}
    </div>
  {/if}

  <!-- Recent Tasks -->
  <div class="rounded-xl p-5" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
    <div class="flex items-center justify-between mb-4">
      <h2 class="text-sm font-semibold" style="color: var(--text-primary)">Recent Tasks</h2>
      <button
        onclick={() => goTo('tasks')}
        class="text-xs font-medium cursor-pointer" style="color: var(--accent-ai)"
      >
        View all →
      </button>
    </div>

    {#if recentTasks.length === 0}
      <p class="text-sm py-6 text-center" style="color: var(--text-tertiary)">No tasks yet</p>
    {:else}
      <div class="space-y-2">
        {#each recentTasks.slice(0, 5) as task (task.task_id)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div
            class="flex items-center justify-between p-3 rounded-lg cursor-pointer transition-colors" style="background: var(--bg-tertiary)"
            onmouseenter={(e) => e.currentTarget.style.background = 'var(--bg-elevated)'}
            onmouseleave={(e) => e.currentTarget.style.background = 'var(--bg-tertiary)'}
            onclick={() => goTo('tasks')}
          >
            <div class="flex items-center gap-3 min-w-0">
              <div
                class="w-2 h-2 rounded-full shrink-0" style="background: {task.status === 'IN_WORK' ? 'var(--accent-success)' : task.status === 'COMPLETED' ? 'var(--accent-primary)' : 'var(--text-tertiary)'}"
              ></div>
              <div class="min-w-0">
                <div class="text-sm font-medium truncate" style="color: var(--text-primary)">{task.name || 'Untitled'}</div>
                <div class="text-xs" style="color: var(--text-tertiary)">{task.status || 'PENDING'}{task.project_id ? ` · ${task.project_id}` : ''}</div>
              </div>
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>
