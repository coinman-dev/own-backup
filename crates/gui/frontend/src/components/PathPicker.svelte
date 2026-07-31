<script lang="ts">
  // The interface's own "choose a folder / choose a file" window, used where the
  // native shell dialog cannot be: on the boot medium WinPE has no shell, so the
  // system dialog never appears and the button reads as broken.
  //
  // Mounted once, in App.svelte; it renders only while `picker.open`.
  import { picker, finishPick } from '../lib/pickPath.svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import type { DirEntry } from '../lib/types';

  let path = $state('');
  let parent: string | null = $state(null);
  let entries: DirEntry[] = $state([]);
  let error: string | null = $state(null);
  let loading = $state(false);
  let selectedFile: string | null = $state(null);
  let newFolder = $state('');
  let creating = $state(false);

  const req = $derived(picker.request);
  // A folder pick returns wherever we are standing; a file pick needs a file.
  const canAccept = $derived(req?.directory ? path !== '' : selectedFile !== null);

  // Remembered between openings: picking a destination twice in a row almost
  // always means the same disk, and re-walking from the drive list is tedious.
  let lastPath = '';

  $effect(() => {
    if (picker.open) start();
  });

  async function start() {
    selectedFile = null;
    error = null;
    const from = picker.request?.startPath || lastPath;
    if (from) {
      await go(from);
      if (!error) return;
      error = null; // the remembered path is gone (a disk that left) — show roots
    }
    await roots();
  }

  async function roots() {
    loading = true;
    try {
      entries = await api.listRoots();
      path = '';
      parent = null;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function go(to: string) {
    loading = true;
    error = null;
    try {
      const l = await api.listDir(to, req?.directory ?? true, req?.extensions ?? []);
      entries = l.entries;
      path = l.path;
      parent = l.parent;
      selectedFile = null;
      lastPath = l.path;
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function onRow(e: DirEntry) {
    if (e.is_dir) go(e.path);
    else selectedFile = selectedFile === e.path ? null : e.path;
  }

  async function makeFolder() {
    const name = newFolder.trim();
    if (!name || !path) return;
    creating = true;
    error = null;
    try {
      const sep = path.endsWith('\\') || path.endsWith('/') ? '' : '\\';
      const made = await api.createDir(`${path}${sep}${name}`);
      newFolder = '';
      await go(made);
    } catch (e) {
      error = String(e);
    } finally {
      creating = false;
    }
  }

  function accept() {
    finishPick(req?.directory ? path : selectedFile);
  }
</script>

{#if picker.open}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="backdrop" onclick={() => finishPick(null)}>
    <div class="dialog" onclick={(e) => e.stopPropagation()}>
      <div class="head">{req?.title ?? 'Выбор пути'}</div>

      <div class="bar">
        <button class="nav" onclick={roots} title="Список дисков">Диски</button>
        <button class="nav" disabled={!parent} onclick={() => parent && go(parent)} title="На уровень выше">↑</button>
        <input
          class="path"
          type="text"
          value={path}
          placeholder="Выберите диск"
          spellcheck="false"
          onkeydown={(e) => e.key === 'Enter' && go((e.currentTarget as HTMLInputElement).value)}
        />
      </div>

      {#if error}
        <div class="err">{error}</div>
      {/if}

      <div class="list">
        {#if loading}
          <div class="muted">Чтение…</div>
        {:else if entries.length === 0}
          <div class="muted">{path === '' ? 'Диски не найдены.' : 'Здесь пусто.'}</div>
        {/if}
        {#each entries as e (e.path)}
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
          <div
            class="row"
            class:sel={selectedFile === e.path}
            class:drive={!!(e.label || e.fs)}
            onclick={() => onRow(e)}
          >
            <span class="ico">{e.is_dir ? '📁' : '📄'}</span>
            <span class="nm">{e.name}</span>
            <!-- Drives carry a label; a bare "C:, D:, E:" tells the user nothing
                 about which of their disks is which. Without a label the
                 filesystem at least says what kind of volume it is. -->
            {#if e.label}
              <span class="tag">{e.label}</span>
            {:else if e.fs}
              <span class="tag muted-tag">{e.fs}</span>
            {/if}
            {#if !e.is_dir && e.size != null}<span class="sz">{formatBytes(e.size)}</span>{/if}
          </div>
        {/each}
      </div>

      {#if req?.directory && path}
        <div class="mk">
          <input
            type="text"
            bind:value={newFolder}
            placeholder="Имя новой папки"
            spellcheck="false"
            onkeydown={(e) => e.key === 'Enter' && makeFolder()}
          />
          <button class="nav" disabled={!newFolder.trim() || creating} onclick={makeFolder}>Создать папку</button>
        </div>
      {/if}

      <div class="foot">
        <span class="chosen" title={selectedFile ?? path}>{selectedFile ?? path}</span>
        <button class="btn" onclick={() => finishPick(null)}>Отмена</button>
        <button class="btn primary" disabled={!canAccept} onclick={accept}>Выбрать</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,.45); display: flex; align-items: center; justify-content: center; z-index: 200; }
  .dialog { width: 620px; max-width: 92vw; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; color: var(--head); border-bottom: 1px solid var(--border); }
  .bar { display: flex; gap: 8px; padding: 12px 18px; align-items: center; }
  .path { flex: 1; min-width: 0; background: var(--input-bg, transparent); border: 1px solid var(--border); border-radius: 4px; padding: 6px 10px; font-size: 13px; color: var(--head); font-family: var(--font); }
  .nav { background: none; border: 1px solid var(--border); border-radius: 4px; padding: 6px 12px; font-size: 13px; color: var(--head); cursor: pointer; font-family: var(--font); }
  .nav:disabled { opacity: .45; cursor: default; }
  .list { height: 320px; overflow: auto; border-top: 1px solid var(--border); border-bottom: 1px solid var(--border); }
  .row { display: flex; align-items: center; gap: 10px; padding: 7px 18px; cursor: pointer; font-size: 13px; color: var(--head); }
  .row:hover { background: var(--hover-bg, rgba(127,127,127,.12)); }
  .row.sel { background: var(--primary); color: #fff; }
  .ico { flex-shrink: 0; }
  .nm { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sz { color: var(--text-muted); font-size: 12px; }
  .tag { color: var(--head); font-size: 12px; opacity: .85; }
  /* On a drive row the label belongs beside the letter - "C: Windows" -
     not pushed to the far edge where it reads as a separate column. */
  .row.drive .nm { flex: 0 0 auto; }
  .row.drive .tag { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .tag.muted-tag { color: var(--text-muted); opacity: 1; }
  .row.sel .tag { color: #fff; }
  .mk { display: flex; gap: 8px; padding: 10px 18px; }
  .mk input { flex: 1; min-width: 0; background: none; border: 1px solid var(--border); border-radius: 4px; padding: 6px 10px; font-size: 13px; color: var(--head); font-family: var(--font); }
  .foot { display: flex; gap: 10px; align-items: center; padding: 12px 18px; }
  .chosen { flex: 1; min-width: 0; font-size: 12px; color: var(--text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .btn { border: 1px solid var(--border); background: none; border-radius: 4px; padding: 7px 18px; font-size: 13px; color: var(--head); cursor: pointer; font-family: var(--font); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn:disabled { opacity: .5; cursor: default; }
  .err { padding: 8px 18px; font-size: 12px; color: var(--error); word-break: break-all; }
  .muted { padding: 14px 18px; font-size: 13px; color: var(--text-muted); }
</style>
