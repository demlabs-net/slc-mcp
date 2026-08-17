<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { marked } from 'marked';
  import DOMPurify from 'dompurify';
  import Modal from '../components/ui/Modal.svelte';
  import Alert from '../components/ui/Alert.svelte';
  import Spinner from '../components/ui/Spinner.svelte';
  import Badge from '../components/ui/Badge.svelte';
  import Icon from '../components/ui/Icon.svelte';

  const CATEGORIES = ['core', 'module', 'task', 'project', 'code_snippet', 'documentation', 'skill', 'custom', 'system'];

  interface Doc {
    document_id: string;
    category: string;
    folder?: string | null;
    tags: string[];
    seat_id?: string | null;
    updated_at: string;
    content_preview?: string;
    content?: string;
  }

  let docs = $state<Doc[]>([]);
  let loading = $state(true);
  let error = $state('');
  let category = $state('');
  let folder = $state('');
  let query = $state('');
  let searchQuery = $state('');
  let searchResults = $state<any[]>([]);
  let searching = $state(false);

  let selected = $state<Doc | null>(null);
  let fullDoc = $state<any>(null);
  let editing = $state(false);
  let editContent = $state('');
  let editTags = $state('');
  let saving = $state(false);

  let showCreate = $state(false);
  let newId = $state('');
  let newCategory = $state('documentation');
  let newContent = $state('');

  async function load() {
    try {
      loading = true;
      error = '';
      const params: any = { limit: 200 };
      if (category) params.category = category;
      if (folder) params.folder = folder;
      if (query) params.query = query;
      const r = await api.documents.list(params);
      docs = r?.documents || [];
    } catch (e: any) {
      error = e?.message || 'Failed to load documents';
    } finally {
      loading = false;
    }
  }

  onMount(load);

  async function doSearch() {
    if (!searchQuery.trim()) return;
    searching = true;
    try {
      const r = await api.search(searchQuery.trim(), 20);
      searchResults = r?.results || [];
    } catch (e: any) {
      error = e?.message || 'Search failed';
    } finally {
      searching = false;
    }
  }

  async function openDoc(doc: Doc) {
    selected = doc;
    fullDoc = null;
    try {
      const r = await api.documents.get(doc.document_id);
      fullDoc = r;
    } catch (e: any) {
      error = e?.message || 'Failed to load document';
    }
  }

  function renderMarkdown(text: string): string {
    const html = marked.parse(text || '', { async: false }) as string;
    return DOMPurify.sanitize(html);
  }

  function startEdit() {
    editing = true;
    editContent = fullDoc?.content || '';
    editTags = (fullDoc?.tags || []).join(', ');
  }

  async function saveEdit() {
    saving = true;
    try {
      await api.documents.update(fullDoc.document_id, {
        content: editContent,
        tags: editTags.split(',').map((t: string) => t.trim()).filter(Boolean),
      });
      editing = false;
      await openDoc(selected!);
      await load();
    } catch (e: any) {
      error = e?.message || 'Save failed';
    } finally {
      saving = false;
    }
  }

  async function deleteDoc(doc: Doc) {
    if (!confirm(`Удалить документ «${doc.document_id}»?`)) return;
    try {
      await api.documents.remove(doc.document_id);
      if (selected?.document_id === doc.document_id) selected = null;
      await load();
    } catch (e: any) {
      error = e?.message || 'Delete failed';
    }
  }

  async function createDoc() {
    if (!newId.trim()) return;
    try {
      await api.documents.create({ document_id: newId.trim(), category: newCategory, content: newContent });
      showCreate = false;
      newId = ''; newContent = '';
      await load();
    } catch (e: any) {
      error = e?.message || 'Create failed';
    }
  }
</script>

