<script lang="ts">
  import { pickPath } from '../lib/pickPath.svelte';
  import { jobs, jobStatus } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import RestoreDashboard from './RestoreDashboard.svelte';
  import type { Job, ImageInfo, LockedImage } from '../lib/types';

  // Copies added by hand via «+ Добавить копию»: files picked from disk, read
  // straight from their own metadata — no jobs.json needed (Boot-CD).
  //
  // One entry per JOB, not per file. The files of one job — a full copy and
  // the increments over it — carry the same job id, and the restore screen
  // pages between them with ‹ ›. Adding them one by one gave a list of lookalike
  // lines, each with arrows that could not move (2026-09-25). So whatever is
  // picked, a file or a folder, the folder is read and its files are grouped by
  // the job they belong to.
  //
  // An encrypted file is grouped without its key: the job id and the date are
  // in its open header. The contents are the restore screen's to open, and it
  // asks for the password when they are needed — once for the whole job.

  /** One job's copies found in one folder. */
  interface Imported {
    id: string;
    /** A synthetic job the restore screen renders: its folder is where the
     *  files are, its last run the newest of them. */
    job: Job;
    /** The job id the files carry; the restore screen finds the versions by
     *  it. Undefined for a file that carries none (the CLI's) — a lone copy. */
    chainJobId: string | undefined;
    sealed: boolean;
    paths: string[];
  }
  let imported: Imported[] = $state([]);
  let addError: string | null = $state(null);
  let addOpen = $state(false); // "Файл / Папка с копиями" choice modal
  /** Keep what was added, so it is there next time. On by default: having to
   *  find the same file again on every launch is the sort of thing nobody asks
   *  for and everybody resents. */
  let rememberAdded = $state(true);

  /** Paths remembered in the app config, restored on the next launch. The card
   *  itself is always read from the image's own metadata — this only records
   *  which files to look at, which is what keeps it working on a machine with
   *  no job database. */
  let config = $state<Record<string, unknown> | null>(null);

  const samePath = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();
  const folderOf = (path: string) => path.replace(/[\\/][^\\/]*$/, '');

  $effect(() => {
    let live = true;
    api.getConfig().then(
      async (cfg) => {
        if (!live) return;
        config = cfg;
        const paths = (cfg.remembered_backups as string[] | undefined) ?? [];
        // Read each folder once, however many of its files are remembered.
        const seen: string[] = [];
        for (const folder of [...new Set(paths.map((p) => folderOf(p).toLowerCase()))]) {
          const groups = await groupsIn(folderOf(paths.find((p) => folderOf(p).toLowerCase() === folder)!));
          if (!live) return;
          for (const g of groups) {
            if (g.paths.some((p) => paths.some((r) => samePath(p, r)))) {
              addEntry(g);
              seen.push(...g.paths);
            }
          }
        }
        // Moved, deleted, or on a drive that is not plugged in: dropped quietly
        // rather than nagging — the file is simply not here. New versions of a
        // remembered job are remembered with it.
        const next = [...paths.filter((p) => seen.some((s) => samePath(s, p))), ...seen];
        const changed =
          next.length !== paths.length || next.some((p) => !paths.some((q) => samePath(p, q)));
        if (live && changed) void persistRemembered(next);
      },
      () => {},
    );
    return () => { live = false; };
  });

  async function persistRemembered(paths: string[]): Promise<void> {
    if (!config) return;
    const unique = paths.filter((p, i) => paths.findIndex((q) => samePath(p, q)) === i);
    const next = { ...config, remembered_backups: unique };
    config = next;
    try {
      await api.saveConfig(next);
    } catch (e) {
      addError = `Список копий не сохранён: ${e}`;
    }
  }

  /** Record `paths` so they come back next launch, if the user asked for it. */
  async function remember(paths: string[]): Promise<void> {
    if (!rememberAdded || paths.length === 0) return;
    const current = (config?.remembered_backups as string[] | undefined) ?? [];
    await persistRemembered([...current, ...paths]);
  }

  /** Forget one entry — from the list and, with all its files, from the config. */
  async function forget(id: string): Promise<void> {
    const gone = imported.find((x) => x.id === id);
    imported = imported.filter((x) => x.id !== id);
    if (selectedId === id) selectedId = jobs.list[0]?.id ?? imported[0]?.id ?? null;
    const current = (config?.remembered_backups as string[] | undefined) ?? [];
    await persistRemembered(current.filter((p) => !gone?.paths.some((g) => samePath(g, p))));
  }

  let selectedId: string | null = $state(null);
  const selectedImported = $derived(imported.find((x) => x.id === selectedId) ?? null);
  const selectedJob = $derived(
    selectedImported?.job ?? jobs.list.find((j) => j.id === selectedId) ?? null,
  );

  $effect(() => {
    const known = (id: string) =>
      jobs.list.some((j) => j.id === id) || imported.some((x) => x.id === id);
    if (!selectedId && jobs.list.length > 0) selectedId = jobs.list[0].id;
    if (selectedId && !known(selectedId)) selectedId = jobs.list[0]?.id ?? null;
  });

  /** A job's name, back from its files: `<name>_<source>_<kind>_<stamp>.<ext>`
   *  is how every copy is named. The name itself is not inside an image. */
  function nameFromFile(path: string): string {
    const file = path.split(/[\\/]/).pop() ?? path;
    return file.match(/^(.*)_[^_]+_(?:full|inc|diff)_\d{8}-\d{6}\.[^.]+$/i)?.[1] ?? file;
  }

  /** «2 копии», «5 копий». */
  function copiesWord(n: number): string {
    const d = n % 10;
    const dd = n % 100;
    if (d === 1 && dd !== 11) return `${n} копия`;
    if (d >= 2 && d <= 4 && (dd < 12 || dd > 14)) return `${n} копии`;
    return `${n} копий`;
  }

  interface FoundFile {
    path: string;
    created_at: string;
    job_id: string;
    info: ImageInfo | null;
    sealed: boolean;
  }

  /** Every job whose copies lie in `folder`, one entry each, newest file first. */
  async function groupsIn(folder: string): Promise<Imported[]> {
    const [plain, sealed] = await Promise.all([
      api.listImageBackups(folder).catch(() => [] as ImageInfo[]),
      api.listLockedImages(folder).catch(() => [] as LockedImage[]),
    ]);
    const files: FoundFile[] = [
      ...plain.map((i) => ({ path: i.image_path, created_at: i.created_at, job_id: i.job_id ?? '', info: i, sealed: false })),
      ...sealed.map((l) => ({ path: l.image_path, created_at: l.created_at, job_id: l.job_id, info: null, sealed: true })),
    ];
    const byJob = new Map<string, FoundFile[]>();
    for (const f of files) {
      // A copy that names no job (the CLI's) has nothing to be grouped by.
      const key = f.job_id || `file:${f.path}`;
      byJob.set(key, [...(byJob.get(key) ?? []), f]);
    }
    return [...byJob.entries()].map(([key, list]) =>
      makeEntry(folder, key.startsWith('file:') ? undefined : key, list),
    );
  }

  /** The synthetic job the restore screen renders for one group of files. */
  function makeEntry(folder: string, jobId: string | undefined, files: FoundFile[]): Imported {
    const list = [...files].sort((a, b) => (a.created_at < b.created_at ? 1 : -1));
    const newest = list[0];
    const info = newest.info;
    const id = jobId ? `import:${folder.toLowerCase()}#${jobId}` : `file:${newest.path}`;
    const name = nameFromFile(newest.path);
    const job = {
      id,
      name,
      source: '',
      source_label: '',
      sources: [],
      source_labels: [],
      requires_password: false,
      disk_source: null,
      source_disks: [],
      // A job's folder, so the restore screen finds every version in it; a
      // lone file is its own "folder" and stays a single copy.
      dest_folder: jobId ? folder : newest.path,
      image_format: newest.path.toLowerCase().endsWith('.qcow2') ? 'qcow2' : 'vhdx',
      last_run: {
        id,
        job_id: id,
        job_name: name,
        started_at: '',
        finished_at: newest.created_at,
        status: 'success',
        image_path: newest.path,
        source_bytes: info?.source_bytes ?? 0,
        bytes_copied: 0,
        used_blocks: info?.used_blocks ?? 0,
        total_blocks: info?.total_blocks ?? 0,
        duration_secs: 0,
        smart_mode: true,
        vss_used: false,
        error: '',
        backup_kind: '',
        base_image_path: '',
        increment_index: 0,
        blocks_unchanged: 0,
        excluded_files: info?.excluded_files ?? 0,
        excluded_bytes: info?.excluded_bytes ?? 0,
        categories: info?.categories ?? null,
      },
    } as unknown as Job;
    return {
      id,
      job,
      chainJobId: jobId,
      sealed: list.some((f) => f.sealed),
      paths: list.map((f) => f.path),
    };
  }

  /** Put an entry on the list, or refresh the one already there (the folder
   *  may have gained a copy since). A job this machine already has, writing
   *  into this very folder, is not added twice: its own line pages the same
   *  files. Returns the id to select. */
  function addEntry(entry: Imported): string {
    const own = jobs.list.find(
      (j) => j.id === entry.chainJobId && samePath(j.dest_folder, entry.job.dest_folder),
    );
    if (own) return own.id;
    const at = imported.findIndex((x) => x.id === entry.id);
    imported = at >= 0 ? imported.map((x, i) => (i === at ? entry : x)) : [...imported, entry];
    return entry.id;
  }

  // "+ Добавить копию" → пользователь выбрал «Файл».
  async function addFile() {
    addOpen = false;
    addError = null;
    const picked = await pickPath({
      directory: false,
      title: 'Резервная копия',
      extensions: ['vhdx', 'avhdx', 'qcow2', 'obk'],
    });
    if (typeof picked !== 'string') return;
    try {
      // The file's folder holds the rest of its job — the full copy under an
      // increment, the increments over a full. They come along.
      const group = (await groupsIn(folderOf(picked))).find((g) =>
        g.paths.some((p) => samePath(p, picked)),
      );
      if (!group) {
        // Not a copy the listing could read: the file's own error says why.
        await api.readImageInfo(picked);
        addError = 'Не удалось прочитать эту копию.';
        return;
      }
      selectedId = addEntry(group);
      await remember(group.paths);
    } catch (e) {
      addError = String(e);
    }
  }

  // "+ Добавить копию" → пользователь выбрал «Папка с копиями»: авто-скан.
  async function addFolder() {
    addOpen = false;
    addError = null;
    // The extensions are named although a FOLDER is being picked: the picker
    // shows those files greyed out, so the folder holding the copies can be
    // told from the one next to it. Without them it looked empty.
    const picked = await pickPath({
      directory: true,
      title: 'Папка с копиями',
      extensions: ['vhdx', 'avhdx', 'qcow2', 'obk'],
    });
    if (typeof picked !== 'string') return;
    try {
      const groups = await groupsIn(picked);
      if (groups.length === 0) {
        addError = 'В выбранной папке не найдено копий с метаданными.';
        return;
      }
      let first: string | null = null;
      for (const g of groups) {
        const id = addEntry(g);
        if (!first) first = id;
      }
      if (first) selectedId = first;
      await remember(groups.flatMap((g) => g.paths));
    } catch (e) {
      addError = String(e);
    }
  }

  function statusColor(s: string): string {
    return s === 'running' ? 'var(--primary)'
      : s === 'queued' ? '#5a8fd8'
      : s === 'success' ? 'var(--green)'
      : s === 'error' ? 'var(--error)'
      : 'var(--text-muted)';
  }
