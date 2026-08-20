<script lang="ts">
  // Admin: пользователи (RBAC) + правила OAuth-allowlist + статус Y360.
  // Порт auth-части легаси Admin.svelte / UserManagement / OAuthAccess.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { currentUser, isAdmin, isSuperAdmin } from '../lib/auth';
  import Alert from '../components/ui/Alert.svelte';
  import Badge from '../components/ui/Badge.svelte';

  const ALL_GROUPS = ['superadmins', 'admins', 'editors', 'users', 'viewers'];
  const RULE_TYPES = ['login', 'email', 'domain', 'ya360_org', 'ya360_group'];

  let users = $state<any[]>([]);
  let rules = $state<any[]>([]);
  let ya360 = $state<any>(null);
  let error = $state('');
  let notice = $state('');

  // Форма правила
  let ruleType = $state('login');
  let ruleValue = $state('');
  let ruleGroups = $state('users');
  let ruleDesc = $state('');

  // Назначение групп пользователю
  let pickUser = $state('');
  let pickGroups = $state<string[]>(['users']);

  let tab = $state('users');

  async function loadUsers() {
    try {
      const res = await api.auth.users.list();
      users = res?.users || [];
    } catch (e: any) { error = e?.message || 'Failed to load users'; }
  }

  async function loadRules() {
    try {
      const res = await api.admin.oauthRules.list();
      rules = res?.rules || [];
      const st = await api.admin.ya360Status();
      ya360 = st;
    } catch (e: any) { error = e?.message || 'Failed to load oauth rules'; }
  }

  onMount(() => {
    loadUsers();
    loadRules();
  });

  async function setGroups(userId: string, groups: string[]) {
    error = '';
    try {
      await api.auth.users.setGroups(userId, groups);
      await loadUsers();
    } catch (e: any) { error = e?.message || 'Failed to update groups'; }
  }

  async function toggleActive(userId: string, isActive: boolean) {
    error = '';
    try {
      await api.auth.users.setActive(userId, !isActive);
      await loadUsers();
    } catch (e: any) { error = e?.message || 'Failed to toggle user'; }
  }

  async function addRule() {
    error = '';
    notice = '';
    if (!ruleValue.trim()) { error = 'value required'; return; }
    try {
      await api.admin.oauthRules.create({
        type: ruleType,
        value: ruleValue.trim(),
        default_groups: [ruleGroups],
        description: ruleDesc,
      });
      ruleValue = '';
      ruleDesc = '';
      await loadRules();
    } catch (e: any) { error = e?.message || 'Failed to create rule'; }
  }

  async function deleteRule(ruleId: string) {
    error = '';
    try {
      await api.admin.oauthRules.remove(ruleId);
      await loadRules();
    } catch (e: any) { error = e?.message || 'Failed to delete rule'; }
  }

  async function applyGroups() {
    error = '';
    if (!pickUser) { error = 'выбери пользователя'; return; }
    if (!pickGroups.length) { error = 'выбери группу(ы)'; return; }
    await setGroups(pickUser, pickGroups);
  }

  const me = $derived($currentUser);
  const canAssignAdmin = $derived($isSuperAdmin);
</script>

