<script lang="ts">
  import { onMount } from 'svelte';
  import { pickPath } from '../lib/pickPath.svelte';
  import BootMediaCard from '../components/BootMediaCard.svelte';
  import { api } from '../lib/api';
  import ArchiveUnlockModal from '../components/ArchiveUnlockModal.svelte';
  import {
    engine,
    needsArchiveKey,
    runVerify,
    toolsState,
    refreshVolumes,
  } from '../lib/engineStore.svelte';
  import type { ActiveMount, ArchiveSecret } from '../lib/types';

  // --- Opening an encrypted copy (§7.4) ---
  // A manual mount or check never borrows the service's key: whoever is at the
  // screen types the password. The dialog is raised BEFORE the operation, so a
  // copy that needs a key is not started and then refused.
  let unlock = $state<{
    open: boolean;
    path: string;
    busy: boolean;
    error: string;
    /** Called with whatever was typed, or with nothing when the copy is plain.
     *  `accepted` is for an operation that outlives its key: it closes the
     *  dialog the moment the key has opened the copy. */
    run: ((secret?: ArchiveSecret, accepted?: () => void) => Promise<void>) | null;
  }>({ open: false, path: '', busy: false, error: '', run: null });

  const unlockClosed = { open: false, path: '', busy: false, error: '', run: null };

  /** Run `op` on `path`, asking for a key first if the copy turns out to need
   *  one. Everything the operation reports (including "wrong password") comes
   *  back through the dialog while it is open. */
  async function withArchiveKey(
    path: string,
    op: (secret?: ArchiveSecret, accepted?: () => void) => Promise<void>,
  ): Promise<void> {
    if (!(await needsArchiveKey(path))) {
      // The operation has already put its own failure on the screen; this
      // catch only stops it escaping an onclick handler.
      await op().catch(() => {});
      return;
    }
    unlock = { open: true, path, busy: false, error: '', run: op };
  }

  async function submitUnlock(secret: ArchiveSecret) {
    const run = unlock.run;
    if (!run) return;
    unlock.busy = true;
    unlock.error = '';
    // Once the key is accepted the dialog has nothing left to say. A check of
    // a whole chain runs for minutes, and a dialog that stayed up until the
    // end read as the password still being worked on (2026-09-23) — while the
    // progress bar behind it was already moving.
    const close = () => {
      if (unlock.run === run) unlock = { ...unlockClosed };
    };
    try {
      await run(secret, close);
      close();
    } catch (e) {
      // Stay open: a wrong password is a thing to retype, not a screen to lose.
      // A failure after the dialog closed is the card's to show, and it does.
      if (unlock.run === run) unlock.error = String(e);
    } finally {
      if (unlock.run === run) unlock.busy = false;
    }
  }

  let mountMsg: string | null = $state(null);
  let mountErr: string | null = $state(null);
  let mountBusy = $state(false);

  let activeMounts: ActiveMount[] = $state([]);

  let verifyMsg: string | null = $state(null);
  let verifyErr: string | null = $state(null);
  let verifyBusy = $state(false);
  const verifyProgress = $derived(engine.verifyProgress);

  let isRefreshingDisks = $state(false);
  let hasLoadedOnce = false;

  const mountedItems = $derived(
    activeMounts.map(m => {
      // Prefer the letters the service resolved on the mounted disk; fall back
      // to the disk inventory, then to the bare disk number.
      let letters: string[] = (m.volume_letters ?? []).map(l => l.toUpperCase());
      if (letters.length === 0) {
        const disk = engine.disks.find(d =>
          d.device_id.toLowerCase() === m.physical_path.toLowerCase()
        );
        if (disk) {
          for (const p of disk.partitions) {
            if (p.volume_letter) {
              letters.push(p.volume_letter.toUpperCase() + ":");
            }
          }
        }
      }
      if (letters.length === 0) {
        const mMatch = m.physical_path.match(/PhysicalDrive(\d+)/i);
        letters.push(mMatch ? `Диск ${mMatch[1]}` : 'Диск');
      }
      const active = toolsState.mountPath.toLowerCase() === m.image_path.toLowerCase();
      return {
        image_path: m.image_path,
        letters,
        active
      };
    })
  );

  const isCurrentPathMounted = $derived(
    activeMounts.some(m => m.image_path.toLowerCase() === toolsState.mountPath.toLowerCase())
  );

  function mountedLettersFor(imagePath: string): string[] {
    const item = mountedItems.find(
      m => m.image_path.toLowerCase() === imagePath.toLowerCase()
    );
    return item ? item.letters : [];
  }

  async function loadData() {
    if (!hasLoadedOnce) {
      isRefreshingDisks = true;
    }
    try {
      const status = await api.getServiceStatus();
      activeMounts = status.active_mounts || [];
      hasLoadedOnce = true;
    } catch (e) {
      console.error("Failed to load active mounts:", e);
    } finally {
      isRefreshingDisks = false;
    }
  }

  onMount(() => {
    loadData();
    const interval = setInterval(loadData, 5000);
    return () => clearInterval(interval);
  });

  async function browse(setPath: (p: string) => void, extensions = ['vhdx', 'avhdx', 'qcow2', 'obk']) {
    try {
      const picked = await pickPath({ directory: false, title: 'Выберите образ', extensions });
      if (typeof picked === 'string') setPath(picked);
    } catch {
      /* dialog cancelled */
    }
  }

  async function mount() {
    await withArchiveKey(toolsState.mountPath, mountWith);
  }

  async function mountWith(secret?: ArchiveSecret) {
    mountBusy = true;
    mountMsg = null;
    mountErr = null;
    isRefreshingDisks = true;
    try {
      await api.mountBackup(toolsState.mountPath, toolsState.rwMode, secret);
      await Promise.all([loadData(), refreshVolumes()]);
      // The disk may need a moment for Windows to assign a letter; poll briefly.
      let letters = mountedLettersFor(toolsState.mountPath);
      for (let i = 0; i < 10 && letters.every(l => l.startsWith('Диск')); i++) {
        await new Promise(r => setTimeout(r, 500));
        await loadData();
        letters = mountedLettersFor(toolsState.mountPath);
      }
      const mode = toolsState.rwMode ? 'чтение и запись' : 'только чтение';
      const where = letters.length ? letters.join(', ') : 'см. проводник';
      mountMsg = `Подключено (${mode}): ${where}. Открыть можно в проводнике.`;
    } catch (e) {
      mountErr = String(e);
      // Rethrown so a wrong password keeps the unlock dialog open with the
      // reason in it, instead of closing as though the mount had worked.
      throw e;
    } finally {
      mountBusy = false;
      isRefreshingDisks = false;
    }
  }

  async function unmount() {
    mountBusy = true;
    mountMsg = null;
    mountErr = null;
    isRefreshingDisks = true;
    try {
      await api.unmountBackup(toolsState.mountPath);
      mountMsg = 'Образ отключён.';
      await Promise.all([loadData(), refreshVolumes()]);
    } catch (e) {
      mountErr = String(e);
    } finally {
      mountBusy = false;
      isRefreshingDisks = false;
    }
  }

  async function verify() {
    await withArchiveKey(toolsState.verifyPath, verifyWith);
  }

  async function verifyWith(secret?: ArchiveSecret, accepted?: () => void) {
    verifyBusy = true;
    verifyMsg = null;
    verifyErr = null;
    try {
      const v = await runVerify(toolsState.verifyPath, secret, accepted);
      // A damaged copy is a failure, not a result: it goes in the red banner.
      // It used to share the green one with «Целостность подтверждена».
      if (v.valid) {
        verifyMsg = `Целостность подтверждена: ${v.blocks_checked} блоков за ${v.duration_secs.toFixed(1)} с.`;
      } else {
        verifyErr = `Копия повреждена: не совпали контрольные суммы у ${v.mismatched_blocks.length} из ${v.blocks_checked} блоков.`;
      }
    } catch (e) {
      verifyErr = String(e);
      throw e; // see mountWith
    } finally {
      verifyBusy = false;
    }
  }
