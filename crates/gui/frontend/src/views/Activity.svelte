<script lang="ts">
  import { media } from '../lib/mediaStore.svelte';
  import { jobs, reloadActivity } from '../lib/jobStore.svelte';
  import { formatBytes, formatDate, formatDuration } from '../lib/format';

  const speed = (r: { bytes_copied: number; duration_secs: number }) =>
    r.duration_secs > 0 ? (r.bytes_copied / r.duration_secs / 1_000_000).toFixed(0) : '0';

  const kindLabel = (k: string) =>
    k === 'incremental' ? 'инкрементная' : k === 'differential' ? 'дифференциальная' : 'полная';

  /** Container format of the produced image, derived from its file name. */
  const formatLabel = (p: string) =>
    p.toLowerCase().endsWith('.qcow2') ? 'qcow2 (сжатый)' : 'VHDX';
</script>

<div class="page">
  <div class="head">
    <h1>Активность</h1>
    <button class="refresh" onclick={reloadActivity}>Обновить</button>
  </div>

  {#if jobs.activity.length === 0}
    <div class="empty">Здесь появится история резервных копий.</div>
  {/if}

  <div class="list">
    {#each jobs.activity as r (r.id)}
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
    {/each}
  </div>
</div>

<style>
  .page { padding: 30px 44px; max-width: 940px; height: 100%; overflow: auto; }
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
  .v { font-size: 14px; color: var(--head); }
</style>
