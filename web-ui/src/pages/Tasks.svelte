<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import Modal from '../components/ui/Modal.svelte';
  import Alert from '../components/ui/Alert.svelte';
  import Spinner from '../components/ui/Spinner.svelte';
  import Badge from '../components/ui/Badge.svelte';
  import Icon from '../components/ui/Icon.svelte';

  const STATUSES = ['PENDING', 'IN_WORK', 'COMPLETED', 'CANCELLED'];
  const STATUS_COLORS: Record<string, string> = {
    PENDING: 'var(--text-tertiary)',
    IN_WORK: 'var(--accent-success)',
    COMPLETED: 'var(--accent-primary)',
    CANCELLED: 'var(--accent-error)',
  };

  interface Task {
    task_id: string;
    name: string;
    status?: string;
    project_id?: string | null;
  }
  interface Project { project_id: string; name: string; status?: string; }

  let tasks = $state<Task[]>([]);
  let projects = $state<Project[]>([]);
  let loading = $state(true);
  let error = $state('');
  let view = $state<'list' | 'kanban'>('list');
  let statusFilter = $state('');
  let projectFilter = $state('');

  let showCreate = $state(false);
  let newName = $state('');
  let newDesc = $state('');
  let newProject = $state('');

  let selected = $state<Task | null>(null);
  let selStatus = $state('PENDING');
  let selDesc = $state('');

  async function load() {
    try {
      loading = true;
      error = '';
      const params: any = { limit: 200 };
      if (statusFilter) params.status = statusFilter;
      if (projectFilter) params.project_id = projectFilter;
      const [t, p] = await Promise.all([api.tasks.list(params), api.projects.list()]);
      tasks = t?.tasks || [];
      projects = p?.projects || [];
    } catch (e: any) {
      error = e?.message || 'Failed to load tasks';
    } finally {
      loading = false;
    }
  }

  onMount(load);

  async function createTask() {
    if (!newName.trim()) return;
    try {
      await api.tasks.create({ name: newName.trim(), description: newDesc, project_id: newProject || undefined });
      showCreate = false;
      newName = ''; newDesc = ''; newProject = '';
      await load();
    } catch (e: any) {
      error = e?.message || 'Create failed';
    }
  }

  function openTask(t: Task) {
    selected = t;
    selStatus = t.status || 'PENDING';
    selDesc = '';
  }

  async function saveTask() {
    if (!selected) return;
    try {
      const patch: any = { status: selStatus };
      if (selDesc) patch.description = selDesc;
      await api.tasks.update(selected.task_id, patch);
      selected = null;
      await load();
    } catch (e: any) {
      error = e?.message || 'Update failed';
    }
  }

  async function setStatus(t: Task, status: string) {
    try {
      await api.tasks.update(t.task_id, { status });
      await load();
    } catch (e: any) {
      error = e?.message || 'Update failed';
    }
  }

  async function deleteTask(t: Task) {
    if (!confirm(`Удалить задачу «${t.name}»?`)) return;
    try {
      await api.tasks.remove(t.task_id);
      await load();
    } catch (e: any) {
      error = e?.message || 'Delete failed';
    }
  }

  let grouped = $derived({
    PENDING: tasks.filter(t => (t.status || 'PENDING') === 'PENDING'),
    IN_WORK: tasks.filter(t => t.status === 'IN_WORK'),
    COMPLETED: tasks.filter(t => t.status === 'COMPLETED'),
    CANCELLED: tasks.filter(t => t.status === 'CANCELLED'),
  });
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <div>
      <h1 class="text-2xl font-bold" style="color: var(--text-primary)">Tasks</h1>
      <p class="text-sm mt-1" style="color: var(--text-secondary)">{tasks.length} задач · {projects.length} проектов</p>
    </div>
    <div class="flex gap-2">
      <button onclick={() => view = view === 'list' ? 'kanban' : 'list'} class="btn btn--secondary btn--sm">
        {view === 'list' ? 'Kanban' : 'List'}
      </button>
      <button onclick={() => showCreate = true} class="btn btn--primary btn--sm">
        <Icon name="plus" size={14} /> New task
      </button>
    </div>
  </div>

  {#if error}
    <Alert type="error">{error}</Alert>
  {/if}

  <!-- Filters -->
  <div class="flex flex-wrap gap-3 items-center">
    <select bind:value={statusFilter} onchange={load} class="form-input" style="max-width: 180px">
      <option value="">All statuses</option>
      {#each STATUSES as s}
        <option value={s}>{s}</option>
      {/each}
    </select>
    <select bind:value={projectFilter} onchange={load} class="form-input" style="max-width: 220px">
      <option value="">All projects</option>
      {#each projects as p}
        <option value={p.project_id}>{p.name}</option>
      {/each}
    </select>
  </div>

  {#if loading}
    <div class="flex justify-center py-12"><Spinner size="lg" /></div>
  {:else if view === 'list'}
    <div class="space-y-2">
      {#each tasks as t (t.task_id)}
        <div class="flex items-center justify-between p-3 rounded-lg cursor-pointer" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)" onclick={() => openTask(t)}>
          <div class="flex items-center gap-3 min-w-0">
            <span class="w-2.5 h-2.5 rounded-full shrink-0" style="background: {STATUS_COLORS[t.status || 'PENDING'] || 'var(--text-tertiary)'}"></span>
            <div class="min-w-0">
              <div class="text-sm font-medium truncate" style="color: var(--text-primary)">{t.name}</div>
              <div class="text-xs" style="color: var(--text-tertiary)">{t.task_id}{t.project_id ? ` · ${t.project_id}` : ''}</div>
            </div>
          </div>
          <div class="flex items-center gap-2 shrink-0">
            {#if t.status}
              <Badge>{t.status}</Badge>
            {/if}
            <button onclick={(e) => { e.stopPropagation(); deleteTask(t); }} class="text-xs px-2 py-1 rounded hover:opacity-70 cursor-pointer" style="color: var(--accent-error)">del</button>
          </div>
        </div>
      {/each}
      {#if tasks.length === 0}
        <p class="text-sm py-10 text-center" style="color: var(--text-tertiary)">Нет задач</p>
      {/if}
    </div>
  {:else}
    <div class="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-4 gap-4">
      {#each STATUSES as s}
        <div class="rounded-xl p-3 space-y-2" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
          <div class="form-label" style="color: {STATUS_COLORS[s]}">{s} ({grouped[s].length})</div>
          {#each grouped[s] as t (t.task_id)}
            <div class="p-2.5 rounded-lg cursor-pointer" style="background: var(--bg-tertiary)" onclick={() => openTask(t)}>
              <div class="text-xs font-medium" style="color: var(--text-primary)">{t.name}</div>
              <div class="text-[10px] mt-0.5" style="color: var(--text-tertiary)">{t.project_id || t.task_id}</div>
            </div>
          {/each}
        </div>
      {/each}
    </div>
  {/if}
</div>

{#if selected}
  <Modal title={selected.name} onclose={() => selected = null}>
    <div class="text-xs mb-3" style="color: var(--text-tertiary)">{selected.task_id}</div>
    <select bind:value={selStatus} class="form-input w-full">
      {#each STATUSES as s}
        <option value={s}>{s}</option>
      {/each}
    </select>
    <textarea bind:value={selDesc} rows={4} placeholder="обновить описание (необязательно)" class="form-input w-full mt-2"></textarea>
    <div class="flex gap-2 mt-3">
      <button onclick={saveTask} class="btn btn--primary btn--sm">Save</button>
      <button onclick={() => selected = null} class="btn btn--secondary btn--sm">Cancel</button>
    </div>
  </Modal>
{/if}

{#if showCreate}
  <Modal title="New task" onclose={() => showCreate = false}>
    <input bind:value={newName} placeholder="name" class="form-input w-full" />
    <textarea bind:value={newDesc} rows={4} placeholder="description (markdown)" class="form-input w-full mt-2"></textarea>
    <select bind:value={newProject} class="form-input w-full mt-2">
      <option value="">Без проекта</option>
      {#each projects as p}
        <option value={p.project_id}>{p.name}</option>
      {/each}
    </select>
    <div class="flex gap-2 mt-3">
      <button onclick={createTask} class="btn btn--primary btn--sm">Create</button>
      <button onclick={() => showCreate = false} class="btn btn--secondary btn--sm">Cancel</button>
    </div>
  </Modal>
{/if}
