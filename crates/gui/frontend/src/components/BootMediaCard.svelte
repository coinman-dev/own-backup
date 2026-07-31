<script lang="ts">
  // "Create a boot medium" — the wizard end of everything the engine can do
  // here: find a base image on this machine, put our program and WebView2 into
  // it, add this machine's drivers, and write the result as a bootable ISO.
  //
  // The whole thing runs in the service (it needs DISM and administrator
  // rights); this only chooses, starts, and reports.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api } from '../lib/api';
  import { pickPath } from '../lib/pickPath.svelte';
  import { formatBytes } from '../lib/format';
  import type { BaseImage, MediaBuildReport, MediaProgress } from '../lib/types';

  let bases: BaseImage[] = $state([]);
  let searching = $state(false);
  let searchError: string | null = $state(null);
  let chosen = $state('');
  let outputIso = $state('');
  let includeDrivers = $state(true);
  let building = $state(false);
  let progress: MediaProgress | null = $state(null);
  let report: MediaBuildReport | null = $state(null);
  let buildError: string | null = $state(null);

  const selected = $derived(bases.find((b) => b.path === chosen) ?? bases[0] ?? null);

  // Nothing found is not a failure of ours — the machine is missing a file, and
  // each of these is a way to get one.
  const advice = [
    'Включить среду восстановления:  reagentc /enable',
    'Указать Winre.wim вручную, если он лежит в другом месте',
    'Достать его из установочного образа Windows',
  ];

  onMount(() => {
    search();
    const un = listen<MediaProgress>('media-progress', (e) => (progress = e.payload));
    return () => { un.then((f) => f()); };
  });

  async function search() {
    searching = true;
    searchError = null;
    try {
      bases = await api.findBaseImages();
      if (bases.length > 0 && !chosen) chosen = bases[0].path;
    } catch (e) {
      searchError = String(e);
    } finally {
      searching = false;
    }
  }

  async function chooseOutput() {
    const dir = await pickPath({ directory: true, title: 'Куда положить ISO' });
    if (dir) outputIso = `${dir}${dir.endsWith('\\') ? '' : '\\'}OwnBackup.iso`;
  }

  async function build() {
    if (!outputIso.trim()) return;
    building = true;
    buildError = null;
    report = null;
    progress = null;
    try {
      report = await api.buildBootMedia({
        base_wim: selected?.path ?? '',
        output_iso: outputIso,
        include_drivers: includeDrivers,
        extra_drivers: '',
      });
    } catch (e) {
      buildError = String(e);
    } finally {
      building = false;
      progress = null;
    }
  }
</script>

