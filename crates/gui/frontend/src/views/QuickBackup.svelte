<script lang="ts">
  import { save } from '@tauri-apps/plugin-dialog';
  import { api } from '../lib/api';
  import { formatBytes, formatClock } from '../lib/format';
  import {
    engine,
    refreshVolumes,
    runBackup,
    runVerify,
    cancelCurrent,
    clearBackupResult,
  } from '../lib/engineStore.svelte';
  import type { ViewName } from '../lib/types';

  interface Props {
    onNavigate: (view: ViewName, jobId?: string) => void;
  }
  let {}: Props = $props();

  // Local form state (safe to lose on tab switch); operation state is in the store.
  let source = $state('');
  let outputPath = $state('');
  let allSectors = $state(false);
  let mountedMsg: string | null = $state(null);
  let localError: string | null = $state(null);
  let busyAction: string | null = $state(null);

  const running = $derived(engine.op === 'backup');
  const progress = $derived(engine.backupProgress);
  const summary = $derived(engine.backupSummary);
  const errorMsg = $derived(localError ?? engine.backupError);

  // Elapsed clock + ETA from RECENT throughput (a sliding window), not the
  // cumulative average — the first samples include VSS setup time and a single
  // early data point extrapolates to absurd values. We show «оценка…» until
  // the window is wide enough to trust.
  let nowMs = $state(Date.now());
  let etaMs = $state(NaN);
  let samples: { t: number; blocks: number }[] = [];

  // Collect a throughput sample whenever the block count advances.
  $effect(() => {
    const p = engine.backupProgress;
    if (!running) {
      samples = [];
      return;
    }
    if (!p) return;
    const last = samples[samples.length - 1];
    if (last && last.blocks === p.blocks_done) return;
    const now = Date.now();
    samples.push({ t: now, blocks: p.blocks_done });
    const cutoff = now - 12000; // ~12s window
    while (samples.length > 2 && samples[0].t < cutoff) samples.shift();
  });

  // Tick elapsed and recompute ETA from the window twice a second.
  $effect(() => {
    if (!running) {
      etaMs = NaN;
      return;
    }
    const tick = () => {
      nowMs = Date.now();
      const p = engine.backupProgress;
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

  const elapsedMs = $derived(running ? nowMs - engine.backupStartMs : 0);

  $effect(() => {
    if (!source && engine.volumes.length > 0) {
      source = engine.volumes[0].device;
    }
  });

  function currentSourceLabel(): string {
    return engine.volumes.find((v) => v.device === source)?.label ?? source;
  }

  function defaultFileName(): string {
    const letter = source.replace(':', '') || 'X';
    const stamp = new Date()
      .toISOString()
      .slice(0, 16)
      .replace(/[-:]/g, '')
      .replace('T', '-');
    return `disk_${letter}_${stamp}.vhdx`;
  }

  async function browseOutput() {
    try {
      const picked = await save({
        title: 'Файл образа',
        defaultPath: defaultFileName(),
        filters: [{ name: 'Образ VHDX', extensions: ['vhdx'] }],
      });
      if (picked) outputPath = picked;
    } catch (e) {
      localError = String(e);
    }
  }

  async function startBackup() {
    if (!source || !outputPath) {
      localError = 'Укажите источник и путь к файлу образа.';
      return;
    }
    localError = null;
    mountedMsg = null;
    clearBackupResult();
    await runBackup(source, outputPath, allSectors, currentSourceLabel());
  }

  async function mountResult() {
    if (!engine.backupSummary) return;
    busyAction = 'mount';
    mountedMsg = null;
    try {
      const physical = await api.mountBackup(engine.backupSummary.image_path);
      mountedMsg = `Образ подключён (только чтение): ${physical}. Диск появится в проводнике.`;
    } catch (e) {
      localError = String(e);
    } finally {
      busyAction = null;
    }
  }

  async function unmountResult() {
    if (!engine.backupSummary) return;
    busyAction = 'unmount';
    try {
      await api.unmountBackup(engine.backupSummary.image_path);
      mountedMsg = 'Образ отключён.';
    } catch (e) {
      localError = String(e);
    } finally {
      busyAction = null;
    }
  }

  async function verifyResult() {
    if (!engine.backupSummary) return;
    busyAction = 'verify';
    mountedMsg = null;
    try {
      const v = await runVerify(engine.backupSummary.image_path);
      mountedMsg = v.valid
        ? `Целостность подтверждена: ${v.blocks_checked} блоков за ${v.duration_secs.toFixed(1)} с.`
        : `ОШИБКА: ${v.mismatched_blocks.length} повреждённых блоков!`;
    } catch (e) {
      localError = String(e);
    } finally {
      busyAction = null;
    }
  }
</script>

<div class="page">
  <div class="flow">
    <div class="card">
      <svg width="90" height="90" viewBox="0 0 24 24" fill="none" stroke="#9aa0b4" stroke-width="1.1">
        <rect x="3" y="5" width="18" height="14" rx="2" />
        <circle cx="12" cy="12" r="4.4" />
        <circle cx="12" cy="12" r="1" fill="#9aa0b4" />
      </svg>
      <div class="card-title">Источник</div>
      {#if engine.volumesLoading && engine.volumes.length === 0}
        <div class="loading-note">Загрузка списка томов…</div>
      {:else}
        <select bind:value={source} disabled={running}>
          {#each engine.volumes as v}
            <option value={v.device}>{v.label}</option>
          {/each}
        </select>
      {/if}
      <button class="link-btn" onclick={refreshVolumes} disabled={running || engine.volumesLoading}>
        {engine.volumesLoading ? 'Обновление…' : 'Обновить список'}
      </button>
      <label class="check">
        <input type="checkbox" bind:checked={allSectors} disabled={running} />
        Посекторно (все блоки)
      </label>
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
        <div class="arrow-dash"></div>
        <div class="go-circle" class:done={summary !== null}>
          <svg width="26" height="26" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round">
            {#if summary}
              <path d="M20 6 9 17l-5-5" />
            {:else}
              <path d="M5 12h14M13 6l6 6-6 6" />
            {/if}
          </svg>
        </div>
        <div class="arrow-dash"></div>
      {/if}
    </div>

    <div class="card">
      <svg width="90" height="84" viewBox="0 0 24 22" fill="#e6b34a">
        <path d="M2 5a2 2 0 0 1 2-2h5l2 2.4h7a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z" />
      </svg>
      <div class="card-title">Файл образа (.vhdx)</div>
      <input
        type="text"
        bind:value={outputPath}
        placeholder="D:\OwnBackup\disk_C.vhdx"
        disabled={running}
        spellcheck="false"
      />
      <button class="link-btn" onclick={browseOutput} disabled={running}>Обзор…</button>
    </div>
  </div>

  {#if running}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>Резервное копирование…</span>
        {#if progress}
          <span>
            {progress.blocks_done} / {progress.blocks_total} блоков ·
            {progress.speed_mbps.toFixed(1)} МБ/с
          </span>
        {:else}
          <span>подготовка…</span>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="progress-meta">
        <span class="route" title="{engine.backupFrom} → {engine.backupTo}">
          {engine.backupFrom} → {engine.backupTo}
        </span>
        <span class="times">
          Прошло {formatClock(elapsedMs)} · Осталось {Number.isFinite(etaMs)
            ? '~' + formatClock(etaMs)
            : 'оценка…'}
        </span>
      </div>
    </div>
  {/if}

  {#if summary && !running}
    <div class="banner ok">
      <div class="dot"></div>
      <div class="banner-text">
        Копия создана: {summary.image_path} —
        {formatBytes(summary.bytes_copied)} ({summary.used_blocks} из {summary.total_blocks} блоков)
        за {summary.duration_secs.toFixed(1)} с.
        Режим: {summary.smart_mode
          ? 'только занятые секторы (NTFS)'
          : 'посекторно — карта занятых секторов доступна пока только для NTFS-томов'}.
        Снимок VSS: {summary.vss_used ? 'да (консистентная копия)' : 'нет (копия «на живую»)'}.
      </div>
    </div>
    <div class="actions">
      <button class="btn outline" onclick={verifyResult} disabled={busyAction !== null}>
        {busyAction === 'verify' ? 'Проверка…' : 'Проверить'}
      </button>
      <button class="btn outline" onclick={mountResult} disabled={busyAction !== null}>
        {busyAction === 'mount' ? 'Подключение…' : 'Подключить как диск'}
      </button>
      <button class="btn outline" onclick={unmountResult} disabled={busyAction !== null}>
        Отключить
      </button>
    </div>
  {/if}

  {#if mountedMsg}
    <div class="banner info"><div class="dot blue"></div><div class="banner-text">{mountedMsg}</div></div>
  {/if}

  {#if errorMsg}
    <div class="banner err"><div class="dot red"></div><div class="banner-text">{errorMsg}</div></div>
  {/if}

  <div class="footer">
    <div class="hint">
      Перед копированием автоматически создаётся снимок VSS — можно бэкапить
      работающий системный диск. Если VSS недоступен, том копируется «на живую».
    </div>
    {#if running}
      <button class="btn danger" onclick={cancelCurrent}>Отмена</button>
    {:else}
      <button class="btn primary-green" onclick={startBackup}>Создать копию</button>
    {/if}
  </div>
</div>

<style>
  .page { padding: 36px 44px; max-width: 980px; margin: 0 auto; }
  .flow { display: flex; align-items: stretch; justify-content: center; gap: 0; }
  .card {
    flex: 1; max-width: 320px; border: 1px solid var(--border); border-radius: var(--radius);
    padding: 22px 20px 24px; text-align: center; background: var(--card-bg);
    display: flex; flex-direction: column; align-items: center; gap: 10px;
  }
  .card svg { margin: 4px auto 6px; }
  .card-title { font-size: 18px; font-weight: 600; color: var(--head); }
  .card select, .card input {
    width: 100%; padding: 9px 12px; border: 1px solid var(--border); border-radius: 4px;
    font-size: 13px; font-family: var(--font); color: var(--text); background: #fff;
  }
  .check { display: flex; align-items: center; gap: 8px; font-size: 13px; color: var(--text-secondary); cursor: pointer; }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 10px 0; }
  .link-btn { background: none; border: none; color: var(--primary); font-size: 13px; cursor: pointer; padding: 0; }
  .connector { width: 150px; flex-shrink: 0; display: flex; align-items: center; justify-content: center; gap: 8px; }
  .arrow-dash { width: 30px; height: 2px; background: repeating-linear-gradient(90deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .go-circle { width: 54px; height: 54px; border-radius: 50%; background: var(--primary); display: flex; align-items: center; justify-content: center; }
  .go-circle.done { background: var(--green); box-shadow: 0 4px 14px rgba(90, 176, 47, 0.4); }
  .ring { width: 62px; height: 62px; border-radius: 50%; display: flex; align-items: center; justify-content: center; }
  .ring-inner { width: 46px; height: 46px; border-radius: 50%; background: var(--card-bg); display: flex; align-items: center; justify-content: center; font-size: 13px; font-weight: 700; color: var(--head); }
  .progress-wrap { margin-top: 32px; }
  .progress-info { display: flex; justify-content: space-between; font-size: 13px; color: var(--head); margin-bottom: 8px; }
  .bar { height: 8px; border-radius: 4px; background: var(--tabbg); overflow: hidden; }
  .bar-fill { height: 100%; background: var(--green); transition: width 0.15s linear; }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 62%; }
  .progress-meta .times { white-space: nowrap; flex-shrink: 0; }
  .banner { margin-top: 24px; border-radius: var(--radius); padding: 14px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); }
  .banner.err { background: rgba(216, 74, 74, 0.08); }
  .dot { width: 20px; height: 20px; border-radius: 50%; background: var(--green); flex-shrink: 0; }
  .dot.blue { background: #5a8fd8; }
  .dot.red { background: var(--error); }
  .banner-text { font-size: 13px; color: var(--head); word-break: break-all; }
  .actions { margin-top: 14px; display: flex; gap: 10px; }
  .btn { border-radius: 4px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn:disabled { opacity: 0.6; cursor: default; }
  .btn.outline { background: transparent; border: 1px solid var(--primary); color: var(--primary); }
  .btn.primary-green { background: var(--green); color: #fff; padding: 11px 30px; }
  .btn.primary-green:hover { background: var(--green-hover); }
  .btn.danger { background: transparent; border: 1px solid var(--error); color: var(--error); }
  .footer { margin-top: 36px; padding-top: 16px; border-top: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; gap: 20px; }
  .hint { font-size: 12px; color: var(--text-muted); max-width: 560px; }
</style>
