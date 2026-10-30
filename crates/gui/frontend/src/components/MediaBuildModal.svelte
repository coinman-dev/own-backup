<script lang="ts">
  // The build dialog of the Windows medium: what goes on it, which drivers,
  // where the ISO lands — and then the queue of stages while it happens. The
  // Linux medium has its own dialog (LinuxMediaBuildModal).
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api } from '../lib/api';
  import { pickPath } from '../lib/pickPath.svelte';
  import { formatBytes } from '../lib/format';
  import type {
    BaseImage,
    DriverPackage,
    MediaBuildReport,
    MediaProgress,
    MediaTool,
  } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { examplePath, joinPath } from '../lib/paths';

  interface Props {
    /** Chosen base image. */
    base: BaseImage | null;
    onClose: () => void;
  }
  let { base, onClose }: Props = $props();

  let drivers: DriverPackage[] = $state([]);
  let selected = $state(new Set<string>());
  let includeDrivers = $state(true);
  // Off by default: the medium works without them, and a GPU driver is often
  // bigger than everything else on it together — held in RAM from boot.
  let loadingDrivers = $state(false);
  let driverError: string | null = $state(null);

  // Third-party programs found on THIS machine. Nothing is downloaded: the
  // copy is made from the user's own installation, which is why they are
  // offered rather than added quietly. Off by default for the same reason.
  // One switch for all of them: two small programs are not worth a row each.
  let tools: MediaTool[] = $state([]);
  let includeTools = $state(false);

  // Compressing the runtime into the image is the default: the ISO is what the
  // user carries around, and the memory it costs only matters on a machine with
  // little of it.
  let runtimeInImage = $state(true);

  let outputDir = $state('');
  let building = $state(false);
  let progress: MediaProgress | null = $state(null);
  let report: MediaBuildReport | null = $state(null);
  let error: string | null = $state(null);

  // Only the classes a rescue medium can use are offered. The rest — display,
  // audio, printers — is what turned the first medium into 2.2 GB, and no one
  // scrolling a list of ninety packages wants to find that out by hand.
  const offered = $derived(drivers.filter((d) => d.recommended));
  const chosenBytes = $derived(
    offered.filter((d) => selected.has(d.inf)).reduce((s, d) => s + d.size_bytes, 0),
  );
  // Disks and network on Windows' own drivers: nothing to pick, but they are
  // this machine's hardware and the user should see that they were looked at.
  const inbox = $derived(drivers.filter((d) => d.inbox));
  /** The build is over: the button becomes "Готово" and closes the dialog. */
  const finished = $derived(!!report);

  // How long the current stage has been running. The two DISM steps take
  // minutes, and a clock that ticks is the difference between "working" and
  // "hung" — especially while the stage has no percentage of its own yet.
  let elapsed = $state(0);
  let stageStarted = 0;
  $effect(() => {
    // Re-runs whenever the stage changes; the interval is replaced with it.
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
  // Not while the hardware is still being polled: the driver list is part of
  // what "Начать" would act on, and starting without it would quietly build a
  // medium with no drivers at all.
  const canStart = $derived(
    !building && !!base && outputDir.trim().length > 0 &&
    !(includeDrivers && loadingDrivers),
  );

  onMount(() => {
    loadDrivers();
    // Quietly: a machine with neither installed is the ordinary case, and an
    // empty section says so without an error.
    api.listMediaTools().then((t) => (tools = t)).catch(() => {});
    // Default destination: where the program itself is, which is the folder the
    // user already knows how to find.
    // `getConfig` hands back the whole config as unknown values; this one is a
    // path, so say so rather than letting `{}` flow into a string.
    api
      .getConfig()
      .then((c) => {
        if (!outputDir) outputDir = typeof c.data_dir === 'string' ? c.data_dir : '';
      })
      .catch(() => {});
    const un = listen<MediaProgress>('media-progress', (e) => (progress = e.payload));
    return () => { un.then((f) => f()); };
  });

  async function loadDrivers() {
    loadingDrivers = true;
    driverError = null;
    try {
      drivers = await api.listDriverPackages();
      selected = new Set(drivers.filter((d) => d.recommended).map((d) => d.inf));
    } catch (e) {
      driverError = String(e);
    } finally {
      loadingDrivers = false;
    }
  }

  function toggle(inf: string) {
    const next = new Set(selected);
    if (next.has(inf)) next.delete(inf); else next.add(inf);
    selected = next;
  }

  async function chooseFolder() {
    const dir = await pickPath({ writable: true, directory: true, title: m.mb_dest() });
    if (dir) outputDir = dir;
  }

  async function start() {
    if (!canStart) return;
    building = true;
    error = null;
    report = null;
    progress = null;
    try {
      report = await api.buildBootMedia({
        base_wim: base?.path ?? '',
        output_iso: joinPath(outputDir, 'OwnBackup.iso'),
        drivers: includeDrivers ? [...selected] : [],
        extra_drivers: '',
        tools: includeTools ? tools.map((t) => t.id) : [],
        // The desktop is the shell. The way back to cmd, for a machine where
        // the desktop does not come up, is `own-backup-cli media build
        // --classic-console`; the dialog no longer offers it (2026-09-26).
        classic_console: false,
        runtime_in_image: runtimeInImage,
      });
    } catch (e) {
      error = String(e);
    } finally {
      building = false;
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={() => !building && onClose()}>
  <div class="dialog" onclick={(e) => e.stopPropagation()}>
    <div class="head">
      {m.mb_head_windows()}
      <button class="x" onclick={onClose} disabled={building} title={m.common_close()}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
      </button>
    </div>

    <div class="body">
      <!-- What goes in -->
      <div class="intro">
        {base
          ? m.mb_intro_win_build({ build: base.build, language: base.language })
          : m.mb_intro_win()}
        {m.mb_intro_win_boot()}
      </div>

      <!-- Options. Hidden once the build starts: a set of checkboxes that can no
           longer be changed is only in the way of the thing being watched. -->
      {#if !building && !finished}
        <label class="opt">
          <input type="checkbox" bind:checked={runtimeInImage} disabled={building} />
          <span>
            {m.mb_compress_runtime()}
            <span class="opt-note">
              {runtimeInImage ? m.mb_compress_on_note() : m.mb_compress_off_note()}
            </span>
          </span>
        </label>

        <label class="opt">
          <input type="checkbox" bind:checked={includeDrivers} disabled={building} />
          <span>
            {m.mb_add_drivers()}
            <span class="opt-note">{m.mb_add_drivers_note()}</span>
          </span>
        </label>

        {#if includeDrivers}
          <div class="drivers">
            {#if loadingDrivers}
              <!-- Polling the device list takes a couple of seconds on a machine
                   with a lot of hardware, and a still line of text reads as a
                   hang. The bar says it is working without pretending to know
                   how far along it is. -->
              <div class="scan">
                <span class="scan-bar"><span class="scan-run"></span></span>
                <span class="muted">{m.mb_polling_hw()}</span>
              </div>
            {:else if driverError}
              <div class="err">{driverError}</div>
            {:else if offered.length === 0 && inbox.length === 0}
              <div class="muted">{m.mb_no_hw()}</div>
            {:else if offered.length === 0}
              <!-- Nothing to choose: the rows below say why, device by device. -->
            {:else}
              <div class="drv-head">
                <span>
                  {m.mb_drv_summary({
                    found: offered.length,
                    chosen: selected.size,
                    size: formatBytes(chosenBytes),
                  })}
                </span>
                <span class="drv-actions">
                  <button class="lnk" onclick={() => (selected = new Set(offered.map((d) => d.inf)))}>{m.mb_select_all()}</button>
                  <button class="lnk" onclick={() => (selected = new Set())}>{m.mb_select_none()}</button>
                </span>
              </div>
              <div class="drv-list">
                {#each offered as d (d.inf)}
                  <label class="drv" title={d.original_name || d.inf}>
                    <input type="checkbox" checked={selected.has(d.inf)}
                           onchange={() => toggle(d.inf)} disabled={building} />
                    <!-- The device's own name, not the INF's: "Intel(R) Ethernet
                         Connection I219-V" is recognisable, "oem34.inf" is not. -->
                    <span class="drv-name">{d.device || d.original_name || d.inf}</span>
                    <span class="drv-class">{d.role || d.class_name}</span>
                    <span class="drv-prov">{d.provider}</span>
                    <!-- A package that is not in this machine's driver store
                         (the hypervisor's guest tools) has no size until it is
                         unpacked; a dash says so instead of "0 Б". -->
                    <span class="drv-size">{d.size_bytes > 0 ? formatBytes(d.size_bytes) : '—'}</span>
                  </label>
                {/each}
              </div>
            {/if}
            {#if !loadingDrivers && !driverError && inbox.length > 0}
              <div class="inbox">
                <div class="muted">
                  {offered.length === 0 ? m.mb_inbox_all() : m.mb_inbox_rest()}
                  {m.mb_inbox_check()}
                </div>
                {#each inbox as d (d.inf)}
                  <div class="drv inbox-row" title={m.mb_inbox_title({ device: d.device, role: d.role, inf: d.inf })}>
                    <span></span><!-- the checkbox column: nothing to tick -->
                    <span class="drv-name">{d.device || d.inf}</span>
                    <span class="drv-class">{d.role}</span>
                    <span class="drv-prov">{d.inf}</span>
                    <span class="drv-size">{m.mb_builtin()}</span>
                  </div>
                {/each}
              </div>
            {/if}
          </div>
        {/if}

        <!-- Third-party programs from this machine. Offered, never assumed:
             they are someone else's software, and the copy is made from the
             user's own installation into their own medium. -->
        {#if tools.length > 0}
          <label class="opt">
            <input type="checkbox" bind:checked={includeTools} disabled={building} />
            <span>
              {m.mb_tools()}
              <span class="opt-note">{m.mb_tools_note()}</span>
              <span class="tools-line">
                {tools
                  .map((t) =>
                    m.mb_tool_line({
                      name: t.name,
                      size: formatBytes(t.size_bytes),
                      files: m.mb_files({ count: t.files }),
                    }),
                  )
                  .join(', ')}
              </span>
            </span>
          </label>
        {/if}

        <!-- Not an option: the screen on the medium is the firmware's business,
             and the one setting that decides it is in the firmware. Video
             adapter drivers used to be offered here instead; the GeForce one
             left WinPE with no picture at all, while switching CSM off gave the
             monitor's recommended mode with no driver (2026-09-18). -->
        <div class="screen">
          <div class="screen-title">{m.mb_screen_title()}</div>
          {m.mb_screen_text()}
          <span class="screen-warn">{m.mb_screen_warn()}</span>
        </div>
      {/if}

      <!-- The queue -->
      {#if progress}
        <div class="queue">
          {#each progress.stages as st, i}
            {@const done = i < progress.stage_index || progress.finished}
            {@const now = i === progress.stage_index && !progress.finished}
            <!-- A stage that has not reported a percentage yet is not a stage
                 doing nothing: DISM's unpack sends its first number after a
                 while, and the clean-up measures gigabytes before it deletes
                 anything. A still bar at 0 % for a minute reads as a hang, so
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
                <!-- What the stage is doing sits at the far end of its own bar,
                     and only while it is doing it: once the stage is done the
                     words describe nothing. The clock beside it is the proof
                     that a step measured in minutes is still moving. -->
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
            <span>{m.mb_rep_base()}</span>
            <span>{report.base_name} · {m.mb_build({ build: report.base_build })} · {report.base_language}</span>
          </div>
          <div class="rep-row">
            <span>{m.mb_rep_drivers()}</span>
            <span>{report.drivers_added > 0 ? m.mb_pcs({ n: report.drivers_added }) : m.mb_drivers_none()}</span>
          </div>
          {#if report.tools_added > 0}
            <div class="rep-row">
              <span>{m.mb_rep_tools()}</span>
              <span>{m.mb_pcs({ n: report.tools_added })} · {formatBytes(report.tools_bytes)}</span>
            </div>
          {/if}
          {#if report.temp_bytes_freed > 0}
            <div class="rep-row">
              <span>{m.mb_rep_cleaned()}</span>
              <span>{m.mb_temp_freed({ size: formatBytes(report.temp_bytes_freed) })}</span>
            </div>
          {/if}
          {#each report.warnings as w}<div class="warn">{w}</div>{/each}
          <div class="rep-note">{m.mb_rep_note()}</div>
        </div>
      {/if}
    </div>

    <!-- Pinned: the destination and the button stay reachable however long the
         list above grows. -->
    <div class="dest">
      <span class="lbl">{m.mb_dest()}</span>
      <input type="text" bind:value={outputDir} placeholder={examplePath('folder')}
             spellcheck="false" disabled={building || finished} />
      <button class="btn outline" onclick={chooseFolder} disabled={building || finished}>
        {m.common_browse()}
      </button>
      {#if finished}
        <button class="btn primary" onclick={onClose}>{m.mb_done()}</button>
      {:else}
        <button class="btn primary" onclick={start} disabled={!canStart}
                title={includeDrivers && loadingDrivers ? m.mb_polling_title() : ''}>
          {building ? m.mb_building() : includeDrivers && loadingDrivers ? m.mb_polling_btn() : m.mb_start()}
        </button>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 250; padding: 24px; }
  .dialog { width: 760px; max-width: 96vw; max-height: 92vh; background: var(--card-bg); border: 1px solid var(--border); border-radius: 12px; box-shadow: var(--shadow-lg); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; font-weight: 600; color: var(--head); border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
  .x { background: none; border: none; border-radius: 4px; color: var(--text-muted); font-size: 15px; cursor: pointer; padding: 2px 6px; }
  .x svg { display: block; }
  .x:hover:not(:disabled) { color: var(--head); background: var(--track); }
  .body { padding: 16px 18px; overflow: auto; flex: 1; min-height: 0; }
  .intro { font-size: 13px; color: var(--text); line-height: 1.6; }
  .opt { display: flex; align-items: flex-start; gap: 9px; font-size: 13px; color: var(--head); margin-top: 16px; cursor: pointer; }
  .opt input { margin-top: 2px; flex-shrink: 0; }
  .opt-note { display: block; font-size: 12px; color: var(--text-muted); line-height: 1.4; margin-top: 2px; }
  .tools-line { display: block; font-size: 12px; color: var(--text-muted); margin-top: 4px; }

  .drivers { margin-top: 16px; }
  .screen { margin-top: 16px; font-size: 12px; color: var(--text-muted); line-height: 1.5; }
  .screen-title { font-size: 13px; color: var(--head); margin-bottom: 2px; }
  .screen-warn { display: block; margin-top: 4px; }
  .inbox { margin-top: 10px; }
  .inbox .muted { margin-bottom: 6px; }
  .inbox-row { cursor: default; color: var(--text-muted); }
  .inbox-row:hover { background: none; }
  .scan { display: flex; align-items: center; gap: 12px; padding: 8px 0; }
  .scan-bar { flex: 0 0 120px; height: 5px; background: var(--track); border-radius: 3px; overflow: hidden; }
  /* Indeterminate on purpose: the count is not known until the walk is done. */
  .scan-run { display: block; width: 40%; height: 100%; background: var(--success); border-radius: 3px; animation: scan 1.1s ease-in-out infinite; }
  @keyframes scan { 0% { transform: translateX(-100%); } 100% { transform: translateX(250%); } }
  .drv-head { display: flex; justify-content: space-between; align-items: center; font-size: 12px; color: var(--text-muted); margin-bottom: 6px; }
  .drv-actions { display: flex; gap: 12px; }
  .lnk { background: none; border: none; color: var(--accent-text); font-size: 12px; cursor: pointer; padding: 0; font-family: var(--font); }
  .lnk:hover { text-decoration: underline; }
  .drv-list { max-height: 220px; overflow: auto; border: 1px solid var(--border); border-radius: 6px; }
  .drv { display: grid; grid-template-columns: 20px 1fr 150px 1fr 80px; gap: 10px; align-items: center; padding: 6px 10px; font-size: 12px; cursor: pointer; color: var(--head); }
  .drv:hover { background: var(--panel2); }
  .drv-name, .drv-prov { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .drv-class { color: var(--text-muted); }
  .drv-prov { color: var(--text-muted); }
  .drv-size { text-align: right; color: var(--text-muted); }
  /* Pinned below the scrolling body, not inside it. */
  /* The dialog's footer: the design's --panel strip, rounded to sit inside
     the 12 px frame. */
  .dest { display: grid; grid-template-columns: auto 1fr auto auto; gap: 10px; align-items: center; padding: 12px 18px; border-top: 1px solid var(--border); flex-shrink: 0; background: var(--panel2); border-radius: 0 0 11px 11px; }
  .lbl { font-size: 12px; color: var(--accent-text); }
  .dest input { min-width: 0; background: var(--input-bg); border: 1px solid var(--border-strong); border-radius: 6px; padding: 7px 10px; font-size: 13px; color: var(--text); font-family: var(--font); }
  .dest input::placeholder { color: var(--text-muted); }
  .dest input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
  .dest input:disabled { background: var(--sunken); border-color: var(--border); color: var(--text-muted); }
  /* «Обзор…» is the design's secondary button, «Начать» / «Готово» the primary. */
  .btn { border-radius: 6px; padding: 8px 18px; font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid var(--border-strong); background: var(--card-bg); color: var(--text); transition: background-color .15s ease; }
  .btn:hover:not(:disabled) { background: var(--panel2); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn.primary:hover:not(:disabled) { background: var(--primary-hover); border-color: var(--primary-hover); }
  .btn:disabled { opacity: .45; cursor: default; }
  .queue { margin-top: 16px; border: 1px solid var(--border); border-radius: 6px; padding: 10px 12px; }
  /* The bar takes everything to the right of the title; the running stage's
     detail rides along with the title rather than holding a column of its own,
     which was empty on every other row and cut the bar short of the edge. */
  .stage { display: grid; grid-template-columns: 16px minmax(190px, auto) 1fr; gap: 10px; align-items: center; padding: 3px 0; font-size: 12px; color: var(--text-muted); }
  .st-title { white-space: nowrap; }
  .stage.now, .stage.done { color: var(--head); }
  .mark { color: var(--success); }
  .mark svg { display: block; }
  .st-bar { position: relative; height: 16px; background: var(--track); border-radius: 3px; overflow: hidden; }
  .st-fill { display: block; height: 100%; background: var(--success); transition: width .25s; }
  /* No number to show yet — the stripe says the step is alive without
     claiming a position it does not know. */
  .st-run { position: absolute; inset: 0 auto 0 0; width: 32%; background: var(--success); opacity: .75; border-radius: 3px; animation: st-slide 1.4s ease-in-out infinite; }
  @keyframes st-slide { 0% { transform: translateX(-105%); } 100% { transform: translateX(320%); } }
  .st-clock { margin-left: 8px; opacity: .75; font-variant-numeric: tabular-nums; }
  .st-detail { position: absolute; right: 8px; top: 0; line-height: 16px; font-size: 11px; color: var(--head); max-width: 70%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .err { font-size: 12px; color: var(--error-text); margin-top: 12px; word-break: break-word; }
  /* A warning is not a footnote: the build finished, but something the user
     asked for is not on the medium. Framed, so it is not read as prose. */
  .warn {
    font-size: 12px;
    color: var(--error-text);
    margin-top: 8px;
    padding: 8px 10px;
    line-height: 1.45;
    border: 1px solid var(--error-line);
    border-left: 3px solid var(--error);
    border-radius: 8px;
    background: var(--error-soft);
  }
  .muted { font-size: 13px; color: var(--text-muted); padding: 6px 0; }
  /* The finished medium: the design's neutral "done" summary. */
  .report { margin-top: 14px; border: 1px solid transparent; border-radius: 8px; padding: 12px 14px; background: var(--accent-soft); }
  .rep-head { font-size: 14px; font-weight: 600; color: var(--head); margin-bottom: 8px; }
  .rep-row { display: flex; gap: 12px; font-size: 13px; margin-bottom: 4px; }
  .rep-row > span:first-child { flex: 0 0 90px; color: var(--accent-text); font-size: 12px; }
  .rep-row > span:last-child { color: var(--head); min-width: 0; word-break: break-word; }
  .rep-note { font-size: 12px; color: var(--text-muted); margin-top: 9px; line-height: 1.5; }
</style>
