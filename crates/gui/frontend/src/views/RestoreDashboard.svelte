<script lang="ts">
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  import {
    engine,
    runRestore,
    cancelCurrent,
    clearRestoreResult,
  } from '../lib/engineStore.svelte';
  import { jobs } from '../lib/jobStore.svelte';
  import InfoModal from '../components/InfoModal.svelte';
  import type { Job } from '../lib/types';

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

  // Real file-category breakdown of the archived data — same source as the
  // backup dashboard (collected during the backup's file walk).
  const CAT_META = [
    { key: 'images', name: 'Изображения', color: '#5ab02f' },
    { key: 'video', name: 'Видео', color: '#d8c81e' },
    { key: 'audio', name: 'Аудио', color: '#e07a2f' },
    { key: 'documents', name: 'Документы', color: '#8b5cf6' },
    { key: 'system', name: 'Система', color: '#4f9be8' },
    { key: 'other', name: 'Другое', color: '#9aa0ad' },
  ] as const;
  const catData = $derived.by(() => {
    const c = lastRun?.categories;
    if (!c) return null;
    const total = CAT_META.reduce((s, m) => s + (c[m.key] ?? 0), 0);
    if (total <= 0) return null;
    return CAT_META.map((m) => ({
      ...m,
      bytes: c[m.key] ?? 0,
      pct: ((c[m.key] ?? 0) / total) * 100,
    }));
  });
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

<div class="dash">
  <div class="flow">
    <!-- LEFT: archive (swapped) -->
    <div class="card">
      <svg width="110" height="102" viewBox="0 0 24 22" fill="#e6b34a">
        <path d="M2 5a2 2 0 0 1 2-2h5l2 2.4h7a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z" />
      </svg>
      <div class="card-title">{job.name}</div>
      <div class="card-sub">
        {#if lastRun?.status === 'success'}
          Копия от {formatDate(lastRun.finished_at)} — {formatBytes(lastRun.bytes_copied)}
        {:else}
          нет готовых копий
        {/if}
      </div>
    </div>

    <div class="connector">
      {#if running && progress}
        <div class="ring" style="background: conic-gradient(var(--primary) {progress.percent}%, var(--border) 0)">
          <div class="ring-inner">{progress.percent}%</div>
        </div>
      {:else if running}
        <div class="ring" style="background: conic-gradient(var(--primary) 0%, var(--border) 0)">
          <div class="ring-inner">…</div>
        </div>
      {:else}
        <div class="dash-line"></div>
        <div class="arrow-circle" class:done={summary !== null}>
          {#if summary}
            <svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>
          {:else}
            <svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14M13 6l6 6-6 6"/></svg>
          {/if}
        </div>
        <div class="dash-line"></div>
      {/if}
    </div>

    <!-- RIGHT: target disk (swapped) -->
    <div class="card">
      <svg width="110" height="110" viewBox="0 0 24 24" fill="none" stroke="#9aa0b4" stroke-width="1.1">
        <rect x="3" y="5" width="18" height="14" rx="2" />
        <circle cx="12" cy="12" r="4.4" />
        <circle cx="12" cy="12" r="1" fill="#9aa0b4" />
      </svg>
      <div class="card-title">Восстановить в</div>
      <select bind:value={target} disabled={running}>
        <option value="" disabled selected>— выберите том —</option>
        {#each engine.volumes as v}
          <option value={v.device}>{v.label}</option>
        {/each}
      </select>
      <div class="warn">Том будет полностью перезаписан</div>
    </div>
  </div>

  {#if !running}
    <div class="stats">
      <span>Последнее копирование: <b>{lastRun?.status === 'success' ? formatDate(lastRun.finished_at) : '—'}</b></span>
      <span class="reco">
        Данные для восстановления: <b>{lastRun?.status === 'success' ? formatBytes(lastRun.used_blocks > 0 ? lastRun.used_blocks * 1048576 : lastRun.bytes_copied) : '—'}</b>
        <button class="q" title="О разбивке по типам" onclick={() => (stubMsg = 'Разбивка по типам считается по файлам, вошедшим в копию (по расширениям), во время сканирования тома. Исключённые файлы не учитываются.')}>?</button>
      </span>
    </div>
    {#if catData}
      <div class="catbar">
        {#each catData as c}<div style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>{/each}
      </div>
      <div class="cats">
        {#each catData as c}
          <div class="cat"><span class="dot" style="background:{c.color}"></span><span class="cat-name">{c.name}</span><span class="cat-size">{formatBytes(c.bytes)}</span></div>
        {/each}
      </div>
    {:else}
      <div class="catbar empty-catbar"></div>
      <div class="cats-note">Разбивка по типам появится после следующего копирования.</div>
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
  .dash { padding: 34px 44px; max-width: 900px; margin: 0 auto; display: flex; flex-direction: column; height: 100%; }
  .flow { display: flex; align-items: stretch; justify-content: center; }
  .card { flex: 1; max-width: 300px; border: 1px solid var(--border); border-radius: 6px; padding: 22px 18px; text-align: center; background: var(--card-bg); display: flex; flex-direction: column; align-items: center; gap: 8px; }
  .card-title { font-size: 20px; font-weight: 600; color: var(--head); }
  .card-sub { font-size: 12px; color: var(--text-muted); word-break: break-all; }
  .card select { width: 100%; padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: #fff; color: var(--text); }
  .warn { font-size: 11px; color: var(--error); }
  .stats { display: flex; justify-content: space-between; font-size: 13px; color: var(--text-muted); margin-top: 34px; }
  .stats b { color: var(--head); font-weight: 600; }
  .reco { display: flex; align-items: center; gap: 6px; }
  .q { width: 16px; height: 16px; border-radius: 50%; border: 1px solid var(--text-muted); background: none; color: var(--text-muted); font-size: 10px; cursor: pointer; padding: 0; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; margin-top: 10px; }
  .cats { display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap; }
  .cat { flex: 1; min-width: 130px; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--head); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .empty-catbar { background: var(--tabbg); }
  .cats-note { font-size: 12px; color: var(--text-muted); margin-top: 12px; }
  .connector { width: 130px; flex-shrink: 0; display: flex; align-items: center; justify-content: center; gap: 8px; }
  .dash-line { width: 26px; height: 2px; background: repeating-linear-gradient(90deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 52px; height: 52px; border-radius: 50%; background: var(--primary); display: flex; align-items: center; justify-content: center; }
  .arrow-circle.done { background: var(--green); }
  .ring { width: 60px; height: 60px; border-radius: 50%; display: flex; align-items: center; justify-content: center; }
  .ring-inner { width: 44px; height: 44px; border-radius: 50%; background: var(--card-bg); display: flex; align-items: center; justify-content: center; font-size: 13px; font-weight: 700; color: var(--head); }
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
  .footer { margin-top: auto; padding-top: 20px; border-top: 1px solid var(--border); display: flex; align-items: center; gap: 14px; }
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
