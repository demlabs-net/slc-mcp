<script lang="ts">
  import { onMount } from 'svelte';
  import type { Snippet } from 'svelte';
  import { getSeat, resetSeat } from './api';

  let {
    currentRoute,
    navigate,
    children,
  }: {
    currentRoute: string;
    navigate: (route: string) => void;
    children: Snippet;
  } = $props();

  const navigation = [
    { name: 'Dashboard', route: 'dashboard', icon: 'M3 12l2-2m0 0l7-7 7 7M5 10v10a1 1 0 001 1h3m10-11l2 2m-2-2v10a1 1 0 01-1 1h-3m-6 0a1 1 0 001-1v-4a1 1 0 011-1h2a1 1 0 011 1v4a1 1 0 001 1m-6 0h6' },
    { name: 'Knowledge', route: 'knowledge', icon: 'M12 6.253v13m0-13C10.832 5.477 9.246 5 7.5 5S4.168 5.477 3 6.253v13C4.168 18.477 5.754 18 7.5 18s3.332.477 4.5 1.253m0-13C13.168 5.477 14.754 5 16.5 5c1.747 0 3.332.477 4.5 1.253v13C19.832 18.477 18.247 18 16.5 18c-1.746 0-3.332.477-4.5 1.253' },
    { name: 'Tasks', route: 'tasks', icon: 'M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4' },
    { name: 'Projects', route: 'projects', icon: 'M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10' },
    { name: 'Seats', route: 'seats', icon: 'M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z' },
    { name: 'Context', route: 'context', icon: 'M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z' },
  ];

  let seat = $state('');
  let sidebarCollapsed = $state(false);
  let dark = $state(false);

  onMount(() => {
    seat = getSeat();
    const saved = localStorage.getItem('theme');
    dark = saved === 'dark' || (!saved && window.matchMedia('(prefers-color-scheme: dark)').matches);
    applyTheme(dark);
  });

  function applyTheme(isDark: boolean) {
    document.documentElement.classList.toggle('dark', isDark);
    document.documentElement.classList.toggle('light', !isDark);
  }

  function toggleTheme() {
    dark = !dark;
    localStorage.setItem('theme', dark ? 'dark' : 'light');
    applyTheme(dark);
  }

  function handleNewSeat() {
    resetSeat();
    window.location.reload();
  }
</script>

