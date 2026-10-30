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
  import { m } from '../lib/paraglide/messages';
  import { joinPath } from '../lib/paths';

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

  // Files shown while a FOLDER is being picked are there to say "the copies
  // are in here" — the folder is the answer, not one of them.
  const pickable = (e: DirEntry) => e.is_dir || !req?.directory;

  async function openVolume(name: string) {
    loading = true;
    error = null;
    try {
      await go(await api.mediumMountVolume(name));
    } catch (e) {
      error = String(e);
      loading = false;
    }
  }

  async function unmountVolume(name: string) {
    error = null;
    try {
      await api.mediumUnmountVolume(name);
      lastPath = '';
    } catch (e) {
      error = String(e);
    }
    const kept = error;
    await roots();
    error = kept;
  }

  function onRow(e: DirEntry) {
    if (e.volume) openVolume(e.volume);
    else if (e.is_dir) go(e.path);
    else if (!req?.directory) selectedFile = selectedFile === e.path ? null : e.path;
  }

  async function makeFolder() {
    const name = newFolder.trim();
    if (!name || !path) return;
    creating = true;
    error = null;
    try {
      await api.mediumMakeWritable(path);
      const made = await api.createDir(joinPath(path, name));
      newFolder = '';
      await go(made);
    } catch (e) {
      error = String(e);
    } finally {
      creating = false;
    }
  }

  async function accept() {
    if (req?.writable) {
      try {
        await api.mediumMakeWritable(path);
      } catch (e) {
        error = String(e);
        return;
      }
    }
    finishPick(req?.directory ? path : selectedFile);
  }
</script>

