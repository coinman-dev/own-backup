<script lang="ts">
  // «Очистка» (PLAN §6б): what can go, how much of it there is, removing it.
  // The service does the work; this asks it and shows the numbers. Removal is
  // plain and cannot be undone, so it is asked for twice: the button, then
  // the question in the same bar.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import { m } from '../lib/paraglide/messages';
  import CleanupFilesModal from '../components/CleanupFilesModal.svelte';
  import type {
    CleanupCatalog,
    CleanupItem,
    CleanupProgress,
    CleanupReport,
    CleanupTally,
  } from '../lib/types';

  const ITEMS_KEY = 'ob.cleanup.items';
  const USERS_KEY = 'ob.cleanup.users';
  const HISTORY_KEY = 'ob.cleanup.historyDays';
  /** «Хранить историю»: 0 removes it all; a week unless chosen otherwise. */
  const KEEP_CHOICES = [0, 1, 7, 30] as const;

  let catalog = $state<CleanupCatalog | null>(null);
  let loadError = $state<string | null>(null);
  let items = $state<Set<string>>(new Set());
  let users = $state<Set<string>>(new Set());
  /** Per item: what the last analysis found, or what the last cleanup removed. */
  let tallies = $state<Record<string, CleanupTally & { running?: string[] }>>({});
  /** What the numbers on screen are. */
  let shown = $state<'none' | 'analyze' | 'run'>('none');
  let busy = $state<'analyze' | 'run' | null>(null);
  let confirming = $state(false);
  let report = $state<CleanupReport | null>(null);
  let error = $state<string | null>(null);
  let focused = $state<string | null>(null);
  /** The item whose files are listed in the window over the page. */
  let viewing = $state<{ id: string; label: string } | null>(null);
  /** The ticks changed after an analysis: its numbers and its list no longer
   *  say what the cleanup would remove. */
  let stale = $state(false);
  let keepDays = $state(
    ((): number => {
      const v = Number(localStorage.getItem(HISTORY_KEY));
      return (KEEP_CHOICES as readonly number[]).includes(v) && localStorage.getItem(HISTORY_KEY) !== null ? v : 7;
    })(),
  );

  function keepLabel(days: number): string {
    return days === 0
      ? m.cl_keep_none()
      : days === 1
        ? m.cl_keep_day()
        : days === 7
          ? m.cl_keep_week()
          : m.cl_keep_month();
  }

  function setKeep(days: number) {
    keepDays = days;
    localStorage.setItem(HISTORY_KEY, String(days));
    changed();
  }

  function remembered(key: string): string[] | null {
    try {
      const v = JSON.parse(localStorage.getItem(key) ?? 'null');
      return Array.isArray(v) ? v.filter((x) => typeof x === 'string') : null;
    } catch {
      return null;
    }
  }

  function remember() {
    localStorage.setItem(ITEMS_KEY, JSON.stringify([...items]));
    localStorage.setItem(USERS_KEY, JSON.stringify([...users]));
  }

  onMount(() => {
    api
      .cleanupCatalog()
      .then((c) => {
        catalog = c;
        const all = c.groups.flatMap((g) => g.items);
        const known = new Set(all.map((i) => i.id));
        const keptItems = remembered(ITEMS_KEY)?.filter((id) => known.has(id));
        items = new Set(keptItems ?? all.filter((i) => i.default_on).map((i) => i.id));
        const allowed = c.users.filter((u) => u.allowed);
        const keptUsers = remembered(USERS_KEY)?.filter((sid) => allowed.some((u) => u.sid === sid));
        const me = allowed.filter((u) => u.me).map((u) => u.sid);
        users = new Set(keptUsers?.length ? keptUsers : me.length ? me : allowed.map((u) => u.sid));
        focused = all[0]?.id ?? null;
      })
      .catch((e) => (loadError = String(e)));
    const un = listen<CleanupProgress>('cleanup-progress', (e) => {
      const p = e.payload;
      if (busy && p.mode === busy) {
        tallies = { ...tallies, [p.item]: p };
      }
    });
    return () => {
      un.then((f) => f());
    };
  });

  function toggleItem(id: string) {
    const next = new Set(items);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    items = next;
    ticked();
  }

  function toggleGroup(groupItems: CleanupItem[]) {
    const next = new Set(items);
    const all = groupItems.every((i) => next.has(i.id));
    for (const i of groupItems) {
      if (all) next.delete(i.id);
      else next.add(i.id);
    }
    items = next;
    ticked();
  }

  function toggleUser(sid: string) {
    const next = new Set(users);
    if (next.has(sid)) next.delete(sid);
    else next.add(sid);
    users = next;
    changed();
  }

  /** Ticks do not touch an analysis: it measured every item, and the cleanup
   *  takes the ticked ones out of it. */
  function ticked() {
    confirming = false;
    remember();
  }

  /** After a change of users or of the days of history kept an analysis says
   *  nothing about the new choice: its numbers go, and the cleanup is no
   *  longer held to its list. */
  function changed() {
    confirming = false;
    remember();
    if (shown === 'analyze') {
      tallies = {};
      report = null;
      shown = 'none';
      stale = true;
    }
  }

  /** The ticked items, in the catalogue's order. */
  function chosen(): string[] {
    return (catalog?.groups ?? []).flatMap((g) => g.items).filter((i) => items.has(i.id)).map((i) => i.id);
  }

  /** Every item of the catalogue, in its order. */
  function every(): string[] {
    return (catalog?.groups ?? []).flatMap((g) => g.items).map((i) => i.id);
  }

  /** An analysis measures every item, so an unticked one shows its size too;
   *  a cleanup takes the ticked ones. */
  async function go(mode: 'analyze' | 'run') {
    const list = mode === 'analyze' ? every() : chosen();
    if (list.length === 0) return;
    confirming = false;
    error = null;
    report = null;
    busy = mode;
    shown = mode;
    stale = false;
    tallies = {};
    try {
      const r =
        mode === 'analyze'
          ? await api.cleanupAnalyze(list, [...users], { history_keep_days: keepDays })
          : await api.cleanupRun(list, [...users], { history_keep_days: keepDays });
      report = r;
      tallies = Object.fromEntries(r.items.map((i) => [i.id, i]));
    } catch (e) {
      error = String(e);
    } finally {
      busy = null;
    }
  }

  function cancel() {
    api.cleanupCancel().catch(() => {});
  }

  function sizeText(id: string): string {
    const t = tallies[id];
    if (!t) {
      if (busy === 'analyze' || (busy && items.has(id))) return '…';
      return m.cl_not_analyzed();
    }
    const records = t.records ?? 0;
    const entries = t.entries ?? 0;
    if (t.files === 0 && t.left_files === 0 && !t.running?.length && records === 0 && entries === 0)
      return m.cl_empty();
    const parts: string[] = [];
    if (t.files > 0 || (records === 0 && entries === 0)) parts.push(formatBytes(t.bytes));
    if (records > 0) parts.push(m.cl_records({ count: records }));
    if (entries > 0) parts.push(m.cl_entries({ count: entries }));
    // A history database or a registry left has no size of its own.
    if (t.left_files > 0)
      parts.push(
        t.left_bytes > 0
          ? m.cl_left({ size: formatBytes(t.left_bytes) })
          : m.cl_left_count({ count: t.left_files }),
      );
    if (t.running?.length) parts.push(m.cl_close({ names: t.running.join(', ') }));
    return parts.join(' · ');
  }

  /** Whether the analysis listed anything of the item to look through. */
  function listed(id: string): boolean {
    const t = tallies[id];
    return !!t && (t.files > 0 || (t.entries ?? 0) > 0);
  }

  function groupBytes(groupItems: CleanupItem[]): number | null {
    const known = groupItems.filter((i) => items.has(i.id) && tallies[i.id]);
    if (known.length === 0) return null;
    return known.reduce((s, i) => s + tallies[i.id].bytes, 0);
  }

  const total = $derived(
    chosen().reduce((s, id) => s + (tallies[id]?.bytes ?? 0), 0),
  );
  const focusedItem = $derived(
    (catalog?.groups ?? []).flatMap((g) => g.items).find((i) => i.id === focused) ?? null,
  );

  function hint(): string {
    if (busy === 'analyze') return `${m.cl_hint_analyzing()} ${formatBytes(total)}`;
    if (busy === 'run') return `${m.cl_hint_cleaning()} ${formatBytes(total)}`;
    if (confirming) return m.cl_confirm();
    if (chosen().length === 0) return m.cl_hint_nothing();
    if (report && shown === 'run') {
      const size = formatBytes(report.total.bytes);
      if (report.cancelled) return m.cl_hint_cancelled({ size });
      if (report.total.left_files > 0) {
        return m.cl_hint_done_left({
          size,
          count: report.total.left_files,
          left: formatBytes(report.total.left_bytes),
        });
      }
      return m.cl_hint_done({ size });
    }
    if (shown === 'analyze' && report) return m.cl_hint_found({ size: formatBytes(total) });
    if (stale) return m.cl_stale();
    return m.cl_hint_start();
  }
