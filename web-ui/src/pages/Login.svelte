<script lang="ts">
  import { login } from '../lib/auth';
  import Alert from '../components/ui/Alert.svelte';

  let username = $state('');
  let password = $state('');
  let error = $state('');
  let loading = $state(false);

  // oauth_error из hash-фрагмента (#/login?oauth_error=exchange_failed)
  let oauthError = $state('');
  if (typeof window !== 'undefined') {
    const q = new URLSearchParams(window.location.hash.split('?')[1] || '');
    oauthError = q.get('oauth_error') || '';
    const denied = q.get('error');
    if (denied === 'access_denied') {
      const hint = q.get('hint') || '';
      oauthError = hint ? `Доступ отклонён (${hint}) — обратитесь к администратору` : 'Доступ отклонён';
    }
    if (oauthError) window.history.replaceState({}, '', window.location.pathname + window.location.hash.split('?')[0]);
  }

  async function handleLogin() {
    error = '';
    loading = true;
    const ok = await login(username, password);
    loading = false;
    if (!ok) {
      error = 'Invalid username or password';
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter') handleLogin();
  }
</script>

<div
  class="min-h-screen flex items-center justify-center px-4 relative overflow-hidden" style="background: var(--bg-primary)"
>
  <div
    class="absolute inset-0 opacity-30" style="background: radial-gradient(ellipse at 50% 50%, rgba(139, 92, 246, 0.15) 0%, rgba(59, 130, 246, 0.1) 40%, transparent 70%)"
  ></div>

  <div class="max-w-md w-full relative z-10">
    <div class="text-center mb-8">
      <div
        class="w-14 h-14 rounded-2xl flex items-center justify-center" style="color: var(--text-primary) text-xl font-bold mx-auto mb-4; background: linear-gradient(135deg, var(--accent-ai), var(--accent-primary)); box-shadow: 0 0 30px rgba(139, 92, 246, 0.3)"
      >
        S
      </div>
      <h1 class="text-2xl font-bold" style="color: var(--text-primary)">SLC</h1>
      <p class="mt-1 text-sm" style="color: var(--text-secondary)">Memory server</p>
    </div>

    <div
      class="rounded-2xl p-8 space-y-6" style="background: rgba(17, 17, 19, 0.8); backdrop-filter: blur(20px); border: 1px solid var(--border-subtle)"
    >
      <h2 class="text-lg font-semibold text-center" style="color: var(--text-primary)">Sign in</h2>

      {#if error || oauthError}
        <Alert type="error">{error || oauthError}</Alert>
      {/if}

      <div class="space-y-4">
        <div>
          <label for="username" class="block text-sm mb-1" style="color: var(--text-secondary)">Username</label>
          <input
            id="username"
            type="text"
            bind:value={username}
            onkeydown={handleKeydown}
            placeholder="Enter your username"
            class="w-full px-4 py-2.5 rounded-lg text-sm" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)"
            autocomplete="username"
          />
        </div>

        <div>
          <label for="password" class="block text-sm mb-1" style="color: var(--text-secondary)">Password</label>
          <input
            id="password"
            type="password"
            bind:value={password}
            onkeydown={handleKeydown}
            placeholder="Enter your password"
            class="w-full px-4 py-2.5 rounded-lg text-sm" style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)"
            autocomplete="current-password"
          />
        </div>

        <button
          onclick={handleLogin}
          disabled={loading}
          class="w-full px-4 py-2.5 rounded-lg text-sm font-medium transition-opacity disabled:opacity-50" style="background: linear-gradient(135deg, var(--accent-ai), var(--accent-primary)); color: #fff"
        >
          {loading ? 'Signing in…' : 'Sign in'}
        </button>
      </div>

      <div class="flex items-center gap-3">
        <div class="flex-1 h-px" style="background: var(--border-subtle)"></div>
        <span class="text-xs" style="color: var(--text-secondary)">или</span>
        <div class="flex-1 h-px" style="background: var(--border-subtle)"></div>
      </div>

      <a
        href="/api/auth/oauth/yandex"
        class="block w-full px-4 py-2.5 rounded-lg text-sm font-medium text-center transition-colors"
        style="background: var(--bg-tertiary); border: 1px solid var(--border-subtle); color: var(--text-primary)"
      >
        Войти через Яндекс
      </a>
    </div>
  </div>
</div>
