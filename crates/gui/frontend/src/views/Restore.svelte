<script lang="ts">
  import { jobs, jobStatus } from '../lib/jobStore.svelte';
  import RestoreDashboard from './RestoreDashboard.svelte';
  import DiskRestore from './DiskRestore.svelte';

  let mode: 'volume' | 'disk' = $state('volume');
  let selectedId: string | null = $state(null);
  const selectedJob = $derived(jobs.list.find((j) => j.id === selectedId) ?? null);

  $effect(() => {
    if (!selectedId && jobs.list.length > 0) selectedId = jobs.list[0].id;
    if (selectedId && !jobs.list.some((j) => j.id === selectedId)) {
      selectedId = jobs.list[0]?.id ?? null;
    }
  });

  function statusColor(s: string): string {
    return s === 'running' ? 'var(--primary)'
      : s === 'queued' ? '#5a8fd8'
      : s === 'success' ? 'var(--green)'
      : s === 'error' ? 'var(--error)'
      : 'var(--text-muted)';
  }
</script>

<div class="restore-root">
<div class="mode-bar">
  <button class="mode" class:on={mode === 'volume'} onclick={() => (mode = 'volume')}>Тома</button>
  <button class="mode" class:on={mode === 'disk'} onclick={() => (mode = 'disk')}>Диск целиком (загрузочный)</button>
</div>

{#if mode === 'disk'}
  <div class="disk-wrap"><DiskRestore /></div>
{:else}
<div class="layout">
  <!-- middle column: backups list (same as Backup) -->
  <div class="jobs-col">
    <div class="col-title">Резервные копии</div>
    <div class="this-pc">
      <svg width="11" height="11" viewBox="0 0 24 24" fill="var(--text-muted)"><path d="M6 9l6 6 6-6z"/></svg>
      Этот компьютер
    </div>
    <div class="jobs-list">
      {#if jobs.list.length === 0}
        <div class="empty-list">Нет резервных копий.<br/>Создайте их в разделе «Резервное копирование».</div>
      {/if}
      {#each jobs.list as job (job.id)}
        <button class="job-item" class:sel={selectedId === job.id} onclick={() => (selectedId = job.id)}>
          <div class="job-icon">
            <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.2"/></svg>
            <span class="status-dot" style="background:{statusColor(jobStatus(job))}"></span>
          </div>
          <div class="job-meta">
            <div class="job-name">{job.name}</div>
            <div class="job-path">{job.dest_folder}</div>
          </div>
        </button>
      {/each}
    </div>
  </div>

  <!-- right: restore panel -->
  <div class="dash-col">
    {#if selectedJob}
      {#key selectedJob.id}
        <RestoreDashboard job={selectedJob} />
      {/key}
    {:else}
      <div class="empty-dash">
        <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="var(--border)" stroke-width="1.4"><path d="M4 4v5h5M20 20v-5h-5M4 9a8 8 0 0 1 14-4M20 15a8 8 0 0 1-14 4"/></svg>
        <div>Нет резервных копий для восстановления</div>
      </div>
    {/if}
  </div>
</div>
{/if}
</div>

<style>
  .restore-root { display: flex; flex-direction: column; height: 100%; }
  .mode-bar { display: flex; gap: 4px; padding: 10px 16px; border-bottom: 1px solid var(--border); flex-shrink: 0; }
  .mode { padding: 7px 16px; border: 1px solid var(--border); border-radius: 6px; background: none; color: var(--text-muted); font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; }
  .mode.on { background: var(--primary); color: #fff; border-color: var(--primary); }
  .disk-wrap { flex: 1; overflow: auto; }
  .layout { display: flex; flex: 1; min-height: 0; }
  .jobs-col { width: 240px; flex-shrink: 0; background: var(--column-bg); border-right: 1px solid var(--border); display: flex; flex-direction: column; }
  .col-title { font-size: 22px; font-weight: 300; color: var(--head); padding: 22px 18px 12px; }
  .this-pc { display: flex; align-items: center; gap: 8px; padding: 6px 18px; font-size: 13px; font-weight: 600; color: var(--head); }
  .jobs-list { flex: 1; overflow: auto; padding: 8px 8px; }
  .empty-list { text-align: center; color: var(--text-muted); font-size: 13px; padding: 30px 10px; line-height: 1.6; }
  .job-item { width: 100%; display: flex; align-items: center; gap: 10px; padding: 10px 8px; border: 1px solid transparent; border-radius: 6px; background: none; cursor: pointer; margin-bottom: 4px; text-align: left; font-family: var(--font); }
  .job-item:hover { background: var(--hover, #f2f2f8); }
  .job-item.sel { background: var(--accent-soft); border-color: var(--primary); }
  .job-icon { position: relative; flex-shrink: 0; }
  .status-dot { position: absolute; right: -2px; bottom: 0; width: 11px; height: 11px; border-radius: 50%; border: 2px solid var(--column-bg); }
  .job-meta { flex: 1; min-width: 0; }
  .job-name { font-size: 14px; font-weight: 600; color: var(--head); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .job-path { font-size: 12px; color: var(--text-muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .dash-col { flex: 1; min-width: 0; overflow: auto; }
  .empty-dash { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; color: var(--text-muted); font-size: 15px; }
</style>