<div class="tool-card">
  <div class="tool-head">
    <span class="glyph">🚀</span>
    <div>
      <div class="tool-title">Создать загрузочный носитель</div>
      <div class="tool-desc">
        Носитель собирается из того, что уже есть на этой машине: среда
        восстановления Windows, драйверы этого компьютера и WebView2. С него
        можно снять копию выключенной системы и восстановить её обратно.
      </div>
    </div>
  </div>

  <!-- Base image -->
  <div class="block">
    <div class="lbl">Основа образа</div>
    {#if searching}
      <div class="muted-text">Поиск образа среды восстановления…</div>
    {:else if searchError}
      <div class="err">{searchError}</div>
    {:else if bases.length === 0}
      <div class="muted-text">
        На этой машине не нашлось образа среды восстановления. Что можно сделать:
      </div>
      <ul class="advice">
        {#each advice as a}<li>{a}</li>{/each}
      </ul>
      <button class="btn outline" onclick={search}>Искать снова</button>
    {:else}
      {#each bases as b}
        <label class="base" class:on={chosen === b.path}>
          <input type="radio" name="base" value={b.path} checked={chosen === b.path}
                 onchange={() => (chosen = b.path)} disabled={building} />
          <span class="base-body">
            <span class="base-name">{b.name || 'Образ восстановления'}</span>
            <span class="base-meta">
              сборка {b.build} · {b.language || '—'} · {b.architecture} ·
              {formatBytes(b.total_bytes)} · {b.origin}
            </span>
            <span class="base-path" title={b.path}>{b.path}</span>
            {#each b.warnings as w}<span class="warn">{w}</span>{/each}
          </span>
        </label>
      {/each}
    {/if}
  </div>

  <!-- Where the ISO goes -->
  <div class="block">
    <div class="lbl">Куда сохранить ISO</div>
    <div class="row">
      <input type="text" bind:value={outputIso} placeholder="D:\OwnBackup.iso"
             spellcheck="false" disabled={building} />
      <button class="btn outline btn-browse" onclick={chooseOutput} disabled={building}>
        Обзор…
      </button>
    </div>
    <label class="opt">
      <input type="checkbox" bind:checked={includeDrivers} disabled={building} />
      <span>
        Добавить драйверы этого компьютера
        <span class="opt-note">
          без них носитель может не увидеть диски: именно поэтому «универсальные»
          носители не находят NVMe и RAID
        </span>
      </span>
    </label>
  </div>

  <!-- Action -->
  <div class="row row-actions">
    <button class="btn primary" onclick={build}
            disabled={building || bases.length === 0 || !outputIso.trim()}>
      {building ? 'Сборка…' : 'Создать носитель'}
    </button>
    {#if building && progress}
      <span class="prog">
        <span class="bar"><span class="fill" style="width:{progress.percent}%"></span></span>
        <span class="prog-txt">
          {progress.phase}{progress.detail ? ` — ${progress.detail}` : ''}
        </span>
      </span>
    {:else if building}
      <span class="muted-text">Подготовка…</span>
    {/if}
  </div>

  {#if buildError}
    <div class="err">{buildError}</div>
  {/if}

  {#if report}
    <div class="report">
      <div class="rep-head">Носитель готов</div>
      <div class="rep-row"><span>Файл</span><span>{report.iso_path}</span></div>
      <div class="rep-row"><span>Размер</span><span>{formatBytes(report.iso_bytes)}</span></div>
      <div class="rep-row">
        <span>Основа</span>
        <span>{report.base_name} · сборка {report.base_build} · {report.base_language}</span>
      </div>
      <div class="rep-row">
        <span>Драйверы</span>
        <span>{report.drivers_added > 0 ? `${report.drivers_added} шт.` : 'не добавлялись'}</span>
      </div>
      {#each report.warnings as w}<div class="warn">{w}</div>{/each}
      <div class="rep-note">
        Запишите ISO на флешку (Rufus и подобные) или подключите как привод в
        виртуальной машине. Носитель загружается сразу в программу.
      </div>
    </div>
  {/if}
</div>

<style>
  .block { margin-top: 14px; }
  .lbl { font-size: 12px; color: var(--primary); margin-bottom: 7px; }
  .base { display: flex; gap: 10px; align-items: flex-start; padding: 9px 11px; border: 1px solid var(--border); border-radius: 5px; margin-bottom: 7px; cursor: pointer; }
  .base.on { border-color: var(--primary); }
  .base input { margin-top: 3px; flex-shrink: 0; }
  .base-body { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .base-name { font-size: 13px; color: var(--head); }
  .base-meta { font-size: 12px; color: var(--text-muted); }
  .base-path { font-size: 11px; color: var(--text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .warn { font-size: 12px; color: var(--warning); margin-top: 4px; line-height: 1.4; }
  .err { font-size: 12px; color: var(--error); margin-top: 10px; word-break: break-word; }
  .advice { margin: 8px 0 10px 18px; font-size: 13px; color: var(--head); line-height: 1.7; }
  .opt { display: flex; align-items: flex-start; gap: 9px; font-size: 13px; color: var(--head); margin-top: 10px; cursor: pointer; }
  .opt input { margin-top: 2px; flex-shrink: 0; }
  .opt-note { display: block; font-size: 12px; color: var(--text-muted); line-height: 1.4; margin-top: 2px; }
  .prog { display: flex; align-items: center; gap: 10px; flex: 1; min-width: 0; }
  .bar { flex: 0 0 160px; height: 6px; background: var(--border); border-radius: 3px; overflow: hidden; }
  .fill { display: block; height: 100%; background: var(--green); transition: width .3s; }
  .prog-txt { font-size: 12px; color: var(--text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .report { margin-top: 14px; border: 1px solid var(--border); border-radius: 5px; padding: 12px 14px; }
  .rep-head { font-size: 14px; color: var(--head); margin-bottom: 8px; }
  .rep-row { display: flex; gap: 12px; font-size: 13px; margin-bottom: 4px; }
  .rep-row > span:first-child { flex: 0 0 90px; color: var(--primary); font-size: 12px; }
  .rep-row > span:last-child { color: var(--head); min-width: 0; word-break: break-word; }
  .rep-note { font-size: 12px; color: var(--text-muted); margin-top: 9px; line-height: 1.5; }
</style>