{#if picker.open}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="backdrop" onclick={() => finishPick(null)}>
    <div class="dialog" onclick={(e) => e.stopPropagation()}>
      <div class="head">{req?.title ?? m.pick_default_title()}</div>

      <div class="bar">
        <button class="nav" onclick={roots} title={m.pick_drives_title()}>{m.pick_drives()}</button>
        <button class="nav" disabled={!parent} onclick={() => parent && go(parent)} title={m.pick_up_title()}>
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5M6 11l6-6 6 6" /></svg>
        </button>
        <input
          class="path"
          type="text"
          value={path}
          placeholder={m.pick_path_placeholder()}
          spellcheck="false"
          onkeydown={(e) => e.key === 'Enter' && go((e.currentTarget as HTMLInputElement).value)}
        />
      </div>

      {#if error}
        <div class="err">{error}</div>
      {/if}

      <div class="list">
        {#if loading}
          <div class="muted">{m.pick_reading()}</div>
        {:else if entries.length === 0}
          <div class="muted">{path === '' ? m.pick_no_drives() : m.pick_empty()}</div>
        {/if}
        {#each entries as e (e.volume || e.path)}
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
          <div
            class="row"
            class:sel={selectedFile === e.path}
            class:drive={!!(e.label || e.fs)}
            class:shown-only={!pickable(e)}
            onclick={() => onRow(e)}
          >
            <span class="ico">
              {#if e.is_dir}
                <svg width="16" height="16" viewBox="0 0 24 24"><path d="M3 7a2 2 0 0 1 2-2h4.2l2 2H19a2 2 0 0 1 2 2v8.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" fill="var(--folder)" /><path d="M3 10h18v7.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" fill="var(--folder2)" /></svg>
              {:else}
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.6" stroke-linejoin="round"><path d="M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z" /><path d="M14 3v5h5" /></svg>
              {/if}
            </span>
            <span class="nm">{e.name}</span>
            <!-- Drives carry a label; a bare "C:, D:, E:" tells the user nothing
                 about which of their disks is which. Without a label the
                 filesystem at least says what kind of volume it is. -->
            {#if e.label}
              <span class="tag">{e.label}</span>
            {:else if e.fs}
              <span class="tag muted-tag">{e.fs}</span>
            {/if}
            {#if (!e.is_dir || e.volume) && e.size != null}<span class="sz">{formatBytes(e.size)}</span>{/if}
            {#if e.volume && e.mounted}
              <button
                class="nav"
                title={m.pick_unmount_title()}
                onclick={(ev) => { ev.stopPropagation(); unmountVolume(e.volume); }}
              >{m.pick_unmount()}</button>
            {/if}
          </div>
        {/each}
      </div>

      {#if req?.directory && path}
        <div class="mk">
          <input
            type="text"
            bind:value={newFolder}
            placeholder={m.pick_new_folder_placeholder()}
            spellcheck="false"
            onkeydown={(e) => e.key === 'Enter' && makeFolder()}
          />
          <button class="nav" disabled={!newFolder.trim() || creating} onclick={makeFolder}>{m.pick_create_folder()}</button>
        </div>
      {/if}

      <div class="foot">
        <span class="chosen" title={selectedFile ?? path}>{selectedFile ?? path}</span>
        <button class="btn" onclick={() => finishPick(null)}>{m.common_cancel()}</button>
        <button class="btn primary" disabled={!canAccept} onclick={accept}>{m.pick_select()}</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .backdrop { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 200; }
  .dialog { width: 620px; max-width: 92vw; background: var(--card-bg); border: 1px solid var(--border); border-radius: 12px; box-shadow: var(--shadow-lg); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; font-weight: 600; color: var(--head); border-bottom: 1px solid var(--border); }
  .bar { display: flex; gap: 8px; padding: 12px 18px; align-items: center; }
  .path { flex: 1; min-width: 0; background: var(--input-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 6px 10px; font-size: 13px; color: var(--text); font-family: var(--font); }
  .path::placeholder, .mk input::placeholder { color: var(--text-muted); }
  .path:focus, .mk input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
  /* «Диски», «На уровень выше», «Создать папку»: the design's secondary button. */
  .nav { background: var(--card-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 6px 12px; font-size: 13px; font-weight: 600; color: var(--text); cursor: pointer; font-family: var(--font); }
  /* Inline, so the arrow keeps the text line's height and the buttons stay level. */
  .nav svg { vertical-align: middle; }
  .nav:hover:not(:disabled) { background: var(--panel2); }
  .nav:disabled { opacity: .45; cursor: default; }
  /* The line under the list belongs to whatever follows it — the new-folder
     row or the footer — so the two never stack into a double line. */
  .list { height: 320px; overflow: auto; border-top: 1px solid var(--border); }
  /* Listed for recognition, not for choosing. */
  .shown-only {
    opacity: .62;
    cursor: default;
  }

  .row { display: flex; align-items: center; gap: 10px; padding: 7px 18px; cursor: pointer; font-size: 13px; color: var(--text); }
  .row:hover { background: var(--panel2); }
  /* Chosen the way the design marks a chosen item: tinted, with an accent edge. */
  .row.sel { background: var(--accent-soft); box-shadow: inset 0 0 0 1px var(--primary); }
  .ico { flex-shrink: 0; }
  .ico svg { display: block; }
  .nm { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sz { color: var(--text-muted); font-size: 12px; }
  .tag { color: var(--text-secondary); font-size: 12px; }
  /* On a drive row the label belongs beside the letter - "C: Windows" -
     not pushed to the far edge where it reads as a separate column. */
  .row.drive .nm { flex: 0 0 auto; }
  .row.drive .tag { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .tag.muted-tag { color: var(--text-muted); }
  .mk { display: flex; gap: 8px; padding: 10px 18px; border-top: 1px solid var(--border); }
  .mk input { flex: 1; min-width: 0; background: var(--input-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 6px 10px; font-size: 13px; color: var(--text); font-family: var(--font); }
  /* The dialog's footer: the design's --panel strip, rounded to sit inside
     the 12 px frame. */
  .foot { display: flex; gap: 10px; align-items: center; padding: 12px 18px; border-top: 1px solid var(--border); background: var(--panel2); border-radius: 0 0 11px 11px; }
  .chosen { flex: 1; min-width: 0; font-size: 12px; color: var(--text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .btn { border: 1px solid var(--border-strong); background: var(--card-bg); border-radius: 6px; padding: 7px 18px; font-size: 13px; font-weight: 600; color: var(--text); cursor: pointer; font-family: var(--font); }
  .btn:hover:not(:disabled) { background: var(--panel2); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn.primary:hover:not(:disabled) { background: var(--primary-hover); border-color: var(--primary-hover); }
  .btn:disabled { opacity: .45; cursor: default; }
  .err { padding: 8px 18px; font-size: 12px; color: var(--error-text); word-break: break-all; }
  .muted { padding: 14px 18px; font-size: 13px; color: var(--text-muted); }
</style>
