<script lang="ts">
  import { tick } from 'svelte';
  import { jobs, jobStatus, deleteJob, saveJob } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import { media } from '../lib/mediaStore.svelte';
  import JobDashboard from './JobDashboard.svelte';
  import BackupWizard from './BackupWizard.svelte';
  import Modal from '../components/Modal.svelte';
  import JobVerifyModal from '../components/JobVerifyModal.svelte';
  import JobVersionsModal from '../components/JobVersionsModal.svelte';
  import JobExportModal from '../components/JobExportModal.svelte';
  import JobMountModal from '../components/JobMountModal.svelte';
  import type { Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { intlLocale } from '../lib/i18n.svelte';

  let selectedId: string | null = $state(null);
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
  type SortKey = 'name' | 'time' | 'size';
  let sortKey = $state<SortKey>((localStorage.getItem(SORT_KEY) as SortKey) ?? 'name');
  let sortOpen = $state(false);

  /// Why "Добавить копию" did nothing, when it does nothing.
  let addError: string | null = $state(null);

  // Delete-confirmation modal.
  let deleteId: string | null = $state(null);
  let deleteFiles = $state(false);
  /// Что помешало удалить задание или его копии — держится до следующей попытки.
  // A function, so the text follows a change of language while it is shown.
  let deleteError: (() => string) | null = $state(null);
  const deleteJobObj = $derived(jobs.list.find((j) => j.id === deleteId) ?? null);

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

  // The history records processed source bytes, not the compressed size of
  // files still present after retention. Compute the totals once per update.
  const processedByJob = $derived.by(() => {
    const totals = new Map<string, number>();
    for (const run of jobs.activity) {
      totals.set(run.job_id, (totals.get(run.job_id) ?? 0) + (run.bytes_copied || 0));
    }
    return totals;
  });
  const SORT_LABELS: Record<SortKey, () => string> = {
    name: m.bs_sort_name,
    time: m.bs_sort_time,
    size: m.bs_sort_size,
  };
  function setSort(k: SortKey) {
    sortKey = k;
    sortOpen = false;
  }
  const sortedJobs = $derived.by(() => {
    const list = [...jobs.list];
    if (sortKey === 'time') {
      list.sort((a, b) => (b.last_run?.finished_at ?? '').localeCompare(a.last_run?.finished_at ?? ''));
    } else if (sortKey === 'size') {
      list.sort((a, b) => (processedByJob.get(b.id) ?? 0) - (processedByJob.get(a.id) ?? 0));
    } else {
      list.sort((a, b) => a.name.localeCompare(b.name, intlLocale()));
    }
    return list;
  });

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

  function askDelete(id: string) {
    deleteId = id;
    deleteFiles = false;
  }

  // ---- The menu of a job (the chevron in its row, or a right click) --------
  //
  // What can be done with a job and its copies, in one place. The row used to
  // carry a lone × that deleted; deleting is now the last item of this menu.
  let menu = $state<{ job: Job; x: number; y: number } | null>(null);
  let menuEl = $state<HTMLDivElement | null>(null);

  function openMenu(job: Job, e: MouseEvent | KeyboardEvent, anchor?: HTMLElement) {
    e.preventDefault();
    e.stopPropagation();
    if (anchor) {
      const r = anchor.getBoundingClientRect();
      menu = { job, x: r.left, y: r.bottom + 4 };
    } else {
      const at = e as MouseEvent;
      menu = { job, x: at.clientX, y: at.clientY };
    }
  }
  // Keep the menu inside the window (a row near the bottom would otherwise
  // open half of it below the edge), and give it the keyboard.
  $effect(() => {
    if (!menu || !menuEl) return;
    const r = menuEl.getBoundingClientRect();
    const x = Math.max(8, Math.min(menu.x, window.innerWidth - r.width - 8));
    const y = Math.max(8, Math.min(menu.y, window.innerHeight - r.height - 8));
    if (x !== menu.x || y !== menu.y) menu = { ...menu, x, y };
    if (!menuEl.contains(document.activeElement)) menuEl.focus();
  });
  function menuKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      menu = null;
      return;
    }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    const items = [...(menuEl?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') ?? [])];
    if (items.length === 0) return;
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const step = e.key === 'ArrowDown' ? 1 : -1;
    items[(at + step + items.length) % items.length].focus();
  }
  /** Run a menu item on the job the menu was opened for. */
  function pick(action: (job: Job) => void) {
    const job = menu?.job;
    menu = null;
    if (job) action(job);
  }
  /** The service works on one thing at a time, and a job that is being copied
   *  has files that are still changing: nothing reads or deletes them then. */
  const busyWith = (job: Job) => jobStatus(job) === 'running';

  // Which window of the menu is open, and for which job.
  let act = $state<
    | { kind: 'verify'; job: Job; all: boolean }
    | { kind: 'clean' | 'mount' | 'export'; job: Job }
    | null
  >(null);
  /** What a menu item that has no window of its own could not do. */
  let menuError: string | null = $state(null);

  // «Переименовать»: only the name changes. Copies are told apart by the id
  // written inside them, not by the job's name, so the chain carries on.
  let renameJob = $state<Job | null>(null);
  let renameText = $state('');
  let renameError = $state('');
  let renameBusy = $state(false);
  let renameInput = $state<HTMLInputElement | null>(null);
  const renameReady = $derived(
    !!renameJob && renameText.trim().length > 0 && renameText.trim() !== renameJob.name,
  );
  async function askRename(job: Job) {
    renameJob = job;
    renameText = job.name;
    renameError = '';
    await tick();
    renameInput?.focus();
    renameInput?.select();
  }
  async function confirmRename() {
    if (!renameJob || !renameReady || renameBusy) return;
    renameBusy = true;
    renameError = '';
    try {
      // The stored job, not the row's copy of it: whatever else changed since
      // the list was drawn must not be written back.
      const fresh = (await api.getBackupJob(renameJob.id)) ?? renameJob;
      await saveJob({ ...fresh, name: renameText.trim() });
      renameJob = null;
    } catch (e) {
      renameError = String(e);
    } finally {
      renameBusy = false;
    }
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

  function openStorage(job: Job) {
    menuError = null;
    api.openFolder(job.dest_folder).catch((e) => (menuError = String(e)));
  }
  async function confirmDelete() {
    if (!deleteId) return;
    deleteError = null;
    try {
      const report = await deleteJob(deleteId, deleteFiles);
      // «Удалить вместе с копиями» — не обещание, которое всегда выполнимо:
      // файл может быть занят, папка недоступна, а старую папку назначения
      // журнал мог уже забыть. Промолчать об этом — значит оставить копии
      // лежать там, где пользователь уверен, что их нет.
      const everythingWent = report.kept.length === 0 && !report.history_truncated;
      if (deleteFiles && !everythingWent) {
        const kept = report.kept.map(([path, why]) => `${path} — ${why}`);
        const truncated = report.history_truncated;
        deleteError = () => {
          const lines = [...kept];
          if (truncated) lines.push(m.bs_history_truncated());
          return m.bs_deleted_partly() + '\n' + lines.join('\n');
        };
      }
    } catch (e) {
      const msg = String(e);
      deleteError = () => msg;
      return; // окно остаётся открытым: задание не удалено
    }
    deleteId = null;
  }

  function statusColor(s: string): string {
    return s === 'running' ? 'var(--primary)'
      : s === 'queued' ? 'var(--info)'
      : s === 'success' ? 'var(--success)'
      : s === 'error' ? 'var(--error)'
      : 'var(--text-muted)';
  }

  const editingId = $derived(draftMode === 'edit' ? draftJob?.id : null);
</script>

<div class="layout">
  <!-- middle column: jobs list -->
  <div class="jobs-col">
    <div class="col-title">{m.bs_title()}</div>
    <div class="this-pc">
      <svg width="11" height="11" viewBox="0 0 24 24" fill="var(--text-muted)"><path d="M6 9l6 6 6-6z"/></svg>
      {m.bs_this_pc()}
      <div class="sort">
        <button class="sort-btn" title={m.bs_sort_title({ order: SORT_LABELS[sortKey]() })} aria-label={m.bs_sort()} onclick={() => (sortOpen = !sortOpen)}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"><path d="M4 7h16M7 12h10M10 17h4"/></svg>
        </button>
        {#if sortOpen}
          <button class="sort-backdrop" aria-label={m.common_close()} onclick={() => (sortOpen = false)}></button>
          <div class="sort-menu">
            <button class:active={sortKey === 'name'} onclick={() => setSort('name')}>{m.bs_sort_name()}</button>
            <button class:active={sortKey === 'time'} onclick={() => setSort('time')}>{m.bs_sort_time()}</button>
            <button class:active={sortKey === 'size'} onclick={() => setSort('size')}>{m.bs_sort_size()}</button>
          </div>
        {/if}
      </div>
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
        <button
          class="job-item"
          class:sel={selectedId === job.id && !draftJob || editingId === job.id}
          class:menu-open={menu?.job.id === job.id}
          onclick={() => selectJob(job.id)}
          oncontextmenu={(e) => openMenu(job, e)}
        >
          <div class="job-icon">
            <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.6"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
            <span class="status-dot" style="background:{statusColor(jobStatus(job))}"></span>
          </div>
          {#if editingId === job.id && draftJob}
            <input class="name-edit" bind:value={draftJob.name} spellcheck="false" onclick={(e) => e.stopPropagation()} />
          {:else}
            <div class="job-meta">
              <div class="job-name">
                {job.name}{#if job.archive_encryption}<span
                    class="enc-badge"
                    title={m.bs_encrypted_title()}
                    aria-label={m.bs_encrypted()}><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="2.2" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg></span
                  >{/if}
              </div>
              <div class="job-path">{job.dest_folder}</div>
            </div>
            <!-- A span, because a button cannot hold a button; focusable AND
                 activatable, so a keyboard user who reaches it can press it. -->
            <span
              class="more"
              role="button"
              tabindex="0"
              title={m.bsm_menu()}
              aria-label={m.bsm_menu()}
              aria-haspopup="menu"
              onclick={(e) => openMenu(job, e, e.currentTarget)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') openMenu(job, e, e.currentTarget);
              }}><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg></span>
          {/if}
        </button>
      {/each}
    </div>
    {#if addError}
      <div class="add-error">{m.bs_add_error({ error: addError })}</div>
    {/if}
    <button class="add-btn" onclick={addJob}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>
      {m.bs_add()}
    </button>
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

{#if menu}
  {@const job = menu.job}
  <div
    class="menu-backdrop"
    role="presentation"
    onclick={() => (menu = null)}
    oncontextmenu={(e) => {
      e.preventDefault();
      menu = null;
    }}
  ></div>
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div
    class="ctx-menu"
    role="menu"
    tabindex="-1"
    aria-label={job.name}
    bind:this={menuEl}
    style="left:{menu.x}px; top:{menu.y}px"
    onkeydown={menuKey}
  >
    <button role="menuitem" onclick={() => pick(askRename)}>{m.bsm_rename()}</button>
    <div class="sep"></div>
    <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'verify', job: j, all: false }))}>{m.bsm_verify_latest()}</button>
    <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'verify', job: j, all: true }))}>{m.bsm_verify_all()}</button>
    <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'clean', job: j }))}>{m.bsm_clean()}</button>
    <div class="sep"></div>
    <button role="menuitem" onclick={() => pick(cloneJob)}>{m.bsm_clone()}</button>
    <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'mount', job: j }))}>{m.bsm_mount()}</button>
    {#if !media.active}
      <!-- The boot medium has no file manager to open a folder in. -->
      <button role="menuitem" onclick={() => pick(openStorage)}>{m.bsm_open()}</button>
    {/if}
    <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'export', job: j }))}>{m.bsm_export()}</button>
    <div class="sep"></div>
    <button role="menuitem" class="danger" onclick={() => pick((j) => askDelete(j.id))}>{m.bsm_delete()}</button>
  </div>
{/if}

