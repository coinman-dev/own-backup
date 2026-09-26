<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { media } from '../lib/mediaStore.svelte';
  import { jobs, reloadActivity } from '../lib/jobStore.svelte';
  import { formatBytes, formatDate, formatDuration } from '../lib/format';
  import type { JobRun, RestoreRunState } from '../lib/types';

  // Restores live in the service's own history, not in the job runs: this
  // list used to show copies only, and a finished restore vanished for good
  // the moment its report was closed on the restore page (2026-09-26).
  let restores = $state<RestoreRunState[]>([]);
  async function loadRestores() {
    try {
      restores = (await api.getRestoreHistory()).filter((r) => r.status !== 'running');
    } catch {
      // An older service without the call, or none at all: copies still show.
    }
  }
  onMount(loadRestores);
  function refresh() {
    reloadActivity();
    loadRestores();
  }

  type Item =
    | { kind: 'backup'; key: string; at: string; run: JobRun }
    | { kind: 'restore'; key: string; at: string; run: RestoreRunState };
  const items = $derived.by((): Item[] => {
    const all: Item[] = [
      ...jobs.activity.map((r) => ({
        kind: 'backup' as const,
        key: `b:${r.id}`,
        at: r.finished_at || r.started_at,
        run: r,
      })),
      ...restores.map((r) => ({
        kind: 'restore' as const,
        key: `r:${r.plan_id}`,
        at: r.finished_at || r.started_at,
        run: r,
      })),
    ];
    return all.sort((a, b) => new Date(b.at).getTime() - new Date(a.at).getTime());
  });

  /** Seconds between two RFC 3339 stamps, or -1. */
  function secondsBetween(from: string, to: string | null): number {
    if (!to) return -1;
    const s = (new Date(to).getTime() - new Date(from).getTime()) / 1000;
    return isFinite(s) ? s : -1;
  }
  /** What the restore was of: the copy's file, or at least the disk. */
  function restoreWhat(r: RestoreRunState): string {
    return r.image_file || `диск ${r.target_disk}`;
  }
  /** The operation that broke, as the report put it. */
  function restoreFailure(r: RestoreRunState): string {
    const op = r.ops.find((o) => o.status === 'failed');
    return op ? `${op.title}: ${op.message}` : '';
  }
  const visibleOps = (r: RestoreRunState) => r.ops.filter((o) => !o.hidden || o.status === 'failed');

  const speed = (r: { bytes_copied: number; duration_secs: number }) =>
    r.duration_secs > 0 ? (r.bytes_copied / r.duration_secs / 1_000_000).toFixed(0) : '0';

  const kindLabel = (k: string) =>
    k === 'incremental' ? 'инкрементная' : k === 'differential' ? 'дифференциальная' : 'полная';

  /** Container format of the produced image, derived from its file name.
   *  Anything unrecognised used to read «VHDX» — an encrypted job's `.obk`
   *  copies included (2026-09-26). */
  function formatLabel(p: string): string {
    const name = (p || '').toLowerCase();
    if (name.endsWith('.obk')) return 'OBK (зашифрованный)';
    if (name.endsWith('.qcow2')) return 'qcow2 (сжатый)';
    if (name.endsWith('.vhdx') || name.endsWith('.avhdx')) return 'VHDX';
    return '—';
  }
</script>

