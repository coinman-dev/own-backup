<script lang="ts">
  import { api } from '../lib/api';
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  import { jobs, jobStatus, runJob, cancelRunning } from '../lib/jobStore.svelte';
  import InfoModal from '../components/InfoModal.svelte';
  import type { Job } from '../lib/types';

  interface Props {
    job: Job;
    onEdit: () => void;
  }
  let { job, onEdit }: Props = $props();

  let actionMsg: string | null = $state(null);
  let actionErr: string | null = $state(null);
  let stubMsg: string | null = $state(null);

  const status = $derived(jobStatus(job));
  const running = $derived(status === 'running');
  const progress = $derived(jobs.progress[job.id] ?? null);
  const verifying = $derived(progress?.phase === 'verify');
  const lastRun = $derived(job.last_run);

  // Elapsed clock + ETA from recent throughput (a sliding window), reset each
  // time a run (or a new phase) starts. "оценка…" until the window is wide.
  let nowMs = $state(Date.now());
  let etaMs = $state(NaN);
  let startMs = $state(0);
  let samples: { t: number; blocks: number }[] = [];
  let lastPhase = '';

  $effect(() => {
    const p = jobs.progress[job.id];
    if (!running || !p) {
      samples = [];
      lastPhase = '';
      return;
    }
    if (p.phase !== lastPhase) {
      lastPhase = p.phase;
      samples = [];
      startMs = Date.now();
    }
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
      const p = jobs.progress[job.id];
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

  // Random data-category breakdown — placeholder for looks (feature "в разработке").
  const cats = [
    { name: 'Изображения', color: '#5ab02f' },
    { name: 'Видео', color: '#d8c81e' },
    { name: 'Аудио', color: '#e07a2f' },
    { name: 'Документы', color: '#8b5cf6' },
    { name: 'Система', color: '#4f9be8' },
    { name: 'Другое', color: '#9aa0ad' },
  ];
  // Deterministic pseudo-random per job id, so it doesn't flicker on re-render.
  function seeded(i: number): number {
    let h = 0;
    for (const ch of job.id + i) h = (h * 31 + ch.charCodeAt(0)) & 0xffff;
    return (h % 90) + 10;
  }
  const totalWeight = cats.reduce((s, _, i) => s + seeded(i), 0);
  const catData = cats.map((c, i) => {
    const w = seeded(i);
    return { ...c, pct: (w / totalWeight) * 100 };
  });

  async function backup() {
    actionErr = null;
    try {
      await runJob(job.id);
    } catch (e) {
      actionErr = String(e);
    }
  }

  async function cancel() {
    await cancelRunning();
  }
</script>

<div class="dash">
  <div class="flow">
    <div class="card">
      <svg width="110" height="110" viewBox="0 0 24 24" fill="none" stroke="#9aa0b4" stroke-width="1.1">
        <rect x="3" y="5" width="18" height="14" rx="2" />
        <circle cx="12" cy="12" r="4.4" />
        <circle cx="12" cy="12" r="1" fill="#9aa0b4" />
      </svg>
      <div class="card-title">{job.name}</div>
      <div class="card-sub">{job.source_label || job.source}</div>
    </div>

    <div class="connector">
      {#if running && progress}
        <div class="ring" style="background: conic-gradient(var(--green) {progress.percent}%, var(--border) 0)">
          <div class="ring-inner">{progress.percent}%</div>
        </div>
      {:else if running}
        <div class="ring" style="background: conic-gradient(var(--green) 0%, var(--border) 0)">
          <div class="ring-inner">…</div>
        </div>
      {:else}
        <div class="dash-line"></div>
        <div class="check" class:ok={lastRun?.status === 'success'} class:err={lastRun?.status === 'error'}>
          {#if lastRun?.status === 'error'}
            <svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>
          {:else}
            <svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>
          {/if}
        </div>
        <div class="dash-line"></div>
      {/if}
    </div>

    <div class="card">
      <svg width="110" height="102" viewBox="0 0 24 22" fill="#e6b34a">
        <path d="M2 5a2 2 0 0 1 2-2h5l2 2.4h7a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z" />
      </svg>
      <div class="card-title">{job.name}</div>
      <div class="card-sub">{job.dest_folder}</div>
    </div>
  </div>

  {#if running}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>{verifying ? 'Проверка резервной копии…' : 'Резервное копирование…'}</span>
        {#if progress}
          <span>
            {progress.blocks_done} / {progress.blocks_total} блоков{#if progress.speed_mbps > 0} · {progress.speed_mbps.toFixed(1)} МБ/с{/if}
          </span>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" class:verify={verifying} style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="progress-meta">
        <span class="route" title="{job.source_label || job.source} → {job.dest_folder}">
          {job.source_label || job.source} → {job.dest_folder}
        </span>
        <span class="times">
          Прошло {formatClock(elapsedMs)} · Осталось {Number.isFinite(etaMs) ? '~' + formatClock(etaMs) : 'оценка…'}
        </span>
      </div>
    </div>
  {:else}
    <!-- stats row -->
    <div class="stats">
      <span>
        Последнее копирование:
        <b>{lastRun ? formatDate(lastRun.finished_at) : 'ещё не выполнялось'}</b>
      </span>
      <span class="reco">
        Данные для восстановления:
        <b>{lastRun ? formatBytes(lastRun.bytes_copied) : '—'}</b>
        <button class="q" title="Разбивка по типам — в разработке" onclick={() => (stubMsg = 'Разбивка данных по типам (изображения, видео, документы…) — в разработке. Сейчас показаны примерные данные для наглядности.')}>?</button>
      </span>
    </div>
    <div class="catbar">
      {#each catData as c}
        <div style="width:{c.pct}%; background:{c.color}"></div>
      {/each}
    </div>
    <div class="cats">
      {#each catData as c}
        <div class="cat">
          <span class="dot" style="background:{c.color}"></span>
          <span class="cat-name">{c.name}</span>
        </div>
      {/each}
    </div>

    {#if lastRun?.status === 'success'}
      <div class="banner ok">
        <span class="bdot"></span>
        <span>Копия создана{lastRun.vss_used ? ' (снимок VSS)' : ''}. {lastRun.smart_mode ? 'Только занятые секторы.' : 'Посекторно.'}</span>
      </div>
    {:else if lastRun?.status === 'error'}
      <div class="banner err"><span class="bdot red"></span><span>Ошибка: {lastRun.error}</span></div>
    {/if}
  {/if}

  {#if actionErr}<div class="banner err"><span class="bdot red"></span><span>{actionErr}</span></div>{/if}
  {#if actionMsg}<div class="banner info"><span class="bdot blue"></span><span>{actionMsg}</span></div>{/if}

  <div class="footer">
    <div class="hint">
      {#if status === 'queued'}
        В очереди на выполнение…
      {:else}
        Расписание: {job.schedule.kind === 'none' ? 'не задано (автозапуск — в разработке)' : job.schedule.kind}
      {/if}
    </div>
    <button class="btn outline" onclick={onEdit} disabled={running}>Параметры</button>
    {#if running}
      <button class="btn danger" onclick={cancel}>Отмена</button>
    {:else}
      <button class="btn green" onclick={backup}>Создать копию</button>
    {/if}
  </div>
</div>

{#if stubMsg}
  <InfoModal message={stubMsg} onClose={() => (stubMsg = null)} />
{/if}

<style>
  .dash { padding: 34px 44px; max-width: 900px; margin: 0 auto; display: flex; flex-direction: column; height: 100%; }
  .flow { display: flex; align-items: stretch; justify-content: center; }
  .card { flex: 1; max-width: 300px; border: 1px solid var(--border); border-radius: 6px; padding: 22px 18px; text-align: center; background: var(--card-bg); display: flex; flex-direction: column; align-items: center; gap: 8px; }
  .card-title { font-size: 20px; font-weight: 600; color: var(--head); }
  .card-sub { font-size: 12px; color: var(--text-muted); word-break: break-all; }
  .connector { width: 130px; flex-shrink: 0; display: flex; align-items: center; justify-content: center; gap: 8px; }
  .dash-line { width: 26px; height: 2px; background: repeating-linear-gradient(90deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .check { width: 52px; height: 52px; border-radius: 50%; background: var(--text-muted); display: flex; align-items: center; justify-content: center; }
  .check.ok { background: var(--green); box-shadow: 0 4px 14px rgba(90,176,47,.4); }
  .check.err { background: var(--error); }
  .ring { width: 60px; height: 60px; border-radius: 50%; display: flex; align-items: center; justify-content: center; }
  .ring-inner { width: 44px; height: 44px; border-radius: 50%; background: var(--card-bg); display: flex; align-items: center; justify-content: center; font-size: 13px; font-weight: 700; color: var(--head); }
  .progress-wrap { margin-top: 30px; }
  .progress-info { display: flex; justify-content: space-between; font-size: 13px; color: var(--head); margin-bottom: 8px; }
  .bar { height: 8px; border-radius: 4px; background: var(--tabbg); overflow: hidden; }
  .bar-fill { height: 100%; background: var(--green); transition: width .15s linear; }
  .bar-fill.verify { background: #5a8fd8; }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 60%; }
  .progress-meta .times { white-space: nowrap; flex-shrink: 0; }
  .stats { display: flex; justify-content: space-between; font-size: 13px; color: var(--text-muted); margin-top: 34px; }
  .stats b { color: var(--head); font-weight: 600; }
  .reco { display: flex; align-items: center; gap: 6px; }
  .q { width: 16px; height: 16px; border-radius: 50%; border: 1px solid var(--text-muted); background: none; color: var(--text-muted); font-size: 10px; cursor: pointer; padding: 0; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; margin-top: 10px; }
  .cats { display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap; }
  .cat { flex: 1; min-width: 110px; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; }
  .cat-name { font-size: 13px; color: var(--head); }
  .banner { margin-top: 22px; border-radius: 6px; padding: 13px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); font-size: 13px; color: var(--head); }
  .banner.err { background: rgba(216,74,74,.08); }
  .bdot { width: 18px; height: 18px; border-radius: 50%; background: var(--green); flex-shrink: 0; }
  .bdot.red { background: var(--error); }
  .bdot.blue { background: #5a8fd8; }
  .footer { margin-top: auto; padding-top: 20px; border-top: 1px solid var(--border); display: flex; align-items: center; gap: 14px; }
  .hint { flex: 1; font-size: 12px; color: var(--text-muted); }
  .btn { border-radius: 4px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn:disabled { opacity: .5; cursor: default; }
  .btn.outline { background: transparent; border: 1px solid var(--primary); color: var(--primary); }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover { background: var(--green-hover); }
  .btn.danger { background: transparent; border: 1px solid var(--error); color: var(--error); }
</style>
