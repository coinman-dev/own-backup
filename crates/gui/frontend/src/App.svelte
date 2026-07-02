<script lang="ts">
  import { onMount } from 'svelte';
  import { initEngine } from './lib/engineStore.svelte';
  import { initJobs } from './lib/jobStore.svelte';
  import Sidebar from './components/Sidebar.svelte';
  import BackupSection from './views/BackupSection.svelte';
  import Restore from './views/Restore.svelte';
  import Tools from './views/Tools.svelte';
  import Activity from './views/Activity.svelte';
  import Settings from './views/Settings.svelte';
  import About from './views/About.svelte';
  import type { ViewName } from './lib/types';

  let currentView: ViewName = $state('backup');

  function handleNavigate(view: ViewName) {
    currentView = view;
  }

  onMount(() => {
    initEngine();
    initJobs();
  });
</script>

<div class="app-layout">
  <Sidebar {currentView} onNavigate={handleNavigate} />

  <main class="content-area">
    {#if currentView === 'backup'}
      <BackupSection />
    {:else if currentView === 'restore'}
      <Restore />
    {:else if currentView === 'tools'}
      <Tools />
    {:else if currentView === 'activity'}
      <Activity />
    {:else if currentView === 'settings'}
      <Settings />
    {:else if currentView === 'about'}
      <About />
    {/if}
  </main>
</div>

<style>
  .app-layout {
    display: flex;
    height: 100vh;
    width: 100vw;
    overflow: hidden;
  }

  .content-area {
    flex: 1;
    min-width: 0;
    background: var(--content-bg);
    overflow: hidden;
  }
</style>