<div class="space-y-6">
  <div class="flex items-center gap-3">
    <h1 class="text-xl font-bold" style="color: var(--text-primary)">Admin</h1>
    <div class="flex gap-1">
      <button
        onclick={() => tab = 'users'}
        class="px-3 py-1.5 rounded-lg text-sm transition-colors"
        style={tab === 'users' ? 'background: var(--bg-tertiary); color: var(--text-primary)' : 'color: var(--text-secondary)'}
      >Users</button>
      <button
        onclick={() => tab = 'oauth'}
        class="px-3 py-1.5 rounded-lg text-sm transition-colors"
        style={tab === 'oauth' ? 'background: var(--bg-tertiary); color: var(--text-primary)' : 'color: var(--text-secondary)'}
      >OAuth access</button>
    </div>
  </div>

  {#if error}
    <Alert type="error">{error}</Alert>
  {/if}
  {#if notice}
    <Alert type="success">{notice}</Alert>
  {/if}

  {#if tab === 'users'}
    <div class="rounded-2xl border overflow-hidden" style="border-color: var(--border-subtle)">
      <table class="w-full text-sm">
        <thead>
          <tr style="background: var(--bg-tertiary)">
            <th class="text-left px-4 py-2.5 font-medium" style="color: var(--text-secondary)">Username</th>
            <th class="text-left px-4 py-2.5 font-medium" style="color: var(--text-secondary)">Email</th>
            <th class="text-left px-4 py-2.5 font-medium" style="color: var(--text-secondary)">Groups</th>
            <th class="text-left px-4 py-2.5 font-medium" style="color: var(--text-secondary)">Active</th>
            <th class="text-left px-4 py-2.5 font-medium" style="color: var(--text-secondary)">Last login</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {#each users as u (u.user_id)}
            <tr class="border-t" style="border-color: var(--border-subtle)">
              <td class="px-4 py-2.5" style="color: var(--text-primary)">
                {u.username}
                {#if me?.user_id === u.user_id}<Badge>you</Badge>{/if}
              </td>
              <td class="px-4 py-2.5" style="color: var(--text-secondary)">{u.email}</td>
              <td class="px-4 py-2.5">
                <div class="flex flex-wrap gap-1">
                  {#each u.groups as g}
                    <Badge>{g}</Badge>
                  {/each}
                </div>
              </td>
              <td class="px-4 py-2.5">
                <button
                  onclick={() => toggleActive(u.user_id, u.is_active)}
                  class="px-2 py-1 rounded-md text-xs cursor-pointer"
                  style={u.is_active
                    ? 'background: rgba(34,197,94,0.15); color: #22c55e'
                    : 'background: rgba(239,68,68,0.15); color: #ef4444'}
                >
                  {u.is_active ? 'active' : 'disabled'}
                </button>
              </td>
              <td class="px-4 py-2.5 text-xs" style="color: var(--text-secondary)">
                {u.last_login ? String(u.last_login).slice(0, 19).replace('T', ' ') : '—'}
              </td>
              <td class="px-4 py-2.5">
                {#if u.groups.length !== 1 || u.groups[0] !== 'users'}
                  <button
                    onclick={() => setGroups(u.user_id, ['users'])}
                    class="text-xs cursor-pointer hover:underline" style="color: var(--text-secondary)"
                  >reset to users</button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if users.length === 0}
        <div class="px-4 py-6 text-sm" style="color: var(--text-secondary)">Нет пользователей</div>
      {/if}
    </div>

    <div class="rounded-2xl border p-4" style="border-color: var(--border-subtle)">
      <h3 class="text-sm font-semibold mb-2" style="color: var(--text-primary)">Назначить группы</h3>
      <div class="flex items-end gap-3 flex-wrap">
        <div>
          <label class="block text-xs mb-1" style="color: var(--text-secondary)">User</label>
          <select bind:value={pickUser} class="px-3 py-2 rounded-lg text-sm" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)">
            <option value="">—</option>
            {#each users as u}
              <option value={u.user_id}>{u.username}</option>
            {/each}
          </select>
        </div>
        <div>
          <label class="block text-xs mb-1" style="color: var(--text-secondary)">Groups</label>
          <select multiple bind:value={pickGroups} class="px-3 py-2 rounded-lg text-sm h-24" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)">
            {#each ALL_GROUPS as g}
              <option value={g}>{g}</option>
            {/each}
          </select>
        </div>
        <button
          onclick={applyGroups}
          class="px-4 py-2 rounded-lg text-sm" style="background: linear-gradient(135deg, var(--accent-ai), var(--accent-primary)); color: #fff"
        >Apply</button>
      </div>
      {#if !canAssignAdmin}
        <p class="text-xs mt-2" style="color: var(--text-secondary)">
          Только superadmin может назначать admins/superadmins.
        </p>
      {/if}
    </div>
  {:else}
    {#if ya360}
      <div class="rounded-2xl border p-4 flex items-center gap-4" style="border-color: var(--border-subtle)">
        <div>
          <div class="text-sm font-medium" style="color: var(--text-primary)">Yandex 360 Directory</div>
          <div class="text-xs mt-0.5" style="color: var(--text-secondary)">
            auth_method: {ya360.auth_method || 'none'} · org_id: {ya360.org_id || '—'} · admin_token: {ya360.has_token ? '✓' : '✗'}
          </div>
        </div>
        <Badge>{ya360.configured ? 'configured' : 'not configured'}</Badge>
      </div>
    {/if}

    <div class="rounded-2xl border p-4" style="border-color: var(--border-subtle)">
      <h3 class="text-sm font-semibold mb-1" style="color: var(--text-primary)">Правила доступа OAuth</h3>
      <p class="text-xs mb-3" style="color: var(--text-secondary)">
        При наличии правил env-переменные (YANDEX_ALLOWED_USERS) игнорируются. Типы: login, email, domain, ya360_org, ya360_group.
      </p>
      <div class="grid grid-cols-1 md:grid-cols-5 gap-2 mb-3">
        <select bind:value={ruleType} class="px-3 py-2 rounded-lg text-sm" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)">
          {#each RULE_TYPES as t}
            <option value={t}>{t}</option>
          {/each}
        </select>
        <input
          bind:value={ruleValue}
          placeholder="value (login/email/domain)"
          class="px-3 py-2 rounded-lg text-sm md:col-span-2" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)"
        />
        <select bind:value={ruleGroups} class="px-3 py-2 rounded-lg text-sm" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)">
          {#each ALL_GROUPS as g}
            <option value={g}>{g}</option>
          {/each}
        </select>
        <button
          onclick={addRule}
          class="px-4 py-2 rounded-lg text-sm" style="background: linear-gradient(135deg, var(--accent-ai), var(--accent-primary)); color: #fff"
        >Add rule</button>
      </div>

      <div class="space-y-2">
        {#each rules as r (r.rule_id)}
          <div class="flex items-center justify-between gap-3 px-3 py-2 rounded-lg text-sm" style="background: var(--bg-tertiary)">
            <div class="flex items-center gap-2 flex-wrap">
              <Badge>{r.type}</Badge>
              <span style="color: var(--text-primary)">{r.value}</span>
              <span class="text-xs" style="color: var(--text-secondary)">→ {r.default_groups?.join(', ') || 'users'}</span>
              {#if r.description}
                <span class="text-xs" style="color: var(--text-secondary)">— {r.description}</span>
              {/if}
            </div>
            <button
              onclick={() => deleteRule(r.rule_id)}
              class="text-xs cursor-pointer hover:underline" style="color: #ef4444"
            >delete</button>
          </div>
        {/each}
        {#if rules.length === 0}
          <div class="text-sm" style="color: var(--text-secondary)">Правил нет — действует env-allowlist (YANDEX_ALLOWED_USERS).</div>
        {/if}
      </div>
    </div>
  {/if}
</div>
