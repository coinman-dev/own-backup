<script lang="ts">
  // «Справка» — единственное место, где программа говорит о себе сама, и
  // единственное, где она говорит о лицензии. Поэтому здесь нет ни одного
  // вшитого утверждения, которое может разойтись с действительностью: версии,
  // связь со службой и папка данных спрашиваются у службы и у настроек.
  //
  // До 2026-09-18 тут лежала англоязычная заглушка, и она врала в трёх местах:
  // называла не ту лицензию, что у программы, «Encryption AES-256-GCM»
  // (тогдашний .obk был удалён; нынешний — другой контейнер) и «gRPC on
  // localhost:50051» (в бою это именованный канал, а порт не слушается
  // вовсе). Ссылки вели на github.com/user/own-backup — такого места нет.
  //
  // 2026-09-26 — ещё четыре: «Версия интерфейса» показывала версию СЛУЖБЫ,
  // «Задания и журнал: SQLite» (задания давно в jobs.json), формата OBK в
  // списке не было, а журналы назывались по-старому, без даты.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { media, ensureMediaMode } from '../lib/mediaStore.svelte';
  import { appVersion, ensureAppVersion } from '../lib/appVersion.svelte';
  import type { ServiceStatus, ServiceHealth } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { esc } from '../lib/i18n.svelte';
  import { platform, isWindows, ensurePlatform } from '../lib/platformStore.svelte';

  let status: ServiceStatus | null = $state(null);
  let health: ServiceHealth | null = $state(null);
  let dataDir = $state('');
  let logMirrorDir = $state('');

  onMount(async () => {
    ensureAppVersion();
    void ensurePlatform();
    await ensureMediaMode();
    const [st, he, cfg] = await Promise.allSettled([
      api.getServiceStatus(),
      api.serviceHealth(),
      api.getConfig(),
    ]);
    if (st.status === 'fulfilled') status = st.value;
    if (he.status === 'fulfilled') health = he.value;
    if (cfg.status === 'fulfilled') {
      dataDir = (cfg.value.data_dir as string) ?? '';
      logMirrorDir = (cfg.value.log_mirror_dir as string) ?? '';
    }
  });

  // Как интерфейс разговаривает со службой. На Windows это именованный канал с
  // ограниченным доступом, и порт при этом не слушается — сказать «gRPC на
  // localhost:50051» было бы неправдой о том, что открыто в системе. В Linux —
  // Unix-сокет; его путь называет сам бэкенд (`platform_info`).
  const PIPE = '\\\\.\\pipe\\OwnBackup';
  const SOCKET = '/run/own-backup/service.sock';
  const link = $derived.by(() => {
    if (isWindows()) {
      return media.active ? m.about_link_media({ pipe: PIPE }) : m.about_link_service({ pipe: PIPE });
    }
    const path = platform.channel || SOCKET;
    return media.active ? m.about_link_socket_media({ path }) : m.about_link_socket({ path });
  });
  // Where the logs are. Windows keeps both beside the program; on Linux the
  // service (root) writes to /var/log and the window and the command line to
  // the person's own state folder.
  const logsHint = $derived(
    isWindows()
      ? m.about_hint_logs()
      : m.about_hint_logs_linux({
          service: esc(platform.serviceLogDir ?? '/var/log/own-backup'),
          ui: esc(platform.uiLogDir ?? '~/.local/state/own-backup/Logs'),
        }),
  );
  const serviceState = $derived.by(() => {
    if (!health) return m.about_svc_asking();
    if (media.active) return m.about_svc_media();
    if (health.reachable)
      return status?.version
        ? m.about_svc_responding_version({ version: status.version })
        : m.about_svc_responding();
    if (health.installed) return m.about_svc_not_responding();
    return m.about_svc_not_installed();
  });
</script>

