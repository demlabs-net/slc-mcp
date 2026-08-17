<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import Modal from '../components/ui/Modal.svelte';
  import Alert from '../components/ui/Alert.svelte';
  import Spinner from '../components/ui/Spinner.svelte';
  import Badge from '../components/ui/Badge.svelte';
  import Icon from '../components/ui/Icon.svelte';

  interface Project {
    project_id: string;
    name: string;
    status?: string;
    description?: string;
  }

  let projects = $state<Project[]>([]);
  let loading = $state(true);
  let error = $state('');
  let showCreate = $state(false);
  let newName = $state('');
  let newDesc = $state('');
  let selected = $state<Project | null>(null);
  let selDesc = $state('');
  let tasksByProject = $state<any[]>([]);

  async function load() {
    try {
      loading = true;
      error = '';
      const r = await api.projects.list({ limit: 200 });
      projects = r?.projects || [];
    } catch (e: any) {
      error = e?.message || 'Failed to load projects';
    } finally {
      loading = false;
    }
  }

  onMount(load);

  async function createProject() {
    if (!newName.trim()) return;
    try {
      await api.projects.create({ name: newName.trim(), description: newDesc });
      showCreate = false;
      newName = ''; newDesc = '';
      await load();
    } catch (e: any) {
      error = e?.message || 'Create failed';
    }
  }

  async function openProject(p: Project) {
    selected = p;
    selDesc = p.description || '';
    try {
      const r = await api.tasks.list({ project_id: p.project_id, limit: 200 });
      tasksByProject = r?.tasks || [];
    } catch (e: any) {
      tasksByProject = [];
    }
  }

  async function saveProject() {
    if (!selected) return;
    try {
      await api.projects.update(selected.project_id, { description: selDesc });
      selected = null;
      await load();
    } catch (e: any) {
      error = e?.message || 'Update failed';
    }
  }

  async function deleteProject(p: Project) {
    if (!confirm(`Удалить проект «${p.name}» и связанные документы?`)) return;
    try {
      await api.projects.remove(p.project_id);
      await load();
    } catch (e: any) {
      error = e?.message || 'Delete failed';
    }
  }
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <div>
      <h1 class="text-2xl font-bold" style="color: var(--text-primary)">Projects</h1>
      <p class="text-sm mt-1" style="color: var(--text-secondary)">{projects.length} проектов</p>
    </div>
    <button onclick={() => showCreate = true} class="btn btn--primary btn--sm">
      <Icon name="plus" size={14} /> New project
    </button>
  </div>

  {#if error}
    <Alert type="error">{error}</Alert>
  {/if}

  {#if loading}
    <div class="flex justify-center py-12"><Spinner size="lg" /></div>
  {:else}
    <div class="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4">
      {#each projects as p (p.project_id)}
        <div class="rounded-xl p-4 cursor-pointer transition-all hover:scale-[1.01]" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)" onclick={() => openProject(p)}>
          <div class="flex items-start justify-between gap-2">
            <div class="text-sm font-semibold" style="color: var(--text-primary)">{p.name}</div>
            <Badge>{p.status || 'active'}</Badge>
          </div>
          <div class="text-xs mt-2 line-clamp-2" style="color: var(--text-tertiary)">{p.project_id}</div>
        </div>
      {/each}
      {#if projects.length === 0}
        <p class="text-sm py-10 text-center col-span-full" style="color: var(--text-tertiary)">Нет проектов</p>
      {/if}
    </div>
  {/if}
</div>

{#if selected}
  <Modal title={selected.name} onclose={() => selected = null}>
    <div class="text-xs mb-2" style="color: var(--text-tertiary)">{selected.project_id}</div>
    <textarea bind:value={selDesc} rows={6} placeholder="description" class="form-input w-full font-mono text-sm"></textarea>
    <div class="mt-3">
      <div class="form-label mb-1" style="color: var(--text-secondary)">Задачи проекта ({tasksByProject.length})</div>
      <div class="max-h-40 overflow-y-auto space-y-1">
        {#each tasksByProject as t}
          <div class="text-xs p-2 rounded" style="background: var(--bg-tertiary); color: var(--text-primary)">{t.name} <span style="color: var(--text-tertiary)">({t.status || '—'})</span></div>
        {/each}
        {#if tasksByProject.length === 0}
          <div class="text-xs" style="color: var(--text-tertiary)">нет задач</div>
        {/if}
      </div>
    </div>
    <div class="flex gap-2 mt-3">
      <button onclick={saveProject} class="btn btn--primary btn--sm">Save</button>
      <button onclick={() => deleteProject(selected!)} class="btn btn--danger btn--sm">Delete</button>
      <button onclick={() => selected = null} class="btn btn--secondary btn--sm">Close</button>
    </div>
  </Modal>
{/if}

{#if showCreate}
  <Modal title="New project" onclose={() => showCreate = false}>
    <input bind:value={newName} placeholder="name" class="form-input w-full" />
    <textarea bind:value={newDesc} rows={4} placeholder="description" class="form-input w-full mt-2"></textarea>
    <div class="flex gap-2 mt-3">
      <button onclick={createProject} class="btn btn--primary btn--sm">Create</button>
      <button onclick={() => showCreate = false} class="btn btn--secondary btn--sm">Cancel</button>
    </div>
  </Modal>
{/if}
