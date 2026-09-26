<script lang="ts">
  import { onMount } from 'svelte';
  import { initEngine } from './lib/engineStore.svelte';
  import { initJobs } from './lib/jobStore.svelte';
  import { api } from './lib/api';
  import { setMediaMode } from './lib/mediaStore.svelte';
  import Sidebar from './components/Sidebar.svelte';
  import PathPicker from './components/PathPicker.svelte';
  import BackupSection from './views/BackupSection.svelte';
  import Restore from './views/Restore.svelte';
  import Boot from './views/Boot.svelte';
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
      setMediaMode(h.media);
      if (h.media) {
        // On the boot medium the engine deliberately runs as a plain process:
        // WinPE's registry is in RAM, so there is nothing to install into, and
        // a schedule could not outlive the session anyway. Only losing the
        // engine is worth saying anything about.
        serviceWarn = h.reachable
          ? null
          : 'Движок не отвечает. Запустите его: own-backup-service.exe --run --media, ' +
            'затем нажмите «Повторить».';
      } else if (!h.installed) {
        // Not installed is always worth flagging: even if a temporary/foreground
        // instance answers, the scheduler only runs from the installed service.
        serviceWarn = h.reachable
          ? 'Служба «Own Backup» не установлена (сейчас отвечает временный экземпляр). ' +
            'Установите её: own-backup-service.exe /install — иначе не будет работать расписание.'
          : 'Служба «Own Backup» не установлена. Для работы программы установите её: ' +
            'запустите own-backup-service.exe /install (появится запрос прав администратора), ' +
            'затем нажмите «Повторить».';
      } else if (!h.reachable) {
        serviceWarn =
          'Служба «Own Backup» установлена, но не запущена. Запустите её ' +
          '(нужны права администратора) или перезагрузите компьютер, затем нажмите «Повторить».';
      } else {
        serviceWarn = null;
      }
    } catch (e) {
      serviceWarn = `Не удалось проверить службу: ${e}`;
    } finally {
      checking = false;
    }
  }

  onMount(() => {
    // Носитель ждёт именно этого: не «окно создано», а «интерфейс нарисовался».
    // Окно создаётся успешно и с мёртвым движком — пустое и синее, — и раньше
    // носитель на этом основании докладывал, что всё хорошо (2026-09-20).
    // Сказать правду может только код, который выполняется ВНУТРИ окна.
    api.guiReady().catch(() => {});
    checkService();
    initEngine();
    initJobs();
  });
</script>

<PathPicker />

<div class="app-layout">
  <Sidebar {currentView} onNavigate={handleNavigate} />

  <main class="content-area">
    {#if serviceWarn}
      <div class="service-warning" role="alert">
        <svg class="sw-icon" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 3.5l9 16H3z" />
          <path d="M12 10v4M12 17h.01" />
        </svg>
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
      {:else if currentView === 'boot'}
        <Boot />
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
    background: var(--error-soft);
    color: var(--error-text);
    border-bottom: 1px solid var(--error-line);
    font-size: 13px;
  }

  .sw-icon {
    flex: 0 0 auto;
    color: var(--error);
  }

  .sw-text {
    flex: 1;
    line-height: 1.45;
  }

  .sw-retry {
    flex: 0 0 auto;
    padding: 5px 14px;
    border: 1px solid var(--error);
    border-radius: 6px;
    background: transparent;
    color: var(--error-text);
    cursor: pointer;
    font-size: 13px;
    font-weight: 600;
  }

  .sw-retry:hover:not(:disabled) {
    background: var(--error-soft);
  }

  .sw-retry:disabled {
    opacity: 0.6;
    cursor: default;
  }
</style>
