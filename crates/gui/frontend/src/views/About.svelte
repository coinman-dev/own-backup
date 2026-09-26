<script lang="ts">
  // «Справка» — единственное место, где программа говорит о себе сама, и
  // единственное, где она говорит о лицензии. Поэтому здесь нет ни одного
  // вшитого утверждения, которое может разойтись с действительностью: версии,
  // связь со службой и папка данных спрашиваются у службы и у настроек.
  //
  // До 2026-09-18 тут лежала англоязычная заглушка, и она врала в трёх местах:
  // «MIT / Apache-2.0» вместо PolyForm Noncommercial, «Encryption AES-256-GCM»
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

  let status: ServiceStatus | null = $state(null);
  let health: ServiceHealth | null = $state(null);
  let dataDir = $state('');
  let logMirrorDir = $state('');

  onMount(async () => {
    ensureAppVersion();
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
  // localhost:50051» было бы неправдой о том, что открыто в системе.
  const link = $derived(
    media.active
      ? 'канал \\\\.\\pipe\\OwnBackup, служба запущена как процесс носителя'
      : 'канал \\\\.\\pipe\\OwnBackup с ограниченным доступом (сетевой порт не слушается)',
  );
  const serviceState = $derived.by(() => {
    if (!health) return 'опрашиваем…';
    if (media.active) return 'режим носителя: служба работает как процесс, в систему не устанавливается';
    if (health.reachable) return `отвечает${status?.version ? `, версия ${status.version}` : ''}`;
    if (health.installed) return 'установлена, но не отвечает — проверьте, запущена ли она';
    return 'не установлена';
  });
</script>

<div class="about-view">
  <div class="page-header">
    <h1 class="page-title">Справка</h1>
  </div>

  <div class="card">
    <h2 class="app-name">Own Backup</h2>
    <p class="app-description">
      Копии дисков и томов целиком, блок за блоком: в копию попадают только
      занятые кластеры, а исключённые файлы не попадают вовсе. Копию можно
      зашифровать паролем. Из копии можно восстановить раздел, диск или
      отдельные файлы, а с загрузочного носителя — и тот диск, с которого
      система загружается.
    </p>

    <div class="info-table">
      <div class="info-row">
        <span class="info-label">Версия интерфейса</span>
        <span class="info-value">{appVersion.value || '—'}</span>
      </div>
      <div class="info-row">
        <span class="info-label">Служба</span>
        <span class="info-value">{serviceState}</span>
      </div>
      <div class="info-row">
        <span class="info-label">Связь со службой</span>
        <span class="info-value">{link}</span>
      </div>
      <div class="info-row">
        <span class="info-label">Формат копий</span>
        <span class="info-value">
          VHDX, qcow2 (сжатие zstd) или OBK — зашифрованный (zstd и AES-256-GCM)
        </span>
      </div>
      <div class="info-row">
        <span class="info-label">Контрольные суммы</span>
        <span class="info-value">xxHash64 на каждый блок, в манифесте внутри образа</span>
      </div>
      <div class="info-row">
        <span class="info-label">Задания и история запусков</span>
        <span class="info-value">jobs.json и activity.json{dataDir ? ` в ${dataDir}` : ''}</span>
      </div>
      {#if logMirrorDir}
        <div class="info-row">
          <span class="info-label">Копия журнала</span>
          <span class="info-value">{logMirrorDir}</span>
        </div>
      {/if}
    </div>
  </div>

  <div class="card">
    <h3 class="section-heading">Лицензия</h3>
    <p class="licence">
      <b>PolyForm Noncommercial 1.0.0.</b> Программой можно бесплатно
      пользоваться в некоммерческих целях — дома, в учёбе, в исследованиях;
      исходный код открыт, его можно изучать и изменять, и производные работы
      остаются на тех же условиях.
    </p>
    <p class="licence">
      Коммерческое использование требует отдельной лицензии — условия в файле
      <code>COMMERCIAL-LICENSE.md</code> рядом с программой.
    </p>
    <p class="licence muted">
      Это не лицензия «open source» в смысле OSI: правильнее говорить
      «исходный код открыт, бесплатно для некоммерческого использования».
      Сторонние компоненты и их лицензии перечислены в
      <code>THIRD-PARTY-NOTICES.md</code>.
    </p>
  </div>

  <div class="card">
    <h3 class="section-heading">Если что-то не получилось</h3>
    <ul class="hints">
      <li>
        Раздел <b>«Активность»</b> хранит все прогоны — там видно, чем
        закончился каждый и с какой ошибкой.
      </li>
      <li>
        Подробности — в журналах в папке <code>Logs</code> рядом с программой:
        <code>own-backup-service-ГГГГ-ММ-ДД.log</code> и
        <code>own-backup-gui-ГГГГ-ММ-ДД.log</code>, по файлу на день, хранятся
        30 дней. Время в них записано по UTC. Чтобы разобраться в ошибке,
        включите в «Параметрах» подробный уровень журнала и повторите.
      </li>
      <li>
        В «Параметрах» можно дублировать журнал на флешку или другой диск —
        он останется, даже если восстановление унесёт диск, на котором лежит
        обычный журнал.
      </li>
      {#if !media.active}
        <li>
          Восстановить систему, которая не загружается, можно с загрузочного
          носителя: «Инструменты» → «Загрузочный носитель».
        </li>
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

  .licence b,
  .hints b {
    color: var(--head);
  }

  code {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 12px;
    color: var(--text);
    background: var(--sunken);
    padding: 1px 5px;
    border-radius: 4px;
  }
</style>
