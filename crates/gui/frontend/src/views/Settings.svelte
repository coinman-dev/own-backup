<script lang="ts">
  import { pickPath } from '../lib/pickPath.svelte';
  import { api } from '../lib/api';
  import type { ServiceStatus, MountBackendStatus } from '../lib/types';
  import { theme, setTheme, type ThemeMode } from '../lib/themeStore.svelte';
  import { lang, setLanguage, type LangMode } from '../lib/i18n.svelte';
  import { m } from '../lib/paraglide/messages';
  import { examplePath } from '../lib/paths';

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
  // A function, so the banner follows a change of language made after saving.
  let saveMessage: (() => string) | null = $state(null);

  /** zstd levels offered. 0 is what a config written before the setting did
   *  anything holds; the engine reads anything outside 1–19 as 3, and so does
   *  the slider. */
  const MIN_LEVEL = 1;
  const MAX_LEVEL = 19;
  const DEFAULT_LEVEL = 3;

  /** The service's answer is still on its way. Kept apart from `isLoading`:
   *  that one is the config, which the window holds itself and answers at once;
   *  this one crosses to the service, which looks for disks attached outside
   *  the program (`diskpart`) and takes seconds. The page used to wait for
   *  both, so every visit began with «Загрузка параметров…» (2026-09-26). */
  let statusLoading = $state(true);

  async function loadSettings() {
    isLoading = true;
    error = null;
    // Not awaited: the settings are drawn without it, the card fills in.
    statusLoading = true;
    api.getServiceStatus().then(
      (st) => { serviceStatus = st; },
      () => { serviceStatus = null; },
    ).finally(() => { statusLoading = false; });
    try {
      const cfg = await api.getConfig();
      loadedConfig = cfg;
      dataDirectory = (cfg.data_dir as string) ?? '';
      const level = Number(cfg.default_compression_level);
      compressionLevel = level >= MIN_LEVEL && level <= MAX_LEVEL ? level : DEFAULT_LEVEL;
      // Two levels now. The older screen could save warn/error/trace; the
      // service reads those as the nearest of the two, and so does this.
      const saved = String(cfg.log_level ?? '').toLowerCase();
      logLevel = saved === 'debug' || saved === 'trace' ? 'debug' : 'info';
      logMirrorDir = (cfg.log_mirror_dir as string) ?? '';
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
    const chosen = await pickPath({ directory: true, title: m.set_log_mirror_pick_title() });
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
      saveMessage = m.set_saved;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  /** «2 ч 5 мин», not «7500s». */
  function formatUptime(secs: number): string {
    const d = Math.floor(secs / 86400);
    const h = Math.floor((secs % 86400) / 3600);
    const min = Math.floor((secs % 3600) / 60);
    if (d > 0) return m.fmt_dur_d_h({ d, h });
    if (h > 0) return m.fmt_dur_h_min({ h, min });
    if (min > 0) return m.fmt_dur_min({ min });
    return m.fmt_dur_s({ s: secs });
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

  // --- The icon beside the clock ---
  //
  // Null while unknown and where there is nothing to switch (the boot medium,
  // not Windows): the row is then left out. It is one Windows user's choice
  // and takes effect at once, so it is not part of the form above and needs
  // no «Сохранить».
  let trayIcon = $state<boolean | null>(null);

  $effect(() => {
    api.trayIconEnabled().then(
      (on) => (trayIcon = on),
      () => (trayIcon = null),
    );
  });

  async function setTrayIcon(on: boolean) {
    error = null;
    try {
      await api.setTrayIconEnabled(on);
      trayIcon = on;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
      // The box was already ticked by the click; put it back to the truth.
      trayIcon = await api.trayIconEnabled().catch(() => null);
    }
  }

  /** The three theme tiles, each a small picture of the window in it: menu
   *  strip, two lines of text and a button. Applied at once — nothing to save. */
  const themeOptions: { mode: ThemeMode; label: () => string; side: string; bg: string; line: string }[] = [
    { mode: 'light', label: m.set_theme_light, side: '#221d49', bg: '#ffffff', line: '#e3e3ec' },
    { mode: 'dark', label: m.set_theme_dark, side: '#110f22', bg: '#1f1e2a', line: '#34334a' },
    {
      mode: 'system',
      label: m.set_theme_system,
      side: 'linear-gradient(135deg, #221d49 50%, #110f22 50%)',
      bg: 'linear-gradient(135deg, #ffffff 50%, #1f1e2a 50%)',
      line: '#9a9ab0',
    },
  ];

  /** Each language is named in itself, so it can be found by someone who
   *  cannot read the other one. */
  const languageOptions: { mode: LangMode; label: () => string }[] = [
    { mode: 'ru', label: () => 'Русский' },
    { mode: 'en', label: () => 'English' },
    { mode: 'system', label: m.set_language_system },
  ];

  /** One line on whether a copy can be connected as a disk, and why not. */
  const mountLine = $derived.by((): { ok: boolean; text: string } => {
    const s = mountStatus;
    if (!s) return { ok: false, text: m.common_unknown() };
    if (s.platform === 'linux') {
      if (!s.linux?.nbd_ok) return { ok: false, text: m.set_mount_no_nbd() };
      return s.linux.ntfs_ok
        ? { ok: true, text: m.set_mount_ready_nbd({ backend: s.linux.ntfs_backend ?? '' }) }
        : { ok: true, text: m.set_mount_no_ntfs() };
    }
    if (!s.iscsi) return { ok: false, text: m.common_unknown() };
    if (!s.iscsi.ready) return { ok: false, text: s.iscsi.message };
    return {
      ok: true,
      text: s.iscsi.running ? m.set_mount_ready() : m.set_mount_ready_on_demand(),
    };
  });
</script>

<div class="settings-view">
  <div class="page-header">
    <h1 class="page-title">{m.set_title()}</h1>
  </div>

  {#if error}
    <div class="error-banner">
      <span>{error}</span>
      <button class="dismiss-btn" onclick={() => (error = null)}>{m.common_close()}</button>
    </div>
  {/if}

  {#if saveMessage}
    <div class="success-banner">
      <span>{saveMessage()}</span>
      <button class="dismiss-btn" onclick={() => (saveMessage = null)}>{m.common_ok()}</button>
    </div>
  {/if}

  {#if isLoading}
    <div class="loading-state">
      <div class="spinner"></div>
      <p>{m.set_loading()}</p>
    </div>
  {:else}
    <div class="settings-grid">
      <div class="card">
        <h2 class="section-title">{m.set_general()}</h2>

        <!-- Shown, not edited: the service takes its folder from where it is
             installed and never read this value, so a field that accepted a
             new path changed nothing but the text in it (2026-09-26). -->
        <div class="form-group">
          <label for="data-dir">{m.set_data_dir()}</label>
          <input id="data-dir" type="text" class="form-input" value={dataDirectory} readonly />
          <p class="help-text">{m.set_data_dir_help()}</p>
        </div>

        <div class="form-group">
          <label for="comp-default">{m.set_compression({ level: compressionLevel })}</label>
          <input
            id="comp-default"
            type="range"
            min={MIN_LEVEL}
            max={MAX_LEVEL}
            class="form-range"
            style:--fill="{((compressionLevel - MIN_LEVEL) / (MAX_LEVEL - MIN_LEVEL)) * 100}%"
            bind:value={compressionLevel}
          />
          <div class="range-labels">
            <span>{m.set_compression_fast({ level: MIN_LEVEL })}</span>
            <span>{m.set_compression_small({ level: MAX_LEVEL })}</span>
          </div>
          <p class="help-text">{m.set_compression_help()}</p>
        </div>

        <div class="form-group">
          <label for="log-level">{m.set_log_level()}</label>
          <select id="log-level" class="form-select" bind:value={logLevel}>
            <option value="info">{m.set_log_info()}</option>
            <option value="debug">{m.set_log_debug()}</option>
          </select>
          <p class="help-text">{m.set_log_level_help()}</p>
        </div>

        <div class="form-group">
          <label for="log-mirror">{m.set_log_mirror()}</label>
          <div class="path-row">
            <input
              id="log-mirror"
              type="text"
              class="form-input"
              placeholder={m.set_log_mirror_placeholder({ example: examplePath('drive') })}
              bind:value={logMirrorDir}
            />
            <button class="btn btn-secondary" onclick={pickMirrorDir}>{m.common_browse()}</button>
          </div>
          <p class="help-text">{m.set_log_mirror_help()}</p>
        </div>

        <div class="form-actions">
          <button class="btn btn-primary" onclick={handleSave}>{m.common_save()}</button>
        </div>
      </div>

      <div class="side-column">
        <div class="card">
          <h2 class="section-title">{m.set_appearance()}</h2>
          <div class="field-label">{m.set_theme()}</div>
          <div class="theme-options">
            {#each themeOptions as o (o.mode)}
              <button
                class="theme-option"
                class:selected={theme.mode === o.mode}
                aria-pressed={theme.mode === o.mode}
                onclick={() => setTheme(o.mode)}
              >
                <span class="theme-preview">
                  <span class="tp-side" style:background={o.side}></span>
                  <span class="tp-body" style:background={o.bg}>
                    <span class="tp-line" style:background={o.line} style:width="60%"></span>
                    <span class="tp-line" style:background={o.line} style:width="85%"></span>
                    <span class="tp-button"></span>
                  </span>
                </span>
                <span class="theme-label">{o.label()}</span>
              </button>
            {/each}
          </div>
          <p class="help-text">{m.set_theme_help()}</p>

          <div class="field-label lang-label">{m.set_language()}</div>
          <div class="lang-options" role="group" aria-label={m.set_language()}>
            {#each languageOptions as o (o.mode)}
              <button
                class="lang-option"
                class:selected={lang.mode === o.mode}
                aria-pressed={lang.mode === o.mode}
                onclick={() => setLanguage(o.mode)}
              >{o.label()}</button>
            {/each}
          </div>
          <p class="help-text">{m.set_language_help()}</p>
        </div>

        <div class="card">
          <h2 class="section-title">{m.set_service()}</h2>

          {#if statusLoading}
            <div class="status-pending">{m.set_service_asking()}</div>
          {:else if serviceStatus?.connected}
            <div class="status-grid">
              <div class="status-item">
                <span class="status-label">{m.set_service_name()}</span>
                <span class="status-value running">
                  <span class="status-dot"></span>
                  {m.set_service_running()}
                </span>
              </div>
              <div class="status-item">
                <span class="status-label">{m.set_service_version()}</span>
                <span class="status-value">{serviceStatus.version}</span>
              </div>
              <div class="status-item">
                <span class="status-label">{m.set_service_uptime()}</span>
                <span class="status-value">{formatUptime(serviceStatus.uptime_secs)}</span>
              </div>
              <div class="status-item">
                <span class="status-label">{m.set_service_jobs()}</span>
                <span class="status-value">{serviceStatus.active_jobs}</span>
              </div>
              <div class="status-item">
                <span class="status-label">{m.set_service_mounts()}</span>
                <span class="status-value">{serviceStatus.active_mounts?.length ?? 0}</span>
              </div>
              <div class="status-item">
                <span class="status-label">{m.set_service_mounting()}</span>
                <span class="status-value" class:running={mountLine.ok} class:failing={!mountLine.ok}>
                  <span class="status-dot" class:offline={!mountLine.ok}></span>
                  {mountLine.text}
                </span>
              </div>
            </div>
          {:else}
            <div class="status-offline">
              <span class="status-dot offline"></span>
              <span>{m.set_service_offline()}</span>
            </div>
          {/if}

          {#if trayIcon !== null}
            <label class="tray-row">
              <input
                type="checkbox"
                checked={trayIcon}
                onchange={(e) => setTrayIcon(e.currentTarget.checked)}
              />
              {m.set_tray()}
            </label>
            <p class="help-text">{m.set_tray_help()}</p>
          {/if}
        </div>
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
    font-weight: 300;
    color: var(--head);
    margin: 0;
  }

  .error-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    background: var(--error-soft);
    border: 1px solid var(--error-line);
    border-radius: 8px;
    color: var(--error-text);
    margin-bottom: 20px;
    font-size: 13px;
  }

  .success-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    background: var(--success-soft);
    border: 1px solid var(--success-line);
    border-radius: 8px;
    color: var(--success-text);
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
    color: var(--head);
    margin: 0 0 16px;
  }

  .help-text {
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--text-muted);
    margin-top: 6px;
  }

  /* Shown, not edited: it should not look like a field waiting for input. */
  .form-input[readonly] {
    background: var(--sunken);
    border-color: var(--border);
    color: var(--text-secondary);
    cursor: default;
  }
  .form-input[readonly]:focus {
    border-color: var(--border);
    box-shadow: none;
  }

  /* Right column: «Оформление» above «Состояние службы». */
  .side-column {
    display: flex;
    flex-direction: column;
    gap: 24px;
    min-width: 0;
  }

  .field-label {
    font-size: 13.5px;
    font-weight: 600;
    color: var(--head);
    margin-bottom: 8px;
  }

  .theme-options {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 8px;
  }

  .theme-option {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 8px;
    border-radius: 8px;
    border: 2px solid var(--border);
    background: none;
    color: var(--text);
    font-size: 13px;
    transition: border-color 0.12s ease;
  }
  .theme-option:hover {
    border-color: var(--border-strong);
  }
  .theme-option.selected {
    border-color: var(--primary);
  }

  .theme-preview {
    height: 54px;
    border-radius: 5px;
    overflow: hidden;
    display: flex;
    border: 1px solid var(--border);
  }
  .tp-side {
    width: 26%;
  }
  .tp-body {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 8px;
  }
  .tp-line {
    height: 5px;
    border-radius: 3px;
  }
  /* The accent of the light theme on every tile: it is the same violet. */
  .tp-button {
    height: 9px;
    width: 36%;
    border-radius: 3px;
    margin-top: auto;
    align-self: flex-end;
    background: #5b4bd6;
  }
  .theme-label {
    text-align: center;
  }

  .lang-label {
    margin-top: 18px;
  }

  /* One control of three segments, the chosen one filled (the design's
     segmented choice, as for the backup method). */
  .lang-options {
    display: flex;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    overflow: hidden;
  }

  .lang-option {
    flex: 1;
    min-width: 0;
    padding: 8px 10px;
    border: none;
    border-right: 1px solid var(--border);
    background: var(--card-bg);
    color: var(--text);
    font-size: 13px;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .lang-option:last-child {
    border-right: none;
  }
  .lang-option:hover:not(.selected) {
    background: var(--panel2);
  }
  .lang-option.selected {
    background: var(--primary);
    color: #fff;
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

  /* Drawn by hand: the WebView's own track left its empty part a light grey
     strip on the dark theme. Filled to the thumb with the accent, the rest
     the theme's --track, as the progress bars are. */
  .form-range {
    width: 100%;
    height: 6px;
    margin: 8px 0;
    appearance: none;
    border-radius: 3px;
    background: linear-gradient(to right, var(--primary) var(--fill), var(--track) var(--fill));
    cursor: pointer;
  }
  .form-range::-webkit-slider-thumb {
    appearance: none;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: var(--primary);
    border: 2px solid var(--card-bg);
    box-shadow: 0 0 0 1px var(--primary);
  }
  .form-range:focus-visible {
    outline: 2px solid var(--primary);
    outline-offset: 4px;
  }

  .range-labels {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
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
    font-size: 13.5px;
    color: var(--text-secondary);
  }

  .status-value {
    font-size: 13.5px;
    font-weight: 600;
    color: var(--head);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .status-value.running {
    color: var(--success-text);
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

  .status-pending {
    font-size: 13px;
    color: var(--text-muted);
    padding: 4px 0;
  }

  .status-offline {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 16px;
    background: var(--error-soft);
    border-radius: 8px;
    font-size: 13px;
    color: var(--text-secondary);
  }

  .tray-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 18px;
    font-size: 13.5px;
    font-weight: 600;
    color: var(--head);
    cursor: pointer;
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }

  .status-value.failing {
    color: var(--error-text);
    text-align: right;
  }
</style>
