<script lang="ts">
  import Sidebar from './components/Sidebar.svelte';
  import Dashboard from './views/Dashboard.svelte';
  import Activity from './views/Activity.svelte';
  import Restore from './views/Restore.svelte';
  import JobSettings from './views/JobSettings.svelte';
  import Settings from './views/Settings.svelte';
  import About from './views/About.svelte';
  import type { ViewName } from './lib/types';

  let currentView: ViewName = $state('dashboard');
  let selectedJobId: string = $state('');

  function handleNavigate(view: ViewName, jobId?: string) {
    currentView = view;
    if (jobId) {
      selectedJobId = jobId;
    }
  }
</script>

<div class="app-layout">
  <Sidebar {currentView} onNavigate={handleNavigate} />

  <main class="content-area">
    {#if currentView === 'dashboard'}
      <Dashboard onNavigate={handleNavigate} />
    {:else if currentView === 'activity'}
      <Activity />
    {:else if currentView === 'restore'}
      <Restore />
    {:else if currentView === 'job-settings'}
      <JobSettings jobId={selectedJobId} onNavigate={handleNavigate} />
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
