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
  import CleanupMasksModal from '../components/CleanupMasksModal.svelte';
  import CleanupAutoModal from '../components/CleanupAutoModal.svelte';
  import { pickPath } from '../lib/pickPath.svelte';
  import { cleanupState } from '../lib/cleanupStore.svelte';
  import { maskProblem } from '../lib/junkMasks';
  import type {
    CleanupCatalog,
    CleanupItem,
    CleanupJunk,
    CleanupOptions,
    CleanupProfile,
    CleanupProgress,
    CleanupReport,
    CleanupTally,
  } from '../lib/types';

  const ITEMS_KEY = 'ob.cleanup.items';
  const USERS_KEY = 'ob.cleanup.users';
  const HISTORY_KEY = 'ob.cleanup.historyDays';
  /** «Хранить историю»: 0 removes it all; a week unless chosen otherwise. */
  const KEEP_CHOICES = [0, 1, 7, 30] as const;
  const JUNK_KEY = 'ob.cleanup.junk';
  /** Masks once offered and taken out of the list (owner, 28.10: `*.tmp*`
   *  takes `.tmpl` templates): dropped from a remembered choice too. */
  const WITHDRAWN = ['*.tmp*'];
  /** Junk by masks: files unchanged for this many days. */
  const AGE_CHOICES = [1, 7, 30, 90] as const;

  let catalog = $state<CleanupCatalog | null>(null);
  let loadError = $state<string | null>(null);
  let items = $state<Set<string>>(new Set());
  let users = $state<Set<string>>(new Set());
  let confirming = $state(false);
  let error = $state<string | null>(null);
  /** The analysis and its marks live in `cleanupState` (lib/cleanupStore):
   *  they outlast a visit to another section. */
  let focused = $state<string | null>(null);
  /** The item whose files are listed in the window over the page. */
  let viewing = $state<{ id: string; label: string } | null>(null);
  /** The junk search's masks, age and folders; the service's defaults until
   *  changed. */
  let junk = $state<CleanupJunk | null>(null);
  /** The window of masks is open. */
  let editingMasks = $state(false);
  /** The automatic cleanup's set, as the service keeps it. */
  let auto = $state<CleanupProfile | null>(null);
  let editingAuto = $state(false);
  let autoSaving = $state(false);
  let autoError = $state<string | null>(null);
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

  function rememberedJunk(): CleanupJunk | null {
    try {
      const v = JSON.parse(localStorage.getItem(JUNK_KEY) ?? 'null');
      if (!v || !Array.isArray(v.masks) || typeof v.age_days !== 'number' || !Array.isArray(v.folders)) return null;
      const masks = v.masks.filter(
        (x: unknown) => typeof x === 'string' && maskProblem(x) === null && !WITHDRAWN.includes(x),
      );
      if (masks.length === 0) return null;
      const age = (AGE_CHOICES as readonly number[]).includes(v.age_days) ? v.age_days : 7;
      return { masks, age_days: age, folders: v.folders.filter((x: unknown) => typeof x === 'string') };
    } catch {
      return null;
    }
  }

  function saveJunk(next: CleanupJunk) {
    if (JSON.stringify(next) === JSON.stringify(junk)) return;
    junk = next;
    localStorage.setItem(JUNK_KEY, JSON.stringify(next));
    changed();
  }

  function setMasks(masks: string[], id: string) {
    editingMasks = false;
    if (!items.has(id)) {
      items = new Set([...items, id]);
      remember();
    }
    if (junk) saveJunk({ ...junk, masks });
  }

  function ageLabel(days: number): string {
    return days === 1
      ? m.cl_age_day()
      : days === 7
        ? m.cl_age_week()
        : days === 30
          ? m.cl_age_month()
          : m.cl_age_quarter();
  }

  async function addFolder() {
    if (!junk) return;
    const dir = await pickPath({ directory: true, title: m.cl_junk_pick() });
    if (dir && !junk.folders.includes(dir)) saveJunk({ ...junk, folders: [...junk.folders, dir] });
  }

  function removeFolder(folder: string) {
    if (junk) saveJunk({ ...junk, folders: junk.folders.filter((f) => f !== folder) });
  }

  /** The ticked items the automatic cleanup can take, and those it cannot
   *  (cleaned only after a look at their list). */
  function autoItems() {
    const all = (catalog?.groups ?? []).flatMap((g) => g.items).filter((i) => items.has(i.id) && allowed(i));
    return { take: all.filter((i) => !i.review), left: all.filter((i) => i.review).map((i) => i.label) };
  }

  async function saveAuto(schedule: CleanupProfile['schedule']) {
    autoSaving = true;
    autoError = null;
    try {
      auto = await api.cleanupSaveProfile({
        schedule: { ...(auto?.schedule ?? {}), ...schedule },
        items: autoItems().take.map((i) => i.id),
        users: [...users],
        options: options(),
      });
      editingAuto = false;
    } catch (e) {
      autoError = String(e);
    } finally {
      autoSaving = false;
    }
  }

  function weekdayNames(days: string[]): string {
    const names: Record<string, () => string> = {
      mon: m.wiz_day_mon, tue: m.wiz_day_tue, wed: m.wiz_day_wed, thu: m.wiz_day_thu,
      fri: m.wiz_day_fri, sat: m.wiz_day_sat, sun: m.wiz_day_sun,
    };
    return ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun']
      .filter((d) => days.includes(d))
      .map((d) => names[d]())
      .join(', ');
  }

  function autoText(): string {
    const p = auto;
    // An older service answers nothing of the kind.
    if (!p?.schedule || p.schedule.kind === 'none' || !p.items?.length) return m.cla_summary_off();
    const at = p.schedule.time;
    const when =
      p.schedule.kind === 'daily'
        ? m.cla_daily({ time: at })
        : p.schedule.kind === 'weekly'
          ? m.cla_weekly({ time: at, days: weekdayNames(p.schedule.days) })
          : m.cla_monthly({ time: at, day: p.schedule.day_of_month });
    return m.cla_summary({ when, count: p.items.length });
  }

  function options(): CleanupOptions {
    return { history_keep_days: keepDays, junk: junk ?? undefined };
  }

  function toggleExcluded(item: string, path: string, size: number) {
    const next = new Map(cleanupState.excluded[item] ?? []);
    if (next.has(path)) next.delete(path);
    else next.set(path, size);
    cleanupState.excluded = { ...cleanupState.excluded, [item]: next };
    confirming = false;
  }

  function excludedBytes(item: string): number {
    return [...(cleanupState.excluded[item]?.values() ?? [])].reduce((s, b) => s + b, 0);
  }

  function openList(id: string, label: string) {
    viewing = { id, label };
    cleanupState.reviewed = new Set([...cleanupState.reviewed, id]);
  }

  /** Ticked items cleaned only from a list nobody has opened yet. One the
   *  analysis found nothing for has nothing to look through. */
  function unreviewed(): CleanupItem[] {
    return (catalog?.groups ?? [])
      .flatMap((g) => g.items)
      .filter(
        (i) =>
          i.review &&
          items.has(i.id) &&
          (cleanupState.shown !== 'analyze' || (!cleanupState.reviewed.has(i.id) && (cleanupState.tallies[i.id]?.files ?? 0) > 0)),
      );
  }

  /** Where the junk item is on its way to a cleanup, for its steps. */
  function step(id: string): 'masks' | 'analyze' | 'review' | 'clean' {
    if (!items.has(id)) return 'masks';
    if (cleanupState.shown !== 'analyze' || !cleanupState.tallies[id]) return 'analyze';
    if ((cleanupState.tallies[id]?.files ?? 0) > 0 && !cleanupState.reviewed.has(id)) return 'review';
    return 'clean';
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
      .cleanupProfile()
      .then((p) => (auto = p?.schedule ? p : null))
      .catch(() => {});
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
        const offered = c.junk ?? { masks: ['*.tmp'], age_days: 7, folders: [] };
        const kept = rememberedJunk() ?? offered;
        junk = { ...kept, folders: c.admin ? kept.folders : [] };
      })
      .catch((e) => (loadError = String(e)));
    const un = listen<CleanupProgress>('cleanup-progress', (e) => {
      const p = e.payload;
      if (cleanupState.busy && p.mode === cleanupState.busy) {
        cleanupState.tallies = { ...cleanupState.tallies, [p.item]: p };
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
    const mine = groupItems.filter(allowed);
    const all = mine.every((i) => next.has(i.id));
    for (const i of mine) {
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
    if (cleanupState.shown === 'analyze') {
      cleanupState.tallies = {};
      cleanupState.report = null;
      cleanupState.shown = 'none';
      cleanupState.stale = true;
    }
    cleanupState.excluded = {};
    cleanupState.reviewed = new Set();
  }

  /** Some of the catalogue only an administrator's window may clean. */
  const limited = $derived(
    !!catalog &&
      !catalog.admin &&
      (catalog.groups.some((g) => g.items.some((i) => i.admin_only)) || catalog.users.some((u) => !u.allowed)),
  );

  /** A click on an item only an administrator's window may clean: the bar
   *  offers to start the window again as one. */
  let askElevate = $state(false);

  async function elevate() {
    askElevate = false;
    try {
      await api.relaunchElevated('cleanup');
    } catch (e) {
      error = String(e);
    }
  }

  /** Whether this window may clean the item (Windows' own components need
   *  an administrator's). */
  function allowed(i: CleanupItem): boolean {
    return !i.admin_only || !!catalog?.admin;
  }

  /** The ticked items, in the catalogue's order. */
  function chosen(): string[] {
    return (catalog?.groups ?? [])
      .flatMap((g) => g.items)
      .filter((i) => items.has(i.id) && allowed(i))
      .map((i) => i.id);
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
    if (mode === 'run' && unreviewed().length > 0) return;
    const exclude = mode === 'run' ? list.flatMap((id) => [...(cleanupState.excluded[id]?.keys() ?? [])]) : [];
    if (mode === 'analyze') {
      cleanupState.excluded = {};
      cleanupState.reviewed = new Set();
    }
    confirming = false;
    error = null;
    cleanupState.report = null;
    cleanupState.busy = mode;
    cleanupState.shown = mode;
    cleanupState.stale = false;
    cleanupState.tallies = {};
    try {
      const r =
        mode === 'analyze'
          ? await api.cleanupAnalyze(list, [...users], options())
          : await api.cleanupRun(list, [...users], options(), exclude);
      cleanupState.report = r;
      cleanupState.tallies = Object.fromEntries(r.items.map((i) => [i.id, i]));
    } catch (e) {
      error = String(e);
    } finally {
      cleanupState.busy = null;
    }
  }

  function cancel() {
    api.cleanupCancel().catch(() => {});
  }

  function sizeText(id: string): string {
    const t = cleanupState.tallies[id];
    if (!t) {
      if (cleanupState.busy === 'analyze' || (cleanupState.busy && items.has(id))) return '…';
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
    const t = cleanupState.tallies[id];
    return !!t && (t.files > 0 || (t.entries ?? 0) > 0);
  }

  function groupBytes(groupItems: CleanupItem[]): number | null {
    const known = groupItems.filter((i) => items.has(i.id) && cleanupState.tallies[i.id]);
    if (known.length === 0) return null;
    return known.reduce((s, i) => s + cleanupState.tallies[i.id].bytes, 0);
  }

  const total = $derived(
    chosen().reduce(
      (s, id) => s + (cleanupState.tallies[id]?.bytes ?? 0) - (cleanupState.shown === 'analyze' ? excludedBytes(id) : 0),
      0,
    ),
  );
  const focusedItem = $derived(
    (catalog?.groups ?? []).flatMap((g) => g.items).find((i) => i.id === focused) ?? null,
  );

  function hint(): string {
    if (cleanupState.busy === 'analyze') return `${m.cl_hint_analyzing()} ${formatBytes(total)}`;
    if (cleanupState.busy === 'run') return `${m.cl_hint_cleaning()} ${formatBytes(total)}`;
    if (askElevate) return m.cl_elevate_ask();
    if (confirming) return m.cl_confirm();
    if (chosen().length === 0) return m.cl_hint_nothing();
    const waiting = unreviewed();
    if (waiting.length > 0 && cleanupState.shown !== 'run') {
      return m.cl_review_first({ names: waiting.map((i) => i.label).join(', ') });
    }
    if (cleanupState.report && cleanupState.shown === 'run') {
      const size = formatBytes(cleanupState.report.total.bytes);
      if (cleanupState.report.cancelled) return m.cl_hint_cancelled({ size });
      if (cleanupState.report.total.left_files > 0) {
        return m.cl_hint_done_left({
          size,
          count: cleanupState.report.total.left_files,
          left: formatBytes(cleanupState.report.total.left_bytes),
        });
      }
      return m.cl_hint_done({ size });
    }
    if (cleanupState.shown === 'analyze' && cleanupState.report) return m.cl_hint_found({ size: formatBytes(total) });
    if (cleanupState.stale) return m.cl_stale();
    return m.cl_hint_start();
  }
</script>

<div class="page">
  <div class="scroll">
    <h1>{m.cl_title()}</h1>
    <p class="lead">{m.cl_lead()}</p>
    {#if limited}
      <div class="elevate">
        <span>{m.cl_elevate_hint()}</span>
        <button class="files-btn" onclick={elevate}>{m.cl_elevate()}</button>
      </div>
    {/if}

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
                  checked={g.items.filter(allowed).length > 0 && g.items.filter(allowed).every((i) => items.has(i.id))}
                  disabled={!!cleanupState.busy}
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
                    class:locked={!allowed(it)}
                    checked={items.has(it.id) && allowed(it)}
                    disabled={!!cleanupState.busy}
                    onclickcapture={(e) => {
                      if (!allowed(it)) {
                        e.preventDefault();
                        focused = it.id;
                        askElevate = true;
                      }
                    }}
                    onchange={() => toggleItem(it.id)}
                    onclick={(e) => e.stopPropagation()}
                    aria-label={it.label}
                  />
                  <span class="name">{it.label}</span>
                  <span
                    class="size"
                    class:muted={!cleanupState.tallies[it.id] || !items.has(it.id)}
                    title={cleanupState.tallies[it.id] ? m.cl_files({ count: cleanupState.tallies[it.id].files }) : ''}
                    >{sizeText(it.id)}</span
                  >
                  {#if cleanupState.shown === 'analyze' && !cleanupState.busy && listed(it.id)}
                    <button
                      class="files-btn"
                      title={m.cl_files_title()}
                      onclick={(e) => {
                        e.stopPropagation();
                        openList(it.id, it.label);
                      }}>{m.cl_files_button()}</button
                    >
                  {/if}
                </div>
              {/each}
            </div>
          {/each}
          <div class="auto">
            <div class="auto-text">
              <span class="auto-name">{m.cla_title()}</span>
              <span>{autoText()}</span>
            </div>
            <button class="files-btn" disabled={!!cleanupState.busy} onclick={() => ((autoError = null), (editingAuto = true))}
              >{m.cla_setup()}</button
            >
          </div>
        </div>

        <div class="side">
          <div class="card">
            <div class="cap">{m.cl_about()}</div>
            {#if focusedItem}
              <div class="about-name">{focusedItem.label}</div>
              <p class="about">{focusedItem.about}</p>
              {#if focusedItem.per_user}<p class="about muted">{m.cl_per_user()}</p>{/if}
              {#if !allowed(focusedItem)}
                <p class="about muted">{m.cl_item_admin_only()}</p>
                <button class="files-btn add" onclick={elevate}>{m.cl_elevate()}</button>
              {/if}
              {#if focusedItem.id === 'browser_history'}
                <label class="keep">
                  <span>{m.cl_keep_history()}</span>
                  <select
                    class="form-input"
                    value={keepDays}
                    disabled={!!cleanupState.busy}
                    onchange={(e) => setKeep(Number((e.currentTarget as HTMLSelectElement).value))}
                  >
                    {#each KEEP_CHOICES as d (d)}<option value={d}>{keepLabel(d)}</option>{/each}
                  </select>
                </label>
              {/if}
              {#if focusedItem.review && junk}
                {@const at = step(focusedItem.id)}
                {@const found = cleanupState.tallies[focusedItem.id]?.files ?? 0}
                <ol class="steps">
                  <li class:now={at === 'masks'} class:done={at !== 'masks'}>
                    <span>{m.cl_step_masks({ count: junk.masks.length })}</span>
                    <button class="files-btn" disabled={!!cleanupState.busy} onclick={() => (editingMasks = true)}
                      >{m.cl_junk_edit_masks({ count: junk.masks.length })}</button
                    >
                  </li>
                  <li class:now={at === 'analyze'} class:done={at === 'review' || at === 'clean'}>
                    <span>{m.cl_step_analyze()}</span>
                    {#if at === 'analyze'}
                      <button class="files-btn" disabled={!!cleanupState.busy || !items.has(focusedItem.id)} onclick={() => go('analyze')}
                        >{m.cl_analyze()}</button
                      >
                    {/if}
                  </li>
                  <li class:now={at === 'review'} class:done={at === 'clean'}>
                    {#if cleanupState.shown === 'analyze' && cleanupState.tallies[focusedItem.id] && found === 0}
                      <span>{m.cl_step_nothing()}</span>
                    {:else}
                      <span>{m.cl_step_review()}</span>
                      {#if cleanupState.shown === 'analyze' && found > 0}
                        {@const it = focusedItem}
                        <button class="files-btn" disabled={!!cleanupState.busy} onclick={() => openList(it.id, it.label)}
                          >{m.cl_step_files({ count: found })}</button
                        >
                      {/if}
                    {/if}
                  </li>
                  <li class:now={at === 'clean' && found > 0}>
                    <span>{m.cl_step_clean()}</span>
                  </li>
                </ol>
                <label class="keep">
                  <span>{m.cl_junk_age()}</span>
                  <select
                    class="form-input"
                    value={junk.age_days}
                    disabled={!!cleanupState.busy}
                    onchange={(e) =>
                      junk && saveJunk({ ...junk, age_days: Number((e.currentTarget as HTMLSelectElement).value) })}
                  >
                    {#each AGE_CHOICES as d (d)}<option value={d}>{ageLabel(d)}</option>{/each}
                  </select>
                </label>
                <div class="keep">
                  <span>{m.cl_junk_folders()}</span>
                  {#if catalog.admin}
                    {#each junk.folders as f (f)}
                      <div class="folder">
                        <span class="folder-path" title={f}><bdi>{f}</bdi></span>
                        <button
                          class="x"
                          disabled={!!cleanupState.busy}
                          title={m.cl_junk_remove_folder()}
                          aria-label={m.cl_junk_remove_folder()}
                          onclick={() => removeFolder(f)}>×</button
                        >
                      </div>
                    {/each}
                    <button class="files-btn add" disabled={!!cleanupState.busy} onclick={addFolder}>{m.cl_junk_add_folder()}</button>
                  {:else}
                    <p class="about muted">{m.cl_junk_folders_admin()}</p>
                  {/if}
                </div>
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
                  disabled={!u.allowed || !!cleanupState.busy}
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

  {#if editingAuto && catalog}
    {@const chosenNow = autoItems()}
    <CleanupAutoModal
      profile={auto ?? {
        schedule: { kind: 'none', days: ['mon', 'wed', 'fri'], time: '03:00', day_of_month: 1, run_missed: true, missed_delay_min: 10 },
        items: [],
        users: [],
        options: options(),
      }}
      items={chosenNow.take.map((i) => ({ id: i.id, label: i.label }))}
      users={catalog.users.filter((u) => users.has(u.sid)).map((u) => ({ sid: u.sid, name: u.name }))}
      left={chosenNow.left}
      saving={autoSaving}
      error={autoError}
      onSave={saveAuto}
      onClose={() => (editingAuto = false)}
    />
  {/if}

  {#if editingMasks && junk}
    {@const id = focused ?? 'junk'}
    <CleanupMasksModal
      offered={catalog?.masks ?? []}
      chosen={junk.masks}
      onSave={(masks) => setMasks(masks, id)}
      onClose={() => (editingMasks = false)}
    />
  {/if}

  {#if viewing}
    {@const id = viewing.id}
    <CleanupFilesModal
      item={id}
      label={viewing.label}
      excluded={cleanupState.excluded[id] ?? new Map()}
      onToggle={(path, size) => toggleExcluded(id, path, size)}
      onClose={() => (viewing = null)}
    />
  {/if}

  <div class="footer">
    <div class="hint">{hint()}</div>
    {#if askElevate && !cleanupState.busy}
      <button class="btn outline" onclick={() => (askElevate = false)}>{m.common_cancel()}</button>
      <button class="btn green" onclick={elevate}>{m.cl_elevate()}</button>
    {:else if cleanupState.busy}
      <button class="btn danger" onclick={cancel}>{m.common_cancel()}</button>
    {:else if confirming}
      <button class="btn outline" onclick={() => (confirming = false)}>{m.common_cancel()}</button>
      <button class="btn green" onclick={() => go('run')}>{m.cl_confirm_yes()}</button>
    {:else}
      <button class="btn outline" disabled={!catalog} onclick={() => go('analyze')}
        >{m.cl_analyze()}</button
      >
      <button
        class="btn green"
        disabled={!catalog || chosen().length === 0 || unreviewed().length > 0}
        onclick={() => (confirming = true)}
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
  .elevate { display: flex; align-items: center; gap: 12px; margin: -8px 0 16px; padding: 8px 14px; font-size: 13px; color: var(--text); background: var(--panel2); border: 1px solid var(--border); border-radius: 8px; max-width: 900px; }
  .elevate span { flex: 1; }
  .auto { display: flex; align-items: center; gap: 12px; border-top: 1px solid var(--border); margin-top: 6px; padding: 10px 18px 4px; font-size: 13px; color: var(--text-secondary); }
  .auto-text { flex: 1; display: flex; flex-direction: column; gap: 2px; }
  .auto-name { font-weight: 600; color: var(--head); font-size: 14px; }
  .cols { display: flex; gap: 16px; align-items: flex-start; }
  .card { border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); padding: 14px 18px; }
  .tree { flex: 1; min-width: 0; padding: 8px 0; }
  /* In sight while the list scrolls: held at the top of the page. */
  .side { width: 340px; flex-shrink: 0; display: flex; flex-direction: column; gap: 16px; position: sticky; top: 0; max-height: calc(100vh - 130px); overflow-y: auto; }
  .steps { margin: 10px 0 0; padding-left: 20px; display: flex; flex-direction: column; gap: 8px; font-size: 13px; color: var(--text-secondary); }
  .steps li { padding-left: 2px; }
  .steps li span { display: block; margin-bottom: 4px; }
  .steps li.now { color: var(--head); font-weight: 600; }
  .steps li.done { color: var(--text-muted); }
  .steps li.done span::after { content: ' ✓'; color: var(--green); }
  .group + .group { border-top: 1px solid var(--border); margin-top: 6px; padding-top: 6px; }
  .row { display: flex; align-items: center; gap: 10px; padding: 6px 18px; font-size: 14px; color: var(--text); cursor: pointer; }
  .row.head { font-weight: 600; color: var(--head); }
  .row.item { padding-left: 42px; border-radius: 4px; }
  .row.item:hover { background: var(--panel2); }
  .row.item.focused { background: var(--accent-soft); }
  .row input { width: auto; margin: 0; flex: 0 0 auto; }
  .row input.locked { opacity: .45; }
  .name { flex: 1; min-width: 0; }
  .size { font-size: 13px; color: var(--text); white-space: nowrap; }
  .muted { color: var(--text-muted); }
  .files-btn { background: transparent; border: 1px solid var(--border-strong); border-radius: 5px; padding: 2px 9px; font-size: 12px; color: var(--accent-text); cursor: pointer; font-family: var(--font); }
  .files-btn:hover { background: var(--accent-soft); }
  .cap { font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: .04em; color: var(--text-muted); margin-bottom: 8px; }
  .about-name { font-size: 14px; font-weight: 600; color: var(--head); margin-bottom: 6px; }
  .about { font-size: 13px; line-height: 1.5; color: var(--text); margin: 0 0 6px; }
  .keep { display: flex; flex-direction: column; gap: 6px; margin-top: 10px; font-size: 13px; color: var(--text); }
  .folder { display: flex; align-items: center; gap: 6px; font-size: 13px; color: var(--text); }
  .folder-path { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; direction: rtl; text-align: left; }
  .x { background: none; border: none; font-size: 16px; line-height: 1; color: var(--text-muted); cursor: pointer; padding: 0 4px; }
  .x:hover:not(:disabled) { color: var(--error-text); }
  .files-btn.add { align-self: flex-start; }
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
