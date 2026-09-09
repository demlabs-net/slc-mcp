<script lang="ts">
  import { onMount } from 'svelte';
  import { writable, derived } from 'svelte/store';
  import { getSeat } from './lib/api';
  import { auth, checkAuth } from './lib/auth';

  import Layout from './lib/Layout.svelte';
  import Login from './pages/Login.svelte';
  import Dashboard from './pages/Dashboard.svelte';
  import KnowledgeBase from './pages/KnowledgeBase.svelte';
  import Tasks from './pages/Tasks.svelte';
  import Projects from './pages/Projects.svelte';
  import Seats from './pages/Seats.svelte';
  import Context from './pages/Context.svelte';
  import Admin from './pages/Admin.svelte';

  const routes: Record<string, any> = {
    dashboard: Dashboard,
    knowledge: KnowledgeBase,
    tasks: Tasks,
    projects: Projects,
    seats: Seats,
    context: Context,
    admin: Admin,
  };

  function getRouteFromHash(): string {
    if (typeof window === 'undefined') return 'dashboard';
    const hash = window.location.hash.replace('#/', '').replace('#', '').split('?')[0];
    return hash && routes[hash] ? hash : 'dashboard';
  }

  const currentRoute = writable(getRouteFromHash());
  const PageComponent = derived(currentRoute, $r => routes[$r] || Dashboard);
  const booted = derived(auth, $a => !$a.loading);
  const authed = derived(auth, $a => {
    if ($a.mode === 'seat') return true; // seat mode: no login needed
    return !!$a.accessToken && !!$a.user;
  });

  // The seat is created on the client (the server auto-provisions it on first request).
  getSeat();

  let checkDone = $state(false);

  onMount(async () => {
    // The mode is decided by /api/auth/me (seat | full) + the OAuth callback.
    await checkAuth();
    checkDone = true;
    const onHashChange = () => currentRoute.set(getRouteFromHash());
    window.addEventListener('hashchange', onHashChange);
    return () => window.removeEventListener('hashchange', onHashChange);
  });

  function navigate(route: string) {
    currentRoute.set(route);
    window.location.hash = `#/${route}`;
  }
</script>

{#if !checkDone || !$booted}
  <div class="min-h-screen flex items-center justify-center" style="background: var(--bg-primary)">
    <div class="text-sm" style="color: var(--text-secondary)">Loading…</div>
  </div>
{:else if !$authed}
  <Login />
{:else}
  <Layout currentRoute={$currentRoute} {navigate}>
    {#key $currentRoute}
      <div class="page-enter">
        <!-- svelte-ignore svelte_component_deprecated -->
        <svelte:component this={$PageComponent} />
      </div>
    {/key}
  </Layout>
{/if}
