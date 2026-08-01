<script lang="ts">
  // The build dialog: what goes on the medium, which drivers, where the ISO
  // lands — and then the queue of stages while it happens.
  //
  // One dialog serves both bases. The Linux one has no engine behind it yet, so
  // its button is disabled and says so; everything above the button is the same
  // conversation, which is the point of writing it once.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api } from '../lib/api';
  import { pickPath } from '../lib/pickPath.svelte';
  import { formatBytes } from '../lib/format';
  import type { BaseImage, DriverPackage, MediaBuildReport, MediaProgress } from '../lib/types';

  interface Props {
    kind: 'windows' | 'linux';
    /** Chosen base image; null for Linux, which has none yet. */
    base: BaseImage | null;
    onClose: () => void;
  }
  let { kind, base, onClose }: Props = $props();

  const isWindows = $derived(kind === 'windows');

  let drivers: DriverPackage[] = $state([]);
  let selected = $state(new Set<string>());
  let includeDrivers = $state(true);
  let loadingDrivers = $state(false);
  let driverError: string | null = $state(null);

  let outputDir = $state('');
  let building = $state(false);
  let progress: MediaProgress | null = $state(null);
  let report: MediaBuildReport | null = $state(null);
  let error: string | null = $state(null);

  // Only the classes a rescue medium can use are offered. The rest — display,
  // audio, printers — is what turned the first medium into 2.2 GB, and no one
  // scrolling a list of ninety packages wants to find that out by hand.
  const offered = $derived(drivers.filter((d) => d.recommended));
  const chosenBytes = $derived(
    offered.filter((d) => selected.has(d.inf)).reduce((s, d) => s + d.size_bytes, 0),
  );
  /** The build is over: the button becomes "Готово" and closes the dialog. */
  const finished = $derived(!!report);
  const canStart = $derived(isWindows && !building && !!base && outputDir.trim().length > 0);

  onMount(() => {
    if (isWindows) loadDrivers();
    // Default destination: where the program itself is, which is the folder the
    // user already knows how to find.
    api.getConfig().then((c) => { if (!outputDir) outputDir = c.data_dir || ''; }).catch(() => {});
    const un = listen<MediaProgress>('media-progress', (e) => (progress = e.payload));
    return () => { un.then((f) => f()); };
  });

  async function loadDrivers() {
    loadingDrivers = true;
    driverError = null;
    try {
      drivers = await api.listDriverPackages();
      selected = new Set(drivers.filter((d) => d.recommended).map((d) => d.inf));
    } catch (e) {
      driverError = String(e);
    } finally {
      loadingDrivers = false;
    }
  }

  function toggle(inf: string) {
    const next = new Set(selected);
    if (next.has(inf)) next.delete(inf); else next.add(inf);
    selected = next;
  }

  async function chooseFolder() {
    const dir = await pickPath({ directory: true, title: 'Куда сохранить ISO' });
    if (dir) outputDir = dir;
  }

  async function start() {
    if (!canStart) return;
    building = true;
    error = null;
    report = null;
    progress = null;
    const sep = outputDir.endsWith('\\') || outputDir.endsWith('/') ? '' : '\\';
    try {
      report = await api.buildBootMedia({
        base_wim: base?.path ?? '',
        output_iso: `${outputDir}${sep}OwnBackup.iso`,
        drivers: includeDrivers ? [...selected] : [],
        extra_drivers: '',
      });
    } catch (e) {
      error = String(e);
    } finally {
      building = false;
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={() => !building && onClose()}>
  <div class="dialog" onclick={(e) => e.stopPropagation()}>
    <div class="head">
      {isWindows
        ? 'Носитель на основе Windows Recovery Environment'
        : 'Носитель на основе Linux'}
      <button class="x" onclick={onClose} disabled={building} title="Закрыть">✕</button>
    </div>

    <div class="body">
      <!-- What goes in -->
      <div class="intro">
        {#if isWindows}
          В образ войдут: среда восстановления Windows
          {#if base}(сборка {base.build}, {base.language}){/if},
          программа My Own Backup (интерфейс, движок, командная строка),
          рантайм WebView2 для интерфейса и системные библиотеки, которых нет в
          WinPE. Носитель загружается сразу в программу; с него можно снять
          копию выключенной системы и восстановить её обратно.
        {:else}
          В образ войдут: минимальная система Linux, ядро с драйверами дисков и
          сети, программа My Own Backup и её интерфейс. Такой носитель
          пригодится там, где нет Windows для сборки, а также для машин, чью
          разметку и файловые системы удобнее читать из Linux (ext4, btrfs,
          XFS, LUKS).
        {/if}
      </div>

      {#if !isWindows}
        <div class="soon">
          Сборка ещё не реализована — здесь пока только описание того, что
          войдёт в носитель.
        </div>
      {/if}

      <!-- Drivers. Hidden once the build starts: the queue takes their place,
           because a list of checkboxes that can no longer be changed is only in
           the way of the thing the user is now watching. -->
      {#if isWindows && !building && !finished}
        <label class="opt">
          <input type="checkbox" bind:checked={includeDrivers} disabled={building} />
          <span>
            Добавить драйверы этого компьютера
            <span class="opt-note">
              без них носитель может не увидеть диски: именно поэтому
              «универсальные» носители не находят NVMe и RAID
            </span>
          </span>
        </label>

        {#if includeDrivers}
          <div class="drivers">
            {#if loadingDrivers}
              <div class="muted">Чтение списка драйверов…</div>
            {:else if driverError}
              <div class="err">{driverError}</div>
            {:else if drivers.length === 0}
              <div class="muted">Сторонних драйверов на этой машине не найдено.</div>
            {:else if offered.length === 0}
              <div class="muted">
                Драйверов дисков и сети на этой машине не нашлось — носитель
                соберётся на встроенных драйверах Windows.
              </div>
            {:else}
              <div class="drv-head">
                <span>
                  Диски и сеть: выбрано {selected.size} из {offered.length} ·
                  {formatBytes(chosenBytes)}
                </span>
                <span class="drv-actions">
                  <button class="lnk" onclick={() => (selected = new Set(offered.map((d) => d.inf)))}>все</button>
                  <button class="lnk" onclick={() => (selected = new Set())}>ни одного</button>
                </span>
              </div>
              <div class="drv-list">
                {#each offered as d (d.inf)}
                  <label class="drv">
                    <input type="checkbox" checked={selected.has(d.inf)}
                           onchange={() => toggle(d.inf)} disabled={building} />
                    <span class="drv-name">{d.original_name || d.inf}</span>
                    <span class="drv-class">{d.class_name || '—'}</span>
                    <span class="drv-prov">{d.provider}</span>
                    <span class="drv-size">{formatBytes(d.size_bytes)}</span>
                  </label>
                {/each}
              </div>
            {/if}
          </div>
        {/if}
      {/if}

      <!-- The queue -->
      {#if progress}
        <div class="queue">
          {#each progress.stages as st, i}
            {@const done = i < progress.stage_index || progress.finished}
            {@const now = i === progress.stage_index && !progress.finished}
            <div class="stage" class:done class:now>
              <span class="mark">{done ? '✓' : now ? '▸' : ''}</span>
              <span class="st-title">
                {st.title}
                {#if now && progress.detail}<span class="st-detail">— {progress.detail}</span>{/if}
              </span>
              <span class="st-bar">
                <span class="st-fill" style="width: {done ? 100 : now ? progress.percent : 0}%"></span>
              </span>
            </div>
          {/each}
        </div>
      {/if}

      {#if error}<div class="err">{error}</div>{/if}

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
          {#if report.temp_bytes_freed > 0}
            <div class="rep-row">
              <span>Убрано</span>
              <span>{formatBytes(report.temp_bytes_freed)} временных файлов</span>
            </div>
          {/if}
          {#each report.warnings as w}<div class="warn">{w}</div>{/each}
          <div class="rep-note">
            Запишите ISO на флешку (Rufus и подобные) или подключите как привод
            в виртуальной машине.
          </div>
        </div>
      {/if}
    </div>

    <!-- Pinned: the destination and the button stay reachable however long the
         list above grows. -->
    <div class="dest">
      <span class="lbl">Куда сохранить ISO</span>
      <input type="text" bind:value={outputDir} placeholder="D:\OwnBackup"
             spellcheck="false" disabled={building || finished} />
      <button class="btn outline" onclick={chooseFolder} disabled={building || finished}>
        Обзор…
      </button>
      {#if finished}
        <button class="btn primary" onclick={onClose}>Готово</button>
      {:else}
        <button class="btn primary" onclick={start} disabled={!canStart}
                title={isWindows ? '' : 'Сборка Linux-носителя ещё не реализована'}>
          {building ? 'Сборка…' : 'Начать'}
        </button>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,.45); display: flex; align-items: center; justify-content: center; z-index: 250; padding: 24px; }
  .dialog { width: 760px; max-width: 96vw; max-height: 92vh; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; color: var(--head); border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
  .x { background: none; border: none; color: var(--text-muted); font-size: 15px; cursor: pointer; padding: 2px 6px; }
  .body { padding: 16px 18px; overflow: auto; flex: 1; min-height: 0; }
  .intro { font-size: 13px; color: var(--head); line-height: 1.6; }
  .soon { margin-top: 10px; font-size: 13px; color: var(--warning); line-height: 1.5; }
  .opt { display: flex; align-items: flex-start; gap: 9px; font-size: 13px; color: var(--head); margin-top: 16px; cursor: pointer; }
  .opt input { margin-top: 2px; flex-shrink: 0; }
  .opt-note { display: block; font-size: 12px; color: var(--text-muted); line-height: 1.4; margin-top: 2px; }
  .drivers { margin-top: 10px; }
  .drv-head { display: flex; justify-content: space-between; align-items: center; font-size: 12px; color: var(--text-muted); margin-bottom: 6px; }
  .drv-actions { display: flex; gap: 12px; }
  .lnk { background: none; border: none; color: var(--primary); font-size: 12px; cursor: pointer; padding: 0; font-family: var(--font); }
  .drv-list { max-height: 220px; overflow: auto; border: 1px solid var(--border); border-radius: 5px; }
  .drv { display: grid; grid-template-columns: 20px 1fr 110px 1fr 80px; gap: 10px; align-items: center; padding: 6px 10px; font-size: 12px; cursor: pointer; color: var(--head); }
  .drv:hover { background: var(--hover-bg, rgba(127,127,127,.10)); }
  .drv.rec .drv-class { color: var(--green); }
  .drv-name, .drv-prov { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .drv-class { color: var(--text-muted); }
  .drv-prov { color: var(--text-muted); }
  .drv-size { text-align: right; color: var(--text-muted); }
  /* Pinned below the scrolling body, not inside it. */
  .dest { display: grid; grid-template-columns: auto 1fr auto auto; gap: 10px; align-items: center; padding: 12px 18px; border-top: 1px solid var(--border); flex-shrink: 0; background: var(--card-bg); border-radius: 0 0 8px 8px; }
  .lbl { font-size: 12px; color: var(--primary); }
  .dest input { min-width: 0; background: none; border: 1px solid var(--border); border-radius: 4px; padding: 7px 10px; font-size: 13px; color: var(--head); font-family: var(--font); }
  .btn { border-radius: 4px; padding: 8px 18px; font-size: 13px; font-family: var(--font); cursor: pointer; border: 1px solid var(--border); background: none; color: var(--head); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn:disabled { opacity: .5; cursor: default; }
  .queue { margin-top: 16px; border: 1px solid var(--border); border-radius: 5px; padding: 10px 12px; }
  /* The bar takes everything to the right of the title; the running stage's
     detail rides along with the title rather than holding a column of its own,
     which was empty on every other row and cut the bar short of the edge. */
  .stage { display: grid; grid-template-columns: 16px minmax(190px, auto) 1fr; gap: 10px; align-items: center; padding: 3px 0; font-size: 12px; color: var(--text-muted); }
  .st-title { white-space: nowrap; }
  .stage.now, .stage.done { color: var(--head); }
  .mark { color: var(--green); }
  .st-bar { height: 5px; background: var(--border); border-radius: 3px; overflow: hidden; }
  .st-fill { display: block; height: 100%; background: var(--green); transition: width .25s; }
  .st-detail { color: var(--text-muted); }
  .err { font-size: 12px; color: var(--error); margin-top: 12px; word-break: break-word; }
  .warn { font-size: 12px; color: var(--warning); margin-top: 6px; line-height: 1.4; }
  .muted { font-size: 13px; color: var(--text-muted); padding: 6px 0; }
  .report { margin-top: 14px; border: 1px solid var(--border); border-radius: 5px; padding: 12px 14px; }
  .rep-head { font-size: 14px; color: var(--head); margin-bottom: 8px; }
  .rep-row { display: flex; gap: 12px; font-size: 13px; margin-bottom: 4px; }
  .rep-row > span:first-child { flex: 0 0 90px; color: var(--primary); font-size: 12px; }
  .rep-row > span:last-child { color: var(--head); min-width: 0; word-break: break-word; }
  .rep-note { font-size: 12px; color: var(--text-muted); margin-top: 9px; line-height: 1.5; }
</style>