<div class="space-y-6">
  <div class="flex items-center justify-between">
    <div>
      <h1 class="text-2xl font-bold" style="color: var(--text-primary)">Knowledge Base</h1>
      <p class="text-sm mt-1" style="color: var(--text-secondary)">Документы, задачи, проекты, знания — единая база</p>
    </div>
    <button onclick={() => showCreate = true} class="btn btn--primary btn--sm">
      <Icon name="plus" size={14} /> New document
    </button>
  </div>

  {#if error}
    <Alert type="error">{error}</Alert>
  {/if}

  <!-- Filters -->
  <div class="flex flex-wrap gap-3 items-center">
    <select bind:value={category} onchange={load} class="form-input" style="max-width: 200px">
      <option value="">All categories</option>
      {#each CATEGORIES as c}
        <option value={c}>{c}</option>
      {/each}
    </select>
    <input bind:value={folder} placeholder="folder (docs/projects/slc)" class="form-input" style="max-width: 260px" onchange={load} />
    <input bind:value={query} placeholder="фильтр по id/тексту" class="form-input" style="max-width: 200px" onchange={load} />
    <button onclick={load} class="btn btn--secondary btn--sm">Apply</button>
  </div>

  <!-- Search -->
  <div class="flex gap-2">
    <input bind:value={searchQuery} placeholder="Гибридный поиск по базе…" class="form-input flex-1"
      onkeydown={(e) => { if (e.key === 'Enter') doSearch(); }} />
    <button onclick={doSearch} class="btn btn--primary btn--sm" disabled={searching}>
      {searching ? '…' : 'Search'}
    </button>
  </div>

  {#if searchQuery && searchResults.length > 0}
    <div class="rounded-xl p-4 space-y-2" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
      <div class="form-label" style="color: var(--text-secondary)">Search results ({searchResults.length})</div>
      {#each searchResults as r (r.document_id)}
        <div class="p-3 rounded-lg cursor-pointer hover:opacity-80" style="background: var(--bg-tertiary)" onclick={() => openDoc({ document_id: r.document_id, category: r.category, folder: r.folder, tags: [], updated_at: '' })}>
          <div class="text-sm font-medium" style="color: var(--text-primary)">{r.document_id} <span class="text-xs" style="color: var(--text-tertiary)">({r.category} · {Number(r.score).toFixed(3)})</span></div>
          <div class="text-xs mt-0.5 line-clamp-2" style="color: var(--text-secondary)">{String(r.content || '').slice(0, 200)}</div>
        </div>
      {/each}
    </div>
  {/if}

  <!-- List -->
  {#if loading}
    <div class="flex justify-center py-12"><Spinner size="lg" /></div>
  {:else if docs.length === 0}
    <p class="text-sm py-10 text-center" style="color: var(--text-tertiary)">Нет документов</p>
  {:else}
    <div class="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4">
      {#each docs as doc (doc.document_id)}
        <div class="rounded-xl p-4 cursor-pointer transition-all hover:scale-[1.01]" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)" onclick={() => openDoc(doc)}>
          <div class="flex items-start justify-between gap-2">
            <div class="text-sm font-semibold break-all" style="color: var(--text-primary)">{doc.document_id}</div>
            <Badge>{doc.category}</Badge>
          </div>
          {#if doc.folder}
            <div class="text-xs mt-1 truncate" style="color: var(--text-tertiary)">{doc.folder}</div>
          {/if}
          {#if doc.content_preview}
            <div class="text-xs mt-2 line-clamp-3" style="color: var(--text-secondary)">{doc.content_preview}</div>
          {/if}
          <div class="flex items-center justify-between mt-3">
            <div class="text-xs" style="color: var(--text-tertiary)">{String(doc.updated_at || '').slice(0, 10)}</div>
            {#if doc.tags?.length}
              <div class="flex gap-1 flex-wrap justify-end">
                {#each doc.tags.slice(0, 3) as t}
                  <span class="px-1.5 py-0.5 rounded text-[10px]" style="background: rgba(139,92,246,0.12); color: var(--accent-ai)">{t}</span>
                {/each}
              </div>
            {/if}
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>

{#if selected}
  <Modal title={selected.document_id} onclose={() => { selected = null; editing = false; }}>
    {#if fullDoc}
      {#if editing}
        <textarea bind:value={editContent} rows={16} class="form-input w-full font-mono text-sm"></textarea>
        <input bind:value={editTags} placeholder="tags (через запятую)" class="form-input mt-2 w-full" />
        <div class="flex gap-2 mt-3">
          <button onclick={saveEdit} class="btn btn--primary btn--sm" disabled={saving}>{saving ? 'Saving…' : 'Save'}</button>
          <button onclick={() => editing = false} class="btn btn--secondary btn--sm">Cancel</button>
        </div>
      {:else}
        <div class="flex gap-2 mb-3">
          <button onclick={startEdit} class="btn btn--secondary btn--sm">Edit</button>
          <button onclick={() => deleteDoc(selected!)} class="btn btn--danger btn--sm">Delete</button>
        </div>
        <div class="doc-viewer prose max-h-[60vh] overflow-y-auto" style="color: var(--text-primary)">
          {@html renderMarkdown(fullDoc.content || '')}
        </div>
      {/if}
    {:else}
      <div class="flex justify-center py-8"><Spinner /></div>
    {/if}
  </Modal>
{/if}

{#if showCreate}
  <Modal title="New document" onclose={() => showCreate = false}>
    <input bind:value={newId} placeholder="document_id (слаг, напр. my_note)" class="form-input w-full" />
    <select bind:value={newCategory} class="form-input w-full mt-2">
      {#each CATEGORIES as c}
        <option value={c}>{c}</option>
      {/each}
    </select>
    <textarea bind:value={newContent} rows={8} placeholder="markdown-контент" class="form-input w-full mt-2"></textarea>
    <div class="flex gap-2 mt-3">
      <button onclick={createDoc} class="btn btn--primary btn--sm">Create</button>
      <button onclick={() => showCreate = false} class="btn btn--secondary btn--sm">Cancel</button>
    </div>
  </Modal>
{/if}
