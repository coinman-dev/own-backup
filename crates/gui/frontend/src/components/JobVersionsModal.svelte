<script lang="ts">
  // «Очистить версии…» — the copies a job has on disk, as a tree of chains, and
  // a way to delete the ones that are ticked.
  //
  // A version is not a file that can go on its own: an increment is read
  // THROUGH the copies under it. So the ticks follow the dependencies — taking
  // a copy takes everything built on it, and keeping a copy keeps everything
  // it stands on. The service checks the same rule again and refuses a set
  // that would leave a copy without its base; this window only makes sure such
  // a set cannot be put together by accident.
  import { onMount } from 'svelte';
  import Modal from './Modal.svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import { chainSize, chainSpan, roleLabel, shortDateTime } from '../lib/jobVersions';
  import type { Job, JobFilesDeleted, JobVersion, JobVersionChain, JobVersions } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    job: Job;
    onClose: () => void;
  }
  let { job, onClose }: Props = $props();

  let versions = $state<JobVersions | null>(null);
  let loadError = $state('');
  let selected = $state<Set<string>>(new Set());
  let collapsed = $state<Set<string>>(new Set());
  let stage = $state<'pick' | 'confirm' | 'busy'>('pick');
  /** What the last deletion did, shown above the list it left behind. */
  let report = $state<{ gone: number; kept: [string, string][] } | null>(null);
  let failure = $state('');
  let firstLoad = true;

  const images = $derived((versions?.chains ?? []).flatMap((c) => c.images));
  const byPath = $derived(new Map(images.map((i) => [i.image_path, i])));
  /** Who is built directly on whom. */
  const builtOn = $derived.by(() => {
    const map = new Map<string, string[]>();
    for (const i of images) {
      if (!i.depends_on) continue;
      map.set(i.depends_on, [...(map.get(i.depends_on) ?? []), i.image_path]);
    }
    return map;
  });
  const sources = $derived([...new Set((versions?.chains ?? []).map((c) => c.source))]);
  const selectedBytes = $derived(
    images.filter((i) => selected.has(i.image_path)).reduce((sum, i) => sum + i.size_bytes, 0),
  );
  /** A source whose newest version is ticked, and one left with nothing. */
  const latestTicked = $derived(
    (versions?.chains ?? []).some((c) => {
      const last = c.images[c.images.length - 1];
      return c.current && !!last && selected.has(last.image_path);
    }),
  );
  const nothingLeft = $derived(
    sources.some((s) =>
      (versions?.chains ?? [])
        .filter((c) => c.source === s)
        .every((c) => c.images.every((i) => selected.has(i.image_path))),
    ),
  );

  async function load() {
    loadError = '';
    try {
      versions = await api.listJobVersions(job.id);
      if (firstLoad) {
        // Offer retired chains first; the chain receiving new increments is
        // kept unless the person explicitly chooses it.
        selected = new Set(versions.chains.filter((c) => !c.current).flatMap((c) => c.images.map((i) => i.image_path)));
        firstLoad = false;
      } else {
        // What was ticked and is gone must not stay ticked.
        selected = new Set([...selected].filter((p) => byPath.has(p)));
      }
    } catch (e) {
      loadError = String(e);
    }
  }
  onMount(load);

  /** A copy and everything that cannot be read without it. */
  function withDependents(path: string): string[] {
    const out: string[] = [];
    const queue = [path];
    while (queue.length) {
      const p = queue.pop()!;
      if (out.includes(p)) continue;
      out.push(p);
      queue.push(...(builtOn.get(p) ?? []));
    }
    return out;
  }

  /** Everything a copy stands on, down to its full copy. */
  function ancestors(path: string): string[] {
    const out: string[] = [];
    let at = byPath.get(path)?.depends_on ?? null;
    while (at && !out.includes(at)) {
      out.push(at);
      at = byPath.get(at)?.depends_on ?? null;
    }
    return out;
  }

  function tick(image: JobVersion, on: boolean) {
    const next = new Set(selected);
    if (on) {
      for (const p of withDependents(image.image_path)) next.add(p);
    } else {
      next.delete(image.image_path);
      for (const p of ancestors(image.image_path)) next.delete(p);
    }
    selected = next;
  }

  function tickChain(chain: JobVersionChain, on: boolean) {
    const next = new Set(selected);
    for (const i of chain.images) {
      if (on) next.add(i.image_path);
      else next.delete(i.image_path);
    }
    selected = next;
  }

  const chainKey = (c: JobVersionChain) => c.images[0]?.image_path ?? '';
  const tickedIn = (c: JobVersionChain) => c.images.filter((i) => selected.has(i.image_path)).length;

  function fold(c: JobVersionChain) {
    const next = new Set(collapsed);
    const key = chainKey(c);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    collapsed = next;
  }

  async function remove() {
    const wanted = [...selected];
    stage = 'busy';
    failure = '';
    report = null;
    try {
      const done: JobFilesDeleted = await api.deleteJobVersions(job.id, wanted);
      // The reply lists files — a copy's sidecar too; versions are counted.
      const gone = done.deleted.filter((p) => wanted.includes(p)).length;
      report = { gone, kept: done.kept };
      selected = new Set();
      await load();
    } catch (e) {
      failure = String(e);
    } finally {
      stage = 'pick';
    }
  }

  function close() {
    if (stage === 'busy') return; // the files are going; there is nothing to stop
    if (stage === 'confirm') {
      stage = 'pick';
      return;
    }
    onClose();
  }
