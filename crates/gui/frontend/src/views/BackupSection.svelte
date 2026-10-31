<script lang="ts">
  import { jobs } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import { nav } from '../lib/navStore.svelte';
  import { sortJobs } from '../lib/jobVersions';
  import JobDashboard from './JobDashboard.svelte';
  import BackupWizard from './BackupWizard.svelte';
  import JobMenu from '../components/JobMenu.svelte';
  import JobRow from '../components/JobRow.svelte';
  import JobSortMenu, { type JobSortKey } from '../components/JobSortMenu.svelte';
  import type { Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let selectedId: string | null = $state(null);
  let jobMenu = $state<JobMenu | null>(null);
  let menuJobId = $state<string | null>(null);
  // Shared draft the wizard edits; its name is also editable in the list.
  let draftJob = $state<Job | null>(null);
  let draftMode: 'wizard' | 'edit' | null = $state(null);
  /** The draft was cloned from an encrypted job (see `cloneJob`). */
  let draftEncrypted = $state(false);
  /** Counts the drafts opened. The wizard is rebuilt for each one: it reads
   *  where to start from the draft once, when it is created. */
  let draftSerial = $state(0);

  // Persisted list prefs.
  const SEL_KEY = 'ob.backup.selectedJobId';
  const SORT_KEY = 'ob.backup.sortKey';
  let sortKey = $state<JobSortKey>((localStorage.getItem(SORT_KEY) as JobSortKey) ?? 'name');

  /// Why "Добавить копию" did nothing, when it does nothing.
  let addError: string | null = $state(null);

  const selectedJob = $derived(jobs.list.find((j) => j.id === selectedId) ?? null);

  $effect(() => {
    // Restore the remembered selection once the list has loaded; fall back to
    // the first job only when there is nothing remembered.
    if (!draftJob && !selectedId && jobs.list.length > 0) {
      const stored = localStorage.getItem(SEL_KEY);
      selectedId = stored && jobs.list.some((j) => j.id === stored) ? stored : jobs.list[0].id;
    }
    // Drop a stale selection only after the list is actually populated, so we
    // never clobber a remembered id while jobs are still being fetched.
    if (selectedId && jobs.list.length > 0 && !jobs.list.some((j) => j.id === selectedId)) {
      selectedId = jobs.list[0]?.id ?? null;
    }
  });
  // Persist the current selection and sort order across navigation / restarts.
  $effect(() => {
    if (selectedId) localStorage.setItem(SEL_KEY, selectedId);
  });
  $effect(() => {
    localStorage.setItem(SORT_KEY, sortKey);
  });

  const sortedJobs = $derived(sortJobs(jobs.list, sortKey, jobs.activity));

  async function addJob() {
    // Volumes are scanned once at startup and kept fresh by the background
    // drive-change poll — no re-scan needed here.
    //
    // The template comes from the engine, so this can fail — and when it did
    // on the boot medium the failure went nowhere: the exception ended in the
    // console nobody can see, the wizard never opened, and the button read as
    // broken. Whatever the reason, it has to be on screen.
    addError = null;
    try {
      draftJob = await api.newJobTemplate();
      draftEncrypted = false;
      draftMode = 'wizard';
      draftSerial++;
    } catch (e) {
      addError = String(e);
    }
  }

  function editJob() {
    if (!selectedJob) return;
    draftJob = JSON.parse(JSON.stringify(selectedJob));
    draftEncrypted = false;
    draftMode = 'edit';
    draftSerial++;
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

  /** «Диск C» → «Диск C (2)», and the next free number after that: a name the
   *  list does not have yet, and no word to translate. */
  function cloneName(name: string): string {
    const stem = name.replace(/\s*\(\d+\)$/, '');
    const taken = new Set(jobs.list.map((j) => j.name));
    let n = 2;
    while (taken.has(`${stem} (${n})`)) n++;
    return `${stem} (${n})`;
  }

  /** «Клонировать параметры»: a new job with everything this one is set up
   *  with — source, destination, schedule, scheme, exclusions — opened in the
   *  wizard so any of it can be changed before the first run. None of the
   *  original's copies come along, and neither does its key: an encrypted
   *  job's clone asks for a password of its own. */
  function cloneJob(job: Job) {
    const copy: Job = JSON.parse(JSON.stringify(job));
    copy.id = '';
    copy.name = cloneName(job.name);
    copy.last_run = null;
    copy.last_verification = null;
    copy.invalid_images = [];
    copy.verification_blocked = false;
    copy.created_at = '';
    copy.updated_at = '';
    // The disk's layout is photographed again when the clone is saved.
    copy.source_disks = [];
    copy.archive_encryption = null;
    draftEncrypted = !!job.archive_encryption;
    draftJob = copy;
    draftMode = 'wizard';
    draftSerial++;
  }

  $effect(() => {
    if (nav.cloneJob) {
      const job = nav.cloneJob;
      nav.cloneJob = null;
      cloneJob(job);
    }
  });

  const editingId = $derived(draftMode === 'edit' ? draftJob?.id : null);
</script>

{#snippet nameEditor()}
  {#if draftJob}
    <input class="name-edit" bind:value={draftJob.name} spellcheck="false" onclick={(e) => e.stopPropagation()} />
  {/if}
{/snippet}

<div class="layout">
  <!-- middle column: jobs list -->
  <div class="jobs-col">
    <div class="col-title">{m.bs_title()}</div>
    <div class="this-pc">
      <svg width="11" height="11" viewBox="0 0 24 24" fill="var(--text-muted)"><path d="M6 9l6 6 6-6z"/></svg>
      {m.bs_this_pc()}
      <JobSortMenu bind:value={sortKey} />
    </div>
    <div class="jobs-list">
      <!-- draft (new) job appears here, name editable inline -->
      {#if draftMode === 'wizard' && draftJob}
        <div class="job-item sel draft">
          <div class="job-icon pc">
            <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="1.5"><rect x="2" y="4" width="20" height="13" rx="1.5"/><path d="M8 20h8M12 17v3"/></svg>
          </div>
          <input class="name-edit" bind:value={draftJob.name} spellcheck="false" placeholder={m.bs_name_placeholder()} />
        </div>
      {/if}

      {#if jobs.list.length === 0 && draftMode !== 'wizard'}
        <div class="empty-list">{m.bs_empty()}<br/>{m.bs_empty_hint()}</div>
      {/if}

      {#each sortedJobs as job (job.id)}
        <JobRow
          {job}
          selected={selectedId === job.id && !draftJob || editingId === job.id}
          menuOpen={menuJobId === job.id}
          path={job.dest_folder}
          encryptedTitle={job.archive_encryption ? m.bs_encrypted_title() : undefined}
          onselect={() => selectJob(job.id)}
          onmenu={(e, anchor) => jobMenu?.open(job, e, anchor)}
          editor={editingId === job.id && draftJob ? nameEditor : undefined}
        />
      {/each}
    </div>
    {#if addError}
      <div class="add-error">{m.bs_add_error({ error: addError })}</div>
    {/if}
    <div class="add-bar">
      <button class="add-btn" onclick={addJob}>
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>
        {m.bs_add()}
      </button>
    </div>
  </div>

  <!-- right: wizard (inline) or dashboard -->
  <div class="dash-col">
    {#if draftJob && draftMode}
      {#key draftSerial}
        <BackupWizard mode={draftMode} job={draftJob} startEncrypted={draftEncrypted} onCancel={cancelWizard} onDone={onWizardDone} />
      {/key}
    {:else if selectedJob}
      {#key selectedJob.id}
        <JobDashboard job={selectedJob} onEdit={editJob} />
      {/key}
    {:else}
      <div class="empty-dash">
        <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="var(--border-strong)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
        <div>{m.bs_empty_dash()}</div>
        <button class="add-btn big" onclick={addJob}>+ {m.bs_add()}</button>
      </div>
    {/if}
  </div>
</div>

<JobMenu bind:this={jobMenu} bind:openJobId={menuJobId} onClone={cloneJob} />

<style>
  .layout { display: flex; height: 100%; }
  .jobs-col { width: 240px; flex-shrink: 0; background: var(--column-bg); border-right: 1px solid var(--border); display: flex; flex-direction: column; }
  /* 22px, not 24: «Резервные копии» in DejaVu Sans (Linux) did not fit the
     240px column and ended in «…». */
  .col-title { font-size: 22px; font-weight: 300; color: var(--head); padding: 22px 8px 12px 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .this-pc { display: flex; align-items: center; gap: 8px; padding: 6px 18px 6px 12px; font-size: 13px; font-weight: 600; color: var(--text-secondary); }
  .jobs-list { flex: 1; overflow: auto; padding: 8px 8px; }
  .empty-list { text-align: center; color: var(--text-muted); font-size: 13px; padding: 30px 10px; line-height: 1.6; }
  /* The draft of a new job; a saved one is a JobRow, which looks the same. */
  .job-item { width: 100%; display: flex; align-items: center; gap: 10px; padding: 10px 8px; border: 1px solid transparent; border-radius: 8px; background: none; cursor: pointer; margin-bottom: 4px; text-align: left; font-family: var(--font); }
  .job-item.sel { background: var(--accent-soft); border-color: var(--primary); }
  .job-item.draft { cursor: default; }
  .job-icon { position: relative; flex-shrink: 0; }
  .name-edit { flex: 1; min-width: 0; font-size: 14px; font-weight: 600; color: var(--text); border: 1px solid var(--primary); border-radius: 6px; padding: 5px 8px; font-family: var(--font); outline: none; background: var(--input-bg); }
  .name-edit:focus { box-shadow: 0 0 0 3px var(--accent-soft); }
  .name-edit::placeholder { color: var(--text-muted); }
  .add-error {
    margin: 0 12px 8px;
    padding: 8px 10px;
    border-radius: 8px;
    background: var(--error-soft);
    border: 1px solid var(--error-line);
    color: var(--error-text);
    font-size: 12px;
    line-height: 1.35;
  }

  /* As tall as the action bar beside it (--footer-height): one line across.
     The button is green, as «Создать копию» in that bar (owner, 2026-10-28). */
  .add-bar { display: flex; align-items: center; flex-shrink: 0; min-height: var(--footer-height); padding: 12px 16px; border-top: 1px solid var(--border); }
  .add-btn { flex: 1; display: flex; align-items: center; justify-content: center; gap: 8px; padding: 10px 24px; line-height: 20px; border: 1px solid transparent; border-radius: 6px; background: var(--green); color: #fff; cursor: pointer; font-size: 14px; font-weight: 600; font-family: var(--font); transition: background-color .15s ease; }
  .add-btn:hover { background: var(--green-hover); }
  .dash-col { flex: 1; min-width: 0; overflow: auto; }
  .empty-dash { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; color: var(--text-muted); font-size: 15px; }
  .add-btn.big { flex: none; }
</style>
