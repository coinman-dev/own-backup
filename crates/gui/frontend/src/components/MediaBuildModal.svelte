<script lang="ts">
  // The build dialog: what goes on the medium, which drivers, where the ISO
  // lands — and then the queue of stages while it happens.
  //
  // One dialog serves both bases. The Linux one has no engine behind it yet, so
  // its button is disabled and says so; everything above the button is the same
  // conversation, which is the point of writing it once.
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

  interface Props {
    kind: 'windows' | 'linux';
    /** Chosen base image; null for Linux, which has none yet. */
    base: BaseImage | null;
    onClose: () => void;
  }
  let { kind, base, onClose }: Props = $props();

  const isWindows = $derived(kind === 'windows');

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
    isWindows && !building && !!base && outputDir.trim().length > 0 &&
    !(includeDrivers && loadingDrivers),
  );

  onMount(() => {
    if (isWindows) {
      loadDrivers();
      // Quietly: a machine with neither installed is the ordinary case, and an
      // empty section says so without an error.
      api.listMediaTools().then((t) => (tools = t)).catch(() => {});
    }
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
    const dir = await pickPath({ directory: true, title: 'Куда сохранить ISO' });
    if (dir) outputDir = dir;
  }

  async function start() {
    if (!canStart) return;
    building = true;
    error = null;
    report = null;
    progress = null;
    const sep = outputDir.endsWith('\\') || outputDir.endsWith('/') ? '' : '\\';
    try {
      report = await api.buildBootMedia({
        base_wim: base?.path ?? '',
        output_iso: `${outputDir}${sep}OwnBackup.iso`,
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
      {isWindows
        ? 'Носитель на основе Windows Recovery Environment'
        : 'Носитель на основе Linux'}
      <button class="x" onclick={onClose} disabled={building} title="Закрыть">✕</button>
    </div>

    <div class="body">
      <!-- What goes in -->
      <div class="intro">
        {#if isWindows}
          В образ войдут: среда восстановления Windows
          {#if base}(сборка {base.build}, {base.language}){/if},
          программа Own Backup (интерфейс, движок, командная строка),
          рантайм WebView2 для интерфейса и системные библиотеки, которых нет в
          WinPE. Носитель загружается сразу в программу; с него можно снять
          копию выключенной системы и восстановить её обратно.
        {:else}
          В образ войдут: минимальная система Linux, ядро с драйверами дисков и
          сети, программа Own Backup и её интерфейс. Такой носитель
          пригодится там, где нет Windows для сборки, а также для машин, чью
          разметку и файловые системы удобнее читать из Linux (ext4, btrfs,
          XFS, LUKS).
        {/if}
      </div>

      {#if !isWindows}
        <div class="soon">
          Сборка ещё не реализована — здесь пока только описание того, что
          войдёт в носитель.
        </div>
      {/if}

      <!-- Options. Hidden once the build starts: a set of checkboxes that can no
           longer be changed is only in the way of the thing being watched. -->
      {#if isWindows && !building && !finished}
        <label class="opt">
          <input type="checkbox" bind:checked={runtimeInImage} disabled={building} />
          <span>
            Сжимать WebView2 в образ
            <span class="opt-note">
              {runtimeInImage
                ? 'файл ISO примерно на 280 МБ меньше, но при загрузке рантайм занимает около 260 МБ оперативной памяти — WinPE держит образ в памяти целиком'
                : 'рантайм лежит на носителе несжатым: ISO больше, зато память при загрузке свободна — файлы читаются по мере надобности'}
            </span>
          </span>
        </label>

        <label class="opt">
          <input type="checkbox" bind:checked={includeDrivers} disabled={building} />
          <span>
            Добавить драйверы этого компьютера
            <span class="opt-note">
              без них носитель может не увидеть диски: именно поэтому
              «универсальные» носители не находят NVMe и RAID
            </span>
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
                <span class="muted">Опрос оборудования: диски и сетевые адаптеры…</span>
              </div>
            {:else if driverError}
              <div class="err">{driverError}</div>
            {:else if offered.length === 0 && inbox.length === 0}
              <div class="muted">
                Дисков и сетевых адаптеров, которым нужен драйвер, на этой машине
                не нашлось.
              </div>
            {:else if offered.length === 0}
              <!-- Nothing to choose: the rows below say why, device by device. -->
            {:else}
              <div class="drv-head">
                <span>
                  Найдено оборудования: {offered.length} · выбрано {selected.size} ·
                  {formatBytes(chosenBytes)}
                </span>
                <span class="drv-actions">
                  <button class="lnk" onclick={() => (selected = new Set(offered.map((d) => d.inf)))}>все</button>
                  <button class="lnk" onclick={() => (selected = new Set())}>ни одного</button>
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
                  {offered.length === 0 ? 'Диски и сеть этой машины работают' : 'Остальные устройства работают'}
                  на драйверах, встроенных в Windows, — выгружать их не нужно. При
                  сборке проверится, что они есть и в среде восстановления; если
                  какого-то нет, он будет добавлен с этой машины.
                </div>
                {#each inbox as d (d.inf)}
                  <div class="drv inbox-row" title="{d.device} — {d.role}, драйвер {d.inf}">
                    <span></span><!-- the checkbox column: nothing to tick -->
                    <span class="drv-name">{d.device || d.inf}</span>
                    <span class="drv-class">{d.role}</span>
                    <span class="drv-prov">{d.inf}</span>
                    <span class="drv-size">встроенный</span>
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
              Средства на всякий случай
              <span class="opt-note">
                программы с этого компьютера, вместе с их настройками
              </span>
              <span class="tools-line">
                {tools
                  .map((t) => `${t.name} — ${formatBytes(t.size_bytes)} · ${t.files} файлов`)
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
          <div class="screen-title">Разрешение экрана на носителе</div>
          Режим экрана выбирает прошивка компьютера при загрузке, видеодрайверы
          для этого не нужны. Чтобы носитель открылся в разрешении, которое
          рекомендует монитор, отключите в настройках BIOS/UEFI модуль CSM
          (Compatibility Support Module — обычно в разделе загрузки, пункт
          «Launch CSM» или «CSM Support»). С включённым CSM экран может остаться
          в 1024×768 или 1280×1024.
          <span class="screen-warn">
            Если основная система этого компьютера установлена в режиме Legacy
            (MBR), без CSM она не загрузится — после работы с носителем включите
            его обратно.
          </span>
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
              <span class="mark">{done ? '✓' : now ? '▸' : ''}</span>
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
          <div class="rep-head">Носитель готов</div>
          <div class="rep-row"><span>Файл</span><span>{report.iso_path}</span></div>
          <div class="rep-row"><span>Размер</span><span>{formatBytes(report.iso_bytes)}</span></div>
          <div class="rep-row">
            <span>Основа</span>
            <span>{report.base_name} · сборка {report.base_build} · {report.base_language}</span>
          </div>
          <div class="rep-row">
            <span>Драйверы</span>
            <span>{report.drivers_added > 0 ? `${report.drivers_added} шт.` : 'не добавлялись'}</span>
          </div>
          {#if report.tools_added > 0}
            <div class="rep-row">
              <span>Средства</span>
              <span>{report.tools_added} шт. · {formatBytes(report.tools_bytes)}</span>
            </div>
          {/if}
          {#if report.temp_bytes_freed > 0}
            <div class="rep-row">
              <span>Убрано</span>
              <span>{formatBytes(report.temp_bytes_freed)} временных файлов</span>
            </div>
          {/if}
          {#each report.warnings as w}<div class="warn">{w}</div>{/each}
          <div class="rep-note">
            Запишите ISO на флешку (Rufus и подобные) или подключите как привод
            в виртуальной машине.
          </div>
        </div>
      {/if}
    </div>

    <!-- Pinned: the destination and the button stay reachable however long the
         list above grows. -->
    <div class="dest">
      <span class="lbl">Куда сохранить ISO</span>
      <input type="text" bind:value={outputDir} placeholder="D:\OwnBackup"
             spellcheck="false" disabled={building || finished} />
      <button class="btn outline" onclick={chooseFolder} disabled={building || finished}>
        Обзор…
      </button>
      {#if finished}
        <button class="btn primary" onclick={onClose}>Готово</button>
      {:else}
        <button class="btn primary" onclick={start} disabled={!canStart}
                title={!isWindows
                  ? 'Сборка Linux-носителя ещё не реализована'
                  : includeDrivers && loadingDrivers
                    ? 'Идёт опрос оборудования'
                    : ''}>
          {building ? 'Сборка…' : includeDrivers && loadingDrivers ? 'Опрос…' : 'Начать'}
        </button>
      {/if}
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,.45); display: flex; align-items: center; justify-content: center; z-index: 250; padding: 24px; }
  .dialog { width: 760px; max-width: 96vw; max-height: 92vh; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow); display: flex; flex-direction: column; }
  .head { padding: 14px 18px; font-size: 16px; color: var(--head); border-bottom: 1px solid var(--border); display: flex; align-items: center; justify-content: space-between; }
  .x { background: none; border: none; color: var(--text-muted); font-size: 15px; cursor: pointer; padding: 2px 6px; }
  .body { padding: 16px 18px; overflow: auto; flex: 1; min-height: 0; }
  .intro { font-size: 13px; color: var(--head); line-height: 1.6; }
  .soon { margin-top: 10px; font-size: 13px; color: var(--warning); line-height: 1.5; }
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
  .scan-bar { flex: 0 0 120px; height: 5px; background: var(--border); border-radius: 3px; overflow: hidden; }
  /* Indeterminate on purpose: the count is not known until the walk is done. */
  .scan-run { display: block; width: 40%; height: 100%; background: var(--green); border-radius: 3px; animation: scan 1.1s ease-in-out infinite; }
  @keyframes scan { 0% { transform: translateX(-100%); } 100% { transform: translateX(250%); } }
  .drv-head { display: flex; justify-content: space-between; align-items: center; font-size: 12px; color: var(--text-muted); margin-bottom: 6px; }
  .drv-actions { display: flex; gap: 12px; }
  .lnk { background: none; border: none; color: var(--primary); font-size: 12px; cursor: pointer; padding: 0; font-family: var(--font); }
  .drv-list { max-height: 220px; overflow: auto; border: 1px solid var(--border); border-radius: 5px; }
  .drv { display: grid; grid-template-columns: 20px 1fr 150px 1fr 80px; gap: 10px; align-items: center; padding: 6px 10px; font-size: 12px; cursor: pointer; color: var(--head); }
  .drv:hover { background: var(--hover-bg, rgba(127,127,127,.10)); }
  .drv-name, .drv-prov { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .drv-class { color: var(--text-muted); }
  .drv-prov { color: var(--text-muted); }
  .drv-size { text-align: right; color: var(--text-muted); }
  /* Pinned below the scrolling body, not inside it. */
  .dest { display: grid; grid-template-columns: auto 1fr auto auto; gap: 10px; align-items: center; padding: 12px 18px; border-top: 1px solid var(--border); flex-shrink: 0; background: var(--card-bg); border-radius: 0 0 8px 8px; }
  .lbl { font-size: 12px; color: var(--primary); }
  .dest input { min-width: 0; background: none; border: 1px solid var(--border); border-radius: 4px; padding: 7px 10px; font-size: 13px; color: var(--head); font-family: var(--font); }
  .btn { border-radius: 4px; padding: 8px 18px; font-size: 13px; font-family: var(--font); cursor: pointer; border: 1px solid var(--border); background: none; color: var(--head); }
  .btn.primary { background: var(--primary); border-color: var(--primary); color: #fff; }
  .btn:disabled { opacity: .5; cursor: default; }
  .queue { margin-top: 16px; border: 1px solid var(--border); border-radius: 5px; padding: 10px 12px; }
  /* The bar takes everything to the right of the title; the running stage's
     detail rides along with the title rather than holding a column of its own,
     which was empty on every other row and cut the bar short of the edge. */
  .stage { display: grid; grid-template-columns: 16px minmax(190px, auto) 1fr; gap: 10px; align-items: center; padding: 3px 0; font-size: 12px; color: var(--text-muted); }
  .st-title { white-space: nowrap; }
  .stage.now, .stage.done { color: var(--head); }
  .mark { color: var(--green); }
  .st-bar { position: relative; height: 16px; background: var(--border); border-radius: 3px; overflow: hidden; }
  .st-fill { display: block; height: 100%; background: var(--green); transition: width .25s; }
  /* No number to show yet — the stripe says the step is alive without
     claiming a position it does not know. */
  .st-run { position: absolute; inset: 0 auto 0 0; width: 32%; background: var(--green); opacity: .75; border-radius: 3px; animation: st-slide 1.4s ease-in-out infinite; }
  @keyframes st-slide { 0% { transform: translateX(-105%); } 100% { transform: translateX(320%); } }
  .st-clock { margin-left: 8px; opacity: .75; font-variant-numeric: tabular-nums; }
  .st-detail { position: absolute; right: 8px; top: 0; line-height: 16px; font-size: 11px; color: var(--head); max-width: 70%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .err { font-size: 12px; color: var(--error); margin-top: 12px; word-break: break-word; }
  /* A warning is not a footnote: the build finished, but something the user
     asked for is not on the medium. Framed, so it is not read as prose. */
  .warn {
    font-size: 12px;
    color: var(--error);
    margin-top: 8px;
    padding: 8px 10px;
    line-height: 1.45;
    border: 1px solid rgba(200, 60, 60, .45);
    border-left-width: 3px;
    border-radius: 4px;
    background: rgba(200, 60, 60, .07);
  }
  .muted { font-size: 13px; color: var(--text-muted); padding: 6px 0; }
  .report { margin-top: 14px; border: 1px solid var(--border); border-radius: 5px; padding: 12px 14px; }
  .rep-head { font-size: 14px; color: var(--head); margin-bottom: 8px; }
  .rep-row { display: flex; gap: 12px; font-size: 13px; margin-bottom: 4px; }
  .rep-row > span:first-child { flex: 0 0 90px; color: var(--primary); font-size: 12px; }
  .rep-row > span:last-child { color: var(--head); min-width: 0; word-break: break-word; }
  .rep-note { font-size: 12px; color: var(--text-muted); margin-top: 9px; line-height: 1.5; }
</style>
