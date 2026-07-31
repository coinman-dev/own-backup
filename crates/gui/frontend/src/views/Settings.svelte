<script lang="ts">
  import { pickPath } from '../lib/pickPath.svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { api } from '../lib/api';
  import type { ServiceStatus } from '../lib/types';

  /** The config as it was loaded, kept whole. Saving edits a copy of it rather
   *  than building a fresh object out of the four fields this page shows —
   *  which is how the remembered backups and the hook settings used to
   *  disappear the first time someone opened Settings and pressed Save. */
  let loadedConfig: Record<string, unknown> = $state({});
  let dataDirectory = $state('');
  let compressionLevel = $state(3);
  let logLevel = $state('info');
  let logMirrorDir = $state('');
  let serviceStatus: ServiceStatus | null = $state(null);
  let isLoading = $state(true);
  let error: string | null = $state(null);
  let saveMessage: string | null = $state(null);

  async function loadSettings() {
    isLoading = true;
    error = null;
    try {
      const [configResult, statusResult] = await Promise.allSettled([
        api.getConfig(),
        api.getServiceStatus(),
      ]);

      if (configResult.status === 'fulfilled') {
        const cfg = configResult.value;
        loadedConfig = cfg;
        dataDirectory = (cfg.data_dir as string) ?? '';
        compressionLevel = (cfg.default_compression_level as number) ?? 3;
        logLevel = (cfg.log_level as string) ?? 'info';
        logMirrorDir = (cfg.log_mirror_dir as string) ?? '';
      }

      serviceStatus = statusResult.status === 'fulfilled' ? statusResult.value : null;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    loadSettings();
  });

  async function pickMirrorDir() {
    const chosen = await pickPath({ directory: true, title: 'Куда дублировать журнал' });
    if (typeof chosen === 'string') logMirrorDir = chosen;
  }

  async function handleSave() {
    saveMessage = null;
    error = null;
    try {
      await api.saveConfig({
        ...loadedConfig,
        data_dir: dataDirectory,
        default_compression_level: compressionLevel,
        log_level: logLevel,
        log_mirror_dir: logMirrorDir.trim(),
      });
      saveMessage = 'Настройки сохранены.';
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  // --- Mount-prerequisites card (Windows: AIM driver; Linux: NBD/NTFS) ---
  let mountStatus: import('$lib/types').MountBackendStatus | null = $state(null);
  const isLinux = $derived(mountStatus?.platform === 'linux');
  const driverInstalled = $derived(mountStatus ? (mountStatus.installed ?? null) : null);
  let driverBusy = $state(false);
  let driverMsg: string | null = $state(null);
  let driverErr: string | null = $state(null);
  let showInstallModal = $state(false);
  // Modal-local install state: the modal stays open with a running progress
  // bar during install, and closes only on success (or shows the error).
  let installing = $state(false);
  let installError: string | null = $state(null);

  async function refreshDriver() {
    try {
      mountStatus = await api.aimDriverStatus();
    } catch (e) {
      mountStatus = null;
      driverErr = String(e);
    }
  }

  // Linux: load the in-tree kernel NBD module (no driver to install).
  async function loadNbd() {
    driverBusy = true;
    driverMsg = null;
    driverErr = null;
    try {
      mountStatus = await api.installAimDriver();
      driverMsg = 'Модуль NBD загружен.';
    } catch (e) {
      driverErr = String(e);
    } finally {
      driverBusy = false;
    }
  }

  $effect(() => {
    refreshDriver();
  });

  async function confirmInstall() {
    // Keep the modal open; run the progress bar; close only on success.
    installing = true;
    installError = null;
    driverMsg = null;
    driverErr = null;
    try {
      await api.installAimDriver();
      await refreshDriver();
      installing = false;
      showInstallModal = false;
      driverMsg = 'Драйвер установлен.';
    } catch (e) {
      installing = false;
      installError = String(e);
    }
  }

  function closeInstallModal() {
    if (installing) return; // don't let the user close mid-install
    showInstallModal = false;
    installError = null;
  }

  /** Where the driver comes from and who owns it — shown in the consent dialog
   *  so the terms are read from the vendor, not from us. */
  const VENDOR_LINKS = [
    'https://arsenalrecon.com/weapons/image-mounter',
    'https://github.com/ArsenalRecon/Arsenal-Image-Mounter',
  ];

  /** Open a link in the user's browser.
   *
   *  A webview swallows `target="_blank"`, and a licence the user is asked to
   *  accept but cannot open would be a poor kind of consent — so the URL goes
   *  to the shell plugin (already permitted in `capabilities`). If that fails,
   *  the address is still on screen to copy. */
  async function openExternal(event: MouseEvent, url: string) {
    event.preventDefault();
    try {
      await invoke('plugin:shell|open', { path: url });
    } catch (e) {
      console.error('could not open', url, e);
    }
  }

  async function uninstallDriver() {
    driverBusy = true;
    driverMsg = null;
    driverErr = null;
    try {
      await api.uninstallAimDriver();
      driverMsg = 'Драйвер удалён.';
      await refreshDriver();
    } catch (e) {
      driverErr = String(e);
    } finally {
      driverBusy = false;
    }
  }
</script>

<div class="settings-view">
  <div class="page-header">
    <h1 class="page-title">Settings</h1>
  </div>

  {#if error}
    <div class="error-banner">
      <span>{error}</span>
      <button class="dismiss-btn" onclick={() => (error = null)}>Dismiss</button>
    </div>
  {/if}

  {#if saveMessage}
    <div class="success-banner">
      <span>{saveMessage}</span>
      <button class="dismiss-btn" onclick={() => (saveMessage = null)}>OK</button>
    </div>
  {/if}

  {#if isLoading}
    <div class="loading-state">
      <div class="spinner"></div>
      <p>Loading settings...</p>
    </div>
  {:else}
    <div class="settings-grid">
      <div class="card">
        <h2 class="section-title">General</h2>

        <div class="form-group">
          <label for="data-dir">Data Directory</label>
          <input
            id="data-dir"
            type="text"
            class="form-input"
            placeholder="e.g. C:\ProgramData\OwnBackup or /var/lib/own-backup"
            bind:value={dataDirectory}
          />
          <p class="help-text">
            Directory where backup metadata and database are stored.
          </p>
        </div>

        <div class="form-group">
          <label for="comp-default">Default Compression Level: {compressionLevel}</label>
          <input
            id="comp-default"
            type="range"
            min="0"
            max="19"
            class="form-range"
            bind:value={compressionLevel}
          />
          <div class="range-labels">
            <span>0 (none)</span>
            <span>19 (max)</span>
          </div>
          <p class="help-text">
            Zstd compression level for new backup jobs. Higher values compress more but are slower.
          </p>
        </div>

        <div class="form-group">
          <label for="log-level">Log Level</label>
          <select id="log-level" class="form-select" bind:value={logLevel}>
            <option value="error">Error</option>
            <option value="warn">Warning</option>
            <option value="info">Info</option>
            <option value="debug">Debug</option>
            <option value="trace">Trace</option>
          </select>
        </div>

        <div class="form-group">
          <label for="log-mirror">Дублировать журнал на диск</label>
          <div class="path-row">
            <input
              id="log-mirror"
              type="text"
              class="form-input"
              placeholder="например, E:\ — пусто значит «не дублировать»"
              bind:value={logMirrorDir}
            />
            <button class="btn btn-secondary" onclick={pickMirrorDir}>Обзор…</button>
          </div>
          <p class="help-text">
            Копия журнала службы и окна ложится в указанную папку — на флешку или
            на другой диск. Каждая строка сразу сбрасывается на носитель, поэтому
            она останется там даже если машина уйдёт в синий экран: обычный журнал
            лежит на том же диске, который восстановление перезаписывает, и
            пропадает вместе с ним. Настройка подхватывается на ходу —
            перезапускать службу не нужно.
          </p>
        </div>

        <div class="form-actions">
          <button class="btn btn-primary" onclick={handleSave}>Save Settings</button>
        </div>
      </div>

      <div class="card">
        <h2 class="section-title">Service Status</h2>

        {#if serviceStatus}
          <div class="status-grid">
            <div class="status-item">
              <span class="status-label">Status</span>
              <span class="status-value running">
                <span class="status-dot"></span>
                Running
              </span>
            </div>
            <div class="status-item">
              <span class="status-label">Version</span>
              <span class="status-value">{serviceStatus.version}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Uptime</span>
              <span class="status-value">{serviceStatus.uptime}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Active Jobs</span>
              <span class="status-value">{serviceStatus.active_jobs}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Total Completed</span>
              <span class="status-value">{serviceStatus.total_backups_completed}</span>
            </div>
          </div>
        {:else}
          <div class="status-offline">
            <span class="status-dot offline"></span>
            <span>Service is not running or unreachable.</span>
          </div>
        {/if}
      </div>

      {#if isLinux}
        <div class="card">
          <h2 class="section-title">Поддержка монтирования (Linux)</h2>
          <p class="driver-desc">
            Отдельный драйвер не нужен: копии (VHDX и qcow2) монтируются через
            встроенный в ядро модуль <b>NBD</b> — программа сама отдаёт блоки,
            без qemu. Для просмотра <b>NTFS</b>-копий нужен драйвер ядра
            <code>ntfs3</code> или пакет <code>ntfs-3g</code>.
            {#if mountStatus?.kernel}<span class="kernel">Ядро: {mountStatus.kernel}</span>{/if}
          </p>

          <div class="driver-row">
            <div class="driver-status">
              {#if mountStatus?.nbd_ok}
                <span class="status-dot"></span><span>Модуль NBD доступен</span>
              {:else}
                <span class="status-dot offline"></span><span>Модуль NBD не загружен</span>
              {/if}
            </div>
            <div class="driver-actions">
              {#if !mountStatus?.nbd_ok}
                <button class="btn btn-primary" onclick={loadNbd} disabled={driverBusy}>
                  {driverBusy ? 'Загрузка…' : 'Загрузить модуль NBD'}
                </button>
              {/if}
            </div>
          </div>

          <div class="driver-row">
            <div class="driver-status">
              {#if mountStatus?.ntfs_ok}
                <span class="status-dot"></span><span>NTFS: {mountStatus.ntfs_backend}</span>
              {:else}
                <span class="status-dot offline"></span>
                <span>NTFS не поддерживается — установите <code>ntfs-3g</code></span>
              {/if}
            </div>
          </div>

          {#if driverMsg}<div class="driver-note ok">{driverMsg}</div>{/if}
          {#if driverErr}<div class="driver-note err">{driverErr}</div>{/if}
        </div>
      {:else}
        <div class="card">
          <h2 class="section-title">Драйвер монтирования (Arsenal Image Mounter)</h2>
          <p class="driver-desc">
            Нужен для подключения резервных копий формата <b>qcow2</b> как диска.
            Для формата VHDX не требуется — он монтируется средствами Windows.
          </p>

          <div class="driver-row">
            <div class="driver-status">
              {#if driverInstalled === null}
                <span class="status-dot offline"></span><span>Статус неизвестен</span>
              {:else if driverInstalled}
                <span class="status-dot"></span><span>Установлен</span>
              {:else}
                <span class="status-dot offline"></span><span>Не установлен</span>
              {/if}
            </div>
            <div class="driver-actions">
              {#if driverInstalled}
                <button class="btn btn-secondary" onclick={uninstallDriver} disabled={driverBusy}>
                  {driverBusy ? 'Удаление…' : 'Удалить'}
                </button>
              {:else}
                <button class="btn btn-primary" onclick={() => (showInstallModal = true)} disabled={driverBusy}>
                  {driverBusy ? 'Установка…' : 'Установить'}
                </button>
              {/if}
            </div>
          </div>

          {#if driverMsg}<div class="driver-note ok">{driverMsg}</div>{/if}
          {#if driverErr}<div class="driver-note err">{driverErr}</div>{/if}
        </div>
      {/if}
    </div>
  {/if}
</div>

{#if showInstallModal}
  <div class="modal-backdrop" role="presentation" onclick={closeInstallModal}>
    <div class="modal" role="dialog" aria-modal="true" onclick={(e) => e.stopPropagation()}>
      <h3 class="modal-title">Установка стороннего драйвера</h3>
      <p>
        Сейчас будет установлен <b>Arsenal Image Mounter</b> — отдельная программа
        компании <b>Arsenal Consulting, Inc.</b> (d/b/a Arsenal Recon), а не часть
        My Own Backup. Разработчик описывает её так:
      </p>
      <p class="modal-quote">
        «Arsenal Image Mounter mounts the contents of disk images as complete disks
        in Microsoft Windows» — подключает содержимое образов дисков как полноценные
        диски Windows.
      </p>
      <p>
        <b>Лицензия.</b> Проект распространяется по двойной лицензии: GNU Affero
        General Public License v3 либо отдельная коммерческая лицензия Arsenal Recon.
        Установленный без ключа драйвер работает в бесплатном режиме: «If Arsenal
        Image Mounter is run without a license, it will run in “Free Mode” and provide
        core functionality».
      </p>
      <p>
        <b>Что произойдёт.</b> Пакет <code>DriverFiles.zip</code> будет загружен
        напрямую из официального репозитория Arsenal Recon и установлен как драйвер
        режима ядра (SCSI-адаптер <code>ROOT\PhDskMnt</code>). My Own Backup этот
        драйвер не распространяет и правообладателем не является. Удалить его можно
        здесь же кнопкой «Удалить».
      </p>
      <p>
        <b>Зачем.</b> Драйвер нужен только для копий формата <b>qcow2</b> —
        подключать их как диск. Копии VHDX монтируются средствами Windows и драйвера
        не требуют.
      </p>
      <p class="modal-links">
        {#each VENDOR_LINKS as url}
          <a href={url} onclick={(e) => openExternal(e, url)}>{url.replace('https://', '')}</a>
        {/each}
      </p>
      <p>
        Нажимая «Принять и установить», вы принимаете условия Arsenal Recon как
        пользователь их продукта.
      </p>
      <div class="modal-actions">
        <button class="btn btn-secondary" onclick={closeInstallModal} disabled={installing}>Отмена</button>
        <button class="btn btn-primary" onclick={confirmInstall} disabled={installing}>
          {installing ? 'Установка…' : 'Принять и установить'}
        </button>
      </div>

      {#if installing}
        <div class="modal-progress">
          <div class="progress-caption">Загрузка и установка драйвера…</div>
          <div class="progress-track"><div class="progress-bar"></div></div>
        </div>
      {:else if installError}
        <div class="modal-install-error">
          <div class="err-title">Не удалось установить драйвер</div>
          <div class="err-detail">{installError}</div>
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .settings-view {
    padding: 28px 32px;
    height: 100%;
    overflow-y: auto;
  }

  .page-header {
    margin-bottom: 24px;
  }

  .page-title {
    font-size: 24px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
  }

  .error-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    background: rgba(234, 67, 53, 0.08);
    border: 1px solid rgba(234, 67, 53, 0.2);
    border-radius: var(--radius);
    color: var(--error);
    margin-bottom: 20px;
    font-size: 13px;
  }

  .success-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    background: rgba(52, 168, 83, 0.08);
    border: 1px solid rgba(52, 168, 83, 0.2);
    border-radius: var(--radius);
    color: var(--success);
    margin-bottom: 20px;
    font-size: 13px;
  }

  .dismiss-btn {
    margin-left: auto;
    background: none;
    border: none;
    font-size: 12px;
    font-weight: 500;
    text-decoration: underline;
    cursor: pointer;
    color: inherit;
  }

  .loading-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 80px 20px;
    color: var(--text-muted);
  }

  .spinner {
    width: 36px;
    height: 36px;
    border: 3px solid var(--border);
    border-top-color: var(--primary);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
    margin-bottom: 16px;
  }

  .settings-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 24px;
    align-items: start;
  }

  .section-title {
    font-size: 16px;
    font-weight: 600;
    color: var(--text);
    margin: 0 0 16px;
  }

  .help-text {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 4px;
  }

  /* Path plus its "Обзор…" button: the field takes what is left. */
  .path-row {
    display: flex;
    gap: 8px;
    align-items: center;
  }

  .path-row .form-input {
    flex: 1;
    min-width: 0;
  }

  .form-range {
    width: 100%;
    accent-color: var(--primary);
  }

  .range-labels {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    color: var(--text-muted);
    margin-top: 4px;
  }

  .form-actions {
    margin-top: 20px;
    display: flex;
    justify-content: flex-end;
  }

  .status-grid {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .status-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding-bottom: 10px;
    border-bottom: 1px solid var(--border);
  }

  .status-item:last-child {
    border-bottom: none;
    padding-bottom: 0;
  }

  .status-label {
    font-size: 13px;
    color: var(--text-muted);
  }

  .status-value {
    font-size: 14px;
    font-weight: 500;
    color: var(--text);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .status-value.running {
    color: var(--success);
  }

  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--success);
    flex-shrink: 0;
  }

  .status-dot.offline {
    background: var(--error);
  }

  .status-offline {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 16px;
    background: rgba(234, 67, 53, 0.06);
    border-radius: 6px;
    font-size: 13px;
    color: var(--text-secondary);
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }

  /* --- driver card --- */
  .driver-desc {
    font-size: 13px;
    color: var(--text-secondary);
    margin: 0 0 16px;
    line-height: 1.5;
  }
  .driver-desc code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12px;
    background: var(--bg-secondary, rgba(127, 127, 127, 0.15));
    padding: 1px 5px;
    border-radius: 4px;
  }
  .driver-desc .kernel {
    display: block;
    margin-top: 6px;
    opacity: 0.75;
  }
  .driver-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 8px;
  }
  .driver-status {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 14px;
    color: var(--text);
  }
  .driver-note {
    margin-top: 12px;
    padding: 10px 12px;
    border-radius: 6px;
    font-size: 13px;
  }
  .driver-note.ok { background: rgba(52, 168, 83, 0.1); color: #2e7d32; }
  .driver-note.err { background: rgba(234, 67, 53, 0.08); color: #c62828; }

  /* --- install modal --- */
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--surface, #fff);
    color: var(--text);
    max-width: 480px;
    width: calc(100% - 48px);
    border-radius: 10px;
    padding: 24px;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.3);
    /* The dialog grew once it started saying whose driver this is and under
       what terms — it scrolls rather than pushing its own buttons off screen. */
    max-height: calc(100vh - 96px);
    overflow-y: auto;
  }
  .modal-title {
    margin: 0 0 12px;
    font-size: 18px;
    font-weight: 600;
  }
  .modal p {
    font-size: 13px;
    color: var(--text-secondary);
    line-height: 1.5;
    margin: 0 0 10px;
  }
  .modal-url {
    font-family: monospace;
    background: rgba(127, 127, 127, 0.1);
    padding: 8px 10px;
    border-radius: 6px;
    word-break: break-all;
  }
  /* The vendor's own words about their product, kept as a quotation so it is
     visibly theirs and not ours. */
  .modal-quote {
    border-left: 3px solid var(--border);
    padding-left: 10px;
    font-style: italic;
  }
  .modal code {
    font-family: monospace;
    font-size: 12px;
    background: rgba(127, 127, 127, 0.12);
    padding: 1px 5px;
    border-radius: 4px;
  }
  .modal-links {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .modal-links a {
    font-family: monospace;
    font-size: 12px;
    color: var(--accent, #5b6ee1);
    text-decoration: none;
    word-break: break-all;
  }
  .modal-links a:hover {
    text-decoration: underline;
  }
  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
    margin-top: 20px;
  }

  /* --- install progress / error (bottom of the modal) --- */
  .modal-progress {
    margin-top: 18px;
  }
  .progress-caption {
    font-size: 12px;
    color: var(--text-secondary);
    margin-bottom: 8px;
  }
  .progress-track {
    position: relative;
    height: 5px;
    background: rgba(127, 127, 127, 0.15);
    border-radius: 3px;
    overflow: hidden;
  }
  .progress-bar {
    position: absolute;
    top: 0;
    height: 100%;
    width: 32%;
    border-radius: 3px;
    background: var(--primary, #6c5ce7);
    animation: indeterminate 1.1s ease-in-out infinite;
  }
  @keyframes indeterminate {
    0% { left: -32%; }
    100% { left: 100%; }
  }
  .modal-install-error {
    margin-top: 18px;
    padding: 12px 14px;
    border-radius: 6px;
    background: rgba(234, 67, 53, 0.08);
    border: 1px solid rgba(234, 67, 53, 0.25);
  }
  .err-title {
    font-size: 13px;
    font-weight: 600;
    color: #c62828;
    margin-bottom: 6px;
  }
  .err-detail {
    font-size: 12px;
    font-family: monospace;
    color: var(--text);
    word-break: break-word;
    line-height: 1.5;
  }
</style>
