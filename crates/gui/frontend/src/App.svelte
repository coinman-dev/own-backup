<script lang="ts">
  import { onMount } from 'svelte';
  import { initEngine, refreshVolumes } from './lib/engineStore.svelte';
  import { initJobs, reloadActivity, reloadJobs } from './lib/jobStore.svelte';
  import { api } from './lib/api';
  import { setMediaMode } from './lib/mediaStore.svelte';
  import { nav } from './lib/navStore.svelte';
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
  import { m } from './lib/paraglide/messages';
  import { onLocale } from './lib/i18n.svelte';
  import { ensurePlatform, isWindows } from './lib/platformStore.svelte';
  import { waiting, initWaiting } from './lib/waitingStore.svelte';

  async function stopWaiting() {
    // The stop button of what the person started — it reaches an operation
    // that still waits its turn as well as one that runs.
    await api.cancelEngineOp().catch(() => {});
  }

  let currentView: ViewName = $state('backup');
  // Warning shown when the backup service isn't running/installed. Everything
  // (jobs, disks, backups) runs through it, so nothing works until it's up.
  // Kept as a function, not a string: the text is drawn in whatever language
  // the window speaks when it is drawn.
  let serviceWarn: (() => string) | null = $state(null);
  let checking = $state(false);

  function handleNavigate(view: ViewName) {
    currentView = view;
  }

  // A section asked for from inside another one (a job's «Подключить как диск»).
  $effect(() => {
    if (nav.request) {
      currentView = nav.request;
      nav.request = null;
    }
  });

  async function checkService() {
    checking = true;
    try {
      const h = await api.serviceHealth();
      setMediaMode(h.media);
      await ensurePlatform();
      // The advice names commands, and those differ: `.exe /install` and the
      // service manager on Windows, sudo and systemctl on Linux.
      const win = isWindows();
      if (h.access_denied) {
        // There, answering, and refusing every call: said as what it is
        // rather than as an empty job list and «service not running».
        serviceWarn = m.app_access_denied;
      } else if (h.group_access === 'not_member' || h.group_access === 'relogin') {
        // Running, and closed to this user by the socket's group: «not
        // running» would send them to start a service that is up.
        serviceWarn = h.group_access === 'relogin' ? m.app_relogin_group : m.app_not_in_group;
      } else if (h.media) {
        // On the boot medium the engine deliberately runs as a plain process:
        // WinPE's registry is in RAM, so there is nothing to install into, and
        // a schedule could not outlive the session anyway. Only losing the
        // engine is worth saying anything about.
        serviceWarn = h.reachable ? null : win ? m.app_engine_down : m.app_engine_down_linux;
      } else if (!h.installed) {
        // Not installed is always worth flagging: even if a temporary/foreground
        // instance answers, the scheduler only runs from the installed service.
        serviceWarn = h.reachable
          ? win ? m.app_not_installed_temp : m.app_not_installed_temp_linux
          : win ? m.app_not_installed : m.app_not_installed_linux;
      } else if (!h.reachable) {
        serviceWarn = win ? m.app_stopped : m.app_stopped_linux;
      } else {
        serviceWarn = null;
      }
    } catch (e) {
      serviceWarn = () => m.app_check_failed({ error: String(e) });
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
    // The service's messages (errors, restore checks, …) come in the window's
    // language: it is told now and at every switch. A config it cannot write
    // leaves them Russian, which is not worth a banner.
    // What the window already holds was put into words by the service in the
    // old language — the disk inventory's partition labels, «LUKS
    // (зашифровано)» in an English window (2026-10-24), and the notes of the
    // runs in the jobs and the activity list (2026-10-26). The service
    // switches as it saves the setting, so once it has, all three are asked
    // again.
    let told = false;
    onLocale((l) => {
      const switched = told;
      told = true;
      api
        .setUiLanguage(l)
        .then(() => {
          if (switched) {
            void refreshVolumes();
            void reloadJobs();
            void reloadActivity();
          }
        })
        .catch(() => {});
    });
    checkService();
    initEngine();
    initJobs();
    initWaiting();
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
        <span class="sw-text">{serviceWarn()}</span>
        <button class="sw-retry" onclick={checkService} disabled={checking}>
          {checking ? m.common_checking() : m.common_retry()}
        </button>
      </div>
    {/if}

    {#each waiting.ops as op (op.id)}
      <div class="waiting-strip" role="status">
        <svg class="sw-icon" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="8.5" />
          <path d="M12 7.5V12l3 2" />
        </svg>
        <span class="sw-text">{m.app_waiting({ what: op.what, behind: op.behind })}</span>
        <button class="sw-retry" onclick={stopWaiting}>{m.common_cancel()}</button>
      </div>
    {/each}

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

  .waiting-strip {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 16px;
    background: var(--warning-soft);
    color: var(--text);
    border-bottom: 1px solid var(--border);
    font-size: 13px;
  }

  .waiting-strip .sw-icon {
    color: var(--warning);
  }

  .waiting-strip .sw-retry {
    border-color: var(--border);
    color: var(--text);
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
