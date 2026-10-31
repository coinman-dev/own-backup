<script lang="ts" module>
  /** A copy added by hand in «Восстановление»: files, and no job in the service. */
  export interface AddedCopy {
    folder: string;
    newest: string;
    forget: () => void;
  }
</script>

<script lang="ts">
  // What can be done with a job and its copies: the menu of a row in
  // «Резервное копирование» and «Восстановление», and the windows it opens.
  // Always mounted: a window outlives the menu that opened it.
  import { tick } from 'svelte';
  import { jobs, jobStatus, deleteJob, saveJob } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import { media } from '../lib/mediaStore.svelte';
  import { toolsState } from '../lib/engineStore.svelte';
  import { goTo } from '../lib/navStore.svelte';
  import Modal from './Modal.svelte';
  import JobVerifyModal from './JobVerifyModal.svelte';
  import JobVersionsModal from './JobVersionsModal.svelte';
  import JobExportModal from './JobExportModal.svelte';
  import JobMountModal from './JobMountModal.svelte';
  import ArchiveReauthorizeModal from './ArchiveReauthorizeModal.svelte';
  import type { Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    /** The job whose menu is open, for its row. */
    openJobId?: string | null;
    onClone: (job: Job) => void;
    /** Set for a row that is a copy added by hand, not a job. */
    imported?: (job: Job) => AddedCopy | null | undefined;
  }
  let { openJobId = $bindable(null), onClone, imported }: Props = $props();

  // Delete-confirmation modal.
  let deleteId: string | null = $state(null);
  let deleteFiles = $state(false);
  /// Что помешало удалить задание или его копии — держится до следующей попытки.
  // A function, so the text follows a change of language while it is shown.
  let deleteError: (() => string) | null = $state(null);
  const deleteJobObj = $derived(jobs.list.find((j) => j.id === deleteId) ?? null);


  // ---- The menu of a job (the chevron in its row, or a right click) --------
  //
  // What can be done with a job and its copies, in one place. The row used to
  // carry a lone × that deleted; deleting is now the last item of this menu.
  let menu = $state<{ job: Job; x: number; y: number } | null>(null);
  $effect(() => {
    openJobId = menu?.job.id ?? null;
  });
  let menuEl = $state<HTMLDivElement | null>(null);

  export function open(job: Job, e: MouseEvent | KeyboardEvent, anchor?: HTMLElement) {
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
  /** The same for a copy added by hand. Taken before the menu closes: what the
   *  template holds about it goes with the menu. */
  function pickAdded(action: (added: AddedCopy) => void) {
    const added = menu && imported?.(menu.job);
    menu = null;
    if (added) action(added);
  }
  /** The service works on one thing at a time, and a job that is being copied
   *  has files that are still changing: nothing reads or deletes them then. */
  const busyWith = (job: Job) => jobStatus(job) === 'running';

  // Which window of the menu is open, and for which job.
  let act = $state<
    | { kind: 'verify'; job: Job; all: boolean }
    | { kind: 'clean' | 'mount' | 'export' | 'authorize'; job: Job }
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

  function openFolder(folder: string) {
    menuError = null;
    api.openFolder(folder).catch((e) => (menuError = String(e)));
  }

  /** «Инструменты» work on a file, which is all a copy added by hand has. */
  function toTools(added: AddedCopy, what: 'verify' | 'mount') {
    if (what === 'verify') {
      toolsState.verifyPath = added.newest;
    } else {
      toolsState.mountPath = added.newest;
      // Read-only: a copy opened to look inside must not be changed by looking.
      toolsState.rwMode = false;
      toolsState.autoMount = true;
    }
    goTo('tools');
  }

  function askDelete(id: string) {
    deleteId = id;
    deleteFiles = false;
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

</script>

{#if menu}
  {@const job = menu.job}
  {@const added = imported?.(job)}
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
  {#if added}
    <!-- Added by hand: the service keeps no job for these, so what needs one
         (renaming, cleaning versions, deleting) is not offered. -->
    <button role="menuitem" onclick={() => pickAdded((a) => toTools(a, 'verify'))}>{m.bsm_verify_latest()}</button>
    <button role="menuitem" onclick={() => pickAdded((a) => toTools(a, 'mount'))}>{m.bsm_mount()}</button>
    {#if !media.active}
      <button role="menuitem" onclick={() => pickAdded((a) => openFolder(a.folder))}>{m.bsm_open()}</button>
    {/if}
    <div class="sep"></div>
    <button role="menuitem" onclick={() => pickAdded((a) => a.forget())}>{m.rl_forget_title()}</button>
  {:else}
      <button role="menuitem" onclick={() => pick(askRename)}>{m.bsm_rename()}</button>
      {#if job.archive_encryption && !media.active}
        <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'authorize', job: j }))}>{m.bsm_reauthorize()}</button>
      {/if}
      <div class="sep"></div>
      <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'verify', job: j, all: false }))}>{m.bsm_verify_latest()}</button>
      <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'verify', job: j, all: true }))}>{m.bsm_verify_all()}</button>
      <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'clean', job: j }))}>{m.bsm_clean()}</button>
      <div class="sep"></div>
      <button role="menuitem" onclick={() => pick(onClone)}>{m.bsm_clone()}</button>
      <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'mount', job: j }))}>{m.bsm_mount()}</button>
      {#if !media.active}
        <!-- The boot medium has no file manager to open a folder in. -->
        <button role="menuitem" onclick={() => pick((j) => openFolder(j.dest_folder))}>{m.bsm_open()}</button>
      {/if}
      <button role="menuitem" disabled={busyWith(job)} onclick={() => pick((j) => (act = { kind: 'export', job: j }))}>{m.bsm_export()}</button>
      <div class="sep"></div>
      <button role="menuitem" class="danger" onclick={() => pick((j) => askDelete(j.id))}>{m.bsm_delete()}</button>
  {/if}
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
{:else if act?.kind === 'authorize'}
  <ArchiveReauthorizeModal job={act.job} onClose={() => (act = null)} />
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
  .menu-backdrop { position: fixed; inset: 0; z-index: 190; }
  .ctx-menu { position: fixed; z-index: 200; min-width: 230px; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow-lg); padding: 6px; outline: none; }
  .ctx-menu button { display: block; width: 100%; text-align: left; padding: 8px 10px; border: none; background: none; border-radius: 4px; font-size: 13px; font-family: var(--font); color: var(--text); cursor: pointer; white-space: nowrap; }
  .ctx-menu button:hover:not(:disabled), .ctx-menu button:focus-visible { background: var(--accent-soft); color: var(--accent-text); outline: none; }
  .ctx-menu button:disabled { color: var(--text-muted); opacity: 0.6; cursor: default; }
  .ctx-menu button.danger:hover, .ctx-menu button.danger:focus-visible { background: var(--error-soft); color: var(--error-text); }
  .ctx-menu .sep { height: 1px; margin: 5px 4px; background: var(--border); }
  .rename-note { margin: 10px 0 0; font-size: 12px; color: var(--text-muted); line-height: 1.45; }
  .del-q { font-size: 14px; color: var(--text); margin: 0 0 16px; }
  .del-files { display: flex; align-items: center; gap: 8px; font-size: 14px; color: var(--text); cursor: pointer; }
  .del-warn { font-size: 13px; color: var(--error-text); font-weight: 600; margin: 12px 0 0; }
  .del-left { font-size: 13px; color: var(--error-text); margin: 12px 0 0; white-space: pre-wrap; max-height: 240px; overflow: auto; }
  .del-left.plain { margin-top: 0; }
</style>
