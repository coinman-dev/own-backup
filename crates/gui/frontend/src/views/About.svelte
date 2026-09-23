<script lang="ts">
  // «Справка» — единственное место, где программа говорит о себе сама, и
  // единственное, где она говорит о лицензии. Поэтому здесь нет ни одного
  // вшитого утверждения, которое может разойтись с действительностью: версии,
  // связь со службой и папка данных спрашиваются у службы и у настроек.
  //
  // До 2026-09-18 тут лежала англоязычная заглушка, и она врала в трёх местах:
  // «MIT / Apache-2.0» вместо PolyForm Noncommercial, «Encryption AES-256-GCM»
  // (шифрование ушло вместе с форматом .obk) и «gRPC on localhost:50051»
  // (в бою это именованный канал, а порт не слушается вовсе). Ссылки вели на
  // github.com/user/own-backup — такого места нет.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { media, ensureMediaMode } from '../lib/mediaStore.svelte';
  import type { ServiceStatus, ServiceHealth } from '../lib/types';

  let status: ServiceStatus | null = $state(null);
  let health: ServiceHealth | null = $state(null);
  let dataDir = $state('');
  let logMirrorDir = $state('');

  onMount(async () => {
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
    <h2 class="app-name">My Own Backup</h2>
    <p class="app-description">
      Копии дисков и томов целиком, блок за блоком: в копию попадают только
      занятые кластеры, а исключённые файлы не попадают вовсе. Из копии можно
      восстановить раздел, диск или отдельные файлы, а с загрузочного носителя —
      и тот диск, с которого система загружается.
    </p>

    <div class="info-table">
      <div class="info-row">
        <span class="info-label">Версия интерфейса</span>
        <span class="info-value">{status?.version ?? '—'}</span>
      </div>
      <div class="info-row">
        <span class="info-label">Служба</span>
        <span class="info-value">{serviceState}</span>
      </div>
      <div class="info-row">
        <span class="info-label">Связь с службой</span>
        <span class="info-value">{link}</span>
      </div>
      <div class="info-row">
        <span class="info-label">Формат копий</span>
        <span class="info-value">динамический VHDX или qcow2 (сжатие zstd)</span>
      </div>
      <div class="info-row">
        <span class="info-label">Контрольные суммы</span>
        <span class="info-value">xxHash64 на каждый блок, в манифесте внутри образа</span>
      </div>
      <div class="info-row">
        <span class="info-label">Задания и журнал</span>
        <span class="info-value">SQLite{dataDir ? ` в ${dataDir}` : ''}</span>
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
        Подробности — в журналах рядом с программой:
        <code>own-backup-service.log</code> и <code>own-backup-gui.log</code>.
        Время в них записано по UTC.
      </li>
      <li>
        В «Параметрах» можно назвать папку для второй копии журнала — например,
        на флешке. Она пишется строка за строкой и нужна для того единственного
        случая, который обычный журнал описать не может: восстановление,
        уносящее машину вместе с диском, на котором лежит журнал.
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
    font-weight: 600;
    color: var(--text);
    margin: 0;
  }

  .card {
    margin-bottom: 18px;
  }

  .app-name {
    font-size: 20px;
    font-weight: 700;
    color: var(--text);
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
    font-weight: 500;
    color: var(--text);
    text-align: right;
  }

  .section-heading {
    font-size: 15px;
    font-weight: 600;
    color: var(--text);
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

  code {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 12px;
    color: var(--text);
  }
</style>
