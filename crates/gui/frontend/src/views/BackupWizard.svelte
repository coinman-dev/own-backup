<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
  import { api } from '../lib/api';
  import { engine } from '../lib/engineStore.svelte';
  import { saveJob, runJob, provideVolumePassword } from '../lib/jobStore.svelte';
  import { formatBytes } from '../lib/format';
  import InfoModal from '../components/InfoModal.svelte';
  import type { Job, DiskInfo, PartitionInfo } from '../lib/types';

  interface Props {
    mode: 'wizard' | 'edit';
    // Shared draft owned by the parent (BackupSection) so the name is also
    // editable inline in the jobs list. Cancel discards it in the parent.
    job: Job;
    onCancel: () => void;
    onDone: (job: Job) => void;
  }
  let { mode, job, onCancel, onDone }: Props = $props();

  let step = $state(0);
  let error: string | null = $state(null);
  let stubMsg: string | null = $state(null);
  const stub = () =>
    (stubMsg = 'Эта настройка сохраняется, но пока не применяется — реализация позже.');

  const steps = [
    { key: 'source', label: 'Источник' },
    { key: 'schedule', label: 'Расписание' },
    { key: 'scheme', label: 'Схема' },
    { key: 'exclusions', label: 'Исключения' },
    { key: 'advanced', label: 'Дополнительно' },
  ];

  const weekdays = [
    ['mon', 'Пн'], ['tue', 'Вт'], ['wed', 'Ср'], ['thu', 'Чт'],
    ['fri', 'Пт'], ['sat', 'Сб'], ['sun', 'Вс'],
  ];
  const scheduleKinds = [
    ['daily', 'Ежедневно'], ['weekly', 'Еженедельно'], ['monthly', 'Ежемесячно'],
    ['event', 'По событию'], ['nonstop', 'Непрерывно'], ['none', 'Не планировать'],
  ];

  function toggleDay(d: string) {
    job.schedule.days = job.schedule.days.includes(d)
      ? job.schedule.days.filter((x) => x !== d)
      : [...job.schedule.days, d];
  }

  // ---- source selection (Paragon-style disk map, multi-volume) ----------

  const FS_LABELS: Record<string, string> = {
    ntfs: 'NTFS', fat32: 'FAT32', ex_fat: 'exFAT', ext4: 'ext4', xfs: 'XFS',
    btrfs: 'Btrfs', re_fs: 'ReFS', unknown: '',
  };
  // Unknown / unformatted → empty string (the UI shows nothing, not "—").
  function fsLabel(fs: string): string {
    return FS_LABELS[fs] ?? (fs === 'unknown' ? '' : fs);
  }
  function tableLabel(t: string): string {
    return t === 'gpt' ? 'GPT' : t === 'mbr' ? 'MBR' : t.toUpperCase();
  }

  // Openable device for a partition (letter "C:" or a `\\?\Volume{GUID}` path
  // for letterless system/hidden volumes). Empty ⇒ not backable ⇒ not clickable.
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  function partLabel(p: PartitionInfo): string {
    if (p.volume_letter) {
      const name = p.volume_label || fsLabel(p.filesystem) || 'Том';
      return `${p.volume_letter}: ${name}`;
    }
    // Letterless (EFI / ext4 / hidden): its on-disk label, else the filesystem.
    return p.volume_label || fsLabel(p.filesystem) || 'Раздел';
  }
  // Per-source token used in the file-name preview (mirrors core's source_token:
  // letter for lettered volumes, V+8 hex of the GUID for letterless volumes,
  // P<disk><offsetHex> for physical-offset partitions).
  function sourceToken(device: string): string {
    const pm = device.match(/^part:(\d+):(\d+):/);
    if (pm) return `P${pm[1]}${BigInt(pm[2]).toString(16).toUpperCase()}`;
    const m = device.match(/Volume\{([0-9a-fA-F]+)/);
    if (m) return 'V' + m[1].slice(0, 8).toUpperCase();
    return driveLetter(device) ?? 'X';
  }

  function isSelected(dev: string): boolean {
    return job.sources.includes(dev);
  }
  function toggleSource(dev: string, label: string) {
    if (job.sources.includes(dev)) {
      const i = job.sources.indexOf(dev);
      job.sources = job.sources.filter((x) => x !== dev);
      job.source_labels = job.source_labels.filter((_, idx) => idx !== i);
    } else {
      job.sources = [...job.sources, dev];
      job.source_labels = [...job.source_labels, label];
    }
    // Keep the primary source in sync (used for file naming / back-compat).
    job.source = job.sources[0] ?? '';
    job.source_label = job.source_labels[0] ?? '';
  }

  // Proportional segments of a disk: partitions plus hatched gaps for
  // unallocated space, ordered by on-disk offset.
  interface DiskSeg {
    kind: 'part' | 'free';
    size: number;
    p?: PartitionInfo;
  }
  function diskSegments(d: DiskInfo): DiskSeg[] {
    const parts = [...d.partitions].sort((a, b) => a.offset_bytes - b.offset_bytes);
    const segs: DiskSeg[] = [];
    const minGap = d.size_bytes * 0.01; // ignore sub-1% alignment gaps
    let cursor = 0;
    for (const p of parts) {
      const gap = p.offset_bytes - cursor;
      if (gap > minGap) segs.push({ kind: 'free', size: gap });
      segs.push({ kind: 'part', size: p.size_bytes, p });
      cursor = Math.max(cursor, p.offset_bytes + p.size_bytes);
    }
    const tail = d.size_bytes - cursor;
    if (tail > minGap) segs.push({ kind: 'free', size: tail });
    return segs;
  }

  // Fraction (0..100) of a partition that is in use, for the fill overlay.
  function usedPct(p: PartitionInfo): number {
    if (p.used_bytes <= 0 || p.size_bytes <= 0) return 0;
    return Math.min(100, (p.used_bytes / p.size_bytes) * 100);
  }
  // Full text for the segment tooltip (shown when the label/size is truncated).
  function segTitle(p: PartitionInfo): string {
    let t = partLabel(p) + ' — ' + formatBytes(p.size_bytes);
    if (p.used_bytes > 0) t += ` (${formatBytes(p.used_bytes)} занято)`;
    const fs = fsLabel(p.filesystem);
    if (fs) t += ` ${fs}`;
    return t;
  }

  // Click a disk header → select ALL its lettered partitions, or clear them
  // all if they are already selected.
  function toggleDisk(d: DiskInfo) {
    const parts = d.partitions.filter((p) => partDevice(p));
    if (parts.length === 0) return;
    const allSelected = parts.every((p) => isSelected(partDevice(p)!));
    for (const p of parts) {
      const dev = partDevice(p)!;
      const selected = isSelected(dev);
      if (allSelected ? selected : !selected) toggleSource(dev, partLabel(p));
    }
  }

  async function browseDest() {
    try {
      const picked = await open({ directory: true, title: 'Папка для образов' });
      if (typeof picked === 'string') job.dest_folder = picked;
    } catch (e) {
      error = String(e);
    }
  }

  // Exclusions
  let newPattern = $state('');
  function addPattern() {
    const p = newPattern.trim();
    if (p) {
      job.exclusions.patterns = [...job.exclusions.patterns, p];
      newPattern = '';
    }
  }
  function removePattern(i: number) {
    job.exclusions.patterns = job.exclusions.patterns.filter((_, idx) => idx !== i);
  }
  async function resetExclusions() {
    job.exclusions.patterns = await api.defaultExclusionPatterns();
  }

  // Uppercase drive letter of a path/device, or null (UNC/no letter).
  function driveLetter(s: string): string | null {
    const t = s.trim().replace(/^\\\\[.?]\\/, '');
    return t.length >= 2 && /[a-zA-Z]/.test(t[0]) && t[1] === ':'
      ? t[0].toUpperCase()
      : null;
  }

  // The drive letter of any selected source that collides with the
  // destination folder, or null when the destination is safe for all sources.
  const collisionLetter = $derived.by(() => {
    const dst = driveLetter(job.dest_folder);
    if (!dst) return null;
    for (const s of job.sources) {
      if (driveLetter(s) === dst) return dst;
    }
    return null;
  });

  // Representative timestamp for the file-name preview (fixed at open — the
  // real one is stamped when the backup actually runs).
  const pad2 = (n: number) => String(n).padStart(2, '0');
  const previewStamp = (() => {
    const d = new Date();
    return `${d.getFullYear()}${pad2(d.getMonth() + 1)}${pad2(d.getDate())}-${pad2(d.getHours())}${pad2(d.getMinutes())}${pad2(d.getSeconds())}`;
  })();

  // Full path of the file the first selected volume will produce, so the user
  // sees the concrete auto-generated name (mirrors the engine's naming; the
  // <тип> here is shown as "full" — the real type/date are decided at run time).
  const previewPath = $derived.by(() => {
    const folder = job.dest_folder.trim();
    if (!folder || job.sources.length === 0) return '';
    const safe = (job.name || 'backup').replace(/[^\p{L}\p{N}]/gu, '_');
    const token = sourceToken(job.sources[0]);
    const sep = folder.endsWith('\\') || folder.endsWith('/') ? '' : '\\';
    return `${folder}${sep}${safe}_${token}_full_${previewStamp}.vhdx`;
  });

  // Universal "folder + file name" field (2-in-1). At rest it shows the full
  // generated path so the whole name is visible; while focused it collapses to
  // the editable "folder\<имя>", and BOTH the folder and the base name can be
  // typed freely (last path segment = base name → job.name, the rest = folder →
  // job.dest_folder). The engine still appends _<буква>_<тип>_<дата>.vhdx.
  let destFocused = $state(false);
  let pathText = $state('');

  function editablePath(): string {
    const folder = job.dest_folder.trim();
    const name = job.name.trim();
    if (!folder && !name) return '';
    const sep = folder && !/[\\/]$/.test(folder) ? '\\' : '';
    return `${folder}${sep}${name}`;
  }
  $effect(() => {
    if (!destFocused) pathText = editablePath();
  });
  const destDisplay = $derived(destFocused ? pathText : previewPath || editablePath());

  function onDestInput(e: Event) {
    pathText = (e.target as HTMLInputElement).value;
    const idx = Math.max(pathText.lastIndexOf('\\'), pathText.lastIndexOf('/'));
    const folder = idx >= 0 ? pathText.slice(0, idx) : '';
    const name = (idx >= 0 ? pathText.slice(idx + 1) : pathText).replace(/\.(a?vhdx)$/i, '');
    job.dest_folder = folder;
    job.name = name;
  }

  function validate(): boolean {
    if (!job.name.trim()) {
      error = 'Укажите имя задания.';
      step = 0;
      return false;
    }
    if (job.sources.length === 0) {
      error = 'Выберите источник (хотя бы один том).';
      step = 0;
      return false;
    }
    if (!job.dest_folder.trim()) {
      error = 'Выберите папку назначения.';
      step = 0;
      return false;
    }
    if (collisionLetter) {
      error = `Нельзя сохранять копию на тот же диск, который копируется (${collisionLetter}:). Выберите для назначения другой диск.`;
      step = 0;
      return false;
    }
    error = null;
    return true;
  }

  // ---- encrypted sources: password entry -------------------------------

  // Per-device password inputs for encrypted (LUKS) sources.
  let pwInputs = $state<Record<string, { password: string; remember: boolean }>>({});

  function findPart(dev: string): PartitionInfo | null {
    for (const d of engine.disks) {
      for (const p of d.partitions) if (p.volume_device === dev) return p;
    }
    return null;
  }
  // Selected sources that are encrypted, paired with their display label.
  function encryptedSelected(): { device: string; label: string }[] {
    return job.sources
      .map((dev, i) => ({ device: dev, label: job.source_labels[i] ?? dev }))
      .filter(({ device }) => findPart(device)?.is_encrypted);
  }

  // Keep the "needs a password" job flag in sync with the selection.
  $effect(() => {
    job.requires_password = encryptedSelected().length > 0;
  });

  function setPw(dev: string, value: string) {
    pwInputs[dev] = { password: value, remember: pwInputs[dev]?.remember ?? false };
  }
  function setRemember(dev: string, remember: boolean) {
    pwInputs[dev] = { password: pwInputs[dev]?.password ?? '', remember };
  }
  // Are all encrypted sources set to "remember"? (Scheduling needs this.)
  function allRemembered(): boolean {
    const enc = encryptedSelected();
    return enc.length > 0 && enc.every(({ device }) => pwInputs[device]?.remember && pwInputs[device]?.password);
  }

  async function finish(run: boolean) {
    if (!validate()) return;
    const enc = encryptedSelected();
    // A run cannot proceed without a password for every encrypted source.
    if (run && enc.some(({ device }) => !pwInputs[device]?.password)) {
      error = 'Для зашифрованного тома введите пароль перед запуском.';
      step = 0;
      return;
    }
    try {
      // Hand any entered passwords to the backend (cached; persisted if "remember").
      for (const { device } of enc) {
        const pw = pwInputs[device];
        if (pw?.password) await provideVolumePassword(device, pw.password, pw.remember);
      }
      const saved = await saveJob(job);
      if (run) await runJob(saved.id);
      onDone(saved);
    } catch (e) {
      error = String(e);
    }
  }

  // Migrate an old single-source job (only `source` set) into the multi-source
  // array. Nothing is selected by default — the user picks volumes explicitly.
  $effect(() => {
    if (job.sources.length === 0 && job.source.trim()) {
      job.sources = [job.source];
      job.source_labels = [job.source_label || job.source];
    }
  });
</script>

{#snippet segInner(p: PartitionInfo, selected: boolean)}
  {#if usedPct(p) > 0}
    <div class="usedfill" style="width: {usedPct(p)}%"></div>
  {/if}
  <div class="seg-top">
    {#if p.is_system || p.is_boot}
      <svg class="lock" width="10" height="10" viewBox="0 0 24 24" fill="currentColor"><path d="M12 2a5 5 0 0 0-5 5v3H6a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-9a1 1 0 0 0-1-1h-1V7a5 5 0 0 0-5-5zm-3 8V7a3 3 0 0 1 6 0v3z"/></svg>
    {/if}
    {#if p.is_encrypted}
      <span class="enc" title="Зашифрован — потребуется пароль">🔒</span>
    {/if}
    <span class="seg-label">{partLabel(p)}</span>
  </div>
  <div class="seg-meta">{formatBytes(p.size_bytes)}{#if p.used_bytes > 0}<span class="muted">&nbsp;({formatBytes(p.used_bytes)} занято)</span>{/if}{#if fsLabel(p.filesystem)}<span class="muted">&nbsp;{fsLabel(p.filesystem)}</span>{/if}</div>
  {#if selected}
    <div class="check"><svg width="10" height="10" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></div>
  {/if}
{/snippet}

<div class="wizard">
    <div class="wz-head">
      <span class="wz-title">Параметры резервного копирования диска</span>
      <span class="wz-name">{job.name}</span>
    </div>

    <!-- tab bar -->
    <div class="tabs">
      {#each steps as s, i}
        <button class="tab" class:active={step === i} onclick={() => (step = i)}>{s.label}</button>
      {/each}
    </div>

    <div class="body">
      <!-- 0: source (top) → destination (bottom) -->
      {#if step === 0}
        <div class="src-wrap">
          <!-- SOURCE -->
          <div class="sec-head">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
            <span class="sec-title">Что копировать</span>
            <span class="sec-sub">выберите один или несколько томов</span>
          </div>

          {#if engine.volumesLoading && engine.disks.length === 0}
            <div class="loading-note">Загрузка списка дисков…</div>
          {:else if engine.volumesError && engine.disks.length === 0}
            <div class="warn-note">{engine.volumesError}</div>
          {:else}
            <div class="disks">
              {#each engine.disks as d}
                <div class="disk-card">
                  <button type="button" class="disk-head" onclick={() => toggleDisk(d)} title="Выбрать / снять все тома диска">
                    <span class="disk-model">{d.model || d.device_id}</span>
                    <span class="disk-meta">{formatBytes(d.size_bytes)} · {tableLabel(d.partition_table)}</span>
                  </button>
                  <div class="disk-body">
                    <div class="disk-ico">
                      <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.3"><path d="M3 8a9 3 0 0 0 18 0M3 8a9 3 0 0 1 18 0v8a9 3 0 0 1-18 0z"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>
                      <div class="disk-name">Диск {d.device_id.replace(/\D+/g, '') || ''}</div>
                    </div>
                    <div class="bar">
                      {#each diskSegments(d) as seg}
                        {#if seg.kind === 'free'}
                          <div class="seg free" style="flex: {seg.size} 1 0"><span class="free-lbl">Не занято</span></div>
                        {:else if seg.p}
                          {@const dev = partDevice(seg.p)}
                          {#if dev}
                            <button
                              type="button"
                              class="seg selectable"
                              class:selected={isSelected(dev)}
                              style="flex: {seg.size} 1 0"
                              title={segTitle(seg.p)}
                              onclick={() => toggleSource(dev, partLabel(seg.p!))}
                            >
                              {@render segInner(seg.p, isSelected(dev))}
                            </button>
                          {:else}
                            <div class="seg" style="flex: {seg.size} 1 0" title={segTitle(seg.p)}>
                              {@render segInner(seg.p, false)}
                            </div>
                          {/if}
                        {/if}
                      {/each}
                    </div>
                  </div>
                </div>
              {/each}
            </div>
          {/if}

          {#if encryptedSelected().length > 0}
            <div class="pw-panel">
              <div class="pw-head">🔒 Зашифрованные тома — введите пароль для расшифровки</div>
              {#each encryptedSelected() as e (e.device)}
                <div class="pw-row">
                  <span class="pw-vol" title={e.device}>{e.label}</span>
                  <input
                    type="password"
                    class="pw-input"
                    placeholder="Пароль"
                    autocomplete="off"
                    value={pwInputs[e.device]?.password ?? ''}
                    oninput={(ev) => setPw(e.device, (ev.target as HTMLInputElement).value)}
                  />
                  <label class="pw-remember">
                    <input
                      type="checkbox"
                      checked={pwInputs[e.device]?.remember ?? false}
                      onchange={(ev) => setRemember(e.device, (ev.target as HTMLInputElement).checked)}
                    /> Запомнить
                  </label>
                </div>
              {/each}
              <div class="pw-hint">
                Пароль хранится в памяти до перезапуска службы. «Запомнить» сохраняет его
                в защищённом хранилище ОС — это нужно, чтобы копия работала по расписанию.
              </div>
            </div>
          {/if}

          <!-- connector -->
          <div class="connector">
            <div class="dash"></div>
            <div class="arrow-circle"><svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M19 12l-7 7-7-7"/></svg></div>
            <div class="dash"></div>
          </div>

          <!-- DESTINATION -->
          <div class="sec-head">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 6a2 2 0 0 1 2-2h5l2 2h5a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z"/></svg>
            <span class="sec-title">Куда сохранять</span>
            <span class="sec-sub">папка и имя файла резервной копии</span>
          </div>
          <div class="dest-card">
            <svg width="42" height="40" viewBox="0 0 24 22" fill="#e6b34a" style="flex-shrink:0"><path d="M4 2h9l2 3h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1z"/><rect x="3" y="9" width="18" height="10" rx="1" fill="#f0c268"/></svg>
            <div class="dest-fields">
              <input
                type="text"
                value={destDisplay}
                oninput={onDestInput}
                onfocus={() => (destFocused = true)}
                onblur={() => (destFocused = false)}
                placeholder="D:\OwnBackup"
                spellcheck="false"
                title={destDisplay}
              />
              <button class="link" onclick={browseDest}>Обзор…</button>
            </div>
          </div>
          {#if collisionLetter}
            <div class="warn-note">⛔ Назначение на том же диске ({collisionLetter}:), что и один из источников. Копию нужно писать на другой диск — выберите другую папку.</div>
          {/if}
        </div>
      {/if}

      <!-- 1: schedule -->
      {#if step === 1}
        <div class="sched">
          {#if job.requires_password && !allRemembered()}
            <div class="warn-note">
              Среди источников есть зашифрованный том. Чтобы копия запускалась по
              расписанию, отметьте «Запомнить» для его пароля на шаге «Источник» —
              иначе служба пропустит запланированный запуск.
            </div>
          {/if}
          <div class="sched-kinds">
            {#each scheduleKinds as [k, lbl]}
              <label class="radio"><input type="radio" name="skind" value={k} checked={job.schedule.kind === k} onchange={() => (job.schedule.kind = k as Job['schedule']['kind'])} /> {lbl}</label>
            {/each}
          </div>
          <div class="sched-right">
            {#if job.schedule.kind === 'weekly'}
              <div class="days">
                {#each weekdays as [d, lbl]}
                  <button class="day" class:on={job.schedule.days.includes(d)} onclick={() => toggleDay(d)}>{lbl}</button>
                {/each}
              </div>
            {/if}
            {#if job.schedule.kind === 'daily' || job.schedule.kind === 'weekly' || job.schedule.kind === 'monthly'}
              <label class="at">В <input type="time" bind:value={job.schedule.time} /></label>
            {/if}
          </div>
        </div>
        <div class="dev-note">⚙ Автозапуск по расписанию — в разработке (сохраняется, но пока не срабатывает).</div>
        <div class="adv-schedule">
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.only_when_locked} onchange={stub} /> Копировать только если компьютер заблокирован</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.wake_to_run} onchange={stub} /> Пробуждать компьютер из спящего режима</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.prevent_sleep} onchange={stub} /> Предотвращать переход в спящий режим</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.run_missed} onchange={stub} /> Выполнять пропущенные операции при запуске</label>
        </div>
      {/if}

      <!-- 2: scheme -->
      {#if step === 2}
        <div class="field-label">Метод резервного копирования:</div>
        <select bind:value={job.scheme.method} class="wide">
          <option value="full">Полное</option>
          <option value="incremental">Инкрементное</option>
          <option value="differential">Дифференциальное</option>
        </select>
        {#if job.scheme.method === 'incremental'}
          <div class="note">Каждый запуск сохраняет только изменения относительно предыдущей версии (цепочка полная → И1 → И2 …).</div>
        {:else if job.scheme.method === 'differential'}
          <div class="note">Каждый запуск сохраняет изменения относительно полной копии (полная → Д1, Д2 …).</div>
        {/if}
        {#if job.scheme.method !== 'full'}
          <label class="radio blk"><input type="radio" checked={job.scheme.only_incremental} onchange={() => (job.scheme.only_incremental = true)} /> Только инкрементные версии после первой полной</label>
          <label class="radio blk"><input type="radio" checked={!job.scheme.only_incremental} onchange={() => (job.scheme.only_incremental = false)} /> Полная версия после каждых <input type="number" min="1" bind:value={job.scheme.full_every_n} class="num" /> версий</label>
        {/if}
        <div class="field-label mt">Правила очистки старых версий:</div>
        <label class="chk"><input type="checkbox" checked={job.scheme.keep_chains !== null} onchange={(e) => (job.scheme.keep_chains = (e.target as HTMLInputElement).checked ? 3 : null)} /> Хранить не более <input type="number" min="1" value={job.scheme.keep_chains ?? 3} oninput={(e) => (job.scheme.keep_chains = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.keep_chains === null} /> последних цепочек</label>
        <label class="chk"><input type="checkbox" checked={job.scheme.cleanup_older_than_days !== null} onchange={(e) => (job.scheme.cleanup_older_than_days = (e.target as HTMLInputElement).checked ? 90 : null)} /> Удалять цепочки старше <input type="number" min="1" value={job.scheme.cleanup_older_than_days ?? 90} oninput={(e) => (job.scheme.cleanup_older_than_days = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.cleanup_older_than_days === null} /> дней</label>
        <label class="chk"><input type="checkbox" bind:checked={job.scheme.keep_first} /> Не удалять первую (самую старую) цепочку</label>
        <div class="note">Цепочка удаляется целиком (полная копия вместе с её инкрементами) после успешного завершения очередного копирования. Цепочка, к которой относится свежая копия, не удаляется никогда.</div>
      {/if}

      <!-- 3: exclusions -->
      {#if step === 3}
        <label class="chk"><input type="checkbox" bind:checked={job.exclusions.enabled} /> Исключить файлы по следующим критериям:</label>
        <div class="note">Маска без пути (<code>*.tmp</code>, <code>pagefile.sys</code>) действует по всему тому; маска с путём (<code>Temp\*.log</code>) — от корня тома. Работает для NTFS; в посекторном режиме исключения не применяются.</div>
        <div class="excl-list">
          {#each job.exclusions.patterns as p, i}
            <div class="excl-row"><span>{p}</span><button class="x" onclick={() => removePattern(i)}>×</button></div>
          {/each}
        </div>
        <div class="excl-add">
          <input type="text" bind:value={newPattern} placeholder="Путь или маска, напр. *.tmp" spellcheck="false" onkeydown={(e) => e.key === 'Enter' && addPattern()} />
          <button class="btn small" onclick={addPattern}>+ Добавить</button>
          <button class="link" onclick={resetExclusions}>Исходный список</button>
        </div>
      {/if}

      <!-- 4: advanced -->
      {#if step === 4}
        <div class="adv">
          <div class="adv-group">
            <div class="adv-title">Режим создания образа</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.sector_by_sector} /> Архивировать в посекторном режиме <span class="ok-badge">работает</span></label>
          </div>
          <div class="adv-group">
            <div class="adv-title">Проверка</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.validate_after} /> Проверять резервную копию после создания <span class="ok-badge">работает</span></label>
          </div>
          <button class="adv-row" onclick={stub}><span>🔒 Защита резервной копии (пароль)</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⌘ Pre/Post-команды</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⚠ Обработка ошибок</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⏻ Выключение компьютера</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⏱ Производительность</span><span class="dev-badge">в разработке</span></button>
        </div>
      {/if}
    </div>

    {#if error}<div class="wz-error">{error}</div>{/if}

    <!-- footer -->
    <div class="footer">
      <div class="spacer"></div>
      <button class="btn ghost" onclick={onCancel}>Отмена</button>
      {#if mode === 'wizard'}
        {#if step > 0}<button class="btn ghost" onclick={() => (step -= 1)}>Назад</button>{/if}
        {#if step < steps.length - 1}
          <button class="btn primary" onclick={() => (step += 1)}>Дальше</button>
        {:else}
          <button class="btn green" onclick={() => finish(true)}>Создать копию</button>
        {/if}
      {:else}
        <button class="btn primary" onclick={() => finish(false)}>Сохранить</button>
      {/if}
    </div>
</div>

{#if stubMsg}<InfoModal message={stubMsg} onClose={() => (stubMsg = null)} />{/if}

<style>
  .wizard { height: 100%; background: var(--content-bg); display: flex; flex-direction: column; overflow: hidden; }
  .wz-head { padding: 22px 34px 14px; display: flex; align-items: baseline; gap: 14px; }
  .wz-title { font-size: 24px; font-weight: 300; color: var(--head); }
  .wz-name { font-size: 13px; color: var(--text-muted); }
  .tabs { display: flex; padding: 0 34px; border-bottom: 1px solid var(--border); }
  .tab { padding: 10px 4px 12px; margin-right: 28px; background: none; border: none; border-bottom: 3px solid transparent; color: var(--text-muted); font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; }
  .tab.active { color: var(--head); border-bottom-color: var(--primary); }
  .body { flex: 1; overflow: auto; padding: 26px 34px; }
  select.wide { width: 100%; padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: var(--card-bg); color: var(--text); }
  .link { background: none; border: none; color: var(--primary); font-size: 13px; cursor: pointer; padding: 0; }

  /* --- Paragon-style source map (step 0) --- */
  .src-wrap { max-width: 940px; margin: 0 auto; }
  .sec-head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .sec-title { font-size: 15px; font-weight: 700; color: var(--head); }
  .sec-sub { font-size: 13px; color: var(--text-muted); }
  .disks { display: flex; flex-direction: column; gap: 14px; }
  .disk-card { border: 1px solid var(--border); border-radius: 6px; overflow: hidden; background: var(--card-bg); }
  .disk-head { width: 100%; display: flex; align-items: center; justify-content: space-between; padding: 6px 12px; background: var(--panel2); border: 0; border-bottom: 1px solid var(--border); margin: 0; font-family: inherit; cursor: pointer; text-align: left; }
  .disk-head:hover { background: var(--hover, rgba(45,125,210,.06)); }
  .disk-model { font-size: 13px; font-weight: 600; color: var(--head); }
  .disk-meta { font-size: 12px; color: var(--text-muted); }
  .disk-body { display: flex; align-items: center; gap: 14px; padding: 7px 12px; }
  .disk-ico { width: 52px; flex-shrink: 0; text-align: center; color: var(--text-muted); }
  .disk-name { font-size: 12px; color: var(--head); margin-top: 1px; }
  .bar { flex: 1; display: flex; height: 44px; border: 1px solid var(--border); border-radius: 3px; overflow: hidden; background: var(--panel2); }
  /* Smallest readable segment width: tiny partitions never shrink below this
     (so their size/FS text stays legible), larger ones grow proportionally. */
  .seg { position: relative; padding: 5px 8px; min-width: 84px; border: 0; border-right: 1px solid var(--border); margin: 0; display: flex; flex-direction: column; gap: 3px; background: rgba(45,125,210,.06); overflow: hidden; text-align: left; font-family: inherit; color: inherit; appearance: none; -webkit-appearance: none; }
  .seg:last-child { border-right: none; }
  .seg.free { min-width: 40px; align-items: center; justify-content: center; background: repeating-linear-gradient(135deg, var(--panel2) 0 6px, var(--border) 6px 7px); }
  .free-lbl { font-size: 11px; color: var(--text-muted); white-space: nowrap; }
  .seg.selectable { cursor: pointer; }
  .seg.selectable:hover { filter: brightness(0.97); }
  .seg.selected { background: rgba(45,125,210,.16); box-shadow: inset 0 0 0 2px var(--primary); }
  /* Left-to-right "used space" fill inside a partition — a subtle shaded strip
     with a defined right edge, so occupancy is readable at a glance. */
  .usedfill { position: absolute; top: 0; bottom: 0; left: 0; z-index: 0; background: rgba(45,125,210,.14); border-right: 1px solid rgba(45,125,210,.45); pointer-events: none; }
  .seg.selected .usedfill { background: rgba(45,125,210,.24); border-right-color: rgba(45,125,210,.6); }
  .seg-top { position: relative; z-index: 1; display: flex; align-items: center; gap: 5px; color: var(--head); }
  .lock { color: var(--text-muted); flex-shrink: 0; }
  .enc { font-size: 10px; line-height: 1; flex-shrink: 0; }

  .pw-panel { margin-top: 16px; padding: 14px; border: 1px solid var(--border); border-radius: 8px; background: var(--surface-2, rgba(127,127,127,.06)); }
  .pw-head { font-size: 13px; font-weight: 700; margin-bottom: 10px; }
  .pw-row { display: flex; align-items: center; gap: 12px; margin-bottom: 8px; }
  .pw-vol { flex: 0 0 200px; font-size: 13px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .pw-input { flex: 1; padding: 6px 10px; border: 1px solid var(--border); border-radius: 5px; font-size: 13px; }
  .pw-remember { display: flex; align-items: center; gap: 6px; font-size: 13px; white-space: nowrap; }
  .pw-hint { font-size: 12px; color: var(--text-muted); margin-top: 8px; }
  .seg-label { font-size: 12px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta { position: relative; z-index: 1; font-size: 11px; color: var(--head); line-height: 1.35; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta .muted { color: var(--text-muted); }
  .check { position: absolute; top: 5px; right: 6px; z-index: 2; width: 16px; height: 16px; border-radius: 3px; background: var(--primary); display: flex; align-items: center; justify-content: center; }
  .connector { display: flex; flex-direction: column; align-items: center; margin: 20px 0; }
  .connector .dash { width: 2px; height: 14px; background: repeating-linear-gradient(180deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 44px; height: 44px; border-radius: 50%; background: var(--green); display: flex; align-items: center; justify-content: center; box-shadow: 0 4px 14px rgba(90,176,47,.35); margin: 4px 0; }
  .dest-card { display: flex; align-items: center; gap: 14px; padding: 12px; border: 1px solid var(--border); border-radius: 6px; background: var(--card-bg); }
  .dest-fields { flex: 1; display: flex; align-items: center; gap: 12px; min-width: 0; }
  .dest-fields input { flex: 1; min-width: 0; padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: var(--card-bg); color: var(--text); }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .note, .dev-note { font-size: 12px; color: var(--text-muted); margin-top: 18px; }
  .dev-note { color: #a06a10; background: rgba(251,188,5,.1); padding: 8px 12px; border-radius: 5px; }
  .warn-note { font-size: 13px; color: #8c1d1d; background: rgba(216,74,74,.1); padding: 10px 14px; border-radius: 5px; margin-top: 14px; font-weight: 600; }
  code { background: var(--panel2); padding: 1px 5px; border-radius: 3px; }
  .sched { display: flex; gap: 50px; }
  .sched-kinds { display: flex; flex-direction: column; gap: 14px; }
  .sched-right { display: flex; flex-direction: column; gap: 16px; padding-top: 2px; }
  .radio { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--head); cursor: pointer; }
  .radio.blk { margin: 10px 0; }
  .days { display: flex; border: 1px solid var(--border); border-radius: 5px; overflow: hidden; }
  .day { padding: 9px 12px; border: none; border-right: 1px solid var(--border); background: #fff; color: var(--head); font-size: 13px; cursor: pointer; font-family: var(--font); }
  .day.on { background: var(--primary); color: #fff; }
  .at { display: flex; align-items: center; gap: 10px; font-size: 14px; color: var(--head); }
  .at input { padding: 7px 10px; border: 1px solid var(--border); border-radius: 4px; }
  .adv-schedule { margin-top: 20px; display: flex; flex-direction: column; gap: 12px; }
  .chk { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--head); cursor: pointer; }
  .field-label { font-size: 15px; font-weight: 600; color: var(--head); margin-bottom: 10px; }
  .field-label.mt { margin-top: 26px; }
  .num { width: 60px; padding: 5px 8px; border: 1px solid var(--border); border-radius: 4px; margin: 0 4px; }
  .excl-list { border: 1px solid var(--border); border-radius: 5px; background: #fff; max-height: 260px; overflow: auto; margin-top: 12px; }
  .excl-row { display: flex; justify-content: space-between; align-items: center; padding: 6px 14px; font-size: 13px; color: var(--head); border-bottom: 1px solid var(--border); }
  .x { background: none; border: none; color: var(--text-muted); font-size: 18px; cursor: pointer; line-height: 1; }
  .excl-add { display: flex; gap: 10px; align-items: center; margin-top: 12px; }
  .excl-add input { flex: 1; padding: 8px 12px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; }
  .adv { display: flex; flex-direction: column; gap: 4px; }
  .adv-group { padding: 12px 0; border-bottom: 1px solid var(--border); }
  .adv-title { font-size: 13px; font-weight: 600; color: var(--primary); margin-bottom: 8px; }
  .adv-row { display: flex; align-items: center; justify-content: space-between; padding: 14px 4px; border-bottom: 1px solid var(--border); background: none; border-top: none; border-left: none; border-right: none; cursor: pointer; font-size: 14px; color: var(--head); font-family: var(--font); width: 100%; }
  .dev-badge { font-size: 11px; color: #a06a10; background: rgba(251,188,5,.15); padding: 3px 9px; border-radius: 10px; }
  .ok-badge { font-size: 11px; color: #2c5b12; background: rgba(90,176,47,.15); padding: 2px 8px; border-radius: 10px; margin-left: 6px; }
  .wz-error { padding: 8px 34px; color: #8c1d1d; font-size: 13px; background: rgba(216,74,74,.08); }
  .footer { display: flex; align-items: center; gap: 12px; padding: 14px 34px; border-top: 1px solid var(--border); background: var(--card-bg); }
  .spacer { flex: 1; }
  .btn { border-radius: 4px; padding: 10px 26px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn.small { padding: 7px 14px; font-size: 13px; }
  .btn.ghost { background: none; border: 1px solid var(--border); color: var(--head); }
  .btn.primary { background: var(--primary); color: #fff; }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover { background: var(--green-hover); }
</style>
