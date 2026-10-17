<script lang="ts">
  import { media } from '../lib/mediaStore.svelte';
  import { api } from '../lib/api';
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  import {
    jobs, jobStatus, lastSuccessfulRun, sourceSummary, runJob, cancelRunning, runClock, etaMs,
  } from '../lib/jobStore.svelte';
  import InfoModal from '../components/InfoModal.svelte';
  import type { Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { intlLocale } from '../lib/i18n.svelte';

  interface Props {
    job: Job;
    onEdit: () => void;
  }
  let { job, onEdit }: Props = $props();

  let actionMsg: string | null = $state(null);
  let actionErr: string | null = $state(null);
  // A function, so the help text follows a change of language while it is open.
  let stubMsg: (() => string) | null = $state(null);

  const status = $derived(jobStatus(job));
  const running = $derived(status === 'running');
  const progress = $derived(jobs.progress[job.id] ?? null);
  const scheduledVerify = $derived(progress?.phase === 'scheduled-verify');
  const verifying = $derived(progress?.phase === 'verify' || scheduledVerify);
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
  /** How the last attempt ended — the ✓/✕ and the banner under the stats. */
  const lastRun = $derived(job.last_run);
  const lastVerification = $derived(job.last_verification);
  /** What the job HAS: the newest copy, its size and what is in it. A failed
   *  attempt after it changes nothing about that (see `lastSuccessfulRun`). */
  const lastGood = $derived(lastSuccessfulRun(job));
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
    return `${m.dash_vol_tag({ n, count: p.volume_count })}${p.volume ? ` · ${p.volume}` : ''} — `;
  });

  const KIND_LABELS: Record<string, () => string> = {
    incremental: m.kind_incremental,
    differential: m.kind_differential,
  };
  const kindLabel = (k: string) => (KIND_LABELS[k] ?? m.kind_full)();

  const SCHED_LABELS: Record<string, () => string> = {
    daily: m.dash_sched_daily,
    weekly: m.dash_sched_weekly,
    monthly: m.dash_sched_monthly,
    event: m.dash_sched_event,
    nonstop: m.dash_sched_nonstop,
  };

  /** What the check after the copy reads, said the way the setting is.
   *  «Проверять только последнюю копию» reads this file alone; otherwise the
   *  whole chain the copy stands on is read — which for a full copy, with
   *  nothing under it, is again this copy alone (`VerifyScope`). Which kind is
   *  being written is in the file name: `…_{full|inc|diff}_{stamp}.{ext}`. */
  const verifyLabel = $derived.by(() => {
    if (scheduledVerify) return m.act_verify_running();
    const tag = progress?.image_file_name?.match(/_(full|inc|diff)_\d{8}-\d{6}\.[^.]+$/i)?.[1];
    const onlyThis = job.advanced.verify_latest_only || tag?.toLowerCase() === 'full';
    return onlyThis ? m.dash_verify_this() : m.dash_verify_all();
  });

  // The round indicator spans BOTH phases when "проверять после создания" is on:
  // 0–50 % = backup, 50–100 % = verify. The bar below stays per-phase.
  // "prepare" (exclusion scan) sits at 0 %; "finalize" (flush to disk) means the
  // copy is done, so the ring holds at the end of the backup half.
  const ringPercent = $derived.by(() => {
    const p = progress;
    if (!p || p.phase === 'prepare') return 0;
    if (p.phase === 'scheduled-verify') return p.percent;
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
    { key: 'images', name: m.cat_images, color: '#5ab02f' },
    { key: 'video', name: m.cat_video, color: '#d8c81e' },
    { key: 'audio', name: m.cat_audio, color: '#e07a2f' },
    { key: 'documents', name: m.cat_documents, color: '#8b5cf6' },
    { key: 'windows', name: m.cat_windows, color: '#4f9be8' },
    { key: 'linux', name: m.cat_linux, color: '#14b8a6' },
    { key: 'other', name: m.cat_other, color: '#9aa0ad' },
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
    const c = liveCategories ?? lastGood?.categories;
    if (!c) return null;
    const total = CAT_META.reduce((s, cm) => s + (c[cm.key] ?? 0), 0);
    if (total <= 0) return null;
    return CAT_META.map((cm) => ({
      ...cm,
      bytes: c[cm.key] ?? 0,
      pct: ((c[cm.key] ?? 0) / total) * 100,
    }));
  });

  /** What the exclusions took out of the copy on screen: this run's own figure
   *  while it runs (null until the scan has said), the last copy's after. The
   *  live view used to take the LAST copy's figure — after the rules changed,
   *  the bar showed gigabytes this run was not excluding at all (2026-09-26). */
  /** «Исключено» — the eighth category colour, the same in both themes as
   *  the other seven (design: CATS). */
  const EXCLUDED_COLOR = '#c0463f';

  const excludedBytes = $derived.by((): number | null => {
    if (liveCategories) return progress?.excluded_bytes ?? null;
    return lastGood?.excluded_bytes ?? 0;
  });

  // The bar covers the whole scanned volume: what the copy holds, plus what the
  // exclusions left out (last segment). Shares are recomputed against that grand
  // total so the bar still adds up to 100%.
  const barData = $derived.by(() => {
    if (!catData) return null;
    const kept = catData.reduce((s, c) => s + c.bytes, 0);
    const ex = excludedBytes ?? 0;
    const total = kept + ex;
    if (total <= 0) return null;
    const out: { name: () => string; color: string; bytes: number; pct: number }[] = catData.map((c) => ({
      name: c.name,
      color: c.color,
      bytes: c.bytes,
      pct: (c.bytes / total) * 100,
    }));
    if (ex > 0) out.push({ name: m.cat_excluded, color: EXCLUDED_COLOR, bytes: ex, pct: (ex / total) * 100 });
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
    if (scheduledVerify) {
      try { await api.cancelEngineOp(); } catch (e) { actionErr = String(e); }
    } else {
      await cancelRunning();
    }
  }
</script>

<!-- The eighth cell, always there so the grid keeps its two even rows. «…»
     while the scan has not said yet; 0 B when the rules took nothing. -->
{#snippet excludedCell()}
  <div class="cat">
    <span class="dot" style:background={EXCLUDED_COLOR}></span>
    <span class="cat-name">{m.cat_excluded()}</span>
    <span class="cat-size">{excludedBytes == null ? '…' : formatBytes(excludedBytes)}</span>
  </div>
{/snippet}

{#snippet errIcon()}
  <svg class="bicon" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--error)" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5" /><path d="M9 9l6 6M15 9l-6 6" /></svg>
{/snippet}

<div class="page">
<div class="dash">
  <div class="flow">
    <div class="card">
      <svg width="110" height="110" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.1">
        <rect x="3" y="5" width="18" height="14" rx="3" />
        <circle cx="12" cy="12" r="3.5" />
        <circle cx="12" cy="12" r=".8" fill="var(--disk-icon)" />
      </svg>
      <div class="card-title">{job.name}</div>
      <div class="card-sub">{sourceSummary(job)}</div>
    </div>

    <div class="connector">
      {#if running && progress}
        <div class="ring" style="background: conic-gradient(var(--success) {ringPercent}%, var(--track) 0)">
          <div class="ring-inner">{ringPercent}%</div>
        </div>
      {:else if running}
        <div class="ring" style="background: conic-gradient(var(--success) 0%, var(--track) 0)">
          <div class="ring-inner">…</div>
        </div>
      {:else}
        <div class="dash-line"></div>
        <div
          class="check"
          class:ok={lastRun?.status === 'success'}
          class:err={lastRun?.status === 'error'}
          class:cancel={lastRun?.status === 'cancelled'}
        >
          {#if lastRun?.status === 'error'}
            <svg width="22" height="22" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.4" fill="none" stroke-linecap="round"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11" /></svg>
          {:else if lastRun?.status === 'cancelled'}
            <svg width="22" height="22" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round"><path d="M6 12h12" /></svg>
          {:else}
            <svg width="24" height="24" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
          {/if}
        </div>
        <div class="dash-line"></div>
      {/if}
    </div>

    <div class="card">
      <svg width="110" height="110" viewBox="0 0 24 24">
        <path d="M3 7a2 2 0 0 1 2-2h4.2l2 2H19a2 2 0 0 1 2 2v8.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" fill="var(--folder)" />
        <path d="M3 10h18v7.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" fill="var(--folder2)" />
      </svg>
      <div class="card-title">{job.name}</div>
      <div class="card-sub">{job.dest_folder}</div>
    </div>
  </div>

  {#if running}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>
          {volTag}{#if preparing}{m.dash_phase_prepare()}
          {:else if excluding}{m.dash_phase_exclusions()}
          {:else if finalizing}{m.dash_phase_finalize()}
          {:else if verifying}{verifyLabel}
          {:else}{m.dash_phase_backup()}{/if}
        </span>
        {#if progress}
          <span>
            {#if preparing}
              {#if progress.blocks_total > 0}
                {m.dash_prep_scanned_of({ done: progress.blocks_done.toLocaleString(intlLocale()), total: progress.blocks_total.toLocaleString(intlLocale()) })}
              {:else}
                {m.dash_prep_scanned({ done: progress.blocks_done.toLocaleString(intlLocale()) })}
              {/if}
            {:else if excluding}
              {#if progress.blocks_total > 0}
                {m.dash_n_of_total({ done: progress.blocks_done.toLocaleString(intlLocale()), total: progress.blocks_total.toLocaleString(intlLocale()) })}
              {:else}
                {m.dash_excluding_note()}
              {/if}
            {:else if finalizing}
              {m.dash_finalizing_note()}
            {:else}
              <!-- Пробел перед «·» пишется явно: в начале блока {#if} Svelte
                   его срезает, и получалось «блоков· 85.1 МБ/с». -->
              {m.dash_blocks({ done: progress.blocks_done, total: progress.blocks_total })}{#if progress.speed_mbps > 0}{'\u00a0· '}{m.dash_speed({ speed: progress.speed_mbps.toFixed(1) })}{/if}
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
          {m.dash_elapsed({ time: formatClock(elapsedMs) })} · {m.dash_remaining({ time: Number.isFinite(eta) ? '~' + formatClock(eta) : m.dash_estimating() })}
        </span>
      </div>
      <!-- The service says when the copy has stood still for minutes: a backup
           disk that keeps failing writes holds a run like that for as long as
           it fails, and a bar that merely stops reads as "still working". -->
      {#if progress?.stalled_secs}
        <div class="stall-note" role="status">
          {m.dash_stalled({ min: Math.max(2, Math.floor(progress.stalled_secs / 60)) })}
        </div>
      {/if}
    </div>
    <!-- Разбивка по типам — сразу, как только сканер её посчитал, а не в конце.
         Он видит каждый файл с размером ещё ДО копирования, так что ответ на
         вопрос «что вообще в этой копии» готов через минуту после старта, а
         не через двадцать. -->
    {#if catData}
      <div class="live-cats">
        <div class="live-cats-head">{m.dash_live_cats_head()}</div>
        <div class="catbar">
          {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}
            <div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name()}: {formatBytes(c.bytes)}"></div>
          {/each}
        </div>
        <div class="cats">
          {#each catData as c}
            <div class="cat">
              <span class="dot" style="background:{c.color}"></span>
              <span class="cat-name">{c.name()}</span>
              <span class="cat-size">{formatBytes(c.bytes)}</span>
            </div>
          {/each}
          {@render excludedCell()}
        </div>
      </div>
    {/if}
  {:else}
    <!-- stats row -->
    <div class="stats">
      <span>
        {m.dash_last_backup()}
        <b>{lastGood
          ? formatDate(lastGood.finished_at)
          : lastRun
            ? m.dash_no_backups_yet()
            : m.dash_never_run()}</b>
      </span>
      <span class="reco">
        {m.dash_restore_data()}
        <b>{lastGood ? formatBytes(lastGood.used_blocks > 0 ? lastGood.used_blocks * 1048576 : lastGood.bytes_copied) : '—'}</b>
        <button class="q" title={m.dash_cats_about()} onclick={() => (stubMsg = m.dash_cats_about_text)}>?</button>
      </span>
    </div>
    {#if catData}
      <div class="catbar">
        {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}
          <div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name()}: {formatBytes(c.bytes)}"></div>
        {/each}
      </div>
      <div class="cats">
        {#each catData as c}
          <div class="cat">
            <span class="dot" style="background:{c.color}"></span>
            <span class="cat-name">{c.name()}</span>
            <span class="cat-size">{formatBytes(c.bytes)}</span>
          </div>
        {/each}
        {@render excludedCell()}
      </div>
    {:else}
      <div class="catbar empty-catbar"></div>
      <!-- The breakdown comes out of the file walk. When that walk did not run,
           the copy knows why — and saying so beats promising it "next time" to
           someone whose next copy will not have it either. -->
      <div class="cats-note">
        {#if lastGood?.file_scan_skipped}
          {m.dash_cats_skipped({ reason: lastGood.file_scan_skipped })}
        {:else}
          {m.dash_cats_next()}
        {/if}
      </div>
    {/if}

    {#if lastVerification?.status === 'error'}
      <div class="banner err">{@render errIcon()}<span>{m.act_verify_failed()} — {formatDate(lastVerification.finished_at)}. {lastVerification.error}</span></div>
    {/if}
    {#if lastRun?.status === 'success'}
      {#if summaryHidden}
        <button class="summary-show" onclick={() => { summaryHidden = false; rememberSummaryState(false); }}>
          <span class="bdot small"></span>{m.dash_show_summary()}
        </button>
      {:else}
        <div class="banner ok">
          <span class="bdot">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
          </span>
          <span>
            {lastRun.vss_used
              ? m.dash_created_vss({ kind: kindLabel(lastRun.backup_kind) })
              : media.active
                ? m.dash_created_cold({ kind: kindLabel(lastRun.backup_kind) })
                : m.dash_created({ kind: kindLabel(lastRun.backup_kind) })}
            {lastRun.smart_mode ? m.dash_used_only() : m.dash_sector_by_sector()}
            {#if lastRun.backup_kind === 'incremental' || lastRun.backup_kind === 'differential'}
              {m.dash_changed_only({ size: formatBytes(lastRun.bytes_copied) })}
            {/if}
            {#if lastRun.consolidated_increments > 0}
              {m.dash_consolidated({ count: lastRun.consolidated_increments })}
            {/if}
            {#if lastRun.excluded_files > 0}
              {m.dash_excluded_files({ count: lastRun.excluded_files, size: formatBytes(lastRun.excluded_bytes) })}
            {:else if exclusionsIgnored}
              <br />
              <b>{m.dash_excl_ignored()}</b>
              {m.dash_excl_ignored_rest({
                rules: m.dash_rules_count({ count: job.exclusions.patterns.length }),
                reason: lastRun.file_scan_skipped ?? '',
              })}
            {/if}
            {#if lastRun.excluded_entries_kept}
              <br />
              <b>{m.dash_entries_kept()}</b>
              {lastRun.excluded_entries_kept}.
            {/if}
            {#if lastRun.layout_changed}
              <br />
              {lastRun.layout_changed}.
            {/if}
          </span>
          <button class="banner-x" title={m.common_hide()} aria-label={m.common_hide()} onclick={() => { summaryHidden = true; rememberSummaryState(true); }}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
          </button>
        </div>
      {/if}
    {:else if lastRun?.status === 'error'}
      <!-- When: the stats above now date the last COPY, and without its own
           time this banner read as if that copy had failed. -->
      <div class="banner err">{@render errIcon()}<span>{m.dash_error({ date: formatDate(lastRun.finished_at), error: lastRun.error ?? '' })}</span></div>
    {/if}
  {/if}

  <!-- The run happened but the service could not write it into the history:
       until a later write succeeds it lives only in the service's memory. -->
  {#if lastRun?.record_error}<div class="banner err">{@render errIcon()}<span>{lastRun.record_error}</span></div>{/if}
  {#if actionErr}<div class="banner err">{@render errIcon()}<span>{actionErr}</span></div>{/if}
  {#if actionMsg}<div class="banner info"><span>{actionMsg}</span></div>{/if}
</div>

  <div class="footer">
    <div class="hint">
      {#if status === 'queued'}
        {m.dash_queued()}
      {:else}
        {m.dash_schedule({ schedule: job.schedule.kind === 'none' ? m.dash_sched_none() : (SCHED_LABELS[job.schedule.kind]?.() ?? job.schedule.kind) })}
      {/if}
    </div>
    <button class="btn outline" onclick={onEdit} disabled={running}>{m.dash_options()}</button>
    {#if running}
      <button class="btn danger" onclick={cancel}>{m.common_cancel()}</button>
    {:else}
      <button class="btn green" onclick={backup}>{m.dash_backup_now()}</button>
    {/if}
  </div>
</div>

{#if stubMsg}
  <InfoModal title={m.dash_cats_about()} message={stubMsg()} onClose={() => (stubMsg = null)} />
{/if}

<style>
  .page { height: 100%; display: flex; flex-direction: column; }
  .dash { flex: 1; overflow: auto; width: 100%; padding: 34px 44px; display: flex; flex-direction: column; }
  .flow { display: flex; align-items: stretch; justify-content: center; }
  .card { flex: 1; max-width: 300px; border: 1px solid var(--border); border-radius: 8px; padding: 22px 18px; text-align: center; background: var(--card-bg); display: flex; flex-direction: column; align-items: center; gap: 8px; }
  .card-title { font-size: 20px; font-weight: 600; color: var(--head); }
  .card-sub { font-size: 12px; color: var(--text-muted); word-break: break-all; }
  .connector { width: 130px; flex-shrink: 0; display: flex; align-items: center; justify-content: center; gap: 8px; }
  .dash-line { width: 26px; height: 2px; background: repeating-linear-gradient(90deg, var(--border-strong) 0 4px, transparent 4px 8px); }
  .check { width: 52px; height: 52px; border-radius: 50%; background: var(--text-muted); display: flex; align-items: center; justify-content: center; }
  .check.ok { background: var(--success); }
  .check.err { background: var(--error); }
  .check.cancel { background: var(--warning); }
  .ring { width: 60px; height: 60px; border-radius: 50%; display: flex; align-items: center; justify-content: center; }
  .ring-inner { width: 44px; height: 44px; border-radius: 50%; background: var(--content-bg); display: flex; align-items: center; justify-content: center; font-size: 13px; font-weight: 700; color: var(--head); }
  .progress-wrap { margin-top: 30px; }
  .progress-info { display: flex; justify-content: space-between; font-size: 13px; color: var(--head); margin-bottom: 8px; }
  .progress-info > span:first-child { font-weight: 600; }
  .progress-info > span:last-child { color: var(--text-secondary); }
  .bar { height: 8px; border-radius: 4px; background: var(--track); overflow: hidden; }
  .bar-fill { height: 100%; border-radius: 4px; background: var(--success); transition: width .15s linear; }
  .bar-fill.verify { background: var(--info); }
  .bar-fill.indet { background-image: linear-gradient(90deg, rgba(255,255,255,.35) 25%, transparent 25%, transparent 50%, rgba(255,255,255,.35) 50%, rgba(255,255,255,.35) 75%, transparent 75%); background-size: 28px 28px; animation: indet-move 0.8s linear infinite; }
  @keyframes indet-move { from { background-position: 0 0; } to { background-position: 28px 0; } }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  /* Занимает всё, что осталось от строки: имя файла копии обрезалось на 60 %
     ширины, хотя правее до «Прошло …» стояла пустота. Обрезка остаётся на
     совсем длинный путь, но включается последней, а не заранее. */
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1 1 auto; min-width: 0; }
  .progress-meta .times { white-space: nowrap; flex-shrink: 0; }
  .stall-note { margin-top: 8px; padding: 6px 10px; border-radius: 6px; font-size: 12px;
    background: var(--warning-soft); border: 1px solid var(--warning-line); color: var(--warning-text); }
  .stats { display: flex; justify-content: space-between; font-size: 13px; color: var(--text-secondary); margin-top: 34px; }
  .stats b { color: var(--head); font-weight: 600; }
  .reco { display: flex; align-items: center; gap: 6px; }
  .q { width: 16px; height: 16px; border-radius: 50%; border: 1px solid var(--border-strong); background: none; color: var(--text-muted); font-size: 10px; cursor: pointer; padding: 0; }
  .q:hover { color: var(--head); border-color: var(--text-muted); }
  /* Разбивка под полосой прогресса: тот же вид, что в итоге, но с заголовком —
     во время копирования она отвечает на другой вопрос («что будет в копии»),
     и без подписи это читалось бы как результат. */
  .live-cats { margin-top: 26px; }
  .live-cats-head { font-size: 13px; font-weight: 600; color: var(--head); margin-bottom: 4px; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; gap: 2px; margin-top: 10px; background: var(--track); }
  /* Every present category stays visible: tiny slices keep 5px and the
     large ones shrink to make room, so the bar still totals 100%. */
  .catseg { min-width: 5px; }
  /* Four to a row, always: eight cells (seven types + «Исключено») make two
     rows that line up column by column. A wrapping flex row put them wherever
     they fit, and one cell fewer shifted the whole second row. */
  .cats { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 10px 8px; margin-top: 14px; }
  .cat { min-width: 0; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--text); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .empty-catbar { background: var(--track); }
  .cats-note { font-size: 12px; color: var(--text-muted); margin-top: 12px; }
  .banner { margin-top: 22px; border-radius: 8px; padding: 13px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); font-size: 13.5px; line-height: 1.5; color: var(--text); }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
  .banner.info { background: var(--info-soft); border: 1px solid var(--info-line); color: var(--info-text); }
  .bdot { width: 20px; height: 20px; border-radius: 50%; background: var(--success); flex-shrink: 0; display: flex; align-items: center; justify-content: center; }
  .bdot.small { width: 8px; height: 8px; }
  .bicon { flex-shrink: 0; }
  .banner .banner-x { margin-left: auto; flex-shrink: 0; width: 22px; height: 22px; border: none; background: none; color: var(--text-muted); cursor: pointer; border-radius: 4px; display: flex; align-items: center; justify-content: center; }
  .banner .banner-x:hover { background: var(--track); color: var(--head); }
  .summary-show { margin-top: 22px; display: flex; align-items: center; gap: 8px; padding: 8px 14px; background: none; border: 1px dashed var(--border-strong); border-radius: 8px; color: var(--text-secondary); font-size: 13px; font-family: var(--font); cursor: pointer; align-self: flex-start; }
  .summary-show:hover { color: var(--head); border-color: var(--text-muted); }
  /* .footer is global (styles/global.css) — full-width bar, same on every page. */
  .hint { flex: 1; font-size: 13px; color: var(--text-secondary); }
  .btn { border-radius: 6px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid transparent; transition: background-color .15s ease; }
  .btn:disabled { opacity: .45; cursor: default; }
  .btn.outline { background: transparent; border-color: var(--primary); color: var(--accent-text); }
  .btn.outline:hover:not(:disabled) { background: var(--accent-soft); }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover:not(:disabled) { background: var(--green-hover); }
  .btn.danger { background: transparent; border-color: var(--error); color: var(--error-text); }
  .btn.danger:hover:not(:disabled) { background: var(--error-soft); }
</style>