<div class="flex h-screen overflow-hidden" style="background-color: var(--bg-primary); color: var(--text-primary)">
  <!-- Sidebar -->
  <aside
    class="flex flex-col border-r transition-all duration-200 shrink-0" style="background-color: var(--bg-secondary); border-color: var(--border-subtle); width: {sidebarCollapsed ? '64px' : '220px'}"
  >
    <!-- Logo -->
    <div class="flex items-center gap-3 px-4 h-14 border-b shrink-0" style="border-color: var(--border-subtle)">
      <div class="w-7 h-7 rounded-lg flex items-center justify-center text-xs font-bold shrink-0" style="color: var(--text-primary); background: linear-gradient(135deg, var(--accent-ai), var(--accent-primary))">
        S
      </div>
      {#if !sidebarCollapsed}
        <span class="text-sm font-semibold whitespace-nowrap" style="color: var(--text-primary)">SLC Control Panel</span>
      {/if}
    </div>

    <!-- Navigation -->
    <nav class="flex-1 py-3 px-2 space-y-0.5 overflow-y-auto">
      {#each navigation as item (item.route)}
        {@const isActive = currentRoute === item.route}
        <div class="tooltip-wrapper">
          <button
            onclick={() => navigate(item.route)}
            class="w-full flex items-center gap-3 px-3 py-2.5 text-sm font-medium transition-all relative cursor-pointer" style="border-radius: 10px;
              {isActive
                ? 'background: linear-gradient(135deg, rgba(139,92,246,0.15), rgba(139,92,246,0.05)); color: var(--accent-ai); border-left: 3px solid var(--accent-ai);'
                : 'color: var(--text-secondary); border-left: 3px solid transparent;'}"
            onmouseenter={(e) => {
              if (!isActive) e.currentTarget.style.background = 'var(--bg-tertiary)';
            }}
            onmouseleave={(e) => {
              if (!isActive) e.currentTarget.style.background = 'transparent';
            }}
          >
          <svg class="w-5 h-5 shrink-0" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" d={item.icon} />
          </svg>
          {#if !sidebarCollapsed}
            <span class="whitespace-nowrap">{item.name}</span>
          {/if}
          </button>
          {#if sidebarCollapsed}
            <span class="tooltip">{item.name}</span>
          {/if}
        </div>
      {/each}
    </nav>

    <!-- Bottom: Seat + Theme -->
    <div class="border-t py-3 px-2 space-y-1 shrink-0" style="border-color: var(--border-subtle)">
      <!-- Theme toggle -->
      <button
        onclick={toggleTheme}
        class="w-full flex items-center gap-3 px-3 py-2 rounded-lg text-sm transition-colors cursor-pointer" style="color: var(--text-secondary)"
        onmouseenter={(e) => e.currentTarget.style.backgroundColor = 'var(--bg-tertiary)'}
        onmouseleave={(e) => e.currentTarget.style.backgroundColor = 'transparent'}
        title="Toggle theme"
      >
        <svg class="w-5 h-5 shrink-0" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
          {#if dark}
            <path stroke-linecap="round" stroke-linejoin="round" d="M12 3v1m0 16v1m9-9h-1M4 12H3m15.364 6.364l-.707-.707M6.343 6.343l-.707-.707m12.728 0l-.707.707M6.343 17.657l-.707.707M16 12a4 4 0 11-8 0 4 4 0 018 0z" />
          {:else}
            <path stroke-linecap="round" stroke-linejoin="round" d="M20.354 15.354A9 9 0 018.646 3.646 9.003 9.003 0 0012 21a9.003 9.003 0 008.354-5.646z" />
          {/if}
        </svg>
        {#if !sidebarCollapsed}
          <span>{dark ? 'Light Mode' : 'Dark Mode'}</span>
        {/if}
      </button>

      <!-- Seat -->
      <button
        onclick={handleNewSeat}
        class="w-full flex items-center gap-3 px-3 py-2 rounded-lg text-sm transition-colors cursor-pointer" style="color: var(--text-secondary)"
        onmouseenter={(e) => e.currentTarget.style.backgroundColor = 'var(--bg-tertiary)'}
        onmouseleave={(e) => e.currentTarget.style.backgroundColor = 'transparent'}
        title="Сменить seat id (новый сид)"
      >
        <svg class="w-5 h-5 shrink-0" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" d="M17 16l4-4m0 0l-4-4m4 4H7m6 4v1a3 3 0 01-3 3H6a3 3 0 01-3-3V7a3 3 0 013-3h4a3 3 0 013 3v1" />
        </svg>
        {#if !sidebarCollapsed}
          <span class="truncate">seat: {seat ? seat.slice(0, 14) + '…' : '…'}</span>
        {/if}
      </button>

      <!-- Collapse toggle -->
      <button
        onclick={() => sidebarCollapsed = !sidebarCollapsed}
        class="w-full flex items-center gap-3 px-3 py-2 rounded-lg text-sm transition-colors cursor-pointer" style="color: var(--text-tertiary)"
        onmouseenter={(e) => e.currentTarget.style.backgroundColor = 'var(--bg-tertiary)'}
        onmouseleave={(e) => e.currentTarget.style.backgroundColor = 'transparent'}
        title={sidebarCollapsed ? 'Expand sidebar' : 'Collapse sidebar'}
      >
        <svg class="w-5 h-5 shrink-0 transition-transform {sidebarCollapsed ? 'rotate-180' : ''}" fill="none" stroke="currentColor" stroke-width="1.5" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" d="M11 19l-7-7 7-7m8 14l-7-7 7-7" />
        </svg>
        {#if !sidebarCollapsed}
          <span>Collapse</span>
        {/if}
      </button>
    </div>
  </aside>

  <!-- Main Content -->
  <main class="flex-1 overflow-y-auto">
    <div class="p-6 max-w-7xl mx-auto">
      {@render children()}
    </div>
  </main>
</div>
