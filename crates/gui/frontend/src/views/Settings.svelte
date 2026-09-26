<script lang="ts">
  import { pickPath } from '../lib/pickPath.svelte';
  import { api } from '../lib/api';
  import type { ServiceStatus, MountBackendStatus } from '../lib/types';

  /** The config as it was loaded, kept whole. Saving edits a copy of it rather
   *  than building a fresh object out of the fields this page shows — which is
   *  how the remembered backups and the hook settings used to disappear the
   *  first time someone opened Settings and pressed Save. */
  let loadedConfig: Record<string, unknown> = $state({});
  let dataDirectory = $state('');
  let compressionLevel = $state(3);
  let logLevel = $state<'info' | 'debug'>('info');
  let logMirrorDir = $state('');
  let serviceStatus: ServiceStatus | null = $state(null);
  let isLoading = $state(true);
  let error: string | null = $state(null);
  let saveMessage: string | null = $state(null);

  /** zstd levels offered. 0 is what a config written before the setting did
   *  anything holds; the engine reads anything outside 1–19 as 3, and so does
   *  the slider. */
  const MIN_LEVEL = 1;
  const MAX_LEVEL = 19;
  const DEFAULT_LEVEL = 3;

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
        const level = Number(cfg.default_compression_level);
        compressionLevel = level >= MIN_LEVEL && level <= MAX_LEVEL ? level : DEFAULT_LEVEL;
        // Two levels now. The older screen could save warn/error/trace; the
        // service reads those as the nearest of the two, and so does this.
        const saved = String(cfg.log_level ?? '').toLowerCase();
        logLevel = saved === 'debug' || saved === 'trace' ? 'debug' : 'info';
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
      // `data_dir` is not edited here (see the field), so it goes back as it
      // came, with the rest of `loadedConfig`.
      await api.saveConfig({
        ...loadedConfig,
        default_compression_level: compressionLevel,
        log_level: logLevel,
        log_mirror_dir: logMirrorDir.trim(),
      });
      saveMessage = 'Параметры сохранены.';
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  /** «2 ч 5 мин», not «7500s». */
  function formatUptime(secs: number): string {
    const d = Math.floor(secs / 86400);
    const h = Math.floor((secs % 86400) / 3600);
    const m = Math.floor((secs % 3600) / 60);
    if (d > 0) return `${d} д ${h} ч`;
    if (h > 0) return `${h} ч ${m} мин`;
    if (m > 0) return `${m} мин`;
    return `${secs} с`;
  }

  // --- What mounting a backup image needs here ---
  //
  // No driver of ours is installed on either platform any more: Windows serves
  // the image over an iSCSI target of ours to its own initiator, Linux uses the
  // in-tree NBD module. Shown as one line of the service's state; the mount
  // itself starts what it needs.
  // `$state<T | null>(null)`, not `let x: T | null = $state(null)`: with the
  // annotation on the variable, TypeScript reads the initialiser as literally
  // `null` and narrows every straight-line use below to `never` (audit AUD-13).
  let mountStatus = $state<MountBackendStatus | null>(null);

  $effect(() => {
    api.mountBackendStatus().then(
      (s) => (mountStatus = s),
      () => (mountStatus = null),
    );
  });

  /** One line on whether a copy can be connected as a disk, and why not. */
  const mountLine = $derived.by((): { ok: boolean; text: string } => {
    const s = mountStatus;
    if (!s) return { ok: false, text: 'неизвестно' };
    if (s.platform === 'linux') {
      if (!s.linux?.nbd_ok) return { ok: false, text: 'модуль NBD не загружен' };
      return s.linux.ntfs_ok
        ? { ok: true, text: `готово (NBD, NTFS: ${s.linux.ntfs_backend})` }
        : { ok: true, text: 'готово, но NTFS не поддерживается — нужен ntfs-3g' };
    }
    if (!s.iscsi) return { ok: false, text: 'неизвестно' };
    if (!s.iscsi.ready) return { ok: false, text: s.iscsi.message };
    return {
      ok: true,
      text: s.iscsi.running ? 'готово' : 'готово, служба iSCSI запустится по требованию',
    };
  });