</script>

<div class="page">
  <div class="scroll">
    <h1>{m.cl_title()}</h1>
    <p class="lead">{m.cl_lead()}</p>

    {#if loadError}
      <div class="banner err">{loadError}</div>
    {:else if catalog}
      <div class="cols">
        <div class="card tree">
          {#each catalog.groups as g (g.id)}
            {@const bytes = groupBytes(g.items)}
            <div class="group">
              <label class="row head">
                <input
                  type="checkbox"
                  checked={g.items.every((i) => items.has(i.id))}
                  disabled={!!busy}
                  onchange={() => toggleGroup(g.items)}
                />
                <span class="name">{g.label}</span>
                <span class="size">{bytes === null ? '' : formatBytes(bytes)}</span>
              </label>
              {#each g.items as it (it.id)}
                <div
                  class="row item"
                  class:focused={focused === it.id}
                  role="button"
                  tabindex="0"
                  onclick={() => (focused = it.id)}
                  onkeydown={(e) => e.key === 'Enter' && (focused = it.id)}
                >
                  <input
                    type="checkbox"
                    checked={items.has(it.id)}
                    disabled={!!busy}
                    onchange={() => toggleItem(it.id)}
                    onclick={(e) => e.stopPropagation()}
                    aria-label={it.label}
                  />
                  <span class="name">{it.label}</span>
                  <span
                    class="size"
                    class:muted={!tallies[it.id] || !items.has(it.id)}
                    title={tallies[it.id] ? m.cl_files({ count: tallies[it.id].files }) : ''}
                    >{sizeText(it.id)}</span
                  >
                  {#if shown === 'analyze' && !busy && listed(it.id)}
                    <button
                      class="files-btn"
                      title={m.cl_files_title()}
                      onclick={(e) => {
                        e.stopPropagation();
                        viewing = { id: it.id, label: it.label };
                      }}>{m.cl_files_button()}</button
                    >
                  {/if}
                </div>
              {/each}
            </div>
          {/each}
        </div>

        <div class="side">
          <div class="card">
            <div class="cap">{m.cl_about()}</div>
            {#if focusedItem}
              <div class="about-name">{focusedItem.label}</div>
              <p class="about">{focusedItem.about}</p>
              {#if focusedItem.per_user}<p class="about muted">{m.cl_per_user()}</p>{/if}
              {#if focusedItem.id === 'browser_history'}
                <label class="keep">
                  <span>{m.cl_keep_history()}</span>
                  <select
                    class="form-input"
                    value={keepDays}
                    disabled={!!busy}
                    onchange={(e) => setKeep(Number((e.currentTarget as HTMLSelectElement).value))}
                  >
                    {#each KEEP_CHOICES as d (d)}<option value={d}>{keepLabel(d)}</option>{/each}
                  </select>
                </label>
              {/if}
            {:else}
              <p class="about muted">{m.cl_pick_item()}</p>
            {/if}
          </div>
          <div class="card">
            <div class="cap">{m.cl_users()}</div>
            {#each catalog.users as u (u.sid)}
              <label class="user" class:off={!u.allowed}>
                <input
                  type="checkbox"
                  checked={users.has(u.sid)}
                  disabled={!u.allowed || !!busy}
                  onchange={() => toggleUser(u.sid)}
                />
                <span>{u.name}</span>
                {#if u.me}<span class="muted">({m.cl_user_me()})</span>{/if}
                {#if !u.allowed}<span class="muted note">{m.cl_user_admin_only()}</span>{/if}
              </label>
            {/each}
            <p class="about muted">{m.cl_users_hint()}</p>
          </div>
        </div>
      </div>
    {/if}

    {#if error}<div class="banner err">{error}</div>{/if}
  </div>

  {#if viewing}
    <CleanupFilesModal item={viewing.id} label={viewing.label} onClose={() => (viewing = null)} />
  {/if}

  <div class="footer">
    <div class="hint">{hint()}</div>
    {#if busy}
      <button class="btn danger" onclick={cancel}>{m.common_cancel()}</button>
    {:else if confirming}
      <button class="btn outline" onclick={() => (confirming = false)}>{m.common_cancel()}</button>
      <button class="btn green" onclick={() => go('run')}>{m.cl_confirm_yes()}</button>
    {:else}
      <button class="btn outline" disabled={!catalog} onclick={() => go('analyze')}
        >{m.cl_analyze()}</button
      >
      <button class="btn green" disabled={!catalog || chosen().length === 0} onclick={() => (confirming = true)}
        >{m.cl_clean()}</button
      >
    {/if}
  </div>
</div>

<style>
  .page { height: 100%; display: flex; flex-direction: column; }
  .scroll { flex: 1; overflow: auto; padding: 30px 44px; }
  h1 { font-size: 24px; font-weight: 300; color: var(--head); margin-bottom: 8px; }
  .lead { font-size: 13px; color: var(--text-secondary); margin: 0 0 20px; max-width: 900px; line-height: 1.5; }
  .cols { display: flex; gap: 16px; align-items: flex-start; }
  .card { border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); padding: 14px 18px; }
  .tree { flex: 1; min-width: 0; padding: 8px 0; }
  .side { width: 340px; flex-shrink: 0; display: flex; flex-direction: column; gap: 16px; }
  .group + .group { border-top: 1px solid var(--border); margin-top: 6px; padding-top: 6px; }
  .row { display: flex; align-items: center; gap: 10px; padding: 6px 18px; font-size: 14px; color: var(--text); cursor: pointer; }
  .row.head { font-weight: 600; color: var(--head); }
  .row.item { padding-left: 42px; border-radius: 4px; }
  .row.item:hover { background: var(--panel2); }
  .row.item.focused { background: var(--accent-soft); }
  .row input { width: auto; margin: 0; flex: 0 0 auto; }
  .name { flex: 1; min-width: 0; }
  .size { font-size: 13px; color: var(--text); white-space: nowrap; }
  .muted { color: var(--text-muted); }
  .files-btn { background: transparent; border: 1px solid var(--border-strong); border-radius: 5px; padding: 2px 9px; font-size: 12px; color: var(--accent-text); cursor: pointer; font-family: var(--font); }
  .files-btn:hover { background: var(--accent-soft); }
  .cap { font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: .04em; color: var(--text-muted); margin-bottom: 8px; }
  .about-name { font-size: 14px; font-weight: 600; color: var(--head); margin-bottom: 6px; }
  .about { font-size: 13px; line-height: 1.5; color: var(--text); margin: 0 0 6px; }
  .keep { display: flex; flex-direction: column; gap: 6px; margin-top: 10px; font-size: 13px; color: var(--text); }
  .user { display: flex; align-items: center; gap: 8px; font-size: 14px; color: var(--text); padding: 4px 0; cursor: pointer; }
  .user input { width: auto; margin: 0; }
  .user.off { cursor: default; }
  .user { flex-wrap: wrap; }
  .user .note { flex-basis: 100%; padding-left: 24px; font-size: 12px; margin-top: -2px; }
  .banner.err { margin-top: 16px; padding: 10px 14px; font-size: 13px; color: var(--error-text); background: var(--error-soft); border: 1px solid var(--error-line); border-radius: 8px; overflow-wrap: anywhere; }
  .hint { flex: 1; font-size: 13px; color: var(--text-secondary); }
  /* The bar's buttons, as on the job dashboard (.footer is global). */
  .btn { border-radius: 6px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid transparent; transition: background-color .15s ease; }
  .btn:disabled { opacity: .45; cursor: default; }
  .btn.outline { background: transparent; border-color: var(--primary); color: var(--accent-text); }
  .btn.outline:hover:not(:disabled) { background: var(--accent-soft); }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover:not(:disabled) { background: var(--green-hover); }
  .btn.danger { background: transparent; border-color: var(--error); color: var(--error-text); }
  .btn.danger:hover:not(:disabled) { background: var(--error-soft); }
</style>