</script>

<div class="restore-root">
<div class="layout">
  <!-- middle column: backups list (same as Backup) -->
  <div class="jobs-col">
    <div class="col-title">Резервные копии</div>
    <div class="this-pc">
      <svg width="11" height="11" viewBox="0 0 24 24" fill="var(--text-muted)"><path d="M6 9l6 6 6-6z"/></svg>
      Этот компьютер
    </div>
    <div class="jobs-list">
      {#if jobs.list.length === 0 && imported.length === 0}
        <div class="empty-list">Нет резервных копий.<br/>Создайте их в разделе «Резервное копирование» или нажмите «Добавить копию».</div>
      {/if}
      {#each jobs.list as job (job.id)}
        <button class="job-item" class:sel={selectedId === job.id} onclick={() => (selectedId = job.id)}>
          <div class="job-icon">
            <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.2"/></svg>
            <span class="status-dot" style="background:{statusColor(jobStatus(job))}"></span>
          </div>
          <div class="job-meta">
            <!-- The same padlock as in «Резервное копирование»: here it matters
                 more, since this is the screen that will ask for the password. -->
            <div class="job-name">
              {job.name}{#if job.archive_encryption}<span
                  class="enc-badge"
                  title="Копии этого задания зашифрованы — для чтения нужен пароль или ключ восстановления"
                  aria-label="Зашифровано">🔒</span
                >{/if}
            </div>
            <div class="job-path">{job.dest_folder}</div>
          </div>
        </button>
      {/each}
      {#each imported as it (it.id)}
        <!-- Added by hand, so it can be taken away by hand: the × removes it
             from the list and from what is remembered for next launch. -->
        <div class="job-item added" class:sel={selectedId === it.id}>
          <button class="job-pick" onclick={() => (selectedId = it.id)}>
            <div class="job-icon">
              <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.2"/></svg>
              <span class="status-dot" style="background:var(--green)"></span>
            </div>
            <div class="job-meta">
              <div class="job-name">
                {it.job.name}{#if it.sealed}<span
                    class="enc-badge"
                    title="Копия зашифрована — для чтения нужен пароль или ключ восстановления"
                    aria-label="Зашифровано">🔒</span
                  >{/if}
              </div>
              <div class="job-path" title={it.job.dest_folder}>{it.chainJobId ? `${copiesWord(it.paths.length)} · ${it.job.dest_folder}` : 'файл копии'}</div>
            </div>
          </button>
          <button class="job-forget" title="Убрать из списка (файлы не удаляются)" onclick={() => forget(it.id)}>×</button>
        </div>
      {/each}
    </div>
    {#if addError}<div class="add-err">{addError}</div>{/if}
    <button class="add-btn" onclick={() => { addError = null; addOpen = true; }}>
      <svg width="18" height="18" viewBox="0 0 24 24" stroke="var(--primary)" stroke-width="2" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>
      Добавить копию
    </button>
  </div>

  <!-- right: restore panel -->
  <div class="dash-col">
    {#if selectedJob}
      {#key selectedJob.id}
        <RestoreDashboard job={selectedJob} chainJobId={selectedImported?.chainJobId} />
      {/key}
    {:else}
      <div class="empty-dash">
        <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="var(--border)" stroke-width="1.4"><path d="M4 4v5h5M20 20v-5h-5M4 9a8 8 0 0 1 14-4M20 15a8 8 0 0 1-14 4"/></svg>
        <div>Нет резервных копий для восстановления</div>
      </div>
    {/if}
  </div>
</div>

{#if addOpen}
  <div class="overlay" role="presentation" onclick={() => (addOpen = false)}>
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <!-- Only a guard: it stops a click inside the dialog from reaching the
         overlay and closing it. Nothing happens that a key could do instead. -->
    <div class="dialog" role="dialog" tabindex="-1" onclick={(e) => e.stopPropagation()}>
      <div class="dialog-title">Добавить копию</div>
      <p>Что открыть?</p>
      <div class="choice-row">
        <button class="choice" onclick={addFile}>
          <svg width="34" height="34" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/></svg>
          <span class="choice-title">Файл</span>
          <span class="choice-sub">один образ копии</span>
        </button>
        <button class="choice" onclick={addFolder}>
          <svg width="34" height="34" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>
          <span class="choice-title">Папка с копиями</span>
          <span class="choice-sub">найти все копии в папке</span>
        </button>
      </div>
      <label class="remember">
        <input type="checkbox" bind:checked={rememberAdded} />
        <span>
          Добавить файл(ы) бэкапа в список Резервных копий
          <em>копия останется в списке после перезапуска — не придётся искать файл заново</em>
        </span>
      </label>
      <div class="dialog-actions">
        <button class="ghost" onclick={() => (addOpen = false)}>Отмена</button>
      </div>
    </div>
  </div>
{/if}
</div>

<style>
  .restore-root { display: flex; flex-direction: column; height: 100%; }
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
  .enc-badge { margin-left: 6px; font-size: 11px; opacity: 0.85; }
  .job-path { font-size: 12px; color: var(--text-muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .add-btn { display: flex; align-items: center; gap: 10px; padding: 15px 18px; background: none; border: none; border-top: 1px solid var(--border); cursor: pointer; font-size: 14px; font-weight: 600; color: var(--head); font-family: var(--font); }
  .add-btn:hover { color: var(--primary); }
  .remember { display: flex; align-items: flex-start; gap: 9px; margin: 4px 0 16px; font-size: 13px; color: var(--head); cursor: pointer; line-height: 1.45; }
  .remember input { width: auto; margin-top: 2px; flex: 0 0 auto; }
  .job-item.added { display: flex; align-items: center; padding: 0; }
  .job-pick { flex: 1; min-width: 0; display: flex; align-items: center; gap: 10px; padding: 10px 8px; background: none; border: none; cursor: pointer; text-align: left; font-family: var(--font); }
  .job-forget { flex: 0 0 auto; width: 26px; height: 26px; margin-right: 6px; border: none; background: none; border-radius: 4px; color: var(--text-muted); font-size: 17px; line-height: 1; cursor: pointer; font-family: var(--font); }
  .job-forget:hover { background: rgba(216,74,74,.12); color: #b32b2b; }
  .remember em { display: block; font-style: normal; font-size: 12px; color: var(--text-muted); margin-top: 3px; }
  .add-err { margin: 0 8px 8px; padding: 8px 12px; font-size: 12px; color: #8c1d1d; background: rgba(216,74,74,.08); border-radius: 5px; }
  .dash-col { flex: 1; min-width: 0; overflow: auto; }
  .empty-dash { height: 100%; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; color: var(--text-muted); font-size: 15px; }
  .overlay { position: fixed; inset: 0; background: rgba(20,17,44,.5); display: flex; align-items: center; justify-content: center; z-index: 150; }
  .dialog { width: 460px; background: var(--card-bg); border-radius: 8px; padding: 24px; box-shadow: 0 20px 60px rgba(0,0,0,.3); }
  .dialog-title { font-size: 17px; font-weight: 600; color: var(--head); margin-bottom: 6px; }
  .dialog p { font-size: 13px; color: var(--text-muted); margin-bottom: 16px; }
  .choice-row { display: flex; gap: 14px; }
  .choice { flex: 1; display: flex; flex-direction: column; align-items: center; gap: 6px; padding: 20px 14px; border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); cursor: pointer; font-family: var(--font); }
  .choice:hover { border-color: var(--primary); background: var(--accent-soft); }
  .choice-title { font-size: 14px; font-weight: 600; color: var(--head); }
  .choice-sub { font-size: 12px; color: var(--text-muted); }
  .dialog-actions { display: flex; justify-content: flex-end; margin-top: 18px; }
  .dialog-actions .ghost { background: none; border: 1px solid var(--border); color: var(--head); border-radius: 4px; padding: 8px 20px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; }
</style>
