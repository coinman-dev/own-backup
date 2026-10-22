<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { media } from '../lib/mediaStore.svelte';
  import { jobs, reloadActivity } from '../lib/jobStore.svelte';
  import { formatBytes, formatDate, formatDuration } from '../lib/format';
  import type { JobRun, RestoreRunState } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

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
  const checking = $derived(Object.values(jobs.progress).filter((p) => p.phase === 'scheduled-verify'));
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
    return r.image_file || m.act_target_disk({ disk: String(r.target_disk) });
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
    k === 'incremental' ? m.kind_incremental() : k === 'differential' ? m.kind_differential() : m.kind_full();

  /** Container format of the produced image, derived from its file name.
   *  Anything unrecognised used to read «VHDX» — an encrypted job's `.obk`
   *  copies included (2026-09-26). */
  function formatLabel(p: string): string {
    const name = (p || '').toLowerCase();
    if (name.endsWith('.obk')) return m.act_fmt_obk();
    if (name.endsWith('.qcow2')) return m.act_fmt_qcow2();
    if (name.endsWith('.vhdx') || name.endsWith('.avhdx')) return 'VHDX';
    return '—';
  }
</script>

<div class="page">
  <div class="head">
    <h1>{m.act_title()}</h1>
    <button class="refresh" onclick={refresh}>{m.common_refresh()}</button>
  </div>

  {#each checking as progress (progress.job_id)}
    <div class="card">
      <div class="body">
        <div class="title">{m.act_verify_running()} — {progress.volume}</div>
        <div class="note">{progress.image_file_name} · {progress.percent}%</div>
        <progress max="100" value={progress.percent}></progress>
        <button class="refresh" onclick={() => api.cancelEngineOp()}>{m.common_cancel()}</button>
      </div>
    </div>
  {/each}
  {#if items.length === 0 && checking.length === 0}
    <div class="empty">{m.act_empty()}</div>
  {/if}

  <div class="list">
    {#each items as item (item.key)}
      {#if item.kind === 'restore'}
        {@const r = item.run}
        <div class="card">
          <div class="ic">
            {#if r.status === 'completed'}
              <div class="badge ok"><svg width="14" height="14" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg></div>
            {:else if r.status === 'cancelled'}
              <div class="badge warn"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.2" fill="none" stroke-linecap="round"><path d="M6 12h12"/></svg></div>
            {:else}
              <div class="badge err"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg></div>
            {/if}
          </div>
          <div class="body">
            <div class="title">
              {r.status === 'completed' ? m.act_restore_done() : r.status === 'cancelled' ? m.act_restore_cancelled() : m.act_restore_failed()}
              — {restoreWhat(r)}
              <span class="time">{formatDate(item.at)}</span>
            </div>
            {#if r.status === 'failed' && restoreFailure(r)}
              <div class="note">{restoreFailure(r)}</div>
            {/if}
            <div class="stats">
              <div><span class="k">{m.act_disk()}</span><span class="v">{r.target_disk}</span></div>
              <div>
                <span class="k">{m.act_ops()}</span>
                <span class="v">{m.act_ops_done({ done: visibleOps(r).filter((o) => o.status === 'done').length, total: visibleOps(r).length })}</span>
              </div>
              <div><span class="k">{m.act_time()}</span><span class="v">{formatDuration(secondsBetween(r.started_at, r.finished_at))}</span></div>
            </div>
            {#if r.follow_ups.length > 0}
              <details class="follow">
                <summary>{m.act_follow_ups({ count: r.follow_ups.length })}</summary>
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
              <div class="badge ok"><svg width="14" height="14" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg></div>
            {:else if r.status === 'cancelled'}
              <div class="badge warn"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.2" fill="none" stroke-linecap="round"><path d="M6 12h12"/></svg></div>
            {:else}
              <div class="badge err"><svg width="12" height="12" viewBox="0 0 24 24" stroke="#fff" stroke-width="3" fill="none" stroke-linecap="round"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg></div>
            {/if}
          </div>
          <div class="body">
            <div class="title">
              {r.operation === 'verify'
                ? (r.status === 'success' ? m.act_verify_done() : r.status === 'cancelled' ? m.act_verify_cancelled() : m.act_verify_failed())
                : (r.status === 'success' ? m.act_backup_done() : r.interrupted ? m.act_backup_interrupted() : r.status === 'cancelled' ? m.act_backup_cancelled() : m.act_backup_failed())}
              — {r.job_name}
              <span class="time">{formatDate(r.finished_at || r.started_at)}</span>
            </div>
            {#if (r.status === 'error' || r.interrupted) && r.error}
              <div class="note">{r.error}</div>
            {/if}
            {#if r.status === 'error' && r.error_kind === 'key_protection'}
              <div class="note">{m.run_error_key_protection()}</div>
            {:else if r.status === 'error' && r.error_kind === 'archive_reauthorize'}
              <div class="note">{m.run_error_reauthorize()}</div>
            {/if}
            {#if r.excluded_entries_kept}
              <div class="note">{r.excluded_entries_kept}</div>
            {/if}
            {#if r.layout_changed}
              <div class="note">{r.layout_changed}</div>
            {/if}
            {#each r.notes ?? [] as note}
              <div class="note">{note}</div>
            {/each}
            {#if r.operation === 'verify'}
              <div class="stats">
                <div><span class="k">{m.act_time()}</span><span class="v">{formatDuration(r.duration_secs)}</span></div>
                <div><span class="k">{m.act_verify_blocks()}</span><span class="v">{r.used_blocks}</span></div>
              </div>
              {#if r.verification}
                <details class="follow">
                  <summary>{m.act_verify_images({ count: r.verification.images.length })}</summary>
                  <ul>{#each r.verification.images as image (image.image_path)}
                    <li>{image.file_name}: {image.valid ? m.act_verify_valid() : image.error || m.act_verify_corrupt({ count: image.mismatched_blocks })}</li>
                  {/each}</ul>
                </details>
              {/if}
            {:else if r.status === 'success'}
              <div class="stats">
                <div><span class="k">{m.act_type()}</span><span class="v">{kindLabel(r.backup_kind)}</span></div>
                <div><span class="k">{m.act_format()}</span><span class="v">{formatLabel(r.image_path)}</span></div>
                <div><span class="k">{m.act_size()}</span><span class="v">{formatBytes(r.bytes_copied)}</span></div>
                <div><span class="k">{m.act_speed()}</span><span class="v">{m.act_speed_value({ speed: speed(r) })}</span></div>
                <div><span class="k">{m.act_time()}</span><span class="v">{formatDuration(r.duration_secs)}</span></div>
                <div><span class="k">{m.act_mode()}</span><span class="v">{r.smart_mode ? m.act_mode_used() : m.act_mode_sector()}</span></div>
                <div>
                  <span class="k">{m.act_snapshot()}</span>
                  <!-- "нет" would read as a shortcoming; on the medium the volume
                       is offline, so no snapshot was needed in the first place. -->
                  <span class="v">{r.vss_used ? 'VSS' : media.active ? m.act_snapshot_not_needed() : m.act_snapshot_none()}</span>
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
  h1 { font-size: 24px; font-weight: 300; color: var(--head); }
  .refresh { background: var(--card-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 7px 16px; font-size: 13px; font-weight: 600; color: var(--text); cursor: pointer; font-family: var(--font); transition: background-color .15s ease; }
  .refresh:hover { background: var(--panel2); }
  .empty { color: var(--text-muted); font-size: 14px; padding: 20px 0; }
  .list { display: flex; flex-direction: column; gap: 12px; }
  .card { border: 1px solid var(--border); border-radius: 8px; padding: 16px 20px; display: flex; gap: 16px; background: var(--card-bg); }
  .badge { width: 24px; height: 24px; border-radius: 50%; display: flex; align-items: center; justify-content: center; }
  .badge.ok { background: var(--success); }
  .badge.err { background: var(--error); }
  .badge.warn { background: var(--warning); }
  .body { flex: 1; min-width: 0; }
  .title { font-size: 14px; color: var(--head); }
  .time { color: var(--text-muted); margin-left: 6px; }
  /* Only ever a failure: the step that broke, or the copy's error. */
  .note { font-size: 13px; color: var(--error-text); margin-top: 5px; word-break: break-all; }
  .stats { display: flex; gap: 40px; margin-top: 12px; flex-wrap: wrap; }
  .stats > div { display: flex; flex-direction: column; }
  .k { font-size: 12px; color: var(--accent-text); }
  .follow { margin-top: 10px; font-size: 13px; color: var(--head); }
  .follow summary { cursor: pointer; color: var(--text-secondary); }
  .follow ul { margin: 6px 0 0 18px; display: flex; flex-direction: column; gap: 4px; color: var(--text-secondary); }
  .v { font-size: 14px; color: var(--text); }
</style>