</script>

<div class="settings-view">
  <div class="page-header">
    <h1 class="page-title">Параметры</h1>
  </div>

  {#if error}
    <div class="error-banner">
      <span>{error}</span>
      <button class="dismiss-btn" onclick={() => (error = null)}>Закрыть</button>
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
      <p>Загрузка параметров…</p>
    </div>
  {:else}
    <div class="settings-grid">
      <div class="card">
        <h2 class="section-title">Общие</h2>

        <!-- Shown, not edited: the service takes its folder from where it is
             installed and never read this value, so a field that accepted a
             new path changed nothing but the text in it (2026-09-26). -->
        <div class="form-group">
          <label for="data-dir">Папка программы</label>
          <input id="data-dir" type="text" class="form-input" value={dataDirectory} readonly />
          <p class="help-text">
            Здесь лежат задания, журналы и служебные файлы программы. Резервные
            копии хранятся там, где указано в каждом задании.
          </p>
        </div>

        <div class="form-group">
          <label for="comp-default">Уровень сжатия по умолчанию: {compressionLevel}</label>
          <input
            id="comp-default"
            type="range"
            min={MIN_LEVEL}
            max={MAX_LEVEL}
            class="form-range"
            bind:value={compressionLevel}
          />
          <div class="range-labels">
            <span>{MIN_LEVEL} — быстрее</span>
            <span>{MAX_LEVEL} — меньше файл</span>
          </div>
          <p class="help-text">
            Сжатие zstd для копий в форматах qcow2 и OBK — у заданий, где не
            выбран свой уровень («Схема» в параметрах задания). VHDX не
            сжимается. Чем выше уровень, тем меньше файл и дольше копирование;
            3 — разумное равновесие, выше 9 копирование заметно медленнее при
            небольшом выигрыше в размере. Действует со следующего запуска.
          </p>
        </div>

        <div class="form-group">
          <label for="log-level">Уровень журнала</label>
          <select id="log-level" class="form-select" bind:value={logLevel}>
            <option value="info">Основной — ход работы и ошибки</option>
            <option value="debug">Подробный — всё, для разбора ошибки</option>
          </select>
          <p class="help-text">
            Подробный журнал нужен, когда что-то пошло не так и нужно понять
            почему; он заметно больше. Применяется на ходу — перезапускать
            службу не нужно.
          </p>
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
          <button class="btn btn-primary" onclick={handleSave}>Сохранить</button>
        </div>
      </div>

      <div class="card">
        <h2 class="section-title">Состояние службы</h2>

        {#if serviceStatus?.connected}
          <div class="status-grid">
            <div class="status-item">
              <span class="status-label">Служба</span>
              <span class="status-value running">
                <span class="status-dot"></span>
                Работает
              </span>
            </div>
            <div class="status-item">
              <span class="status-label">Версия</span>
              <span class="status-value">{serviceStatus.version}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Время работы</span>
              <span class="status-value">{formatUptime(serviceStatus.uptime_secs)}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Выполняется заданий</span>
              <span class="status-value">{serviceStatus.active_jobs}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Копий подключено как диск</span>
              <span class="status-value">{serviceStatus.active_mounts?.length ?? 0}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Подключение копий как диска</span>
              <span class="status-value" class:running={mountLine.ok} class:failing={!mountLine.ok}>
                <span class="status-dot" class:offline={!mountLine.ok}></span>
                {mountLine.text}
              </span>
            </div>
          </div>
        {:else}
          <div class="status-offline">
            <span class="status-dot offline"></span>
            <span>Служба не запущена или не отвечает.</span>
          </div>
        {/if}
      </div>
    </div>
  {/if}
</div>

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

  /* Shown, not edited: it should not look like a field waiting for input. */
  .form-input[readonly] {
    color: var(--text-muted);
    cursor: default;
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

  .status-value.failing {
    color: var(--error);
    text-align: right;
  }
</style>