</script>

<Modal
  open
  wide
  danger
  title={m.jvc_title()}
  onClose={close}
  onConfirm={() => (stage === 'pick' ? (stage = 'confirm') : remove())}
  confirmDisabled={selected.size === 0 || stage === 'busy'}
  confirmLabel={stage === 'busy' ? m.jvc_deleting() : m.bs_delete()}
  cancelLabel={stage === 'confirm' ? m.common_back() : m.common_close()}
>
  {#if stage !== 'pick'}
    <p class="ask">{m.jvc_confirm({ versions: m.jvc_n_versions({ count: selected.size }), size: formatBytes(selectedBytes), name: job.name })}</p>
    <p class="warn">{m.jvc_no_undo()}</p>
    {#if nothingLeft}
      <p class="warn">{m.jvc_warn_all()}</p>
    {:else if latestTicked}
      <p class="warn">{m.jvc_warn_latest()}</p>
    {/if}
  {:else}
    <p class="intro">{m.jvc_intro()}</p>

    {#if report}
      {#if report.gone > 0}
        <div class="banner ok">{m.jvc_deleted({ versions: m.jvc_n_versions({ count: report.gone }) })}</div>
      {/if}
      {#if report.kept.length > 0}
        <div class="banner err">
          {m.jvc_kept()}
          {#each report.kept as [path, why]}
            <div class="kept-line">{path} — {why}</div>
          {/each}
        </div>
      {/if}
    {/if}
    {#if failure}<div class="banner err">{failure}</div>{/if}
    {#if loadError}<div class="banner err">{loadError}</div>{/if}
    {#each versions?.unreadable ?? [] as [folder, why]}
      <div class="banner err">{m.jvc_unreadable({ folder })} {why}</div>
    {/each}
    {#if versions?.history_truncated}
      <div class="banner note">{m.bs_history_truncated()}</div>
    {/if}

    {#if !versions && !loadError}
      <p class="empty">{m.jv_loading()}</p>
    {:else if versions && versions.chains.length === 0}
      <p class="empty">{m.jv_none()}</p>
    {:else if versions}
      <div class="table">
        <div class="row head">
          <span>{m.jvc_col_version()}</span>
          <span>{m.jvc_col_type()}</span>
          <span class="num">{m.jvc_col_size()}</span>
        </div>
        {#each sources as source (source)}
          {#if sources.length > 1}
            <div class="row source">{m.jvc_source({ name: source })}</div>
          {/if}
          {#each versions.chains.filter((c) => c.source === source) as chain (chainKey(chain))}
            {@const ticked = tickedIn(chain)}
            {@const open = !collapsed.has(chainKey(chain))}
            <div class="row chain">
              <span class="cell-name">
                <button class="fold" aria-expanded={open} aria-label={open ? m.jvc_fold() : m.jvc_unfold()} onclick={() => fold(chain)}>
                  <svg class:shut={!open} width="11" height="11" viewBox="0 0 24 24" fill="currentColor"><path d="M6 9l6 6 6-6z" /></svg>
                </button>
                <label>
                  <input
                    type="checkbox"
                    checked={ticked === chain.images.length}
                    indeterminate={ticked > 0 && ticked < chain.images.length}
                    onchange={(e) => tickChain(chain, e.currentTarget.checked)}
                  />
                  {m.jvc_chain({ span: chainSpan(chain) })}
                </label>
                {#if chain.current}<span class="tag">{m.jvc_current()}</span>{/if}
              </span>
              <span class="muted">{m.jvc_n_versions({ count: chain.images.length })}</span>
              <span class="num">{formatBytes(chainSize(chain))}</span>
            </div>
            {#if open}
              {#each chain.images as image (image.image_path)}
                <div class="row version" title={image.file_name}>
                  <span class="cell-name">
                    <label>
                      <input
                        type="checkbox"
                        checked={selected.has(image.image_path)}
                        onchange={(e) => tick(image, e.currentTarget.checked)}
                      />
                      {shortDateTime(image.created_at)}
                    </label>
                  </span>
                  <span>{roleLabel(image.role)}</span>
                  <span class="num">{formatBytes(image.size_bytes)}</span>
                </div>
              {/each}
            {/if}
          {/each}
        {/each}
      </div>
      <div class="total">
        {#if selected.size > 0}
          {m.jvc_selected({ versions: m.jvc_n_versions({ count: selected.size }), size: formatBytes(selectedBytes) })}
        {:else}
          {m.jvc_selected_none()}
        {/if}
      </div>
      {#if nothingLeft}
        <div class="banner note">{m.jvc_warn_all()}</div>
      {:else if latestTicked}
        <div class="banner note">{m.jvc_warn_latest()}</div>
      {/if}
    {/if}
  {/if}
</Modal>

<style>
  .intro { margin: 0 0 14px; font-size: 13px; color: var(--text-secondary); line-height: 1.5; }
  .ask { margin: 0 0 12px; font-size: 14px; color: var(--text); line-height: 1.5; }
  .warn { margin: 8px 0 0; font-size: 13px; font-weight: 600; color: var(--error-text); line-height: 1.5; }
  .empty { margin: 18px 0; text-align: center; font-size: 13px; color: var(--text-muted); }

  .table { border: 1px solid var(--border); border-radius: 8px; overflow: hidden; }
  .row { display: grid; grid-template-columns: minmax(0, 1fr) 132px 92px; align-items: center; gap: 12px; padding: 8px 14px; font-size: 13px; color: var(--text); border-top: 1px solid var(--border); }
  .row:first-child { border-top: none; }
  .row.head { background: var(--panel2); font-size: 12px; font-weight: 600; color: var(--text-secondary); }
  .row.source { display: block; background: var(--sunken); font-size: 12px; font-weight: 600; color: var(--text-secondary); }
  .row.chain { font-weight: 600; color: var(--head); }
  .row.version .cell-name { padding-left: 38px; }
  .row.version:hover, .row.chain:hover { background: var(--panel2); }
  .cell-name { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .cell-name label { display: flex; align-items: center; gap: 8px; cursor: pointer; min-width: 0; white-space: nowrap; }
  .cell-name label input { flex-shrink: 0; }
  .num { text-align: right; white-space: nowrap; }
  .muted { color: var(--text-muted); font-weight: 400; }
  .fold { display: flex; padding: 3px; border: none; background: none; color: var(--text-muted); cursor: pointer; border-radius: 4px; }
  .fold:hover { background: var(--track); color: var(--head); }
  .fold svg { transition: transform 0.12s ease; }
  .fold svg.shut { transform: rotate(-90deg); }
  .tag { flex-shrink: 0; padding: 1px 7px; border-radius: 9px; background: var(--accent-soft); color: var(--accent-text); font-size: 11px; font-weight: 600; }
  .total { margin-top: 12px; font-size: 13px; color: var(--text-secondary); }

  .banner { margin: 0 0 12px; padding: 10px 12px; border-radius: 8px; font-size: 13px; line-height: 1.45; }
  .total + .banner { margin: 12px 0 0; }
  .banner.ok { background: var(--success-soft); border: 1px solid var(--success-line); color: var(--success-text); }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
  .banner.note { background: var(--warning-soft); border: 1px solid var(--warning-line); color: var(--warning-text); }
  .kept-line { margin-top: 4px; overflow-wrap: anywhere; }
</style>