</script>

<ArchiveUnlockModal
  open={unlock.open}
  imageName={unlock.path.split(/[\\/]/).pop() ?? unlock.path}
  busy={unlock.busy}
  error={unlock.error}
  onUnlock={submitUnlock}
  onCancel={() => (unlock = { ...unlockClosed })}
/>

<div class="page">
  <div class="inner">
  <h1>Инструменты</h1>

  <div class="tool-card">
    <div class="tool-head">
      <span class="glyph">
        <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="13" width="18" height="7" rx="2" /><path d="M5 13l2.5-8h9L19 13M7 16.5h.01" /></svg>
      </span>
      <div>
        <div class="tool-title">Подключить образ как диск</div>
        <div class="tool-desc">
          Образ подключается как диск; тома внутри получают буквы и
          открываются в проводнике.
        </div>
      </div>
    </div>
    <div class="row">
      <input
        type="text"
        bind:value={toolsState.mountPath}
        placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
        spellcheck="false"
        disabled={mountBusy}
      />
      <button class="btn outline btn-browse" onclick={() => browse((p) => (toolsState.mountPath = p), ['vhdx', 'avhdx', 'qcow2', 'obk'])} disabled={mountBusy}>
        Обзор…
      </button>
    </div>
    <div class="row row-actions">
      <div class="mounted-info">
        {#if isRefreshingDisks}
          <span class="spinner-container">
            <span class="spinner-icon">
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4.5 10A8 8 0 0 1 19 8" /><path d="M19.5 3.5V8H15" /><path d="M19.5 14A8 8 0 0 1 5 16" /><path d="M4.5 20.5V16H9" /></svg>
            </span>
            <span>Поиск примонтированных дисков...</span>
          </span>
        {:else if mountedItems.length === 0}
          <span class="muted-text">Нет примонтированных дисков</span>
        {:else}
          <span>Примонтировано: </span>
          {#each mountedItems as item, i}
            {#each item.letters as letter, j}
              <button
                class="btn btn-letter {item.active ? 'primary' : 'outline'}"
                onclick={() => { toolsState.mountPath = item.image_path; }}
                disabled={mountBusy}
              >
                {letter}
              </button>
            {/each}
          {/each}
        {/if}
      </div>
      <div class="actions-right">
        <label class="rw-checkbox">
          <input type="checkbox" bind:checked={toolsState.rwMode} disabled={mountBusy || isCurrentPathMounted} />
          RW Mode
        </label>
        <button class="btn primary" onclick={mount} disabled={mountBusy || !toolsState.mountPath || isCurrentPathMounted}>
          Подключить
        </button>
        <!-- Nothing mounted, nothing to detach: the button says so by being
             unavailable, instead of failing on the second press. -->
        <button class="btn outline" onclick={unmount} disabled={mountBusy || !isCurrentPathMounted}>
          Отключить
        </button>
      </div>
    </div>
    {#if mountMsg}<div class="banner ok">{mountMsg}</div>{/if}
    {#if mountErr}<div class="banner err">{mountErr}</div>{/if}
  </div>

  <div class="tool-card">
    <div class="tool-head">
      <span class="glyph">
        <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l8 3v6c0 4.5-3.4 8-8 9-4.6-1-8-4.5-8-9V6z" /><path d="M8.5 12l2.5 2.5 4.5-5" /></svg>
      </span>
      <div>
        <div class="tool-title">Проверить образ</div>
        <div class="tool-desc">
          Сверяет контрольные суммы каждого блока с метаданными в образе (или файлом .obkm).
        </div>
      </div>
    </div>
    <div class="row">
      <input
        type="text"
        bind:value={toolsState.verifyPath}
        placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
        spellcheck="false"
        disabled={verifyBusy}
      />
      <button class="btn outline" onclick={() => browse((p) => (toolsState.verifyPath = p))} disabled={verifyBusy}>
        Обзор…
      </button>
      <button class="btn primary" onclick={verify} disabled={verifyBusy || !toolsState.verifyPath}>
        {verifyBusy ? 'Проверка…' : 'Проверить'}
      </button>
    </div>
    {#if verifyBusy && verifyProgress}
      <div class="bar"><div class="bar-fill" style="width:{verifyProgress.percent}%"></div></div>
    {/if}
    {#if verifyMsg}<div class="banner ok">{verifyMsg}</div>{/if}
    {#if verifyErr}<div class="banner err">{verifyErr}</div>{/if}
  </div>

  <BootMediaCard />
</div>
</div>

<style>
  /* The scrollbar belongs to the window's edge, not to the column of content:
     with max-width on the scrolling element it appeared 860 px in, floating in
     the middle of the page. */
  .page {
    overflow: auto;
    height: 100%;
  }

  .inner {
    padding: 30px 44px;
  }

  h1 {
    font-size: 24px;
    font-weight: 300;
    color: var(--head);
    margin-bottom: 24px;
  }

  .tool-card {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--card-bg);
    padding: 20px 22px;
    margin-bottom: 16px;
  }


  .tool-head {
    display: flex;
    gap: 14px;
    align-items: flex-start;
    margin-bottom: 14px;
  }

  /* The tool's icon on a tinted square, as in the design. */
  .glyph {
    flex: none;
    width: 40px;
    height: 40px;
    border-radius: 8px;
    background: var(--accent-soft);
    color: var(--accent-text);
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .tool-title {
    font-size: 16px;
    font-weight: 600;
    color: var(--head);
  }

  .tool-desc {
    font-size: 12px;
    color: var(--text-secondary);
    margin-top: 3px;
    line-height: 1.5;
  }

  .row {
    display: flex;
    gap: 10px;
  }

  .row-actions {
    justify-content: space-between;
    align-items: center;
    margin-top: 12px;
  }

  .btn-browse {
    min-width: 95px;
  }

  .mounted-info {
    font-size: 13px;
    color: var(--text-secondary);
  }

  .muted-text {
    color: var(--text-muted);
  }

  .btn-letter {
    min-width: 36px !important;
    height: 36px !important;
    padding: 0 8px !important;
    display: inline-flex !important;
    align-items: center !important;
    justify-content: center !important;
    font-size: 13px !important;
    margin-right: 6px;
    border-radius: 6px;
    font-weight: 600;
    box-sizing: border-box !important;
  }

  @keyframes spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
  }

  .spinner-container {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--text-muted);
  }

  .spinner-icon {
    display: inline-block;
    animation: spin 1s linear infinite;
  }

  /* Block, so it turns about its own centre rather than the text line's. */
  .spinner-icon svg {
    display: block;
  }

  .actions-right {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .rw-checkbox {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    user-select: none;
    cursor: pointer;
    color: var(--text-secondary);
  }

  .row input {
    flex: 1;
    padding: 9px 12px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    font-size: 13px;
    font-family: var(--font);
    background: var(--input-bg);
    color: var(--text);
  }

  .row input::placeholder {
    color: var(--text-muted);
  }

  .row input:focus {
    outline: none;
    border-color: var(--primary);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }

  .row input:disabled {
    background: var(--sunken);
    border-color: var(--border);
    color: var(--text-muted);
  }

  .btn {
    border-radius: 6px;
    padding: 9px 20px;
    font-size: 13px;
    font-weight: 600;
    font-family: var(--font);
    cursor: pointer;
    border: 1px solid transparent;
    white-space: nowrap;
  }

  .btn:disabled {
    opacity: 0.45;
    cursor: default;
  }

  .btn.primary {
    background: var(--primary);
    color: #fff;
  }

  .btn.primary:hover:not(:disabled) {
    background: var(--primary-hover);
  }

  /* «Обзор…», «Отключить», a letter that is not the chosen image: the
     design's secondary button. */
  .btn.outline {
    background: var(--card-bg);
    border-color: var(--border-strong);
    color: var(--text);
  }

  .btn.outline:hover:not(:disabled) {
    background: var(--panel2);
  }

  .bar {
    height: 8px;
    border-radius: 4px;
    background: var(--track);
    overflow: hidden;
    margin-top: 12px;
  }

  .bar-fill {
    height: 100%;
    border-radius: 4px;
    background: var(--success);
    transition: width 0.15s linear;
  }

  .banner {
    margin-top: 12px;
    border-radius: 8px;
    padding: 11px 14px;
    font-size: 13px;
    word-break: break-all;
  }

  .banner.ok {
    background: var(--success-soft);
    border: 1px solid var(--success-line);
    color: var(--success-text);
  }

  .banner.err {
    background: var(--error-soft);
    border: 1px solid var(--error-line);
    color: var(--error-text);
  }
</style>
