<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { api, getSeat } from '../lib/api';
  import { tokenForStream } from '../lib/auth';
  import Alert from '../components/ui/Alert.svelte';
  import Spinner from '../components/ui/Spinner.svelte';
  import Badge from '../components/ui/Badge.svelte';

  let ctx = $state<any>(null);
  let notifications = $state<any[]>([]);
  let reminders = $state<any[]>([]);
  let focuses = $state<any[]>([]);
  let loading = $state(true);
  let error = $state('');
  let sseLog = $state<string[]>([]);

  let newReminder = $state('');
  let newRemindAt = $state('');
  let newFocus = $state('');

  async function load() {
    try {
      error = '';
      const [c, n, r, f] = await Promise.all([
        api.context(),
        api.notifications.list(),
        api.reminders.list(),
        api.focuses.list(),
      ]);
      ctx = c;
      notifications = n?.notifications || [];
      reminders = r?.reminders || [];
      focuses = f?.focuses || [];
    } catch (e: any) {
      error = e?.message || 'Failed to load context';
    } finally {
      loading = false;
    }
  }

  let es: EventSource | null = null;

  onMount(() => {
    load();
    const interval = setInterval(load, 30000);
    // SSE notifications
    try {
      const token = tokenForStream();
      es = new EventSource(
        `/api/events?seat=${encodeURIComponent(getSeat())}${token ? `&token=${encodeURIComponent(token)}` : ''}`
      );
      es.onmessage = (ev) => {
        sseLog = [...sseLog.slice(-19), ev.data.slice(0, 200)];
      };
      es.onerror = () => { /* переподключение автоматическое */ };
    } catch {
      /* no SSE */
    }
    return () => { clearInterval(interval); es?.close(); };
  });

  async function popNotifications() {
    try {
      await api.notifications.pop();
      await load();
    } catch (e: any) {
      error = e?.message || 'Pop failed';
    }
  }

  async function addReminder() {
    if (!newReminder.trim() || !newRemindAt.trim()) return;
    try {
      await api.reminders.create({ content: newReminder.trim(), remind_at: newRemindAt.trim() });
      newReminder = ''; newRemindAt = '';
      await load();
    } catch (e: any) {
      error = e?.message || 'Reminder failed';
    }
  }

  async function cancelReminder(id: string) {
    try {
      await api.reminders.cancel(id);
      await load();
    } catch (e: any) {
      error = e?.message || 'Cancel failed';
    }
  }

  async function addFocus() {
    if (!newFocus.trim()) return;
    try {
      await api.focuses.add({ title: newFocus.trim() });
      newFocus = '';
      await load();
    } catch (e: any) {
      error = e?.message || 'Focus failed';
    }
  }

  async function removeFocus(id: string) {
    try {
      await api.focuses.remove(id);
      await load();
    } catch (e: any) {
      error = e?.message || 'Remove failed';
    }
  }
</script>

