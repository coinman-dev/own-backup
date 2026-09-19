<script lang="ts">
  import { media } from '../lib/mediaStore.svelte';
  import { api } from '../lib/api';
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  import {
    jobs, jobStatus, sourceSummary, runJob, cancelRunning, runClock, etaMs,
  } from '../lib/jobStore.svelte';
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
  const preparing = $derived(progress?.phase === 'prepare');
  const finalizing = $derived(progress?.phase === 'finalize');
  // The post-copy exclusion pass (true removal / in-image exclusions): minutes
  // of real work that used to hide under the "сброс кэша" label and read as a
  // hang — 7 of the 26 minutes of a real run (2026-08-07).
  const excluding = $derived(progress?.phase === 'exclusions');
  /* Бегущая полоса — только когда считать нечего. У удаления исключённых
     файлов итог есть (сколько путей велено удалить), поэтому там обычная
     шкала и честное «Осталось»; «сброс кэша» итога не имеет никогда. */
  const indetBar = $derived(
    progress?.phase === 'finalize' || (excluding && (progress?.blocks_total ?? 0) === 0),
  );
  const lastRun = $derived(job.last_run);
  // The job asks for exclusions and the copy did not apply them. Reporting
  // "Исключено файлов: 0" for that reads as "the rules matched nothing", which
  // is the opposite of what happened — the rules were never run.
  const exclusionsIgnored = $derived(
    !!lastRun?.file_scan_skipped &&
      lastRun.excluded_files === 0 &&
      job.exclusions.enabled &&
      job.exclusions.patterns.length > 0,
  );

  // "Том 2/3 · C: Windows — " prefix while a multi-volume job runs.
  const volTag = $derived.by(() => {
    const p = progress;
    if (!p || !p.volume_count || p.volume_count <= 1) return '';
    const n = (p.volume_index ?? 0) + 1;
    return `Том ${n}/${p.volume_count}${p.volume ? ` · ${p.volume}` : ''} — `;
  });

  const KIND_LABELS: Record<string, string> = {
    incremental: 'инкрементная',
    differential: 'дифференциальная',
  };
  const kindLabel = (k: string) => KIND_LABELS[k] ?? 'полная';

  // The round indicator spans BOTH phases when "проверять после создания" is on:
  // 0–50 % = backup, 50–100 % = verify. The bar below stays per-phase.
  // "prepare" (exclusion scan) sits at 0 %; "finalize" (flush to disk) means the
  // copy is done, so the ring holds at the end of the backup half.
  const ringPercent = $derived.by(() => {
    const p = progress;
    if (!p || p.phase === 'prepare') return 0;
    if (p.phase === 'finalize' || p.phase === 'exclusions')
      return job.advanced.validate_after ? 50 : 100;
    if (job.advanced.validate_after) {
      return Math.round(p.phase === 'verify' ? 50 + p.percent / 2 : p.percent / 2);
    }
    return p.percent;
  });

  // Elapsed clock + ETA from recent throughput. Both are counted in the job
  // store: this view is unmounted every time the user walks through the menu,
  // and a clock kept here started again from zero on the way back.
  let nowMs = $state(Date.now());
  const eta = $derived(running ? etaMs(job.id) : NaN);

  // The only thing this view still ticks: the "Прошло" line, which has to
  // move on its own between progress messages.
  $effect(() => {
    if (!running) return;
    nowMs = Date.now();
    const id = setInterval(() => (nowMs = Date.now()), 500);
    return () => clearInterval(id);
  });

  const elapsedMs = $derived.by(() => {
    const c = runClock[job.id];
    return running && c ? nowMs - c.startMs : 0;
  });

  // Destination as shown in the progress line: the folder plus the file this
  // run is writing into it — "D:\Temp" alone does not say WHAT is appearing
  // there, and with several copies in one folder the name is the identity.
  const destPath = $derived.by(() => {
    const name = progress?.image_file_name;
    if (!name) return job.dest_folder;
    return job.dest_folder.replace(/[\\/]+$/, '') + '\\' + name;
  });

  // Real data-category breakdown, collected during the backup's file walk.
  const CAT_META = [
    { key: 'images', name: 'Изображения', color: '#5ab02f' },
    { key: 'video', name: 'Видео', color: '#d8c81e' },
    { key: 'audio', name: 'Аудио', color: '#e07a2f' },
    { key: 'documents', name: 'Документы', color: '#8b5cf6' },
    { key: 'windows', name: 'Windows', color: '#4f9be8' },
    { key: 'linux', name: 'Linux', color: '#14b8a6' },
    { key: 'other', name: 'Другое', color: '#9aa0ad' },
  ] as const;
  // Сводка, присланная сразу после скана: приходит одним сообщением, а
  // дальше её переносит из сообщения в сообщение хранилище прогона — здесь
  // хранить её нельзя, это представление живёт только пока на него смотрят.
  const liveCategories = $derived.by(() => {
    const pairs = progress?.categories;
    return pairs && pairs.length ? Object.fromEntries(pairs) : null;
  });

  const catData = $derived.by(() => {
    // Во время копирования — свежая сводка от сканера; после — из результата.
    const c = liveCategories ?? lastRun?.categories;
    if (!c) return null;
    const total = CAT_META.reduce((s, m) => s + (c[m.key] ?? 0), 0);
    if (total <= 0) return null;
    return CAT_META.map((m) => ({
      ...m,
      bytes: c[m.key] ?? 0,
      pct: ((c[m.key] ?? 0) / total) * 100,
    }));
  });

  // The bar covers the whole scanned volume: what the copy holds, plus what the
  // exclusions left out (last segment). Shares are recomputed against that grand
  // total so the bar still adds up to 100%.
  const barData = $derived.by(() => {
    if (!catData) return null;
    const kept = catData.reduce((s, c) => s + c.bytes, 0);
    const ex = lastRun?.excluded_bytes ?? 0;
    const total = kept + ex;
    if (total <= 0) return null;
    const out: { name: string; color: string; bytes: number; pct: number }[] = catData.map((c) => ({
      name: c.name,
      color: c.color,
      bytes: c.bytes,
      pct: (c.bytes / total) * 100,
    }));
    if (ex > 0) out.push({ name: 'Исключено', color: '#c0463f', bytes: ex, pct: (ex / total) * 100 });
    return out;
  });

  // Collapsible "Копия создана…" summary card; its state is remembered across
  // sessions (localStorage), per this job so hiding one doesn't hide others.
  //
  // The key has to FOLLOW the job. Svelte reuses this component when the
  // selected job changes rather than remounting it, so a key computed once at
  // creation stayed on whichever job happened to be open first — hiding the
  // card on one job then hid it on every other, which is the exact thing the
  // per-job key exists to prevent (audit AUD-13, reactivity warning).
  const SUMMARY_KEY = $derived(`hideRunSummary:${job.id}`);
  let summaryHidden = $state(false);
  // Only READING reacts to the key. Writing happens where the user actually
  // folds the card: an effect that both read and wrote would fire on the
  // switch to another job and save the previous job's value under the new key.
  $effect(() => {
    summaryHidden =
      typeof localStorage !== 'undefined' && localStorage.getItem(SUMMARY_KEY) === '1';
  });
  function rememberSummaryState(hidden: boolean) {
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem(SUMMARY_KEY, hidden ? '1' : '0');
    }
  }

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

