<script lang="ts">
  // Linux: the boot medium onto a USB stick. The ISO is built by
  // scripts/build-linux-media.sh (live-build needs root, a Debian tree and
  // the network — not a dialog's job); here it is written, read back and given
  // a partition for the medium's logs, by the service.
  //
  // The stick is erased whole, so the dialog names it the way the person sees
  // it (model and size, not only /dev/sdb), refuses a stick the desktop has
  // mounted, and asks for an explicit tick before the button wakes up.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api, errorText } from '../lib/api';
  import { pickPath } from '../lib/pickPath.svelte';
  import { formatBytes } from '../lib/format';
  import type { MediaProgress, MediaStick, MediaStickReport } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  let iso = $state('');
  let sticks: MediaStick[] = $state([]);
  let loading = $state(false);
  let listError: string | null = $state(null);
  let device = $state('');
  let confirmed = $state(false);
  let writing = $state(false);
  let progress: MediaProgress | null = $state(null);
  let report: MediaStickReport | null = $state(null);
  let error: string | null = $state(null);

  const chosen = $derived(sticks.find((s) => s.device === device) ?? null);
  const canWrite = $derived(
    !writing && !report && iso.trim().length > 0 && !!chosen && chosen.mounted.length === 0 && confirmed,
  );

  onMount(() => {
    refresh();
    const un = listen<MediaProgress>('media-progress', (e) => (progress = e.payload));
    return () => { un.then((f) => f()); };
  });

  async function refresh() {
    loading = true;
    listError = null;
    try {
      sticks = await api.listMediaSticks();
      // A stick that went away takes its tick with it.
      if (!sticks.some((s) => s.device === device)) {
        device = sticks.length === 1 ? sticks[0].device : '';
        confirmed = false;
      }
    } catch (e) {
      listError = errorText(e);
    } finally {
      loading = false;
    }
  }

  async function chooseIso() {
    const path = await pickPath({ directory: false, title: m.ms_pick_iso(), extensions: ['iso'] });
    if (path) iso = path;
  }

  function choose(d: string) {
    if (writing || report) return;
    if (d !== device) confirmed = false;
    device = d;
  }

  function transport(t: string): string {
    if (t === 'usb') return m.ms_transport_usb();
    if (t === 'mmc') return m.ms_transport_mmc();
    return m.ms_transport_removable();
  }

  function stickName(s: MediaStick): string {
    return `${s.model || s.device} · ${formatBytes(s.size_bytes)}`;
  }

  async function write() {
    if (!canWrite || !chosen) return;
    writing = true;
    error = null;
    progress = null;
    try {
      report = await api.writeMediaStick({ iso_path: iso.trim(), device: chosen.device });
    } catch (e) {
      error = errorText(e);
      // What the stick holds now is not what the list said.
      confirmed = false;
      refresh();
    } finally {
      writing = false;
    }
  }

  async function cancel() {
    try {
      await api.cancelEngineOp();
    } catch {
      // The write ends on its own; the error, if any, arrives with it.
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={() => !writing && onClose()}>
  <div class="dialog" onclick={(e) => e.stopPropagation()}>
    <div class="head">
      {m.ms_head()}
      <button class="x" onclick={onClose} disabled={writing} title={m.common_close()}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
      </button>
    </div>

    <div class="body">
      <div class="intro">
        {m.ms_intro()} <code>scripts/build-linux-media.sh</code>. {m.ms_intro_write()}
      </div>

      <div class="field">
        <span class="lbl">{m.ms_iso()}</span>
        <input type="text" bind:value={iso} placeholder="own-backup-media_…_amd64.iso"
               spellcheck="false" disabled={writing || !!report} />
        <button class="btn outline" onclick={chooseIso} disabled={writing || !!report}>
          {m.common_browse()}
        </button>
      </div>

      <div class="sticks-head">
        <span class="lbl">{m.ms_stick()}</span>
        <button class="lnk" onclick={refresh} disabled={loading || writing}>{m.common_refresh()}</button>
      </div>
      {#if listError}
        <div class="err">{listError}</div>
      {:else if loading && sticks.length === 0}
        <div class="muted">{m.ms_looking()}</div>
      {:else if sticks.length === 0}
        <div class="muted">{m.ms_no_sticks()}</div>
      {:else}
        <div class="sticks">
          {#each sticks as s (s.device)}
            <label class="stick" class:busy={s.mounted.length > 0}>
              <input type="radio" name="stick" checked={device === s.device}
                     onchange={() => choose(s.device)} disabled={writing || !!report} />
              <span class="s-name">{s.model || s.device}</span>
              <span class="s-dev">{s.device}</span>
              <span class="s-bus">{transport(s.transport)}</span>
              <span class="s-size">{formatBytes(s.size_bytes)}</span>
              {#if s.mounted.length > 0}
                <span class="s-mounted">{m.ms_mounted({ mounts: s.mounted.join(', ') })}</span>
              {/if}
            </label>
          {/each}
        </div>
      {/if}

      {#if chosen && chosen.mounted.length === 0 && !report}
        <label class="confirm">
          <input type="checkbox" bind:checked={confirmed} disabled={writing} />
          <span>{m.ms_erase_confirm({ name: stickName(chosen), device: chosen.device })}</span>
        </label>
      {/if}

      {#if progress && (writing || report)}
        <div class="queue">
          {#each progress.stages as st, i}
            {@const done = i < progress.stage_index || progress.finished}
            {@const now = i === progress.stage_index && !progress.finished}
            <div class="stage" class:done class:now>
              <span class="mark">
                {#if done}
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
                {:else if now}
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M9 5l7 7-7 7" /></svg>
                {/if}
              </span>
              <span class="st-title">{st.title}</span>
              <span class="st-bar">
                <span class="st-fill" style="width: {done ? 100 : now ? progress.percent : 0}%"></span>
              </span>
            </div>
          {/each}
        </div>
      {/if}

      {#if error}<div class="err">{error}</div>{/if}

      {#if report}
        <div class="report">
          <div class="rep-head">{m.ms_rep_ready()}</div>
          <div class="rep-row"><span>{m.ms_stick()}</span><span>{report.device}</span></div>
          <div class="rep-row">
            <span>{m.ms_rep_image()}</span>
            <span>{report.volume_id} · {formatBytes(report.iso_bytes)} · {m.ms_rep_verified()}</span>
          </div>
          <div class="rep-row">
            <span>{m.ms_rep_logs()}</span>
            <span>
              {#if report.log_partition}
                {m.ms_rep_logs_on({ partition: report.log_partition })}
              {:else}
                {m.ms_rep_logs_memory({ why: report.log_skipped ?? '' })}
              {/if}
            </span>
          </div>
          <div class="rep-note">{m.ms_rep_note()}</div>
        </div>
      {/if}
    </div>

    <div class="foot">
      {#if report}
        <button class="btn primary" onclick={onClose}>{m.mb_done()}</button>
      {:else if writing}
        <button class="btn outline" onclick={cancel}>{m.common_cancel()}</button>
        <button class="btn primary" disabled>{m.ms_writing()}</button>
      {:else}
        <button class="btn outline" onclick={onClose}>{m.common_close()}</button>
        <button class="btn primary" onclick={write} disabled={!canWrite}>{m.ms_write()}</button>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 250; padding: 24px; }
  .dialog { width: 680px; max-width: 96vw; max-height: 92vh; background: var(--card-bg); border: 1px solid var(--border); border-radius: 12px; box-shadow: var(--shadow-lg); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; font-weight: 600; color: var(--head); border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
  .x { background: none; border: none; border-radius: 4px; color: var(--text-muted); cursor: pointer; padding: 2px 6px; }
  .x svg { display: block; }
  .x:hover:not(:disabled) { color: var(--head); background: var(--track); }
  .body { padding: 16px 18px; overflow: auto; flex: 1; min-height: 0; }
  .intro { font-size: 13px; color: var(--text); line-height: 1.6; }
  .intro code { font-family: var(--font-mono); font-size: 12px; }
  .lbl { font-size: 12px; color: var(--accent-text); }
  .field { display: grid; grid-template-columns: auto 1fr auto; gap: 10px; align-items: center; margin-top: 16px; }
  .field input { min-width: 0; background: var(--input-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 7px 10px; font-size: 13px; color: var(--text); font-family: var(--font); }
  .field input::placeholder { color: var(--text-muted); }
  .field input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
  .field input:disabled { background: var(--sunken); border-color: var(--border); color: var(--text-muted); }
  .sticks-head { display: flex; justify-content: space-between; align-items: center; margin-top: 16px; margin-bottom: 6px; }
  .lnk { background: none; border: none; color: var(--accent-text); font-size: 12px; cursor: pointer; padding: 0; font-family: var(--font); }
  .lnk:hover:not(:disabled) { text-decoration: underline; }
  .lnk:disabled { color: var(--text-muted); cursor: default; }
  .sticks { border: 1px solid var(--border); border-radius: 6px; }
  .stick { display: grid; grid-template-columns: 20px 1fr auto auto 80px; gap: 10px; align-items: center; padding: 7px 10px; font-size: 13px; color: var(--head); cursor: pointer; }
  .stick:hover { background: var(--panel2); }
  .stick.busy { color: var(--text-muted); }
  .s-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .s-dev, .s-bus { font-size: 12px; color: var(--text-muted); }
  .s-size { text-align: right; font-size: 12px; color: var(--text-muted); }
  .s-mounted { grid-column: 2 / -1; font-size: 12px; color: var(--warning-text); }
  .confirm { display: flex; align-items: flex-start; gap: 9px; margin-top: 14px; padding: 10px 12px; font-size: 13px; color: var(--error-text); background: var(--error-soft); border: 1px solid var(--error-line); border-radius: 8px; cursor: pointer; line-height: 1.45; }
  .confirm input { margin-top: 2px; flex-shrink: 0; }
  .muted { font-size: 13px; color: var(--text-muted); padding: 6px 0; }
  .err { font-size: 12px; color: var(--error-text); margin-top: 12px; word-break: break-word; }
  .queue { margin-top: 16px; border: 1px solid var(--border); border-radius: 6px; padding: 10px 12px; }
  .stage { display: grid; grid-template-columns: 16px minmax(170px, auto) 1fr; gap: 10px; align-items: center; padding: 3px 0; font-size: 12px; color: var(--text-muted); }
  .st-title { white-space: nowrap; }
  .stage.now, .stage.done { color: var(--head); }
  .mark { color: var(--success); }
  .mark svg { display: block; }
  .st-bar { position: relative; height: 16px; background: var(--track); border-radius: 3px; overflow: hidden; }
  .st-fill { display: block; height: 100%; background: var(--success); transition: width .25s; }
  .report { margin-top: 14px; border-radius: 8px; padding: 12px 14px; background: var(--accent-soft); }
  .rep-head { font-size: 14px; font-weight: 600; color: var(--head); margin-bottom: 8px; }
  .rep-row { display: flex; gap: 12px; font-size: 13px; margin-bottom: 4px; }
  .rep-row > span:first-child { flex: 0 0 90px; color: var(--accent-text); font-size: 12px; }
  .rep-row > span:last-child { color: var(--head); min-width: 0; word-break: break-word; }
  .rep-note { font-size: 12px; color: var(--text-muted); margin-top: 9px; line-height: 1.5; }
  .foot { display: flex; justify-content: flex-end; gap: 10px; padding: 12px 18px; border-top: 1px solid var(--border); background: var(--panel2); border-radius: 0 0 11px 11px; flex-shrink: 0; }
  .btn { border-radius: 6px; padding: 8px 18px; font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid var(--border-strong); background: var(--card-bg); color: var(--text); transition: background-color .15s ease; }
  .btn:hover:not(:disabled) { background: var(--panel2); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn.primary:hover:not(:disabled) { background: var(--primary-hover); border-color: var(--primary-hover); }
  .btn:disabled { opacity: .45; cursor: default; }
</style>
