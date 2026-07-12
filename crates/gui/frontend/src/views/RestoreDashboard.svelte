<script lang="ts">
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  import {
    engine,
    runRestore,
    cancelCurrent,
    clearRestoreResult,
  } from '../lib/engineStore.svelte';
  import { jobs } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import InfoModal from '../components/InfoModal.svelte';
  import DiskMap from '../components/DiskMap.svelte';
  import type { Job, DiskInfo, PartitionInfo, ImageInfo } from '../lib/types';

  interface Props {
    job: Job;
  }
  let { job }: Props = $props();

  let target = $state('');
  let confirmOpen = $state(false);
  let confirmText = $state('');
  let formError: string | null = $state(null);
  let stubMsg: string | null = $state(null);

  const lastRun = $derived(job.last_run);

  // The archived-disk card read from the IMAGE FILE's embedded metadata, so it
  // works WITHOUT jobs.json (e.g. a Boot-CD restore). Reloaded whenever the
  // selected copy's image path changes; null until loaded / for old images.
  let imageInfo = $state<ImageInfo | null>(null);
  $effect(() => {
    const path = lastRun?.status === 'success' ? lastRun.image_path : '';
    if (!path) {
      imageInfo = null;
      return;
    }
    let cancelled = false;
    api.readImageInfo(path).then(
      (info) => { if (!cancelled) imageInfo = info; },
      () => { if (!cancelled) imageInfo = null; },
    );
    return () => { cancelled = true; };
  });

  // Excluded-files line: prefer the count embedded in the image (works from a
  // file without jobs.json), fall back to the run record.
  const excluded = $derived({
    files: imageInfo?.excluded_files ?? lastRun?.excluded_files ?? 0,
    bytes: imageInfo?.excluded_bytes ?? lastRun?.excluded_bytes ?? 0,
  });

  // Real file-category breakdown of the archived data — same source as the
  // backup dashboard (collected during the backup's file walk).
  const CAT_META = [
    { key: 'images', name: 'Изображения', color: '#5ab02f' },
    { key: 'video', name: 'Видео', color: '#d8c81e' },
    { key: 'audio', name: 'Аудио', color: '#e07a2f' },
    { key: 'documents', name: 'Документы', color: '#8b5cf6' },
    { key: 'windows', name: 'Windows', color: '#4f9be8' },
    { key: 'linux', name: 'Linux', color: '#14b8a6' },
    { key: 'other', name: 'Другое', color: '#9aa0ad' },
  ] as const;
  const catData = $derived.by(() => {
    // Prefer the breakdown embedded in the image (works without jobs.json);
    // fall back to the run record for images taken before it was embedded.
    const c = imageInfo?.categories ?? lastRun?.categories;
    if (!c) return null;
    const total = CAT_META.reduce((s, m) => s + (c[m.key] ?? 0), 0);
    if (total <= 0) return null;
    return CAT_META.map((m) => ({
      ...m,
      bytes: c[m.key] ?? 0,
      pct: ((c[m.key] ?? 0) / total) * 100,
    }));
  });
  // --- Disk maps: top = the disk this copy captured (read-only), bottom = all
  // system disks to pick a restore target from (same visuals as the source tab).
  function diskNumber(d: DiskInfo): number {
    const m = d.device_id.match(/(\d+)\s*$/);
    return m ? parseInt(m[1], 10) : 0;
  }
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  function isBootableCopy(d: DiskInfo): boolean {
    return job.disk_source != null && job.disk_source.disk_number === diskNumber(d);
  }
  function diskHasSelection(d: DiskInfo): boolean {
    if (isBootableCopy(d)) return true;
    return d.partitions.some((p) => {
      const dev = partDevice(p);
      return !!dev && job.sources.includes(dev);
    });
  }
  // The archived disk map, in priority order:
  //  1. the snapshot embedded in the IMAGE FILE (works without jobs.json / on a
  //     Boot-CD; also correct when the original disk is detached),
  //  2. the snapshot stored on the job at creation (jobs.json),
  //  3. a live query of currently-attached disks (pre-snapshot backups).
  const backedUpDisks = $derived(
    imageInfo?.source_disk_layout
      ? [imageInfo.source_disk_layout]
      : job.source_disks && job.source_disks.length > 0
        ? job.source_disks
        : engine.disks.filter(diskHasSelection),
  );
  const systemDisks = $derived(engine.disks);

  // Single restore target for now (click a volume to select / clear it).
  function pickTarget(dev: string) {
    target = target === dev ? '' : dev;
  }

  const canRestore = $derived(lastRun?.status === 'success' && !!lastRun.image_path);
  const backupBusy = $derived(
    jobs.queue.current !== null || jobs.queue.pending.length > 0,
  );

  const running = $derived(engine.op === 'restore');
  const progress = $derived(engine.restoreProgress);
  const summary = $derived(engine.restoreSummary);
  const errorMsg = $derived(formError ?? engine.restoreError);

  // Elapsed + ETA from recent throughput.
  let nowMs = $state(Date.now());
  let etaMs = $state(NaN);
  let startMs = $state(0);
  let samples: { t: number; blocks: number }[] = [];

  $effect(() => {
    const p = engine.restoreProgress;
    if (!running || !p) {
      samples = [];
      return;
    }
    if (samples.length === 0) startMs = Date.now();
    const last = samples[samples.length - 1];
    if (last && last.blocks === p.blocks_done) return;
    const t = Date.now();
    samples.push({ t, blocks: p.blocks_done });
    while (samples.length > 2 && samples[0].t < t - 12000) samples.shift();
  });

  $effect(() => {
    if (!running) {
      etaMs = NaN;
      return;
    }
    const tick = () => {
      nowMs = Date.now();
      const p = engine.restoreProgress;
      if (!p || samples.length < 2) {
        etaMs = NaN;
        return;
      }
      const first = samples[0];
      const last = samples[samples.length - 1];
      const dt = last.t - first.t;
      const db = last.blocks - first.blocks;
      if (dt < 1500 || db <= 0) {
        etaMs = NaN;
        return;
      }
      etaMs = ((p.blocks_total - p.blocks_done) * dt) / db;
    };
    tick();
    const id = setInterval(tick, 500);
    return () => clearInterval(id);
  });

  const elapsedMs = $derived(running && startMs ? nowMs - startMs : 0);

  function askConfirm() {
    formError = null;
    if (!canRestore) {
      formError = 'У этой копии нет готового образа для восстановления.';
      return;
    }
    if (!target) {
      formError = 'Выберите том, куда восстановить.';
      return;
    }
    if (backupBusy) {
      formError = 'Дождитесь завершения резервного копирования.';
      return;
    }
    confirmText = '';
    confirmOpen = true;
  }

  async function doRestore() {
    if (confirmText !== 'ДА' || !lastRun) return;
    confirmOpen = false;
    clearRestoreResult();
    await runRestore(lastRun.image_path, target);
  }

  const targetLabel = $derived(engine.volumes.find((v) => v.device === target)?.label ?? target);
