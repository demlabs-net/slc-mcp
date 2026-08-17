<script lang="ts">
  import { onMount } from 'svelte';
  import { writable, derived } from 'svelte/store';
  import { getSeat } from './lib/api';

  import Layout from './lib/Layout.svelte';
  import Dashboard from './pages/Dashboard.svelte';
  import KnowledgeBase from './pages/KnowledgeBase.svelte';
  import Tasks from './pages/Tasks.svelte';
  import Projects from './pages/Projects.svelte';
  import Seats from './pages/Seats.svelte';
  import Context from './pages/Context.svelte';

  const routes: Record<string, any> = {
    dashboard: Dashboard,
    knowledge: KnowledgeBase,
    tasks: Tasks,
    projects: Projects,
    seats: Seats,
    context: Context,
  };

  function getRouteFromHash(): string {
    if (typeof window === 'undefined') return 'dashboard';
    const hash = window.location.hash.replace('#/', '').replace('#', '').split('?')[0];
    return hash && routes[hash] ? hash : 'dashboard';
  }

  const currentRoute = writable(getRouteFromHash());
  const PageComponent = derived(currentRoute, $r => routes[$r] || Dashboard);

  // Сид создаётся на клиенте (сервер авто-провиженит при первом запросе).
  getSeat();

  onMount(() => {
    const onHashChange = () => currentRoute.set(getRouteFromHash());
    window.addEventListener('hashchange', onHashChange);
    return () => window.removeEventListener('hashchange', onHashChange);
  });

  function navigate(route: string) {
    currentRoute.set(route);
    window.location.hash = `#/${route}`;
  }
</script>

<Layout currentRoute={$currentRoute} {navigate}>
  {#key $currentRoute}
    <div class="page-enter">
      <!-- svelte-ignore svelte_component_deprecated -->
      <svelte:component this={$PageComponent} />
    </div>
  {/key}
</Layout>