<div class="page">
<div class="dash">
  <div class="flow">
    <div class="card">
      <svg width="110" height="110" viewBox="0 0 24 24" fill="none" stroke="#9aa0b4" stroke-width="1.1">
        <rect x="3" y="5" width="18" height="14" rx="2" />
        <circle cx="12" cy="12" r="4.4" />
        <circle cx="12" cy="12" r="1" fill="#9aa0b4" />
      </svg>
      <div class="card-title">{job.name}</div>
      <div class="card-sub">{sourceSummary(job)}</div>
    </div>

    <div class="connector">
      {#if running && progress}
        <div class="ring" style="background: conic-gradient(var(--green) {ringPercent}%, var(--border) 0)">
          <div class="ring-inner">{ringPercent}%</div>
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
        <span>
          {volTag}{#if preparing}Подготовка: поиск исключаемых файлов…
          {:else if excluding}Удаление исключённых файлов из образа…
          {:else if finalizing}Завершение: запись образа на диск…
          {:else if verifying}Проверка резервной копии…
          {:else}Резервное копирование…{/if}
        </span>
        {#if progress}
          <span>
            {#if preparing}
              {progress.blocks_done.toLocaleString('ru-RU')} записей просмотрено
              {#if progress.blocks_total > 0}из {progress.blocks_total.toLocaleString('ru-RU')}{/if}
            {:else if excluding}
              {#if progress.blocks_total > 0}
                {progress.blocks_done.toLocaleString('ru-RU')} из {progress.blocks_total.toLocaleString('ru-RU')}
              {:else}
                файлы удаляются и зануляются в готовой копии…
              {/if}
            {:else if finalizing}
              сброс кэша на диск, подождите…
            {:else}
              <!-- Пробел перед «·» пишется явно: в начале блока {#if} Svelte
                   его срезает, и получалось «блоков· 85.1 МБ/с». -->
              {progress.blocks_done} / {progress.blocks_total} блоков{#if progress.speed_mbps > 0}{'\u00a0· '}{progress.speed_mbps.toFixed(1)} МБ/с{/if}
            {/if}
          </span>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" class:verify={verifying} class:indet={indetBar} style="width:{indetBar ? 100 : (progress?.percent ?? 0)}%"></div></div>
      <div class="progress-meta">
        <span class="route" title="{sourceSummary(job)} → {destPath}">
          {sourceSummary(job)} → {destPath}
        </span>
        <span class="times">
          Прошло {formatClock(elapsedMs)} · Осталось {Number.isFinite(eta) ? '~' + formatClock(eta) : 'оценка…'}
        </span>
      </div>
    </div>
    <!-- Разбивка по типам — сразу, как только сканер её посчитал, а не в конце.
         Он видит каждый файл с размером ещё ДО копирования, так что ответ на
         вопрос «что вообще в этой копии» готов через минуту после старта, а
         не через двадцать. -->
    {#if catData}
      <div class="live-cats">
        <div class="live-cats-head">Что войдёт в копию — по типам файлов</div>
        <div class="catbar">
          {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}
            <div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>
          {/each}
        </div>
        <div class="cats">
          {#each catData as c}
            <div class="cat">
              <span class="dot" style="background:{c.color}"></span>
              <span class="cat-name">{c.name}</span>
              <span class="cat-size">{formatBytes(c.bytes)}</span>
            </div>
          {/each}
        </div>
      </div>
    {/if}
  {:else}
    <!-- stats row -->
    <div class="stats">
      <span>
        Последнее копирование:
        <b>{lastRun ? formatDate(lastRun.finished_at) : 'ещё не выполнялось'}</b>
      </span>
      <span class="reco">
        Данные для восстановления:
        <b>{lastRun ? formatBytes(lastRun.used_blocks > 0 ? lastRun.used_blocks * 1048576 : lastRun.bytes_copied) : '—'}</b>
        <button class="q" title="О разбивке по типам" onclick={() => (stubMsg = 'Разбивка по типам считается по файлам, вошедшим в копию (по расширениям), во время сканирования тома. Исключённые файлы не учитываются.')}>?</button>
      </span>
    </div>
    {#if catData}
      <div class="catbar">
        {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}
          <div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>
        {/each}
      </div>
      <div class="cats">
        {#each catData as c}
          <div class="cat">
            <span class="dot" style="background:{c.color}"></span>
            <span class="cat-name">{c.name}</span>
            <span class="cat-size">{formatBytes(c.bytes)}</span>
          </div>
        {/each}
        {#if lastRun && lastRun.excluded_files > 0}
          <div class="cat">
            <span class="dot" style="background:#c0463f"></span>
            <span class="cat-name">Исключено</span>
            <span class="cat-size">{formatBytes(lastRun.excluded_bytes)}</span>
          </div>
        {/if}
      </div>
    {:else}
      <div class="catbar empty-catbar"></div>
      <!-- The breakdown comes out of the file walk. When that walk did not run,
           the copy knows why — and saying so beats promising it "next time" to
           someone whose next copy will not have it either. -->
      <div class="cats-note">
        {#if lastRun?.file_scan_skipped}
          Разбивка по типам для этой копии не считалась: {lastRun.file_scan_skipped}.
        {:else}
          Разбивка по типам появится после следующего копирования (считается при сканировании тома).
        {/if}
      </div>
    {/if}

    {#if lastRun?.status === 'success'}
      {#if summaryHidden}
        <button class="summary-show" onclick={() => { summaryHidden = false; rememberSummaryState(false); }}>
          <span class="bdot small"></span> Показать сведения о копии
        </button>
      {:else}
        <div class="banner ok">
          <span class="bdot"></span>
          <span>
            Копия создана — {kindLabel(lastRun.backup_kind)}{lastRun.vss_used
              ? ', снимок VSS'
              : media.active
                ? ', холодная копия (система офлайн)'
                : ''}.
            {lastRun.smart_mode ? 'Только занятые секторы.' : 'Посекторно.'}
            {#if lastRun.backup_kind === 'incremental' || lastRun.backup_kind === 'differential'}
              Записано только изменённое: {formatBytes(lastRun.bytes_copied)}.
            {/if}
            {#if lastRun.consolidated_increments > 0}
              Объединено версий: {lastRun.consolidated_increments} — самые старые
              инкременты влиты в полную копию, она теперь соответствует более
              позднему дню.
            {/if}
            {#if lastRun.excluded_files > 0}
              Исключено файлов: {lastRun.excluded_files} ({formatBytes(lastRun.excluded_bytes)}).
            {:else if exclusionsIgnored}
              <br />
              <b>Исключения не применялись</b> ({job.exclusions.patterns.length}
              {job.exclusions.patterns.length === 1 ? 'правило' : 'правил'}): {lastRun.file_scan_skipped}.
              Всё, что под них попадает, в копии есть.
            {/if}
          </span>
          <button class="banner-x" title="Скрыть" aria-label="Скрыть" onclick={() => { summaryHidden = true; rememberSummaryState(true); }}>✕</button>
        </div>
      {/if}
    {:else if lastRun?.status === 'error'}
      <div class="banner err"><span class="bdot red"></span><span>Ошибка: {lastRun.error}</span></div>
    {/if}
  {/if}

  {#if actionErr}<div class="banner err"><span class="bdot red"></span><span>{actionErr}</span></div>{/if}
  {#if actionMsg}<div class="banner info"><span class="bdot blue"></span><span>{actionMsg}</span></div>{/if}
</div>

  <div class="footer">
    <div class="hint">
      {#if status === 'queued'}
        В очереди на выполнение…
      {:else}
        Расписание: {job.schedule.kind === 'none' ? 'не задано (запуск вручную)' : ({ daily: 'ежедневно', weekly: 'еженедельно', monthly: 'ежемесячно', event: 'по событию', nonstop: 'непрерывно' }[job.schedule.kind] ?? job.schedule.kind)}
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
  .page { height: 100%; display: flex; flex-direction: column; }
  .dash { flex: 1; overflow: auto; width: 100%; padding: 34px 44px; max-width: 900px; margin: 0 auto; display: flex; flex-direction: column; }
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
  .bar-fill.indet { background-image: linear-gradient(90deg, rgba(255,255,255,.35) 25%, transparent 25%, transparent 50%, rgba(255,255,255,.35) 50%, rgba(255,255,255,.35) 75%, transparent 75%); background-size: 28px 28px; animation: indet-move 0.8s linear infinite; }
  @keyframes indet-move { from { background-position: 0 0; } to { background-position: 28px 0; } }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  /* Занимает всё, что осталось от строки: имя файла копии обрезалось на 60 %
     ширины, хотя правее до «Прошло …» стояла пустота. Обрезка остаётся на
     совсем длинный путь, но включается последней, а не заранее. */
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1 1 auto; min-width: 0; }
  .progress-meta .times { white-space: nowrap; flex-shrink: 0; }
  .stats { display: flex; justify-content: space-between; font-size: 13px; color: var(--text-muted); margin-top: 34px; }
  .stats b { color: var(--head); font-weight: 600; }
  .reco { display: flex; align-items: center; gap: 6px; }
  .q { width: 16px; height: 16px; border-radius: 50%; border: 1px solid var(--text-muted); background: none; color: var(--text-muted); font-size: 10px; cursor: pointer; padding: 0; }
  /* Разбивка под полосой прогресса: тот же вид, что в итоге, но с заголовком —
     во время копирования она отвечает на другой вопрос («что будет в копии»),
     и без подписи это читалось бы как результат. */
  .live-cats { margin-top: 26px; }
  .live-cats-head { font-size: 13px; color: var(--text-muted); margin-bottom: 4px; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; margin-top: 10px; }
  /* Every present category stays visible: tiny slices keep 5px and the
     large ones shrink to make room, so the bar still totals 100%. */
  .catseg { min-width: 5px; }
  .cats { display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap; }
  .cat { flex: 1; min-width: 130px; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--head); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .empty-catbar { background: var(--tabbg); }
  .cats-note { font-size: 12px; color: var(--text-muted); margin-top: 12px; }
  .banner { margin-top: 22px; border-radius: 6px; padding: 13px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); font-size: 13px; color: var(--head); }
  .banner.err { background: rgba(216,74,74,.08); }
  .bdot { width: 18px; height: 18px; border-radius: 50%; background: var(--green); flex-shrink: 0; }
  .bdot.red { background: var(--error); }
  .bdot.small { width: 10px; height: 10px; }
  .banner .banner-x { margin-left: auto; flex-shrink: 0; width: 22px; height: 22px; border: none; background: none; color: var(--text-muted); font-size: 13px; line-height: 1; cursor: pointer; border-radius: 4px; }
  .banner .banner-x:hover { background: rgba(0,0,0,.06); color: var(--head); }
  .summary-show { margin-top: 22px; display: flex; align-items: center; gap: 8px; padding: 8px 12px; background: none; border: 1px dashed var(--border); border-radius: 6px; color: var(--text-muted); font-size: 12px; font-weight: 600; font-family: var(--font); cursor: pointer; align-self: flex-start; }
  .summary-show:hover { color: var(--head); border-color: var(--text-muted); }
  .bdot.blue { background: #5a8fd8; }
  /* .footer is global (styles/global.css) — full-width bar, same on every page. */
  .hint { flex: 1; font-size: 12px; color: var(--text-muted); }
  .btn { border-radius: 4px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn:disabled { opacity: .5; cursor: default; }
  .btn.outline { background: transparent; border: 1px solid var(--primary); color: var(--primary); }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover { background: var(--green-hover); }
  .btn.danger { background: transparent; border: 1px solid var(--error); color: var(--error); }
</style>
