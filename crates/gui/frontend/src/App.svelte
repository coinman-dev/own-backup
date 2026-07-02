<script lang="ts">
  import { onMount } from 'svelte';
  import { initEngine } from './lib/engineStore.svelte';
  import Sidebar from './components/Sidebar.svelte';
  import BackupSection from './views/BackupSection.svelte';
  import Tools from './views/Tools.svelte';
  import JobSettings from './views/JobSettings.svelte';
  import Settings from './views/Settings.svelte';
  import About from './views/About.svelte';
  import type { ViewName } from './lib/types';

  let currentView: ViewName = $state('backup');
  let selectedJobId: string = $state('');

  function handleNavigate(view: ViewName, jobId?: string) {
    currentView = view;
    if (jobId) {
      selectedJobId = jobId;
    }
  }

  onMount(() => {
    initEngine();
  });
</script>

<div class="app-layout">
  <Sidebar {currentView} onNavigate={handleNavigate} />

  <main class="content-area">
    {#if currentView === 'backup'}
      <BackupSection onNavigate={handleNavigate} />
    {:else if currentView === 'tools'}
      <Tools />
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