<div class="about-view">
  <div class="page-header">
    <h1 class="page-title">{m.about_title()}</h1>
  </div>

  <div class="card">
    <h2 class="app-name">Own Backup</h2>
    <p class="app-description">{m.about_description()}</p>

    <div class="info-table">
      <div class="info-row">
        <span class="info-label">{m.about_ui_version()}</span>
        <span class="info-value">{appVersion.value || '—'}</span>
      </div>
      <div class="info-row">
        <span class="info-label">{m.about_service()}</span>
        <span class="info-value">{serviceState}</span>
      </div>
      <div class="info-row">
        <span class="info-label">{m.about_link()}</span>
        <span class="info-value">{link}</span>
      </div>
      <div class="info-row">
        <span class="info-label">{m.about_format()}</span>
        <span class="info-value">{m.about_format_value()}</span>
      </div>
      <div class="info-row">
        <span class="info-label">{m.about_checksums()}</span>
        <span class="info-value">{m.about_checksums_value()}</span>
      </div>
      <div class="info-row">
        <span class="info-label">{m.about_jobs()}</span>
        <span class="info-value">{dataDir ? m.about_jobs_files_in({ dir: dataDir }) : m.about_jobs_files()}</span>
      </div>
      {#if logMirrorDir}
        <div class="info-row">
          <span class="info-label">{m.about_log_mirror()}</span>
          <span class="info-value">{logMirrorDir}</span>
        </div>
      {/if}
    </div>
  </div>

  <div class="card">
    <h3 class="section-heading">{m.about_licence()}</h3>
    <p class="licence">
      <b>PolyForm Noncommercial 1.0.0.</b> {m.about_licence_terms()}
    </p>
    <p class="licence">{@html m.about_licence_commercial()}</p>
    <p class="licence muted">
      {m.about_licence_osi()}
      <code>THIRD-PARTY-NOTICES.txt</code>.
    </p>
  </div>

  <div class="card">
    <h3 class="section-heading">{m.about_trouble()}</h3>
    <ul class="hints">
      <li>{@html m.about_hint_activity()}</li>
      <li>{@html logsHint}</li>
      <li>{m.about_hint_mirror()}</li>
      {#if !media.active}
        <li>{m.about_hint_media()}</li>
      {/if}
    </ul>
  </div>
</div>

<style>
  .about-view {
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

  .card {
    margin-bottom: 18px;
  }

  .app-name {
    font-size: 20px;
    font-weight: 600;
    color: var(--head);
    margin: 0 0 8px;
  }

  .app-description {
    font-size: 14px;
    color: var(--text-secondary);
    line-height: 1.6;
    margin: 0 0 20px;
  }

  .info-table {
    border-top: 1px solid var(--border);
  }

  .info-row {
    display: flex;
    justify-content: space-between;
    gap: 24px;
    padding: 10px 0;
    border-bottom: 1px solid var(--border);
  }

  .info-label {
    font-size: 13px;
    color: var(--text-muted);
    flex-shrink: 0;
  }

  .info-value {
    font-size: 13px;
    font-weight: 600;
    color: var(--head);
    text-align: right;
  }

  .section-heading {
    font-size: 16px;
    font-weight: 600;
    color: var(--head);
    margin: 0 0 10px;
  }

  .licence {
    font-size: 13px;
    color: var(--text-secondary);
    line-height: 1.6;
    margin: 0 0 10px;
  }

  .licence.muted {
    color: var(--text-muted);
    margin-bottom: 0;
  }

  .hints {
    margin: 0;
    padding-left: 20px;
    font-size: 13px;
    color: var(--text-secondary);
    line-height: 1.6;
  }

  .hints li {
    margin-bottom: 8px;
  }

  .hints li:last-child {
    margin-bottom: 0;
  }

  /* The design's bullets are accent dots. */
  .hints li::marker {
    color: var(--primary);
  }

  /* The <b>/<code> in these come from translated messages ({@html}), which
     scoped selectors do not reach. */
  .licence :global(b),
  .hints :global(b) {
    color: var(--head);
  }

  code,
  .licence :global(code),
  .hints :global(code) {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 12px;
    color: var(--text);
    background: var(--sunken);
    padding: 1px 5px;
    border-radius: 4px;
  }
</style>