<div class="space-y-6">
  <div>
    <h1 class="text-2xl font-bold" style="color: var(--text-primary)">Context</h1>
    <p class="text-sm mt-1" style="color: var(--text-secondary)">Срез контекста сита, уведомления, напоминания, фокусы</p>
  </div>

  {#if error}
    <Alert type="error">{error}</Alert>
  {/if}

  {#if loading}
    <div class="flex justify-center py-12"><Spinner size="lg" /></div>
  {:else}
    <div class="grid grid-cols-1 lg:grid-cols-2 gap-6">
      <!-- Context slice -->
      <div class="rounded-xl p-5" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
        <h2 class="text-sm font-semibold mb-3" style="color: var(--text-primary)">Контекст ({ctx?.used_tokens ?? 0}/{ctx?.limit_tokens ?? '?'} токенов)</h2>
        <div class="space-y-2 text-sm">
          <div class="flex justify-between"><span style="color: var(--text-secondary)">Активный документ</span><span class="break-all">{ctx?.active_document || '—'}</span></div>
          <div class="flex justify-between"><span style="color: var(--text-secondary)">Проектов</span><span>{(ctx?.projects || []).length}</span></div>
          <div class="flex justify-between"><span style="color: var(--text-secondary)">Задач</span><span>{(ctx?.tasks || []).length}</span></div>
          {#if ctx?.warning}
            <Alert type="warning">{ctx.warning}</Alert>
          {/if}
        </div>
        {#if (ctx?.projects || []).length > 0}
          <div class="mt-3">
            <div class="form-label mb-1" style="color: var(--text-secondary)">Проекты</div>
            <div class="flex flex-wrap gap-1">
              {#each ctx.projects as p}
                <Badge>{p.name || p.document_id}</Badge>
              {/each}
            </div>
          </div>
        {/if}
      </div>

      <!-- Notifications -->
      <div class="rounded-xl p-5" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
        <div class="flex items-center justify-between mb-3">
          <h2 class="text-sm font-semibold" style="color: var(--text-primary)">Уведомления ({notifications.length})</h2>
          <button onclick={popNotifications} class="btn btn--secondary btn--sm">Pop</button>
        </div>
        <div class="space-y-2 max-h-64 overflow-y-auto">
          {#each notifications as n (n.notification_id)}
            <div class="p-2.5 rounded-lg" style="background: var(--bg-tertiary)">
              <div class="text-xs font-medium" style="color: var(--text-primary)">[{n.source}] {n.title}</div>
              {#if n.body}
                <div class="text-xs mt-0.5 line-clamp-2" style="color: var(--text-secondary)">{n.body}</div>
              {/if}
            </div>
          {/each}
          {#if notifications.length === 0}
            <p class="text-xs py-6 text-center" style="color: var(--text-tertiary)">Нет уведомлений</p>
          {/if}
        </div>
      </div>

      <!-- Reminders -->
      <div class="rounded-xl p-5" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
        <h2 class="text-sm font-semibold mb-3" style="color: var(--text-primary)">Напоминания</h2>
        <div class="flex gap-2 mb-3">
          <input bind:value={newReminder} placeholder="содержимое" class="form-input flex-1" />
          <input bind:value={newRemindAt} placeholder="2026-08-20T15:00:00Z" class="form-input" style="max-width: 180px" />
          <button onclick={addReminder} class="btn btn--primary btn--sm">Add</button>
        </div>
        <div class="space-y-1.5 max-h-48 overflow-y-auto">
          {#each reminders as r (r.reminder_id)}
            <div class="flex items-center justify-between p-2 rounded" style="background: var(--bg-tertiary)">
              <div class="text-xs min-w-0">
                <span style="color: var(--text-primary)">{r.content}</span>
                <span class="ml-2" style="color: var(--text-tertiary)">{String(r.remind_at || '').slice(0, 16).replace('T', ' ')} · {r.status}</span>
              </div>
              {#if r.status === 'pending'}
                <button onclick={() => cancelReminder(r.reminder_id)} class="text-xs shrink-0 cursor-pointer" style="color: var(--accent-error)">cancel</button>
              {/if}
            </div>
          {/each}
          {#if reminders.length === 0}
            <p class="text-xs py-4 text-center" style="color: var(--text-tertiary)">Нет напоминаний</p>
          {/if}
        </div>
      </div>

      <!-- Focuses -->
      <div class="rounded-xl p-5" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
        <h2 class="text-sm font-semibold mb-3" style="color: var(--text-primary)">Фокусы</h2>
        <div class="flex gap-2 mb-3">
          <input bind:value={newFocus} placeholder="новый фокус" class="form-input flex-1" onkeydown={(e) => { if (e.key === 'Enter') addFocus(); }} />
          <button onclick={addFocus} class="btn btn--primary btn--sm">Add</button>
        </div>
        <div class="space-y-1.5 max-h-48 overflow-y-auto">
          {#each focuses as f (f.focus_id)}
            <div class="flex items-center justify-between p-2 rounded" style="background: var(--bg-tertiary)">
              <div class="text-xs min-w-0">
                <span style="color: var(--text-primary)">{f.title}</span>
                {#if f.priority}<span class="ml-2 text-[10px] px-1.5 py-0.5 rounded" style="background: rgba(139,92,246,0.12); color: var(--accent-ai)">p{f.priority}</span>{/if}
                {#if f.description}<div class="text-[10px] line-clamp-1 mt-0.5" style="color: var(--text-tertiary)">{f.description}</div>{/if}
              </div>
              <button onclick={() => removeFocus(f.focus_id)} class="text-xs shrink-0 cursor-pointer" style="color: var(--accent-error)">✕</button>
            </div>
          {/each}
          {#if focuses.length === 0}
            <p class="text-xs py-4 text-center" style="color: var(--text-tertiary)">Нет фокусов</p>
          {/if}
        </div>
      </div>
    </div>

    <!-- SSE log -->
    <div class="rounded-xl p-5" style="background: var(--bg-secondary); border: 1px solid var(--border-subtle)">
      <h2 class="text-sm font-semibold mb-2" style="color: var(--text-primary)">SSE-события (live)</h2>
      <div class="text-xs font-mono space-y-1 max-h-40 overflow-y-auto" style="color: var(--text-tertiary)">
        {#each sseLog as line}
          <div class="truncate">{line}</div>
        {/each}
        {#if sseLog.length === 0}
          <div>— ожидание событий (keep-alive каждые 15с) —</div>
        {/if}
      </div>
    </div>
  {/if}
</div>
