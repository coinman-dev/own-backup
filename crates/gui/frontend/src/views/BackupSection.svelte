<script lang="ts">
  import { jobs, jobStatus, deleteJob } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import JobDashboard from './JobDashboard.svelte';
  import BackupWizard from './BackupWizard.svelte';
  import type { Job } from '../lib/types';

  let selectedId: string | null = $state(null);
  // Shared draft the wizard edits; its name is also editable in the list.
  let draftJob: Job | null = $state(null);
  let draftMode: 'wizard' | 'edit' | null = $state(null);

  const selectedJob = $derived(jobs.list.find((j) => j.id === selectedId) ?? null);

  $effect(() => {
    if (!draftJob && !selectedId && jobs.list.length > 0) selectedId = jobs.list[0].id;
    if (selectedId && !jobs.list.some((j) => j.id === selectedId)) {
      selectedId = jobs.list[0]?.id ?? null;
    }
  });

  async function addJob() {
    // Volumes are scanned once at startup and kept fresh by the background
    // drive-change poll — no re-scan needed here.
    draftJob = await api.newJobTemplate();
    draftMode = 'wizard';
  }

  function editJob() {
    if (!selectedJob) return;
    draftJob = JSON.parse(JSON.stringify(selectedJob));
    draftMode = 'edit';
  }

  function cancelWizard() {
    draftJob = null;
    draftMode = null;
  }

  function onWizardDone(job: Job) {
    selectedId = job.id;
    draftJob = null;
    draftMode = null;
  }

  function selectJob(id: string) {
    // Switching away discards an in-progress draft.
    draftJob = null;
    draftMode = null;
    selectedId = id;
  }

  async function removeJob(id: string, e: Event) {
    e.stopPropagation();
    if (confirm('Удалить это задание? Файлы образов на диске останутся.')) {
      await deleteJob(id);
    }
  }

  function statusColor(s: string): string {
    return s === 'running' ? 'var(--primary)'
      : s === 'queued' ? '#5a8fd8'
      : s === 'success' ? 'var(--green)'
      : s === 'error' ? 'var(--error)'
      : 'var(--text-muted)';
  }

  const editingId = $derived(draftMode === 'edit' ? draftJob?.id : null);
</script>

<div class="layout">
  <!-- middle column: jobs list -->
  <div class="jobs-col">
    <div class="col-title">Резервные копии</div>
    <div class="this-pc">
      <svg width="11" height="11" viewBox="0 0 24 24" fill="var(--text-muted)"><path d="M6 9l6 6 6-6z"/></svg>
      Этот компьютер
    </div>
    <div class="jobs-list">
      <!-- draft (new) job appears here, name editable inline -->
      {#if draftMode === 'wizard' && draftJob}
        <div class="job-item sel draft">
          <div class="job-icon pc">
            <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.5"><rect x="2" y="4" width="20" height="13" rx="1.5"/><path d="M8 20h8M12 17v3"/></svg>
          </div>
          <input class="name-edit" bind:value={draftJob.name} spellcheck="false" placeholder="Имя копии" />
        </div>
      {/if}

      {#if jobs.list.length === 0 && draftMode !== 'wizard'}
        <div class="empty-list">Пока нет заданий.<br/>Нажмите «Добавить копию».</div>
      {/if}

      {#each jobs.list as job (job.id)}
        <button class="job-item" class:sel={selectedId === job.id && !draftJob || editingId === job.id} onclick={() => selectJob(job.id)}>
          <div class="job-icon">
            <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.2"/></svg>
            <span class="status-dot" style="background:{statusColor(jobStatus(job))}"></span>
          </div>
          {#if editingId === job.id && draftJob}
            <input class="name-edit" bind:value={draftJob.name} spellcheck="false" onclick={(e) => e.stopPropagation()} />
          {:else}
            <div class="job-meta">
              <div class="job-name">{job.name}</div>
              <div class="job-path">{job.dest_folder}</div>
            </div>
            <span class="del" onclick={(e) => removeJob(job.id, e)} role="button" tabindex="0">×</span>
          {/if}
        </button>
      {/each}
    </div>
    <button class="add-btn" onclick={addJob}>
      <svg width="18" height="18" viewBox="0 0 24 24" stroke="var(--primary)" stroke-width="2" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>
      Добавить копию
    </button>
  </div>

  <!-- right: wizard (inline) or dashboard -->
  <div class="dash-col">
    {#if draftJob && draftMode}
      <BackupWizard mode={draftMode} job={draftJob} onCancel={cancelWizard} onDone={onWizardDone} />
    {:else if selectedJob}
      {#key selectedJob.id}
        <JobDashboard job={selectedJob} onEdit={editJob} />
      {/key}
    {:else}
      <div class="empty-dash">
        <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="var(--border)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
        <div>Создайте задание резервного копирования</div>
        <button class="add-btn big" onclick={addJob}>+ Добавить копию</button>
      </div>
    {/if}
  </div>
</div>

<style>
  .layout { display: flex; height: 100%; }
  .jobs-col { width: 240px; flex-shrink: 0; background: var(--column-bg); border-right: 1px solid var(--border); display: flex; flex-direction: column; }
  .col-title { font-size: 22px; font-weight: 300; color: var(--head); padding: 22px 18px 12px; }
  .this-pc { display: flex; align-items: center; gap: 8px; padding: 6px 18px; font-size: 13px; font-weight: 600; color: var(--head); }
  .jobs-list { flex: 1; overflow: auto; padding: 8px 8px; }
  .empty-list { text-align: center; color: var(--text-muted); font-size: 13px; padding: 30px 10px; line-height: 1.6; }
  .job-item { width: 100%; display: flex; align-items: center; gap: 10px; padding: 10px 8px; border: 1px solid transparent; border-radius: 6px; background: none; cursor: pointer; margin-bottom: 4px; text-align: left; font-family: var(--font); }
  .job-item:hover { background: var(--hover, #f2f2f8); }
  .job-item.sel { background: var(--accent-soft); border-color: var(--primary); }
  .job-item.draft { cursor: default; }
  .job-icon { position: relative; flex-shrink: 0; }
  .status-dot { position: absolute; right: -2px; bottom: 0; width: 11px; height: 11px; border-radius: 50%; border: 2px solid var(--column-bg); }
  .job-meta { flex: 1; min-width: 0; }
  .job-name { font-size: 14px; font-weight: 600; color: var(--head); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .job-path { font-size: 12px; color: var(--text-muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .name-edit { flex: 1; min-width: 0; font-size: 14px; font-weight: 600; color: var(--head); border: 1px solid var(--primary); border-radius: 4px; padding: 5px 8px; font-family: var(--font); outline: none; background: #fff; }
  .del { opacity: 0; color: var(--text-muted); font-size: 18px; line-height: 1; padding: 0 4px; }
  .job-item:hover .del { opacity: 1; }
  .add-btn { display: flex; align-items: center; gap: 10px; padding: 15px 18px; background: none; border: none; border-top: 1px solid var(--border); cursor: pointer; font-size: 14px; font-weight: 600; color: var(--head); font-family: var(--font); }
  .add-btn:hover { color: var(--primary); }
  .dash-col { flex: 1; min-width: 0; overflow: auto; }
  .empty-dash { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; color: var(--text-muted); font-size: 15px; }
  .add-btn.big { border: 1px solid var(--primary); border-radius: 4px; color: var(--primary); padding: 10px 24px; }
</style>
