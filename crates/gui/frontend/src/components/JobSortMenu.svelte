<script lang="ts" module>
  export type JobSortKey = 'name' | 'time' | 'size';
</script>

<script lang="ts">
  // The order of a list of jobs, beside «Этот компьютер»: the same button in
  // «Резервное копирование» and «Восстановление». Each keeps its own choice.
  import { m } from '../lib/paraglide/messages';

  let { value = $bindable() }: { value: JobSortKey } = $props();
  let open = $state(false);

  const LABELS: Record<JobSortKey, () => string> = {
    name: m.bs_sort_name,
    time: m.bs_sort_time,
    size: m.bs_sort_size,
  };
  const KEYS: JobSortKey[] = ['name', 'time', 'size'];
</script>

<div class="sort">
  <button class="sort-btn" title={m.bs_sort_title({ order: LABELS[value]() })} aria-label={m.bs_sort()} onclick={() => (open = !open)}>
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"><path d="M4 7h16M7 12h10M10 17h4"/></svg>
  </button>
  {#if open}
    <button class="sort-backdrop" aria-label={m.common_close()} onclick={() => (open = false)}></button>
    <div class="sort-menu">
      {#each KEYS as key (key)}
        <button class:active={value === key} onclick={() => { value = key; open = false; }}>{LABELS[key]()}</button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .sort { margin-left: auto; position: relative; display: flex; }
  .sort-btn { background: none; border: none; padding: 3px 5px; cursor: pointer; color: var(--text-muted); display: flex; border-radius: 4px; }
  .sort-btn:hover { color: var(--head); background: var(--track); }
  .sort-backdrop { position: fixed; inset: 0; z-index: 40; background: none; border: none; cursor: default; }
  .sort-menu { position: absolute; top: 100%; right: 0; margin-top: 4px; z-index: 50; min-width: 150px; padding: 4px; display: flex; flex-direction: column; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow-lg); }
  .sort-menu button { text-align: left; background: none; border: none; padding: 8px 12px; font-size: 13px; font-weight: 600; color: var(--text); cursor: pointer; border-radius: 4px; font-family: var(--font); }
  .sort-menu button:hover { background: var(--panel2); }
  .sort-menu button.active { color: var(--accent-text); }
</style>
