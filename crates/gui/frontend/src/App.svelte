<script lang="ts">
  import { onMount } from 'svelte';
  import { initEngine } from './lib/engineStore.svelte';
  import { initJobs } from './lib/jobStore.svelte';
  import { api } from './lib/api';
  import Sidebar from './components/Sidebar.svelte';
  import BackupSection from './views/BackupSection.svelte';
  import Restore from './views/Restore.svelte';
  import Tools from './views/Tools.svelte';
  import Activity from './views/Activity.svelte';
  import Settings from './views/Settings.svelte';
  import About from './views/About.svelte';
  import type { ViewName } from './lib/types';

  let currentView: ViewName = $state('backup');
  // Warning shown when the backup service isn't running/installed. Everything
  // (jobs, disks, backups) runs through it, so nothing works until it's up.
  let serviceWarn: string | null = $state(null);
  let checking = $state(false);

  function handleNavigate(view: ViewName) {
    currentView = view;
  }

  async function checkService() {
    checking = true;
    try {
      const h = await api.serviceHealth();
      if (h.reachable) {
        serviceWarn = null;
      } else if (!h.installed) {
        serviceWarn =
          'Служба «Own Backup» не установлена. Для работы программы установите её: ' +
          'запустите own-backup-service.exe /install (появится запрос прав администратора), ' +
          'затем нажмите «Повторить».';
      } else {
        serviceWarn =
          'Служба «Own Backup» установлена, но не запущена. Запустите её ' +
          '(нужны права администратора) или перезагрузите компьютер, затем нажмите «Повторить».';
      }
    } catch (e) {
      serviceWarn = `Не удалось проверить службу: ${e}`;
    } finally {
      checking = false;
    }
  }

  onMount(() => {
    checkService();
    initEngine();
    initJobs();
  });
</script>

<div class="app-layout">
  <Sidebar {currentView} onNavigate={handleNavigate} />

  <main class="content-area">
    {#if serviceWarn}
      <div class="service-warning" role="alert">
        <span class="sw-icon">⚠️</span>
        <span class="sw-text">{serviceWarn}</span>
        <button class="sw-retry" onclick={checkService} disabled={checking}>
          {checking ? 'Проверка…' : 'Повторить'}
        </button>
      </div>
    {/if}

    <div class="view-host">
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
    </div>
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
    display: flex;
    flex-direction: column;
    background: var(--content-bg);
    overflow: hidden;
  }

  .view-host {
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  .service-warning {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 16px;
    background: #fff4e5;
    color: #7a4b00;
    border-bottom: 1px solid #f0c98a;
    font-size: 13px;
  }

  .sw-icon {
    flex: 0 0 auto;
    font-size: 16px;
  }

  .sw-text {
    flex: 1;
    line-height: 1.4;
  }

  .sw-retry {
    flex: 0 0 auto;
    padding: 5px 14px;
    border: 1px solid #d99a3a;
    border-radius: 6px;
    background: #ffd9a8;
    color: #7a4b00;
    cursor: pointer;
    font-size: 13px;
  }

  .sw-retry:hover:not(:disabled) {
    background: #ffc880;
  }

  .sw-retry:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
