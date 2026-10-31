<script lang="ts">
  // One line of a list of jobs: the same in «Резервное копирование» and
  // «Восстановление». The chevron and a right click ask for the job's menu.
  import type { Snippet } from 'svelte';
  import { jobStatus } from '../lib/jobStore.svelte';
  import type { Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    job: Job;
    selected: boolean;
    /** This job's menu is open. */
    menuOpen: boolean;
    /** The line under the name. */
    path: string;
    /** The padlock's explanation; no padlock when absent. */
    encryptedTitle?: string;
    onselect: () => void;
    onmenu: (e: MouseEvent | KeyboardEvent, anchor?: HTMLElement) => void;
    /** Replaces the name, the path and the chevron while the name is edited. */
    editor?: Snippet;
  }
  let { job, selected, menuOpen, path, encryptedTitle, onselect, onmenu, editor }: Props = $props();

  function statusColor(s: string): string {
    return s === 'running' ? 'var(--primary)'
      : s === 'queued' ? 'var(--info)'
      : s === 'success' ? 'var(--success)'
      : s === 'error' ? 'var(--error)'
      : 'var(--text-muted)';
  }
</script>

<button class="job-item" class:sel={selected} class:menu-open={menuOpen} onclick={onselect} oncontextmenu={(e) => onmenu(e)}>
  <div class="job-icon">
    <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.6"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
    <span class="status-dot" style="background:{statusColor(jobStatus(job))}"></span>
  </div>
  {#if editor}
    {@render editor()}
  {:else}
    <div class="job-meta">
      <div class="job-name">
        {job.name}{#if encryptedTitle}<span
            class="enc-badge"
            title={encryptedTitle}
            aria-label={m.bs_encrypted()}><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="2.2" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg></span
          >{/if}
      </div>
      <div class="job-path" title={path}>{path}</div>
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
      onclick={(e) => onmenu(e, e.currentTarget)}
      onkeydown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') onmenu(e, e.currentTarget);
      }}><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg></span>
  {/if}
</button>

<style>
  .job-item { width: 100%; display: flex; align-items: center; gap: 10px; padding: 10px 8px; border: 1px solid transparent; border-radius: 8px; background: none; cursor: pointer; margin-bottom: 4px; text-align: left; font-family: var(--font); }
  .job-item:hover { background: var(--panel2); }
  .job-item.sel { background: var(--accent-soft); border-color: var(--primary); }
  .job-icon { position: relative; flex-shrink: 0; }
  .status-dot { position: absolute; right: -2px; bottom: 0; width: 11px; height: 11px; border-radius: 50%; border: 2px solid var(--column-bg); }
  .job-meta { flex: 1; min-width: 0; }
  .job-name { font-size: 14px; font-weight: 600; color: var(--text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .enc-badge { margin-left: 6px; font-size: 11px; }
  .enc-badge svg { vertical-align: -1px; }
  .job-path { font-size: 12px; color: var(--text-muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  /* Shown for the row under the pointer, the chosen one, and the one whose
     menu is open or which the keyboard has reached. */
  .more { opacity: 0; color: var(--text-muted); padding: 4px; display: flex; border-radius: 4px; }
  .more:hover, .job-item.menu-open .more { color: var(--head); background: var(--track); }
  .job-item:hover .more, .job-item.sel .more, .job-item.menu-open .more, .more:focus-visible { opacity: 1; }
</style>