<div class="page">
  <div class="head">
    <h1>Активность</h1>
    <button class="refresh" onclick={refresh}>Обновить</button>
  </div>

  {#if items.length === 0}
    <div class="empty">Здесь появится история резервных копий и восстановлений.</div>
  {/if}

  <div class="list">
    {#each items as item (item.key)}
      {#if item.kind === 'restore'}
        {@const r = item.run}
        <div class="card">
          <div class="ic">
            {#if r.status === 'completed'}
              <div class="badge ok"><svg width="13" height="13" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></div>
            {:else if r.status === 'cancelled'}
              <div class="badge warn"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><line x1="6" y1="12" x2="18" y2="12"/></svg></div>
            {:else}
              <div class="badge err"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg></div>
            {/if}
          </div>
          <div class="body">
            <div class="title">
              {r.status === 'completed' ? 'Восстановление выполнено' : r.status === 'cancelled' ? 'Восстановление отменено' : 'Сбой восстановления'}
              — {restoreWhat(r)}
              <span class="time">{formatDate(item.at)}</span>
            </div>
            {#if r.status === 'failed' && restoreFailure(r)}
              <div class="note">{restoreFailure(r)}</div>
            {/if}
            <div class="stats">
              <div><span class="k">Диск</span><span class="v">{r.target_disk}</span></div>
              <div>
                <span class="k">Операций</span>
                <span class="v">{visibleOps(r).filter((o) => o.status === 'done').length} из {visibleOps(r).length}</span>
              </div>
              <div><span class="k">Время</span><span class="v">{formatDuration(secondsBetween(r.started_at, r.finished_at))}</span></div>
            </div>
            {#if r.follow_ups.length > 0}
              <details class="follow">
                <summary>Что осталось сделать ({r.follow_ups.length})</summary>
                <ul>
                  {#each r.follow_ups as f, i (i)}<li>{f}</li>{/each}
                </ul>
              </details>
            {/if}
          </div>
        </div>
      {:else}
        {@const r = item.run}
        <div class="card">
          <div class="ic">
            {#if r.status === 'success'}
              <div class="badge ok"><svg width="13" height="13" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></div>
            {:else if r.status === 'cancelled'}
              <div class="badge warn"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><line x1="6" y1="12" x2="18" y2="12"/></svg></div>
            {:else}
              <div class="badge err"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg></div>
            {/if}
          </div>
          <div class="body">
            <div class="title">
              {r.status === 'success' ? 'Резервное копирование выполнено' : r.status === 'cancelled' ? 'Копирование отменено' : 'Сбой резервного копирования'}
              — {r.job_name}
              <span class="time">{formatDate(r.finished_at || r.started_at)}</span>
            </div>
            {#if r.status === 'error' && r.error}
              <div class="note">{r.error}</div>
            {/if}
            {#if r.status === 'success'}
              <div class="stats">
                <div><span class="k">Тип</span><span class="v">{kindLabel(r.backup_kind)}</span></div>
                <div><span class="k">Формат</span><span class="v">{formatLabel(r.image_path)}</span></div>
                <div><span class="k">Размер</span><span class="v">{formatBytes(r.bytes_copied)}</span></div>
                <div><span class="k">Скорость</span><span class="v">{speed(r)} МБ/с</span></div>
                <div><span class="k">Время</span><span class="v">{formatDuration(r.duration_secs)}</span></div>
                <div><span class="k">Режим</span><span class="v">{r.smart_mode ? 'занятые' : 'посекторно'}</span></div>
                <div>
                  <span class="k">Снимок</span>
                  <!-- "нет" would read as a shortcoming; on the medium the volume
                       is offline, so no snapshot was needed in the first place. -->
                  <span class="v">{r.vss_used ? 'VSS' : media.active ? 'не нужен (офлайн)' : 'нет'}</span>
                </div>
              </div>
            {/if}
          </div>
        </div>
      {/if}
    {/each}
  </div>
</div>

<style>
  .page { padding: 30px 44px; height: 100%; overflow: auto; }
  .head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 22px; }
  h1 { font-size: 28px; font-weight: 300; color: var(--head); }
  .refresh { background: none; border: 1px solid var(--border); border-radius: 4px; padding: 7px 16px; font-size: 13px; color: var(--head); cursor: pointer; font-family: var(--font); }
  .empty { color: var(--text-muted); font-size: 14px; padding: 20px 0; }
  .list { display: flex; flex-direction: column; gap: 12px; }
  .card { border: 1px solid var(--border); border-radius: 6px; padding: 16px 20px; display: flex; gap: 16px; background: var(--card-bg); box-shadow: var(--shadow); }
  .badge { width: 24px; height: 24px; border-radius: 50%; display: flex; align-items: center; justify-content: center; }
  .badge.ok { background: var(--green); }
  .badge.err { background: var(--error); }
  .badge.warn { background: var(--warning); }
  .body { flex: 1; min-width: 0; }
  .title { font-size: 14px; color: var(--head); }
  .time { color: var(--text-muted); margin-left: 6px; }
  .note { font-size: 13px; color: var(--text-muted); margin-top: 5px; word-break: break-all; }
  .stats { display: flex; gap: 40px; margin-top: 12px; flex-wrap: wrap; }
  .stats > div { display: flex; flex-direction: column; }
  .k { font-size: 12px; color: var(--primary); }
  .follow { margin-top: 10px; font-size: 13px; color: var(--head); }
  .follow summary { cursor: pointer; color: var(--text-secondary); }
  .follow ul { margin: 6px 0 0 18px; display: flex; flex-direction: column; gap: 4px; color: var(--text-secondary); }
  .v { font-size: 14px; color: var(--head); }
</style>