</script>

<div class="page">
<div class="dash">
  {#if !running}
    <!-- WHAT: the disk this copy captured — same read-only map as Параметры → Источник -->
    <div class="sec-head">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
      <span class="sec-title">Что восстановить</span>
      <span class="sec-sub">разделы, вошедшие в эту копию</span>
    </div>

    {#if engine.volumesLoading && engine.disks.length === 0}
      <div class="loading-note">Загрузка списка дисков…</div>
    {:else if backedUpDisks.length === 0}
      <div class="warn-note">Диск этой копии сейчас не подключён — карту показать нельзя.</div>
    {:else}
      <div class="disks">
        {#each backedUpDisks as d (d.device_id)}
          <DiskMap disk={d} selected={job.sources} selectedIndices={imageInfo?.backed_up_parts ?? []} readonly boot={imageInfo?.is_whole_disk ?? isBootableCopy(d)} />
        {/each}
      </div>
    {/if}

    {#if catData}
      <div class="cat-block">
        <div class="cat-head">Что в копии — по типам файлов</div>
        <div class="catbar">
          {#each catData as c}<div style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>{/each}
        </div>
        <div class="cats">
          {#each catData as c}
            <div class="cat"><span class="dot" style="background:{c.color}"></span><span class="cat-name">{c.name}</span><span class="cat-size">{formatBytes(c.bytes)}</span></div>
          {/each}
          {#if excluded.files > 0}
            <div class="cat"><span class="dot" style="background:#c0463f"></span><span class="cat-name">Исключено</span><span class="cat-size">{formatBytes(excluded.bytes)}</span></div>
          {/if}
        </div>
      </div>
    {/if}

    <!-- arrow down: choose where to restore -->
    <div class="connector">
      <div class="v-dash"></div>
      <div class="arrow-circle"><svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M19 12l-7 7-7-7"/></svg></div>
      <div class="v-dash"></div>
    </div>

    <!-- WHERE: pick a target disk/volume from the system (like + Добавить копию) -->
    <div class="sec-head">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
      <span class="sec-title">Куда восстановить</span>
      <span class="sec-sub">выберите том — он будет полностью перезаписан</span>
    </div>
    {#if systemDisks.length === 0}
      <div class="warn-note">Диски не найдены.</div>
    {:else}
      <div class="disks">
        {#each systemDisks as d (d.device_id)}
          <DiskMap disk={d} selected={target ? [target] : []} onToggle={(dev) => pickTarget(dev)} />
        {/each}
      </div>
    {/if}
  {/if}

  {#if running}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>Восстановление…</span>
        {#if progress}
          <span>{progress.blocks_done} / {progress.blocks_total} блоков · {progress.speed_mbps.toFixed(1)} МБ/с</span>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="progress-meta">
        <span class="route">{job.name} → {targetLabel}</span>
        <span class="times">Прошло {formatClock(elapsedMs)} · Осталось {Number.isFinite(etaMs) ? '~' + formatClock(etaMs) : 'оценка…'}</span>
      </div>
    </div>
  {/if}

  {#if summary && !running}
    <div class="banner ok"><span class="bdot"></span><span>Восстановление завершено: {formatBytes(summary.bytes_restored)} за {summary.duration_secs.toFixed(1)} с.</span></div>
  {/if}
  {#if errorMsg}<div class="banner err"><span class="bdot red"></span><span>{errorMsg}</span></div>{/if}
</div>

  <div class="footer">
    <div class="hint">
      {#if backupBusy}
        Идёт резервное копирование — восстановление недоступно.
      {:else}
        Восстанавливается последняя копия. Отдельные файлы — через «Инструменты → Подключить как диск».
      {/if}
    </div>
    {#if running}
      <button class="btn danger" onclick={cancelCurrent}>Отмена</button>
    {:else}
      <button class="btn green" onclick={askConfirm} disabled={!canRestore || backupBusy}>Восстановить</button>
    {/if}
  </div>
</div>

{#if stubMsg}<InfoModal message={stubMsg} onClose={() => (stubMsg = null)} />{/if}

{#if confirmOpen}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">Подтверждение восстановления</div>
      <p>Все данные на томе <b>{target}</b> будут безвозвратно перезаписаны копией <b>{job.name}</b> от {lastRun ? formatDate(lastRun.finished_at) : ''}.</p>
      <p>Введите <b>ДА</b> (заглавными), чтобы продолжить:</p>
      <input type="text" bind:value={confirmText} spellcheck="false" />
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (confirmOpen = false)}>Отмена</button>
        <button class="btn danger-solid" disabled={confirmText !== 'ДА'} onclick={doRestore}>Восстановить</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .page { height: 100%; display: flex; flex-direction: column; }
  .dash { flex: 1; overflow: auto; width: 100%; padding: 34px 44px; max-width: 900px; margin: 0 auto; display: flex; flex-direction: column; }
  .sec-head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .sec-title { font-size: 15px; font-weight: 700; color: var(--head); }
  .sec-sub { font-size: 13px; color: var(--text-muted); }
  .disks { display: flex; flex-direction: column; gap: 14px; }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .warn-note { font-size: 13px; color: #8c1d1d; background: rgba(216,74,74,.08); padding: 10px 14px; border-radius: 5px; }
  .cat-block { margin-top: 14px; padding: 14px 16px; border: 1px solid var(--border); border-radius: 6px; background: var(--card-bg); }
  .cat-head { font-size: 13px; font-weight: 600; color: var(--head); margin-bottom: 4px; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; margin-top: 10px; }
  .cats { display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap; }
  .cat { flex: 1; min-width: 130px; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--head); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .connector { display: flex; flex-direction: column; align-items: center; margin: 20px 0; }
  .connector .v-dash { width: 2px; height: 14px; background: repeating-linear-gradient(180deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 44px; height: 44px; border-radius: 50%; background: var(--green); display: flex; align-items: center; justify-content: center; box-shadow: 0 4px 14px rgba(90,176,47,.35); margin: 4px 0; }
  .progress-wrap { margin-top: 30px; }
  .progress-info { display: flex; justify-content: space-between; font-size: 13px; color: var(--head); margin-bottom: 8px; }
  .bar { height: 8px; border-radius: 4px; background: var(--tabbg); overflow: hidden; }
  .bar-fill { height: 100%; background: var(--primary); transition: width .15s linear; }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 60%; }
  .banner { margin-top: 22px; border-radius: 6px; padding: 13px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); font-size: 13px; color: var(--head); }
  .banner.err { background: rgba(216,74,74,.08); }
  .bdot { width: 18px; height: 18px; border-radius: 50%; background: var(--green); flex-shrink: 0; }
  .bdot.red { background: var(--error); }
  /* .footer is global (styles/global.css) — full-width bar, same on every page. */
  .hint { flex: 1; font-size: 12px; color: var(--text-muted); }
  .btn { border-radius: 4px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn:disabled { opacity: .5; cursor: default; }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover:not(:disabled) { background: var(--green-hover); }
  .btn.danger { background: transparent; border: 1px solid var(--error); color: var(--error); }
  .btn.ghost { background: none; border: 1px solid var(--border); color: var(--head); }
  .btn.danger-solid { background: var(--error); color: #fff; }
  .overlay { position: fixed; inset: 0; background: rgba(20,17,44,.5); display: flex; align-items: center; justify-content: center; z-index: 150; }
  .dialog { width: 440px; background: #fff; border-radius: 8px; padding: 24px; box-shadow: 0 20px 60px rgba(0,0,0,.3); }
  .dialog-title { font-size: 17px; font-weight: 600; color: var(--head); margin-bottom: 12px; }
  .dialog p { font-size: 13px; color: var(--text); margin-bottom: 10px; line-height: 1.5; }
  .dialog input { width: 100%; padding: 9px 12px; border: 1px solid var(--border); border-radius: 4px; font-size: 14px; margin-bottom: 16px; }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 10px; }
</style>