{#if act?.kind === 'verify'}
  <JobVerifyModal job={act.job} allVersions={act.all} onClose={() => (act = null)} />
{:else if act?.kind === 'clean'}
  <JobVersionsModal job={act.job} onClose={() => (act = null)} />
{:else if act?.kind === 'mount'}
  <JobMountModal job={act.job} onClose={() => (act = null)} />
{:else if act?.kind === 'export'}
  <JobExportModal job={act.job} onClose={() => (act = null)} />
{/if}

<Modal
  open={renameJob !== null}
  title={m.bsm_rename_title()}
  confirmLabel={m.bsm_rename()}
  confirmDisabled={!renameReady || renameBusy}
  cancelLabel={m.common_cancel()}
  onClose={() => (renameJob = null)}
  onConfirm={confirmRename}
>
  <input
    class="form-input"
    type="text"
    spellcheck="false"
    aria-label={m.bs_name_placeholder()}
    placeholder={m.bs_name_placeholder()}
    bind:this={renameInput}
    bind:value={renameText}
    onkeydown={(e) => {
      if (e.key === 'Enter') void confirmRename();
    }}
  />
  <p class="rename-note">{m.bsm_rename_note()}</p>
  {#if renameError}
    <p class="del-left">{renameError}</p>
  {/if}
</Modal>

{#if menuError}
  <Modal
    open
    title={m.bsm_failed_title()}
    confirmLabel={m.common_got_it()}
    cancelLabel=""
    onClose={() => (menuError = null)}
    onConfirm={() => (menuError = null)}
  >
    <p class="del-left plain">{menuError}</p>
  </Modal>
{/if}

<Modal
  open={deleteId !== null}
  title={m.bs_delete_title()}
  danger
  confirmLabel={m.bs_delete()}
  cancelLabel={m.common_cancel()}
  onClose={() => {
    // A failure is shown inside this window; closing it closes the failure
    // too. Left set, it reappeared at once under «Копии удалены не полностью»
    // — about a job that had not been deleted at all.
    deleteId = null;
    deleteError = null;
  }}
  onConfirm={confirmDelete}
>
  <p class="del-q">{deleteJobObj ? m.bs_delete_q_named({ name: deleteJobObj.name }) : m.bs_delete_q()}</p>
  <label class="del-files">
    <input type="checkbox" bind:checked={deleteFiles} />
    {m.bs_delete_files()}
  </label>
  {#if deleteFiles}
    <p class="del-warn">{m.bs_delete_warn()}</p>
  {/if}
  {#if deleteError}
    <p class="del-left">{deleteError?.()}</p>
  {/if}
</Modal>

{#if deleteError && deleteId === null}
  <Modal
    open
    title={m.bs_partly_title()}
    confirmLabel={m.common_got_it()}
    cancelLabel=""
    onClose={() => (deleteError = null)}
    onConfirm={() => (deleteError = null)}
  >
    <p class="del-left">{deleteError?.()}</p>
  </Modal>
{/if}

<style>
  .layout { display: flex; height: 100%; }
  .jobs-col { width: 240px; flex-shrink: 0; background: var(--column-bg); border-right: 1px solid var(--border); display: flex; flex-direction: column; }
  .col-title { font-size: 24px; font-weight: 300; color: var(--head); padding: 22px 18px 12px; }
  .this-pc { display: flex; align-items: center; gap: 8px; padding: 6px 18px; font-size: 13px; font-weight: 600; color: var(--text-secondary); }
  .sort { margin-left: auto; position: relative; display: flex; }
  .sort-btn { background: none; border: none; padding: 3px 5px; cursor: pointer; color: var(--text-muted); display: flex; border-radius: 4px; }
  .sort-btn:hover { color: var(--head); background: var(--track); }
  .sort-backdrop { position: fixed; inset: 0; z-index: 40; background: none; border: none; cursor: default; }
  .sort-menu { position: absolute; top: 100%; right: 0; margin-top: 4px; z-index: 50; min-width: 150px; padding: 4px; display: flex; flex-direction: column; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow-lg); }
  .sort-menu button { text-align: left; background: none; border: none; padding: 8px 12px; font-size: 13px; font-weight: 600; color: var(--text); cursor: pointer; border-radius: 4px; font-family: var(--font); }
  .sort-menu button:hover { background: var(--panel2); }
  .sort-menu button.active { color: var(--accent-text); }
  .del-q { font-size: 14px; color: var(--text); margin: 0 0 16px; }
  .del-files { display: flex; align-items: center; gap: 8px; font-size: 14px; color: var(--text); cursor: pointer; }
  .del-warn { font-size: 13px; color: var(--error-text); font-weight: 600; margin: 12px 0 0; }
  .del-left { font-size: 13px; color: var(--error-text); margin: 12px 0 0; white-space: pre-wrap; max-height: 240px; overflow: auto; }
  .jobs-list { flex: 1; overflow: auto; padding: 8px 8px; }
  .empty-list { text-align: center; color: var(--text-muted); font-size: 13px; padding: 30px 10px; line-height: 1.6; }
  .job-item { width: 100%; display: flex; align-items: center; gap: 10px; padding: 10px 8px; border: 1px solid transparent; border-radius: 8px; background: none; cursor: pointer; margin-bottom: 4px; text-align: left; font-family: var(--font); }
  .job-item:hover { background: var(--panel2); }
  .job-item.sel { background: var(--accent-soft); border-color: var(--primary); }
  .job-item.draft { cursor: default; }
  .job-icon { position: relative; flex-shrink: 0; }
  .status-dot { position: absolute; right: -2px; bottom: 0; width: 11px; height: 11px; border-radius: 50%; border: 2px solid var(--column-bg); }
  .job-meta { flex: 1; min-width: 0; }
  .job-name { font-size: 14px; font-weight: 600; color: var(--text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .enc-badge { margin-left: 6px; font-size: 11px; }
  .enc-badge svg { vertical-align: -1px; }
  .job-path { font-size: 12px; color: var(--text-muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .name-edit { flex: 1; min-width: 0; font-size: 14px; font-weight: 600; color: var(--text); border: 1px solid var(--primary); border-radius: 6px; padding: 5px 8px; font-family: var(--font); outline: none; background: var(--input-bg); }
  .name-edit:focus { box-shadow: 0 0 0 3px var(--accent-soft); }
  .name-edit::placeholder { color: var(--text-muted); }
  /* Shown for the row under the pointer, the chosen one, and the one whose
     menu is open or which the keyboard has reached. */
  .more { opacity: 0; color: var(--text-muted); padding: 4px; display: flex; border-radius: 4px; }
  .more:hover, .job-item.menu-open .more { color: var(--head); background: var(--track); }
  .job-item:hover .more, .job-item.sel .more, .job-item.menu-open .more, .more:focus-visible { opacity: 1; }

  .menu-backdrop { position: fixed; inset: 0; z-index: 190; }
  .ctx-menu { position: fixed; z-index: 200; min-width: 230px; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow-lg); padding: 6px; outline: none; }
  .ctx-menu button { display: block; width: 100%; text-align: left; padding: 8px 10px; border: none; background: none; border-radius: 4px; font-size: 13px; font-family: var(--font); color: var(--text); cursor: pointer; white-space: nowrap; }
  .ctx-menu button:hover:not(:disabled), .ctx-menu button:focus-visible { background: var(--accent-soft); color: var(--accent-text); outline: none; }
  .ctx-menu button:disabled { color: var(--text-muted); opacity: 0.6; cursor: default; }
  .ctx-menu button.danger:hover, .ctx-menu button.danger:focus-visible { background: var(--error-soft); color: var(--error-text); }
  .ctx-menu .sep { height: 1px; margin: 5px 4px; background: var(--border); }
  .rename-note { margin: 10px 0 0; font-size: 12px; color: var(--text-muted); line-height: 1.45; }
  .del-left.plain { margin-top: 0; }
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

  /* As tall as the action bar beside it (--footer-height): one line across. */
  .add-btn { display: flex; align-items: center; gap: 10px; flex-shrink: 0; height: var(--footer-height); padding: 0 18px; background: none; border: none; border-top: 1px solid var(--border); cursor: pointer; font-size: 14px; font-weight: 600; color: var(--accent-text); font-family: var(--font); }
  .add-btn:hover { background: var(--panel2); }
  .dash-col { flex: 1; min-width: 0; overflow: auto; }
  .empty-dash { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; color: var(--text-muted); font-size: 15px; }
  .add-btn.big { height: auto; border: 1px solid var(--primary); border-radius: 6px; color: var(--accent-text); padding: 10px 24px; }
  .add-btn.big:hover { background: var(--accent-soft); }
</style>
