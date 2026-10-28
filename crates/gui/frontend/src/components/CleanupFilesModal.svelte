<script lang="ts">
  // The files an analysis found for one cleanup item — what «Очистка» will
  // remove (the service holds the cleanup to this list). A page at a time:
  // a recycle bin holds hundreds of thousands. A file unticked here stays:
  // the cleanup is sent the list of unticked files.
  import { onMount } from 'svelte';
  import Modal from './Modal.svelte';
  import { api } from '../lib/api';
  import { formatBytes, formatDate } from '../lib/format';
  import { m } from '../lib/paraglide/messages';
  import type { CleanupFilesPage } from '../lib/types';

  interface Props {
    item: string;
    label: string;
    /** This item's unticked files and their sizes. */
    excluded: Map<string, number>;
    onToggle: (path: string, size: number) => void;
    onClose: () => void;
  }

  let { item, label, excluded, onToggle, onClose }: Props = $props();
  const excludedBytes = $derived([...excluded.values()].reduce((s, b) => s + b, 0));

  const PAGE = 500;
  let sort = $state<'size' | 'path'>('size');
  let filter = $state('');
  let page = $state<CleanupFilesPage | null>(null);
  let rows = $state<CleanupFilesPage['files']>([]);
  let loading = $state(false);
  let error = $state<string | null>(null);
  /** Answers to an older question are dropped. */
  let asked = 0;

  async function load(more: boolean) {
    const ask = ++asked;
    loading = true;
    error = null;
    try {
      const p = await api.cleanupFiles(item, more ? rows.length : 0, PAGE, sort, filter.trim());
      if (ask !== asked) return;
      page = p;
      rows = more ? [...rows, ...p.files] : p.files;
    } catch (e) {
      if (ask === asked) error = String(e);
    } finally {
      if (ask === asked) loading = false;
    }
  }

  onMount(() => {
    load(false);
  });

  let typing: ReturnType<typeof setTimeout> | undefined;
  function onFilter() {
    clearTimeout(typing);
    typing = setTimeout(() => load(false), 250);
  }

  function setSort(s: 'size' | 'path') {
    if (sort === s) return;
    sort = s;
    load(false);
  }
</script>

<Modal open xwide title={m.clf_title({ item: label })} {onClose} cancelLabel={m.common_close()}>
  <div class="head">
    <input
      type="text"
      class="form-input search"
      placeholder={m.clf_search()}
      bind:value={filter}
      oninput={onFilter}
    />
    <div class="sort" role="group" aria-label={m.clf_sort()}>
      <button class:on={sort === 'size'} onclick={() => setSort('size')}>{m.clf_sort_size()}</button>
      <button class:on={sort === 'path'} onclick={() => setSort('path')}>{m.clf_sort_path()}</button>
    </div>
  </div>

  {#if page}
    <div class="summary">
      {m.clf_summary({ count: page.total, size: formatBytes(page.bytes) })}
      {#if excluded.size > 0}
        <span class="kept-note">{m.clf_kept({ count: excluded.size, size: formatBytes(excludedBytes) })}</span>
      {/if}
      {#if page.truncated}<span class="warn">{m.clf_truncated()}</span>{/if}
    </div>
  {/if}

  {#if error}
    <div class="err">{error}</div>
  {/if}

  <div class="list">
    {#each rows as f (f.path)}
      <label class="row" class:kept={excluded.has(f.path)} title={f.path}>
        <input
          type="checkbox"
          checked={!excluded.has(f.path)}
          onchange={() => onToggle(f.path, f.size)}
          aria-label={m.clf_tick()}
        />
        <span class="path"><bdi>{f.path}</bdi></span>
        <span class="size">{formatBytes(f.size)}</span>
        <span class="date">{f.modified ? formatDate(f.modified) : ''}</span>
      </label>
    {:else}
      {#if !loading && page}<div class="empty">{m.clf_empty()}</div>{/if}
    {/each}
  </div>

  {#if page && rows.length < page.total}
    <div class="more">
      <span>{m.clf_shown({ shown: rows.length, total: page.total })}</span>
      <button class="btn-more" disabled={loading} onclick={() => load(true)}>{m.clf_more()}</button>
    </div>
  {/if}
  <p class="note">{m.clf_note()}</p>
</Modal>

<style>
  .head { display: flex; gap: 12px; align-items: center; margin-bottom: 10px; }
  .search { flex: 1; }
  .sort { display: flex; border: 1px solid var(--border-strong); border-radius: 6px; overflow: hidden; }
  .sort button { background: transparent; border: none; padding: 7px 12px; font-size: 13px; color: var(--text); cursor: pointer; font-family: var(--font); }
  .sort button.on { background: var(--accent-soft); color: var(--accent-text); font-weight: 600; }
  .summary { font-size: 13px; color: var(--text-secondary); margin-bottom: 8px; }
  .warn { color: var(--warning-text, var(--text-secondary)); margin-left: 8px; }
  .err { font-size: 13px; color: var(--error-text); margin-bottom: 8px; }
  .list { max-height: 50vh; overflow: auto; border: 1px solid var(--border); border-radius: 6px; }
  .row { display: flex; align-items: center; gap: 12px; padding: 5px 10px; cursor: pointer; font-size: 12.5px; border-bottom: 1px solid var(--border); }
  .row:last-child { border-bottom: none; }
  .row input { width: auto; margin: 0; flex: 0 0 auto; }
  .row.kept .path, .row.kept .size { color: var(--text-muted); text-decoration: line-through; }
  .kept-note { margin-left: 8px; }
  .path { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; direction: rtl; text-align: left; color: var(--text); }
  .size { width: 90px; text-align: right; white-space: nowrap; color: var(--text); }
  .date { width: 170px; text-align: right; white-space: nowrap; color: var(--text-muted); }
  .empty { padding: 14px; font-size: 13px; color: var(--text-muted); }
  .more { display: flex; align-items: center; justify-content: space-between; margin-top: 8px; font-size: 13px; color: var(--text-secondary); }
  .btn-more { background: transparent; border: 1px solid var(--primary); color: var(--accent-text); border-radius: 6px; padding: 6px 14px; font-size: 13px; font-weight: 600; cursor: pointer; font-family: var(--font); }
  .btn-more:disabled { opacity: .45; cursor: default; }
  .note { font-size: 12px; color: var(--text-muted); margin: 10px 0 0; }
</style>
