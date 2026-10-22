<script lang="ts">
  // Linux: the boot medium built by the service, the way the Windows one is —
  // the program makes the ISO itself and saves it into a folder the person
  // chooses; a stick is written only when they tick «Записать на флешку» and
  // pick one. The medium is Debian 13 with exactly the installed program
  // (the service packs the installed package; a changed program file stops
  // it), built with live-build as root in /var/cache/own-backup/media.
  //
  // Writing an ISO that already exists stays available as a link: that is
  // MediaStickModal, opened by the card.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api, errorText } from '../lib/api';
  import { pickPath } from '../lib/pickPath.svelte';
  import { formatBytes } from '../lib/format';
  import type {
    LinuxMediaReport,
    LinuxMediaStatus,
    MediaProgress,
    MediaStick,
    MediaStickIdentity,
  } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { examplePath } from '../lib/paths';

  interface Props {
    onClose: () => void;
    /** Open the writer of a ready ISO instead. */
    onWriteReady: () => void;
  }
  let { onClose, onWriteReady }: Props = $props();

  let status = $state<LinuxMediaStatus | null>(null);
  let statusError: string | null = $state(null);
  let checking = $state(false);
  let clearing = $state(false);
  let cleared: string | null = $state(null);

  let outputDir = $state('');
  let toStick = $state(false);
  let sticks: MediaStick[] = $state([]);
  let loadingSticks = $state(false);
  let stickError: string | null = $state(null);
  let device = $state('');
  let confirmed = $state(false);

  let building = $state(false);
  let cancelling = $state(false);
  let progress: MediaProgress | null = $state(null);
  let report = $state<LinuxMediaReport | null>(null);
  let error: string | null = $state(null);

  const chosen = $derived(sticks.find((s) => s.device === device) ?? null);
  const finished = $derived(!!report);
  const stickReady = $derived(!toStick || (!!chosen && chosen.mounted.length === 0 && confirmed));
  const canStart = $derived(
    !building && !report && !!status && status.problems.length === 0 &&
    outputDir.trim().length > 0 && stickReady,
  );

  // The stage clock, as in the Windows dialog: a step measured in minutes
  // with a clock that ticks reads as working, not hung.
  let elapsed = $state(0);
  let stageStarted = 0;
  $effect(() => {
    const stage = progress?.stage_index ?? -1;
    if (stage < 0 || progress?.finished) {
      elapsed = 0;
      return;
    }
    stageStarted = performance.now();
    elapsed = 0;
    const id = setInterval(() => {
      elapsed = Math.floor((performance.now() - stageStarted) / 1000);
    }, 1000);
    return () => clearInterval(id);
  });
  const clock = (s: number) => `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;

  onMount(() => {
    check();
    api
      .defaultMediaFolder()
      .then((d) => {
        if (!outputDir) outputDir = d;
      })
      .catch(() => {});
    const un = listen<MediaProgress>('media-progress', (e) => {
      if (building) progress = e.payload;
    });
    return () => { un.then((f) => f()); };
  });

  async function check() {
    checking = true;
    statusError = null;
    try {
      status = await api.linuxMediaStatus();
    } catch (e) {
      statusError = errorText(e);
    } finally {
      checking = false;
    }
  }

  async function clearCache() {
    clearing = true;
    cleared = null;
    try {
      const r = await api.clearLinuxMediaCache();
      cleared = m.lmb_cache_cleared({ size: formatBytes(r.freed_bytes) });
    } catch (e) {
      cleared = errorText(e);
    } finally {
      clearing = false;
      check();
    }
  }

  function identity(s: MediaStick): MediaStickIdentity {
    return { model: s.model, serial: s.serial, size_bytes: s.size_bytes, dev: s.dev };
  }

  function sameStick(a: MediaStick, b: MediaStick): boolean {
    return a.model === b.model && a.serial === b.serial && a.size_bytes === b.size_bytes && a.dev === b.dev;
  }

  async function refreshSticks() {
    loadingSticks = true;
    stickError = null;
    const before = chosen;
    try {
      sticks = await api.listMediaSticks();
      const now = sticks.find((s) => s.device === device);
      // A stick that went away takes its tick with it; so does another stick
      // that took its name.
      if (!now) {
        device = sticks.length === 1 ? sticks[0].device : '';
        confirmed = false;
      } else if (before && !sameStick(before, now)) {
        confirmed = false;
      }
    } catch (e) {
      stickError = errorText(e);
    } finally {
      loadingSticks = false;
    }
  }

  function setToStick(on: boolean) {
    toStick = on;
    confirmed = false;
    if (on) refreshSticks();
  }

  function choose(d: string) {
    if (building || report) return;
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

  async function chooseFolder() {
    const dir = await pickPath({ directory: true, title: m.mb_dest() });
    if (dir) outputDir = dir;
  }

  async function start() {
    if (!canStart) return;
    building = true;
    cancelling = false;
    error = null;
    report = null;
    progress = null;
    try {
      report = await api.buildLinuxMedia({
        output_dir: outputDir.trim(),
        stick_device: toStick && chosen ? chosen.device : '',
        stick_expect: toStick && chosen ? identity(chosen) : null,
        deb_path: '',
      });
    } catch (e) {
      error = errorText(e);
      if (toStick) {
        // What the stick holds now is not what the list said.
        confirmed = false;
        refreshSticks();
      }
      check();
    } finally {
      building = false;
      cancelling = false;
    }
  }

  async function cancel() {
    cancelling = true;
    try {
      await api.cancelEngineOp();
    } catch {
      // The build ends on its own; the error, if any, arrives with it.
    }
  }
</script>

{#snippet warnIcon()}
  <svg class="w-icon" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--warning)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5l9 16H3z" /><path d="M12 10v4M12 17h.01" /></svg>
{/snippet}

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={() => !building && onClose()}>
  <div class="dialog" onclick={(e) => e.stopPropagation()}>
    <div class="head">
      {m.mb_head_linux()}
      <button class="x" onclick={onClose} disabled={building} title={m.common_close()}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
      </button>
    </div>

    <div class="body">
      <div class="intro">
        {m.lmb_intro()}
        {#if status?.installed_version}
          {m.lmb_intro_version({ version: status.installed_version })}
        {/if}
      </div>
      <div class="needs">{m.lmb_needs()}</div>

      {#if statusError}
        <div class="warn-box">{@render warnIcon()}<span>{statusError}</span></div>
      {:else if status && status.problems.length > 0 && !building && !finished}
        {#each status.problems as p}
          <div class="warn-box">{@render warnIcon()}<span>{p}</span></div>
        {/each}
        <button class="lnk again" onclick={check} disabled={checking}>
          {checking ? m.lmb_checking() : m.lmb_check_again()}
        </button>
      {/if}

      {#if status && status.cache_bytes > 0 && !building}
        <div class="cache">
          <span>{m.lmb_cache({ size: formatBytes(status.cache_bytes), path: status.cache_dir })}</span>
          <button class="lnk" onclick={clearCache} disabled={clearing}>
            {clearing ? m.lmb_cache_clearing() : m.lmb_cache_clear()}
          </button>
        </div>
      {/if}
      {#if cleared}<div class="muted">{cleared}</div>{/if}

      {#if !building && !finished}
        <label class="opt">
          <input type="checkbox" checked={toStick}
                 onchange={(e) => setToStick((e.currentTarget as HTMLInputElement).checked)} />
          <span>
            {m.lmb_to_stick()}
            <span class="opt-note">{m.lmb_to_stick_note()}</span>
          </span>
        </label>

        {#if toStick}
          <div class="sticks-head">
            <span class="lbl">{m.ms_stick()}</span>
            <button class="lnk" onclick={refreshSticks} disabled={loadingSticks}>{m.common_refresh()}</button>
          </div>
          {#if stickError}
            <div class="err">{stickError}</div>
          {:else if loadingSticks && sticks.length === 0}
            <div class="muted">{m.ms_looking()}</div>
          {:else if sticks.length === 0}
            <div class="muted">{m.ms_no_sticks()}</div>
          {:else}
            <div class="sticks">
              {#each sticks as s (s.device)}
                <label class="stick" class:busy={s.mounted.length > 0}>
                  <input type="radio" name="lmb-stick" checked={device === s.device}
                         onchange={() => choose(s.device)} />
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
          {#if chosen && chosen.mounted.length === 0}
            <label class="confirm">
              <input type="checkbox" bind:checked={confirmed} />
              <span>{m.ms_erase_confirm({ name: stickName(chosen), device: chosen.device })}</span>
            </label>
          {/if}
        {/if}

        <button class="lnk ready" onclick={onWriteReady}>{m.lmb_write_ready()}</button>
      {/if}

      {#if progress && (building || finished)}
        <div class="queue">
          {#each progress.stages as st, i}
            {@const done = i < progress.stage_index || progress.finished}
            {@const now = i === progress.stage_index && !progress.finished}
            <!-- live-build's download and unpack cannot measure themselves:
                 the running stage animates until it has a real number. -->
            {@const waiting = now && progress.percent === 0}
            <div class="stage" class:done class:now>
              <span class="mark">
                {#if done}
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
                {:else if now}
                  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M9 5l7 7-7 7" /></svg>
                {/if}
              </span>
              <span class="st-title">{st.title}</span>
              <span class="st-bar" class:waiting>
                {#if waiting}
                  <span class="st-run"></span>
                {:else}
                  <span class="st-fill" style="width: {done ? 100 : now ? progress.percent : 0}%"></span>
                {/if}
                {#if now}
                  <span class="st-detail">
                    {progress.detail}{#if elapsed > 2}<span class="st-clock">{clock(elapsed)}</span>{/if}
                  </span>
                {/if}
              </span>
            </div>
          {/each}
        </div>
      {/if}

      {#if error}<div class="err">{error}</div>{/if}

      {#if report}
        <div class="report">
          <div class="rep-head">{m.mb_rep_ready()}</div>
          <div class="rep-row"><span>{m.mb_rep_file()}</span><span>{report.iso_path}</span></div>
          <div class="rep-row"><span>{m.mb_rep_size()}</span><span>{formatBytes(report.iso_bytes)}</span></div>
          <div class="rep-row">
            <span>{m.lmb_rep_program()}</span>
            <span>{report.version} · {report.from_installed ? m.lmb_rep_installed() : m.lmb_rep_given()}</span>
          </div>
          {#if report.stick}
            <div class="rep-row">
              <span>{m.ms_stick()}</span>
              <span>{report.stick.device} · {report.stick.volume_id} · {m.ms_rep_verified()}</span>
            </div>
            <div class="rep-row">
              <span>{m.ms_rep_logs()}</span>
              <span>
                {#if report.stick.log_partition}
                  {m.ms_rep_logs_on({ partition: report.stick.log_partition })}
                {:else}
                  {m.ms_rep_logs_memory({ why: report.stick.log_skipped ?? '' })}
                {/if}
              </span>
            </div>
          {/if}
          <div class="rep-row">
            <span>{m.lmb_rep_cache()}</span>
            <span>{formatBytes(report.cache_bytes)} · {report.cache_dir}</span>
          </div>
          {#each report.warnings as w}<div class="warn">{w}</div>{/each}
          <div class="rep-note">{report.stick ? m.ms_rep_note() : m.lmb_rep_note()}</div>
        </div>
      {/if}
    </div>

    <div class="dest">
      <span class="lbl">{m.mb_dest()}</span>
      <input type="text" bind:value={outputDir} placeholder={examplePath('folder')}
             spellcheck="false" disabled={building || finished} />
      <button class="btn outline" onclick={chooseFolder} disabled={building || finished}>
        {m.common_browse()}
      </button>
      {#if finished}
        <button class="btn primary" onclick={onClose}>{m.mb_done()}</button>
      {:else if building}
        <button class="btn outline" onclick={cancel} disabled={cancelling}>
          {cancelling ? m.lmb_cancelling() : m.common_cancel()}
        </button>
      {:else}
        <button class="btn primary" onclick={start} disabled={!canStart}>{m.mb_start()}</button>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 250; padding: 24px; }
  .dialog { width: 760px; max-width: 96vw; max-height: 92vh; background: var(--card-bg); border: 1px solid var(--border); border-radius: 12px; box-shadow: var(--shadow-lg); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; font-weight: 600; color: var(--head); border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
  .x { background: none; border: none; border-radius: 4px; color: var(--text-muted); cursor: pointer; padding: 2px 6px; }
  .x svg { display: block; }
  .x:hover:not(:disabled) { color: var(--head); background: var(--track); }
  .body { padding: 16px 18px; overflow: auto; flex: 1; min-height: 0; }
  .intro { font-size: 13px; color: var(--text); line-height: 1.6; }
  .needs { margin-top: 10px; font-size: 12px; color: var(--text-muted); line-height: 1.5; }
  .warn-box { display: flex; align-items: flex-start; gap: 10px; font-size: 12px; margin-top: 10px; line-height: 1.45; padding: 10px 12px; border-radius: 8px; background: var(--warning-soft); border: 1px solid var(--warning-line); color: var(--warning-text); word-break: break-word; }
  .w-icon { flex: none; margin-top: 1px; }
  .cache { display: flex; gap: 14px; align-items: baseline; margin-top: 10px; font-size: 12px; color: var(--text-muted); }
  .opt { display: flex; align-items: flex-start; gap: 9px; font-size: 13px; color: var(--head); margin-top: 16px; cursor: pointer; }
  .opt input { margin-top: 2px; flex-shrink: 0; }
  .opt-note { display: block; font-size: 12px; color: var(--text-muted); line-height: 1.4; margin-top: 2px; }
  .lbl { font-size: 12px; color: var(--accent-text); }
  .lnk { background: none; border: none; color: var(--accent-text); font-size: 12px; cursor: pointer; padding: 0; font-family: var(--font); }
  .lnk:hover:not(:disabled) { text-decoration: underline; }
  .lnk:disabled { color: var(--text-muted); cursor: default; }
  .lnk.again { margin-top: 6px; }
  .lnk.ready { display: block; margin-top: 16px; }
  .sticks-head { display: flex; justify-content: space-between; align-items: center; margin-top: 12px; margin-bottom: 6px; }
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
  .stage { display: grid; grid-template-columns: 16px minmax(190px, auto) 1fr; gap: 10px; align-items: center; padding: 3px 0; font-size: 12px; color: var(--text-muted); }
  .st-title { white-space: nowrap; }
  .stage.now, .stage.done { color: var(--head); }
  .mark { color: var(--success); }
  .mark svg { display: block; }
  .st-bar { position: relative; height: 16px; background: var(--track); border-radius: 3px; overflow: hidden; }
  .st-fill { display: block; height: 100%; background: var(--success); transition: width .25s; }
  .st-run { position: absolute; inset: 0 auto 0 0; width: 32%; background: var(--success); opacity: .75; border-radius: 3px; animation: st-slide 1.4s ease-in-out infinite; }
  @keyframes st-slide { 0% { transform: translateX(-105%); } 100% { transform: translateX(320%); } }
  .st-clock { margin-left: 8px; opacity: .75; font-variant-numeric: tabular-nums; }
  .st-detail { position: absolute; right: 8px; top: 0; line-height: 16px; font-size: 11px; color: var(--head); max-width: 70%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .warn { font-size: 12px; color: var(--error-text); margin-top: 8px; padding: 8px 10px; line-height: 1.45; border: 1px solid var(--error-line); border-left: 3px solid var(--error); border-radius: 8px; background: var(--error-soft); }
  .report { margin-top: 14px; border-radius: 8px; padding: 12px 14px; background: var(--accent-soft); }
  .rep-head { font-size: 14px; font-weight: 600; color: var(--head); margin-bottom: 8px; }
  .rep-row { display: flex; gap: 12px; font-size: 13px; margin-bottom: 4px; }
  .rep-row > span:first-child { flex: 0 0 90px; color: var(--accent-text); font-size: 12px; }
  .rep-row > span:last-child { color: var(--head); min-width: 0; word-break: break-word; }
  .rep-note { font-size: 12px; color: var(--text-muted); margin-top: 9px; line-height: 1.5; }
  .dest { display: grid; grid-template-columns: auto 1fr auto auto; gap: 10px; align-items: center; padding: 12px 18px; border-top: 1px solid var(--border); flex-shrink: 0; background: var(--panel2); border-radius: 0 0 11px 11px; }
  .dest input { min-width: 0; background: var(--input-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 7px 10px; font-size: 13px; color: var(--text); font-family: var(--font); }
  .dest input::placeholder { color: var(--text-muted); }
  .dest input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
  .dest input:disabled { background: var(--sunken); border-color: var(--border); color: var(--text-muted); }
  .btn { border-radius: 6px; padding: 8px 18px; font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid var(--border-strong); background: var(--card-bg); color: var(--text); transition: background-color .15s ease; }
  .btn:hover:not(:disabled) { background: var(--panel2); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn.primary:hover:not(:disabled) { background: var(--primary-hover); border-color: var(--primary-hover); }
  .btn:disabled { opacity: .45; cursor: default; }
</style>
